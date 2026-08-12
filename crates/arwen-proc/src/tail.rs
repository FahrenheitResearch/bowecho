// SPDX-License-Identifier: Apache-2.0

//! Incremental JSONL tailing of `events.jsonl`.
//!
//! This is the ONE event source in Studio: a live run (the child's stdout
//! is redirected into the file), a reattach after Studio restarted, and a
//! fixture replay all read through here. The stdout pipe is never the
//! source of truth (recon rule: reattach = heartbeat + events replay).

use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use arwen_plan::events::{RunEventEnvelope, parse_event_line};

/// Tail state: byte offset consumed so far plus the trailing partial line
/// (a writer may be mid-line at any poll; the partial is held back until
/// its newline arrives).
pub struct JsonlTail {
    path: PathBuf,
    offset: u64,
    partial: String,
}

/// One poll's harvest.
#[derive(Debug, Default)]
pub struct TailBatch {
    pub events: Vec<RunEventEnvelope>,
    /// Malformed complete lines (never fatal to the stream; reported).
    pub errors: Vec<String>,
}

impl JsonlTail {
    /// Tail from the start of the file (replay-then-follow).
    pub fn from_start(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            offset: 0,
            partial: String::new(),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Read every complete line appended since the last poll. A missing
    /// file is an empty batch (the run may not have started writing yet).
    /// A file shorter than our offset means truncation/replacement — the
    /// tail restarts from the beginning rather than reading garbage.
    pub fn poll(&mut self) -> TailBatch {
        let mut batch = TailBatch::default();
        let Ok(mut file) = std::fs::File::open(&self.path) else {
            return batch;
        };
        let length = match file.metadata() {
            Ok(metadata) => metadata.len(),
            Err(error) => {
                batch.errors.push(format!("stat events.jsonl: {error}"));
                return batch;
            }
        };
        if length < self.offset {
            self.offset = 0;
            self.partial.clear();
        }
        if length == self.offset {
            return batch;
        }
        if file.seek(SeekFrom::Start(self.offset)).is_err() {
            return batch;
        }
        // Per-poll byte budget: the engine's in-process observer emits
        // model_progress at FULL step cadence, and a reattach replays the
        // whole history — cap one poll's read so a huge backlog becomes
        // several frames instead of one long one. Leftovers are picked up
        // by the next poll.
        const MAX_POLL_BYTES: u64 = 8 * 1024 * 1024;
        let want = (length - self.offset).min(MAX_POLL_BYTES);
        let mut bytes = Vec::new();
        if let Err(error) = file.take(want).read_to_end(&mut bytes) {
            batch.errors.push(format!("read events.jsonl: {error}"));
            return batch;
        }
        self.offset += bytes.len() as u64;
        // Writers emit UTF-8; a split multi-byte char at the chunk edge is
        // tolerated by lossy conversion only at the partial boundary.
        let text = String::from_utf8_lossy(&bytes);
        let mut buffer = std::mem::take(&mut self.partial);
        buffer.push_str(&text);
        let mut start = 0usize;
        while let Some(newline) = buffer[start..].find('\n') {
            let line = &buffer[start..start + newline];
            match parse_event_line(line) {
                Ok(Some(envelope)) => batch.events.push(envelope),
                Ok(None) => {}
                Err(error) => batch.errors.push(error),
            }
            start += newline + 1;
        }
        self.partial = buffer[start..].to_string();
        batch
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn temp_path(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "arwen-proc-tail-{label}-{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ))
    }

    const LINE_A: &str = r#"{"schema_version":"gpuwm.run-plan.event.v1","sequence":0,"emitted_unix_ms":1,"event":"stage_started","stage":"fetch"}"#;
    const LINE_B: &str = r#"{"schema_version":"gpuwm.run-plan.event.v1","sequence":1,"emitted_unix_ms":2,"event":"completed"}"#;

    #[test]
    fn missing_file_then_appends_then_partial_lines() {
        let path = temp_path("basic");
        let mut tail = JsonlTail::from_start(&path);
        assert!(tail.poll().events.is_empty());

        let mut file = std::fs::File::create(&path).unwrap();
        writeln!(file, "{LINE_A}").unwrap();
        // Partial second line: held back until its newline lands.
        write!(file, "{}", &LINE_B[..40]).unwrap();
        file.flush().unwrap();
        let batch = tail.poll();
        assert_eq!(batch.events.len(), 1);
        assert!(batch.errors.is_empty());

        writeln!(file, "{}", &LINE_B[40..]).unwrap();
        file.flush().unwrap();
        let batch = tail.poll();
        assert_eq!(batch.events.len(), 1);
        assert!(batch.events[0].event.is_terminal());

        drop(file);
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn malformed_line_is_reported_not_fatal() {
        let path = temp_path("malformed");
        std::fs::write(&path, format!("{LINE_A}\nnot json\n{LINE_B}\n")).unwrap();
        let mut tail = JsonlTail::from_start(&path);
        let batch = tail.poll();
        assert_eq!(batch.events.len(), 2);
        assert_eq!(batch.errors.len(), 1);
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn truncated_file_restarts_from_the_beginning() {
        let path = temp_path("truncate");
        std::fs::write(&path, format!("{LINE_A}\n{LINE_B}\n")).unwrap();
        let mut tail = JsonlTail::from_start(&path);
        assert_eq!(tail.poll().events.len(), 2);
        std::fs::write(&path, format!("{LINE_A}\n")).unwrap();
        let batch = tail.poll();
        assert_eq!(batch.events.len(), 1, "restarted after truncation");
        std::fs::remove_file(&path).unwrap();
    }
}
