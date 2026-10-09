//! Core data model shared by scanners, the removal executor and the UI.

use std::path::{Path, PathBuf};

use crate::platform::Os;

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Section {
    Folder,
    Machine,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, PartialOrd, Ord)]
pub enum SourceId {
    Artifacts,
    Worktrees,
    AgentWorktrees,
    Ios,
    Android,
    Docker,
    DevCaches,
    Xcode,
    Homebrew,
    Trash,
    /// Branches left behind by removed worktrees. Never scanned: offered as
    /// a follow-up once the removal is done.
    Branches,
}

impl SourceId {
    pub const ALL: [SourceId; 11] = [
        SourceId::Artifacts,
        SourceId::Worktrees,
        SourceId::AgentWorktrees,
        SourceId::Ios,
        SourceId::Android,
        SourceId::Docker,
        SourceId::DevCaches,
        SourceId::Xcode,
        SourceId::Homebrew,
        SourceId::Trash,
        SourceId::Branches,
    ];

    pub fn section(self) -> Section {
        match self {
            SourceId::Artifacts | SourceId::Worktrees => Section::Folder,
            _ => Section::Machine,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            SourceId::Artifacts => "Artifacts",
            SourceId::Worktrees => "Worktrees",
            SourceId::AgentWorktrees => "Agent worktrees",
            SourceId::Ios => "iOS",
            SourceId::Android => "Android",
            SourceId::Docker => "Docker",
            SourceId::DevCaches => "Dev caches",
            SourceId::Xcode => "Xcode",
            SourceId::Homebrew => "Homebrew",
            SourceId::Trash => "Trash",
            SourceId::Branches => "Branches",
        }
    }
}

pub type ItemId = u64;

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Status {
    Merged,
    Clean,
    Dirty(u32),
    /// Git-ignored files that are not build artifacts (`.env`, local DBs):
    /// `git worktree remove` deletes them without asking.
    Ignored(u32),
    Stale(u32),
    Broken,
    Booted,
    Running,
    Unavailable,
    Orphan,
    LastUsed(u32),
    Detail(String),
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Removal {
    /// Delete the directory itself.
    RemoveDir(PathBuf),
    /// Delete the directory contents, keeping the directory.
    ClearDir(PathBuf),
    /// Delete several folders or files, each one checked by the guard first.
    RemovePaths(Vec<PathBuf>),
    /// `git worktree remove`, run in the repository that owns the worktree.
    Worktree { path: PathBuf, repo: PathBuf },
    /// Run a native tool command.
    Command {
        argv: Vec<String>,
        cwd: Option<PathBuf>,
    },
}

impl Removal {
    /// Human-readable command shown on the review screen, the way a user
    /// of `os` would type it: a POSIX shell, or PowerShell on Windows.
    pub fn describe_for(&self, os: Os) -> String {
        match os {
            Os::MacOs | Os::Linux => self.describe_sh(),
            Os::Windows => self.describe_powershell(),
        }
    }

    fn describe_powershell(&self) -> String {
        let path = |p: &Path| quote_powershell(&p.to_string_lossy());
        match self {
            Removal::RemoveDir(p) => format!("Remove-Item -Recurse -Force {}", path(p)),
            Removal::ClearDir(p) => {
                // Spelled out, not joined: this text is Windows' own even
                // when another system renders it.
                let inside = format!("{}\\*", p.to_string_lossy().trim_end_matches('\\'));
                format!("Remove-Item -Recurse -Force {}", quote_powershell(&inside))
            }
            Removal::RemovePaths(paths) => {
                let paths: Vec<String> = paths.iter().map(|p| path(p)).collect();
                format!("Remove-Item -Recurse -Force {}", paths.join(", "))
            }
            Removal::Worktree { path: wt, repo } => {
                format!("cd {}; git worktree remove {}", path(repo), path(wt))
            }
            Removal::Command { argv, cwd } => {
                let cmd = argv
                    .iter()
                    .map(
                        |arg| match arg.chars().any(|c| c.is_whitespace() || c == '\'') {
                            true => quote_powershell(arg),
                            false => arg.clone(),
                        },
                    )
                    .collect::<Vec<_>>()
                    .join(" ");
                match cwd {
                    Some(dir) => format!("cd {}; {cmd}", path(dir)),
                    None => cmd,
                }
            }
        }
    }

    fn describe_sh(&self) -> String {
        match self {
            Removal::RemoveDir(p) => format!("rm -rf {}", quote(&p.to_string_lossy())),
            Removal::ClearDir(p) => format!("rm -rf {}/*", quote(&p.to_string_lossy())),
            Removal::RemovePaths(paths) => {
                let paths: Vec<String> =
                    paths.iter().map(|p| quote(&p.to_string_lossy())).collect();
                format!("rm -rf {}", paths.join(" "))
            }
            Removal::Worktree { path, repo } => format!(
                "(cd {} && git worktree remove {})",
                quote(&repo.to_string_lossy()),
                quote(&path.to_string_lossy())
            ),
            Removal::Command { argv, cwd } => {
                let cmd = argv.iter().map(|a| quote(a)).collect::<Vec<_>>().join(" ");
                match cwd {
                    Some(dir) => format!("(cd {} && {})", quote(&dir.to_string_lossy()), cmd),
                    None => cmd,
                }
            }
        }
    }
}

/// PowerShell quotes with single quotes, and a single quote inside is doubled.
fn quote_powershell(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

fn quote(s: &str) -> String {
    if s.chars().any(|c| c.is_whitespace() || c == '\'') {
        format!("'{}'", s.replace('\'', "'\\''"))
    } else {
        s.to_string()
    }
}

/// What to check again right before removing an item.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Recheck {
    /// Folder whose live processes lock the item (defaults to its path).
    pub scope: Option<PathBuf>,
    /// Process names that lock the item while running.
    pub busy: Vec<String>,
    /// Command-line fragments of a process that locks the item (a running
    /// emulator: `-avd Pixel_8`).
    pub args: Vec<String>,
    /// A command whose output, when it contains the needle, means the item
    /// is in use (`xcrun simctl list devices booted` and a device UDID).
    /// A failing command counts as in use.
    pub probe: Option<(Vec<String>, String)>,
    /// A command whose trimmed output must still be this value (a branch
    /// still at the commit that was verified as merged). A failing command
    /// counts as changed.
    pub expect: Option<(Vec<String>, String)>,
}

/// A branch whose worktree was removed and whose work is already in the
/// default branch, so deleting it loses nothing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LeftoverBranch {
    /// The repository (or bare repository) that owns the branch.
    pub repo: PathBuf,
    pub branch: String,
    /// The commit the branch pointed at when it was verified.
    pub head: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Item {
    pub id: ItemId,
    pub source: SourceId,
    pub label: String,
    pub path: Option<PathBuf>,
    pub size: Option<u64>,
    pub status: Vec<Status>,
    pub lock: Option<String>,
    pub safe: bool,
    pub removal: Removal,
    pub age_days: Option<u32>,
    pub recheck: Recheck,
}

impl Item {
    pub fn selectable(&self) -> bool {
        self.lock.is_none()
    }

    pub fn preselected(&self) -> bool {
        self.safe && self.selectable()
    }
}

const UNITS: [&str; 5] = ["B", "K", "M", "G", "T"];

fn scale(bytes: u64) -> (f64, usize) {
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1000.0 && unit < UNITS.len() - 1 {
        value /= 1000.0;
        unit += 1;
    }
    (value, unit)
}

/// The one size notation of the UI: `1.9 GB`, `412 MB`, `0 B` (base 1000).
pub fn format_size_long(bytes: u64) -> String {
    let (value, unit) = scale(bytes);
    let suffix = if unit == 0 {
        "B".to_string()
    } else {
        format!("{}B", UNITS[unit])
    };
    if unit >= 3 {
        format!("{:.1} {}", value, suffix)
    } else {
        format!("{} {}", value.round() as u64, suffix)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item() -> Item {
        Item {
            id: 1,
            source: SourceId::Artifacts,
            label: "x".into(),
            path: Some(PathBuf::from("/tmp/x")),
            size: None,
            status: vec![],
            lock: None,
            safe: false,
            removal: Removal::RemoveDir(PathBuf::from("/tmp/x")),
            age_days: None,
            recheck: crate::model::Recheck::default(),
        }
    }

    #[test]
    fn format_size_long_form() {
        assert_eq!(format_size_long(1_900_000_000), "1.9 GB");
        assert_eq!(format_size_long(412_000_000), "412 MB");
        assert_eq!(format_size_long(0), "0 B");
    }

    #[test]
    fn locked_item_is_never_selectable_nor_preselected() {
        let it = Item {
            lock: Some("claude · PID 1".into()),
            safe: true,
            ..item()
        };
        assert!(!it.selectable());
        assert!(!it.preselected());
    }

    #[test]
    fn safe_unlocked_item_is_preselected() {
        assert!(
            Item {
                safe: true,
                ..item()
            }
            .preselected()
        );
    }

    #[test]
    fn describe_command_quotes_args_with_spaces() {
        let r = Removal::Command {
            argv: vec!["rm".into(), "/a b".into()],
            cwd: None,
        };
        assert_eq!(r.describe_for(Os::MacOs), "rm '/a b'");
        assert_eq!(r.describe_for(Os::Linux), "rm '/a b'");
    }

    #[test]
    fn unix_describes_removals_as_rm() {
        for os in [Os::MacOs, Os::Linux] {
            assert_eq!(
                Removal::RemoveDir("/a/b c".into()).describe_for(os),
                "rm -rf '/a/b c'"
            );
            assert_eq!(
                Removal::ClearDir("/a/b".into()).describe_for(os),
                "rm -rf /a/b/*"
            );
        }
    }

    /// The review shows what a Windows user could type: PowerShell, with
    /// its own quoting (a single quote is doubled, never backslashed).
    #[test]
    fn windows_describes_removals_as_powershell() {
        let os = Os::Windows;
        assert_eq!(
            Removal::RemoveDir("C:\\Users\\u\\my app\\node_modules".into()).describe_for(os),
            "Remove-Item -Recurse -Force 'C:\\Users\\u\\my app\\node_modules'"
        );
        assert_eq!(
            Removal::RemoveDir("C:\\it's\\target".into()).describe_for(os),
            "Remove-Item -Recurse -Force 'C:\\it''s\\target'"
        );
        assert_eq!(
            Removal::ClearDir("C:\\cache".into()).describe_for(os),
            "Remove-Item -Recurse -Force 'C:\\cache\\*'"
        );
        assert_eq!(
            Removal::RemovePaths(vec!["C:\\a".into(), "C:\\b c".into()]).describe_for(os),
            "Remove-Item -Recurse -Force 'C:\\a', 'C:\\b c'"
        );
        assert_eq!(
            Removal::Worktree {
                path: "C:\\wt".into(),
                repo: "C:\\my repo".into()
            }
            .describe_for(os),
            "cd 'C:\\my repo'; git worktree remove 'C:\\wt'"
        );
        assert_eq!(
            Removal::Command {
                argv: vec!["docker".into(), "volume".into(), "rm".into(), "a b".into()],
                cwd: None
            }
            .describe_for(os),
            "docker volume rm 'a b'"
        );
        assert_eq!(
            Removal::Command {
                argv: vec!["git".into(), "branch".into(), "-D".into(), "feat".into()],
                cwd: Some("C:\\r".into())
            }
            .describe_for(os),
            "cd 'C:\\r'; git branch -D feat"
        );
    }

    #[test]
    fn source_sections_and_labels() {
        assert_eq!(SourceId::Artifacts.section(), Section::Folder);
        assert_eq!(SourceId::Worktrees.section(), Section::Folder);
        assert_eq!(SourceId::DevCaches.section(), Section::Machine);
        assert_eq!(SourceId::AgentWorktrees.label(), "Agent worktrees");
        assert_eq!(SourceId::DevCaches.label(), "Dev caches");
    }
}
