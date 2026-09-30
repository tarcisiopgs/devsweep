//! Native macOS notification when a removal finishes.

use std::process::{Command, Stdio};

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

/// Post the notification without waiting for it. A missing or failing
/// `osascript` is ignored: the Done screen already says the same thing.
pub fn post(title: &str, body: &str) {
    let argv = osascript_argv(title, body);
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
}
