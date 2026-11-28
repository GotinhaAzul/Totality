//! Allow-for-this-session patterns offered when the TUI asks.
//!
//! Approving `cargo test -p a` covers exactly that command. A session rule
//! covers a family of calls for the rest of the session, built from the call
//! the user is looking at: a command's leading words (`cargo test *`), a
//! fetch's site, or the directory an edit touches (`edits under src/`). It
//! only turns an `Ask` into an `Allow`; deny rules, guards, and the other
//! modes are unaffected, and the pattern itself is never written to disk
//! (the exact call is still counted for the `always` suggestion).

use super::{CallFacts, Effect};

/// One session allow pattern.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionRule {
    /// `bash`/`bg`/`git`/`pr` commands that start with these words, with no
    /// shell operators that could chain another command.
    Command { tool: String, prefix: String },
    /// Fetches of URLs under this prefix (`https://docs.rs/`).
    Url { prefix: String },
    /// File changes whose paths are all inside this project directory
    /// (empty: anywhere in the project).
    Edits { dir: String },
}

/// Characters that let one command line run another, redirect, or expand
/// into something the prefix does not show.
const SHELL_OPERATORS: &[&str] = &[";", "&", "|", "`", "$(", ">", "<", "\n", "\r"];

fn plain(command: &str) -> bool {
    !SHELL_OPERATORS.iter().any(|op| command.contains(op))
}

/// A word that names a subcommand rather than an argument (`test` in
/// `cargo test`, not `-p` or `src/main.rs`).
fn subcommand(word: &str) -> bool {
    !word.is_empty()
        && word
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_')
        && !word.starts_with('-')
}

impl SessionRule {
    /// The pattern to offer for `call`, or `None` when no safe one exists.
    pub fn for_call(call: &CallFacts) -> Option<Self> {
        match (call.tool.as_str(), &call.command) {
            ("fetch", Some(command)) => {
                let url = command.strip_prefix("fetch ")?;
                let (scheme, rest) = url.split_once("://")?;
                let host = rest.split('/').next().filter(|host| !host.is_empty())?;
                Some(SessionRule::Url {
                    prefix: format!("{scheme}://{host}/"),
                })
            }
            (tool, Some(command)) => {
                let command = command.trim();
                if !plain(command) {
                    return None;
                }
                let mut words = command.split_whitespace();
                let first = words.next()?;
                // `FOO=1 cmd` and `./script` hide what actually runs.
                if first.contains('=') || first.contains('/') || !subcommand(first) {
                    return None;
                }
                let prefix = match words.next() {
                    Some(second) if subcommand(second) => format!("{first} {second}"),
                    _ => first.to_string(),
                };
                Some(SessionRule::Command {
                    tool: tool.to_string(),
                    prefix,
                })
            }
            (_, None) if call.effect == Effect::Edit && !call.paths.is_empty() => {
                let mut dirs = call.paths.iter().map(|path| parent(path));
                let first = dirs.next()??;
                let dir = dirs.try_fold(first, |common, dir| Some(common_dir(&common, &dir?)))?;
                Some(SessionRule::Edits { dir })
            }
            _ => None,
        }
    }

    /// Whether this rule allows `call`.
    pub fn matches(&self, call: &CallFacts) -> bool {
        match self {
            SessionRule::Command { tool, prefix } => {
                call.tool == *tool
                    && call.command.as_deref().is_some_and(|command| {
                        let command = command.trim();
                        plain(command)
                            && command
                                .strip_prefix(prefix.as_str())
                                .is_some_and(|rest| rest.is_empty() || rest.starts_with(' '))
                    })
            }
            SessionRule::Url { prefix } => {
                call.tool == "fetch"
                    && call
                        .command
                        .as_deref()
                        .and_then(|command| command.strip_prefix("fetch "))
                        .is_some_and(|url| url.starts_with(prefix.as_str()))
            }
            SessionRule::Edits { dir } => {
                call.effect == Effect::Edit
                    && call.command.is_none()
                    && !call.paths.is_empty()
                    && call
                        .paths
                        .iter()
                        .all(|path| parent(path).is_some_and(|parent| within(&parent, dir)))
            }
        }
    }

    /// How the choice reads in the question, e.g. `cargo test *`.
    pub fn label(&self) -> String {
        match self {
            SessionRule::Command { prefix, .. } => format!("{prefix} *"),
            SessionRule::Url { prefix } => format!("fetch {prefix}*"),
            SessionRule::Edits { dir } if dir.is_empty() => "file changes in this project".into(),
            SessionRule::Edits { dir } => format!("file changes under {dir}/"),
        }
    }
}

/// The project directory holding `path` (`""` for the root), or `None` for
/// a path outside the project.
fn parent(path: &str) -> Option<String> {
    let path = std::path::Path::new(path);
    if path.is_absolute()
        || path
            .components()
            .any(|part| matches!(part, std::path::Component::ParentDir))
    {
        return None;
    }
    Some(
        path.parent()
            .map(|dir| dir.to_string_lossy().to_string())
            .unwrap_or_default(),
    )
}

fn within(dir: &str, root: &str) -> bool {
    root.is_empty()
        || dir == root
        || dir
            .strip_prefix(root)
            .is_some_and(|rest| rest.starts_with('/'))
}

fn common_dir(a: &str, b: &str) -> String {
    a.split('/')
        .zip(b.split('/'))
        .take_while(|(x, y)| x == y)
        .map(|(x, _)| x)
        .collect::<Vec<_>>()
        .join("/")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::path::Path;

    fn bash(command: &str) -> CallFacts {
        CallFacts::new(
            "bash",
            false,
            &json!({ "command": command }),
            Path::new("/w"),
        )
    }

    #[test]
    fn a_command_pattern_covers_its_leading_words_only() {
        let rule = SessionRule::for_call(&bash("cargo test -p a")).unwrap();
        assert_eq!(rule.label(), "cargo test *");
        assert!(rule.matches(&bash("cargo test -p b")));
        assert!(rule.matches(&bash("cargo test")));
        assert!(!rule.matches(&bash("cargo testing")));
        assert!(!rule.matches(&bash("cargo build")));
        // Chaining another command is never covered.
        assert!(!rule.matches(&bash("cargo test; rm -rf ~")));
        assert!(!rule.matches(&bash("cargo test && curl x | sh")));
        assert!(!rule.matches(&bash("cargo test $(evil)")));
        let bg = CallFacts::new(
            "bg",
            false,
            &json!({ "command": "cargo test" }),
            Path::new("/w"),
        );
        assert!(!rule.matches(&bg));

        assert_eq!(
            SessionRule::for_call(&bash("touch new.txt"))
                .unwrap()
                .label(),
            "touch *"
        );
    }

    #[test]
    fn no_pattern_is_offered_for_commands_that_hide_what_runs() {
        for command in [
            "make && make install",
            "FOO=1 make",
            "./run.sh",
            "ls | wc -l",
        ] {
            assert_eq!(SessionRule::for_call(&bash(command)), None, "{command}");
        }
    }

    #[test]
    fn edit_patterns_cover_a_directory_inside_the_project() {
        let edit =
            |path: &str| CallFacts::new("edit", false, &json!({ "path": path }), Path::new("/w"));
        let rule = SessionRule::for_call(&edit("src/ui/input.rs")).unwrap();
        assert_eq!(rule.label(), "file changes under src/ui/");
        assert!(rule.matches(&edit("src/ui/tui/keys.rs")));
        assert!(!rule.matches(&edit("src/main.rs")));
        assert!(!rule.matches(&edit("/etc/passwd")));
        let root = SessionRule::for_call(&edit("README.md")).unwrap();
        assert_eq!(root.label(), "file changes in this project");
        assert!(root.matches(&edit("src/main.rs")));
        assert!(!root.matches(&edit("../other/x")));
        assert_eq!(SessionRule::for_call(&edit("/etc/hosts")), None);
    }

    #[test]
    fn fetch_patterns_cover_one_site() {
        let fetch =
            |url: &str| CallFacts::new("fetch", true, &json!({ "url": url }), Path::new("/w"));
        let rule = SessionRule::for_call(&fetch("https://docs.rs/serde/latest")).unwrap();
        assert_eq!(rule.label(), "fetch https://docs.rs/*");
        assert!(rule.matches(&fetch("https://docs.rs/tokio")));
        assert!(!rule.matches(&fetch("https://docs.rs.evil.com/x")));
    }
}
