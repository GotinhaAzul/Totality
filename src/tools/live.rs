//! A live tail of a running tool's output, for the frontend only.
//!
//! The agent loop installs a [`LiveSink`] around each tool call; `bash`
//! feeds what the command prints into a [`LiveTail`] and the sink forwards
//! the last few lines a few times a second as
//! [`crate::agent::AgentEvent::ToolProgress`]. The model never sees this: it
//! still gets the capped final result.

use std::future::Future;
use std::sync::{Arc, Mutex};

use tokio::sync::mpsc::UnboundedSender;

use crate::agent::AgentEvent;

/// Lines of a running command shown under its tool row.
pub const TAIL_LINES: usize = 3;
/// Bytes of recent output kept to cut the tail from.
const TAIL_BYTES: usize = 8 * 1024;

/// Where a running call's progress goes.
#[derive(Clone)]
pub struct LiveSink {
    pub call_id: String,
    pub tx: UnboundedSender<AgentEvent>,
}

tokio::task_local! {
    static SINK: LiveSink;
}

/// Run `future` with `sink` installed for the tool it calls.
pub async fn with_sink<F: Future>(sink: LiveSink, future: F) -> F::Output {
    SINK.scope(sink, future).await
}

/// The sink installed for the current tool call, if any.
pub fn current() -> Option<LiveSink> {
    SINK.try_with(Clone::clone).ok()
}

/// Recent output from one or more streams, shared with their readers.
#[derive(Clone, Default)]
pub struct LiveTail {
    inner: Arc<Mutex<TailState>>,
}

#[derive(Default)]
struct TailState {
    bytes: Vec<u8>,
    /// Bumped on every append, so the sender knows when to send.
    version: u64,
}

impl LiveTail {
    pub fn push(&self, chunk: &[u8]) {
        let Ok(mut state) = self.inner.lock() else {
            return;
        };
        state.bytes.extend_from_slice(chunk);
        if state.bytes.len() > TAIL_BYTES {
            let excess = state.bytes.len() - TAIL_BYTES;
            state.bytes.drain(..excess);
        }
        state.version += 1;
    }

    /// The last [`TAIL_LINES`] non-empty lines, cleaned for display, and the
    /// version they come from.
    pub fn snapshot(&self) -> (u64, String) {
        let Ok(state) = self.inner.lock() else {
            return (0, String::new());
        };
        let text = String::from_utf8_lossy(&state.bytes);
        let text = crate::sanitize::terminal_output(&text);
        let lines: Vec<&str> = text
            .lines()
            .filter(|line| !line.trim().is_empty())
            .collect();
        let start = lines.len().saturating_sub(TAIL_LINES);
        (state.version, lines[start..].join("\n"))
    }
}

/// Send `tail` to `sink` whenever it changed since `last` (a version).
pub fn send_if_changed(sink: &LiveSink, tail: &LiveTail, last: &mut u64) {
    let (version, text) = tail.snapshot();
    if version == *last {
        return;
    }
    *last = version;
    let _ = sink.tx.send(AgentEvent::ToolProgress {
        call_id: sink.call_id.clone(),
        tail: text,
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_tail_keeps_the_last_lines_and_drops_escapes() {
        let tail = LiveTail::default();
        tail.push(b"tick 1\ntick 2\n");
        tail.push(b"\x1b[32mtick 3\x1b[0m\n\ntick 4\n");
        let (version, text) = tail.snapshot();
        assert_eq!(version, 2);
        assert_eq!(text, "tick 2\ntick 3\ntick 4");
    }
}
