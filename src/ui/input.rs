//! Multiline input, history, and slash-command completion.

use std::io::Write as _;
use std::path::PathBuf;

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::style::Modifier;
use ratatui::style::Style;
use ratatui::widgets::{Block, Borders};
use ratatui_textarea::{CursorMove, TextArea, WrapMode};

use crate::ui::theme::{Role, Theme};

/// Prompts kept in memory and on disk.
const HISTORY_LIMIT: usize = 200;

/// A paste longer than this many lines (the prompt box's height) or
/// characters is shown as a one-line chip; the full text is still sent.
const PASTE_COLLAPSE_LINES: usize = 8;
const PASTE_COLLAPSE_CHARS: usize = 1000;

/// Owned editing state for the prompt.
pub struct InputState {
    pub textarea: TextArea<'static>,
    history: Vec<String>,
    history_index: Option<usize>,
    draft: String,
    pub completion_index: usize,
    /// Set by Tab to show on-demand (path) completions; cleared on edit.
    popup_open: bool,
    /// Per-project file prompts are appended to, so Up recalls them across
    /// restarts. `None` keeps history in memory only.
    history_file: Option<PathBuf>,
    /// Collapsed pastes in the prompt: the chip shown and the text it stands
    /// for. Editing a chip expands it first, so the text is never lost.
    pastes: Vec<(String, String)>,
    paste_seq: usize,
    /// Texts kept in memory by Ctrl+C or the history picker that have not
    /// been sent yet. A recalled one is appended to the history file when it
    /// is finally sent.
    stashed: Vec<String>,
}

impl Default for InputState {
    fn default() -> Self {
        Self::new()
    }
}

impl InputState {
    pub fn new() -> Self {
        let mut textarea = TextArea::new(vec![String::new()]);
        textarea.set_placeholder_text("type a message, @file to attach, or /help");
        textarea.set_max_histories(200);
        textarea.set_cursor_line_style(Style::default());
        textarea.set_wrap_mode(WrapMode::WordOrGlyph);
        textarea.move_cursor(CursorMove::End);
        InputState {
            textarea,
            history: Vec::new(),
            history_index: None,
            draft: String::new(),
            completion_index: 0,
            popup_open: false,
            history_file: None,
            pastes: Vec::new(),
            paste_seq: 0,
            stashed: Vec::new(),
        }
    }

    /// The current text with lines joined by newlines.
    pub fn text(&self) -> String {
        self.textarea.lines().join("\n")
    }

    pub fn is_empty(&self) -> bool {
        self.textarea.lines().iter().all(|line| line.is_empty())
    }

    /// The text as it will be sent: collapsed pastes expanded.
    pub fn expanded_text(&self) -> String {
        self.expand(self.text())
    }

    fn expand(&self, mut text: String) -> String {
        for (chip, content) in &self.pastes {
            // Every copy of a chip expands, including one the user duplicated
            // by copying it within the prompt.
            if text.contains(chip.as_str()) {
                text = text.replace(chip.as_str(), content);
            }
        }
        text
    }

    /// Whether the prompt shows a collapsed paste.
    pub fn has_collapsed_paste(&self) -> bool {
        let text = self.text();
        self.pastes
            .iter()
            .any(|(chip, _)| text.contains(chip.as_str()))
    }

    /// Whether a chip marker was edited and no longer expands.
    pub fn has_broken_paste(&self) -> bool {
        let text = self.text();
        self.pastes
            .iter()
            .any(|(chip, _)| !text.contains(chip.as_str()))
    }

    /// Whether the cursor is inside or at either edge of a collapsed paste.
    fn cursor_in_paste(&self) -> bool {
        let cursor = self.textarea.cursor();
        let (row, col) = (cursor.0, cursor.1);
        let text = self.text();
        let mut offset = 0;
        for (index, line) in text.split('\n').enumerate() {
            if index == row {
                offset += col;
                break;
            }
            offset += line.chars().count() + 1;
        }
        self.pastes.iter().any(|(chip, _)| {
            text.match_indices(chip.as_str()).any(|(byte, _)| {
                let start = text[..byte].chars().count();
                start <= offset && offset <= start + chip.chars().count()
            })
        })
    }

    /// Insert pasted text at the cursor. A large paste becomes a chip like
    /// `[Pasted text #1 +120 lines]` so the rest of the draft stays visible;
    /// returns the chip when it collapsed.
    pub fn insert_paste(&mut self, text: &str) -> Option<String> {
        // A paste inside a chip would corrupt its marker; expand first.
        if self.cursor_in_paste() {
            self.expand_pastes();
        }
        // `lines()` already ignores the single trailing newline editors add;
        // trailing blank lines are real rows and must count.
        let lines = text.lines().count();
        if lines <= PASTE_COLLAPSE_LINES && text.chars().count() <= PASTE_COLLAPSE_CHARS {
            self.insert_text(text);
            return None;
        }
        self.paste_seq += 1;
        let chip = if lines > 1 {
            format!("[Pasted text #{} +{lines} lines]", self.paste_seq)
        } else {
            format!(
                "[Pasted text #{} {} chars]",
                self.paste_seq,
                text.chars().count()
            )
        };
        self.pastes.push((chip.clone(), text.to_string()));
        self.insert_text(&chip);
        Some(chip)
    }

    /// Put every collapsed paste's full text back in the editor.
    pub fn expand_pastes(&mut self) -> bool {
        if !self.has_collapsed_paste() {
            return false;
        }
        let text = self.expanded_text();
        self.set_text(&text);
        true
    }

    /// Recover a chip whose marker was edited: replace the remaining marker
    /// start on that line with the full text. Returns false when even the
    /// marker start is gone.
    pub fn repair_pastes(&mut self) -> bool {
        let mut text = self.text();
        let mut changed = false;
        for (chip, content) in &self.pastes {
            if text.contains(chip.as_str()) {
                continue;
            }
            let start_marker = chip
                .split_whitespace()
                .take(3)
                .collect::<Vec<_>>()
                .join(" ");
            if start_marker.is_empty() {
                continue;
            }
            if let Some(start) = text.find(&start_marker) {
                let end = text[start..]
                    .find('\n')
                    .map(|offset| start + offset)
                    .unwrap_or(text.len());
                text.replace_range(start..end, content);
                changed = true;
            }
        }
        if changed {
            self.set_text(&text);
        }
        changed
    }

    pub fn take(&mut self) -> String {
        let text = self.expanded_text();
        self.textarea.clear();
        self.pastes.clear();
        self.reset_completion();
        text
    }

    pub fn set_text(&mut self, text: &str) {
        self.pastes.clear();
        self.textarea.clear();
        self.textarea.insert_str(text);
        self.textarea.move_cursor(CursorMove::End);
        self.reset_completion();
    }

    /// Submit the prompt when idle. Returns `None` (and preserves the text)
    /// while a turn is streaming.
    pub fn submit(&mut self, idle: bool) -> Option<String> {
        if !idle {
            return None;
        }
        let text = self.expanded_text();
        if text.trim().is_empty() {
            return None;
        }
        self.push_history(&text);
        self.textarea.clear();
        self.pastes.clear();
        self.history_index = None;
        self.draft.clear();
        self.reset_completion();
        Some(text)
    }

    pub fn push_history(&mut self, text: &str) {
        if text.trim().is_empty() {
            return;
        }
        // A stashed draft was never written to the file. Sending it later
        // must append it even though it is already the newest in-memory
        // entry, or it disappears after a restart.
        let was_stashed = self
            .stashed
            .iter()
            .position(|entry| entry == text)
            .map(|index| self.stashed.remove(index))
            .is_some();
        if was_stashed {
            // It is already the newest in-memory entry, but the file never
            // saw it. Do not add an adjacent duplicate.
            self.append_history_file(text);
        } else if self.history.last().map(String::as_str) != Some(text) {
            self.history.push(text.to_string());
            if self.history.len() > HISTORY_LIMIT {
                self.history.remove(0);
            }
            self.append_history_file(text);
        }
    }

    /// Keep a discarded draft one Up away for this run, without writing it
    /// to the history file (it was never sent).
    pub fn stash_in_history(&mut self, text: &str) {
        self.stash_impl(text, true);
    }

    /// Like [`InputState::stash_in_history`], but the draft saved by history
    /// browsing survives so Ctrl+R can be cancelled without losing it.
    pub fn stash_in_history_keep_draft(&mut self, text: &str) {
        self.stash_impl(text, false);
    }

    fn stash_impl(&mut self, text: &str, clear_draft: bool) {
        if text.trim().is_empty() {
            return;
        }
        if self.history.last().map(String::as_str) != Some(text) {
            self.history.push(text.to_string());
            if self.history.len() > HISTORY_LIMIT {
                self.history.remove(0);
            }
            self.stashed.push(text.to_string());
        }
        // The cleared line is no longer a browsing position, even when the
        // text was already the newest entry.
        self.history_index = None;
        if clear_draft {
            self.draft.clear();
        }
    }

    /// Load prompt history from `path` (JSON-lines of strings, newest last)
    /// and append future prompts to it. A missing or damaged file is not an
    /// error: history is a convenience.
    pub fn attach_history_file(&mut self, path: PathBuf) {
        if let Ok(raw) = std::fs::read_to_string(&path) {
            let loaded: Vec<String> = raw
                .lines()
                .filter_map(|line| serde_json::from_str::<String>(line).ok())
                .filter(|text| !text.trim().is_empty())
                .collect();
            let skip = loaded.len().saturating_sub(HISTORY_LIMIT);
            let mut history: Vec<String> = loaded.into_iter().skip(skip).collect();
            history.dedup();
            history.append(&mut self.history);
            self.history = history;
            // Rewrite oversized files so they stay bounded.
            if skip > HISTORY_LIMIT {
                let body: String = self
                    .history
                    .iter()
                    .filter_map(|text| serde_json::to_string(text).ok())
                    .map(|line| line + "\n")
                    .collect();
                let _ = std::fs::write(&path, body);
            }
        }
        self.history_file = Some(path);
    }

    fn append_history_file(&self, text: &str) {
        let Some(path) = &self.history_file else {
            return;
        };
        let Ok(line) = serde_json::to_string(text) else {
            return;
        };
        use std::os::unix::fs::OpenOptionsExt as _;
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(path)
        {
            let _ = writeln!(file, "{line}");
        }
    }

    /// Prompts sent so far, oldest first.
    pub fn history(&self) -> &[String] {
        &self.history
    }

    /// Whether Up should go further back in history: the prompt is empty, or
    /// it still holds the unedited entry Up recalled and the cursor is on its
    /// first *visual* row (a long line soft-wraps into several).
    pub fn walks_history_up(&self) -> bool {
        self.is_empty() || (self.on_recalled_entry() && self.first_screen_row())
    }

    /// Down's mirror of [`InputState::walks_history_up`]: on the last visual
    /// row of an unedited recalled entry.
    pub fn walks_history_down(&self) -> bool {
        self.on_recalled_entry() && self.last_screen_row()
    }

    fn first_screen_row(&self) -> bool {
        self.textarea.screen_cursor().row == 0
    }

    fn last_screen_row(&self) -> bool {
        // The textarea knows the display rows after it is drawn. Probe the
        // end of the text to find the last one without disturbing this cursor.
        let mut probe = self.textarea.clone();
        probe.move_cursor(CursorMove::Bottom);
        self.textarea.screen_cursor().row >= probe.screen_cursor().row
    }

    /// Whether the prompt still holds an unedited entry recalled from
    /// history. The hidden `draft` belongs to that browsing position.
    pub fn on_recalled_entry(&self) -> bool {
        self.history_index
            .and_then(|index| self.history.get(index))
            .is_some_and(|entry| *entry == self.text())
    }

    /// Fill the prompt from the history picker and mark the entry as recalled,
    /// so Down can restore the draft saved before browsing.
    pub fn fill_history_entry(&mut self, text: &str) {
        self.set_text(text);
        self.history_index = self.history.iter().rposition(|entry| entry == text);
    }

    pub fn history_prev(&mut self) -> bool {
        if self.history.is_empty() {
            return false;
        }
        let index = match self.history_index {
            Some(index) => index.saturating_sub(1),
            None => {
                self.draft = self.text();
                self.history.len() - 1
            }
        };
        self.history_index = Some(index);
        let text = self.history[index].clone();
        self.set_text(&text);
        true
    }

    pub fn history_next(&mut self) -> bool {
        let Some(index) = self.history_index else {
            return false;
        };
        if index + 1 < self.history.len() {
            self.history_index = Some(index + 1);
            let text = self.history[index + 1].clone();
            self.set_text(&text);
        } else {
            self.history_index = None;
            let draft = std::mem::take(&mut self.draft);
            self.set_text(&draft);
        }
        true
    }

    /// Apply an editing key. `Ctrl+J` inserts a newline, `Ctrl+U` kills to
    /// the start of the line as in readline, `Ctrl+_` / `Ctrl+Z` undo and
    /// `Ctrl+Shift+Z` redoes; everything else is delegated to the textarea so
    /// word movement and the other kills keep working.
    pub fn handle_key(&mut self, key: KeyEvent) -> bool {
        if key.kind == KeyEventKind::Release {
            return false;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        // Editing a chip would leave a marker that no longer expands. Put the
        // full text back first, then apply the key to it.
        let edits = matches!(key.code, KeyCode::Backspace | KeyCode::Delete)
            || (matches!(key.code, KeyCode::Char(_)) && !ctrl && !alt);
        if edits && self.cursor_in_paste() {
            self.expand_pastes();
        }
        if ctrl && matches!(key.code, KeyCode::Char('j') | KeyCode::Char('J')) {
            self.insert_newline();
            return true;
        }
        if ctrl && matches!(key.code, KeyCode::Char('u') | KeyCode::Char('U')) {
            self.reset_completion();
            // Readline C-u at the start of a line kills nothing; the textarea
            // would join it to the previous line instead.
            return if self.textarea.cursor().1 == 0 {
                true
            } else {
                self.textarea.delete_line_by_head()
            };
        }
        if ctrl && matches!(key.code, KeyCode::Char('z') | KeyCode::Char('Z')) {
            self.reset_completion();
            return if shift || key.code == KeyCode::Char('Z') {
                self.textarea.redo()
            } else {
                self.textarea.undo()
            };
        }
        if alt && matches!(key.code, KeyCode::Char('z') | KeyCode::Char('Z')) {
            self.reset_completion();
            return self.textarea.redo();
        }
        // Ctrl+_ arrives as Ctrl+_, Ctrl+-, or Ctrl+7 depending on the
        // terminal.
        if ctrl && matches!(key.code, KeyCode::Char('_' | '-' | '7')) {
            self.reset_completion();
            return self.textarea.undo();
        }
        self.reset_completion();
        self.textarea.input(key)
    }

    pub fn insert_newline(&mut self) {
        self.textarea.insert_newline();
        self.textarea.move_cursor(CursorMove::End);
        self.reset_completion();
    }

    pub fn insert_text(&mut self, text: &str) {
        self.textarea.insert_str(text);
        self.reset_completion();
    }

    pub fn undo(&mut self) -> bool {
        self.textarea.undo()
    }

    pub fn redo(&mut self) -> bool {
        self.textarea.redo()
    }

    /// The prompt text before the cursor, which completion works on.
    pub fn before_cursor(&self) -> String {
        let ratatui_textarea::DataCursor(row, col) = self.textarea.cursor();
        let lines = self.textarea.lines();
        let mut before = String::new();
        for line in lines.iter().take(row) {
            before.push_str(line);
            before.push('\n');
        }
        if let Some(line) = lines.get(row) {
            before.extend(line.chars().take(col));
        }
        before
    }

    /// Whether the cursor sits at the very end of the prompt.
    pub fn cursor_at_end(&self) -> bool {
        self.before_cursor().len() == self.text().len()
    }

    /// Replace the text between byte `start` and the cursor with
    /// `insertion`, leaving the cursor just after it. Text after the cursor
    /// is preserved.
    pub fn replace_before_cursor(&mut self, start: usize, insertion: &str) {
        let before = self.before_cursor();
        let text = self.text();
        let after = &text[before.len()..];
        let start = start.min(before.len());
        let head = format!("{}{insertion}", &before[..start]);
        self.textarea.clear();
        self.textarea.insert_str(format!("{head}{after}"));
        let row = head.matches('\n').count();
        let col = head.rsplit('\n').next().unwrap_or("").chars().count();
        self.textarea.move_cursor(CursorMove::Jump(
            row.min(u16::MAX as usize) as u16,
            col.min(u16::MAX as usize) as u16,
        ));
        self.reset_completion();
    }

    pub fn cycle_completion(&mut self, delta: isize, len: usize) {
        if len == 0 {
            self.completion_index = 0;
            return;
        }
        let len = len as isize;
        let next = (self.completion_index as isize + delta).rem_euclid(len);
        self.completion_index = next as usize;
    }

    /// Open the on-demand completion popup (file paths) until the text
    /// changes.
    pub fn open_popup(&mut self) {
        self.popup_open = true;
    }

    pub fn popup_open(&self) -> bool {
        self.popup_open
    }

    pub fn reset_completion(&mut self) {
        self.completion_index = 0;
        self.popup_open = false;
    }

    /// Screen rows the prompt needs at `width` columns with soft wrapping.
    pub fn visual_rows(&self, width: usize) -> usize {
        let width = width.max(1);
        self.textarea
            .lines()
            .iter()
            .map(|line| crate::ui::markdown::wrap_plain(line, width).len().max(1))
            .sum()
    }

    /// Settle the textarea's viewport for a `width`×`height` prompt. The
    /// textarea only scrolls to keep the cursor visible, so after the box
    /// grows it would keep hiding rows that now fit; scroll those back.
    pub fn fit_viewport(&mut self, width: u16, height: u16) {
        use ratatui::buffer::Buffer;
        use ratatui::layout::Rect;
        use ratatui::widgets::Widget as _;

        if width == 0 || height == 0 {
            return;
        }
        // Rendering once records the real viewport size in the textarea.
        let area = Rect::new(0, 0, width, height);
        (&self.textarea).render(area, &mut Buffer::empty(area));
        let (top, _) = self.textarea.scroll_offset();
        if top > 0 && self.visual_rows(width as usize) <= height as usize {
            // `scroll` drags the cursor into the old viewport; put it back.
            let ratatui_textarea::DataCursor(row, col) = self.textarea.cursor();
            self.textarea
                .scroll((-(top.min(i16::MAX as u16) as i16), 0));
            self.textarea.move_cursor(CursorMove::Jump(
                row.min(u16::MAX as usize) as u16,
                col.min(u16::MAX as usize) as u16,
            ));
        }
    }

    pub fn set_placeholder(&mut self, text: &str) {
        if self.textarea.placeholder_text() != text {
            self.textarea.set_placeholder_text(text);
        }
    }

    /// Apply the active theme to the textarea widget.
    pub fn apply_theme(&mut self, theme: &Theme) {
        self.textarea.set_style(theme.style(Role::Fg));
        // A block cursor: a foreground-only style is invisible on a space.
        self.textarea.set_cursor_style(
            Style::default()
                .fg(theme.color(Role::Bg))
                .bg(theme.color(Role::Cursor))
                .add_modifier(Modifier::BOLD),
        );
        self.textarea.set_cursor_line_style(Style::default());
        self.textarea
            .set_placeholder_style(theme.style(Role::Muted));
        // The surrounding rounded box and prompt glyph are drawn by the
        // painter; the textarea itself is borderless.
        self.textarea
            .set_block(Block::default().borders(Borders::NONE));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    #[test]
    fn enter_sends_only_when_idle() {
        let mut input = InputState::new();
        input.set_text("hello");
        assert_eq!(input.submit(false), None);
        assert_eq!(input.text(), "hello");
        assert_eq!(input.submit(true).as_deref(), Some("hello"));
        assert!(input.is_empty());
    }

    #[test]
    fn ctrl_j_inserts_a_newline() {
        let mut input = InputState::new();
        input.insert_text("one");
        input.handle_key(key(KeyCode::Char('j'), KeyModifiers::CONTROL));
        input.insert_text("two");
        assert_eq!(input.text(), "one\ntwo");
    }

    #[test]
    fn up_recalls_history_when_empty() {
        let mut input = InputState::new();
        input.set_text("first");
        input.submit(true).unwrap();
        input.set_text("second");
        input.submit(true).unwrap();
        assert!(input.is_empty());
        assert!(input.history_prev());
        assert_eq!(input.text(), "second");
        assert!(input.history_prev());
        assert_eq!(input.text(), "first");
        assert!(input.history_next());
        assert_eq!(input.text(), "second");
        assert!(input.history_next());
        assert!(input.is_empty());
    }

    #[test]
    fn up_keeps_walking_history_until_the_entry_is_edited() {
        let mut input = InputState::new();
        for text in ["first", "second\nwith two rows", "third"] {
            input.set_text(text);
            input.submit(true).unwrap();
        }
        assert!(input.walks_history_up());
        input.history_prev();
        assert_eq!(input.text(), "third");
        assert!(input.walks_history_up());
        input.history_prev();
        assert_eq!(input.text(), "second\nwith two rows");
        // The cursor lands on the last row: Up moves it first.
        assert!(!input.walks_history_up());
        input.handle_key(key(KeyCode::Up, KeyModifiers::NONE));
        assert!(input.walks_history_up());
        input.history_prev();
        assert_eq!(input.text(), "first");
        assert!(input.walks_history_down());
        input.history_next();
        assert_eq!(input.text(), "second\nwith two rows");
        // An edited entry is a draft: arrows move the cursor again.
        input.insert_text("!");
        assert!(!input.walks_history_up());
        assert!(!input.walks_history_down());
    }

    #[test]
    fn ctrl_u_kills_to_line_start_and_ctrl_underscore_undoes() {
        let mut input = InputState::new();
        input.insert_text("keep\nkill this");
        input.handle_key(key(KeyCode::Char('u'), KeyModifiers::CONTROL));
        assert_eq!(input.text(), "keep\n");
        input.handle_key(key(KeyCode::Char('_'), KeyModifiers::CONTROL));
        assert_eq!(input.text(), "keep\nkill this");
        input.handle_key(key(KeyCode::Char('u'), KeyModifiers::CONTROL));
        input.handle_key(key(KeyCode::Char('z'), KeyModifiers::CONTROL));
        assert_eq!(input.text(), "keep\nkill this");
    }

    #[test]
    fn replacing_before_the_cursor_keeps_the_tail() {
        let mut input = InputState::new();
        input.set_text("fix @src/ma please");
        // Put the cursor right after "@src/ma".
        input.textarea.move_cursor(CursorMove::Jump(0, 11));
        assert_eq!(input.before_cursor(), "fix @src/ma");
        assert!(!input.cursor_at_end());
        input.replace_before_cursor(5, "src/main.rs ");
        assert_eq!(input.text(), "fix @src/main.rs  please");
        assert_eq!(input.before_cursor(), "fix @src/main.rs ");
    }

    #[test]
    fn history_persists_across_instances() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("prompt_history.jsonl");
        let mut first = InputState::new();
        first.attach_history_file(path.clone());
        first.set_text("line one\nline two");
        first.submit(true).unwrap();
        first.set_text("second");
        first.submit(true).unwrap();

        let mut second = InputState::new();
        second.attach_history_file(path);
        assert!(second.history_prev());
        assert_eq!(second.text(), "second");
        assert!(second.history_prev());
        assert_eq!(second.text(), "line one\nline two");
    }

    #[test]
    fn a_large_paste_collapses_to_a_chip_and_is_sent_in_full() {
        let mut input = InputState::new();
        input.insert_text("look at this: ");
        let body: String = (1..=20).map(|n| format!("line {n}\n")).collect();
        let chip = input.insert_paste(&body).expect("collapsed");
        assert_eq!(chip, "[Pasted text #1 +20 lines]");
        assert_eq!(input.text(), format!("look at this: {chip}"));
        assert!(input.has_collapsed_paste());
        input.insert_text(" thanks");
        assert_eq!(
            input.submit(true).unwrap(),
            format!("look at this: {body} thanks")
        );
        // History recalls the full text, not the chip.
        assert!(input.history_prev());
        assert!(input.text().contains("line 20"));
        assert!(!input.has_collapsed_paste());
    }

    #[test]
    fn small_pastes_are_inserted_and_chips_expand_for_editing() {
        let mut input = InputState::new();
        assert_eq!(input.insert_paste("a\nb\nc"), None);
        assert_eq!(input.text(), "a\nb\nc");
        input.set_text("");
        input.insert_paste(&"x".repeat(1500)).expect("collapsed");
        assert_eq!(input.text(), "[Pasted text #1 1500 chars]");
        assert!(input.expand_pastes());
        assert_eq!(input.text(), "x".repeat(1500));
        assert!(!input.expand_pastes());
    }

    #[test]
    fn editing_a_chip_expands_it_and_repairs_a_broken_marker() {
        let mut input = InputState::new();
        let body: String = (1..=20).map(|n| format!("line {n}\n")).collect();
        let chip = input.insert_paste(&body).expect("collapsed");
        let inside = chip.chars().count() - 1;
        input
            .textarea
            .move_cursor(CursorMove::Jump(0, inside as u16));
        input.handle_key(key(KeyCode::Backspace, KeyModifiers::NONE));
        assert!(input.text().contains("line 20"), "{}", input.text());
        assert!(!input.has_collapsed_paste());

        // A marker damaged outside the editor's key path is repaired on
        // submit instead of being sent literally.
        let mut broken = InputState::new();
        let chip = broken.insert_paste(&body).expect("collapsed");
        broken.textarea.move_cursor(CursorMove::End);
        broken
            .textarea
            .input(key(KeyCode::Backspace, KeyModifiers::NONE));
        assert!(broken.has_broken_paste());
        assert!(broken.repair_pastes());
        assert!(broken.text().contains("line 20"), "{}", broken.text());
        assert!(!broken.text().contains(&chip));
    }

    #[test]
    fn a_stashed_draft_is_written_when_it_is_finally_sent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("prompt_history.jsonl");
        let mut input = InputState::new();
        input.attach_history_file(path.clone());
        input.set_text("a draft");
        input.stash_in_history("a draft");
        input.set_text("");
        assert!(input.history_prev());
        assert_eq!(input.text(), "a draft");
        assert_eq!(input.submit(true).as_deref(), Some("a draft"));

        let mut second = InputState::new();
        second.attach_history_file(path);
        assert!(second.history_prev());
        assert_eq!(second.text(), "a draft");
    }

    #[test]
    fn ctrl_u_at_the_start_of_a_line_does_nothing() {
        let mut input = InputState::new();
        input.set_text("one\ntwo");
        input.textarea.move_cursor(CursorMove::Jump(1, 0));
        input.handle_key(key(KeyCode::Char('u'), KeyModifiers::CONTROL));
        assert_eq!(input.text(), "one\ntwo");
        input.textarea.move_cursor(CursorMove::Jump(1, 1));
        input.handle_key(key(KeyCode::Char('u'), KeyModifiers::CONTROL));
        assert_eq!(input.text(), "one\nwo");
    }

    #[test]
    fn wrapped_history_walks_by_visual_row() {
        let mut input = InputState::new();
        let long = "word ".repeat(30);
        for text in ["oldest", long.as_str(), "newest"] {
            input.set_text(text);
            input.submit(true).unwrap();
        }
        assert!(input.history_prev());
        assert_eq!(input.text(), "newest");
        assert!(input.history_prev());
        assert_eq!(input.text(), long);
        input.fit_viewport(20, 6);
        assert!(
            !input.walks_history_up(),
            "the cursor starts on the last row"
        );
        // Up moves within the wrapped entry, not to older history.
        input.handle_key(key(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(input.text(), long);
        for _ in 0..40 {
            input.handle_key(key(KeyCode::Up, KeyModifiers::NONE));
        }
        assert!(input.walks_history_up(), "the cursor reached the first row");
        assert!(input.history_prev());
        assert_eq!(input.text(), "oldest");
    }

    #[test]
    fn before_cursor_spans_lines() {
        let mut input = InputState::new();
        input.set_text("one\ntwo");
        assert_eq!(input.before_cursor(), "one\ntwo");
        assert!(input.cursor_at_end());
    }

    #[test]
    fn refitting_after_growth_shows_every_row_and_keeps_the_cursor() {
        let mut input = InputState::new();
        let long = "word ".repeat(30);
        input.insert_text(long.trim_end());
        // Typed while the box was one row high, then the box grows.
        input.fit_viewport(40, 1);
        assert!(input.textarea.scroll_offset().0 > 0);
        input.fit_viewport(40, 4);
        assert_eq!(input.textarea.scroll_offset().0, 0);
        input.insert_text("!");
        assert!(input.text().ends_with("word!"), "{}", input.text());
        assert_eq!(input.visual_rows(40), 4);
    }
}
