//! Main screen: source sidebar, item list and status bar.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph};

use crate::app::{App, Focus, SortBy, SourceState};
use crate::model::{Item, Section, SourceId, Status, format_size, format_size_long};

pub const SIDEBAR_W: u16 = 24;
const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

fn yellow() -> Style {
    Style::default().fg(Color::Yellow)
}
fn dim() -> Style {
    Style::default().fg(Color::Gray).add_modifier(Modifier::DIM)
}
fn title() -> Style {
    Style::default()
        .fg(Color::White)
        .add_modifier(Modifier::BOLD)
}

pub fn spinner(app: &App) -> &'static str {
    SPINNER[(app.spinner_tick % SPINNER.len() as u64) as usize]
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
            Layout::horizontal([Constraint::Length(SIDEBAR_W), Constraint::Min(1)]).areas(body);
        draw_sidebar(f, app, side);
        draw_items(f, app, list);
    }
    draw_bar(f, app, bar);
}

/// Total shown next to a source: spinner, `error`, or its size.
fn source_total(app: &App, source: SourceId) -> Span<'static> {
    let view = &app.sources[&source];
    match &view.state {
        SourceState::Scanning => Span::styled(format!("{} ", spinner(app)).repeat(1), yellow()),
        SourceState::Failed(_) => Span::styled("error", Style::default().fg(Color::Red)),
        SourceState::Done => {
            let total = app.source_total(source);
            if view.items.is_empty() {
                Span::styled(format_size(total), dim())
            } else {
                Span::styled(format_size(total), Style::default().fg(Color::White))
            }
        }
    }
}

fn draw_sidebar(f: &mut Frame, app: &App, area: Rect) {
    let focused = app.focus == Focus::Sidebar;
    let block = Block::default()
        .borders(Borders::RIGHT)
        .border_style(if focused { yellow() } else { dim() });
    let inner = block.inner(area);
    f.render_widget(block, area);

    let mut lines = Vec::new();
    let mut last_section = None;
    for (i, source) in app.visible_sources().into_iter().enumerate() {
        let section = source.section();
        if last_section != Some(section) {
            if last_section.is_some() {
                lines.push(Line::default());
            }
            let header = match section {
                Section::Folder => "THIS FOLDER",
                Section::Machine => "MACHINE",
            };
            lines.push(Line::styled(
                format!(" {header}"),
                dim().add_modifier(Modifier::BOLD),
            ));
            last_section = Some(section);
        }
        let current = i == app.cursor_source;
        let marker = if current { "▸ " } else { "  " };
        let total = source_total(app, source);
        let label_w = (inner.width as usize).saturating_sub(4 + total.width());
        let label = format!(
            "{:<label_w$}",
            truncate(source.label(), label_w.saturating_sub(1))
        );
        let empty = app.sources[&source].items.is_empty()
            && app.sources[&source].state == SourceState::Done;
        let style = if current && focused {
            yellow().add_modifier(Modifier::BOLD)
        } else if current {
            title()
        } else if empty {
            dim()
        } else {
            Style::default().fg(Color::White)
        };
        lines.push(Line::from(vec![
            Span::styled(format!(" {marker}"), style),
            Span::styled(label, style),
            total,
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
            Style::default().fg(Color::White)
        };
        spans.push(Span::styled(format!("{} ", source.label()), style));
        spans.push(source_total(app, source));
    }
    let block = Block::default()
        .borders(Borders::BOTTOM)
        .border_style(dim());
    f.render_widget(Paragraph::new(Line::from(spans)).block(block), area);
}

/// Status words for an item, colored by meaning.
pub fn status_spans(item: &Item) -> Vec<Span<'static>> {
    if let Some(lock) = &item.lock {
        return vec![Span::styled(format!("⊘ {lock}"), dim())];
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
    spans
}

fn truncate(s: &str, width: usize) -> String {
    if s.chars().count() <= width {
        return format!("{s:<width$}");
    }
    let cut: String = s.chars().take(width.saturating_sub(1)).collect();
    format!("{cut}…")
}

fn draw_items(f: &mut Frame, app: &App, area: Rect) {
    let Some(source) = app.focused_source() else {
        return;
    };
    let view = &app.sources[&source];
    let items = app.visible_items();
    let focused = app.focus == Focus::Items;
    let area = Rect {
        x: area.x + 1,
        width: area.width.saturating_sub(2),
        ..area
    };

    let sort = match app.sort {
        SortBy::Size => "size",
        SortBy::Name => "name",
        SortBy::Age => "age",
    };
    let count = format!("{} · {} items", source.label(), items.len());
    let filter = match (&app.filter, app.editing_filter) {
        (Some(f), true) => format!("  / {f}▏"),
        (Some(f), false) => format!("  / {f}"),
        _ => String::new(),
    };
    let right = format!("sort: {sort}");
    let pad = (area.width as usize)
        .saturating_sub(count.chars().count() + filter.chars().count() + right.len());
    let mut lines = vec![
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
        Line::styled("─".repeat(area.width as usize), dim()),
    ];
    if let SourceState::Failed(msg) = &view.state {
        lines.push(Line::styled(msg.clone(), Style::default().fg(Color::Red)));
    }
    for note in &view.notes {
        lines.push(Line::styled(note.clone(), dim()));
    }

    let rows = (area.height as usize).saturating_sub(lines.len());
    let offset = app.cursor_item.saturating_sub(rows.saturating_sub(1));
    let size_w = 7;
    let status_w = ((area.width as usize) / 3).min(28);
    let label_w = (area.width as usize).saturating_sub(4 + status_w + size_w + 2);
    for (i, item) in items.iter().enumerate().skip(offset).take(rows) {
        let is_cursor = focused && i == app.cursor_item;
        let selected = app.selected.contains(&item.id);
        let locked = item.lock.is_some();
        let check = if selected {
            Span::styled("[x] ", Style::default().fg(Color::Green))
        } else {
            Span::styled("[ ] ", dim())
        };
        let label_style = if is_cursor {
            yellow().add_modifier(Modifier::BOLD)
        } else if locked {
            dim()
        } else {
            Style::default().fg(Color::White)
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
        let size = item.size.map(format_size).unwrap_or_else(|| "…".into());
        let mut spans = vec![
            check,
            Span::styled(truncate(&item.label, label_w), label_style),
            Span::raw("  "),
        ];
        spans.extend(status);
        spans.push(Span::styled(
            format!("{size:>size_w$}"),
            if locked {
                dim()
            } else {
                Style::default().fg(Color::White)
            },
        ));
        let mut line = Line::from(spans);
        if locked {
            line = line.patch_style(Style::default().add_modifier(Modifier::DIM));
        }
        lines.push(line);
    }
    f.render_widget(Paragraph::new(lines), area);
}

fn draw_bar(f: &mut Frame, app: &App, area: Rect) {
    let (n, bytes) = app.selected_total();
    let keys = "space toggle · tab pane · a all · s sort · / filter · ⏎ review · q quit";
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

#[cfg(test)]
mod tests {
    use crate::app::App;
    use crate::model::{Item, Removal, SourceId, Status};
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
        a.on_scan(ScanEvent::Failed(
            SourceId::Artifacts,
            "permission denied".into(),
        ));
        a.on_scan(ScanEvent::Done(SourceId::Artifacts));
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
