//! Disk usage and time helpers.

use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::time::SystemTime;

/// Bytes actually used on disk under `path` (like `du`), never following symlinks.
pub fn dir_size(path: &Path) -> u64 {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => return 0,
        Ok(_) => {}
        Err(_) => return 0,
    }
    ignore::WalkBuilder::new(path)
        .standard_filters(false)
        .follow_links(false)
        .build()
        .filter_map(Result::ok)
        .filter_map(|entry| entry.metadata().ok())
        .map(|meta| meta.blocks() * 512)
        .sum()
}

/// Whole days elapsed since `t` (0 for times in the future).
pub fn days_since(t: SystemTime) -> u32 {
    SystemTime::now()
        .duration_since(t)
        .map(|d| (d.as_secs() / 86_400) as u32)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn dir_size_counts_nested_files() {
        let d = tempfile::tempdir().unwrap();
        fs::create_dir_all(d.path().join("a/b")).unwrap();
        fs::write(d.path().join("a/one"), vec![1u8; 10_000]).unwrap();
        fs::write(d.path().join("a/b/two"), vec![1u8; 10_000]).unwrap();
        assert!(dir_size(d.path()) >= 20_000);
    }

    #[test]
    fn dir_size_does_not_follow_symlinks() {
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("big"), vec![1u8; 1_000_000]).unwrap();
        let d = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), d.path().join("link")).unwrap();
        assert!(dir_size(d.path()) < 100_000);
    }

    #[test]
    fn dir_size_of_missing_path_is_zero() {
        assert_eq!(dir_size(std::path::Path::new("/nonexistent/devsweep")), 0);
    }

    #[test]
    fn days_since_counts_whole_days() {
        let t = SystemTime::now() - std::time::Duration::from_secs(3 * 86_400 + 60);
        assert_eq!(days_since(t), 3);
    }
}
