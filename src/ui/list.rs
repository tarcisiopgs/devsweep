//! Main screen: source list, item list and status bar.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};

use crate::app::{App, Focus, SortBy, SourceState};
use crate::model::{Item, Section, SourceId, Status, format_size_long};
use crate::platform::Os;

const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
/// Width of a size column: "999.9 GB".
pub const SIZE_W: usize = 8;
/// Width of a source's total in the sidebar: a size or "no access".
const TOTAL_W: usize = 9;

fn yellow() -> Style {
    Style::default().fg(Color::Yellow)
}
fn dim() -> Style {
    Style::default().fg(Color::Gray).add_modifier(Modifier::DIM)
}
fn white() -> Style {
    Style::default().fg(Color::White)
}
fn title() -> Style {
    white().add_modifier(Modifier::BOLD)
}

pub fn spinner(app: &App) -> &'static str {
    SPINNER[(app.spinner_tick % SPINNER.len() as u64) as usize]
}

/// What the user can do about a scan failure or note, if anything.
/// Permission failures get their own prompt, see [`access_lines`].
pub fn recovery(msg: &str) -> Option<&'static str> {
    if msg.contains("daemon stopped") {
        Some("Start Docker Desktop, then press r to scan again.")
    } else {
        None
    }
}

/// `~/Workspace` instead of `/Users/me/Workspace`.
pub fn tilde(app: &App) -> String {
    let target = app.target.display().to_string();
    match std::env::var("HOME") {
        Ok(home) if !home.is_empty() && target.starts_with(&home) => {
            format!("~{}", &target[home.len()..])
        }
        _ => target,
    }
}

/// Outer frame shared by every screen; returns the inner area.
pub fn frame(f: &mut Frame, app: &App, area: Rect) -> Rect {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Plain)
        .border_style(dim())
        .title(Line::from(vec![
            Span::styled(" devsweep ", title()),
            Span::styled(format!("─ {} ", tilde(app)), dim()),
        ]));
    let inner = block.inner(area);
    f.render_widget(block, area);
    inner
}

pub fn draw(f: &mut Frame, app: &App, area: Rect) {
    let inner = frame(f, app, area);
    let [body, bar] = Layout::vertical([Constraint::Min(1), Constraint::Length(2)]).areas(inner);
    if area.width < super::NARROW_W {
        let [tabs, list] =
            Layout::vertical([Constraint::Length(2), Constraint::Min(1)]).areas(body);
        draw_tabs(f, app, tabs);
        draw_items(f, app, list);
    } else {
        let [side, list] =
            Layout::horizontal([Constraint::Length(sidebar_width(app)), Constraint::Min(1)])
                .areas(body);
        draw_sidebar(f, app, side);
        draw_items(f, app, list);
    }
    draw_bar(f, app, bar);
    if app.show_help {
        draw_help(f, area, app.os);
    }
}

/// Total shown next to a source: spinner, `error`, or its size.
fn source_total(app: &App, source: SourceId) -> Span<'static> {
    let view = &app.sources[&source];
    let text = match &view.state {
        SourceState::Scanning => {
            return Span::styled(format!("{:>TOTAL_W$}", spinner(app)), yellow());
        }
        // A missing permission is not a failure of the tool.
        SourceState::Failed(_) if app.needs_disk_access(source) => {
            return Span::styled(
                format!("{:>TOTAL_W$}", "no access"),
                Style::default().fg(Color::Cyan),
            );
        }
        SourceState::Failed(_) => {
            return Span::styled(
                format!("{:>TOTAL_W$}", "error"),
                Style::default().fg(Color::Red),
            );
        }
        SourceState::Done => format!("{:>TOTAL_W$}", format_size_long(app.source_total(source))),
    };
    if view.items.is_empty() {
        Span::styled(text, dim())
    } else {
        Span::styled(text, white())
    }
}

fn label_width(app: &App) -> usize {
    app.visible_sources()
        .iter()
        .map(|s| s.label().chars().count())
        .max()
        .unwrap_or(0)
}

/// Gutter, label and size, plus the right border.
fn sidebar_width(app: &App) -> u16 {
    (2 + label_width(app) + 2 + TOTAL_W + 1 + 1) as u16
}

/// Sources with their size; the focused one gets a gutter mark.
fn draw_sidebar(f: &mut Frame, app: &App, area: Rect) {
    let focused = app.focus == Focus::Sidebar;
    let block = Block::default()
        .borders(Borders::RIGHT)
        .border_style(if focused { yellow() } else { dim() });
    let inner = block.inner(area);
    f.render_widget(block, area);

    let sources = app.visible_sources();
    let label_w = sources
        .iter()
        .map(|s| s.label().chars().count())
        .max()
        .unwrap_or(0);
    let width = inner.width as usize;

    let mut lines = Vec::new();
    let mut last_section = None;
    for (i, source) in sources.into_iter().enumerate() {
        let section = source.section();
        if last_section != Some(section) {
            if last_section.is_some() {
                lines.push(Line::default());
            }
            let name = match section {
                Section::Folder => "this folder",
                Section::Machine => "machine",
            };
            let rule = "─".repeat(width.saturating_sub(name.len() + 5));
            lines.push(Line::styled(format!(" ── {name} {rule}"), dim()));
            last_section = Some(section);
        }
        let current = i == app.cursor_source;
        let empty = app.sources[&source].items.is_empty()
            && app.sources[&source].state == SourceState::Done;
        let (gutter, style) = match (current, focused) {
            (true, true) => ("▌", yellow().add_modifier(Modifier::BOLD)),
            (true, false) => ("▌", title()),
            _ if empty => (" ", dim()),
            _ => (" ", white()),
        };
        lines.push(Line::from(vec![
            Span::styled(gutter, style),
            Span::raw(" "),
            Span::styled(format!("{:<label_w$}  ", source.label()), style),
            source_total(app, source),
        ]));
    }
    f.render_widget(Paragraph::new(lines), inner);
}

fn draw_tabs(f: &mut Frame, app: &App, area: Rect) {
    let mut spans = vec![Span::raw(" ")];
    for (i, source) in app.visible_sources().into_iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled(" │ ", dim()));
        }
        let style = if i == app.cursor_source {
            yellow().add_modifier(Modifier::BOLD)
        } else {
            white()
        };
        spans.push(Span::styled(source.label().to_string(), style));
        let total = source_total(app, source);
        spans.push(Span::styled(
            format!(" {}", total.content.trim_start()),
            total.style,
        ));
    }
    let block = Block::default()
        .borders(Borders::BOTTOM)
        .border_style(dim());
    f.render_widget(Paragraph::new(Line::from(spans)).block(block), area);
}

/// Status words for an item, colored by meaning. A locked item shows only
/// who holds it; the `⊘` mark sits in the checkbox column.
pub fn status_spans(item: &Item) -> Vec<Span<'static>> {
    if let Some(lock) = &item.lock {
        return vec![Span::styled(lock.clone(), dim())];
    }
    let mut spans = Vec::new();
    for status in &item.status {
        if !spans.is_empty() {
            spans.push(Span::styled(" · ", dim()));
        }
        let (text, style) = match status {
            Status::Merged => ("merged".to_string(), Style::default().fg(Color::Green)),
            Status::Clean => ("clean".into(), dim()),
            Status::Dirty(n) => (
                format!("dirty · {n} files"),
                Style::default().fg(Color::Red),
            ),
            Status::Ignored(n) => (
                format!("{n} ignored {}", if *n == 1 { "file" } else { "files" }),
                Style::default().fg(Color::Red),
            ),
            Status::Stale(d) => (format!("stale {d}d"), Style::default().fg(Color::Cyan)),
            Status::Broken => ("broken".into(), Style::default().fg(Color::Red)),
            Status::Booted => ("booted".into(), Style::default().fg(Color::Cyan)),
            Status::Running => ("running".into(), Style::default().fg(Color::Cyan)),
            Status::Unavailable => ("unavailable".into(), Style::default().fg(Color::Cyan)),
            Status::Orphan => ("orphan".into(), Style::default().fg(Color::Cyan)),
            Status::LastUsed(d) => (format!("used {d}d ago"), dim()),
            Status::Detail(s) => (s.clone(), dim()),
        };
        spans.push(Span::styled(text, style));
    }
    if item.safe {
        if !spans.is_empty() {
            spans.push(Span::styled(" · ", dim()));
        }
        spans.push(Span::styled("safe", dim()));
    }
    spans
}

fn truncate(s: &str, width: usize) -> String {
    if s.chars().count() <= width {
        return format!("{s:<width$}");
    }
    let cut: String = s.chars().take(width.saturating_sub(1)).collect();
    format!("{cut}…")
}

/// Heading of the item pane: source, total and count, or scan progress.
fn heading(app: &App, source: SourceId, shown: usize) -> String {
    let view = &app.sources[&source];
    let noun = if shown == 1 { "item" } else { "items" };
    match view.state {
        SourceState::Done | SourceState::Failed(_) if view.items.is_empty() => {
            source.label().to_string()
        }
        SourceState::Scanning => format!(
            "{}  {} scanning · {} found",
            source.label(),
            spinner(app),
            view.items.len()
        ),
        _ => format!(
            "{}  {} · {shown} {noun}",
            source.label(),
            format_size_long(app.source_total(source))
        ),
    }
}

/// Heading row and rule of the item pane.
fn pane_header(
    app: &App,
    source: SourceId,
    shown: usize,
    width: usize,
    focused: bool,
) -> Vec<Line<'static>> {
    let sort = match app.sort {
        SortBy::Size => "size",
        SortBy::Name => "name",
        SortBy::Age => "age",
    };
    let count = heading(app, source, shown);
    let filter = match (&app.filter, app.editing_filter) {
        (Some(f), true) => format!("  / {f}▏"),
        (Some(f), false) => format!("  / {f}"),
        _ => String::new(),
    };
    let right = format!("sort: {sort}");
    let pad = width.saturating_sub(count.chars().count() + filter.chars().count() + right.len());
    vec![
        Line::from(vec![
            Span::styled(
                count,
                if focused {
                    yellow().add_modifier(Modifier::BOLD)
                } else {
                    title()
                },
            ),
            Span::styled(filter, yellow()),
            Span::raw(" ".repeat(pad)),
            Span::styled(right, dim()),
        ]),
        Line::styled("─".repeat(width), dim()),
    ]
}

/// What the pane says besides its rows: a failure, the scanner's notes, the
/// Full Disk Access prompt, or why the list is empty.
fn pane_messages(app: &App, source: SourceId, empty: bool) -> Vec<Line<'static>> {
    let view = &app.sources[&source];
    let mut lines = Vec::new();
    let blocked = app.needs_disk_access(source);
    match &view.state {
        SourceState::Failed(_) if blocked => {}
        SourceState::Failed(msg) => {
            lines.push(Line::styled(msg.clone(), Style::default().fg(Color::Red)));
            let hint = recovery(msg).unwrap_or("Press r to scan again.");
            lines.push(Line::styled(hint, dim()));
        }
        _ => {}
    }
    for note in &view.notes {
        lines.push(Line::styled(note.clone(), dim()));
        if let Some(hint) = recovery(note) {
            lines.push(Line::styled(hint, dim()));
        }
    }
    if blocked {
        if !view.notes.is_empty() {
            lines.push(Line::default());
        }
        lines.extend(access_lines(app, source));
    }
    // A failure or a note already says why the list is empty.
    let explained = matches!(view.state, SourceState::Failed(_)) || !view.notes.is_empty();
    if empty && !explained {
        let msg = match (&view.state, source.section(), &app.filter) {
            (SourceState::Scanning, _, _) => "Rows appear here as they are found.".to_string(),
            (_, _, Some(f)) if !f.is_empty() => format!("Nothing matches \"{f}\"."),
            (_, Section::Folder, _) => format!("Nothing to clean under {}.", tilde(app)),
            (_, Section::Machine, _) => "Nothing to clean here.".to_string(),
        };
        lines.push(Line::styled(msg, dim()));
    }
    lines
}

/// One item: checkbox, label, status and size.
fn item_row(
    app: &App,
    item: &Item,
    is_cursor: bool,
    status_w: usize,
    label_w: usize,
) -> Line<'static> {
    let selected = app.selected.contains(&item.id);
    let locked = item.lock.is_some();
    let check = if locked {
        Span::styled(" ⊘  ", dim())
    } else if selected {
        Span::styled("[x] ", Style::default().fg(Color::Green))
    } else {
        Span::styled("[ ] ", dim())
    };
    let label_style = if is_cursor {
        yellow().add_modifier(Modifier::BOLD)
    } else if locked {
        dim()
    } else {
        white()
    };
    let mut status = status_spans(item);
    let status_len: usize = status.iter().map(|s| s.width()).sum();
    if status_len > status_w {
        let text: String = status.iter().map(|s| s.content.to_string()).collect();
        let style = status.first().map(|s| s.style).unwrap_or_default();
        status = vec![Span::styled(truncate(&text, status_w), style)];
    } else {
        status.push(Span::raw(" ".repeat(status_w - status_len)));
    }
    let size = item.size.map_or_else(|| "…".into(), format_size_long);
    let value_style = if locked { dim() } else { white() };
    let mut spans = vec![
        check,
        Span::styled(truncate(&item.label, label_w), label_style),
        Span::raw("  "),
    ];
    spans.extend(status);
    spans.push(Span::styled(format!(" {size:>SIZE_W$}"), value_style));
    let line = Line::from(spans);
    if locked {
        line.patch_style(Style::default().add_modifier(Modifier::DIM))
    } else {
        line
    }
}

fn draw_items(f: &mut Frame, app: &App, area: Rect) {
    let Some(source) = app.focused_source() else {
        return;
    };
    let items = app.visible_items();
    let focused = app.focus == Focus::Items;
    let area = Rect {
        x: area.x + 1,
        width: area.width.saturating_sub(2),
        ..area
    };
    let width = area.width as usize;
    let mut lines = pane_header(app, source, items.len(), width, focused);
    lines.extend(pane_messages(app, source, items.is_empty()));

    let rows = (area.height as usize).saturating_sub(lines.len());
    let offset = app.cursor_item.saturating_sub(rows.saturating_sub(1));
    let status_w = (width / 3).min(30);
    let label_w = width.saturating_sub(4 + 2 + status_w + 1 + SIZE_W);
    for (i, item) in items.iter().enumerate().skip(offset).take(rows) {
        let is_cursor = focused && i == app.cursor_item;
        lines.push(item_row(app, item, is_cursor, status_w, label_w));
    }
    f.render_widget(Paragraph::new(lines), area);
}

/// The Full Disk Access prompt: why, whose permission, and the one key
/// that opens the right settings pane. Access only applies after the
/// terminal restarts, so the copy never promises that `r` is enough.
fn access_lines(app: &App, source: SourceId) -> Vec<Line<'static>> {
    let what = match source {
        SourceId::Trash => "the Trash",
        _ => "every folder here",
    };
    let term = &app.terminal;
    let mut subject = term.clone();
    if let Some(first) = subject.get_mut(..1) {
        first.make_ascii_uppercase();
    }
    let (action, then) = if app.opened_settings {
        (
            "open System Settings › Full Disk Access again".to_string(),
            format!("Opened. After turning {term} on, quit it and run devsweep again."),
        )
    } else {
        (
            "open System Settings › Full Disk Access".to_string(),
            format!("turn {term} on, then quit it and run devsweep again"),
        )
    };
    vec![
        Line::styled(
            format!("macOS only lets apps with Full Disk Access read {what}."),
            white(),
        ),
        Line::styled(format!("{subject} doesn't have it yet."), white()),
        Line::default(),
        Line::from(vec![
            Span::styled("o  ", yellow().add_modifier(Modifier::BOLD)),
            Span::styled(action, white()),
        ]),
        Line::styled(format!("   {then}"), dim()),
    ]
}

fn draw_bar(f: &mut Frame, app: &App, area: Rect) {
    let (n, bytes) = app.selected_total();
    let keys = "space toggle · ⏎ review · ? help · a all · s sort · / filter · q quit";
    let line = Line::from(vec![
        Span::styled(
            format!(" {n} selected · {}", format_size_long(bytes)),
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!("   {keys}"), dim()),
    ]);
    let block = Block::default().borders(Borders::TOP).border_style(dim());
    f.render_widget(Paragraph::new(line).block(block), area);
}

/// Key and mark legend, centered over the list.
fn draw_help(f: &mut Frame, area: Rect, os: Os) {
    let key = |k: &str, what: &str| {
        Line::from(vec![
            Span::styled(format!("  {k:<11}"), yellow()),
            Span::styled(what.to_string(), white()),
        ])
    };
    let mark = |m: Span<'static>, what: &str| {
        Line::from(vec![
            Span::raw("  "),
            m,
            Span::styled(what.to_string(), white()),
        ])
    };
    let mut lines = vec![
        key("↑↓ j k", "move"),
        key("tab ← →", "switch between sources and items"),
        key("space", "select or unselect the item"),
        key("a", "select every unlocked item of the source"),
        key("s", "sort by size, name or age"),
        key("/", "filter by name"),
        key("r", "scan again"),
        key("o", "open Full Disk Access settings, when asked"),
        key("⏎", "review what will be removed"),
        key("q", "quit"),
        Line::default(),
        mark(
            Span::styled("[x]        ", Style::default().fg(Color::Green)),
            "selected",
        ),
        mark(
            Span::styled(" ⊘         ", dim()),
            "locked: in use, cannot be selected",
        ),
        mark(
            Span::styled("safe       ", dim()),
            "regenerates by itself; preselected",
        ),
        mark(
            Span::styled("merged     ", Style::default().fg(Color::Green)),
            "its branch is in the default branch",
        ),
        mark(
            Span::styled("dirty      ", Style::default().fg(Color::Red)),
            "would lose uncommitted or ignored files",
        ),
        mark(
            Span::styled("stale      ", Style::default().fg(Color::Cyan)),
            "no commit for a while",
        ),
    ];
    if os != Os::MacOs {
        // Full Disk Access is a macOS setting.
        lines.retain(|line| !line.to_string().contains("Full Disk Access"));
    }
    let w = 60.min(area.width.saturating_sub(4));
    let h = (lines.len() as u16 + 2).min(area.height.saturating_sub(2));
    let rect = Rect {
        x: area.x + (area.width.saturating_sub(w)) / 2,
        y: area.y + (area.height.saturating_sub(h)) / 2,
        width: w,
        height: h,
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(yellow())
        .title(Span::styled(
            " help · any key closes ",
            yellow().add_modifier(Modifier::BOLD),
        ));
    f.render_widget(Clear, rect);
    f.render_widget(Paragraph::new(lines).block(block), rect);
}

#[cfg(test)]
mod tests {
    use crate::app::App;
    use crate::model::{Item, Removal, SourceId, Status};
    use crate::platform::Os;
    use crate::scan::ScanEvent;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::style::{Color, Modifier};
    use std::path::PathBuf;

    fn item(
        id: u64,
        source: SourceId,
        label: &str,
        size: Option<u64>,
        status: Vec<Status>,
        lock: Option<&str>,
        safe: bool,
    ) -> Item {
        Item {
            id,
            source,
            label: label.into(),
            path: None,
            size,
            status,
            lock: lock.map(String::from),
            safe,
            removal: Removal::Command {
                argv: vec!["true".into()],
                cwd: None,
            },
            age_days: None,
            recheck: crate::model::Recheck::default(),
        }
    }

    fn sources() -> Vec<SourceId> {
        vec![
            SourceId::Artifacts,
            SourceId::Worktrees,
            SourceId::Ios,
            SourceId::Docker,
            SourceId::DevCaches,
            SourceId::Android,
        ]
    }

    /// The worktree list from the spec mockup, focused on its items.
    fn mockup() -> App {
        let mut a = App::new(PathBuf::from("/Users/u/Workspace"), sources());
        a.os = Os::MacOs;
        let found = [
            item(
                1,
                SourceId::Artifacts,
                "glowz · node_modules",
                Some(14_200_000_000),
                vec![],
                None,
                false,
            ),
            item(
                2,
                SourceId::Worktrees,
                "codex/glowz-robots",
                Some(412_000_000),
                vec![Status::Merged, Status::Clean],
                None,
                true,
            ),
            item(
                3,
                SourceId::Worktrees,
                "codex/customer-module",
                Some(388_000_000),
                vec![Status::Merged, Status::Clean],
                None,
                true,
            ),
            item(
                4,
                SourceId::Worktrees,
                "codex/glowz-dev-deploy",
                Some(301_000_000),
                vec![Status::Dirty(3)],
                None,
                false,
            ),
            item(
                5,
                SourceId::Worktrees,
                "orca/website/arowana",
                Some(290_000_000),
                vec![Status::Clean],
                Some("claude · PID 4821"),
                false,
            ),
            item(
                6,
                SourceId::Worktrees,
                "codex/090b",
                Some(95_000_000),
                vec![Status::Clean, Status::Stale(41)],
                None,
                false,
            ),
            item(
                7,
                SourceId::Ios,
                "iPhone 18 Pro · iOS 27.0",
                Some(19_000_000_000),
                vec![],
                None,
                false,
            ),
            item(
                8,
                SourceId::DevCaches,
                "pnpm store",
                Some(8_200_000_000),
                vec![Status::Detail("JavaScript".into())],
                None,
                true,
            ),
        ];
        for it in found {
            a.on_scan(ScanEvent::Found(it));
        }
        for s in [
            SourceId::Artifacts,
            SourceId::Worktrees,
            SourceId::Ios,
            SourceId::DevCaches,
        ] {
            a.on_scan(ScanEvent::Done(s));
        }
        a.on_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        a.on_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        a
    }

    fn render(app: &App, w: u16, h: u16) -> Terminal<TestBackend> {
        let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
        t.draw(|f| crate::ui::draw(f, app)).unwrap();
        t
    }

    #[test]
    fn snapshot_scanning() {
        let a = App::new(PathBuf::from("/Users/u/Workspace"), sources());
        insta::assert_snapshot!(render(&a, 100, 20).backend());
    }

    #[test]
    fn snapshot_worktrees_list_with_locked_row() {
        insta::assert_snapshot!(render(&mockup(), 100, 20).backend());
    }

    #[test]
    fn snapshot_failed_source() {
        let mut a = App::new(PathBuf::from("/w"), vec![SourceId::Artifacts]);
        a.os = Os::MacOs;
        a.on_scan(ScanEvent::NoAccess(SourceId::Artifacts));
        a.on_scan(ScanEvent::Failed(
            SourceId::Artifacts,
            "permission denied".into(),
        ));
        a.on_scan(ScanEvent::Done(SourceId::Artifacts));
        insta::assert_snapshot!(render(&a, 100, 12).backend());
    }

    #[test]
    fn snapshot_trash_permission_denied_on_linux() {
        let mut a = App::new(PathBuf::from("/w"), vec![SourceId::Trash]);
        a.os = Os::Linux;
        a.on_scan(ScanEvent::NoAccess(SourceId::Trash));
        a.on_scan(ScanEvent::Failed(
            SourceId::Trash,
            "permission denied reading the Trash".into(),
        ));
        a.on_scan(ScanEvent::Done(SourceId::Trash));
        insta::assert_snapshot!(render(&a, 100, 12).backend());
    }

    #[test]
    fn help_mentions_disk_access_only_on_macos() {
        let help = |os| {
            let mut a = mockup();
            a.os = os;
            a.on_key(KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE));
            render(&a, 100, 24).backend().to_string()
        };
        assert!(help(Os::MacOs).contains("Full Disk Access"));
        assert!(!help(Os::Linux).contains("Full Disk Access"));
        assert!(!help(Os::Windows).contains("Full Disk Access"));
    }

    #[test]
    fn snapshot_trash_without_disk_access() {
        let mut a = App::new(PathBuf::from("/w"), vec![SourceId::Trash]);
        a.os = Os::MacOs;
        a.terminal = "Ghostty".into();
        a.on_scan(ScanEvent::NoAccess(SourceId::Trash));
        a.on_scan(ScanEvent::Failed(
            SourceId::Trash,
            "permission denied reading ~/.Trash".into(),
        ));
        a.on_scan(ScanEvent::Done(SourceId::Trash));
        insta::assert_snapshot!(render(&a, 100, 12).backend());
    }

    #[test]
    fn snapshot_trash_after_opening_settings() {
        let mut a = App::new(PathBuf::from("/w"), vec![SourceId::Trash]);
        a.os = Os::MacOs;
        a.terminal = "Ghostty".into();
        a.on_scan(ScanEvent::NoAccess(SourceId::Trash));
        a.on_scan(ScanEvent::Failed(
            SourceId::Trash,
            "permission denied reading ~/.Trash".into(),
        ));
        a.on_scan(ScanEvent::Done(SourceId::Trash));
        a.on_key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::NONE));
        insta::assert_snapshot!(render(&a, 100, 12).backend());
    }

    #[test]
    fn snapshot_docker_daemon_stopped_note() {
        let mut a = App::new(PathBuf::from("/w"), vec![SourceId::Docker]);
        a.on_scan(ScanEvent::Note(SourceId::Docker, "daemon stopped".into()));
        a.on_scan(ScanEvent::Done(SourceId::Docker));
        insta::assert_snapshot!(render(&a, 100, 12).backend());
    }

    #[test]
    fn snapshot_narrow_70_cols() {
        insta::assert_snapshot!(render(&mockup(), 70, 16).backend());
    }

    #[test]
    fn snapshot_too_small() {
        insta::assert_snapshot!(render(&mockup(), 30, 8).backend());
    }

    #[test]
    fn failures_and_notes_carry_their_next_step() {
        assert!(
            super::recovery("daemon stopped")
                .unwrap()
                .contains("Docker Desktop")
        );
        // Permission problems get the Full Disk Access prompt instead.
        assert_eq!(super::recovery("permission denied"), None);
        assert_eq!(super::recovery("something else"), None);
    }

    #[test]
    fn snapshot_help_overlay() {
        let mut a = mockup();
        a.on_key(KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE));
        insta::assert_snapshot!(render(&a, 100, 24).backend());
    }

    #[test]
    fn snapshot_empty_folder_source() {
        let mut a = App::new(PathBuf::from("/w"), vec![SourceId::Worktrees]);
        a.on_scan(ScanEvent::Done(SourceId::Worktrees));
        insta::assert_snapshot!(render(&a, 100, 12).backend());
    }

    #[test]
    fn locked_row_is_dim_and_cursor_row_is_yellow() {
        let t = render(&mockup(), 100, 20);
        let buf = t.backend().buffer();
        let row_of = |needle: &str| {
            (0..buf.area.height)
                .find(|&y| {
                    (0..buf.area.width)
                        .map(|x| buf[(x, y)].symbol())
                        .collect::<String>()
                        .contains(needle)
                })
                .unwrap()
        };
        let col_of = |y: u16, needle: &str| {
            let line: String = (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect();
            line.find(needle)
                .map(|b| line[..b].chars().count() as u16)
                .unwrap()
        };
        let locked = row_of("arowana");
        assert!(
            buf[(col_of(locked, "arowana"), locked)]
                .modifier
                .contains(Modifier::DIM)
        );
        let cursor = row_of("glowz-robots");
        assert_eq!(
            buf[(col_of(cursor, "glowz-robots"), cursor)].fg,
            Color::Yellow
        );
    }
}
