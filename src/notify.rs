//! Desktop notification when a removal finishes.

use std::process::{Command, Stdio};

use crate::platform::Os;

/// `osascript` argv that shows `body` under `title`. Both travel as script
/// arguments, never inside the AppleScript source, so no quoting is needed.
pub fn osascript_argv(title: &str, body: &str) -> Vec<String> {
    [
        "osascript",
        "-e",
        "on run argv",
        "-e",
        "display notification (item 2 of argv) with title (item 1 of argv)",
        "-e",
        "end run",
        title,
        body,
    ]
    .into_iter()
    .map(String::from)
    .collect()
}

/// The command that shows a desktop notification on `os`, when it has one:
/// `osascript` on macOS, `notify-send` on Linux when installed. Windows has
/// no such command; the terminal bell is all it gets.
pub fn argv(
    os: Os,
    has_bin: &dyn Fn(&str) -> bool,
    title: &str,
    body: &str,
) -> Option<Vec<String>> {
    match os {
        Os::MacOs => Some(osascript_argv(title, body)),
        // `--` so a text starting with a dash is never read as an option.
        Os::Linux if has_bin("notify-send") => Some(
            ["notify-send", "--", title, body]
                .into_iter()
                .map(String::from)
                .collect(),
        ),
        Os::Linux | Os::Windows => None,
    }
}

/// Post the notification without waiting for it. A missing or failing
/// tool is ignored: the Done screen already says the same thing.
pub fn post(title: &str, body: &str) {
    let has_bin = |bin: &str| crate::scan::which(bin).is_some();
    let Some(argv) = argv(Os::current(), &has_bin, title, body) else {
        return;
    };
    let _ = Command::new(&argv[0])
        .args(&argv[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_travels_as_arguments_not_script_source() {
        let argv = osascript_argv("devsweep", r#"Freed "1 GB" \ end run"#);
        assert_eq!(argv[0], "osascript");
        assert_eq!(argv[argv.len() - 2], "devsweep");
        assert_eq!(argv[argv.len() - 1], r#"Freed "1 GB" \ end run"#);
        let script: Vec<&String> = argv.iter().skip(1).step_by(2).take(3).collect();
        assert!(script.iter().all(|l| !l.contains("Freed")));
    }

    #[test]
    fn macos_uses_osascript() {
        assert_eq!(
            argv(Os::MacOs, &|_| false, "devsweep", "done"),
            Some(osascript_argv("devsweep", "done"))
        );
    }

    #[test]
    fn linux_uses_notify_send_only_when_installed() {
        assert_eq!(
            argv(Os::Linux, &|bin| bin == "notify-send", "devsweep", "done"),
            Some(vec![
                "notify-send".to_string(),
                "--".to_string(),
                "devsweep".to_string(),
                "done".to_string()
            ])
        );
        assert_eq!(argv(Os::Linux, &|_| false, "devsweep", "done"), None);
    }

    #[test]
    fn windows_posts_nothing() {
        assert_eq!(argv(Os::Windows, &|_| true, "devsweep", "done"), None);
    }
}
