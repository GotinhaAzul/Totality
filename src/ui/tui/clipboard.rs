//! `/copy` and `/scrollback`: getting text out of the alternate screen.
//!
//! The TUI owns the screen and the mouse, so the terminal's own selection
//! and scrollback see nothing of the conversation. `/copy` puts the last
//! answer (or its last code block) on the clipboard with OSC 52, which works
//! over SSH and through tmux (`set-clipboard on`); `/scrollback` writes the
//! transcript to the normal screen so terminal search, tmux copy-mode, and
//! scrollback tools can reach it.

use super::*;

use crate::session::EntryKind;

/// The OSC 52 sequence that sets the clipboard to `text`.
pub(super) fn osc52(text: &str) -> String {
    format!("\x1b]52;c;{}\x07", base64(text.as_bytes()))
}

fn base64(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = match chunk.len() {
            3 => u32::from(chunk[0]) << 16 | u32::from(chunk[1]) << 8 | u32::from(chunk[2]),
            2 => u32::from(chunk[0]) << 16 | u32::from(chunk[1]) << 8,
            _ => u32::from(chunk[0]) << 16,
        };
        for index in 0..4 {
            if index <= chunk.len() {
                out.push(TABLE[(n >> (18 - 6 * index) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// The last fenced code block in `text`, without its fences. A fence is a
/// run of at least three backticks or tildes; the closing fence must use the
/// same character and be at least as long, so a four-backtick block can quote
/// a normal three-backtick one.
pub(super) fn last_code_block(text: &str) -> Option<String> {
    fn fence(line: &str) -> Option<(char, usize)> {
        let trimmed = line.trim_start();
        let character = trimmed.chars().next()?;
        if character != '`' && character != '~' {
            return None;
        }
        let length = trimmed.chars().take_while(|c| *c == character).count();
        (length >= 3).then_some((character, length))
    }

    let mut blocks = Vec::new();
    let mut current: Option<(char, usize, Vec<&str>)> = None;
    for line in text.lines() {
        if let Some((character, length)) = fence(line) {
            let closes = matches!(
                &current,
                Some((open, open_length, _))
                    if *open == character && length >= *open_length
            );
            if closes {
                if let Some((_, _, lines)) = current.take() {
                    blocks.push(lines.join("\n"));
                }
            } else if let Some((_, _, lines)) = current.as_mut() {
                // A shorter or different fence is content inside the block.
                lines.push(line);
            } else {
                current = Some((character, length, Vec::new()));
            }
        } else if let Some((_, _, lines)) = current.as_mut() {
            lines.push(line);
        }
    }
    // A cancelled or truncated answer can leave the final fence open. Keep
    // the text it did produce instead of falling back to an earlier block.
    if let Some((_, _, lines)) = current {
        let block = lines.join("\n");
        if !block.trim().is_empty() {
            blocks.push(block);
        }
    }
    blocks.pop()
}

impl App {
    /// The newest assistant reply with text on the active branch.
    fn last_answer(&self) -> Option<String> {
        let runtime = self.runtime.as_ref()?;
        let session = match runtime.session.lock() {
            Ok(session) => session,
            Err(poison) => poison.into_inner(),
        };
        session
            .branch_entries()
            .into_iter()
            .rev()
            .find_map(|entry| match &entry.kind {
                EntryKind::Assistant(data) => data
                    .content
                    .as_deref()
                    .filter(|text| !text.trim().is_empty())
                    .map(str::to_string),
                _ => None,
            })
    }

    /// `/copy [code]`: the last answer, or its last code block.
    pub(super) fn command_copy(&mut self, rest: &str) {
        let Some(answer) = self.last_answer() else {
            self.set_notice("nothing to copy yet", Role::Muted);
            return;
        };
        let (text, what) = match rest.trim() {
            "" => (answer, "the last answer"),
            "code" => match last_code_block(&answer) {
                Some(code) => (code, "the last code block"),
                None => {
                    self.set_notice("the last answer has no code block", Role::Muted);
                    return;
                }
            },
            _ => {
                self.set_notice("usage: /copy [code]", Role::Error);
                return;
            }
        };
        let mut out = std::io::stdout();
        let written = std::io::Write::write_all(&mut out, osc52(&text).as_bytes())
            .and_then(|()| std::io::Write::flush(&mut out));
        match written {
            Ok(()) => self.set_notice(
                format!(
                    "copied {what} ({} chars) via OSC 52; under tmux, set-clipboard must be on",
                    text.chars().count()
                ),
                Role::Success,
            ),
            Err(error) => self.set_notice(format!("copy failed: {error}"), Role::Error),
        }
    }

    /// The transcript as plain text at `width` columns.
    pub(super) fn transcript_plain(&mut self, width: usize) -> String {
        let show_reasoning = self.config.ui.show_reasoning;
        self.transcript.prepare(width, &self.theme, show_reasoning);
        let mut text: String = self
            .transcript
            .lines()
            .iter()
            .map(|line| {
                let mut row: String = line
                    .spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect();
                row.truncate(row.trim_end().len());
                row + "\n"
            })
            .collect();
        // The width used for the dump must not stick to the next frame.
        self.repaint = true;
        text.truncate(text.trim_end().len());
        text
    }

    /// `/scrollback`: write the transcript to the normal screen, where the
    /// terminal keeps it, then come back.
    pub(super) fn command_scrollback(&mut self) {
        let width = ratatui::crossterm::terminal::size()
            .map(|(columns, _)| columns as usize)
            .unwrap_or(100)
            .max(20);
        let text = self.transcript_plain(width);
        suspend_terminal(|| {
            let mut out = std::io::stdout();
            let _ = std::io::Write::write_all(&mut out, text.replace('\n', "\r\n").as_bytes());
            let _ = std::io::Write::write_all(&mut out, b"\r\n");
            let _ = std::io::Write::flush(&mut out);
        });
        self.title.clear();
        self.set_notice(
            "transcript written to the terminal's scrollback; it stays there after you quit",
            Role::Success,
        );
    }

    /// What to print on the normal screen after the TUI exits: how to come
    /// back to this session.
    pub(super) fn exit_summary(&self) -> Option<String> {
        let runtime = self.runtime.as_ref()?;
        let session = match runtime.session.lock() {
            Ok(session) => session,
            Err(poison) => poison.into_inner(),
        };
        let path = session.path.as_ref()?;
        if session.branch_entries().is_empty() {
            return None;
        }
        // `--resume <id>` only resolves inside this project's sessions
        // directory; a session opened from a path elsewhere needs that path.
        let project_dir = self.core().map(|core| core.paths.project_sessions_dir());
        let resume = if project_dir.as_deref() == path.parent() {
            session.id.clone()
        } else {
            path.display().to_string()
        };
        Some(format!(
            "Session {} saved to {}\nResume with: tot --resume {}   (or tot --continue)",
            short_session_id(&session.id),
            path.display(),
            resume
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn osc52_encodes_the_text_as_base64() {
        assert_eq!(base64(b"hi"), "aGk=");
        assert_eq!(base64(b"hello world"), "aGVsbG8gd29ybGQ=");
        assert_eq!(base64(b"abc"), "YWJj");
        assert_eq!(osc52("hi"), "\x1b]52;c;aGk=\x07");
    }

    #[test]
    fn the_last_code_block_is_found() {
        let text = "intro\n```rust\nfn a() {}\n```\nmid\n```\nls -la\n```\nend";
        assert_eq!(last_code_block(text).as_deref(), Some("ls -la"));
        assert_eq!(last_code_block("no code"), None);
    }

    #[test]
    fn nested_and_tilde_fences_are_understood() {
        // Four backticks quote a document that itself has a backtick block.
        let nested = "````markdown\n```rust\nfn a() {}\n```\n````";
        assert_eq!(
            last_code_block(nested).as_deref(),
            Some("```rust\nfn a() {}\n```")
        );
        let tilde = "~~~sh\nls -la\n~~~";
        assert_eq!(last_code_block(tilde).as_deref(), Some("ls -la"));
        // A shorter fence inside a longer one is content, not a close.
        let mixed = "````\n```\ninner\n```\n````";
        assert_eq!(last_code_block(mixed).as_deref(), Some("```\ninner\n```"));
    }

    #[test]
    fn an_unterminated_final_block_is_copied_not_an_earlier_one() {
        let text = "```\nfirst\n```\n```\nsecond";
        assert_eq!(last_code_block(text).as_deref(), Some("second"));
    }
}
