//! The settings catalog behind `/settings`: every scalar config key the
//! harness reads, with its type, what it does, and when a change takes
//! effect. Reading and writing go through the config's own TOML shape, so a
//! value set here is exactly what the same line in `config.toml` would set.
//!
//! Collections (profiles, model profiles, MCP servers, hooks, permission
//! rules, verify steps) are not single values; `/settings` points to the
//! file and the command that manages each one instead.

use anyhow::{Context, Result, bail};

use crate::config::Config;

/// The shape of a setting's value.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Kind {
    Bool,
    /// One of a fixed set of names.
    Choice(&'static [&'static str]),
    /// An integer of at least `min`.
    Int {
        min: i64,
    },
    /// Like `Int`, but may be unset (`-`), which means "automatic".
    OptInt {
        min: i64,
    },
    /// A number in `min..=max`.
    Float {
        min: f64,
        max: f64,
    },
    /// Like `Float`, but may be unset (`-`).
    OptFloat {
        min: f64,
        max: f64,
    },
    /// Free text; empty is allowed.
    Text,
    /// Free text that is never displayed.
    Secret,
    /// A comma-separated list of strings.
    List,
    /// A profile name from the loaded profiles.
    Profile,
    /// A theme name; `/theme` handles it with its live preview.
    Theme,
}

/// When a changed value takes effect in a running TUI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Apply {
    /// The TUI applies it at once; nothing else reads it.
    Ui,
    /// The runtime is rebuilt with the new config (tools, prompt, provider).
    Rebuild,
    /// The session's permission level changes too.
    Permission,
    /// Read only at startup: saved now, used from the next launch.
    Restart,
}

impl Apply {
    pub fn label(self) -> &'static str {
        match self {
            Apply::Ui | Apply::Permission | Apply::Rebuild => "applied now",
            Apply::Restart => "next launch",
        }
    }
}

/// One configurable key.
#[derive(Debug, Clone, Copy)]
pub struct Setting {
    /// Dotted config key, for example `ui.bell` or `review`.
    pub key: &'static str,
    pub kind: Kind,
    pub about: &'static str,
    pub apply: Apply,
    /// Environment variable (without the `TOT_` prefix) that overrides it.
    pub env: Option<&'static str>,
}

impl Setting {
    /// The `[section]` heading this key lives under, or `general`.
    pub fn section(&self) -> &'static str {
        match self.key.split_once('.') {
            Some((section, _)) => section,
            None => "general",
        }
    }

    /// Whether the value may be a secret and the file should stay private.
    pub fn is_secret(&self) -> bool {
        self.kind == Kind::Secret
    }
}

const fn s(key: &'static str, kind: Kind, apply: Apply, about: &'static str) -> Setting {
    Setting {
        key,
        kind,
        about,
        apply,
        env: None,
    }
}

const fn e(
    key: &'static str,
    kind: Kind,
    apply: Apply,
    env: &'static str,
    about: &'static str,
) -> Setting {
    Setting {
        key,
        kind,
        about,
        apply,
        env: Some(env),
    }
}

const SAMPLER: Kind = Kind::OptFloat {
    min: 0.0,
    max: 10.0,
};
const SECS: Kind = Kind::Int { min: 1 };
const COUNT: Kind = Kind::Int { min: 1 };
const ZERO_OK: Kind = Kind::Int { min: 0 };

/// Engine names `[model] engine` accepts; `auto` stores the empty string.
pub const ENGINES: &[&str] = &[
    "auto",
    "llama.cpp",
    "vllm",
    "sglang",
    "ollama",
    "lmstudio",
    "koboldcpp",
    "tabbyapi",
    "mlx",
    "generic",
];

use Apply::{Permission, Rebuild, Restart, Ui};

/// Every setting, in display order.
pub const SETTINGS: &[Setting] = &[
    // Behavior first: the keys people change most.
    s(
        "review",
        Kind::Choice(&["off", "auto", "multi_file", "always"]),
        Rebuild,
        "pause file edits for hunk review: off, auto (risky or 2nd file), multi_file, always",
    ),
    s(
        "permissions.mode",
        Kind::Choice(&["normal", "accept-edits", "deny-all", "plan"]),
        Permission,
        "startup permission level (skip-ask/yolo are per-run only: /permissions)",
    ),
    s(
        "default_profile",
        Kind::Profile,
        Rebuild,
        "tool profile sessions start with; picking one switches to it now",
    ),
    s(
        "gates.auto_settle",
        Kind::Bool,
        Rebuild,
        "run verify and review from the harness when the model settles",
    ),
    s(
        "gates.verify_detected",
        Kind::Bool,
        Rebuild,
        "use verify steps detected from the project manifest when none are saved",
    ),
    s(
        "gates.todo_plan",
        Kind::Choice(&["derive", "require", "off"]),
        Rebuild,
        "plan gate when a run edits its third file",
    ),
    s(
        "format.on_edit",
        Kind::Bool,
        Rebuild,
        "run the project's formatter on edited files it already keeps formatted",
    ),
    s(
        "lsp.gate",
        Kind::Choice(&["off", "warn", "block"]),
        Rebuild,
        "new language-server errors after an edit: ignore, attach, or undo the edit",
    ),
    s(
        "git.commit",
        Kind::Choice(&["ask", "allow"]),
        Rebuild,
        "model commits wait for /commit confirm (ask) or run at once (allow)",
    ),
    s(
        "git.pr",
        Kind::Choice(&["ask", "allow"]),
        Rebuild,
        "draft PRs and PR comments wait for /pr confirm (ask) or run at once",
    ),
    s(
        "git.subject_max",
        COUNT,
        Rebuild,
        "longest accepted commit subject, in characters",
    ),
    // Interface.
    s(
        "ui.theme",
        Kind::Theme,
        Ui,
        "color theme (opens /theme with live preview)",
    ),
    s(
        "ui.color_mode",
        Kind::Choice(&["auto", "truecolor", "256", "16"]),
        Ui,
        "terminal color depth",
    ),
    s(
        "ui.show_reasoning",
        Kind::Bool,
        Ui,
        "expand model thinking in the transcript (Ctrl+T toggles)",
    ),
    s(
        "ui.bell",
        Kind::Bool,
        Ui,
        "ring the terminal bell when something waits on you",
    ),
    s(
        "ui.mouse",
        Kind::Bool,
        Restart,
        "capture the mouse so the wheel scrolls (hold Shift to select text)",
    ),
    // Model connection and sampling.
    e(
        "model.endpoint",
        Kind::Text,
        Rebuild,
        "ENDPOINT",
        "OpenAI-compatible base URL",
    ),
    e(
        "model.model",
        Kind::Text,
        Rebuild,
        "MODEL",
        "model id (opens /model to pick from the endpoint)",
    ),
    e(
        "model.api_key",
        Kind::Secret,
        Rebuild,
        "API_KEY",
        "bearer token for the endpoint; never shown",
    ),
    s(
        "model.engine",
        Kind::Choice(ENGINES),
        Rebuild,
        "server engine; pinning skips the metadata probes",
    ),
    s(
        "model.context_window",
        Kind::OptInt { min: 1 },
        Rebuild,
        "context window in tokens; unset probes the server",
    ),
    s(
        "model.max_output_tokens",
        COUNT,
        Rebuild,
        "longest reply one model call may produce",
    ),
    s(
        "model.temperature",
        Kind::Float { min: 0.0, max: 5.0 },
        Rebuild,
        "sampling temperature (a model profile may override)",
    ),
    s(
        "model.top_p",
        Kind::Float { min: 0.0, max: 1.0 },
        Rebuild,
        "nucleus sampling cutoff (a model profile may override)",
    ),
    s(
        "model.min_p",
        SAMPLER,
        Rebuild,
        "min-p sampling; unset leaves the server's",
    ),
    s(
        "model.repeat_penalty",
        SAMPLER,
        Rebuild,
        "repetition penalty; unset leaves the server's",
    ),
    s(
        "model.dry_multiplier",
        SAMPLER,
        Rebuild,
        "DRY repetition multiplier; unset leaves the server's",
    ),
    s(
        "model.dry_base",
        SAMPLER,
        Rebuild,
        "DRY base; unset leaves the server's",
    ),
    s(
        "model.xtc_probability",
        SAMPLER,
        Rebuild,
        "XTC probability; unset leaves the server's",
    ),
    s(
        "model.seed",
        Kind::OptInt { min: i64::MIN },
        Rebuild,
        "sampling seed; unset leaves the server's",
    ),
    s(
        "model.response_schema",
        Kind::Text,
        Rebuild,
        "JSON Schema file that constrains the final answer; empty disables",
    ),
    s(
        "model.start_command",
        Kind::Text,
        Restart,
        "shell command run at startup when the endpoint is unreachable",
    ),
    s(
        "model.request_timeout_secs",
        SECS,
        Rebuild,
        "longest a whole model call may take",
    ),
    s(
        "model.connect_timeout_secs",
        SECS,
        Rebuild,
        "longest wait to connect to the endpoint",
    ),
    s(
        "model.first_token_timeout_secs",
        SECS,
        Rebuild,
        "longest wait for the first streamed token",
    ),
    s(
        "model.stream_idle_timeout_secs",
        SECS,
        Rebuild,
        "longest silence inside a stream",
    ),
    s(
        "model.retries",
        ZERO_OK,
        Rebuild,
        "resends of a model call that failed for a transient reason",
    ),
    s(
        "model.retry_deadline_secs",
        ZERO_OK,
        Rebuild,
        "longest total wait across those retries",
    ),
    s(
        "model.retry_backoff_ms",
        ZERO_OK,
        Rebuild,
        "first backoff step; later ones are 4x and 15x",
    ),
    // Run budgets.
    s(
        "budgets.max_tool_calls",
        COUNT,
        Rebuild,
        "tool-call checkpoint interval (renews on progress)",
    ),
    s(
        "budgets.max_model_calls",
        COUNT,
        Rebuild,
        "model-call checkpoint interval (renews on progress)",
    ),
    s(
        "budgets.max_wall_minutes",
        ZERO_OK,
        Rebuild,
        "wall-clock checkpoint interval in minutes",
    ),
    s(
        "budgets.idle_budget",
        COUNT,
        Rebuild,
        "idle calls before the run pivots, then stops as stalled",
    ),
    s(
        "budgets.verify_attempts",
        ZERO_OK,
        Rebuild,
        "failed verify runs allowed before the run stops",
    ),
    s(
        "budgets.result_cap_chars",
        Kind::OptInt { min: 1 },
        Rebuild,
        "characters one tool result may use; unset scales with the window",
    ),
    s(
        "budgets.batch_cap_chars",
        Kind::OptInt { min: 1 },
        Rebuild,
        "characters one batch of results may use; unset scales",
    ),
    // System prompt.
    s(
        "prompt.identity",
        Kind::Text,
        Rebuild,
        "replaces the built-in identity line when set",
    ),
    s(
        "prompt.instructions",
        Kind::Text,
        Rebuild,
        "extra instructions appended to the system prompt",
    ),
    s(
        "prompt.file",
        Kind::Text,
        Rebuild,
        "file with extra instructions (config dir, then project)",
    ),
    e(
        "prompt.agents_md",
        Kind::Bool,
        Rebuild,
        "AGENTS_MD",
        "load the AGENTS.md chain into the system prompt",
    ),
    s(
        "prompt.attach_named_files",
        Kind::Bool,
        Rebuild,
        "attach files a prompt names without @",
    ),
    s(
        "skills.index",
        Kind::Choice(&["full", "names", "off"]),
        Rebuild,
        "how much of the skill index the system prompt carries",
    ),
    // Images.
    s(
        "vision.model",
        Kind::Text,
        Rebuild,
        "model that describes images for a text-only model; empty disables",
    ),
    s(
        "vision.endpoint",
        Kind::Text,
        Rebuild,
        "endpoint for vision.model; empty uses model.endpoint",
    ),
    s(
        "vision.api_key",
        Kind::Secret,
        Rebuild,
        "API key for vision.endpoint; never shown",
    ),
    s(
        "vision.allow_remote",
        Kind::Bool,
        Rebuild,
        "allow sending images to a public endpoint",
    ),
    s(
        "vision.max_bytes",
        COUNT,
        Rebuild,
        "largest image accepted, in bytes",
    ),
    s(
        "vision.keep_exif",
        Kind::Bool,
        Rebuild,
        "send images with their metadata instead of stripping it",
    ),
    // Tools.
    s(
        "fetch.allow_private",
        Kind::List,
        Rebuild,
        "hosts the fetch tool may reach on private addresses (comma-separated)",
    ),
    s(
        "hooks.timeout_secs",
        SECS,
        Rebuild,
        "default deadline for a lifecycle hook",
    ),
    s(
        "hooks.max_output_bytes",
        Kind::Int { min: 256 },
        Rebuild,
        "hook output recorded inline; more is archived",
    ),
    // Sessions.
    s(
        "session.dir",
        Kind::Text,
        Restart,
        "where sessions are stored; empty uses the data directory",
    ),
    e(
        "session.no_session",
        Kind::Bool,
        Restart,
        "NO_SESSION",
        "do not write session files",
    ),
    s(
        "session.redact",
        Kind::Bool,
        Restart,
        "redact secrets before session entries and artifacts are written",
    ),
    s(
        "session.artifact_retention_days",
        ZERO_OK,
        Restart,
        "days to keep artifacts; 0 never cleans up",
    ),
    s(
        "session.keep_artifacts",
        Kind::Bool,
        Restart,
        "never clean up artifacts",
    ),
    s(
        "session.clean_artifacts",
        Kind::Bool,
        Restart,
        "clean up expired artifacts at startup",
    ),
];

/// Look a setting up by its dotted key.
pub fn find(key: &str) -> Option<&'static Setting> {
    let key = key.trim();
    SETTINGS.iter().find(|setting| setting.key == key)
}

fn get<'a>(value: &'a toml::Value, key: &str) -> Option<&'a toml::Value> {
    let mut node = value;
    for part in key.split('.') {
        node = node.get(part)?;
    }
    Some(node)
}

/// The effective value of `setting` in `config`; `None` is unset.
pub fn current(config: &Config, setting: &Setting) -> Option<toml::Value> {
    let value = toml::Value::try_from(config).ok()?;
    get(&value, setting.key).cloned()
}

/// A value as `/settings` shows it. Secrets show only whether they are set.
pub fn display(setting: &Setting, value: Option<&toml::Value>) -> String {
    let Some(value) = value else {
        return "(unset)".into();
    };
    match (setting.kind, value) {
        (Kind::Profile, toml::Value::String(text)) if text.is_empty() => "coding (built-in)".into(),
        (Kind::Secret, toml::Value::String(text)) => {
            if text.is_empty() {
                "(empty)".into()
            } else {
                "(set)".into()
            }
        }
        (Kind::Choice(_), toml::Value::String(text)) if text.is_empty() => "auto".into(),
        (_, toml::Value::String(text)) if text.is_empty() => "(empty)".into(),
        (_, toml::Value::String(text)) => one_line(text, 60),
        (_, toml::Value::Array(items)) if items.is_empty() => "(none)".into(),
        (_, toml::Value::Array(items)) => items
            .iter()
            .map(|item| {
                item.as_str()
                    .map(str::to_string)
                    .unwrap_or(item.to_string())
            })
            .collect::<Vec<_>>()
            .join(", "),
        // Show f32 values the way they were typed (0.95, not 0.949999…).
        (_, toml::Value::Float(number)) => format!("{}", *number as f32),
        (_, other) => other.to_string(),
    }
}

fn one_line(text: &str, max: usize) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() > max {
        let mut cut: String = flat.chars().take(max.saturating_sub(1)).collect();
        cut.push('…');
        cut
    } else {
        flat
    }
}

/// Values the picker offers for `setting`, when there is a fixed set.
pub fn choices(setting: &Setting, config: &Config) -> Vec<String> {
    match setting.kind {
        Kind::Bool => vec!["true".into(), "false".into()],
        Kind::Choice(names) => names.iter().map(|name| name.to_string()).collect(),
        Kind::Profile => config.profiles.keys().cloned().collect(),
        _ => Vec::new(),
    }
}

/// Parse user text into the TOML value for `setting`. `Ok(None)` (from `-`,
/// `unset`, or `default`; `auto` for an optional number) removes the key so
/// the built-in default applies again.
pub fn parse(setting: &Setting, raw: &str, config: &Config) -> Result<Option<toml::Value>> {
    let text = raw.trim();
    if matches!(text, "-" | "unset" | "default") {
        return Ok(None);
    }
    let value = match setting.kind {
        Kind::Bool => match text.to_ascii_lowercase().as_str() {
            "true" | "on" | "yes" | "1" => toml::Value::Boolean(true),
            "false" | "off" | "no" | "0" => toml::Value::Boolean(false),
            _ => bail!("{}: use true or false", setting.key),
        },
        Kind::Choice(names) => {
            let lower = text.to_ascii_lowercase();
            let lower = match lower.as_str() {
                "multi-file" | "multi" => "multi_file".to_string(),
                "ask" if setting.key == "permissions.mode" => "normal".to_string(),
                _ => lower,
            };
            if !names.contains(&lower.as_str()) {
                bail!("{}: choose one of {}", setting.key, names.join(", "));
            }
            if setting.key == "model.engine" && lower == "auto" {
                toml::Value::String(String::new())
            } else {
                toml::Value::String(lower)
            }
        }
        Kind::Profile => {
            if !config.profiles.contains_key(text) {
                let names: Vec<&str> = config.profiles.keys().map(String::as_str).collect();
                bail!("unknown profile {text:?}; available: {}", names.join(", "));
            }
            toml::Value::String(text.to_string())
        }
        Kind::Int { min } | Kind::OptInt { min } => {
            if text == "auto" && matches!(setting.kind, Kind::OptInt { .. }) {
                return Ok(None);
            }
            let number: i64 = text
                .replace('_', "")
                .parse()
                .with_context(|| format!("{}: {text:?} is not a whole number", setting.key))?;
            if number < min {
                bail!("{} must be at least {min}", setting.key);
            }
            toml::Value::Integer(number)
        }
        Kind::Float { min, max } | Kind::OptFloat { min, max } => {
            let number: f64 = text
                .parse()
                .with_context(|| format!("{}: {text:?} is not a number", setting.key))?;
            if !(min..=max).contains(&number) {
                bail!("{} must be between {min} and {max}", setting.key);
            }
            toml::Value::Float(number)
        }
        Kind::Text | Kind::Secret | Kind::Theme => {
            // Quotes let a value be explicitly empty: `/settings prompt.file ""`.
            let text = text
                .strip_prefix('"')
                .and_then(|rest| rest.strip_suffix('"'))
                .unwrap_or(text);
            toml::Value::String(text.to_string())
        }
        Kind::List => {
            let items = text
                .split(',')
                .map(str::trim)
                .filter(|item| !item.is_empty())
                .map(|item| toml::Value::String(item.to_string()))
                .collect();
            toml::Value::Array(items)
        }
    };
    Ok(Some(value))
}

/// `config` with `setting` changed to `value` (`None` restores the built-in
/// default), validated. Runtime-only state (the selected profile, trust,
/// sandbox, detected verify steps) is carried over unchanged.
pub fn with_value(
    config: &Config,
    setting: &Setting,
    value: Option<toml::Value>,
) -> Result<Config> {
    let mut tree = toml::Value::try_from(config).context("cannot serialize the config")?;
    let parts: Vec<&str> = setting.key.split('.').collect();
    let (leaf, sections) = parts.split_last().context("empty setting key")?;
    let mut table = tree.as_table_mut().context("config root is not a table")?;
    for section in sections {
        table = table
            .entry(section.to_string())
            .or_insert_with(|| toml::Value::Table(Default::default()))
            .as_table_mut()
            .with_context(|| format!("[{section}] is not a table"))?;
    }
    match value {
        Some(value) => {
            table.insert(leaf.to_string(), value);
        }
        None => {
            // Restore the built-in default rather than leaving a hole: a
            // required field must still deserialize.
            table.remove(*leaf);
            let builtin = toml::Value::try_from(Config::builtin()).context("defaults")?;
            if let Some(default) = get(&builtin, setting.key) {
                table.insert(leaf.to_string(), default.clone());
            }
        }
    }
    let mut next: Config = tree
        .try_into()
        .with_context(|| format!("invalid value for {}", setting.key))?;
    next.validate()?;
    next.profile_name = config.profile_name.clone();
    next.model_profile_override = config.model_profile_override.clone();
    next.derived_surface = config.derived_surface.clone();
    next.trust = config.trust;
    next.sandbox = config.sandbox;
    next.sandbox_explicit = config.sandbox_explicit;
    next.verify.detected_from = config.verify.detected_from.clone();
    Ok(next)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_setting_resolves_against_the_builtin_config() {
        let config = Config::builtin();
        let serialized = toml::Value::try_from(&config).unwrap();
        for setting in SETTINGS {
            let optional = matches!(setting.kind, Kind::OptInt { .. } | Kind::OptFloat { .. });
            if !optional {
                assert!(
                    get(&serialized, setting.key).is_some(),
                    "{} is not a config key",
                    setting.key
                );
            }
        }
        let mut keys: Vec<&str> = SETTINGS.iter().map(|setting| setting.key).collect();
        keys.sort_unstable();
        keys.dedup();
        assert_eq!(keys.len(), SETTINGS.len(), "duplicate setting keys");
    }

    #[test]
    fn review_is_off_by_default() {
        let config = Config::builtin();
        assert_eq!(config.review, crate::config::ReviewMode::Off);
        let setting = find("review").unwrap();
        assert_eq!(display(setting, current(&config, setting).as_ref()), "off");
    }

    #[test]
    fn values_round_trip_through_the_config() {
        let mut config = Config::builtin();
        config.trust = true;
        config.profile_name = "full".into();
        let review = find("review").unwrap();
        let value = parse(review, "always", &config).unwrap();
        let next = with_value(&config, review, value).unwrap();
        assert_eq!(next.review, crate::config::ReviewMode::Always);
        assert!(next.trust, "runtime state is carried over");
        assert_eq!(next.profile_name, "full");

        let bell = find("ui.bell").unwrap();
        let next = with_value(&next, bell, parse(bell, "off", &next).unwrap()).unwrap();
        assert!(!next.ui.bell);

        let window = find("model.context_window").unwrap();
        let next = with_value(&next, window, parse(window, "65536", &next).unwrap()).unwrap();
        assert_eq!(next.model.context_window, Some(65536));
        let next = with_value(&next, window, parse(window, "-", &next).unwrap()).unwrap();
        assert_eq!(next.model.context_window, None);

        let top_p = find("model.top_p").unwrap();
        let next = with_value(&next, top_p, parse(top_p, "0.9", &next).unwrap()).unwrap();
        assert!((next.model.top_p - 0.9).abs() < 1e-6);
        assert_eq!(display(top_p, current(&next, top_p).as_ref()), "0.9");

        let hosts = find("fetch.allow_private").unwrap();
        let next = with_value(&next, hosts, parse(hosts, "a.lan, b:8080", &next).unwrap()).unwrap();
        assert_eq!(next.fetch.allow_private, ["a.lan", "b:8080"]);

        let engine = find("model.engine").unwrap();
        let next = with_value(&next, engine, parse(engine, "vllm", &next).unwrap()).unwrap();
        assert_eq!(next.model.engine, "vllm");
        let next = with_value(&next, engine, parse(engine, "auto", &next).unwrap()).unwrap();
        assert_eq!(next.model.engine, "");
        assert_eq!(display(engine, current(&next, engine).as_ref()), "auto");
    }

    #[test]
    fn bad_values_are_refused_with_the_accepted_ones() {
        let config = Config::builtin();
        let review = find("review").unwrap();
        let error = parse(review, "sometimes", &config).unwrap_err().to_string();
        assert!(error.contains("off, auto"), "{error}");
        let calls = find("budgets.max_tool_calls").unwrap();
        assert!(parse(calls, "0", &config).is_err());
        assert_eq!(
            parse(calls, "-", &config).unwrap(),
            None,
            "restores the default"
        );
        let profile = find("default_profile").unwrap();
        assert!(parse(profile, "nope", &config).is_err());
        assert!(parse(profile, "full", &config).is_ok());
        let hooks = find("hooks.max_output_bytes").unwrap();
        assert!(parse(hooks, "100", &config).is_err());
    }

    #[test]
    fn secrets_are_never_displayed() {
        let mut config = Config::builtin();
        config.model.api_key = "sk-very-secret".into();
        let key = find("model.api_key").unwrap();
        let shown = display(key, current(&config, key).as_ref());
        assert_eq!(shown, "(set)");
    }

    #[test]
    fn save_key_writes_and_removes_nested_keys() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[ui]\ntheme = \"light\"\n").unwrap();
        Config::save_key(
            &path,
            "review",
            Some(toml::Value::String("off".into())),
            false,
        )
        .unwrap();
        Config::save_key(&path, "ui.bell", Some(toml::Value::Boolean(false)), false).unwrap();
        Config::save_key(
            &path,
            "model.context_window",
            Some(toml::Value::Integer(8192)),
            false,
        )
        .unwrap();
        let raw: toml::Value = toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(raw["review"].as_str(), Some("off"));
        assert_eq!(raw["ui"]["bell"].as_bool(), Some(false));
        assert_eq!(raw["ui"]["theme"].as_str(), Some("light"));
        assert!(Config::file_sets_key(&path, "model.context_window"));
        Config::save_key(&path, "model.context_window", None, false).unwrap();
        assert!(!Config::file_sets_key(&path, "model.context_window"));
        // Removing a key under a missing section is a no-op, not a new table.
        Config::save_key(&path, "vision.model", None, false).unwrap();
        assert!(!Config::file_sets_key(&path, "vision"));
        let cfg = Config::load(&path, None).unwrap();
        assert_eq!(cfg.review, crate::config::ReviewMode::Off);
        assert!(!cfg.ui.bell);
    }
}
