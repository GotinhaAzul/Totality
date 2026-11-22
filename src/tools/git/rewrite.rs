//! `git` action `rewrite_history`: remove paths from every branch and tag.
//!
//! History surgery stays out of `bash` ([`super::destructive_git`]); this is
//! the one sanctioned route, and its guardrails live here, in the action body,
//! where no prompt or repository file can skip them. Whether the action may
//! run at all is the permission policy's call (mode, rules), made before this
//! code is reached.
//!
//! 1. Refuse outside a repository root, in a linked worktree, on a detached
//!    HEAD, with a dirty tree, with linked worktrees, or when no branch or
//!    tag holds the paths.
//! 2. Write `git bundle create --all` under `<data-dir>/rewrites/`, outside
//!    the work tree, before anything changes. Commits only reflogs hold are
//!    packed too: the cleanup prunes them, and `--all` does not include them.
//! 3. Record every branch and tag: OID, object type, and tag metadata.
//! 4. Rewrite with `git filter-repo` when it is installed, otherwise
//!    `git filter-branch --index-filter`. Paths reach either one as data
//!    (argv, or a NUL-separated pathspec file), never as shell text.
//! 5. Verify: `git fsck`, the same refs with the same object types, the same
//!    per-commit metadata and parent counts, the same tag metadata, and none
//!    of the paths left in any branch or tag.
//! 6. On any failure, put every recorded ref back at its old OID and restore
//!    the work tree; the bundle stays. On success, drop the old history
//!    (`refs/original`, reflogs, unreachable objects) so the repository
//!    actually shrinks, and report old → new OIDs.

use std::collections::{BTreeMap, HashSet};
use std::ffi::OsStr;
use std::path::{Component, Path, PathBuf};

use serde_json::Value;

use super::{Run, enclosing_repo, git, git_line, quote, run, run_env, worktree_paths};
use crate::tools::{ToolContext, ToolResult};

/// Refs the action rewrites. Remote-tracking refs and the stash are left
/// alone: they are reported, and they keep the old objects reachable.
const SCOPE: &[&str] = &["refs/heads", "refs/tags"];

/// Field and record separators. `git log` spells them `%x1f`/`%x1e`;
/// `for-each-ref` spells them `%1f`/`%1e`.
const FIELD: &str = "%x1f";
const RECORD: char = '\u{1e}';

/// One branch or tag before the rewrite.
#[derive(Debug, Clone, PartialEq, Eq)]
struct RefState {
    oid: String,
    /// `commit` or `tag` (annotated), as `for-each-ref` reports it.
    kind: String,
    /// Tag object fields other than the target: tagger, date, message.
    tag_meta: String,
}

pub(super) fn rewrite_history(ctx: &ToolContext, args: &Value) -> ToolResult {
    let paths = match requested_paths(args) {
        Ok(paths) => paths,
        Err(message) => return ToolResult::err("invalid_args", message),
    };
    let cwd = ctx.cwd.as_path();
    if let Err(error) = preflight(cwd, &paths) {
        return error;
    }

    let before = record(cwd);
    if before.is_empty() {
        return ToolResult::err(
            "GIT_NOTHING_TO_REWRITE",
            "the repository has no branches or tags",
        );
    }
    let head_branch = git_line(cwd, &["symbolic-ref", "--quiet", "HEAD"]).unwrap_or_default();
    let size_before = pack_size(cwd);

    let rewrites = ctx.paths.data_dir.join("rewrites");
    // Commits only the reflogs hold would be pruned by the cleanup, and
    // `bundle create --all` does not include them; put them in the bundle.
    let tips = reflog_only_tips(cwd);
    let bundle = match backup(cwd, &rewrites, ctx.artifacts.session_id(), &tips) {
        Ok(bundle) => bundle,
        Err(error) => return error,
    };

    let engine = if git(cwd, &["filter-repo", "--version"]).ok {
        Engine::FilterRepo
    } else {
        Engine::FilterBranch
    };
    let refnames: Vec<&str> = before.keys().map(String::as_str).collect();
    let rewrite = match engine {
        Engine::FilterRepo => filter_repo(cwd, &paths, &refnames),
        Engine::FilterBranch => filter_branch(cwd, &rewrites, &paths, &refnames),
    };
    let checked = match rewrite {
        Ok(()) => verify(cwd, &before, &paths),
        Err(error) => Err(error),
    };
    if let Err(problem) = checked {
        let restored = rollback(cwd, &before, &head_branch);
        return ToolResult::err(
            "GIT_REWRITE_FAILED",
            format!(
                "the rewrite with {} did not hold, so nothing was kept: {problem}\n{restored}\nThe backup bundle is {}.",
                engine.label(),
                bundle.display()
            ),
        );
    }

    let cleanup = drop_old_history(cwd);
    let after = record(cwd);
    let mut report = format!(
        "rewrote {} ref(s) with {}, removing {} from history.\nbackup: {}\nverified: fsck clean; every commit kept its author, committer, dates, message and parents; tags kept their type and message; the paths are gone from every branch and tag.\n\nold -> new:\n",
        before.len(),
        engine.label(),
        paths.join(", "),
        bundle.display()
    );
    for (name, old) in &before {
        let new = after
            .get(name)
            .map(|state| state.oid.as_str())
            .unwrap_or("?");
        let mark = if new == old.oid { " (unchanged)" } else { "" };
        report.push_str(&format!("  {name}: {} -> {new}{mark}\n", old.oid));
    }
    report.push_str(&format!(
        "\npack size: {} -> {}\n",
        size_before,
        pack_size(cwd)
    ));
    report.push_str(&cleanup);
    if !tips.is_empty() {
        report.push_str(&format!(
            "kept {} reflog-only commit(s) in the bundle, which gc would have dropped: {}\nto restore one: `git bundle unbundle {}` and then `git branch <name> <oid>`.\n",
            tips.len(),
            tips.join(", "),
            bundle.display()
        ));
    }
    let left = outside_scope(cwd);
    if !left.is_empty() {
        report.push_str(&format!(
            "\nnot rewritten, and still holding the old history: {}. Delete or refresh them (for a remote, push the rewritten branches with the user's agreement) before the old objects can go.\n",
            left.join(", ")
        ));
    }
    report.push_str(
        "\nCollaborators must re-clone or reset onto the new history; nothing was pushed.",
    );
    ToolResult::ok(report)
}

#[derive(Debug, Clone, Copy)]
enum Engine {
    FilterRepo,
    FilterBranch,
}

impl Engine {
    fn label(self) -> &'static str {
        match self {
            Engine::FilterRepo => "git filter-repo",
            Engine::FilterBranch => "git filter-branch",
        }
    }
}

/// The paths to remove, normalized and relative to the repository root.
fn requested_paths(args: &Value) -> Result<Vec<String>, String> {
    let usage = "rewrite_history needs paths: the files or directories to remove from every branch and tag, relative to the repository root, for example {\"action\":\"rewrite_history\",\"paths\":[\"assets/demo.mp4\"]}";
    let Some(list) = args.get("paths").and_then(Value::as_array) else {
        return Err(usage.into());
    };
    let mut paths = Vec::new();
    for raw in list {
        let Some(raw) = raw.as_str() else {
            return Err(usage.into());
        };
        if raw.contains(['\0', '\n', '\r']) {
            return Err(format!("{raw:?} contains a control character"));
        }
        let path = Path::new(raw);
        if path.is_absolute() {
            return Err(format!(
                "{raw:?} is absolute; give it relative to the repository root"
            ));
        }
        let mut parts = Vec::new();
        for component in path.components() {
            match component {
                Component::Normal(part) => parts.push(part.to_string_lossy().to_string()),
                Component::CurDir => {}
                _ => return Err(format!("{raw:?} leaves the repository root")),
            }
        }
        if parts.is_empty() {
            return Err(format!("{raw:?} names the whole repository"));
        }
        if parts[0] == ".git" {
            return Err(format!("{raw:?} is inside .git"));
        }
        let normalized = parts.join("/");
        if !paths.contains(&normalized) {
            paths.push(normalized);
        }
    }
    if paths.is_empty() {
        return Err(usage.into());
    }
    Ok(paths)
}

/// A literal pathspec: no globbing, no magic, whatever the name contains.
fn literal(path: &str) -> String {
    format!(":(literal){path}")
}

/// Every refusal that needs no change to the repository.
fn preflight(cwd: &Path, paths: &[String]) -> Result<(), ToolResult> {
    if let Some(top) = enclosing_repo(cwd) {
        return Err(ToolResult::err(
            "GIT_REPO_OUTSIDE_ROOT",
            format!(
                "rewrite_history runs only at a repository root; the root here is {}",
                top.display()
            ),
        ));
    }
    let git_dir = git_line(cwd, &["rev-parse", "--absolute-git-dir"]);
    let common = git_line(
        cwd,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    );
    if git_dir.is_none() || git_dir != common {
        return Err(ToolResult::err(
            "GIT_LINKED_WORKTREE",
            "this is a linked worktree; run rewrite_history from the main worktree",
        ));
    }
    if worktree_paths(cwd).len() > 1 {
        return Err(ToolResult::err(
            "GIT_HAS_WORKTREES",
            "the repository has linked worktrees whose checkouts would go stale; remove them first (git action worktree, op remove)",
        ));
    }
    if git_line(cwd, &["symbolic-ref", "--quiet", "HEAD"]).is_none() {
        return Err(ToolResult::err(
            "GIT_DETACHED_HEAD",
            "HEAD is detached; switch to a branch first (git action branch, op switch)",
        ));
    }
    let status = git(cwd, &["status", "--porcelain", "--untracked-files=all"]);
    if !status.ok || !status.stdout.trim().is_empty() {
        let listed: Vec<&str> = status.stdout.lines().take(10).collect();
        return Err(ToolResult::err(
            "GIT_DIRTY",
            format!(
                "the work tree has uncommitted or untracked files, and a rewrite resets the checkout; commit, stash, or remove them first:\n{}",
                listed.join("\n")
            ),
        ));
    }
    let mut args = vec![
        "log".to_string(),
        "--format=%H".into(),
        "-n".into(),
        "1".into(),
    ];
    args.extend(SCOPE.iter().map(|scope| format!("--glob={scope}/*")));
    args.push("--".into());
    args.extend(paths.iter().map(|path| literal(path)));
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let found = git(cwd, &args);
    if found.ok && found.stdout.trim().is_empty() {
        return Err(ToolResult::err(
            "GIT_NOTHING_TO_REWRITE",
            format!(
                "no branch or tag ever contained {}; check the paths with `git log --all --stat`",
                paths.join(", ")
            ),
        ));
    }
    Ok(())
}

/// Branches and tags by full ref name.
fn record(cwd: &Path) -> BTreeMap<String, RefState> {
    let format = [
        "%(refname)",
        "%(objectname)",
        "%(objecttype)",
        "%(taggername)",
        "%(taggeremail)",
        "%(taggerdate:raw)",
        "%(contents:subject)",
        "%(contents:body)",
    ]
    .join("%1f")
        + "%1e";
    let format = format!("--format={format}");
    let mut args = vec!["for-each-ref", format.as_str()];
    args.extend(SCOPE);
    git(cwd, &args)
        .stdout
        .split(RECORD)
        .filter_map(|record| {
            let record = record.trim_start_matches('\n');
            let mut fields = record.split('\u{1f}');
            let name = fields.next()?.to_string();
            let oid = fields.next()?.to_string();
            let kind = fields.next()?.to_string();
            if name.is_empty() {
                return None;
            }
            let tag_meta = if kind == "tag" {
                fields.collect::<Vec<_>>().join("\u{1f}")
            } else {
                String::new()
            };
            Some((
                name,
                RefState {
                    oid,
                    kind,
                    tag_meta,
                },
            ))
        })
        .collect()
}

/// `size-pack` plus loose objects, as `git count-objects -vH` prints them.
fn pack_size(cwd: &Path) -> String {
    let counts = git(cwd, &["count-objects", "-vH"]).stdout;
    let field = |key: &str| {
        counts
            .lines()
            .find_map(|line| line.strip_prefix(key))
            .map(str::trim)
            .unwrap_or("?")
            .to_string()
    };
    format!("{} packed + {} loose", field("size-pack:"), field("size:"))
}

/// Commits the reflogs hold, distinct.
fn reflog_tips(cwd: &Path) -> Vec<String> {
    let mut seen = HashSet::new();
    git(cwd, &["log", "-g", "--all", "--format=%H"])
        .stdout
        .lines()
        .filter(|line| !line.is_empty() && seen.insert(*line))
        .map(str::to_string)
        .collect()
}

/// The reflog tips no ref reaches: the cleanup's `gc --prune=now` would drop
/// them, and `bundle create --all` does not include them. Stale entries that
/// name objects this clone no longer has are skipped.
fn reflog_only_tips(cwd: &Path) -> Vec<String> {
    let tips = reflog_tips(cwd);
    if tips.is_empty() {
        return tips;
    }
    let listed = run(
        "git",
        cwd,
        &["cat-file", "--batch-check=%(objectname) %(objecttype)"],
        Some(&format!("{}\n", tips.join("\n"))),
    );
    if !listed.ok {
        // Cannot screen stale entries; keep them all in the bundle, where an
        // object this clone lacks makes `bundle create` refuse loudly.
        return tips;
    }
    let commits: HashSet<&str> = listed
        .stdout
        .lines()
        .filter_map(|line| line.split_once(' '))
        .filter(|(_, kind)| *kind == "commit")
        .map(|(oid, _)| oid)
        .collect();
    let tips: Vec<String> = tips
        .into_iter()
        .filter(|tip| commits.contains(tip.as_str()))
        .collect();
    if tips.is_empty() {
        return tips;
    }
    let mut args = vec!["rev-list"];
    args.extend(tips.iter().map(String::as_str));
    args.extend(["--not", "--all"]);
    let reached = git(cwd, &args);
    if !reached.ok {
        // Cannot tell which tips the refs hold; keep them all in the bundle.
        return tips;
    }
    let dangling: HashSet<&str> = reached.stdout.lines().collect();
    tips.into_iter()
        .filter(|tip| dangling.contains(tip.as_str()))
        .collect()
}

/// `git bundle create <rewrites>/<session>-<time>.bundle --all`, refused when
/// that directory would sit inside the work tree.
fn backup(
    cwd: &Path,
    rewrites: &Path,
    session: &str,
    tips: &[String],
) -> Result<PathBuf, ToolResult> {
    if let Err(error) = std::fs::create_dir_all(rewrites) {
        return Err(ToolResult::err(
            "GIT_BACKUP_FAILED",
            format!("cannot create {}: {error}", rewrites.display()),
        ));
    }
    let real = rewrites
        .canonicalize()
        .unwrap_or_else(|_| rewrites.to_path_buf());
    let root = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
    if real.starts_with(&root) {
        return Err(ToolResult::err(
            "GIT_BACKUP_FAILED",
            format!(
                "the backup directory {} is inside the work tree, where the rewrite could reach it; set a data directory outside the repository",
                real.display()
            ),
        ));
    }
    let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S%.3f");
    let session: String = session
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let bundle = real.join(format!("{session}-{stamp}.bundle"));
    let target = bundle.to_string_lossy().to_string();
    let mut args = vec!["bundle", "create", &target, "--all"];
    // Extra tips are packed but not advertised as refs; `git bundle
    // unbundle` still writes their objects back, which is how a reflog-only
    // commit comes back after `gc --prune=now`.
    args.extend(tips.iter().map(String::as_str));
    let created = git(cwd, &args);
    if !created.ok || !git(cwd, &["bundle", "verify", &target]).ok {
        let _ = std::fs::remove_file(&bundle);
        return Err(ToolResult::err(
            "GIT_BACKUP_FAILED",
            format!("{}; nothing was rewritten", created.error()),
        ));
    }
    Ok(bundle)
}

/// `git filter-repo`: paths as argv, commits and their messages kept as they
/// are (no empty-commit pruning, no hash rewriting in messages).
fn filter_repo(cwd: &Path, paths: &[String], refnames: &[&str]) -> Result<(), String> {
    let mut args = vec![
        "filter-repo",
        "--force",
        "--quiet",
        "--invert-paths",
        "--prune-empty",
        "never",
        "--prune-degenerate",
        "never",
        "--preserve-commit-hashes",
        // Keep any replace refs the user had; add none for old commits.
        "--replace-refs",
        "update-no-add",
    ];
    for path in paths {
        args.extend(["--path", path.as_str()]);
    }
    args.push("--refs");
    args.extend(refnames);
    let run = git(cwd, &args);
    if !run.ok {
        return Err(run.error());
    }
    // filter-repo leaves the index and work tree at the old commit when it
    // rewrites only some refs; check out the rewritten HEAD.
    let reset = git(cwd, &["reset", "--hard", "--quiet"]);
    if !reset.ok {
        return Err(reset.error());
    }
    Ok(())
}

/// `git filter-branch --index-filter`. The filter is shell text, so the paths
/// never appear in it: they go through a NUL-separated pathspec file whose
/// own path the harness chose and quotes.
fn filter_branch(
    cwd: &Path,
    rewrites: &Path,
    paths: &[String],
    refnames: &[&str],
) -> Result<(), String> {
    let work = WorkDir::new(rewrites)?;
    let spec = work.path().join("paths");
    let mut contents = Vec::new();
    for path in paths {
        contents.extend_from_slice(literal(path).as_bytes());
        contents.push(0);
    }
    if let Err(error) = std::fs::write(&spec, contents) {
        return Err(format!("cannot write {}: {error}", spec.display()));
    }
    let filter = format!(
        "git rm -r -q --cached --ignore-unmatch --pathspec-file-nul --pathspec-from-file={}",
        quote(&spec.to_string_lossy())
    );
    let temp = work.path().join("tree");
    let temp = temp.to_string_lossy().to_string();
    let mut args = vec![
        "filter-branch",
        "-f",
        "-d",
        temp.as_str(),
        "--index-filter",
        filter.as_str(),
        "--tag-name-filter",
        "cat",
        "--",
    ];
    args.extend(refnames);
    let run: Run = run_env(
        "git",
        cwd,
        &args,
        None,
        &[("FILTER_BRANCH_SQUELCH_WARNING", OsStr::new("1"))],
    );
    if !run.ok {
        return Err(run.error());
    }
    Ok(())
}

/// A scratch directory under the rewrites directory, removed on drop.
struct WorkDir(PathBuf);

impl WorkDir {
    fn new(parent: &Path) -> Result<Self, String> {
        let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S%.6f");
        let path = parent.join(format!("work-{}-{stamp}", std::process::id()));
        std::fs::create_dir(&path)
            .map_err(|error| format!("cannot create {}: {error}", path.display()))?;
        Ok(Self(path))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for WorkDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// What a commit must keep: author, committer, dates, parent count, message.
fn commit_meta(cwd: &Path, oid: &str) -> Result<Vec<String>, String> {
    let format = ["%an", "%ae", "%ad", "%cn", "%ce", "%cd", "%P", "%B"].join(FIELD) + "%x1e";
    let run = git(
        cwd,
        &[
            "log",
            "--topo-order",
            "--date=raw",
            &format!("--format={format}"),
            oid,
        ],
    );
    if !run.ok {
        return Err(run.error());
    }
    Ok(run
        .stdout
        .split(RECORD)
        .map(|record| record.trim_start_matches('\n'))
        .filter(|record| !record.is_empty())
        .map(|record| {
            let fields: Vec<&str> = record.split('\u{1f}').collect();
            let parents = fields.get(6).map_or(0, |p| p.split_whitespace().count());
            let mut kept: Vec<String> = fields.iter().map(|f| f.to_string()).collect();
            if kept.len() > 6 {
                kept[6] = format!("{parents} parent(s)");
            }
            kept.join("\u{1f}")
        })
        .collect())
}

/// Everything the rewrite promised, or the first thing it broke.
fn verify(cwd: &Path, before: &BTreeMap<String, RefState>, paths: &[String]) -> Result<(), String> {
    let fsck = git(cwd, &["fsck", "--no-dangling", "--no-progress"]);
    if !fsck.ok {
        return Err(format!("fsck reported a problem: {}", fsck.error()));
    }
    let after = record(cwd);
    let old: Vec<&String> = before.keys().collect();
    let new: Vec<&String> = after.keys().collect();
    if old != new {
        return Err(format!(
            "the set of branches and tags changed: before {old:?}, after {new:?}"
        ));
    }
    for (name, old) in before {
        let new = &after[name];
        if old.kind != new.kind {
            return Err(format!(
                "{name} was a {} and is now a {}",
                old.kind, new.kind
            ));
        }
        if old.tag_meta != new.tag_meta {
            return Err(format!(
                "{name} lost its tagger, date or message (a signed tag cannot keep its signature through a rewrite)"
            ));
        }
        let old_log = commit_meta(cwd, &old.oid)?;
        let new_log = commit_meta(cwd, &new.oid)?;
        if old_log.len() != new_log.len() {
            return Err(format!(
                "{name} had {} commits and now has {}",
                old_log.len(),
                new_log.len()
            ));
        }
        if let Some(index) = (0..old_log.len()).find(|&i| old_log[i] != new_log[i]) {
            return Err(format!(
                "commit {} of {name} (newest first) changed its author, committer, dates, parents or message",
                index + 1
            ));
        }
    }
    let mut args = vec![
        "log".to_string(),
        "--format=%H".into(),
        "-n".into(),
        "1".into(),
    ];
    args.extend(SCOPE.iter().map(|scope| format!("--glob={scope}/*")));
    args.push("--".into());
    args.extend(paths.iter().map(|path| literal(path)));
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let left = git(cwd, &args);
    if !left.ok {
        return Err(left.error());
    }
    if let Some(commit) = left.stdout.lines().next() {
        return Err(format!("commit {commit} still contains a removed path"));
    }
    Ok(())
}

/// Put every recorded ref back, drop refs the rewrite added, and reset the
/// checkout. The tree was clean before the rewrite, so the reset loses
/// nothing.
fn rollback(cwd: &Path, before: &BTreeMap<String, RefState>, head_branch: &str) -> String {
    let mut problems = Vec::new();
    for (name, state) in before {
        let run = git(cwd, &["update-ref", name, &state.oid]);
        if !run.ok {
            problems.push(run.error());
        }
    }
    for name in refs_under(cwd, &["refs/heads", "refs/tags", "refs/original"]) {
        if !before.contains_key(&name) {
            let run = git(cwd, &["update-ref", "-d", &name]);
            if !run.ok {
                problems.push(run.error());
            }
        }
    }
    if !head_branch.is_empty() {
        let run = git(cwd, &["symbolic-ref", "HEAD", head_branch]);
        if !run.ok {
            problems.push(run.error());
        }
    }
    let reset = git(cwd, &["reset", "--hard", "--quiet"]);
    if !reset.ok {
        problems.push(reset.error());
    }
    if problems.is_empty() {
        "every branch and tag is back at its old commit, and the checkout matches it.".into()
    } else {
        format!(
            "restoring the refs also failed: {}. Restore from the bundle with `git fetch <bundle> '+refs/*:refs/*'`.",
            problems.join("; ")
        )
    }
}

fn refs_under(cwd: &Path, prefixes: &[&str]) -> Vec<String> {
    let mut args = vec!["for-each-ref", "--format=%(refname)"];
    args.extend(prefixes);
    git(cwd, &args).stdout.lines().map(str::to_string).collect()
}

/// Refs the action does not rewrite but that still exist (remote-tracking,
/// stash, notes, replace refs).
fn outside_scope(cwd: &Path) -> Vec<String> {
    refs_under(cwd, &[])
        .into_iter()
        .filter(|name| {
            !SCOPE
                .iter()
                .any(|scope| name.starts_with(&format!("{scope}/")))
        })
        .collect()
}

/// Drop what still holds the old objects inside this repository, so the
/// removed content is really gone; the bundle keeps the only copy of the
/// pre-rewrite refs and of the reflog-only commits this prunes.
fn drop_old_history(cwd: &Path) -> String {
    let mut notes = Vec::new();
    for name in refs_under(cwd, &["refs/original"]) {
        let _ = git(cwd, &["update-ref", "-d", &name]);
    }
    for args in [
        &["reflog", "expire", "--expire=now", "--all"][..],
        &["gc", "--prune=now", "--quiet"][..],
    ] {
        let run = git(cwd, args);
        if !run.ok {
            notes.push(run.error());
        }
    }
    if notes.is_empty() {
        String::new()
    } else {
        format!(
            "cleanup was incomplete, so the old objects may still take space: {}\n",
            notes.join("; ")
        )
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::tools::bash::test_context;

    fn ok(dir: &Path, args: &[&str]) -> String {
        let run = git(dir, args);
        assert!(run.ok, "{}", run.error());
        run.stdout.trim().to_string()
    }

    /// Bytes that do not compress, so the blob dominates the pack size.
    fn noise(len: usize) -> Vec<u8> {
        let mut state: u64 = 0x9e37_79b9_7f4a_7c15;
        (0..len)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                state as u8
            })
            .collect()
    }

    fn commit(dir: &Path, subject: &str, author: &str) {
        ok(dir, &["add", "-A"]);
        ok(
            dir,
            &[
                "-c",
                &format!("user.name={author}"),
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                subject,
            ],
        );
    }

    /// The hard-bench A04 shape: HTML files, a video committed once and
    /// deleted later, a merged branch, a lightweight and an annotated tag.
    fn fixture(root: &Path) -> (ToolContext, PathBuf) {
        let repo = root.join("repo");
        std::fs::create_dir_all(repo.join("assets")).unwrap();
        for args in [
            vec!["init", "-q", "-b", "main"],
            vec!["config", "user.email", "test@example.invalid"],
            vec!["config", "user.name", "Test"],
            vec!["config", "commit.gpgsign", "false"],
            vec!["config", "tag.gpgsign", "false"],
        ] {
            ok(&repo, &args);
        }
        std::fs::write(repo.join("index.html"), "<h1>one</h1>\n").unwrap();
        commit(&repo, "Add the landing page", "Ana");
        std::fs::write(repo.join("assets/demo.mp4"), noise(1 << 20)).unwrap();
        std::fs::write(repo.join("about.html"), "<p>about</p>\n").unwrap();
        commit(&repo, "Add the demo video and about page", "Bo");
        ok(&repo, &["tag", "v1.0"]);
        ok(&repo, &["switch", "-q", "-c", "feature"]);
        std::fs::write(repo.join("contact.html"), "<p>contact</p>\n").unwrap();
        commit(&repo, "Add contact page", "Cy");
        ok(&repo, &["switch", "-q", "main"]);
        std::fs::write(repo.join("index.html"), "<h1>two</h1>\n").unwrap();
        commit(&repo, "Reword the heading", "Ana");
        ok(
            &repo,
            &[
                "merge",
                "-q",
                "--no-ff",
                "-m",
                "Merge branch 'feature'",
                "feature",
            ],
        );
        std::fs::remove_file(repo.join("assets/demo.mp4")).unwrap();
        commit(&repo, "Drop the demo video", "Bo");
        ok(
            &repo,
            &["tag", "-a", "v2.0", "-m", "Release 2.0\n\nSmaller."],
        );
        let mut ctx = test_context(root);
        ctx.cwd = repo.clone();
        (ctx, repo)
    }

    fn meta(repo: &Path, refname: &str) -> Vec<String> {
        commit_meta(repo, refname).unwrap()
    }

    #[test]
    fn removes_a_path_from_every_branch_and_tag_and_keeps_the_rest() {
        let root = tempfile::tempdir().unwrap();
        let (ctx, repo) = fixture(root.path());
        let blob = ok(&repo, &["rev-parse", "v1.0:assets/demo.mp4"]);
        let before: Vec<Vec<String>> = ["main", "feature", "v1.0", "v2.0"]
            .iter()
            .map(|name| meta(&repo, name))
            .collect();
        let old_main = ok(&repo, &["rev-parse", "main"]);

        let result = rewrite_history(&ctx, &json!({"paths": ["./assets/demo.mp4"]}));
        assert!(!result.is_error, "{}", result.content);

        let objects = ok(&repo, &["rev-list", "--objects", "--all"]);
        assert!(!objects.contains(&blob), "the blob is still reachable");
        assert!(!objects.contains("demo.mp4"));
        let after: Vec<Vec<String>> = ["main", "feature", "v1.0", "v2.0"]
            .iter()
            .map(|name| meta(&repo, name))
            .collect();
        assert_eq!(
            before, after,
            "messages, authors, dates and parents survive"
        );
        assert_eq!(meta(&repo, "main").len(), 6);
        let merge = ok(&repo, &["log", "--merges", "--format=%s", "main"]);
        assert_eq!(merge, "Merge branch 'feature'");
        assert_eq!(ok(&repo, &["cat-file", "-t", "v2.0"]), "tag");
        assert_eq!(ok(&repo, &["cat-file", "-t", "v1.0"]), "commit");
        let message = ok(&repo, &["tag", "-l", "-n9", "v2.0"]);
        assert!(message.contains("Release 2.0") && message.contains("Smaller."));
        assert!(refs_under(&repo, &["refs/original"]).is_empty());
        assert!(
            ok(&repo, &["status", "--porcelain"]).is_empty(),
            "the checkout matches the rewritten HEAD"
        );
        let size = ok(&repo, &["count-objects", "-v"]);
        let kib: u64 = size
            .lines()
            .filter_map(|line| {
                line.strip_prefix("size-pack: ")
                    .or_else(|| line.strip_prefix("size: "))
            })
            .map(|value| value.trim().parse::<u64>().unwrap())
            .sum();
        assert!(kib < 300, "repository is still {kib} KiB");

        let bundles: Vec<PathBuf> = std::fs::read_dir(ctx.paths.data_dir.join("rewrites"))
            .unwrap()
            .flatten()
            .map(|entry| entry.path())
            .collect();
        assert_eq!(bundles.len(), 1, "one bundle, and no leftover work dir");
        assert!(!bundles[0].starts_with(&repo));
        let heads = ok(
            &repo,
            &["bundle", "list-heads", &bundles[0].to_string_lossy()],
        );
        assert!(
            heads.contains(&old_main),
            "the bundle holds the old history"
        );
        let new_main = ok(&repo, &["rev-parse", "main"]);
        assert!(
            result
                .content
                .contains(&format!("refs/heads/main: {old_main} -> {new_main}")),
            "{}",
            result.content
        );
    }

    #[test]
    fn reflog_only_commits_survive_in_the_bundle() {
        let root = tempfile::tempdir().unwrap();
        let (ctx, repo) = fixture(root.path());
        // A reset-away commit: only the reflog still names it.
        std::fs::write(repo.join("note.txt"), "only in the reflog\n").unwrap();
        commit(&repo, "Kept only by the reflog", "Ana");
        let tip = ok(&repo, &["rev-parse", "HEAD"]);
        ok(&repo, &["reset", "-q", "--hard", "HEAD~1"]);
        assert!(ok(&repo, &["log", "-g", "--all", "--format=%H"]).contains(&tip));

        let result = rewrite_history(&ctx, &json!({"paths": ["assets/demo.mp4"]}));
        assert!(!result.is_error, "{}", result.content);
        assert!(result.content.contains(&tip), "{}", result.content);
        // The cleanup pruned it from the object store...
        assert!(!git(&repo, &["cat-file", "-e", &tip]).ok, "gc kept {tip}");
        // ... and the backup bundle is how it comes back.
        let bundle = std::fs::read_dir(ctx.paths.data_dir.join("rewrites"))
            .unwrap()
            .flatten()
            .map(|entry| entry.path())
            .next()
            .expect("a bundle");
        ok(&repo, &["bundle", "unbundle", &bundle.to_string_lossy()]);
        assert_eq!(ok(&repo, &["cat-file", "-t", &tip]), "commit");
        assert_eq!(
            ok(&repo, &["show", "-s", "--format=%s", &tip]),
            "Kept only by the reflog"
        );
    }

    #[test]
    fn a_path_is_data_not_shell_text() {
        let root = tempfile::tempdir().unwrap();
        let (ctx, repo) = fixture(root.path());
        let hostile = "x'; touch pwned; echo '$(touch pwned2)";
        std::fs::write(repo.join(hostile), "secret\n").unwrap();
        commit(&repo, "Add an odd file", "Ana");
        std::fs::write(repo.join("[ab].txt"), "kept\n").unwrap();
        std::fs::write(repo.join("a.txt"), "kept\n").unwrap();
        commit(&repo, "Add bracket names", "Ana");

        let result = rewrite_history(&ctx, &json!({"paths": [hostile, "[ab].txt"]}));
        assert!(!result.is_error, "{}", result.content);
        for marker in ["pwned", "pwned2"] {
            assert!(!repo.join(marker).exists(), "{marker} was created");
            assert!(!root.path().join(marker).exists());
        }
        let files = ok(&repo, &["ls-tree", "-r", "--name-only", "main"]);
        assert!(!files.contains("touch pwned"));
        assert!(!files.contains("[ab].txt"));
        assert!(
            files.lines().any(|line| line == "a.txt"),
            "a glob-looking path matched only itself"
        );
    }

    #[test]
    fn refuses_before_touching_anything() {
        let root = tempfile::tempdir().unwrap();
        let (ctx, repo) = fixture(root.path());
        let main = ok(&repo, &["rev-parse", "main"]);
        for (args, code) in [
            (json!({}), "invalid_args"),
            (json!({"paths": []}), "invalid_args"),
            (json!({"paths": ["../x"]}), "invalid_args"),
            (json!({"paths": ["/etc/passwd"]}), "invalid_args"),
            (json!({"paths": ["."]}), "invalid_args"),
            (json!({"paths": [".git/config"]}), "invalid_args"),
            (
                json!({"paths": ["never/existed.bin"]}),
                "GIT_NOTHING_TO_REWRITE",
            ),
        ] {
            let result = rewrite_history(&ctx, &args);
            assert!(result.is_error);
            assert!(
                result.content.starts_with(&format!("[{code}]")),
                "{args}: {}",
                result.content
            );
        }
        std::fs::write(repo.join("index.html"), "dirty\n").unwrap();
        let dirty = rewrite_history(&ctx, &json!({"paths": ["assets/demo.mp4"]}));
        assert!(
            dirty.content.starts_with("[GIT_DIRTY]"),
            "{}",
            dirty.content
        );
        ok(&repo, &["checkout", "--", "index.html"]);
        ok(&repo, &["checkout", "-q", "--detach", "main"]);
        let detached = rewrite_history(&ctx, &json!({"paths": ["assets/demo.mp4"]}));
        assert!(detached.content.starts_with("[GIT_DETACHED_HEAD]"));
        assert_eq!(ok(&repo, &["rev-parse", "main"]), main);
        assert!(!ctx.paths.data_dir.join("rewrites").exists(), "no bundle");
    }

    #[test]
    fn a_backup_inside_the_work_tree_is_refused() {
        let root = tempfile::tempdir().unwrap();
        let (mut ctx, repo) = fixture(root.path());
        ctx.paths.data_dir = repo.join("data");
        let result = rewrite_history(&ctx, &json!({"paths": ["assets/demo.mp4"]}));
        assert!(
            result.content.starts_with("[GIT_BACKUP_FAILED]"),
            "{}",
            result.content
        );
    }

    #[test]
    fn rollback_restores_every_ref_and_the_checkout() {
        let root = tempfile::tempdir().unwrap();
        let (_ctx, repo) = fixture(root.path());
        let before = record(&repo);
        // Simulate a bad rewrite: move main, retag, add a stray ref, and
        // leave refs/original behind.
        ok(&repo, &["reset", "-q", "--hard", "HEAD~2"]);
        ok(&repo, &["tag", "-f", "v1.0", "HEAD"]);
        ok(&repo, &["branch", "stray"]);
        ok(
            &repo,
            &["update-ref", "refs/original/refs/heads/main", "HEAD"],
        );
        let report = rollback(&repo, &before, "refs/heads/main");
        assert!(
            report.starts_with("every branch and tag is back"),
            "{report}"
        );
        assert_eq!(record(&repo), before);
        assert!(refs_under(&repo, &["refs/original"]).is_empty());
        assert!(ok(&repo, &["status", "--porcelain"]).is_empty());
        assert_eq!(
            std::fs::read_to_string(repo.join("index.html")).unwrap(),
            "<h1>two</h1>\n"
        );
    }

    #[test]
    fn verify_catches_a_changed_message() {
        let root = tempfile::tempdir().unwrap();
        let (_ctx, repo) = fixture(root.path());
        let before = record(&repo);
        ok(
            &repo,
            &["commit", "-q", "--amend", "-m", "Different subject"],
        );
        let problem = verify(&repo, &before, &["assets/demo.mp4".to_string()]).unwrap_err();
        assert!(problem.contains("refs/heads/main"), "{problem}");
    }
}
