//! Run checkpoints, wall clock, and the progress ledger.
//!
//! One [`ProgressLedger`] decides whether a run is still making progress.
//! Every tool result is classified once: `changed` (a file changed or the
//! workspace moved), `verified` (a verification passed), `learned` (a success
//! whose output was not seen this turn), or `failed-new` (an error not seen
//! this turn). Anything else is idle. `[budgets] idle_budget` idle calls
//! within the last twice as many (since the last change or passing verify)
//! is a stall: the first one pivots, the second ends the turn.
//!
//! Tool-call, model-call, and wall-clock limits are checkpoints, not hard
//! stops: one renews when the ledger saw a change, a passing verify, or a new
//! result since the last one (a new error alone does not renew), so a long
//! task runs as long as it keeps making progress.

use std::collections::{HashMap, HashSet, VecDeque};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::config::BudgetConfig;

#[derive(Debug, Clone)]
pub struct RunLimits {
    /// Checkpoint interval, not a hard cap.
    pub max_tool_calls: u32,
    /// Checkpoint interval, not a hard cap.
    pub max_model_calls: u32,
    /// Checkpoint interval, not a hard cap.
    pub max_wall: Duration,
    /// Idle calls in a row (no change, no passing verify, nothing new) that
    /// make a stall.
    pub idle_budget: u32,
    pub verify_attempts: u32,
}

impl From<&BudgetConfig> for RunLimits {
    fn from(config: &BudgetConfig) -> Self {
        Self {
            max_tool_calls: config.max_tool_calls.max(1),
            max_model_calls: config.max_model_calls.max(1),
            max_wall: Duration::from_secs(u64::from(config.max_wall_minutes.max(1)) * 60),
            idle_budget: config.idle_budget.max(1),
            verify_attempts: config.verify_attempts,
        }
    }
}

/// The wall-clock allowance covers at least this many model calls at the
/// slowest recently measured call duration, so a slow engine or a long
/// thinking model is never stopped for being slow.
const WALL_CALLS: u32 = 6;
/// Model-call durations remembered for scaling.
const RECENT_CALLS: usize = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    ToolBudget,
    ModelBudget,
    WallClock,
    Stalled,
    Cancelled,
}

impl StopReason {
    /// Code recorded in the session and the headless JSON stream.
    pub fn code(&self) -> &'static str {
        match self {
            StopReason::ToolBudget => "budget_tools",
            StopReason::ModelBudget => "budget_models",
            StopReason::WallClock => "budget_wall",
            StopReason::Stalled => "stalled",
            StopReason::Cancelled => "cancelled",
        }
    }

    pub fn message(&self) -> &'static str {
        match self {
            StopReason::ToolBudget => {
                "the tool-call checkpoint found no recent progress; the run is repeating without changing anything"
            }
            StopReason::ModelBudget => {
                "the model-call checkpoint found no recent progress; the run is repeating without changing anything"
            }
            StopReason::WallClock => {
                "the wall-clock checkpoint found no recent progress; the run is repeating without changing anything"
            }
            StopReason::Stalled => {
                "the last calls produced no new information or file change; stop and report the last known state, or change approach in a new turn"
            }
            StopReason::Cancelled => "cancelled by the user",
        }
    }
}

/// What one tool result did for the run, as the ledger classifies it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProgressEvent {
    /// A file changed, or the workspace moved under an idle streak.
    Changed,
    /// A project verification passed.
    Verified,
    /// A success whose output was not seen this turn.
    Learned,
    /// An error whose message was not seen this turn: new information too.
    FailedNew,
    /// A repeat, a known error, or a success that told us nothing new.
    Idle,
}

/// Result of asking a checkpoint whether the turn may continue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BudgetCheck {
    /// Not at a checkpoint yet.
    Ok,
    /// The checkpoint was reached while the run was still productive; the
    /// allowance was renewed and the turn continues.
    Renewed(StopReason),
    /// The checkpoint was reached with no recent progress. This is the only
    /// budget outcome that ends a turn.
    Exhausted(StopReason),
}

#[derive(Debug)]
pub struct RunProgress {
    pub limits: RunLimits,
    /// Wall clock for the whole turn, used for reporting only.
    pub started_at: Instant,
    /// Cumulative counters for reporting and messages.
    pub tool_calls: u32,
    pub model_calls: u32,
    /// Current checkpoint allowance.
    epoch_tool_calls: u32,
    epoch_model_calls: u32,
    epoch_started_at: Instant,
    /// How many times a productive checkpoint renewed the allowance.
    pub renewals: u32,
    pub ledger: ProgressLedger,
    pub settle_continuations: u32,
    pub verify_attempts: u32,
    pub review_nudged: bool,
    pub context_retry_used: bool,
    pub last_stop: Option<StopReason>,
    /// Harness nudges spent on broken output this turn.
    pub output_recoveries: u32,
    /// Whether the output reservation was already raised this turn (for a
    /// thinking model or after a length stop).
    pub output_raised: bool,
    /// Whether one retry already turned thinking off.
    pub thinking_retry_used: bool,
    /// Whether the user was told that tool calls are arriving as text.
    pub text_calls_noticed: bool,
    /// Whether the exact tokenizer check already forced a compaction. Reset
    /// after every successful model reply so a later context fill can check
    /// again.
    pub presend_compaction_used: bool,
    /// Whether the advance notice for the todo gate was attached this turn.
    pub todo_notice_sent: bool,
    /// Whether a no-progress checkpoint already asked for a different
    /// approach this turn. The first one pivots; the second ends the turn.
    pub pivot_used: bool,
    /// When the model call in flight started.
    model_call_at: Option<Instant>,
    /// Durations of the last few model calls, request to final chunk.
    recent_calls: VecDeque<Duration>,
}

impl RunProgress {
    pub fn new(limits: RunLimits) -> Self {
        let idle_budget = limits.idle_budget;
        let now = Instant::now();
        Self {
            limits,
            started_at: now,
            tool_calls: 0,
            model_calls: 0,
            epoch_tool_calls: 0,
            epoch_model_calls: 0,
            epoch_started_at: now,
            renewals: 0,
            ledger: ProgressLedger::new(idle_budget),
            settle_continuations: 0,
            verify_attempts: 0,
            review_nudged: false,
            todo_notice_sent: false,
            context_retry_used: false,
            last_stop: None,
            output_recoveries: 0,
            output_raised: false,
            thinking_retry_used: false,
            text_calls_noticed: false,
            presend_compaction_used: false,
            pivot_used: false,
            model_call_at: None,
            recent_calls: VecDeque::new(),
        }
    }

    pub fn elapsed(&self) -> Duration {
        self.started_at.elapsed()
    }

    /// The slowest of the last few model calls, if any finished.
    fn slowest_call(&self) -> Option<Duration> {
        self.recent_calls.iter().max().copied()
    }

    /// The wall-clock allowance: `[budgets] max_wall_minutes`, or
    /// [`WALL_CALLS`] model calls at the measured speed when that is longer.
    pub fn effective_wall(&self) -> Duration {
        let measured = self
            .slowest_call()
            .map(|call| call * WALL_CALLS)
            .unwrap_or_default();
        self.limits.max_wall.max(measured)
    }

    /// Check the model-call and wall-clock checkpoint before a model call.
    pub fn model_checkpoint(&mut self) -> BudgetCheck {
        let model_hit = self.epoch_model_calls >= self.limits.max_model_calls;
        let wall_hit = self.epoch_started_at.elapsed() >= self.effective_wall();
        if !model_hit && !wall_hit {
            return BudgetCheck::Ok;
        }
        let reason = if model_hit {
            StopReason::ModelBudget
        } else {
            StopReason::WallClock
        };
        if self.ledger.progressed_since_checkpoint() {
            self.renew(reason)
        } else {
            BudgetCheck::Exhausted(reason)
        }
    }

    pub fn model_call_started(&mut self) {
        self.model_calls += 1;
        self.epoch_model_calls += 1;
        self.model_call_at = Some(Instant::now());
    }

    /// Record how long the model call in flight took, retries included.
    pub fn model_call_finished(&mut self) {
        if let Some(at) = self.model_call_at.take() {
            self.record_call_duration(at.elapsed());
        }
    }

    fn record_call_duration(&mut self, duration: Duration) {
        if self.recent_calls.len() == RECENT_CALLS {
            self.recent_calls.pop_front();
        }
        self.recent_calls.push_back(duration);
    }

    /// Check the tool-call checkpoint before a batch of `n` calls. A batch is
    /// allowed to cross the checkpoint: the allowance is renewed first when
    /// the run is productive.
    pub fn tool_checkpoint(&mut self, n: u32) -> BudgetCheck {
        if self.epoch_tool_calls.saturating_add(n) <= self.limits.max_tool_calls {
            return BudgetCheck::Ok;
        }
        if self.ledger.progressed_since_checkpoint() {
            self.renew(StopReason::ToolBudget)
        } else {
            BudgetCheck::Exhausted(StopReason::ToolBudget)
        }
    }

    /// Record a tool result. `outcome.result` is the content shown to the
    /// model; `cwd` is the workspace the call ran in.
    pub fn record_tool_result(&mut self, cwd: &Path, outcome: ToolOutcome<'_>) -> ProgressEvent {
        self.tool_calls += 1;
        self.epoch_tool_calls += 1;
        self.ledger.observe(cwd, outcome)
    }

    /// A certain loop (the loop guard's exact repeat or cycle): the idle
    /// budget is spent at once, with `reason` as the stop message.
    pub fn record_loop(&mut self, reason: impl Into<String>) {
        self.ledger.record_loop(reason.into());
    }

    pub fn stalled(&self) -> bool {
        self.ledger.stalled()
    }

    /// Why the turn is being stopped as `stalled`.
    pub fn stall_message(&self) -> String {
        self.ledger.stall_message()
    }

    /// Re-check a stall once a batch is over. A call later in the same batch
    /// may have changed the workspace (a `sed -i`, a script writing a file);
    /// the run was making progress, so it is not a stall.
    pub fn recheck_workspace(&mut self, cwd: &Path) {
        self.ledger.recheck(cwd);
    }

    /// A successful reply breaks a context-error episode: the next compaction
    /// or context retry gets its own allowance.
    pub fn recovered_context(&mut self) {
        self.context_retry_used = false;
        self.presend_compaction_used = false;
    }

    /// At most two settle continuations per turn, shared by verify and todo.
    pub fn can_continue_settle(&self) -> bool {
        self.settle_continuations < 2
    }

    /// Message for a checkpoint that ended the turn.
    pub fn checkpoint_message(&self, reason: StopReason) -> String {
        let no_progress =
            "no file change, passing verification, or new result since the last checkpoint";
        match reason {
            StopReason::ToolBudget => format!(
                "tool-call checkpoint: {} calls in this allowance, {no_progress}. The run is repeating without progress; summarize the last known state or start a new turn with a different approach.",
                self.epoch_tool_calls
            ),
            StopReason::ModelBudget => format!(
                "model-call checkpoint: {} model calls in this allowance, {no_progress}. The run is repeating without progress; summarize the last known state or start a new turn with a different approach.",
                self.epoch_model_calls
            ),
            StopReason::WallClock => format!(
                "wall-clock checkpoint: {} minutes in this allowance{}, {no_progress}. The run is repeating without progress; summarize the last known state or start a new turn with a different approach.",
                self.effective_wall().as_secs() / 60,
                if self.effective_wall() > self.limits.max_wall {
                    " (scaled to the measured model speed)"
                } else {
                    ""
                }
            ),
            StopReason::Stalled => {
                "the last calls produced no new information or file change; stop and report the last known state, or change approach in a new turn".into()
            }
            StopReason::Cancelled => "cancelled by the user".into(),
        }
    }

    /// The first no-progress stop of a turn (a budget checkpoint or the stall
    /// guard) does not end it: the model gets one fresh allowance and clean
    /// stall counters, with an instruction to drop the approach that is
    /// repeating. Returns false once that pivot is spent, and the turn ends
    /// as before.
    pub fn try_pivot(&mut self) -> bool {
        if self.pivot_used {
            return false;
        }
        self.pivot_used = true;
        self.ledger.reset();
        self.epoch_tool_calls = 0;
        self.epoch_model_calls = 0;
        self.epoch_started_at = Instant::now();
        true
    }

    /// The instruction sent to the model when a checkpoint pivots.
    pub fn pivot_message(&self, reason: StopReason) -> String {
        let why = match reason {
            StopReason::Stalled => {
                "recent calls keep repeating without changing a file or passing a verification"
                    .to_string()
            }
            _ => "the calls since the last checkpoint changed no file, passed no verification, and returned nothing new".to_string(),
        };
        format!(
            "[totality] {}: {why}; the current approach is not working. Do not repeat it or try small variations of it, and do not search for a ready-made tool that may not exist. In one sentence, say why it failed, then switch to a different method: for example, write and run a small script with what is installed, use a different tool, or read the relevant documentation once. You have one more allowance; if it also makes no progress, the turn ends.",
            reason.code()
        )
    }

    /// User-facing notice when a productive checkpoint renews the allowance.
    pub fn renewal_message(&self, trigger: StopReason) -> String {
        format!(
            "budget checkpoint renewed ({}): {} tool calls, {} model calls, {} min used; the run is still changing files, passing verification, or learning something new, so the turn continues.",
            trigger.code(),
            self.tool_calls,
            self.model_calls,
            self.elapsed().as_secs() / 60
        )
    }

    fn renew(&mut self, trigger: StopReason) -> BudgetCheck {
        self.epoch_tool_calls = 0;
        self.epoch_model_calls = 0;
        self.epoch_started_at = Instant::now();
        self.renewals += 1;
        // A renewal must be earned again by new work in the next allowance.
        self.ledger.start_checkpoint();
        BudgetCheck::Renewed(trigger)
    }
}

fn result_digest(result: &str) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    result.hash(&mut hasher);
    hasher.finish()
}

/// One tool result as the progress ledger sees it.
#[derive(Debug, Clone, Copy)]
pub struct ToolOutcome<'a> {
    /// Canonical family of the call: `bash` by program and operation, other
    /// tools by name and key arguments. Names the idle family in a stop
    /// message and keys known errors.
    pub family: &'a str,
    /// Raw call arguments, so an error echoing them back is not new.
    pub arguments: &'a str,
    /// Result content shown to the model.
    pub result: &'a str,
    pub is_error: bool,
    pub changed_file: bool,
    pub verified_ok: bool,
}

/// Tools whose every call only reads. The list is the registry's read-only
/// half (`Tool::read_only`), named here because the guard sees a tool name,
/// not a handle: `read`, `grep`, `glob`, `recall`, `repo_map`, `lsp`,
/// `evidence`, `fetch`, `skill`, `review`, `iocs`, `surface`, `attack_path`.
/// `symbol` is `read` with `symbol`, and `project`/`hunk` are helpers, not
/// tools.
const READ_ONLY_TOOLS: &[&str] = &[
    "read",
    "grep",
    "glob",
    "recall",
    "repo_map",
    "lsp",
    "evidence",
    "fetch",
    "skill",
    "review",
    "iocs",
    "surface",
    "attack_path",
];

/// Tools whose calls read only for some actions. The rest of each schema is
/// there to change something: `bg start`/`stop` runs and kills a process,
/// `todo plan`/`start`/`done` rewrite the plan, `thread split`/`note`/`close`
/// rewrite the thread, `finding add`/`update`/`report`/`verify` writes the
/// ledger, `detect run` creates findings, and `pr draft`/`comment` creates or
/// writes a pull request.
const READ_ONLY_ACTIONS: &[(&str, &[&str])] = &[
    // `status` lists every process when no id is given; `list` is the same
    // read under its clearer name.
    ("bg", &["logs", "status", "list"]),
    ("todo", &["list"]),
    ("thread", &["status"]),
    ("finding", &["list"]),
    ("detect", &["validate"]),
    ("pr", &["checks", "review"]),
];

/// `git` tool actions that only read. `branch` and `worktree` read in their
/// `op = "list"` form. `status`, `diff`, `log`, and `blame` are handed to
/// `bash`, where the command classifier decides they read only.
const GIT_READ_ACTIONS: &[&str] = &["commit_message"];

/// Whether a call only inspects. `bash` is classified by its command, `git`
/// and the mixed tools by their action, and the rest by the tool's own
/// declaration. A tool this does not know (an MCP or external tool) is not
/// read-only here; its own `Tool::read_only` is consulted by the caller.
pub fn call_read_only(name: &str, arguments: &str) -> bool {
    let args: Value = serde_json::from_str(arguments).unwrap_or(Value::Null);
    let action = args.get("action").and_then(Value::as_str).unwrap_or("");
    match name {
        "bash" => args
            .get("command")
            .and_then(Value::as_str)
            .is_some_and(crate::policy::command::reads_only),
        "git" => {
            let op = args.get("op").and_then(Value::as_str).unwrap_or("");
            GIT_READ_ACTIONS.contains(&action)
                || (matches!(action, "branch" | "worktree") && op == "list")
        }
        _ => match READ_ONLY_ACTIONS.iter().find(|(tool, _)| *tool == name) {
            Some((_, actions)) => actions.contains(&action),
            None => READ_ONLY_TOOLS.contains(&name),
        },
    }
}

/// The single progress decision for a turn.
///
/// Each result is one [`ProgressEvent`]. Strong progress (`changed`,
/// `verified`) clears the window. Weak progress (`learned`, `failed-new`)
/// adds a productive call to it, and an idle call adds an idle one. The run
/// is stalled when `budget` of the last `2 × budget` calls since the last
/// strong progress were idle: a plain idle streak stalls at exactly `budget`,
/// and a model that re-runs one command between new probes stalls too,
/// while exploration that keeps learning never does.
///
/// At the stall decision the workspace is read once and compared with the
/// reading taken when the window's first idle call came in: a `sed -i`
/// through `bash` changes files without reporting it, and that is progress.
/// The loop guard's certain repeats spend the budget at once
/// ([`ProgressLedger::record_loop`]).
#[derive(Debug)]
pub struct ProgressLedger {
    budget: u32,
    /// Idle flags of the recent calls since the last strong progress, at
    /// most `2 × budget`.
    window: VecDeque<bool>,
    /// Digests of successful outputs seen this turn (header line excluded).
    seen_results: HashSet<u64>,
    /// Digests of (family, error) pairs seen this turn, with the call's own
    /// arguments removed from the error text.
    seen_errors: HashSet<u64>,
    /// Idle calls per family in the window, for the stop message.
    idle_families: HashMap<String, u32>,
    /// The loop guard's reason, when a certain loop spent the budget.
    loop_reason: Option<String>,
    /// Workspace identity when the window's first idle call came in.
    baseline: Option<String>,
    /// Whether the stall decision already took its fresh reading.
    checked: bool,
    /// A change, a passing verify, or a new result since the last
    /// checkpoint renewal (or pivot). A new error does not count.
    since_checkpoint: bool,
    workspace: WorkspaceMemo,
    /// How the workspace is identified. A field so a test can count reads.
    fingerprint: Fingerprint,
}

impl ProgressLedger {
    pub fn new(budget: u32) -> Self {
        Self {
            budget: budget.max(1),
            window: VecDeque::new(),
            seen_results: HashSet::new(),
            seen_errors: HashSet::new(),
            idle_families: HashMap::new(),
            loop_reason: None,
            baseline: None,
            checked: false,
            since_checkpoint: false,
            workspace: WorkspaceMemo::default(),
            fingerprint: workspace_state,
        }
    }

    pub fn observe(&mut self, cwd: &Path, outcome: ToolOutcome<'_>) -> ProgressEvent {
        self.workspace.begin_call();
        let event = if outcome.changed_file {
            ProgressEvent::Changed
        } else if outcome.verified_ok {
            ProgressEvent::Verified
        } else if outcome.is_error {
            let key = error_digest(outcome.family, outcome.arguments, outcome.result);
            if self.seen_errors.insert(key) {
                ProgressEvent::FailedNew
            } else {
                ProgressEvent::Idle
            }
        } else if self
            .seen_results
            .insert(result_digest(news(outcome.result)))
        {
            ProgressEvent::Learned
        } else {
            ProgressEvent::Idle
        };
        match event {
            ProgressEvent::Changed | ProgressEvent::Verified => {
                self.workspace.invalidate();
                self.clear_window();
                self.since_checkpoint = true;
            }
            ProgressEvent::Learned => {
                self.push(false, None);
                self.since_checkpoint = true;
            }
            // A new error keeps the idle window open (it is information), but
            // it does not buy another checkpoint allowance: a run that only
            // finds new ways to fail is what the safety net is for.
            ProgressEvent::FailedNew => self.push(false, None),
            ProgressEvent::Idle => {
                if self.idle() == 0 {
                    self.baseline = self.workspace.baseline(cwd, self.fingerprint);
                }
                self.push(true, Some(coarse_family(outcome.family)));
                if self.stalled() && !self.checked && self.loop_reason.is_none() {
                    self.checked = true;
                    if self.workspace_moved(cwd) {
                        self.clear_window();
                        self.since_checkpoint = true;
                        return ProgressEvent::Changed;
                    }
                }
            }
        }
        event
    }

    fn push(&mut self, idle: bool, family: Option<String>) {
        self.window.push_back(idle);
        if let Some(family) = family {
            *self.idle_families.entry(family).or_default() += 1;
        }
        let capacity = (self.budget as usize) * 2;
        while self.window.len() > capacity {
            self.window.pop_front();
        }
    }

    fn clear_window(&mut self) {
        self.window.clear();
        self.idle_families.clear();
        self.baseline = None;
        self.checked = false;
    }

    fn workspace_moved(&mut self, cwd: &Path) -> bool {
        let now = self.workspace.fresh(cwd, self.fingerprint);
        matches!(
            (self.baseline.as_deref(), now.as_deref()),
            (Some(before), Some(after)) if before != after
        )
    }

    /// A certain loop spends the idle budget at once.
    pub fn record_loop(&mut self, reason: String) {
        self.loop_reason.get_or_insert(reason);
    }

    /// Idle calls in the window.
    pub fn idle(&self) -> u32 {
        self.window.iter().filter(|idle| **idle).count() as u32
    }

    pub fn stalled(&self) -> bool {
        self.loop_reason.is_some() || self.idle() >= self.budget
    }

    pub fn progressed_since_checkpoint(&self) -> bool {
        self.since_checkpoint
    }

    pub fn start_checkpoint(&mut self) {
        self.since_checkpoint = false;
    }

    /// After a pivot the next approach is judged on its own calls. Results
    /// already seen stay seen: repeating them is still idle.
    pub fn reset(&mut self) {
        self.clear_window();
        self.loop_reason = None;
        self.since_checkpoint = false;
    }

    /// End of a batch: a later call in it may have moved the workspace.
    pub fn recheck(&mut self, cwd: &Path) {
        if !self.stalled() || self.loop_reason.is_some() {
            return;
        }
        self.workspace.begin_call();
        if self.workspace_moved(cwd) {
            self.clear_window();
            self.since_checkpoint = true;
        }
    }

    pub fn stall_message(&self) -> String {
        if let Some(reason) = &self.loop_reason {
            return reason.clone();
        }
        let family = self
            .idle_families
            .iter()
            .max_by(|a, b| a.1.cmp(b.1).then_with(|| b.0.cmp(a.0)))
            .map(|(family, count)| format!(" ({count} of them {family})"))
            .unwrap_or_default();
        format!(
            "{} of the last {} calls{family} changed no file, passed no verification, and returned nothing new; stop repeating them and report the last known state, or change approach in a new turn",
            self.idle(),
            self.window.len()
        )
    }
}

/// The part of a result that is news: the `[tool] name - args` header line
/// dropped, so different arguments with the same answer tell us nothing new.
/// Digits stay: a line number, a revision, or a count is often the answer.
fn news(result: &str) -> &str {
    match result.split_once('\n') {
        Some((first, rest)) if first.starts_with("[tool] ") => rest,
        _ if result.starts_with("[tool] ") => "",
        _ => result,
    }
}

/// A failure keyed by coarse family and message, with the call's own argument
/// words removed: `cat missing-3.txt` and `cat missing-4.txt` fail with the
/// same error once the echoed path is set aside.
fn error_digest(family: &str, arguments: &str, result: &str) -> u64 {
    let trim = |word: &str| {
        word.trim_matches(|ch: char| !ch.is_alphanumeric() && !matches!(ch, '.' | '_' | '-' | '/'))
            .trim_end_matches('.')
            .to_string()
    };
    let args: HashSet<String> = arguments
        .split(|ch: char| {
            ch.is_whitespace() || matches!(ch, '"' | '\'' | ',' | '{' | '}' | '[' | ']' | ':' | '=')
        })
        .map(trim)
        .filter(|word| word.len() >= 3)
        .collect();
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    // The coarse family: a `read` family names its path, and a path the
    // error echoes is exactly what must not make it new.
    coarse_family(family).hash(&mut hasher);
    for word in news(result).split_whitespace() {
        let core = trim(word);
        if core.len() >= 3 && args.contains(&core) {
            continue;
        }
        word.hash(&mut hasher);
    }
    hasher.finish()
}

/// How the workspace is identified. A function pointer rather than a closure
/// so a test can replace it with a counter.
type Fingerprint = fn(&Path) -> Option<String>;

/// The workspace identity, read lazily and memoized across calls.
///
/// Reading it costs a `git status` plus a stat per listed path, or a bounded
/// walk outside a repository, so it is taken once and reused: the first
/// counter that needs a baseline reads it, every later counter reuses it, and
/// only a reported file change clears it. A trip decision takes one fresh
/// reading — a `sed -i` through `bash` changes files without the harness
/// seeing it — and that reading becomes the new baseline. Two counters that
/// trip on the same call share the reading.
#[derive(Debug, Default)]
struct WorkspaceMemo {
    cwd: Option<PathBuf>,
    state: Option<String>,
    /// Whether `state` is current for `cwd`.
    valid: bool,
    /// Whether this call already took a fresh reading for a trip decision.
    fresh_taken: bool,
}

impl WorkspaceMemo {
    fn begin_call(&mut self) {
        self.fresh_taken = false;
    }

    /// The cached identity, read now only if the memo is empty or invalid.
    fn baseline(&mut self, cwd: &Path, fingerprint: Fingerprint) -> Option<String> {
        if !self.valid || self.cwd.as_deref() != Some(cwd) {
            self.read(cwd, fingerprint);
        }
        self.state.clone()
    }

    /// The identity as it is now, for a trip decision. One reading per call.
    fn fresh(&mut self, cwd: &Path, fingerprint: Fingerprint) -> Option<String> {
        if !self.fresh_taken || self.cwd.as_deref() != Some(cwd) {
            self.read(cwd, fingerprint);
        }
        self.state.clone()
    }

    fn read(&mut self, cwd: &Path, fingerprint: Fingerprint) {
        self.state = fingerprint(cwd);
        self.cwd = Some(cwd.to_path_buf());
        self.valid = true;
        self.fresh_taken = true;
    }

    fn invalidate(&mut self) {
        self.valid = false;
    }
}

/// Identity of the working tree, used to tell real work from a loop: `git
/// status` plus the size and mtime of every changed or untracked path in a
/// repository (nothing is written to the object store), otherwise a bounded
/// walk of paths, sizes, and modification times. `None` when it cannot be
/// read.
pub fn workspace_state(cwd: &Path) -> Option<String> {
    if let Some(state) = crate::tools::git::working_state(cwd) {
        return Some(state);
    }
    if !cwd.is_dir() {
        return None;
    }
    Some(metadata_digest(cwd))
}

/// Bounded walk for a directory that is not a git repository. Build output and
/// dependency trees are skipped, like the repository map does, so a build does
/// not read as a file change.
fn metadata_digest(root: &Path) -> String {
    const MAX_ENTRIES: usize = 2_000;
    let mut builder = ignore::WalkBuilder::new(root);
    builder
        .hidden(true)
        .git_ignore(true)
        .git_global(false)
        .git_exclude(true)
        .parents(false)
        .follow_links(false)
        .filter_entry(|entry| {
            let name = entry.file_name().to_string_lossy();
            !matches!(
                name.as_ref(),
                ".git"
                    | ".hg"
                    | ".svn"
                    | "target"
                    | "node_modules"
                    | "dist"
                    | "build"
                    | ".venv"
                    | "venv"
                    | "__pycache__"
                    | ".next"
                    | ".cache"
                    | "vendor"
            )
        });
    let mut entries: Vec<(String, u64, u128)> = Vec::new();
    for entry in builder.build().flatten() {
        if entries.len() >= MAX_ENTRIES {
            break;
        }
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        if !meta.is_file() {
            continue;
        }
        let modified = meta
            .modified()
            .ok()
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(0, |elapsed| elapsed.as_nanos());
        let path = entry.path().strip_prefix(root).unwrap_or(entry.path());
        entries.push((path.to_string_lossy().into_owned(), meta.len(), modified));
    }
    entries.sort();
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    entries.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

/// Canonical family for a tool call, so near-identical retries count as the
/// same operation: `bash` calls by program (and subcommand for tools that take
/// one), other tools by name and key arguments. Body arguments (`content`,
/// `old`, `new`, ...) do not create a new family.
pub fn tool_family(name: &str, arguments: &str) -> String {
    let args: Value = serde_json::from_str(arguments).unwrap_or(Value::Null);
    if name == "bash" {
        let command = args.get("command").and_then(Value::as_str).unwrap_or("");
        return format!("bash {}", bash_family(command));
    }
    let args = canonical_args(&args, false);
    if args.is_empty() {
        name.to_string()
    } else {
        format!("{name} {args}")
    }
}

/// Programs whose second bare word is a subcommand worth keeping (`cargo
/// test`, `git status`). For everything else the program alone is the family,
/// so `tesseract a.png`, `tesseract b.png --psm 6` and the like stay together.
const SUBCOMMAND_PROGRAMS: &[&str] = &[
    "cargo",
    "git",
    "go",
    "dotnet",
    "npm",
    "pnpm",
    "yarn",
    "bun",
    "uv",
    "pip",
    "pip3",
    "rustup",
    "kubectl",
    "docker",
    "podman",
    "systemctl",
    "mise",
    "nix",
    "gh",
];

const WRAPPERS: &[&str] = &[
    "sudo", "doas", "env", "time", "timeout", "command", "nohup", "exec", "nice", "stdbuf",
];

/// Flags that take a value, so the word after them is not the operation:
/// `git -C repo status`, `kubectl -n ns get pods`, `nice -n 5 cargo test`.
const VALUE_FLAGS: &[&str] = &[
    "-C",
    "-c",
    "-n",
    "-u",
    "-b",
    "-R",
    "-s",
    "--context",
    "--namespace",
    "--target",
    "--profile",
];

/// The family without its subcommand, for the unproductive-success counter:
/// `bash cargo test` and `bash cargo build` are both `bash cargo`, and every
/// read of the same tool collapses to the tool name.
fn coarse_family(family: &str) -> String {
    if let Some(rest) = family.strip_prefix("bash ") {
        let program = rest.split(' ').next().unwrap_or(rest);
        return format!("bash {program}");
    }
    family.split(' ').next().unwrap_or(family).to_string()
}

/// Argument keys that carry code or file bodies. Their contents change between
/// retries, so they say nothing about the operation.
const BODY_KEYS: &[&str] = &[
    "content",
    "old",
    "new",
    "replacement",
    "replace",
    "patch",
    "diff",
    "code",
    "script",
    "text",
    "body",
    "source",
];

fn bash_family(command: &str) -> String {
    let tokens = shell_words(&strip_heredocs(command));
    let mut index = 0;
    let program = loop {
        let Some(token) = tokens.get(index) else {
            return "shell".into();
        };
        if is_assignment(token) {
            index += 1;
            continue;
        }
        if matches!(token.as_str(), "cd" | "pushd" | "popd") {
            index = skip_flags(&tokens, index + 1);
            if tokens.get(index).is_some() {
                index += 1;
            }
            continue;
        }
        if WRAPPERS.contains(&token.as_str()) {
            index = skip_flags(&tokens, index + 1);
            // `timeout 30 cargo test`: the duration is a bare word.
            if token == "timeout" && tokens.get(index).is_some() {
                index += 1;
            }
            continue;
        }
        break token.as_str();
    };
    let program = program.rsplit('/').next().unwrap_or(program);
    if !SUBCOMMAND_PROGRAMS.contains(&program) {
        return program.to_string();
    }
    let operation = tokens
        .get(skip_flags(&tokens, index + 1))
        .filter(|token| is_operation(token))
        .map(String::as_str);
    match operation {
        Some(operation) => format!("{program} {operation}"),
        None => program.to_string(),
    }
}

/// Skip flags starting at `index`, consuming the value of a flag that takes
/// one. Returns the index of the first word that is not a flag or a value.
fn skip_flags(tokens: &[String], mut index: usize) -> usize {
    while let Some(token) = tokens.get(index) {
        if !token.starts_with('-') {
            break;
        }
        let takes_value = VALUE_FLAGS.contains(&token.as_str());
        index += 1;
        if takes_value {
            index += 1;
        }
    }
    index
}

fn is_assignment(token: &str) -> bool {
    let Some((name, _)) = token.split_once('=') else {
        return false;
    };
    !name.is_empty()
        && name
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
}

fn is_operation(token: &str) -> bool {
    !token.is_empty()
        && !token.starts_with('-')
        && !token.contains(['/', '=', '$', '*', '?'])
        && !token.chars().all(|ch| ch.is_ascii_digit())
        && token
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.' | ':'))
}

/// Remove here-document bodies, which are code and differ between retries.
fn strip_heredocs(command: &str) -> String {
    let mut kept: Vec<&str> = Vec::new();
    let mut delimiter: Option<String> = None;
    for line in command.lines() {
        if let Some(wanted) = &delimiter {
            if line.trim() == wanted {
                delimiter = None;
            }
            continue;
        }
        kept.push(line);
        delimiter = heredoc_delimiter(line);
    }
    if kept.is_empty() {
        command.to_string()
    } else {
        kept.join("\n")
    }
}

fn heredoc_delimiter(line: &str) -> Option<String> {
    let bytes = line.as_bytes();
    let mut quote: Option<u8> = None;
    let mut index = 0;
    while index + 1 < bytes.len() {
        let byte = bytes[index];
        match quote {
            Some(open) => {
                if byte == open {
                    quote = None;
                }
                index += 1;
                continue;
            }
            None => match byte {
                b'\'' | b'"' => {
                    quote = Some(byte);
                    index += 1;
                    continue;
                }
                b'<' if bytes[index + 1] == b'<' => {
                    let mut start = index + 2;
                    if bytes.get(start) == Some(&b'-') {
                        start += 1;
                    }
                    let rest = line[start..].trim_start();
                    let delimiter: String = rest
                        .trim_start_matches(['\'', '"'])
                        .chars()
                        .take_while(|ch| {
                            !ch.is_whitespace() && !matches!(ch, '\'' | '"' | ';' | '|')
                        })
                        .collect();
                    return (!delimiter.is_empty()).then_some(delimiter);
                }
                _ => {}
            },
        }
        index += 1;
    }
    None
}

/// Split a shell command into words, dropping quotes and separators. Enough to
/// name the program and subcommand; not a shell parser.
fn shell_words(command: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    let mut chars = command.chars();
    while let Some(ch) = chars.next() {
        match quote {
            Some(open) => {
                if ch == open {
                    quote = None;
                } else {
                    current.push(ch);
                }
            }
            None => match ch {
                '\'' | '"' => quote = Some(ch),
                '\\' => {
                    if let Some(next) = chars.next() {
                        current.push(next);
                    }
                }
                ch if ch.is_whitespace() || matches!(ch, ';' | '|' | '&' | '<' | '>') => {
                    if !current.is_empty() {
                        words.push(std::mem::take(&mut current));
                    }
                }
                ch => current.push(ch),
            },
        }
    }
    if !current.is_empty() {
        words.push(current);
    }
    words
}

fn canonical_args(args: &Value, keep_bodies: bool) -> String {
    let Value::Object(map) = args else {
        return String::new();
    };
    let mut parts: Vec<String> = Vec::new();
    for (key, value) in map {
        if value.is_null() {
            continue;
        }
        let value = if !keep_bodies && BODY_KEYS.contains(&key.as_str()) {
            "<body>".to_string()
        } else {
            canonical_value(value)
        };
        parts.push(format!("{key}={value}"));
    }
    parts.sort();
    parts.join(",")
}

fn canonical_value(value: &Value) -> String {
    match value {
        Value::String(text) => collapse(text),
        Value::Array(items) => {
            let mut items: Vec<String> = items.iter().map(canonical_value).collect();
            items.sort();
            format!("[{}]", items.join(","))
        }
        Value::Bool(flag) => flag.to_string(),
        Value::Number(number) => number.to_string(),
        Value::Null => String::new(),
        other => collapse(&other.to_string()),
    }
}

/// Collapse whitespace so formatting differences do not create a new call.
fn collapse(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limits() -> RunLimits {
        RunLimits {
            max_tool_calls: 10,
            max_model_calls: 4,
            max_wall: Duration::from_secs(600),
            idle_budget: 3,
            verify_attempts: 2,
        }
    }

    fn ago(duration: Duration) -> Instant {
        Instant::now()
            .checked_sub(duration)
            .expect("monotonic clock has room")
    }

    #[test]
    fn the_wall_clock_covers_several_calls_at_the_measured_speed() {
        let mut progress = RunProgress::new(RunLimits {
            max_wall: Duration::from_secs(60),
            ..limits()
        });
        // No call measured yet: the configured allowance.
        assert_eq!(progress.effective_wall(), Duration::from_secs(60));
        // A fast engine never shrinks it.
        progress.record_call_duration(Duration::from_secs(2));
        assert_eq!(progress.effective_wall(), Duration::from_secs(60));
        // A slow one stretches it to WALL_CALLS of its slowest recent call.
        progress.record_call_duration(Duration::from_secs(40));
        assert_eq!(progress.effective_wall(), Duration::from_secs(240));

        // 90 seconds in: past the configured minute, within the scaled wall.
        progress.epoch_started_at = ago(Duration::from_secs(90));
        assert!(matches!(progress.model_checkpoint(), BudgetCheck::Ok));
        progress.epoch_started_at = ago(Duration::from_secs(250));
        assert!(matches!(
            progress.model_checkpoint(),
            BudgetCheck::Exhausted(StopReason::WallClock)
        ));
    }

    #[test]
    fn only_the_last_few_calls_set_the_pace() {
        let mut progress = RunProgress::new(RunLimits {
            max_wall: Duration::from_secs(60),
            ..limits()
        });
        progress.record_call_duration(Duration::from_secs(100));
        for _ in 0..RECENT_CALLS {
            progress.record_call_duration(Duration::from_secs(1));
        }
        assert_eq!(progress.effective_wall(), Duration::from_secs(60));
    }

    /// A fresh, empty workspace: its state never changes during a test.
    fn empty_workspace() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    fn outcome<'a>(family: &'a str, result: &'a str) -> ToolOutcome<'a> {
        ToolOutcome {
            family,
            arguments: "",
            result,
            is_error: false,
            changed_file: false,
            verified_ok: false,
        }
    }

    fn ok(progress: &mut RunProgress, cwd: &Path, family: &str, result: &str) -> ProgressEvent {
        progress.record_tool_result(cwd, outcome(family, result))
    }

    fn fail(progress: &mut RunProgress, cwd: &Path, family: &str, result: &str) -> ProgressEvent {
        progress.record_tool_result(
            cwd,
            ToolOutcome {
                is_error: true,
                ..outcome(family, result)
            },
        )
    }

    fn changed(progress: &mut RunProgress, cwd: &Path) -> ProgressEvent {
        progress.record_tool_result(
            cwd,
            ToolOutcome {
                changed_file: true,
                ..outcome("write", "[tool] write - a.rs\nwrote a.rs")
            },
        )
    }

    #[test]
    fn each_result_is_one_progress_event() {
        let dir = empty_workspace();
        let cwd = dir.path();
        let mut progress = RunProgress::new(limits());
        assert_eq!(changed(&mut progress, cwd), ProgressEvent::Changed);
        let verified = progress.record_tool_result(
            cwd,
            ToolOutcome {
                verified_ok: true,
                ..outcome("verify", "passed")
            },
        );
        assert_eq!(verified, ProgressEvent::Verified);
        assert_eq!(
            ok(&mut progress, cwd, "read", "fn a()"),
            ProgressEvent::Learned
        );
        assert_eq!(
            ok(&mut progress, cwd, "read", "fn a()"),
            ProgressEvent::Idle
        );
        assert_eq!(
            fail(&mut progress, cwd, "bash cargo", "E0425"),
            ProgressEvent::FailedNew
        );
        assert_eq!(
            fail(&mut progress, cwd, "bash cargo", "E0425"),
            ProgressEvent::Idle
        );
        // The same message from another family is new information.
        assert_eq!(
            fail(&mut progress, cwd, "bash rustc", "E0425"),
            ProgressEvent::FailedNew
        );
    }

    #[test]
    fn the_header_line_is_not_news() {
        let dir = empty_workspace();
        let cwd = dir.path();
        let mut progress = RunProgress::new(limits());
        assert_eq!(
            ok(&mut progress, cwd, "grep", "[tool] grep - foo\nno matches"),
            ProgressEvent::Learned
        );
        // Different arguments, the same answer: nothing new was learned.
        assert_eq!(
            ok(&mut progress, cwd, "grep", "[tool] grep - bar\nno matches"),
            ProgressEvent::Idle
        );
        assert_eq!(
            ok(&mut progress, cwd, "read", "[tool] read - a.rs\nline 1"),
            ProgressEvent::Learned
        );
        assert_eq!(
            ok(&mut progress, cwd, "read", "[tool] read - b.rs\nline 2"),
            ProgressEvent::Learned,
            "a number is often the answer"
        );
    }

    #[test]
    fn idle_calls_in_a_row_stall_and_progress_resets_them() {
        let dir = empty_workspace();
        let cwd = dir.path();
        let mut progress = RunProgress::new(limits());
        ok(&mut progress, cwd, "bash make", "same");
        ok(&mut progress, cwd, "bash make", "same");
        ok(&mut progress, cwd, "bash make", "same");
        assert!(!progress.stalled(), "two idle calls after one learned");
        changed(&mut progress, cwd);
        ok(&mut progress, cwd, "bash make", "same");
        ok(&mut progress, cwd, "bash make", "same");
        assert!(!progress.stalled());
        ok(&mut progress, cwd, "bash make", "same");
        assert!(progress.stalled());
        assert!(
            progress.stall_message().contains("3 of them bash make"),
            "{}",
            progress.stall_message()
        );
    }

    /// Distinct `python3 -c` probes with new output each time are exploration:
    /// every one is `learned`, so the run keeps going (the old success-family
    /// budget stopped it).
    #[test]
    fn distinct_probes_with_new_output_keep_running() {
        let dir = empty_workspace();
        let cwd = dir.path();
        let mut progress = RunProgress::new(limits());
        for index in 0..40 {
            let result = format!("[tool] bash - python3 -c ...\nvalue {}", "x".repeat(index));
            ok(&mut progress, cwd, "bash python3", &result);
        }
        assert!(!progress.stalled());
    }

    /// The D09 run: one command re-run between probes that each return
    /// something new. No unbroken idle streak, but half the window is idle.
    #[test]
    fn a_repeat_interleaved_with_new_probes_stalls_by_density() {
        let dir = empty_workspace();
        let cwd = dir.path();
        let mut progress = RunProgress::new(limits());
        let mut calls = 0;
        while !progress.stalled() && calls < 40 {
            ok(&mut progress, cwd, "bash sed", &format!("TOTAL {calls}"));
            ok(&mut progress, cwd, "bash sed", "TOTAL 42");
            calls += 2;
        }
        assert!(
            progress.stalled(),
            "the repeat must not hide behind the probes"
        );
        assert!(calls <= 8, "{calls}");
    }

    #[test]
    fn an_error_that_only_echoes_its_arguments_is_not_new() {
        let dir = empty_workspace();
        let cwd = dir.path();
        let mut progress = RunProgress::new(limits());
        let failure = |progress: &mut RunProgress, name: &str| {
            let args = serde_json::json!({"command": format!("cat {name}")}).to_string();
            let result = format!(
                "[tool] bash - cat {name}\ncat: {name}: No such file or directory\n[exit 1]"
            );
            progress.record_tool_result(
                cwd,
                ToolOutcome {
                    arguments: &args,
                    is_error: true,
                    ..outcome("bash cat", &result)
                },
            )
        };
        assert_eq!(
            failure(&mut progress, "missing-1.txt"),
            ProgressEvent::FailedNew
        );
        assert_eq!(failure(&mut progress, "missing-2.txt"), ProgressEvent::Idle);
        let other = progress.record_tool_result(
            cwd,
            ToolOutcome {
                is_error: true,
                ..outcome("bash cat", "cat: permission denied")
            },
        );
        assert_eq!(
            other,
            ProgressEvent::FailedNew,
            "a different message is news"
        );
    }

    /// Two failing calls alternating: each error is new once, then idle.
    #[test]
    fn alternating_known_failures_stall() {
        let dir = empty_workspace();
        let cwd = dir.path();
        let mut progress = RunProgress::new(limits());
        for _ in 0..2 {
            fail(&mut progress, cwd, "bash cargo", "error: a");
            fail(&mut progress, cwd, "bash npm", "error: b");
        }
        assert!(!progress.stalled(), "two new errors, then two idle");
        fail(&mut progress, cwd, "bash cargo", "error: a");
        assert!(progress.stalled());
    }

    /// A `bg logs` poll with growing output is `learned`; with static output
    /// it is idle.
    #[test]
    fn background_polls_are_judged_by_their_output() {
        let dir = empty_workspace();
        let cwd = dir.path();
        let mut progress = RunProgress::new(limits());
        let mut log = String::new();
        for line in [
            "compiling a",
            "compiling b",
            "compiling c",
            "compiling d",
            "linking",
        ] {
            log.push_str(line);
            log.push('\n');
            ok(&mut progress, cwd, "bg logs", &log);
        }
        assert!(!progress.stalled());
        for _ in 0..3 {
            ok(&mut progress, cwd, "bg logs", &log);
        }
        assert!(progress.stalled());
    }

    /// A `sed -i` loop changes files without reporting a change: at the
    /// stall decision the workspace moved, so it is progress.
    #[test]
    fn a_workspace_that_moved_under_an_idle_streak_is_progress() {
        let dir = empty_workspace();
        let cwd = dir.path();
        let mut progress = RunProgress::new(limits());
        // The first empty output is news; the next two start the idle streak.
        ok(&mut progress, cwd, "bash sed", "");
        ok(&mut progress, cwd, "bash sed", "");
        ok(&mut progress, cwd, "bash sed", "");
        std::fs::write(cwd.join("a.txt"), "changed").unwrap();
        assert_eq!(
            ok(&mut progress, cwd, "bash sed", ""),
            ProgressEvent::Changed
        );
        assert!(!progress.stalled());
    }

    #[test]
    fn recheck_clears_a_stall_after_a_later_workspace_change() {
        let dir = empty_workspace();
        let cwd = dir.path();
        let mut progress = RunProgress::new(limits());
        for _ in 0..4 {
            ok(&mut progress, cwd, "bash true", "");
        }
        assert!(progress.stalled());
        progress.recheck_workspace(cwd);
        assert!(progress.stalled(), "nothing moved");
        std::fs::write(cwd.join("b.txt"), "x").unwrap();
        progress.recheck_workspace(cwd);
        assert!(!progress.stalled());
    }

    #[test]
    fn a_certain_loop_spends_the_budget_and_keeps_its_reason() {
        let dir = empty_workspace();
        let cwd = dir.path();
        let mut progress = RunProgress::new(limits());
        progress.record_loop("the same call ran 4 times in a row.");
        assert!(progress.stalled());
        // A workspace change cannot clear a certain loop at batch end.
        std::fs::write(cwd.join("c.txt"), "x").unwrap();
        progress.recheck_workspace(cwd);
        assert!(progress.stalled());
        assert!(progress.stall_message().contains("4 times in a row"));
        assert!(progress.try_pivot());
        assert!(!progress.stalled(), "a pivot starts a fresh idle count");
        assert!(!progress.try_pivot(), "one pivot per turn");
    }

    #[test]
    fn results_seen_before_a_pivot_stay_seen() {
        let dir = empty_workspace();
        let cwd = dir.path();
        let mut progress = RunProgress::new(limits());
        ok(&mut progress, cwd, "read", "same file");
        assert!(progress.try_pivot());
        assert_eq!(
            ok(&mut progress, cwd, "read", "same file"),
            ProgressEvent::Idle
        );
    }

    #[test]
    fn checkpoints_renew_only_on_progress_since_the_last_one() {
        let dir = empty_workspace();
        let cwd = dir.path();
        let mut progress = RunProgress::new(limits());
        for _ in 0..4 {
            progress.model_call_started();
        }
        ok(&mut progress, cwd, "read", "new facts");
        assert_eq!(
            progress.model_checkpoint(),
            BudgetCheck::Renewed(StopReason::ModelBudget)
        );
        assert_eq!(progress.renewals, 1);
        assert_eq!(progress.model_calls, 4, "cumulative remains for reporting");
        assert_eq!(progress.model_checkpoint(), BudgetCheck::Ok);
        for _ in 0..4 {
            progress.model_call_started();
        }
        // Only repeats in this allowance: the renewal must be earned again.
        ok(&mut progress, cwd, "read", "new facts");
        assert_eq!(
            progress.model_checkpoint(),
            BudgetCheck::Exhausted(StopReason::ModelBudget)
        );
    }

    #[test]
    fn tool_checkpoint_renews_for_a_productive_batch() {
        let dir = empty_workspace();
        let cwd = dir.path();
        let mut progress = RunProgress::new(limits());
        for index in 0..9 {
            ok(
                &mut progress,
                cwd,
                "read",
                &format!("file {}", "y".repeat(index)),
            );
        }
        assert_eq!(
            progress.tool_checkpoint(3),
            BudgetCheck::Renewed(StopReason::ToolBudget)
        );
        assert_eq!(progress.tool_checkpoint(3), BudgetCheck::Ok);
    }

    #[test]
    fn the_first_no_progress_checkpoint_pivots_once() {
        let mut progress = RunProgress::new(limits());
        for _ in 0..4 {
            progress.model_call_started();
        }
        assert_eq!(
            progress.model_checkpoint(),
            BudgetCheck::Exhausted(StopReason::ModelBudget)
        );
        assert!(progress.try_pivot());
        assert_eq!(
            progress.model_checkpoint(),
            BudgetCheck::Ok,
            "fresh allowance"
        );
        for _ in 0..4 {
            progress.model_call_started();
        }
        assert_eq!(
            progress.model_checkpoint(),
            BudgetCheck::Exhausted(StopReason::ModelBudget)
        );
        assert!(!progress.try_pivot());
    }

    #[test]
    fn workspace_reads_are_memoized_and_bounded() {
        static COMPUTED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        fn counted(cwd: &Path) -> Option<String> {
            COMPUTED.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            workspace_state(cwd)
        }
        let dir = empty_workspace();
        let cwd = dir.path();
        let mut progress = RunProgress::new(RunLimits {
            idle_budget: 1_000,
            ..limits()
        });
        progress.ledger.fingerprint = counted;
        // Many short idle streaks over an unchanged workspace share one reading.
        for index in 0..20 {
            ok(&mut progress, cwd, "bash x", "same");
            ok(
                &mut progress,
                cwd,
                "bash x",
                &format!("new {}", "z".repeat(index)),
            );
        }
        assert_eq!(COMPUTED.load(std::sync::atomic::Ordering::SeqCst), 1);
        // A long streak takes one fresh reading at the stall decision only.
        let mut progress = RunProgress::new(limits());
        progress.ledger.fingerprint = counted;
        COMPUTED.store(0, std::sync::atomic::Ordering::SeqCst);
        for _ in 0..10 {
            ok(&mut progress, cwd, "bash x", "");
        }
        assert_eq!(COMPUTED.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    #[test]
    fn read_only_classification_follows_the_registry() {
        let reads = |name: &str, args: serde_json::Value| call_read_only(name, &args.to_string());
        // Whole tools that only read.
        for tool in READ_ONLY_TOOLS {
            assert!(reads(tool, serde_json::json!({})), "{tool} only reads");
        }
        // Mixed tools: only the reading actions are exempt.
        for (tool, args) in [
            ("bg", serde_json::json!({"action": "logs", "id": "bg-1"})),
            ("bg", serde_json::json!({"action": "status"})),
            ("bg", serde_json::json!({"action": "list"})),
            ("todo", serde_json::json!({"action": "list"})),
            ("thread", serde_json::json!({"action": "status"})),
            ("finding", serde_json::json!({"action": "list"})),
            ("detect", serde_json::json!({"action": "validate"})),
            ("pr", serde_json::json!({"action": "checks"})),
            ("pr", serde_json::json!({"action": "review"})),
            ("git", serde_json::json!({"action": "commit_message"})),
            ("git", serde_json::json!({"action": "branch", "op": "list"})),
        ] {
            assert!(reads(tool, args.clone()), "{tool} {args} reads");
        }
        for (tool, args) in [
            (
                "bg",
                serde_json::json!({"action": "start", "command": "make"}),
            ),
            ("bg", serde_json::json!({"action": "stop", "id": "bg-1"})),
            (
                "todo",
                serde_json::json!({"action": "plan", "tasks": ["a"]}),
            ),
            ("thread", serde_json::json!({"action": "note", "note": "x"})),
            (
                "finding",
                serde_json::json!({"action": "add", "title": "x"}),
            ),
            ("finding", serde_json::json!({"action": "report"})),
            (
                "detect",
                serde_json::json!({"action": "run", "target": "x"}),
            ),
            ("pr", serde_json::json!({"action": "draft"})),
            ("pr", serde_json::json!({"action": "comment", "body": "x"})),
            ("git", serde_json::json!({"action": "commit"})),
            (
                "git",
                serde_json::json!({"action": "branch", "op": "create"}),
            ),
            // Read-only git commands belong to bash now, so the `git` tool
            // must not claim to read them.
            ("git", serde_json::json!({"action": "status"})),
            ("git", serde_json::json!({"action": "diff"})),
            ("git", serde_json::json!({"action": "log"})),
            ("git", serde_json::json!({"action": "blame"})),
            ("write", serde_json::json!({"path": "a", "content": "x"})),
            ("verify", serde_json::json!({})),
            ("bash", serde_json::json!({"command": "cargo test"})),
            (
                "bash",
                serde_json::json!({"command": "curl -X POST http://x/y"}),
            ),
        ] {
            assert!(!reads(tool, args.clone()), "{tool} {args} changes things");
        }
    }

    fn args(command: &str) -> String {
        serde_json::json!({"command": command}).to_string()
    }

    #[test]
    fn bash_families_ignore_paths_numbers_and_code_bodies() {
        assert_eq!(
            tool_family(
                "bash",
                &args("tesseract /tmp/a/receipt-1.png out-1.txt --psm 6")
            ),
            "bash tesseract"
        );
        assert_eq!(
            tool_family("bash", &args("tesseract /tmp/b/receipt-2.png out-2.txt")),
            "bash tesseract"
        );
        assert_eq!(
            tool_family(
                "bash",
                &args("python3 -c \"from PIL import Image; print(1)\"")
            ),
            "bash python3"
        );
        assert_eq!(
            tool_family(
                "bash",
                &args("python3 -c \"from PIL import Image; print(2); print(3)\"")
            ),
            "bash python3"
        );
        assert_eq!(
            tool_family(
                "bash",
                &args(
                    "cd /tmp/scratch-7 && python3 - <<'EOF'\nfrom PIL import Image\nprint(1)\nEOF"
                )
            ),
            "bash python3"
        );
        assert_eq!(
            tool_family(
                "bash",
                &args("python3 << 'EOF'\nimport zlib\nprint(2)\nEOF")
            ),
            "bash python3"
        );
        assert_eq!(
            tool_family("bash", &args("cargo test --locked")),
            "bash cargo test"
        );
        assert_eq!(tool_family("bash", &args("git status")), "bash git status");
        assert_ne!(
            tool_family("bash", &args("git status")),
            tool_family("bash", &args("git diff"))
        );
        assert_eq!(tool_family("bash", &args("")), "bash shell");
    }

    #[test]
    fn bash_families_skip_flag_values() {
        let family = |command: &str| tool_family("bash", &args(command));
        assert_eq!(family("git -C repo status"), "bash git status");
        assert_eq!(family("git -c user.name=x status"), "bash git status");
        assert_eq!(family("sudo -u deploy cargo test"), "bash cargo test");
        assert_eq!(family("nice -n 5 cargo test"), "bash cargo test");
        assert_eq!(family("kubectl -n ns get pods"), "bash kubectl get");
        assert_eq!(family("timeout 30 cargo test"), "bash cargo test");
        assert_eq!(family("timeout -s KILL 30 cargo test"), "bash cargo test");
        // A here-doc without a space after `<<` is still a here-doc.
        assert_eq!(family("python3 <<\"EOF\"\nprint(1)\nEOF"), "bash python3");
        assert_eq!(family("cat<<'EOF'\nhello\nEOF"), "bash cat");
    }

    #[test]
    fn other_tool_families_keep_targets_and_drop_bodies() {
        let family = |value: serde_json::Value| tool_family("tests", &value.to_string());
        assert_ne!(
            family(serde_json::json!({"path": "a.rs"})),
            family(serde_json::json!({"path": "b.rs"}))
        );
        assert_eq!(
            family(serde_json::json!({"paths": ["b.rs", "a.rs"]})),
            family(serde_json::json!({"paths": ["a.rs", "b.rs"]}))
        );
        assert_eq!(
            tool_family(
                "edit",
                &serde_json::json!({"path": "a.rs", "old": "one", "new": "two"}).to_string()
            ),
            tool_family(
                "edit",
                &serde_json::json!({"path": "a.rs", "old": "three", "new": "four"}).to_string()
            )
        );
        assert_ne!(
            tool_family(
                "write",
                &serde_json::json!({"path": "/tmp/scratch-1/x.txt", "content": "a"}).to_string()
            ),
            tool_family(
                "write",
                &serde_json::json!({"path": "/tmp/scratch-2/x.txt", "content": "b"}).to_string()
            ),
            "temporary-path normalization is for bash families only"
        );
    }
}
