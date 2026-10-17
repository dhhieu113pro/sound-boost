//! Opt-in diagnostics: what the audio engine actually asks of us.
//!
//! Compiled out unless the `diagnostics` cargo feature is on:
//!
//! ```text
//! cargo build --release -p sb-apo --features diagnostics
//! ```
//!
//! The log exists because the interesting failures here are invisible from
//! outside. A symptom like "the DLL is loaded into `audiodg.exe` but the shared
//! control block was never created" has a dozen possible causes, and the audio
//! engine reports none of them: it simply drops an APO it does not like. The
//! only way to tell those causes apart is to write down what happened, at the
//! moment it happened, from inside the effect.
//!
//! Everything logged here is written from a control path: `Initialize`, the
//! format queries, lock/unlock, `GetLatency`. The real-time callback
//! (`APOProcess`) may not log at all -- not to a file, not even the `format!`
//! that builds the line, because formatting allocates and an allocation can
//! wait on a lock the audio thread must never wait on. It calls
//! [`rt_processed`] instead, which is two relaxed atomic increments, and the
//! next control-path call flushes those counters into the log.

#[cfg(feature = "diagnostics")]
use std::sync::atomic::{AtomicU64, Ordering};
#[cfg(feature = "diagnostics")]
use std::sync::Mutex;

use crate::abi::GUID;
use crate::format::{SampleFormat, StreamFormat};

/// Where the log goes: the first of these the caller can actually write.
///
/// More than one, because the process doing the writing is `audiodg.exe`,
/// running as `LOCAL SERVICE` with whatever token the audio engine handed it. A
/// directory the interactive user can write to is not necessarily writable for
/// that process -- it may run with a restricted token, or at a lower integrity
/// level than the target directory. `C:\Users\Public` comes first so the log can
/// be read without elevation; the other two are fallbacks (`%TEMP%` for
/// `audiodg.exe` is the service account's own temp directory, which it owns).
pub fn candidates() -> Vec<std::path::PathBuf> {
    let mut paths = vec![std::path::PathBuf::from(r"C:\Users\Public\sb-apo-debug.log")];
    paths.push(std::env::temp_dir().join(FILE_NAME));
    paths.push(std::path::PathBuf::from(r"C:\Windows\Temp\sb-apo-debug.log"));
    paths
}

/// The name of the log file itself.
pub const FILE_NAME: &str = "sb-apo-debug.log";

/// Serialises writers, so a trace is a sequence of whole lines.
///
/// Only control paths take this. The audio thread never does -- see
/// [`rt_processed`].
#[cfg(feature = "diagnostics")]
static SERIAL: Mutex<()> = Mutex::new(());

/// Monotonic line counter, so a trace can be put in order across threads.
#[cfg(feature = "diagnostics")]
static SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Buffers the audio thread has processed, frames it has processed, and how
/// many buffers have already been written out.
#[cfg(feature = "diagnostics")]
static RT_BUFFERS: AtomicU64 = AtomicU64::new(0);
#[cfg(feature = "diagnostics")]
static RT_FRAMES: AtomicU64 = AtomicU64::new(0);
#[cfg(feature = "diagnostics")]
static RT_REPORTED: AtomicU64 = AtomicU64::new(0);

/// Append one line, ignoring every failure.
///
/// Diagnostics must never be the reason the audio engine has a bad day, so this
/// returns nothing and swallows the errors.
///
/// Two details are load-bearing for reading the result afterwards:
///
/// * Each line is built as one `String` and written with a single `write_all`.
///   A `write!` with format arguments issues several writes, and the engine
///   calls us from several threads at once: measured on a busy machine, lines
///   from two threads splice into each other character by character. One
///   buffer in, one line out.
/// * Every line carries a sequence number and the calling thread, so a trace
///   can be put back in order and calls from the engine's control thread can be
///   told apart from calls from an audio thread.
#[cfg(feature = "diagnostics")]
pub fn log(role: &str, event: &str, detail: &str) {
    let guard = SERIAL.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    write_line(&line_text(role, event, detail));
    flush_rt_locked();
    drop(guard);
}

/// Prefix a line with its sequence number and thread.
#[cfg(feature = "diagnostics")]
fn line_text(role: &str, event: &str, detail: &str) -> String {
    let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
    format!(
        "[{sequence} {:?}] [{role}] {event}: {detail}\n",
        std::thread::current().id()
    )
}

/// Write a line to the first log path that accepts it. Caller holds the lock.
#[cfg(feature = "diagnostics")]
fn write_line(line: &str) {
    use std::io::Write;
    for path in candidates() {
        let opened = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path);
        if let Ok(mut file) = opened {
            let _ = file.write_all(line.as_bytes());
            return;
        }
    }
}

/// Record a processed buffer from the audio thread.
///
/// Atomics only: no allocation, no lock, nothing that can block. The evidence
/// that the effect really ran is collected here and written out by the next
/// control-path call, which is why a trace can show `LockForProcess` followed
/// by an `[rt]` line later in the file.
#[cfg(feature = "diagnostics")]
#[inline]
pub fn rt_processed(frames: usize) {
    RT_BUFFERS.fetch_add(1, Ordering::Relaxed);
    RT_FRAMES.fetch_add(frames as u64, Ordering::Relaxed);
}

/// The normal build has no counters to bump.
#[cfg(not(feature = "diagnostics"))]
#[inline(always)]
pub fn rt_processed(_frames: usize) {}

/// Report the real-time counters if they moved since the last report.
///
/// Called with the log lock held, from control paths only.
#[cfg(feature = "diagnostics")]
fn flush_rt_locked() {
    let processed = RT_BUFFERS.load(Ordering::Relaxed);
    let reported = RT_REPORTED.load(Ordering::Relaxed);
    if processed <= reported {
        return;
    }
    let frames = RT_FRAMES.load(Ordering::Relaxed);
    RT_REPORTED.store(processed, Ordering::Relaxed);
    write_line(&line_text(
        "rt",
        "APOProcess",
        &format!("{processed} buffers / {frames} frames processed on the audio thread"),
    ));
}

/// The normal build's version: nothing is written and nothing is formatted.
#[cfg(not(feature = "diagnostics"))]
#[inline(always)]
pub fn log(_role: &str, _event: &str, _detail: &str) {}

/// Render a CLSID the way the registry does, so it can be matched by eye.
pub fn guid_text(guid: &GUID) -> String {
    format!(
        "{{{:08X}-{:04X}-{:04X}-{:02X}{:02X}-{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}}}",
        guid.data1,
        guid.data2,
        guid.data3,
        guid.data4[0],
        guid.data4[1],
        guid.data4[2],
        guid.data4[3],
        guid.data4[4],
        guid.data4[5],
        guid.data4[6],
        guid.data4[7],
    )
}

/// Describe a stream the way the engine's own tools would.
pub fn format_text(format: &StreamFormat) -> String {
    format!(
        "{} Hz, {} ch, {}",
        format.sample_rate,
        format.channels,
        match format.sample_format {
            SampleFormat::Float32 => "float32",
            SampleFormat::Pcm16 => "pcm16",
            SampleFormat::Pcm32 => "pcm32",
        }
    )
}
