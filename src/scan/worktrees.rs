//! Git worktrees, both under the scanned folder and in coding-agent roots.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use ignore::WalkState;

use crate::model::{Item, LeftoverBranch, Removal, SourceId, Status};
use crate::scan::artifacts::classify;
use crate::scan::{ScanCtx, ScanEvent, Scanner, Sender, lock, run_timeout, size_later};

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

/// The branch that removing the worktree `wt` leaves behind in `repo`, when
/// deleting it loses nothing: its tip is already in the default branch, or
/// its whole diff landed there as a single commit (a squash merge). A branch
/// whose upstream is merely gone is not enough. Asked before the removal,
/// while the worktree can still say which branch it holds.
pub fn leftover_branch(wt: &Path, repo: &Path) -> Option<LeftoverBranch> {
    let branch = git(wt, &["symbolic-ref", "-q", "--short", "HEAD"]).filter(|b| !b.is_empty())?;
    let head = git(
        repo,
        &[
            "rev-parse",
            "--verify",
            "-q",
            &format!("refs/heads/{branch}"),
        ],
    )?;
    let default = default_branch(repo)?;
    let default_name = default.strip_prefix("origin/").unwrap_or(&default);
    if branch == default_name || branch == "main" || branch == "master" {
        return None;
    }
    let merged = git_ok(repo, &["merge-base", "--is-ancestor", &head, &default])
        || squash_merged(repo, &head, &default);
    merged.then(|| LeftoverBranch {
        repo: repo.to_path_buf(),
        branch,
        head,
    })
}

/// Whether the default branch has one commit carrying the whole diff of
/// `head`. The branch is squashed into a throwaway commit on its merge base
/// and `git cherry` looks for the same patch; that commit is unreachable and
/// goes with the next `git gc`.
fn squash_merged(repo: &Path, head: &str, default: &str) -> bool {
    let Some(base) = git(repo, &["merge-base", default, head]) else {
        return false;
    };
    let Some(squashed) = git(
        repo,
        &[
            "-c",
            "user.name=devsweep",
            "-c",
            "user.email=devsweep@localhost",
            "commit-tree",
            &format!("{head}^{{tree}}"),
            "-p",
            &base,
            "-m",
            "devsweep: squash check",
        ],
    ) else {
        return false;
    };
    git(repo, &["cherry", default, &squashed]).is_some_and(|out| out.starts_with('-'))
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
            recheck: crate::model::Recheck::default(),
        };
    };
    let removal = Removal::Worktree {
        path: wt.to_path_buf(),
        repo: main.to_path_buf(),
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
            lock: lock.or_else(|| Some("git did not answer".into())),
            safe: false,
            removal,
            age_days: None,
            recheck: crate::model::Recheck::default(),
        };
    };
    let head = git(wt, &["rev-parse", "HEAD"]).unwrap_or_default();
    let branch = git(wt, &["symbolic-ref", "-q", "--short", "HEAD"]);
    let merged = is_merged(main, &head, branch.as_deref());
    let dirty = status_out.lines().filter(|l| !l.trim().is_empty()).count() as u32;
    // Unknown counts as something to lose: the worktree is then never safe.
    let ignored = precious_ignored(wt);
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
    match ignored {
        Some(0) => {}
        Some(n) => status.push(Status::Ignored(n)),
        None => status.push(Status::Detail("ignored files unknown".into())),
    }
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
        safe: merged && dirty == 0 && ignored == Some(0) && lock.is_none(),
        lock,
        removal,
        age_days: age,
        recheck: crate::model::Recheck::default(),
    }
}

/// Git-ignored entries of a worktree that are not build artifacts, which
/// `git worktree remove` would delete along with it. `None` when git fails.
pub fn precious_ignored(wt: &Path) -> Option<u32> {
    let out = git(wt, &["status", "--porcelain", "--ignored"])?;
    let count = out
        .lines()
        .filter_map(|l| l.strip_prefix("!! "))
        .filter(|rel| classify(&wt.join(rel.trim_end_matches('/'))).is_none())
        .count();
    Some(count as u32)
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
                if !lock(&ctx.seen_worktrees).insert(wt.clone()) {
                    continue;
                }
                // Where the worktree lives decides its section: one inside an
                // agent root is an agent worktree, whatever folder was scanned.
                let (source, label) = match agent_label(&ctx.home, &wt) {
                    Some(label) => (SourceId::AgentWorktrees, label),
                    None => (
                        SourceId::Worktrees,
                        match wt.strip_prefix(&ctx.target) {
                            Ok(rel) => rel.display().to_string(),
                            Err(_) => tilde(&ctx.home, &wt),
                        },
                    ),
                };
                emit(tx, describe(ctx, source, &wt, Some(&repo), label));
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
                    || (at_home && ctx.os.skip_at_home().contains(&name))
                {
                    return WalkState::Skip;
                }
                if path.join(".git").is_dir() {
                    lock(&repos).push(path.to_path_buf());
                    return WalkState::Skip;
                }
                WalkState::Continue
            })
        });
    let mut repos = repos.into_inner().unwrap_or_else(PoisonError::into_inner);
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
                if lock(&ctx.seen_worktrees).contains(&wt) {
                    continue;
                }
                let main = match main_repo(ctx, &wt) {
                    MainRepo::Found(main) => Some(main),
                    MainRepo::Gone => None,
                    MainRepo::Unknown => {
                        emit(
                            tx,
                            unknown(
                                ctx,
                                &wt,
                                agent_label(&ctx.home, &wt)
                                    .unwrap_or_else(|| tilde(&ctx.home, &wt)),
                            ),
                        );
                        continue;
                    }
                };
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

enum MainRepo {
    /// The repository (or bare repository) that owns the worktree.
    Found(PathBuf),
    /// The worktree's `gitdir:` target no longer exists.
    Gone,
    /// git could not tell (failure, timeout, privacy prompt).
    Unknown,
}

/// Owner of a linked worktree. Only a missing `gitdir:` target counts as
/// gone; any git failure is unknown, never a reason to delete the folder.
fn main_repo(ctx: &ScanCtx, wt: &Path) -> MainRepo {
    let Ok(dotgit) = std::fs::read_to_string(wt.join(".git")) else {
        return MainRepo::Unknown;
    };
    let Some(gitdir) = dotgit
        .trim()
        .strip_prefix("gitdir:")
        .map(|g| PathBuf::from(g.trim()))
    else {
        return MainRepo::Unknown;
    };
    let gitdir = if gitdir.is_absolute() {
        gitdir
    } else {
        wt.join(gitdir)
    };
    // Only a gitdir that is certainly missing means gone. A permission error
    // or an unmounted volume says nothing about the repository.
    match std::fs::exists(&gitdir) {
        Ok(true) => {}
        Ok(false) if storage_present(ctx, &gitdir) => return MainRepo::Gone,
        _ => return MainRepo::Unknown,
    }
    let Some(common) = git(
        wt,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )
    .map(PathBuf::from) else {
        return MainRepo::Unknown;
    };
    if common.file_name().is_some_and(|n| n == ".git") {
        match common.parent() {
            Some(parent) => MainRepo::Found(parent.to_path_buf()),
            None => MainRepo::Unknown,
        }
    } else {
        // Bare repository: git commands run inside it directly.
        MainRepo::Found(common)
    }
}

/// Whether the place a missing `gitdir` lived in is certainly there, so its
/// absence means the repository is gone and not merely out of reach.
///
/// The closest folder that still exists is the evidence. It has to resolve
/// (a symlink to an unplugged disk does not), hold something (a mount point
/// left behind by a disk that is not mounted is empty) and sit on a volume
/// the system reports as mounted. Anything short of that proves nothing.
fn storage_present(ctx: &ScanCtx, gitdir: &Path) -> bool {
    let mut ancestors = gitdir.ancestors().skip(1);
    let existing = loop {
        match ancestors.next() {
            Some(dir) if dir.as_os_str().is_empty() => return false,
            Some(dir) => match std::fs::symlink_metadata(dir) {
                Ok(_) => break dir,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(_) => return false,
            },
            None => return false,
        }
    };
    let Ok(missing) = gitdir.strip_prefix(existing) else {
        return false;
    };
    // `..` after a folder that does not exist leads somewhere the system
    // could not follow either.
    if missing
        .components()
        .any(|c| !matches!(c, std::path::Component::Normal(_)))
    {
        return false;
    }
    let Ok(real) = std::fs::canonicalize(existing) else {
        return false;
    };
    let holds_something =
        std::fs::read_dir(&real).is_ok_and(|mut entries| entries.next().is_some());
    holds_something
        && ctx
            .os
            .volume_mounted(&real.join(missing), &ctx.mounts, &|p| p.is_dir())
}

/// A worktree git could not describe: shown, locked, never removable.
fn unknown(ctx: &ScanCtx, wt: &Path, label: String) -> Item {
    Item {
        id: ctx.next_id(),
        source: SourceId::AgentWorktrees,
        label,
        path: Some(wt.to_path_buf()),
        size: None,
        status: vec![Status::Detail("status unknown".into())],
        lock: Some("git did not answer".into()),
        safe: false,
        removal: Removal::Command {
            argv: vec!["git".into(), "worktree".into(), "list".into()],
            cwd: Some(wt.to_path_buf()),
        },
        age_days: None,
        recheck: crate::model::Recheck::default(),
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
    use crate::model::{LeftoverBranch, Removal, Status};
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

    #[test]
    fn home_folders_skipped_follow_the_platform() {
        use crate::platform::Os;
        let d = tempfile::tempdir().unwrap();
        let home = fs::canonicalize(d.path()).unwrap();
        for folder in ["Library", ".cache"] {
            let r = home.join(folder).join("r");
            repo(&r);
            let wt = home.join(folder).join("wt");
            git(
                &r,
                &["worktree", "add", "-q", wt.to_str().unwrap(), "-b", "feat"],
            );
        }
        let found = |os| -> Vec<PathBuf> {
            run_folder(&ctx(&home, &home, InUse::default()).with_os(os))
                .into_iter()
                .filter_map(|i| i.path)
                .collect()
        };
        assert_eq!(found(Os::Linux), [home.join("Library/wt")]);
        assert_eq!(found(Os::MacOs), [home.join(".cache/wt")]);
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
    fn ignored_files_that_are_not_artifacts_block_safe() {
        // `git worktree remove` deletes ignored files like `.env` without --force.
        let (_d, root, wt) = setup(true);
        fs::write(wt.join(".gitignore"), ".env\nnode_modules/\n").unwrap();
        git(&wt, &["add", ".gitignore"]);
        git(&wt, &["commit", "-q", "-m", "ignore"]);
        git(&root.join("r"), &["merge", "-q", "--ff-only", "feat"]);
        fs::write(wt.join(".env"), "SECRET=1").unwrap();
        let items = run_folder(&ctx(&root, &root, InUse::default()));
        assert!(items[0].status.contains(&Status::Clean));
        assert!(!items[0].safe);
        assert!(
            items[0].status.contains(&Status::Ignored(1)),
            "{:?}",
            items[0].status
        );
    }

    #[test]
    fn ignored_artifacts_alone_keep_a_merged_worktree_safe() {
        let (_d, root, wt) = setup(true);
        fs::write(wt.join(".gitignore"), "node_modules/\n").unwrap();
        git(&wt, &["add", ".gitignore"]);
        git(&wt, &["commit", "-q", "-m", "ignore"]);
        git(&root.join("r"), &["merge", "-q", "--ff-only", "feat"]);
        fs::create_dir_all(wt.join("node_modules/x")).unwrap();
        let items = run_folder(&ctx(&root, &root, InUse::default()));
        assert!(items[0].safe, "{:?}", items[0].status);
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
            Removal::Worktree {
                path: wt,
                repo: root.join("r"),
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
        assert!(items[0].lock.is_some(), "status unknown must be locked");
        assert!(matches!(&items[0].removal, Removal::Worktree { .. }));
    }

    #[test]
    fn agent_worktree_of_bare_repo_is_not_broken() {
        let d = tempfile::tempdir().unwrap();
        let home = fs::canonicalize(d.path()).unwrap();
        let r = home.join("r");
        repo(&r);
        let bare = home.join("bare.git");
        git(
            &home,
            &[
                "clone",
                "-q",
                "--bare",
                r.to_str().unwrap(),
                bare.to_str().unwrap(),
            ],
        );
        let root = home.join(".codex/worktrees");
        fs::create_dir_all(&root).unwrap();
        git(
            &bare,
            &[
                "worktree",
                "add",
                "-q",
                "--detach",
                root.join("w").to_str().unwrap(),
                "main",
            ],
        );
        let items = run_agent(
            &ctx(&home.join("elsewhere"), &home, InUse::default()),
            vec![root],
        );
        assert!(
            !items[0].status.contains(&Status::Broken),
            "{:?}",
            items[0].status
        );
        assert!(matches!(&items[0].removal, Removal::Worktree { repo, .. } if *repo == bare));
    }

    #[test]
    fn agent_worktree_with_failing_git_is_locked_unknown() {
        let d = tempfile::tempdir().unwrap();
        let home = fs::canonicalize(d.path()).unwrap();
        let r = home.join("r");
        repo(&r);
        let root = home.join(".codex/worktrees");
        fs::create_dir_all(&root).unwrap();
        let wt = root.join("w");
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
        // gitdir still exists, but its commondir points nowhere: git fails.
        fs::write(
            r.join(".git/worktrees/w/commondir"),
            "/nonexistent/devsweep\n",
        )
        .unwrap();
        let items = run_agent(
            &ctx(&home.join("elsewhere"), &home, InUse::default()),
            vec![root],
        );
        assert!(!items[0].status.contains(&Status::Broken));
        assert!(items[0].lock.is_some());
        assert!(!items[0].safe);
        assert!(!matches!(items[0].removal, Removal::RemoveDir(_)));
    }

    /// An unmounted disk leaves the `gitdir:` target missing without the
    /// repository being gone: the worktree must never become a folder to
    /// delete.
    #[test]
    fn worktree_of_a_repo_on_an_unmounted_linux_disk_is_unknown_not_broken() {
        let d = tempfile::tempdir().unwrap();
        let home = fs::canonicalize(d.path()).unwrap();
        let agent_root = home.join(".codex/worktrees");
        let wt = agent_root.join("w1");
        fs::create_dir_all(&wt).unwrap();
        fs::write(
            wt.join(".git"),
            "gitdir: /mnt/devsweep-absent-disk/r/.git/worktrees/w1\n",
        )
        .unwrap();
        let scan = |os| {
            run_agent(
                &ctx(&home.join("elsewhere"), &home, InUse::default()).with_os(os),
                vec![agent_root.clone()],
            )
        };
        let items = scan(crate::platform::Os::Linux);
        assert_eq!(items.len(), 1);
        assert!(!items[0].status.contains(&Status::Broken));
        assert!(items[0].lock.is_some());
        assert!(!matches!(items[0].removal, Removal::RemoveDir(_)));
    }

    /// A worktree folder under an agent root whose `.git` file holds `gitdir`.
    fn orphan(agent_root: &Path, gitdir: &str) -> PathBuf {
        let wt = agent_root.join("w1");
        fs::create_dir_all(&wt).unwrap();
        fs::write(wt.join(".git"), format!("gitdir: {gitdir}\n")).unwrap();
        wt
    }

    fn scan_orphan(home: &Path, agent_root: &Path) -> Item {
        let mut items = run_agent(
            &ctx(&home.join("elsewhere"), home, InUse::default()),
            vec![agent_root.to_path_buf()],
        );
        assert_eq!(items.len(), 1);
        items.remove(0)
    }

    fn assert_unknown(item: &Item) {
        assert!(!item.status.contains(&Status::Broken), "{:?}", item.status);
        assert!(item.lock.is_some());
        assert!(!matches!(item.removal, Removal::RemoveDir(_)));
    }

    /// A mount point left empty by a disk that is not mounted (an fstab or
    /// encrypted volume at `/data`, an sshfs folder in the home folder) is
    /// no proof that the repository is gone, wherever it is.
    #[test]
    fn worktree_of_a_repo_under_an_empty_mount_point_is_unknown_not_broken() {
        let d = tempfile::tempdir().unwrap();
        let home = fs::canonicalize(d.path()).unwrap();
        let mount_point = home.join("data");
        fs::create_dir_all(&mount_point).unwrap();
        let agent_root = home.join(".codex/worktrees");
        let gitdir = mount_point.join("r/.git/worktrees/w1");
        orphan(&agent_root, gitdir.to_str().unwrap());
        assert_unknown(&scan_orphan(&home, &agent_root));
    }

    /// `git worktree add --relative-paths` writes a relative `gitdir:`; it
    /// must be judged by where it leads, not by how it is spelled.
    #[test]
    fn relative_gitdir_into_an_empty_mount_point_is_unknown_not_broken() {
        let d = tempfile::tempdir().unwrap();
        let home = fs::canonicalize(d.path()).unwrap();
        fs::create_dir_all(home.join("data")).unwrap();
        let agent_root = home.join(".codex/worktrees");
        orphan(&agent_root, "../../../data/r/.git/worktrees/w1");
        assert_unknown(&scan_orphan(&home, &agent_root));
    }

    /// A `gitdir:` that walks back up after a missing folder cannot be
    /// followed, so nothing is known about where it ends.
    #[test]
    fn gitdir_with_dots_after_a_missing_folder_is_unknown_not_broken() {
        let d = tempfile::tempdir().unwrap();
        let home = fs::canonicalize(d.path()).unwrap();
        fs::write(home.join("file"), "x").unwrap();
        let agent_root = home.join(".codex/worktrees");
        let gitdir = home.join("missing/../data/r/.git/worktrees/w1");
        orphan(&agent_root, gitdir.to_str().unwrap());
        assert_unknown(&scan_orphan(&home, &agent_root));
    }

    /// A symlink to a disk that is gone dangles: the repository behind it
    /// may be intact.
    #[cfg(unix)]
    #[test]
    fn gitdir_through_a_dangling_symlink_is_unknown_not_broken() {
        let d = tempfile::tempdir().unwrap();
        let home = fs::canonicalize(d.path()).unwrap();
        fs::write(home.join("file"), "x").unwrap();
        std::os::unix::fs::symlink(home.join("unplugged"), home.join("code")).unwrap();
        let agent_root = home.join(".codex/worktrees");
        let gitdir = home.join("code/r/.git/worktrees/w1");
        orphan(&agent_root, gitdir.to_str().unwrap());
        assert_unknown(&scan_orphan(&home, &agent_root));
    }

    /// The repository was deleted from a folder that still holds other
    /// things: that folder is certainly there, so the worktree is broken.
    #[test]
    fn worktree_of_a_repo_deleted_from_a_folder_in_use_is_broken() {
        let d = tempfile::tempdir().unwrap();
        let home = fs::canonicalize(d.path()).unwrap();
        fs::create_dir_all(home.join("code/other-project")).unwrap();
        let agent_root = home.join(".codex/worktrees");
        let gitdir = home.join("code/r/.git/worktrees/w1");
        let wt = orphan(&agent_root, gitdir.to_str().unwrap());
        let item = scan_orphan(&home, &agent_root);
        assert_eq!(item.status, vec![Status::Broken]);
        assert_eq!(item.removal, Removal::RemoveDir(wt));
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
    fn folder_scan_files_agent_root_worktrees_under_agent_worktrees() {
        // Scanning $HOME finds the main repo, whose worktrees live in an
        // agent root: they belong to Agent worktrees, not This folder.
        let d = tempfile::tempdir().unwrap();
        let home = fs::canonicalize(d.path()).unwrap();
        let r = home.join("r");
        repo(&r);
        let orca = home.join("orca/workspaces/site");
        fs::create_dir_all(&orca).unwrap();
        let agent_wt = orca.join("arowana");
        let local_wt = home.join("wt");
        for wt in [&agent_wt, &local_wt] {
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
        }
        let items = run_folder(&ctx(&home, &home, InUse::default()));
        let agent = items
            .iter()
            .find(|i| i.path.as_ref() == Some(&agent_wt))
            .unwrap();
        assert_eq!(agent.source, SourceId::AgentWorktrees);
        assert_eq!(agent.label, "orca/site/arowana");
        assert!(matches!(&agent.removal, Removal::Worktree { repo, .. } if *repo == r));
        let local = items
            .iter()
            .find(|i| i.path.as_ref() == Some(&local_wt))
            .unwrap();
        assert_eq!(local.source, SourceId::Worktrees);
        assert_eq!(local.label, "wt");
    }

    #[test]
    fn unreadable_gitdir_is_unknown_not_broken() {
        // A main repo in a folder we cannot read (privacy prompt, unmounted
        // volume) must never turn its worktree into a deletable folder.
        let d = tempfile::tempdir().unwrap();
        let home = fs::canonicalize(d.path()).unwrap();
        let private = home.join("private");
        let r = private.join("r");
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
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&private, fs::Permissions::from_mode(0o000)).unwrap();
        let items = run_agent(
            &ctx(&home.join("elsewhere"), &home, InUse::default()),
            vec![agent_root],
        );
        fs::set_permissions(&private, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(items.len(), 1);
        assert!(
            !items[0].status.contains(&Status::Broken),
            "{:?}",
            items[0].status
        );
        assert!(!matches!(items[0].removal, Removal::RemoveDir(_)));
        assert!(items[0].lock.is_some());
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

    fn rev(dir: &Path, name: &str) -> String {
        let out = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["rev-parse", name])
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// Land `feat` on `main` as a single commit, the way a squash merge does.
    fn squash_merge(r: &Path) {
        git(r, &["merge", "-q", "--squash", "feat"]);
        git(r, &["commit", "-q", "-m", "feat (squashed)"]);
    }

    #[test]
    fn branch_merged_into_the_default_branch_is_a_leftover() {
        let (_d, root, wt) = setup(true);
        let r = root.join("r");
        assert_eq!(
            leftover_branch(&wt, &r),
            Some(LeftoverBranch {
                repo: r.clone(),
                branch: "feat".into(),
                head: rev(&r, "feat"),
            })
        );
    }

    #[test]
    fn unmerged_branch_is_never_a_leftover() {
        let (_d, root, wt) = setup(false);
        assert_eq!(leftover_branch(&wt, &root.join("r")), None);
    }

    #[test]
    fn squash_merged_branch_is_a_leftover() {
        let (_d, root, wt) = setup(false);
        let r = root.join("r");
        squash_merge(&r);
        assert_eq!(
            leftover_branch(&wt, &r).map(|b| b.branch),
            Some("feat".into())
        );
    }

    #[test]
    fn squash_merged_branch_with_newer_commits_is_not_a_leftover() {
        // Work added after the squash merge exists nowhere else.
        let (_d, root, wt) = setup(false);
        let r = root.join("r");
        squash_merge(&r);
        fs::write(wt.join("later.txt"), "later").unwrap();
        git(&wt, &["add", "."]);
        git(&wt, &["commit", "-q", "-m", "later"]);
        assert_eq!(leftover_branch(&wt, &r), None);
    }

    #[test]
    fn detached_worktree_leaves_no_branch_behind() {
        let d = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(d.path()).unwrap();
        let r = root.join("r");
        repo(&r);
        let wt = root.join("wt");
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
        assert_eq!(leftover_branch(&wt, &r), None);
    }

    #[test]
    fn the_default_branch_is_never_a_leftover() {
        // A bare repository keeps `main` in a worktree like any other branch.
        let d = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(d.path()).unwrap();
        let r = root.join("r");
        repo(&r);
        let bare = root.join("bare.git");
        git(
            &root,
            &[
                "clone",
                "-q",
                "--bare",
                r.to_str().unwrap(),
                bare.to_str().unwrap(),
            ],
        );
        let wt = root.join("wt");
        git(
            &bare,
            &["worktree", "add", "-q", wt.to_str().unwrap(), "main"],
        );
        assert_eq!(leftover_branch(&wt, &bare), None);
    }
}
