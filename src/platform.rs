//! What differs between macOS, Linux and Windows, as plain functions of
//! [`Os`]. Taking the system as a value keeps every platform's rules
//! testable on any of them.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::model::SourceId;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Os {
    MacOs,
    Linux,
    Windows,
}

/// Environment lookup, injectable in tests.
pub type Env<'a> = &'a dyn Fn(&str) -> Option<OsString>;

/// The real environment of this process.
pub fn process_env(key: &str) -> Option<OsString> {
    std::env::var_os(key)
}

impl Os {
    pub const ALL: [Os; 3] = [Os::MacOs, Os::Linux, Os::Windows];

    /// The system this binary was built for.
    pub fn current() -> Os {
        if cfg!(target_os = "macos") {
            Os::MacOs
        } else if cfg!(windows) {
            Os::Windows
        } else {
            Os::Linux
        }
    }

    /// Whether the source exists on this system at all.
    pub fn has_source(self, source: SourceId) -> bool {
        match source {
            SourceId::Ios | SourceId::Xcode | SourceId::Homebrew => self == Os::MacOs,
            _ => true,
        }
    }

    /// The user's home folder, from the variable this system sets.
    pub fn home_dir(self, env: Env) -> Option<PathBuf> {
        let key = match self {
            Os::MacOs | Os::Linux => "HOME",
            Os::Windows => "USERPROFILE",
        };
        env(key).filter(|v| !v.is_empty()).map(PathBuf::from)
    }

    /// Folders the guard never removes: the home folder and the ones that
    /// hold every tool's data on this system.
    pub fn protected_dirs(self, home: &Path, env: Env) -> Vec<PathBuf> {
        let (inside, overrides): (&[&str], &[&str]) = match self {
            Os::MacOs => (
                &[
                    "Library",
                    "Library/Caches",
                    "Library/Developer",
                    "Library/Logs",
                ],
                &[],
            ),
            Os::Linux => (
                &[
                    ".cache",
                    ".config",
                    ".local",
                    ".local/share",
                    ".local/state",
                ],
                &[
                    "XDG_CACHE_HOME",
                    "XDG_CONFIG_HOME",
                    "XDG_DATA_HOME",
                    "XDG_STATE_HOME",
                ],
            ),
            Os::Windows => (
                &[
                    "AppData",
                    "AppData/Local",
                    "AppData/LocalLow",
                    "AppData/Roaming",
                    "AppData/Local/Temp",
                ],
                &["LOCALAPPDATA", "APPDATA"],
            ),
        };
        std::iter::once(home.to_path_buf())
            .chain(inside.iter().map(|rel| home.join(rel)))
            .chain(
                overrides
                    .iter()
                    .filter_map(|key| env(key))
                    .filter(|v| !v.is_empty())
                    .map(PathBuf::from),
            )
            .collect()
    }

    /// Roots outside the home folder where removals are still allowed: the
    /// per-user cache folder, when the system keeps it elsewhere. A value
    /// that is relative, or that is the home folder or above it, would
    /// widen the guard instead and is dropped.
    pub fn guard_extra_roots(
        self,
        home: &Path,
        env: Env,
        darwin_cache: Option<PathBuf>,
    ) -> Vec<PathBuf> {
        let candidate = match self {
            Os::MacOs => darwin_cache,
            Os::Linux => env("XDG_CACHE_HOME").map(PathBuf::from),
            Os::Windows => None,
        };
        candidate
            .filter(|p| p.is_absolute() && !home.starts_with(p))
            .into_iter()
            .collect()
    }

    /// Folders directly inside the home folder that the repository walk
    /// does not enter.
    pub fn skip_at_home(self) -> &'static [&'static str] {
        match self {
            Os::MacOs => &["Library", ".Trash"],
            Os::Linux => &[".cache"],
            Os::Windows => &["AppData"],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::SourceId;
    use std::ffi::OsString;
    use std::path::{Path, PathBuf};

    fn env_of(pairs: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<OsString> {
        move |key| {
            pairs
                .iter()
                .find(|(k, _)| *k == key)
                .map(|(_, v)| OsString::from(v))
        }
    }

    #[test]
    fn apple_only_sources_exist_only_on_macos() {
        for os in Os::ALL {
            for source in SourceId::ALL {
                let apple = matches!(source, SourceId::Ios | SourceId::Xcode | SourceId::Homebrew);
                assert_eq!(
                    os.has_source(source),
                    !apple || os == Os::MacOs,
                    "{os:?} {source:?}"
                );
            }
        }
    }

    #[test]
    fn home_comes_from_userprofile_on_windows() {
        let env = env_of(&[("USERPROFILE", "C:\\Users\\u")]);
        assert_eq!(
            Os::Windows.home_dir(&env),
            Some(PathBuf::from("C:\\Users\\u"))
        );
        assert_eq!(Os::Linux.home_dir(&env), None);
        assert_eq!(Os::MacOs.home_dir(&env), None);
    }

    #[test]
    fn home_comes_from_home_on_unix() {
        let env = env_of(&[("HOME", "/home/u")]);
        assert_eq!(Os::Linux.home_dir(&env), Some(PathBuf::from("/home/u")));
        assert_eq!(Os::MacOs.home_dir(&env), Some(PathBuf::from("/home/u")));
        assert_eq!(Os::Windows.home_dir(&env), None);
    }

    #[test]
    fn an_empty_home_variable_is_no_home() {
        let env = env_of(&[("HOME", "")]);
        assert_eq!(Os::Linux.home_dir(&env), None);
    }

    #[test]
    fn macos_protected_dirs_are_unchanged() {
        let home = Path::new("/Users/u");
        let env = env_of(&[]);
        assert_eq!(
            Os::MacOs.protected_dirs(home, &env),
            [
                "/Users/u",
                "/Users/u/Library",
                "/Users/u/Library/Caches",
                "/Users/u/Library/Developer",
                "/Users/u/Library/Logs",
            ]
            .map(PathBuf::from)
        );
    }

    #[test]
    fn linux_protects_the_xdg_folders() {
        let home = Path::new("/home/u");
        let env = env_of(&[]);
        assert_eq!(
            Os::Linux.protected_dirs(home, &env),
            [
                "/home/u",
                "/home/u/.cache",
                "/home/u/.config",
                "/home/u/.local",
                "/home/u/.local/share",
                "/home/u/.local/state",
            ]
            .map(PathBuf::from)
        );
    }

    #[test]
    fn linux_protects_xdg_overrides() {
        let home = Path::new("/home/u");
        let env = env_of(&[("XDG_DATA_HOME", "/data/xdg"), ("XDG_CACHE_HOME", "/c")]);
        let dirs = Os::Linux.protected_dirs(home, &env);
        assert!(dirs.contains(&PathBuf::from("/data/xdg")));
        assert!(dirs.contains(&PathBuf::from("/c")));
        // The defaults stay protected next to the overrides.
        assert!(dirs.contains(&PathBuf::from("/home/u/.local/share")));
    }

    #[test]
    fn windows_protects_appdata() {
        let home = Path::new("/u");
        let env = env_of(&[("LOCALAPPDATA", "/elsewhere/Local")]);
        let dirs = Os::Windows.protected_dirs(home, &env);
        for rel in [
            "AppData",
            "AppData/Local",
            "AppData/LocalLow",
            "AppData/Roaming",
            "AppData/Local/Temp",
        ] {
            assert!(dirs.contains(&home.join(rel)), "{rel}");
        }
        assert!(dirs.contains(&PathBuf::from("/elsewhere/Local")));
        assert!(dirs.contains(&home.to_path_buf()));
    }

    #[test]
    fn extra_root_above_home_is_dropped() {
        let home = Path::new("/home/u");
        for above in ["/", "/home", "/home/u"] {
            let env = move |k: &str| (k == "XDG_CACHE_HOME").then(|| OsString::from(above));
            assert!(
                Os::Linux.guard_extra_roots(home, &env, None).is_empty(),
                "{above}"
            );
        }
        let env = env_of(&[("XDG_CACHE_HOME", "/var/cache/u")]);
        assert_eq!(
            Os::Linux.guard_extra_roots(home, &env, None),
            [PathBuf::from("/var/cache/u")]
        );
    }

    #[test]
    fn relative_extra_root_is_dropped() {
        let home = Path::new("/home/u");
        let env = env_of(&[("XDG_CACHE_HOME", "cache")]);
        assert!(Os::Linux.guard_extra_roots(home, &env, None).is_empty());
        assert!(
            Os::MacOs
                .guard_extra_roots(Path::new("/Users/u"), &env, Some(PathBuf::from("T")))
                .is_empty()
        );
    }

    #[test]
    fn macos_extra_root_is_the_darwin_cache_dir() {
        let home = Path::new("/Users/u");
        let env = env_of(&[("XDG_CACHE_HOME", "/var/cache/u")]);
        let cache = PathBuf::from("/private/var/folders/ab/C");
        assert_eq!(
            Os::MacOs.guard_extra_roots(home, &env, Some(cache.clone())),
            std::slice::from_ref(&cache)
        );
        // Neither of the other systems has one.
        assert!(
            Os::Windows
                .guard_extra_roots(home, &env, Some(cache))
                .is_empty()
        );
    }

    #[test]
    fn home_folders_skipped_by_the_walk() {
        assert_eq!(Os::MacOs.skip_at_home(), ["Library", ".Trash"]);
        assert_eq!(Os::Linux.skip_at_home(), [".cache"]);
        assert_eq!(Os::Windows.skip_at_home(), ["AppData"]);
    }
}
