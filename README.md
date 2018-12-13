# histlog

![](https://img.shields.io/crates/v/histlog.svg) ![](https://docs.rs/histlog/badge.svg)

Provides off-thread serialization of HdrHistogram interval logs to file.

For use with the [`hdrhistogram`](https://crates.io/crates/hdrhistogram) crate,
a rust port of Gil Tene's [HdrHistogram](https://hdrhistogram.github.io/HdrHistogram/),
that provides a clean interface with sane defaults for off-thread serialization
of HdrHistogram interval logs to file.

## purpose

HdrHistogram is often used to measure latency. Generally, if something is important
enough to measure latency, it's unlikely you want to write to a file on the same
thread.

One option would be to serialize to an in-memory buffer (e.g. `Vec<u8>`). However,
this would still require allocating to the buffer, and would eventually require a
lot of memory for a long-running process.

`histlog::HistLog` allows the hot thread to pass off it's `hdrhistogram::Histogram` at regular intervals
to a designated writer thread that can afford to dilly dally with IO. The interval
log is written incrementally and can be inspected and analyzed while the program
is still running.

`HistLog` relies completely on the `hdrhistogram` crate, both for the in-memory
recording of values and serialization. What it does provide is off-thread writing with
a clean interface and sane defaults that make it relatively easy to use.

## examples

A `HistLog` has a "series" name and a "tag." The HdrHistogram interval log format provides
for one tag per entry. The series name is used to name the file the interval log is written to:

```rust
use std::time::*;

let log_dir = "/tmp/path/to/logs";
let series = "server-latency";          // used to name the log file
let tag = "xeon-e7-8891-v2";            // recorded with each entry
let freq = Duration::from_secs(1);      // how often results sent to writer thread

// `HistLog::new` could fail creating file, `hdrhistogram::Histogram`
let mut server1 = histlog::HistLog::new(log_dir, series, tag, freq).unwrap();

// use `HistLog::clone_with_tag` to serialize a separate tag to same file.
let mut server2 = server1.clone_with_tag("xeon-e5-2670");

for i in 0..1000u64 { // dummy data
    server1.record(i).unwrap(); // call to `hdrhistogram::Histogram::record` could fail
    server2.record(i * 2).unwrap();
}

assert_eq!(server1.path(), server2.path()); // both being saved to same file, via same writer thread
```

`HistLog`'s api design is built for event loops. Each iteration of the loop, new values are
recorded, and the current time is checked to see whether the current `Histogram` should be
passed off to the writer thread:

```rust
use std::time::*;

let mut spintime = histlog::HistLog::new("/tmp/var/hist", "spintime", "main", Duration::from_secs(60)).unwrap();

let mut loop_time = Instant::now();
let mut prev: Instant;

loop {
    prev = loop_time;
    loop_time = Instant::now();
    spintime.record(histlog::nanos(loop_time - prev)).unwrap(); // nanos: Duration -> u64
    spintime.check_send(loop_time); // sends to writer thread if elapsed > freq,
    // or...
    spintime.check_try_send(loop_time).unwrap(); // non-blocking equivalent (can fail)

    // do important stuff ...
}
```

## logs

Logs are saved to `<log dir>/<series name>.<datetime>.hdrhistogram-interval-log.v2.gz`.

Format of log is like this:

```console,ignore
#[StartTime: 1544631293.283 (seconds since epoch)]
#[BaseTime: 0.000 (seconds since epoch)]
Tag=xeon-e7-8891-v2,1544631293.283,0.003,999.000,HISTFAAAAC94Ae3GMRUAMAgD0bRI6FovNVcHmGREAgNR [...]
Tag=xeon-e5-2670,1544631293.283,0.003,999.000,HISTFAAAABx4AZNpmSzMwMDAxAABzFCaEUoz2X+AsQA/awK [...]
[...]
```

Only the histogram data is compressed (deflate), so a `.gz` extension is perhaps misleading.

Log file can be viewed/analyzed [here](https://hdrhistogram.github.io/HdrHistogramJSDemo/logparser.html)
(javascript, runs locally) or with the Java-based [HistogramLogAnalyzer](https://github.com/HdrHistogram/HistogramLogAnalyzer).

[Full documentation](https://docs.rs/hdrhistogram/6.1.1/hdrhistogram/serialization/interval_log/index.html) of log
serialization available from the `hdrhistogram` crate.

## limitations

- The series name and tags are currently limited to `&'static str` because the overhead of using
`String` is prohibitive. This may change in future versions if a performant means of
allowing dynamic tags presents itself that's not inordinately complicated to use.
- `HistLog::check_send` and `HistLog::check_try_send` create a new `hdrhistogram::Histogram`
and send the current/prev one to the writer thread each interval. Internally, an
`hdrhistogram::Histogram` uses a `Vec` to store its counts, so there's an allocation involved.
- Only `u64` values can be recorded, currently.
