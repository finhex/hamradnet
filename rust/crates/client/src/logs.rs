//! Recent log lines, kept for the app's log window and `hamradnet logs`.
//!
//! Every log event still goes to stderr as before; this keeps a copy of the
//! most recent ones so the app can show them, and copy them for whoever is
//! helping when something goes wrong.

use std::collections::VecDeque;
use std::io::{self, Write};
use std::sync::{Arc, Mutex};

use tracing_subscriber::fmt::MakeWriter;

/// How many recent lines are kept. Older ones are dropped; the terminal still
/// has everything.
const KEEP: usize = 2000;

#[derive(Clone, Default)]
pub struct LogBuffer {
    inner: Arc<Mutex<Lines>>,
}

#[derive(Default)]
struct Lines {
    lines: VecDeque<String>,
    /// Every line ever added, so a viewer can tell whether anything is new
    /// even once old lines start being dropped.
    added: u64,
}

impl LogBuffer {
    /// The lines held as one text, how many there are, and the running count
    /// they end at.
    pub fn snapshot(&self) -> (String, usize, u64) {
        let guard = self.inner.lock().unwrap();
        let text = guard.lines.iter().map(String::as_str).collect::<Vec<_>>().join("\n");
        (text, guard.lines.len(), guard.added)
    }

    fn push(&self, text: &str) {
        let mut guard = self.inner.lock().unwrap();
        for line in text.trim_end_matches('\n').split('\n') {
            guard.lines.push_back(line.to_string());
            guard.added += 1;
        }
        while guard.lines.len() > KEEP {
            guard.lines.pop_front();
        }
    }
}

/// One log event on its way out. The formatter may write an event in several
/// pieces, so it is collected whole and handed on when the formatter is done
/// with it.
pub struct EventWriter {
    buffer: LogBuffer,
    bytes: Vec<u8>,
}

impl Write for EventWriter {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        self.bytes.extend_from_slice(data);
        Ok(data.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Drop for EventWriter {
    fn drop(&mut self) {
        let _ = io::stderr().write_all(&self.bytes);
        self.buffer.push(&String::from_utf8_lossy(&self.bytes));
    }
}

impl<'a> MakeWriter<'a> for LogBuffer {
    type Writer = EventWriter;

    fn make_writer(&'a self) -> Self::Writer {
        EventWriter { buffer: self.clone(), bytes: Vec::new() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_the_most_recent_lines_and_counts_them_all() {
        let logs = LogBuffer::default();
        for i in 0..KEEP + 5 {
            logs.push(&format!("line {i}\n"));
        }
        let (text, count, added) = logs.snapshot();
        assert_eq!(count, KEEP);
        assert_eq!(added, (KEEP + 5) as u64);
        assert!(text.starts_with("line 5\n"));
        assert!(text.ends_with(&format!("line {}", KEEP + 4)));
    }
}
