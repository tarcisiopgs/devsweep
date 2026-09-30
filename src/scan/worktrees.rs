//! Git worktrees, both under the scanned folder and in coding-agent roots.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use ignore::WalkState;

use crate::model::{Item, Removal, SourceId, Status};
use crate::scan::artifacts::classify;
use crate::scan::{ScanCtx, ScanEvent, Scanner, Sender, run_timeout, size_later};

/// A worktree is stale once its HEAD commit is this old.
pub const STALE_DAYS: u32 = 14;

/// Agent worktree roots, relative to `$HOME`.
pub const AGENT_ROOTS: &[&str] = &[
    ".codex/worktrees",
    "orca/workspaces",
    ".claude-squad/worktrees",
];

#[derive(Debug, PartialEq, Eq)]
pub struct WorktreeEntry {
    pub path: PathBuf,
    pub head: String,
    pub branch: Option<String>,
    pub prunable: bool,
}

/// Parse `git worktree list --porcelain`.
pub fn parse_porcelain(s: &str) -> Vec<WorktreeEntry> {
    let mut entries = Vec::new();
    let mut current: Option<WorktreeEntry> = None;
    for line in s.lines() {
        if let Some(path) = line.strip_prefix("worktree ") {
            entries.extend(current.take());
            current = Some(WorktreeEntry {
                path: PathBuf::from(path),
                head: String::new(),
                branch: None,
                prunable: false,
            });
        } else if let Some(entry) = current.as_mut() {
            if let Some(head) = line.strip_prefix("HEAD ") {
                entry.head = head.to_string();
            } else if let Some(branch) = line.strip_prefix("branch ") {
                entry.branch = Some(branch.trim_start_matches("refs/heads/").to_string());
            } else if line.starts_with("prunable") {
                entry.prunable = true;
            }
        }
    }
    entries.extend(current);
    entries
}

/// Longest a single git call may take before the worktree is reported as
/// "status unknown" instead of stalling the whole scan.
const GIT_TIMEOUT: Duration = Duration::from_secs(15);

fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let argv: Vec<&str> = ["git", "-C", dir.to_str()?]
        .into_iter()
        .chain(args.iter().copied())
        .collect();
    run_timeout(&argv, None, GIT_TIMEOUT)
        .ok()
        .map(|out| out.trim().to_string())
}

fn git_ok(dir: &Path, args: &[&str]) -> bool {
    git(dir, args).is_some()
}

fn default_branch(main: &Path) -> Option<String> {
    if let Some(remote) = git(
        main,
        &["symbolic-ref", "--short", "refs/remotes/origin/HEAD"],
    ) {
        return Some(remote);
    }
    ["main", "master"]
        .into_iter()
        .find(|b| {
            git_ok(
                main,
                &["rev-parse", "--verify", "-q", &format!("refs/heads/{b}")],
            )
        })
        .map(String::from)
}

fn is_merged(main: &Path, head: &str, branch: Option<&str>) -> bool {
    if let Some(default) = default_branch(main)
        && !head.is_empty()
        && git_ok(main, &["merge-base", "--is-ancestor", head, &default])
    {
        return true;
    }
    branch.is_some_and(|b| {
        git(
            main,
            &[
                "for-each-ref",
                "--format=%(upstream:track)",
                &format!("refs/heads/{b}"),
            ],
        )
        .as_deref()
            == Some("[gone]")
    })
}

/// Build the item for one worktree. `main` is `None` when the main
/// repository cannot be reached.
fn describe(
    ctx: &ScanCtx,
    source: SourceId,
    wt: &Path,
    main: Option<&Path>,
    label: String,
) -> Item {
    let lock = ctx.inuse.lock_for(wt);
    let Some(main) = main else {
        return Item {
            id: ctx.next_id(),
            source,
            label,
            path: Some(wt.to_path_buf()),
            size: None,
            status: vec![Status::Broken],
            lock,
            safe: false,
            removal: Removal::RemoveDir(wt.to_path_buf()),
            age_days: None,
        };
    };
    let removal = Removal::Command {
        argv: vec![
            "git".into(),
            "worktree".into(),
            "remove".into(),
            wt.display().to_string(),
        ],
        cwd: Some(main.to_path_buf()),
    };
    // A failing or hanging `git status` says nothing about the worktree
    // itself: never call it broken, never preselect it.
    let Some(status_out) = git(wt, &["status", "--porcelain"]) else {
        return Item {
            id: ctx.next_id(),
            source,
            label,
            path: Some(wt.to_path_buf()),
            size: None,
            status: vec![Status::Detail("status unknown".into())],
            lock,
            safe: false,
            removal,
            age_days: None,
        };
    };
    let head = git(wt, &["rev-parse", "HEAD"]).unwrap_or_default();
    let branch = git(wt, &["symbolic-ref", "-q", "--short", "HEAD"]);
    let merged = is_merged(main, &head, branch.as_deref());
    let dirty = status_out.lines().filter(|l| !l.trim().is_empty()).count() as u32;
    let age = git(wt, &["log", "-1", "--format=%ct"])
        .and_then(|t| t.parse::<u64>().ok())
        .map(|t| {
            crate::fsutil::days_since(std::time::UNIX_EPOCH + std::time::Duration::from_secs(t))
        });

    let mut status = Vec::new();
    if merged {
        status.push(Status::Merged);
    }
    status.push(if dirty > 0 {
        Status::Dirty(dirty)
    } else {
        Status::Clean
    });
    if let Some(days) = age.filter(|d| *d >= STALE_DAYS) {
        status.push(Status::Stale(days));
    }
    Item {
        id: ctx.next_id(),
        source,
        label,
        path: Some(wt.to_path_buf()),
        size: None,
        status,
        safe: merged && dirty == 0 && lock.is_none(),
        lock,
        removal,
        age_days: age,
    }
}

fn emit(tx: &Sender<ScanEvent>, item: Item) {
    let (path, id) = (item.path.clone(), item.id);
    let _ = tx.send(ScanEvent::Found(item));
    if let Some(path) = path {
        size_later(path, id, tx.clone());
    }
}

/// Label for a worktree in an agent root: `codex/name`, `orca/repo/name`.
fn agent_label(home: &Path, wt: &Path) -> Option<String> {
    AGENT_ROOTS.iter().find_map(|root| {
        let rel = wt.strip_prefix(home.join(root)).ok()?;
        let short = root.split('/').next()?.trim_start_matches('.');
        Some(format!("{short}/{}", rel.display()))
    })
}

fn tilde(home: &Path, p: &Path) -> String {
    match p.strip_prefix(home) {
        Ok(rel) => format!("~/{}", rel.display()),
        Err(_) => p.display().to_string(),
    }
}

/// Worktrees of the repositories found under the scanned folder.
pub struct Worktrees;

struct DoneGuard<'a>(&'a ScanCtx);

impl Drop for DoneGuard<'_> {
    fn drop(&mut self) {
        self.0.mark_worktrees_done();
    }
}

impl Scanner for Worktrees {
    fn source(&self) -> SourceId {
        SourceId::Worktrees
    }

    fn available(&self) -> bool {
        crate::scan::which("git").is_some()
    }

    fn scan(&self, ctx: &ScanCtx, tx: &Sender<ScanEvent>) -> anyhow::Result<()> {
        let _done = DoneGuard(ctx);
        for repo in find_repos(ctx) {
            let Some(list) = git(&repo, &["worktree", "list", "--porcelain"]) else {
                continue;
            };
            for entry in parse_porcelain(&list).into_iter().skip(1) {
                if entry.prunable || !entry.path.exists() {
                    continue;
                }
                let wt = std::fs::canonicalize(&entry.path).unwrap_or(entry.path);
                if !ctx.seen_worktrees.lock().unwrap().insert(wt.clone()) {
                    continue;
                }
                let label = match wt.strip_prefix(&ctx.target) {
                    Ok(rel) => rel.display().to_string(),
                    Err(_) => agent_label(&ctx.home, &wt).unwrap_or_else(|| tilde(&ctx.home, &wt)),
                };
                emit(
                    tx,
                    describe(ctx, SourceId::Worktrees, &wt, Some(&repo), label),
                );
            }
        }
        Ok(())
    }
}

/// Repositories (a `.git` directory) under the target, not descending into them.
fn find_repos(ctx: &ScanCtx) -> Vec<PathBuf> {
    let repos = Mutex::new(Vec::new());
    ignore::WalkBuilder::new(&ctx.target)
        .standard_filters(false)
        .follow_links(false)
        .same_file_system(true)
        .build_parallel()
        .run(|| {
            Box::new(|entry| {
                let Ok(entry) = entry else {
                    return WalkState::Continue;
                };
                if !entry.file_type().is_some_and(|t| t.is_dir()) {
                    return WalkState::Continue;
                }
                let path = entry.path();
                let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                let at_home = path.parent() == Some(ctx.home.as_path());
                if name == ".git"
                    || classify(path).is_some()
                    || (at_home && (name == "Library" || name == ".Trash"))
                {
                    return WalkState::Skip;
                }
                if path.join(".git").is_dir() {
                    repos.lock().unwrap().push(path.to_path_buf());
                    return WalkState::Skip;
                }
                WalkState::Continue
            })
        });
    let mut repos = repos.into_inner().unwrap();
    repos.sort();
    repos
}

/// Worktrees that coding agents keep outside any project folder.
pub struct AgentWorktrees {
    pub roots: Vec<PathBuf>,
}

impl AgentWorktrees {
    pub fn for_home(home: &Path) -> AgentWorktrees {
        AgentWorktrees {
            roots: AGENT_ROOTS.iter().map(|r| home.join(r)).collect(),
        }
    }
}

impl Scanner for AgentWorktrees {
    fn source(&self) -> SourceId {
        SourceId::AgentWorktrees
    }

    fn available(&self) -> bool {
        crate::scan::which("git").is_some() && self.roots.iter().any(|r| r.is_dir())
    }

    fn scan(&self, ctx: &ScanCtx, tx: &Sender<ScanEvent>) -> anyhow::Result<()> {
        ctx.wait_worktrees_done();
        for root in self.roots.iter().filter(|r| r.is_dir()) {
            let root = std::fs::canonicalize(root).unwrap_or(root.clone());
            for wt in find_worktree_dirs(&root, 3) {
                if ctx.seen_worktrees.lock().unwrap().contains(&wt) {
                    continue;
                }
                let main = git(
                    &wt,
                    &["rev-parse", "--path-format=absolute", "--git-common-dir"],
                )
                .map(PathBuf::from)
                .and_then(|common| common.parent().map(Path::to_path_buf))
                .filter(|main| main.join(".git").exists());
                let label = agent_label(&ctx.home, &wt)
                    .or_else(|| {
                        let short = root
                            .file_name()?
                            .to_string_lossy()
                            .trim_start_matches('.')
                            .to_string();
                        Some(format!(
                            "{short}/{}",
                            wt.strip_prefix(&root).ok()?.display()
                        ))
                    })
                    .unwrap_or_else(|| tilde(&ctx.home, &wt));
                emit(
                    tx,
                    describe(ctx, SourceId::AgentWorktrees, &wt, main.as_deref(), label),
                );
            }
        }
        Ok(())
    }
}

/// Directories holding a `.git` *file* (a linked worktree), up to `depth` below `root`.
fn find_worktree_dirs(root: &Path, depth: usize) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut frontier = vec![root.to_path_buf()];
    for _ in 0..depth {
        let mut next = Vec::new();
        for dir in frontier {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if !entry.file_type().is_ok_and(|t| t.is_dir()) {
                    continue;
                }
                if path.join(".git").is_file() {
                    found.push(path);
                } else if !path.join(".git").exists() && classify(&path).is_none() {
                    next.push(path);
                }
            }
        }
        frontier = next;
    }
    found.sort();
    found
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inuse::InUse;
    use crate::model::{Removal, Status};
    use std::fs;
    use std::path::Path;
    use std::process::Command;

    fn git(dir: &Path, args: &[&str]) {
        git_env(dir, args, &[]);
    }

    fn git_env(dir: &Path, args: &[&str], env: &[(&str, &str)]) {
        let out = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .envs(env.iter().copied())
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {:?}: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn repo(dir: &Path) {
        fs::create_dir_all(dir).unwrap();
        git(dir, &["init", "-q", "-b", "main"]);
        git(dir, &["config", "user.email", "t@t"]);
        git(dir, &["config", "user.name", "t"]);
        fs::write(dir.join("a.txt"), "a").unwrap();
        git(dir, &["add", "."]);
        git(dir, &["commit", "-q", "-m", "init"]);
    }

    fn ctx(target: &Path, home: &Path, inuse: InUse) -> ScanCtx {
        ScanCtx::new(target.to_path_buf(), home.to_path_buf(), inuse)
    }

    fn run_folder(c: &ScanCtx) -> Vec<Item> {
        let (tx, rx) = crossbeam_channel::unbounded();
        Worktrees.scan(c, &tx).unwrap();
        drop(tx);
        rx.iter()
            .filter_map(|e| {
                if let ScanEvent::Found(i) = e {
                    Some(i)
                } else {
                    None
                }
            })
            .collect()
    }

    fn run_agent(c: &ScanCtx, roots: Vec<PathBuf>) -> Vec<Item> {
        c.mark_worktrees_done();
        let (tx, rx) = crossbeam_channel::unbounded();
        AgentWorktrees { roots }.scan(c, &tx).unwrap();
        drop(tx);
        rx.iter()
            .filter_map(|e| {
                if let ScanEvent::Found(i) = e {
                    Some(i)
                } else {
                    None
                }
            })
            .collect()
    }

    /// A repo under `target/r` with one worktree at `target/wt`.
    fn setup(branch_merged: bool) -> (tempfile::TempDir, PathBuf, PathBuf) {
        let d = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(d.path()).unwrap();
        let r = root.join("r");
        repo(&r);
        git(&r, &["checkout", "-q", "-b", "feat"]);
        fs::write(r.join("b.txt"), "b").unwrap();
        git(&r, &["add", "."]);
        git(&r, &["commit", "-q", "-m", "feat"]);
        git(&r, &["checkout", "-q", "main"]);
        if branch_merged {
            git(&r, &["merge", "-q", "--ff-only", "feat"]);
        }
        let wt = root.join("wt");
        git(&r, &["worktree", "add", "-q", wt.to_str().unwrap(), "feat"]);
        (d, root, wt)
    }

    #[test]
    fn parses_porcelain_fixture() {
        let entries = parse_porcelain(include_str!("../../tests/fixtures/worktree-porcelain.txt"));
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[1].branch, None);
        assert_eq!(entries[2].branch.as_deref(), Some("agent-a96a"));
        assert!(entries[2].prunable);
    }

    #[test]
    fn merged_clean_worktree_is_safe() {
        let (_d, root, _wt) = setup(true);
        let items = run_folder(&ctx(&root, &root, InUse::default()));
        assert_eq!(items.len(), 1);
        assert!(items[0].status.contains(&Status::Merged));
        assert!(items[0].status.contains(&Status::Clean));
        assert!(items[0].safe);
    }

    #[test]
    fn unmerged_worktree_is_not_safe() {
        let (_d, root, _wt) = setup(false);
        let items = run_folder(&ctx(&root, &root, InUse::default()));
        assert!(!items[0].status.contains(&Status::Merged));
        assert!(!items[0].safe);
    }

    #[test]
    fn detached_head_contained_in_main_is_merged() {
        let d = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(d.path()).unwrap();
        let r = root.join("r");
        repo(&r);
        git(
            &r,
            &[
                "worktree",
                "add",
                "-q",
                "--detach",
                root.join("wt").to_str().unwrap(),
                "main",
            ],
        );
        let items = run_folder(&ctx(&root, &root, InUse::default()));
        assert!(items[0].status.contains(&Status::Merged));
    }

    #[test]
    fn dirty_worktree_reports_file_count_and_is_not_safe() {
        let (_d, root, wt) = setup(true);
        for f in ["x", "y", "z"] {
            fs::write(wt.join(f), "1").unwrap();
        }
        let items = run_folder(&ctx(&root, &root, InUse::default()));
        assert!(items[0].status.contains(&Status::Dirty(3)));
        assert!(!items[0].safe);
    }

    #[test]
    fn stale_after_14_days() {
        let (_d, root, wt) = setup(true);
        let old = format!(
            "{} +0000",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs()
                - 41 * 86_400
                - 60
        );
        fs::write(wt.join("c.txt"), "c").unwrap();
        git(&wt, &["add", "."]);
        git_env(
            &wt,
            &["commit", "-q", "-m", "old"],
            &[("GIT_COMMITTER_DATE", &old), ("GIT_AUTHOR_DATE", &old)],
        );
        let items = run_folder(&ctx(&root, &root, InUse::default()));
        assert!(
            items[0].status.contains(&Status::Stale(41)),
            "{:?}",
            items[0].status
        );
    }

    #[test]
    fn not_stale_at_13_days() {
        let (_d, root, wt) = setup(true);
        let old = format!(
            "{} +0000",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs()
                - 13 * 86_400
                - 60
        );
        fs::write(wt.join("c.txt"), "c").unwrap();
        git(&wt, &["add", "."]);
        git_env(
            &wt,
            &["commit", "-q", "-m", "old"],
            &[("GIT_COMMITTER_DATE", &old), ("GIT_AUTHOR_DATE", &old)],
        );
        let items = run_folder(&ctx(&root, &root, InUse::default()));
        assert!(
            !items[0]
                .status
                .iter()
                .any(|s| matches!(s, Status::Stale(_)))
        );
        assert_eq!(items[0].age_days, Some(13));
    }

    #[test]
    fn locked_worktree_is_not_safe_even_if_merged() {
        let (_d, root, wt) = setup(true);
        let iu = InUse::parse(&format!("p42\ncclaude\nn{}\n", wt.display()));
        let items = run_folder(&ctx(&root, &root, iu));
        assert_eq!(items[0].lock.as_deref(), Some("claude · PID 42"));
        assert!(!items[0].safe);
    }

    #[test]
    fn removal_runs_git_worktree_remove_in_main_repo() {
        let (_d, root, wt) = setup(true);
        let items = run_folder(&ctx(&root, &root, InUse::default()));
        assert_eq!(
            items[0].removal,
            Removal::Command {
                argv: vec![
                    "git".into(),
                    "worktree".into(),
                    "remove".into(),
                    wt.display().to_string()
                ],
                cwd: Some(root.join("r")),
            }
        );
    }

    #[test]
    fn failing_git_status_is_unknown_not_broken() {
        let (_d, root, wt) = setup(true);
        // A corrupt index makes `git status` fail while the main repo is fine.
        let gitdir = std::fs::read_to_string(wt.join(".git")).unwrap();
        let gitdir = PathBuf::from(gitdir.trim().trim_start_matches("gitdir: "));
        fs::write(gitdir.join("index"), b"garbage").unwrap();
        let items = run_folder(&ctx(&root, &root, InUse::default()));
        assert!(
            items[0]
                .status
                .contains(&Status::Detail("status unknown".into())),
            "{:?}",
            items[0].status
        );
        assert!(!items[0].status.contains(&Status::Broken));
        assert!(!items[0].safe);
        assert!(
            matches!(&items[0].removal, Removal::Command { argv, .. } if argv[1] == "worktree")
        );
    }

    #[test]
    fn worktree_with_missing_main_repo_is_broken() {
        let d = tempfile::tempdir().unwrap();
        let home = fs::canonicalize(d.path()).unwrap();
        let r = home.join("r");
        repo(&r);
        let agent_root = home.join(".codex/worktrees");
        fs::create_dir_all(&agent_root).unwrap();
        let wt = agent_root.join("w1");
        git(
            &r,
            &[
                "worktree",
                "add",
                "-q",
                "--detach",
                wt.to_str().unwrap(),
                "main",
            ],
        );
        fs::remove_dir_all(r.join(".git")).unwrap();
        let items = run_agent(
            &ctx(&home.join("elsewhere"), &home, InUse::default()),
            vec![agent_root],
        );
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].status, vec![Status::Broken]);
        assert_eq!(items[0].removal, Removal::RemoveDir(wt));
        assert!(!items[0].safe);
    }

    #[test]
    fn agent_roots_find_nested_worktrees_and_skip_seen() {
        let d = tempfile::tempdir().unwrap();
        let home = fs::canonicalize(d.path()).unwrap();
        let r = home.join("r");
        repo(&r);
        let orca = home.join("orca/workspaces");
        let a = orca.join("site/arowana");
        let b = orca.join("site/betta");
        fs::create_dir_all(orca.join("site")).unwrap();
        git(
            &r,
            &[
                "worktree",
                "add",
                "-q",
                "--detach",
                a.to_str().unwrap(),
                "main",
            ],
        );
        git(
            &r,
            &[
                "worktree",
                "add",
                "-q",
                "--detach",
                b.to_str().unwrap(),
                "main",
            ],
        );
        let c = ctx(&home.join("elsewhere"), &home, InUse::default());
        c.seen_worktrees.lock().unwrap().insert(b.clone());
        let items = run_agent(&c, vec![orca]);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].label, "orca/site/arowana");
        assert_eq!(items[0].source, SourceId::AgentWorktrees);
    }
}
