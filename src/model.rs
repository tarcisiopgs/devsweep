//! Core data model shared by scanners, the removal executor and the UI.

use std::path::PathBuf;

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
}

impl SourceId {
    pub const ALL: [SourceId; 9] = [
        SourceId::Artifacts,
        SourceId::Worktrees,
        SourceId::AgentWorktrees,
        SourceId::Ios,
        SourceId::Android,
        SourceId::Docker,
        SourceId::DevCaches,
        SourceId::Xcode,
        SourceId::Homebrew,
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
    /// Run a native tool command.
    Command {
        argv: Vec<String>,
        cwd: Option<PathBuf>,
    },
}

impl Removal {
    /// Human-readable command shown on the review screen.
    pub fn describe(&self) -> String {
        match self {
            Removal::RemoveDir(p) => format!("rm -rf {}", quote(&p.to_string_lossy())),
            Removal::ClearDir(p) => format!("rm -rf {}/*", quote(&p.to_string_lossy())),
            Removal::RemovePaths(paths) => {
                let paths: Vec<String> =
                    paths.iter().map(|p| quote(&p.to_string_lossy())).collect();
                format!("rm -rf {}", paths.join(" "))
            }
            Removal::Command { argv, cwd } => {
                let cmd = argv.iter().map(|a| quote(a)).collect::<Vec<_>>().join(" ");
                match cwd {
                    Some(dir) => format!("(cd {}) {}", quote(&dir.to_string_lossy()), cmd),
                    None => cmd,
                }
            }
        }
    }
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
    use std::path::PathBuf;

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
        assert_eq!(r.describe(), "rm '/a b'");
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
