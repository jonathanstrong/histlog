use std::sync::Arc;
use std::time::{Instant, Duration, SystemTime, UNIX_EPOCH};
use std::path::{Path, PathBuf};
use std::thread::{self, JoinHandle};
use std::io;
use std::{mem, fs};

use hdrhistogram::{Histogram};
use hdrhistogram::serialization::V2DeflateSerializer;
use hdrhistogram::serialization::interval_log::{IntervalLogWriterBuilder, Tag};
use crossbeam_channel as channel;
use chrono::{DateTime, Utc};

type C = u64;

/// Significant figure passed to `hdrhistogram::Histogram::new` upon
/// construction
pub const SIG_FIG: u8 = 3;
/// Capacity of `crossbeam_channel::bounded` queue used to communicate
/// between the measuring thread and the writer thread.
pub const CHANNEL_SIZE: usize = 8;

pub fn nanos(d: Duration) -> u64 {
    d.as_secs() * 1_000_000_000_u64 + (d.subsec_nanos() as u64)
}

pub struct HistLog {
    save_dir: PathBuf,
    series: &'static str,
    tag: &'static str,
    freq: Duration,
    last_sent: Instant,
    tx: channel::Sender<Option<Entry>>,
    hist: Histogram<C>,
    thread: Option<Arc<thread::JoinHandle<Result<usize, Error>>>>,
}

pub struct Entry {
    pub tag: &'static str,
    pub start: SystemTime,
    pub end: SystemTime,
    pub hist: Histogram<C>,
}

#[derive(Debug)]
pub enum Error {
    Io(io::Error),
    HdrCreation(hdrhistogram::errors::CreationError),
    HdrRecord(hdrhistogram::errors::RecordError),
    TrySend(channel::TrySendError<()>),
}

impl Clone for HistLog {
    fn clone(&self) -> Self {
        let thread = self.thread.as_ref().map(|x| Arc::clone(x));
        Self {
            save_dir: self.save_dir.clone(),
            series: self.series.clone(),
            tag: self.tag.clone(),
            freq: self.freq.clone(),
            last_sent: Instant::now(),
            tx: self.tx.clone(),
            hist: self.hist.clone(),
            thread,
        }
    }
}

impl HistLog {
    pub fn new<P>(save_dir: P, series: &'static str, tag: &'static str, freq: Duration) -> Result<Self, Error>
        where P: AsRef<Path>
    {
        if !save_dir.as_ref().exists() {
            fs::create_dir_all(save_dir.as_ref()).map_err(Error::Io)?;
        }
        let save_dir = save_dir.as_ref().to_path_buf();
        let (tx, rx) = channel::bounded(CHANNEL_SIZE);
        let thread = Some(Arc::new(Self::scribe(series, rx, save_dir.clone())?));
        let last_sent = Instant::now();
        let hist = Histogram::new(SIG_FIG).map_err(Error::HdrCreation)?;
        Ok(Self { save_dir, series, tag, freq, last_sent, tx, hist, thread })
    }

    pub fn new_with_tag(&self, tag: &'static str) -> Result<Self, Error> {
        Self::new(self.save_dir.clone(), self.series, tag, self.freq)
    }

    pub fn clone_with_tag(&self, tag: &'static str) -> Self {
        assert!(self.thread.is_some(),
            "self.thread cannot be `None` unless `HistLog` was already dropped");
        let thread = self.thread.as_ref().map(|x| Arc::clone(x)).unwrap();
        let tx = self.tx.clone();
        Self {
            save_dir: self.save_dir.clone(),
            series: self.series,
            tag,
            freq: self.freq,
            last_sent: Instant::now(),
            tx,
            hist: self.hist.clone(),
            thread: Some(thread),
        }
    }

    pub fn clone_with_tag_and_freq(&self, tag: &'static str, freq: Duration) -> Self {
        let mut clone = self.clone_with_tag(tag);
        clone.freq = freq;
        clone
    }

    pub fn record(&mut self, value: u64) -> Result<(), Error> {
        self.hist.record(value).map_err(Error::HdrRecord)
    }

    /// If for some reason there was a pause in between using the struct, 
    /// this resets the internal state of both the values recorded to the
    /// `Histogram` and the value of when it last sent a `Histogram` onto
    /// the writing thread.
    /// 
    pub fn reset(&mut self) {
        self.hist.clear();
        self.last_sent = Instant::now();
    }

    fn send(&mut self, loop_time: Instant) {
        let end = SystemTime::now();
        let start = end - (loop_time - self.last_sent);
        assert!(end > start, "end <= start!");
        let mut next = Histogram::new_from(&self.hist);
        mem::swap(&mut self.hist, &mut next);
        self.tx.send(Some(Entry { tag: self.tag, start, end, hist: next })).expect("sending entry failed");
        self.last_sent = loop_time;
    }

    fn try_send(&mut self, loop_time: Instant) -> Result<(), Error>{
        let end = SystemTime::now();
        let start = end - (loop_time - self.last_sent);
        assert!(end > start, "end <= start!");
        let mut next = Histogram::new_from(&self.hist);
        mem::swap(&mut self.hist, &mut next);
        let entry = Entry { tag: self.tag, start, end, hist: next };
        match self.tx.try_send(Some(entry)) {
            Ok(_) => {
                self.last_sent = loop_time;
                Ok(())
            }

            Err(channel::TrySendError::Full(Some(Entry { mut hist, .. }))) => {
                // recoverable, swap rejected hist back in place 
                // and continue trying...
                mem::swap(&mut self.hist, &mut hist);
                Err(Error::TrySend(channel::TrySendError::Full(())))
            }

            Err(channel::TrySendError::Disconnected(_)) => {
                Err(Error::TrySend(channel::TrySendError::Disconnected(())))
            }

            Err(channel::TrySendError::Full(None)) => {
                Err(Error::TrySend(channel::TrySendError::Full(())))
            }
        }
    }

    pub fn check_send(&mut self, loop_time: Instant) {
        if loop_time > self.last_sent && loop_time - self.last_sent >= self.freq {
            self.send(loop_time);
        }
    }

    fn scribe(
        series: &'static str,
        rx: channel::Receiver<Option<Entry>>,
        save_dir: PathBuf,
    ) -> Result<JoinHandle<Result<usize, Error>>, Error> {
        let mut ser = V2DeflateSerializer::new();
        let start_time = SystemTime::now();
        let now = Utc::now();
        let filename =
            format!("{series}.{time}.hdrhistogram-interval-log.v2.gz",
                series = series, 
                time = now.format("%Y-%m-%d-%H:%M:%SZ"));
        let path = save_dir.join(&filename);
        let file = fs::File::create(&path).map_err(Error::Io)?;
        thread::Builder::new().name(format!("histlog:{}", series)).spawn(move || {
            let mut buf = io::LineWriter::new(file);
            let mut wtr =
                IntervalLogWriterBuilder::new() 
                    .with_base_time(UNIX_EPOCH)
                    .with_start_time(start_time)
                    .begin_log_with(&mut buf, &mut ser)
                    .map_err(Error::Io)?; // unrecoverable, so exit early
            let mut n_rcvd = 0;
            loop {
                match rx.recv() {
                    Ok(Some(Entry { tag, start, end, hist })) => {
                        // this fails silently
                        //
                        // improved implementation might include a logger so there
                        // is some record that it failed.
                        //
                        // alternatively, this could panic, so at least you know at
                        // the end it didn't work.
                        //
                        // the fact that `file` is created before the thread is spawned
                        // is mitigating, because typically if you can create the file,
                        // you can write to it, too.
                        wtr.write_histogram(&hist, start.duration_since(UNIX_EPOCH).unwrap(),
                                            end.duration_since(start).unwrap(), Tag::new(tag))
                            .ok();
                        n_rcvd += 1;
                    }

                    Ok(None) => break, // terminate signal sent by `Drop`

                    _ => thread::sleep(Duration::new(0, 1)), // nothing new, yield thread
                }
            }
            Ok(n_rcvd)
        }).map_err(Error::Io)
    }
}

impl Drop for HistLog {
    fn drop(&mut self) {
        // don't remember why this was added now ... presumably to
        // prepare the internal/queue state in some way.
        if !self.hist.is_empty() { self.send(Instant::now()) }

        if let Some(arc) = self.thread.take() {
            if let Ok(thread) = Arc::try_unwrap(arc) {
                let _ = self.tx.send(None);
                let _ = thread.join();
            }
        }
    }
}
