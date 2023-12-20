//! A tool for use with the `hdrhistogram` crate, a rust port of HdrHistogram, that provides
//! a clean interface with sane defaults for off-thread serialization of HdrHistogram interval
//! logs to file.
//!

// there's a bunch of places we .clone() what is either a &'static str or
// a SmolStr and it would be pretty tedious to work around the warning
#![cfg_attr(not(feature = "smol_str"), allow(noop_method_call))]

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
#[cfg(not(feature = "minstant"))]
use std::time::Instant;
use std::path::{Path, PathBuf};
use std::thread::{self, JoinHandle};
use std::io;
use std::{mem, fs};

use hdrhistogram::{Histogram};
use hdrhistogram::serialization::V2DeflateSerializer;
use hdrhistogram::serialization::interval_log::{IntervalLogWriterBuilder};
use crossbeam_channel as channel;
use chrono::Utc;
#[cfg(feature = "minstant")]
use minstant::Instant;
#[cfg(feature = "smol_str")]
use smol_str::SmolStr;

/// Type of value recorded to the hdrhistogram
pub type C = u64;

#[cfg(not(feature = "smol_str"))]
pub type SeriesName = &'static str;
#[cfg(feature = "smol_str")]
pub type SeriesName = SmolStr;

#[cfg(not(feature = "smol_str"))]
pub type Tag = &'static str;
#[cfg(feature = "smol_str")]
pub type Tag = SmolStr;

/// Significant figure passed to `hdrhistogram::Histogram::new` upon
/// construction
pub const DEFAULT_SIG_FIG: u8 = 3;
/// Capacity of `crossbeam_channel::bounded` queue used to communicate
/// between the measuring thread and the writer thread
pub const CHANNEL_SIZE: usize = 8;
/// Amount of time `HistLog::drop` will spin on a full channel to
/// the writer thread to send a terminate signal
pub const DROP_DEADLINE: Duration = Duration::from_millis(100);

/// Returns `Duration` as number of nanoseconds (`u64`)
///
/// # Examples
/// ```
/// # use std::time::*;
/// assert_eq!(histlog::nanos(Duration::from_secs(1)), 1_000_000_000u64);
/// ```
pub fn nanos(d: Duration) -> u64 {
    d.as_secs() * 1_000_000_000_u64 + (d.subsec_nanos() as u64)
}

/// Provides off-thread serialization of HdrHistogram interval logs to file.
///
/// # Purpose
///
/// HdrHistogram is often used to measure latency. Generally, if something is important
/// enough to measure latency, it's unlikely you want to write to a file on the same
/// thread.
///
/// One option would be to serialize to an in-memory buffer (e.g. `Vec<u8>`). However,
/// this would still require allocating to the buffer, and would eventually require a
/// lot of memory for a long-running process.
///
/// `HistLog` allows the hot thread to pass off it's `hdrhistogram::Histogram` at regular intervals
/// to a designated writer thread that can afford to dilly dally with IO. The interval
/// log is written incrementally and can be inspected and analyzed while the program
/// is still running.
///
/// `HistLog` relies completely on the rust port of `HdrHistogram`, both for the in-memory
/// recording of values and serialization. What it does provide is off-thread writing with
/// a clean interface and sane defaults that make it relatively easy to use.
///
/// # Examples
///
/// A `HistLog` has a "series" name and a "tag." The HdrHistogram interval log format provides
/// for one tag per entry. The series name is used to name the file the interval log is written to:
///
/// ```
/// use std::time::*;
///
/// let log_dir = "/tmp/path/to/logs";
/// let series = "server-latency";          // used to name the log file
/// let tag = "xeon-e7-8891-v2";            // recorded with each entry
/// let freq = Duration::from_secs(1);      // how often results sent to writer thread
///
/// // `HistLog::new` could fail creating file, `hdrhistogram::Histogram`
/// let mut server1 = histlog::HistLog::new(log_dir, series, tag, freq).unwrap();
///
/// // use `HistLog::clone_with_tag` to serialize a separate tag to same file.
/// let mut server2 = server1.clone_with_tag("xeon-e5-2670");
///
/// for i in 0..1000u64 { // dummy data
///     server1.record(i).unwrap(); // call to `hdrhistogram::Histogram::record` could fail
///     server2.record(i * 2).unwrap();
/// }
///
/// assert_eq!(server1.path(), server2.path()); // both being saved to same file, via same writer thread
/// ```
///
/// `HistLog`'s api design is built for event loops. Each iteration of the loop, new values are
/// recorded, and the current time is checked to see whether the current `Histogram` should be
/// passed off to the writer thread:
///
/// ```
/// use std::time::*;
/// #[cfg(feature = "minstant")]
/// use minstant::Instant;
///
/// let mut spintime = histlog::HistLog::new("/tmp/var/hist", "spintime", "main", Duration::from_secs(60)).unwrap();
///
/// let mut loop_time = Instant::now();
/// let mut prev: Instant;
///
/// loop {
///     prev = loop_time;
///     loop_time = Instant::now();
///     spintime.record(histlog::nanos(loop_time - prev)).unwrap(); // nanos: Duration -> u64
///     spintime.check_send(loop_time); // sends to writer thread if elapsed > freq,
///     // or...
///     spintime.check_try_send(loop_time).unwrap(); // non-blocking equivalent (can fail)
///
///     // do important stuff ...
///
/// # break
/// }
/// ```
///
/// # Logs
///
/// Logs are saved to `<log dir>/<series name>.<datetime>.hdrhistogram-interval-log.v2.gz`.
/// 
/// Format of log is like this:
/// 
/// ```console,ignore
/// #[StartTime: 1544631293.283 (seconds since epoch)]
/// #[BaseTime: 0.000 (seconds since epoch)]
/// Tag=xeon-e7-8891-v2,1544631293.283,0.003,999.000,HISTFAAAAC94Ae3GMRUAMAgD0bRI6FovNVcHmGREAgNR [...]
/// Tag=xeon-e5-2670,1544631293.283,0.003,999.000,HISTFAAAABx4AZNpmSzMwMDAxAABzFCaEUoz2X+AsQA/awK [...]
/// [...]
/// ```
/// 
/// Only the histogram data is compressed (deflate), so a `.gz` extension is perhaps misleading.
/// 
/// Log file can be viewed/analyzed [here](https://hdrhistogram.github.io/HdrHistogramJSDemo/logparser.html)
/// (javascript, runs locally) or with the Java-based [HistogramLogAnalyzer](https://github.com/HdrHistogram/HistogramLogAnalyzer).
///
/// [Full documentation](https://docs.rs/hdrhistogram/6.1.1/hdrhistogram/serialization/interval_log/index.html) of log
/// serialization available from the `hdrhistogram` crate.
///
/// # Features
///
/// - `minstant`: use `minstant::Instant` as a faster replacement for `std::time::Instant`
/// - `smol_str`: switch `&'static str` for `smol_str::SmolStr` for the `SeriesName` and `Tag`
///   types, allowing dynamic string values to be provided instead of static strings.
///
/// # Limitations
///
/// - `HistLog::check_send` and `HistLog::check_try_send` create a new `hdrhistogram::Histogram`
/// and send the current/prev one to the writer thread each interval. Internally, an
/// `hdrhistogram::Histogram` uses a `Vec` to store its counts, so there's an allocation involved.
/// - Only `u64` values can be recorded, currently.
///
pub struct HistLog {
    filename: PathBuf,
    series: SeriesName,
    tag: Tag,
    freq: Duration,
    last_sent: Instant,
    tx: channel::Sender<Option<Entry>>,
    hist: Histogram<C>,
    thread: Option<Arc<thread::JoinHandle<Result<usize, Error>>>>,
}

struct Entry {
    pub tag: Tag,
    pub start: SystemTime,
    pub end: SystemTime,
    pub hist: Histogram<C>,
}

/// Unifies all the errors that might occur from using a `HistLog` in one enum.
#[derive(Debug)]
pub enum Error {
    Io(io::Error),
    //HdrCreation(hdrhistogram::errors::CreationError),
    HdrRecord(hdrhistogram::errors::RecordError),
    TrySend(channel::TrySendError<()>),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            Error::Io(e) => write!(f, "Io({e})"),
            Error::HdrRecord(e) => write!(f, "HdrRecord({e})"),
            Error::TrySend(e) => write!(f, "TrySend({e})"),
        }
    }
}

impl std::error::Error for Error {}

impl From<io::Error> for Error {
    fn from(err: io::Error) -> Self {
        Error::Io(err)
    }
}

impl Clone for HistLog {
    fn clone(&self) -> Self {
        let thread = self.thread.as_ref().map(Arc::clone);
        Self {
            filename: self.filename.clone(),
            series: self.series.clone(),
            tag: self.tag.clone(),
            freq: self.freq,
            last_sent: Instant::now(),
            tx: self.tx.clone(),
            hist: self.hist.clone(),
            thread,
        }
    }
}

impl HistLog {
    /// Create a new `HistLog`.
    ///
    /// If `save_dir` does not exist, will attempt to create it (which could
    /// fail). Creating a new log file could fail. Spawning the writer thread could fail.
    ///
    /// Default significant figures of `3` is used.
    #[cfg(not(feature = "smol_str"))]
    pub fn new<P>(save_dir: P, series: SeriesName, tag: Tag, freq: Duration) -> Result<Self, Error>
        where P: AsRef<Path>
    {
        Self::new_with_sig_fig(DEFAULT_SIG_FIG, save_dir, series, tag, freq)
    }

    /// Create a new `HistLog`.
    ///
    /// If `save_dir` does not exist, will attempt to create it (which could
    /// fail). Creating a new log file could fail. Spawning the writer thread could fail.
    ///
    /// Default significant figures of `3` is used.
    #[cfg(feature = "smol_str")]
    pub fn new<P, S, T>(save_dir: P, series: S, tag: T, freq: Duration) -> Result<Self, Error>
        where P: AsRef<Path>,
              S: AsRef<str>,
              T: AsRef<str>
    {
        let series = SmolStr::new(series.as_ref());
        let tag = SmolStr::new(tag.as_ref());
        Self::new_with_sig_fig(DEFAULT_SIG_FIG, save_dir, series, tag, freq)
    }

    /// Create a new `HistLog`, specifying the number of significant digits
    #[cfg(not(feature = "smol_str"))]
    pub fn new_with_sig_fig<P>(
        sig_fig: u8,
        save_dir: P,
        series: SeriesName,
        tag: Tag,
        freq: Duration,
    ) -> Result<Self, Error>
        where P: AsRef<Path>
    {
        Self::inner_new(sig_fig, save_dir, series, tag, freq)
    }

    /// Create a new `HistLog`, specifying the number of significant digits
    #[cfg(feature = "smol_str")]
    pub fn new_with_sig_fig<P, S, T>(
        sig_fig: u8,
        save_dir: P,
        series: S,
        tag: T,
        freq: Duration,
    ) -> Result<Self, Error>
        where P: AsRef<Path>,
              S: AsRef<str>,
              T: AsRef<str>
    {
        let series = SmolStr::new(series.as_ref());
        let tag = SmolStr::new(tag.as_ref());
        Self::inner_new(sig_fig, save_dir, series, tag, freq)
    }

    #[allow(clippy::needless_borrows_for_generic_args)]
    fn inner_new<P>(
        sig_fig: u8,
        save_dir: P,
        series: SeriesName,
        tag: Tag,
        freq: Duration,
    ) -> Result<Self, Error>
        where P: AsRef<Path>
    {
        let save_dir = save_dir.as_ref().to_path_buf();
        let scribe_series = series.clone();
        let filename = Self::get_filename(&save_dir, &series);
        let (tx, rx) = channel::bounded(CHANNEL_SIZE);
        let thread = Some(Arc::new(Self::scribe(scribe_series, rx, filename.as_path())?));
        let last_sent = Instant::now();
        let hist = Histogram::new(sig_fig).expect("Histogram::new"); //.map_err(Error::HdrCreation)?;
        Ok(Self { filename, series, tag, freq, last_sent, tx, hist, thread })
    }

    // not sure if this is a good thing to have
    //
    #[doc(hidden)]
    pub fn new_with_tag(&self, tag: Tag) -> Result<Self, Error> {
        let mut save_dir = self.filename.clone();
        if !save_dir.pop() { // `.pop` should remove the file name, leaving dir
            return Err(Error::Io(io::Error::new(io::ErrorKind::Other,
                "`filename.pop()` returned `false`! expected it to have a file name, return `true`.")))
        }
        Self::new(save_dir, self.series.clone(), tag, self.freq)
    }

    /// Returns the path of the log file the `HistLog` is writing to.
    ///
    pub fn path(&self) -> &Path { self.filename.as_path() }

    /// Record a new histogram with a `tag` that will serialize to the
    /// same interval log file as its parent. Each cloned `HistLog`'s entries
    /// will be written to their own lines in the log file, identifiable by tag.
    ///
    /// # Limitations
    ///
    /// No effort is made to check whether `tag` is a duplicate of a previous tag,
    /// and using a duplicate may produce unexpected results.
    #[cfg(not(feature = "smol_str"))]
    pub fn clone_with_tag(&self, tag: Tag) -> Self {
        self.inner_clone_with_tag(tag)
    }

    #[cfg(feature = "smol_str")]
    pub fn clone_with_tag<T: AsRef<str>>(&self, tag: T) -> Self {
        let tag = SmolStr::new(tag.as_ref());
        self.inner_clone_with_tag(tag)
    }

    fn inner_clone_with_tag(&self, tag: Tag) -> Self {
        assert!(self.thread.is_some(),
            "self.thread cannot be `None` unless `HistLog` was already dropped");
        let thread = self.thread.as_ref().map(Arc::clone).unwrap();
        let tx = self.tx.clone();
        Self {
            filename: self.filename.clone(),
            series: self.series.clone(),
            tag,
            freq: self.freq,
            last_sent: Instant::now(),
            tx,
            hist: self.hist.clone(),
            thread: Some(thread),
        }
    }

    #[doc(hidden)]
    pub fn clone_with_tag_and_freq(&self, tag: Tag, freq: Duration) -> Self {
        let mut clone = self.clone_with_tag(tag);
        clone.freq = freq;
        clone
    }

    /// Record a single value to the histogram. This could fail if the value
    /// is outside of the highest range permitted. See the
    /// [`hdrhistogram` docs](https://docs.rs/hdrhistogram/6.1.1/hdrhistogram/struct.Histogram.html#method.record)
    /// for further deails. The `hdrhistogram::Histogram` used by `HistLog`
    /// is created with a significant figure of 3 (`histlog::SIG_FIG` const).
    ///
    #[inline]
    pub fn record(&mut self, value: u64) -> Result<(), Error> {
        self.hist.record(value).map_err(Error::HdrRecord)
    }

    /// Reset the state of the internal histogram and the last sent value.
    ///
    /// One situation this might be used is if there was a pause in recording.
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
        self.tx.send(Some(Entry { tag: self.tag.clone(), start, end, hist: next })).ok(); //.expect("sending entry failed");
        self.last_sent = loop_time;
    }

    fn try_send(&mut self, loop_time: Instant) -> Result<(), Error>{
        let end = SystemTime::now();
        let start = end - (loop_time - self.last_sent);
        assert!(end > start, "end <= start!");
        let mut next = Histogram::new_from(&self.hist);
        mem::swap(&mut self.hist, &mut next);
        let entry = Entry { tag: self.tag.clone(), start, end, hist: next };
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

    /// Send the current histogram to the writer thread if the elapsed time
    /// since the last send is greater than the interval frequency.
    ///
    /// If the channel is disconnected, this will fail silently, instead of panicking.
    ///
    pub fn check_send(&mut self, loop_time: Instant) -> bool {
        let expired = loop_time > self.last_sent && loop_time - self.last_sent >= self.freq;
        if expired { self.send(loop_time); }
        expired
    }

    /// Non-blocking variant of `HistLog::check_send`, which will also return any errors,
    /// including a disconnected channel, encountered while trying to send to the
    /// writer thread.
    ///
    #[inline]
    pub fn check_try_send(&mut self, loop_time: Instant) -> Result<bool, Error> {
        let elapsed = loop_time.saturating_duration_since(self.last_sent);
        let expired = elapsed >= self.freq;
        if expired { self.try_send(loop_time)?; }
        Ok(expired)
    }

    fn get_filename<S: AsRef<str>>(save_dir: &Path, series: S) -> PathBuf {
        use rand::prelude::*;
        let now = Utc::now();
        let id: u32 = thread_rng().gen();
        let series: &str = series.as_ref();
        let filename =
            format!("{series}.{time}.{id:x}.hlog",
                series = series, 
                time = now.format("%Y-%m-%d-%H%M%SZ"));
        save_dir.join(filename)
    }

    fn ensure_parent_dir_exists<P: AsRef<Path>>(path: P) -> Result<(), io::Error> {
        let path: &Path = path.as_ref();
        match path.parent() {
            Some(parent) if parent.exists() => Ok(()),
            Some(parent) => {
                std::fs::create_dir_all(parent)?;
                Ok(())
            }

            None => {
                Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("parent path (of {}) is not a directory", path.display()),
                ))
            }
        }
    }

    fn scribe(
        series: SeriesName,
        rx: channel::Receiver<Option<Entry>>,
        filename: &Path,
    ) -> Result<JoinHandle<Result<usize, Error>>, Error> {
        let mut ser = V2DeflateSerializer::new();
        let start_time = SystemTime::now();
        Self::ensure_parent_dir_exists(filename)?;
        let file = fs::File::create(filename).map_err(Error::Io)?;
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
                        // TODO: this currently fails silently
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
                        //
                        #[cfg(feature = "smol_str")]
                        let tag: &str = tag.as_ref();
                        wtr.write_histogram(&hist, start.duration_since(UNIX_EPOCH).unwrap(),
                                            end.duration_since(start).unwrap(), hdrhistogram::serialization::interval_log::Tag::new(tag))
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
    /// Checks if the current instance is the last remaining instance with a reference
    /// to the underlying writer thread, and, if so, sends a terminate signal to the
    /// writer thread and attempts to join it.
    ///
    /// # May Pause Up To 5ms
    ///
    /// In the event the channel to the writer thread is full, will continue trying
    /// to send a terminate command (busy polling the channel) until `DROP_DEADLINE`
    /// has expired (currently 5ms), upon which it will abort.
    ///
    /// If channel is disconnected, will simply abort without trying to join the
    /// writer thread.
    ///
    fn drop(&mut self) {
        // don't remember why this was added now ... presumably to
        // prepare the internal/queue state in some way.
        if !self.hist.is_empty() { self.send(Instant::now()) }

        if let Some(arc) = self.thread.take() {
            if let Ok(thread) = Arc::try_unwrap(arc) {
                let start = Instant::now();
                while Instant::now() - start < DROP_DEADLINE {
                    match self.tx.try_send(None) {
                        Ok(_) => {
                            let _ = thread.join();
                            break
                        }

                        Err(channel::TrySendError::Full(_)) => {}

                        Err(_) => {
                            break
                        }
                    }
                }
            }
        }
    }
}

#[allow(unused)]
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_histlog_record_one_and_drop() {
        let mut hist = HistLog::new("/tmp/histlog", "test", "red", Duration::from_millis(1)).unwrap();
        for i in 0..1000u64 {
            hist.record(i).unwrap();
        }
        assert_eq!(hist.check_send(Instant::now()), false);
        assert!(hist.check_try_send(Instant::now()).is_ok());
        assert_eq!(hist.check_try_send(Instant::now()).unwrap(), false);
        thread::sleep(Duration::from_millis(3));
        assert_eq!(hist.check_send(Instant::now()), true);
        let path = hist.filename.clone();
        drop(hist);
        assert!(path.exists());
    }

    #[test]
    fn clone_it() {
        let mut hist = HistLog::new("/tmp/histlog", "test", "red", Duration::from_millis(1)).unwrap();
        let tx = hist.tx.clone();
        let mut a = hist.clone_with_tag("blue");
        for i in 0..1000u64 {
            hist.record(i).unwrap();
            a.record(i * 2).unwrap();
        }
        drop(hist);
        drop(a);
        match tx.try_send(None) {
            Err(channel::TrySendError::Disconnected(None)) => {},
            other => panic!("unexpected variant: {:?}", other)
        }
    }

    #[test]
    fn generated_hlog_filenames_are_unique() {
        let save_dir = Path::new("a/b/c");
        let series = "d-e-f";
        let filenames: Vec<PathBuf> = (0..1000)
            .map(|_| HistLog::get_filename(&save_dir, &series))
            .collect();
        let unique = filenames.iter().cloned().collect::<std::collections::HashSet<_>>();
        assert_eq!(filenames.len(), unique.len());
    }
}
