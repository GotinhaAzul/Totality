//! `/settings`: every configurable key in one picker, where each lives, and
//! changes saved to the global config and applied to the running session.

use super::*;
use crate::settings::{self, Apply, Kind, Setting};

impl App {
    /// `/settings` lists everything; `/settings where` shows the files and
    /// collections; `/settings <key>` shows or picks a value; `/settings
    /// <key> <value>` sets it (`-` restores the default).
    pub(super) async fn command_settings(&mut self, rest: &str) {
        // Set by the picker just before it ran this command line, so a change
        // made from the list returns to the list.
        let from_picker = std::mem::take(&mut self.settings_return);
        let rest = rest.trim();
        if rest.is_empty() {
            self.open_settings_list("");
            return;
        }
        if matches!(rest, "where" | "paths" | "files") {
            self.settings_where();
            return;
        }
        let (key, value) = match rest.find(char::is_whitespace) {
            Some(position) => (&rest[..position], rest[position..].trim()),
            None => (rest, ""),
        };
        let Some(setting) = settings::find(key) else {
            let close: Vec<&str> = settings::SETTINGS
                .iter()
                .map(|setting| setting.key)
                .filter(|candidate| candidate.contains(key))
                .take(6)
                .collect();
            let hint = if close.is_empty() {
                "run /settings to see them all".to_string()
            } else {
                format!("did you mean {}?", close.join(", "))
            };
            self.set_notice(format!("unknown setting {key:?}; {hint}"), Role::Error);
            return;
        };
        // Keys with a richer command of their own go through it.
        match (setting.key, value) {
            ("ui.theme", value) => return self.command_theme(value),
            ("model.model", value) => return self.command_model(value).await,
            ("default_profile", value) => return self.command_profile(value).await,
            _ => {}
        }
        if value.is_empty() {
            self.settings_show(setting);
            return;
        }
        self.settings_set(setting, value).await;
        if from_picker {
            self.open_settings_list(setting.key);
        }
    }

    /// The settings list, preselecting `current` when given.
    fn open_settings_list(&mut self, current: &str) {
        let mut items = vec![PickItem {
            label: "where".into(),
            description: "config files, profiles, MCP, hooks, rules, verify steps".into(),
            action: PickAction::Run("/settings where".into()),
        }];
        for setting in settings::SETTINGS {
            let value = settings::current(&self.config, setting);
            let shown = settings::display(setting, value.as_ref());
            // Notes go before the description so a narrow screen keeps them.
            let mut description = shown.clone();
            for note in self.setting_notes(setting) {
                description.push_str(&format!(" [{note}]"));
            }
            description.push_str(&format!("  · {}", setting.about));
            let action = match setting.kind {
                Kind::Bool => {
                    let on = value.as_ref().and_then(toml::Value::as_bool) == Some(true);
                    PickAction::Run(format!("/settings {} {}", setting.key, !on))
                }
                Kind::Choice(_) | Kind::Profile | Kind::Theme => {
                    PickAction::Run(format!("/settings {}", setting.key))
                }
                _ if setting.key == "model.model" => PickAction::Run("/model".into()),
                Kind::Secret => PickAction::Fill(format!("/settings {} ", setting.key)),
                _ => {
                    // Prefill the current value so it can be edited in place.
                    let prefill = match &value {
                        Some(toml::Value::String(text)) => text.clone(),
                        Some(toml::Value::Array(items)) if items.is_empty() => String::new(),
                        Some(_) => shown,
                        None => String::new(),
                    };
                    PickAction::Fill(format!("/settings {} {prefill}", setting.key))
                }
            };
            items.push(PickItem {
                label: setting.key.to_string(),
                description,
                action,
            });
        }
        let subtitle = self
            .core()
            .map(|core| core.paths.config_file().display().to_string())
            .unwrap_or_default();
        let picker = PickerModal::new(
            "settings",
            "Settings · Enter toggles or edits",
            items,
            current,
        )
        .with_subtitle(subtitle);
        self.open_picker(picker, "no settings");
    }

    /// Why a saved value might not be what this session uses.
    fn setting_notes(&self, setting: &Setting) -> Vec<String> {
        let mut notes = Vec::new();
        if setting.apply == Apply::Restart {
            notes.push("next launch".to_string());
        }
        if let Some(name) = setting.env
            && crate::paths::env_var(name).is_ok()
        {
            notes.push(format!("TOT_{name} overrides"));
        }
        if let Some(project) = self.trusted_project_config()
            && Config::file_sets_key(&project, setting.key)
        {
            notes.push("project config overrides".to_string());
        }
        notes
    }

    fn trusted_project_config(&self) -> Option<PathBuf> {
        let core = self.core()?;
        (self.config.trust || self.options.trust)
            .then(|| core.paths.project_dir().join("config.toml"))
    }

    /// `/settings <key>`: a value picker for fixed choices, otherwise the
    /// value, what it does, and how to change it.
    fn settings_show(&mut self, setting: &'static Setting) {
        let value = settings::current(&self.config, setting);
        let shown = settings::display(setting, value.as_ref());
        let choices = settings::choices(setting, &self.config);
        if !choices.is_empty() {
            let current = shown;
            let items = choices
                .into_iter()
                .map(|choice| PickItem {
                    action: PickAction::Run(format!("/settings {} {choice}", setting.key)),
                    description: if choice == current {
                        "current".into()
                    } else {
                        String::new()
                    },
                    label: choice,
                })
                .collect();
            let picker = PickerModal::new(
                "settings-value",
                format!("{} · {}", setting.key, setting.about),
                items,
                current,
            );
            self.open_picker(picker, "no choices");
            return;
        }
        let mut text = format!(
            "{} = {shown}\n{}\ntakes effect: {}",
            setting.key,
            setting.about,
            setting.apply.label()
        );
        for note in self.setting_notes(setting) {
            text.push_str(&format!("\nnote: {note}"));
        }
        let unset = matches!(setting.kind, Kind::OptInt { .. } | Kind::OptFloat { .. });
        text.push_str(&format!(
            "\nchange: /settings {} <value>   (- restores the default{})",
            setting.key,
            if unset { "; auto = unset" } else { "" }
        ));
        self.transcript
            .push_note(format!("settings · {}", setting.key), text, false);
        self.transcript.scroll_end();
    }

    /// Validate, apply to the running session, then save to the global
    /// config. Nothing is saved when the value cannot be applied.
    async fn settings_set(&mut self, setting: &'static Setting, raw: &str) {
        let value = match settings::parse(setting, raw, &self.config) {
            Ok(value) => value,
            Err(error) => {
                self.set_notice(error.to_string(), Role::Error);
                return;
            }
        };
        let next = match settings::with_value(&self.config, setting, value.clone()) {
            Ok(next) => next,
            Err(error) => {
                self.set_notice(format!("{}: {error:#}", setting.key), Role::Error);
                return;
            }
        };
        match setting.apply {
            Apply::Ui => {
                self.config = next;
                match setting.key {
                    "ui.show_reasoning" => {
                        self.transcript
                            .set_show_reasoning(self.config.ui.show_reasoning);
                    }
                    "ui.color_mode" => {
                        let name = self.theme.name.clone();
                        self.apply_theme_named(&name);
                    }
                    _ => {}
                }
            }
            Apply::Restart => self.config = next,
            Apply::Rebuild => {
                if let Err(error) = self.rebuild_core_with(next).await {
                    self.set_notice(
                        format!("{}: not applied, not saved: {error:#}", setting.key),
                        Role::Error,
                    );
                    return;
                }
            }
            Apply::Permission => {
                let mode = next.permissions.mode;
                self.config = next;
                if mode != crate::policy::PermissionMode::Plan {
                    self.set_permission_mode(mode).await;
                }
            }
        }
        let Some(path) = self.core().map(|core| core.paths.config_file()) else {
            return;
        };
        let applied = settings::display(setting, settings::current(&self.config, setting).as_ref());
        match Config::save_key(&path, setting.key, value, setting.is_secret()) {
            Ok(()) => {
                let mut notice = format!("{} = {applied} (saved", setting.key);
                if setting.apply == Apply::Restart {
                    notice.push_str("; takes effect next launch");
                }
                notice.push(')');
                let notes: Vec<String> = self
                    .setting_notes(setting)
                    .into_iter()
                    .filter(|note| note != "next launch")
                    .collect();
                let role = if notes.is_empty() {
                    Role::Success
                } else {
                    notice.push_str(&format!("; {}", notes.join("; ")));
                    Role::Warning
                };
                self.set_notice(notice, role);
            }
            Err(error) => self.set_notice(
                format!(
                    "{} = {applied} for this session; not saved: {error:#}",
                    setting.key
                ),
                Role::Warning,
            ),
        }
    }

    /// `/settings where`: the files behind the settings and the collections
    /// that are edited there or through their own commands.
    fn settings_where(&mut self) {
        let Some(core) = self.core().cloned() else {
            return;
        };
        let paths = &core.paths;
        let trusted = self.config.trust || self.options.trust;
        let project = paths.project_dir();
        let exists = |path: &Path| if path.exists() { "" } else { "  (not created)" };
        let config_file = paths.config_file();
        let project_config = project.join("config.toml");
        let mut lines = vec![
            "Files".to_string(),
            format!(
                "  global config    {}{}",
                config_file.display(),
                exists(&config_file)
            ),
            format!(
                "  project config   {}{}{}",
                project_config.display(),
                exists(&project_config),
                if trusted {
                    ""
                } else {
                    "  (ignored: untrusted, see /trust)"
                }
            ),
            format!(
                "  profiles         {}",
                paths.global_profiles_dir().display()
            ),
            format!("  themes           {}", paths.themes_dir().display()),
            format!("  skills           {}", paths.skills_dir().display()),
            format!("  workflows        {}", paths.workflows_dir().display()),
            format!("  tools            {}", paths.tools_dir().display()),
            format!(
                "  sessions         {}",
                paths.project_sessions_dir().display()
            ),
            format!("  log              {}", paths.log_file().display()),
            String::new(),
            "Collections (edit in the config file)".to_string(),
        ];
        let names = |keys: Vec<&String>| -> String {
            if keys.is_empty() {
                "none".into()
            } else {
                keys.into_iter()
                    .map(String::as_str)
                    .collect::<Vec<_>>()
                    .join(", ")
            }
        };
        lines.push(format!(
            "  [profile.*]        {}  · /profile switches",
            names(self.config.profiles.keys().collect())
        ));
        lines.push(format!(
            "  [model_profile.*]  {}  · /model <name> <profile>",
            names(self.config.model_profiles.keys().collect())
        ));
        lines.push(format!(
            "  [mcp.*]            {}  · /mcp shows status",
            names(self.config.mcp.keys().collect())
        ));
        lines.push(format!(
            "  [hooks.*]          {}",
            names(self.config.hooks.entries.keys().collect())
        ));
        lines.push(format!(
            "  [[permissions.rules]]  {} rule(s)  · /permissions why",
            self.config.permissions.rules.len()
        ));
        let steps: Vec<&str> = self
            .config
            .verify
            .steps
            .iter()
            .map(|step| step.name.as_str())
            .collect();
        lines.push(format!(
            "  verify steps       {}{}  · /verify",
            if steps.is_empty() {
                "none".to_string()
            } else {
                steps.join(", ")
            },
            if self.config.verify.is_detected() {
                " (detected)"
            } else {
                ""
            }
        ));
        lines.push(String::new());
        lines.push(
            "Precedence: CLI flags > TOT_* environment > trusted project config > global config > defaults.\n/settings writes the global config."
                .to_string(),
        );
        self.transcript
            .push_note("settings · where", lines.join("\n"), false);
        self.transcript.scroll_end();
    }
}
