//! The `/settings` screen: category tabs across the top, the tab's sections
//! on the left, every key with its value on the right grouped under section
//! headings, what the highlighted key does below, and the keys that drive it
//! at the bottom. Typing searches every tab at once.

use std::cell::Cell;

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Margin, Rect};
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block as WidgetBlock, BorderType, Borders, Clear, Paragraph, Scrollbar, ScrollbarOrientation,
    ScrollbarState,
};
use unicode_width::UnicodeWidthStr;

use crate::settings::Setting;
use crate::ui::theme::{Role, Theme};
use crate::ui::widgets::{PickAction, clip_end};

/// Tabs in display order.
pub const TABS: &[&str] = &[
    "Behavior",
    "Model",
    "Appearance",
    "Budgets",
    "Prompt",
    "Tools",
    "Session",
    "Files",
];

/// One key in the list.
#[derive(Debug, Clone)]
pub struct SettingsRow {
    pub tab: &'static str,
    pub section: &'static str,
    pub key: String,
    pub label: String,
    pub value: String,
    /// Why the saved value might not be what this session uses.
    pub notes: Vec<String>,
    pub about: String,
    pub action: PickAction,
}

/// Where a key is shown: its tab, its section, and its label there.
pub fn placement(setting: &Setting) -> (&'static str, &'static str, String) {
    let key = setting.key;
    let (section, rest) = key.split_once('.').unwrap_or(("general", key));
    let words = humanize(rest);
    match section {
        "general" => match key {
            "default_profile" => ("Behavior", "General", "Default tool profile".into()),
            "review" => ("Behavior", "General", "Hunk review".into()),
            _ => ("Behavior", "General", words),
        },
        "permissions" => ("Behavior", "Permissions", words),
        "gates" => ("Behavior", "Gates", words),
        "format" => ("Behavior", "Editing", format!("Format {}", lower(&words))),
        "lsp" => ("Behavior", "Editing", format!("LSP {}", lower(&words))),
        "git" => ("Behavior", "Git", words),
        "ui" => ("Appearance", "Interface", words),
        "model" => {
            let sub = match rest {
                "engine" | "model" | "endpoint" | "api_key" | "start_command"
                | "response_schema" => "Endpoint",
                "context_window" | "max_output_tokens" => "Window",
                _ if rest.contains("timeout") || rest.starts_with("retr") => "Network",
                _ => "Sampling",
            };
            ("Model", sub, words)
        }
        "budgets" => ("Budgets", "Limits", words),
        "prompt" => ("Prompt", "Prompt", words),
        "skills" => ("Prompt", "Skills", format!("Skills {}", lower(&words))),
        "vision" => ("Tools", "Vision", words),
        "fetch" => ("Tools", "Fetch", words),
        "hooks" => ("Tools", "Hooks", words),
        "session" => ("Session", "Session", words),
        _ => ("Behavior", "Other", humanize(key)),
    }
}

/// `show_reasoning` → `Show reasoning`, with a few known acronyms.
fn humanize(raw: &str) -> String {
    let words: Vec<String> = raw
        .split(['_', '.'])
        .filter(|word| !word.is_empty())
        .map(|word| match word {
            "api" | "lsp" | "pr" | "url" | "exif" | "ms" | "id" | "xtc" | "dry" => {
                word.to_uppercase()
            }
            "secs" => "(s)".into(),
            "p" => "P".into(),
            other => other.to_string(),
        })
        .collect();
    let mut text = words.join(" ");
    if let Some(first) = text.get(0..1) {
        text = first.to_uppercase() + &text[1..];
    }
    text
}

fn lower(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) if !text.starts_with("LSP") && !text.starts_with("API") => {
            first.to_lowercase().collect::<String>() + chars.as_str()
        }
        _ => text.to_string(),
    }
}

/// The `/settings` screen's state.
#[derive(Debug, Clone, Default)]
pub struct SettingsModal {
    pub rows: Vec<SettingsRow>,
    pub tab: usize,
    /// Index into [`SettingsModal::visible`].
    pub selected: usize,
    pub filter: String,
    /// The global config file, shown in the Files tab.
    pub config_path: String,
    /// The last render found the terminal too small; keys other than Esc are
    /// ignored so they cannot act on an invisible screen.
    pub too_small: Cell<bool>,
}

impl SettingsModal {
    pub fn new(mut rows: Vec<SettingsRow>, config_path: String) -> Self {
        // Group each tab's rows by section, sections in the order they first
        // appear, rows in registry order within them.
        let mut order: Vec<(&'static str, &'static str)> = Vec::new();
        for row in &rows {
            if !order.contains(&(row.tab, row.section)) {
                order.push((row.tab, row.section));
            }
        }
        rows.sort_by_key(|row| {
            let tab = TABS
                .iter()
                .position(|tab| *tab == row.tab)
                .unwrap_or(TABS.len());
            let section = order
                .iter()
                .position(|entry| *entry == (row.tab, row.section))
                .unwrap_or(usize::MAX);
            (tab, section)
        });
        Self {
            rows,
            config_path,
            ..Self::default()
        }
    }

    /// Rows on the current tab, or every match while searching.
    pub fn visible(&self) -> Vec<&SettingsRow> {
        let needle = self.filter.to_lowercase();
        if needle.is_empty() {
            let tab = TABS.get(self.tab).copied().unwrap_or("");
            return self.rows.iter().filter(|row| row.tab == tab).collect();
        }
        self.rows
            .iter()
            .filter(|row| {
                [&row.key, &row.label, &row.about, &row.value]
                    .iter()
                    .any(|field| field.to_lowercase().contains(&needle))
            })
            .collect()
    }

    /// Sections of the visible rows, in order.
    pub fn sections(&self) -> Vec<&'static str> {
        let mut sections: Vec<&'static str> = Vec::new();
        for row in self.visible() {
            if !sections.contains(&row.section) {
                sections.push(row.section);
            }
        }
        sections
    }

    pub fn selected_row(&self) -> Option<&SettingsRow> {
        self.visible().get(self.selected).copied()
    }

    pub fn move_by(&mut self, delta: isize) {
        let count = self.visible().len();
        if count == 0 {
            self.selected = 0;
            return;
        }
        self.selected = (self.selected as isize + delta).clamp(0, count as isize - 1) as usize;
    }

    /// Left/Right: the previous or next tab (wrapping). Ignored while
    /// searching, which spans every tab.
    pub fn switch_tab(&mut self, delta: isize) {
        if !self.filter.is_empty() {
            return;
        }
        let count = TABS.len() as isize;
        self.tab = (self.tab as isize + delta).rem_euclid(count) as usize;
        self.selected = 0;
    }

    /// Tab/Shift+Tab: the first row of the next or previous section.
    pub fn jump_section(&mut self, delta: isize) {
        let visible = self.visible();
        let Some(current) = visible.get(self.selected).map(|row| row.section) else {
            return;
        };
        let sections = self.sections();
        let index = sections
            .iter()
            .position(|section| *section == current)
            .unwrap_or(0) as isize;
        let target = sections[(index + delta).rem_euclid(sections.len() as isize) as usize];
        if let Some(position) = visible.iter().position(|row| row.section == target) {
            self.selected = position;
        }
    }

    pub fn push_filter(&mut self, ch: char) {
        self.filter.push(ch);
        self.selected = 0;
    }

    pub fn pop_filter(&mut self) {
        self.filter.pop();
        self.selected = 0;
    }

    /// Show the tab holding `key` with it selected.
    pub fn select_key(&mut self, key: &str) {
        let Some(row) = self.rows.iter().find(|row| row.key == key) else {
            return;
        };
        self.filter.clear();
        self.tab = TABS.iter().position(|tab| *tab == row.tab).unwrap_or(0);
        self.selected = self
            .visible()
            .iter()
            .position(|row| row.key == key)
            .unwrap_or(0);
    }
}

/// Draw the screen over most of `area`.
pub fn render(frame: &mut Frame, view: &SettingsModal, area: Rect, theme: &Theme) {
    let rect = area;
    if rect.width < 30 || rect.height < 10 {
        // Never leave an invisible modal capturing keys. Say why, and let
        // the key handler ignore everything but Esc.
        view.too_small.set(true);
        frame.render_widget(Clear, rect);
        let message = "Settings needs at least 30×10; enlarge the terminal (Esc closes)";
        frame.render_widget(
            Paragraph::new(clip_end(message, rect.width as usize))
                .style(theme.style(Role::Warning)),
            rect,
        );
        return;
    }
    view.too_small.set(false);
    frame.render_widget(Clear, rect);
    let block = WidgetBlock::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(theme.style(Role::AccentDim))
        .title(Span::styled(" Settings ", theme.bold(Role::Accent)));
    let inner = block.inner(rect);
    frame.render_widget(block, rect);

    let [tabs, rule, body, about_rule, about, keys] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(3),
        Constraint::Length(1),
        Constraint::Length(2),
        Constraint::Length(1),
    ])
    .areas(inner);

    frame.render_widget(
        Paragraph::new(tabs_line(view, tabs.width as usize, theme)),
        tabs,
    );
    let rule_line = || Line::styled("─".repeat(inner.width as usize), theme.style(Role::Border));
    frame.render_widget(Paragraph::new(rule_line()), rule);
    frame.render_widget(Paragraph::new(rule_line()), about_rule);

    let visible = view.visible();
    let selected = visible.get(view.selected).copied();
    let sections = view.sections();
    let current_section = selected.map(|row| row.section);

    // Left: the sections, the one holding the selection highlighted.
    let left_width = sections
        .iter()
        .map(|section| UnicodeWidthStr::width(*section))
        .max()
        .unwrap_or(8)
        .clamp(8, 20) as u16
        + 4;
    let [left, divider, right] = Layout::horizontal([
        Constraint::Length(left_width),
        Constraint::Length(1),
        Constraint::Min(10),
    ])
    .areas(body);
    let left_lines: Vec<Line> = sections
        .iter()
        .map(|section| {
            if Some(*section) == current_section {
                Line::from(Span::styled(
                    format!("  {section}"),
                    theme.bold(Role::Accent),
                ))
            } else {
                Line::from(Span::styled(format!("  {section}"), theme.style(Role::Fg)))
            }
        })
        .collect();
    frame.render_widget(Paragraph::new(left_lines), left);
    let bar: Vec<Line> = (0..divider.height)
        .map(|_| Line::styled("│", theme.style(Role::Border)))
        .collect();
    frame.render_widget(Paragraph::new(bar), divider);

    // Right: section headings and rows.
    let label_width = visible
        .iter()
        .map(|row| UnicodeWidthStr::width(row.label.as_str()))
        .max()
        .unwrap_or(10)
        .clamp(10, 36);
    let room = right.width.saturating_sub(2) as usize;
    let mut lines: Vec<Line> = Vec::new();
    let mut selected_line = 0usize;
    let mut previous: Option<&str> = None;
    for (index, row) in visible.iter().enumerate() {
        if previous != Some(row.section) {
            let heading = if Some(row.section) == current_section {
                theme.bold(Role::Fg).add_modifier(Modifier::UNDERLINED)
            } else {
                theme.style(Role::Muted).add_modifier(Modifier::UNDERLINED)
            };
            lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(row.section.to_string(), heading),
            ]));
            previous = Some(row.section);
        }
        let is_selected = index == view.selected;
        if is_selected {
            selected_line = lines.len();
        }
        let in_section = Some(row.section) == current_section;
        let (label_style, value_style) = if is_selected {
            (theme.bold(Role::Accent), theme.bold(Role::Accent))
        } else if in_section {
            (theme.style(Role::Fg), theme.style(Role::Fg))
        } else {
            (theme.style(Role::Muted), theme.style(Role::Muted))
        };
        let label = clip_end(&row.label, label_width);
        let pad = label_width.saturating_sub(UnicodeWidthStr::width(label.as_str())) + 3;
        let mut spans = vec![
            Span::styled(
                if is_selected { "› " } else { "  " },
                theme.bold(Role::Accent),
            ),
            Span::styled(label, label_style),
            Span::raw(" ".repeat(pad)),
        ];
        let value_room = room.saturating_sub(label_width + 5);
        if value_room > 3 {
            spans.push(Span::styled(clip_end(&row.value, value_room), value_style));
            if !row.notes.is_empty() {
                let used = UnicodeWidthStr::width(row.value.as_str()).min(value_room);
                let notes = format!("  [{}]", row.notes.join(" · "));
                if value_room > used + 6 {
                    spans.push(Span::styled(
                        clip_end(&notes, value_room - used),
                        theme.style(Role::Warning),
                    ));
                }
            }
        }
        lines.push(Line::from(spans));
    }
    if lines.is_empty() {
        lines.push(Line::styled(
            "  nothing matches · Backspace to widen the search",
            theme.style(Role::Muted),
        ));
    }
    let height = right.height as usize;
    let total = lines.len();
    // Keep the selection a few rows above the bottom edge.
    let scroll = selected_line
        .saturating_sub(height.saturating_sub(4))
        .min(total.saturating_sub(height));
    frame.render_widget(
        Paragraph::new(lines).scroll((scroll as u16, 0)),
        right.inner(Margin::new(1, 0)),
    );
    if total > height {
        let mut state = ScrollbarState::new(total.saturating_sub(height)).position(scroll);
        frame.render_stateful_widget(
            Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .begin_symbol(None)
                .end_symbol(None)
                .track_symbol(Some("│"))
                .thumb_symbol("┃")
                .style(theme.style(Role::Accent)),
            right,
            &mut state,
        );
    }

    // What the highlighted key does.
    let about_lines = match selected {
        Some(row) => {
            let mut first = vec![Span::styled(
                format!("  {}", row.about),
                theme.style(Role::Fg),
            )];
            if !row.notes.is_empty() {
                first.push(Span::styled(
                    format!("  ({})", row.notes.join(" · ")),
                    theme.style(Role::Warning),
                ));
            }
            vec![
                Line::from(first),
                Line::styled(
                    if row.key.starts_with('/') {
                        format!("  {}", row.key)
                    } else {
                        format!("  {} · /settings {} <value>", row.key, row.key)
                    },
                    theme.style(Role::Muted),
                ),
            ]
        }
        None => Vec::new(),
    };
    frame.render_widget(Paragraph::new(about_lines), about);

    let hint = if view.filter.is_empty() {
        "  ⏎ change · ⇥ next section · ←/→ tabs · ↑↓ move · type to search · esc close"
    } else {
        "  ⏎ change · ⇥ next section · ↑↓ move · backspace widen · esc clear search"
    };
    frame.render_widget(
        Paragraph::new(Line::styled(
            clip_end(hint, keys.width as usize),
            theme.style(Role::Muted),
        )),
        keys,
    );
}

/// The tab strip, or the search when one is typed.
fn tabs_line(view: &SettingsModal, width: usize, theme: &Theme) -> Line<'static> {
    if !view.filter.is_empty() {
        let count = view.visible().len();
        return Line::from(vec![
            Span::styled(
                " search ",
                theme.bold(Role::Bg).bg(theme.color(Role::Accent)),
            ),
            Span::styled(format!(" {}▏", view.filter), theme.bold(Role::Fg)),
            Span::styled(
                format!("  {count} match(es) in every tab"),
                theme.style(Role::Muted),
            ),
        ]);
    }
    let mut spans = Vec::new();
    let mut used = 0usize;
    let mut active_shown = false;
    for (index, tab) in TABS.iter().enumerate() {
        let label = format!(" {tab} ");
        let label_width = UnicodeWidthStr::width(label.as_str()) + 1;
        if used + label_width > width {
            if !active_shown && view.tab >= index {
                // The strip does not fit before the active tab; keep the
                // active tab visible instead of ending on an ellipsis.
                let active = TABS.get(view.tab).copied().unwrap_or("");
                spans.clear();
                spans.push(Span::styled("… ", theme.style(Role::Muted)));
                spans.push(Span::styled(
                    format!(" {active} "),
                    theme.bold(Role::Bg).bg(theme.color(Role::Accent)),
                ));
            } else {
                spans.push(Span::styled("…", theme.style(Role::Muted)));
            }
            break;
        }
        used += label_width;
        if index == view.tab {
            active_shown = true;
            spans.push(Span::styled(
                label,
                theme.bold(Role::Bg).bg(theme.color(Role::Accent)),
            ));
        } else {
            spans.push(Span::styled(label, theme.style(Role::Fg)));
        }
        spans.push(Span::raw(" "));
    }
    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(tab: &'static str, section: &'static str, key: &str) -> SettingsRow {
        SettingsRow {
            tab,
            section,
            key: key.into(),
            label: humanize(key),
            value: "v".into(),
            notes: Vec::new(),
            about: format!("about {key}"),
            action: PickAction::Run(format!("/settings {key}")),
        }
    }

    #[test]
    fn the_active_tab_stays_visible_on_a_narrow_strip() {
        let theme = Theme::bundled("default");
        let view = SettingsModal {
            tab: TABS.len() - 1,
            ..SettingsModal::default()
        };
        let line = tabs_line(&view, 40, &theme);
        let text: String = line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
        assert!(text.contains("Files"), "{text}");
    }

    #[test]
    fn a_tiny_terminal_marks_the_view_too_small() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let theme = Theme::bundled("default");
        let view = SettingsModal::default();
        let mut terminal = Terminal::new(TestBackend::new(20, 5)).unwrap();
        terminal
            .draw(|frame| render(frame, &view, frame.area(), &theme))
            .unwrap();
        assert!(view.too_small.get());
    }

    #[test]
    fn every_setting_lands_on_a_known_tab_with_a_readable_label() {
        for setting in crate::settings::SETTINGS {
            let (tab, section, label) = placement(setting);
            assert!(TABS.contains(&tab), "{} -> {tab}", setting.key);
            assert_ne!(section, "Other", "{} has no section", setting.key);
            assert!(!label.contains('_'), "{label}");
        }
        assert_eq!(humanize("show_reasoning"), "Show reasoning");
        assert_eq!(humanize("api_key"), "API key");
    }

    #[test]
    fn tabs_sections_and_search_move_the_selection() {
        let mut view = SettingsModal::new(
            vec![
                row("Behavior", "General", "review"),
                row("Behavior", "Gates", "gates.a"),
                row("Behavior", "Gates", "gates.b"),
                row("Model", "Endpoint", "model.engine"),
            ],
            String::new(),
        );
        assert_eq!(view.visible().len(), 3);
        assert_eq!(view.sections(), vec!["General", "Gates"]);
        view.jump_section(1);
        assert_eq!(view.selected_row().unwrap().key, "gates.a");
        view.jump_section(1);
        assert_eq!(view.selected_row().unwrap().key, "review");
        view.switch_tab(1);
        assert_eq!(view.selected_row().unwrap().key, "model.engine");
        view.switch_tab(-1);
        for ch in "gates.b".chars() {
            view.push_filter(ch);
        }
        assert_eq!(view.visible().len(), 1);
        view.filter.clear();
        view.select_key("model.engine");
        assert_eq!(TABS[view.tab], "Model");
        assert_eq!(view.selected_row().unwrap().key, "model.engine");
    }
}
