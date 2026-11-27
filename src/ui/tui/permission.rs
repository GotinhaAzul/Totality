//! Interactive permission prompts: the TUI side of a policy `ask`.
//!
//! The turn task sends the request over the background channel and waits; the
//! transcript shows the exact command or path (and a diff when one exists),
//! and the footer lists the answers: this call, a pattern for the rest of the
//! session, the saved rule when one is suggested, no, and "no, and tell the
//! model" once the user has typed a draft. Arrows and Enter (or the number
//! keys) pick. `y`/`n` still answer, but only as the first key on an empty
//! prompt after the question has been up for a moment, so a sentence that
//! was being typed when it appeared goes into the prompt instead of
//! approving anything. Cancelling the turn or closing the TUI denies.

use super::*;

use crate::policy::{AskAnswer, AskRequest, Asker, PermissionMode};

/// A pending question and the channel its answer goes back on.
pub(super) struct PendingAsk {
    pub request: AskRequest,
    pub reply: tokio::sync::oneshot::Sender<AskAnswer>,
}

/// Forwards policy questions from the turn task to the event loop.
pub(super) struct TuiAsker {
    pub tx: UnboundedSender<BgEvent>,
}

#[async_trait::async_trait]
impl Asker for TuiAsker {
    async fn ask(&self, request: AskRequest) -> AskAnswer {
        let (reply, answer) = tokio::sync::oneshot::channel();
        if self
            .tx
            .send(BgEvent::Permission(PendingAsk { request, reply }))
            .is_err()
        {
            return AskAnswer::Deny;
        }
        answer.await.unwrap_or(AskAnswer::Deny)
    }
}

/// How long a question must be on screen before a bare `y`/`n`/digit
/// answers it; keys typed sooner go to the prompt.
const ANSWER_DEBOUNCE: Duration = Duration::from_millis(400);

/// One answer the user can pick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Choice {
    Allow,
    AllowSession,
    AllowAlways,
    Deny,
    DenyWithNote,
}

/// Selection state for the open question.
#[derive(Debug, Clone)]
pub(super) struct AskUi {
    pub shown: Instant,
    pub selected: usize,
    /// The user moved the selection; a draft no longer re-selects the
    /// "tell the model" answer.
    pub moved: bool,
    /// Something was typed into the prompt since the question appeared.
    pub typed: bool,
}

impl Default for AskUi {
    fn default() -> Self {
        Self {
            shown: Instant::now(),
            selected: 0,
            moved: false,
            typed: false,
        }
    }
}

/// The transcript text for a question: the exact subject, then the preview,
/// then the answers.
pub(super) fn ask_text(request: &AskRequest) -> String {
    let mut text = match request.tool.as_str() {
        "bash" | "bg" => format!("$ {}", request.subject),
        _ => format!("{}: {}", request.tool, request.subject),
    };
    if let Some(rule) = &request.rule {
        text.push_str(&format!("\n(asked by rule {rule})"));
    }
    if let Some(preview) = &request.preview {
        text.push_str("\n\n");
        text.push_str(preview);
    }
    if let Some(suggestion) = &request.suggestion {
        text.push_str(&format!(
            "\n\nYou approved this exact command {} times before in this project. \"Always\" saves this rule to your global config:\n{}",
            suggestion.times,
            suggestion.toml()
        ));
    }
    text.push_str("\n\nAllow this call?");
    for (index, (_, label)) in choices_for(request, false).iter().enumerate() {
        text.push_str(&format!("\n  {} {label}", index + 1));
    }
    text.push_str(
        "\nUp/Down and Enter choose. Type a message first to deny and tell the model what to do instead.",
    );
    text
}

/// The answers for `request`, with their long labels.
fn choices_for(request: &AskRequest, draft: bool) -> Vec<(Choice, String)> {
    let mut choices = vec![(Choice::Allow, "Yes, this exact call".to_string())];
    if let Some(rule) = &request.session_rule {
        choices.push((
            Choice::AllowSession,
            format!("Yes, and allow {} for this session", rule.label()),
        ));
    }
    if request.suggestion.is_some() {
        choices.push((
            Choice::AllowAlways,
            "Yes, always (save the rule)".to_string(),
        ));
    }
    choices.push((Choice::Deny, "No".to_string()));
    if draft {
        choices.push((
            Choice::DenyWithNote,
            "No, and send my message as the reason".to_string(),
        ));
    }
    choices
}

/// The footer's short name for a choice.
fn short_label(choice: Choice, request: &AskRequest) -> String {
    match choice {
        Choice::Allow => "yes".into(),
        Choice::AllowSession => format!(
            "session: {}",
            request
                .session_rule
                .as_ref()
                .map(|rule| rule.label())
                .unwrap_or_default()
        ),
        Choice::AllowAlways => "always".into(),
        Choice::Deny => "no".into(),
        Choice::DenyWithNote => "no + send message".into(),
    }
}

impl App {
    /// Let the current core's turns ask through this TUI.
    pub(super) fn install_asker(&self) {
        if let Some(core) = self.core() {
            core.permissions.set_asker(Arc::new(TuiAsker {
                tx: self.bg_tx.clone(),
            }));
        }
    }

    pub(super) fn show_permission_request(&mut self, pending: PendingAsk) {
        // A newer question replaces an unanswered one; the old one is denied.
        if let Some(previous) = self.pending_ask.take() {
            let _ = previous.reply.send(AskAnswer::Deny);
        }
        self.transcript.push_note(
            format!("permission · {}", pending.request.tool),
            ask_text(&pending.request),
            false,
        );
        self.transcript.scroll_end();
        self.pending_ask = Some(pending);
        self.ask_ui = AskUi::default();
    }

    /// The open question's answers, given the prompt's draft.
    pub(super) fn ask_choices(&self) -> Vec<(Choice, String)> {
        self.pending_ask
            .as_ref()
            .map(|pending| choices_for(&pending.request, !self.input.is_empty()))
            .unwrap_or_default()
    }

    /// The highlighted answer: "tell the model" once a draft exists, unless
    /// the user moved the selection.
    pub(super) fn ask_selected(&self) -> usize {
        let choices = self.ask_choices();
        if !self.ask_ui.moved
            && let Some(index) = choices
                .iter()
                .position(|(choice, _)| *choice == Choice::DenyWithNote)
        {
            return index;
        }
        self.ask_ui.selected.min(choices.len().saturating_sub(1))
    }

    /// The footer and activity line's answer list, the selection marked.
    pub(super) fn permission_keys(&self) -> String {
        let Some(pending) = &self.pending_ask else {
            return String::new();
        };
        let selected = self.ask_selected();
        let mut parts: Vec<String> = self
            .ask_choices()
            .iter()
            .enumerate()
            .map(|(index, (choice, _))| {
                let label = short_label(*choice, &pending.request);
                if index == selected {
                    format!("▸{} {label}", index + 1)
                } else {
                    format!("{} {label}", index + 1)
                }
            })
            .collect();
        parts.push("↑↓ enter".into());
        parts.join(" · ")
    }

    /// Answer the pending question. Returns false when there is none.
    pub(super) fn answer_permission(&mut self, answer: AskAnswer) -> bool {
        let Some(pending) = self.pending_ask.take() else {
            return false;
        };
        let verdict = match &answer {
            AskAnswer::Allow => "allowed".to_string(),
            AskAnswer::AllowAlways => "allowed and saved the rule".to_string(),
            AskAnswer::AllowSession => format!(
                "allowed {} for this session",
                pending
                    .request
                    .session_rule
                    .as_ref()
                    .map(|rule| rule.label())
                    .unwrap_or_default()
            ),
            AskAnswer::Deny => "denied".to_string(),
            AskAnswer::DenyWithNote(_) => "denied and sent your message".to_string(),
        };
        self.set_notice(
            format!("{verdict}: {}", pending.request.subject),
            if answer.allows() {
                Role::Success
            } else {
                Role::Warning
            },
        );
        let _ = pending.reply.send(answer);
        true
    }

    fn answer_choice(&mut self, choice: Choice) {
        let answer = match choice {
            Choice::Allow => AskAnswer::Allow,
            Choice::AllowSession => AskAnswer::AllowSession,
            Choice::AllowAlways => AskAnswer::AllowAlways,
            Choice::Deny => AskAnswer::Deny,
            Choice::DenyWithNote => match self.input.submit(true) {
                Some(note) => AskAnswer::DenyWithNote(note),
                None => AskAnswer::Deny,
            },
        };
        self.answer_permission(answer);
    }

    /// Keys while a question is open. Up/Down move the selection, Enter
    /// picks it, Esc denies (keeping any draft). A bare `y`, `n`, `a`, `s`,
    /// or digit answers only on an empty prompt, as the first key after the
    /// question has been up for [`ANSWER_DEBOUNCE`]. Every other key edits
    /// the prompt as usual (returns false), so nothing typed is lost.
    pub(super) fn handle_permission_key(&mut self, key: &KeyEvent) -> bool {
        if self.pending_ask.is_none() {
            return false;
        }
        let modified = key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SHIFT);
        let choices = self.ask_choices();
        match key.code {
            KeyCode::Up | KeyCode::Down if !modified => {
                let selected = self.ask_selected() as isize;
                let delta = if key.code == KeyCode::Up { -1 } else { 1 };
                let len = choices.len() as isize;
                self.ask_ui.selected = (selected + delta).rem_euclid(len.max(1)) as usize;
                self.ask_ui.moved = true;
                true
            }
            KeyCode::Enter if !modified => {
                if let Some((choice, _)) = choices.get(self.ask_selected()) {
                    self.answer_choice(*choice);
                }
                true
            }
            KeyCode::Esc => {
                self.answer_permission(AskAnswer::Deny);
                true
            }
            KeyCode::Char(ch) if !modified || ch.is_ascii_uppercase() => {
                let ready = self.input.is_empty()
                    && !self.ask_ui.typed
                    && self.ask_ui.shown.elapsed() >= ANSWER_DEBOUNCE;
                let has = |wanted: Choice| choices.iter().any(|(choice, _)| *choice == wanted);
                let picked = match ch.to_ascii_lowercase() {
                    _ if !ready => None,
                    'y' => Some(Choice::Allow),
                    'n' => Some(Choice::Deny),
                    'a' if has(Choice::AllowAlways) => Some(Choice::AllowAlways),
                    's' if has(Choice::AllowSession) => Some(Choice::AllowSession),
                    digit @ '1'..='9' => choices
                        .get(digit as usize - '1' as usize)
                        .map(|(choice, _)| *choice),
                    _ => None,
                };
                match picked {
                    Some(choice) => {
                        self.answer_choice(choice);
                        true
                    }
                    None => {
                        self.ask_ui.typed = true;
                        false
                    }
                }
            }
            _ => {
                if matches!(key.code, KeyCode::Backspace | KeyCode::Delete) {
                    self.ask_ui.typed = true;
                }
                false
            }
        }
    }

    /// Keys while a `skip-ask` or `yolo` switch waits for confirmation: `y`
    /// enables it, any other key cancels so a stray keystroke cannot remove
    /// approvals.
    pub(super) async fn handle_mode_confirm(&mut self, key: &KeyEvent) -> bool {
        let Some(mode) = self.pending_mode_confirm.take() else {
            return false;
        };
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => self.set_permission_mode(mode).await,
            _ => self.set_notice(
                format!("{} not enabled; permissions unchanged", mode.label()),
                Role::Muted,
            ),
        }
        true
    }

    /// Footer status: an open question first, then a non-default mode.
    pub(super) fn permission_status(&self) -> Option<(String, Role)> {
        if self.pending_ask.is_some() {
            return Some((
                format!("permission: {}", self.permission_keys()),
                Role::Warning,
            ));
        }
        if let Some(mode) = self.pending_mode_confirm {
            return Some((
                format!("{}: press y to confirm · any key cancels", mode.label()),
                Role::Warning,
            ));
        }
        let mode = self.core()?.permissions.mode();
        match mode {
            // `normal` is the default, so it adds nothing to the footer.
            PermissionMode::Ask => None,
            PermissionMode::Plan => Some((
                "plan mode · /plan approve to apply · /plan exit to discard".into(),
                Role::Warning,
            )),
            PermissionMode::AcceptEdits => {
                Some(("accept-edits: edits run, commands ask".into(), Role::Muted))
            }
            PermissionMode::DenyAll => Some((
                "deny-all: anything that would ask is denied".into(),
                Role::Warning,
            )),
            PermissionMode::SkipAsk => Some((
                "skip-ask: no approvals; deny rules and guards apply".into(),
                Role::Error,
            )),
            PermissionMode::Yolo => Some(("yolo: no restrictions".into(), Role::Error)),
        }
    }
}
