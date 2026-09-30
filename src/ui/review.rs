//! Review, removal progress and summary screens.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

use super::list::{frame, spinner};
use crate::app::{App, Progress, Screen};
use crate::model::{Item, SourceId, format_size, format_size_long};

fn dim() -> Style {
    Style::default().fg(Color::Gray).add_modifier(Modifier::DIM)
}
fn green() -> Style {
    Style::default().fg(Color::Green)
}
fn red() -> Style {
    Style::default().fg(Color::Red)
}
fn title() -> Style {
    Style::default()
        .fg(Color::White)
        .add_modifier(Modifier::BOLD)
}

fn layout(f: &mut Frame, app: &App, area: Rect) -> (Rect, Rect, Rect) {
    let inner = frame(f, app, area);
    let [head, body, foot] = Layout::vertical([
        Constraint::Length(2),
        Constraint::Min(1),
        Constraint::Length(2),
    ])
    .areas(inner);
    let pad = |r: Rect| Rect {
        x: r.x + 1,
        width: r.width.saturating_sub(2),
        ..r
    };
    (pad(head), pad(body), foot)
}

fn footer(f: &mut Frame, area: Rect, keys: &str) {
    let block = Block::default().borders(Borders::TOP).border_style(dim());
    f.render_widget(
        Paragraph::new(Line::styled(format!(" {keys}"), dim())).block(block),
        area,
    );
}

fn right_aligned(left: Vec<Span<'static>>, right: Span<'static>, width: u16) -> Line<'static> {
    let used: usize = left.iter().map(|s| s.width()).sum::<usize>() + right.width();
    let mut spans = left;
    spans.push(Span::raw(" ".repeat((width as usize).saturating_sub(used))));
    spans.push(right);
    Line::from(spans)
}

/// Lines from `offset`, clamped so the last page stays full.
fn window(lines: Vec<Line<'static>>, offset: usize, height: u16) -> Vec<Line<'static>> {
    let h = height as usize;
    let start = offset.min(lines.len().saturating_sub(h));
    lines.into_iter().skip(start).take(h).collect()
}

/// Keep the line at `focus` on screen.
fn scroll(lines: Vec<Line<'static>>, focus: usize, height: u16) -> Vec<Line<'static>> {
    let h = height as usize;
    let start = focus
        .saturating_sub(h.saturating_sub(2))
        .min(lines.len().saturating_sub(h));
    lines.into_iter().skip(start).take(h).collect()
}

pub fn draw_review(f: &mut Frame, app: &App, area: Rect) {
    let (head, body, foot) = layout(f, app, area);
    let (n, bytes) = app.selected_total();
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                "Review",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!(" · {n} items · {}", format_size_long(bytes)),
                title(),
            ),
        ])),
        head,
    );
    let mut lines = Vec::new();
    for (source, items) in app.review_groups() {
        lines.push(Line::styled(source.label().to_string(), title()));
        for item in items {
            let size = Span::styled(
                item.size.map(format_size).unwrap_or_else(|| "…".into()),
                Style::default().fg(Color::White),
            );
            lines.push(right_aligned(
                vec![
                    Span::raw("  "),
                    Span::styled(item.label.clone(), Style::default().fg(Color::White)),
                ],
                size,
                body.width,
            ));
            lines.push(Line::styled(
                format!("    {}", item.removal.describe()),
                dim(),
            ));
        }
        lines.push(Line::default());
    }
    f.render_widget(
        Paragraph::new(window(lines, app.review_scroll, body.height)),
        body,
    );
    footer(f, foot, "y confirm · ↑↓ scroll · any other key back");
}

fn grouped(app: &App) -> Vec<(SourceId, Vec<&Item>)> {
    let mut groups: Vec<(SourceId, Vec<&Item>)> = Vec::new();
    for item in &app.removal {
        match groups.last_mut() {
            Some((source, items)) if *source == item.source => items.push(item),
            _ => groups.push((item.source, vec![item])),
        }
    }
    groups
}

fn progress_line(app: &App, item: &Item, width: u16) -> Line<'static> {
    let label = Span::styled(item.label.clone(), Style::default().fg(Color::White));
    let (mark, right) = match app.progress.get(&item.id).unwrap_or(&Progress::Pending) {
        Progress::Pending => (Span::styled("  · ", dim()), Span::styled("waiting", dim())),
        Progress::Running => (
            Span::styled(
                format!("  {} ", spinner(app)),
                Style::default().fg(Color::Yellow),
            ),
            Span::styled("removing", Style::default().fg(Color::Yellow)),
        ),
        Progress::Ok(bytes) => (
            Span::styled("  ✓ ", green()),
            Span::styled(format_size(*bytes), green()),
        ),
        Progress::Err(msg) => (
            Span::styled("  ✗ ", red()),
            Span::styled(msg.clone(), red()),
        ),
    };
    right_aligned(vec![mark, label], right, width)
}

pub fn draw_progress(f: &mut Frame, app: &App, area: Rect) {
    let (head, body, foot) = layout(f, app, area);
    let (freed_n, freed) = app.freed_total();
    let done = app.screen == Screen::Done;
    let header = if done {
        Line::styled(
            format!("Freed {} in {freed_n} items", format_size_long(freed)),
            green().add_modifier(Modifier::BOLD),
        )
    } else {
        Line::from(vec![
            Span::styled(
                format!("{} Removing", spinner(app)),
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!(" · {} freed so far", format_size_long(freed)),
                title(),
            ),
        ])
    };
    f.render_widget(Paragraph::new(header), head);

    let mut lines = Vec::new();
    let mut focus = 0;
    if done {
        let failed: Vec<&Item> = app
            .removal
            .iter()
            .filter(|i| matches!(app.progress.get(&i.id), Some(Progress::Err(_))))
            .collect();
        if failed.is_empty() {
            lines.push(Line::styled("Everything selected was removed.", dim()));
        } else {
            lines.push(Line::styled(
                format!("{} failed", failed.len()),
                red().add_modifier(Modifier::BOLD),
            ));
            for item in failed {
                lines.push(progress_line(app, item, body.width));
            }
        }
    } else {
        for (source, items) in grouped(app) {
            lines.push(Line::styled(source.label().to_string(), title()));
            for item in items {
                if matches!(app.progress.get(&item.id), Some(Progress::Running)) {
                    focus = lines.len();
                }
                lines.push(progress_line(app, item, body.width));
            }
        }
    }
    f.render_widget(Paragraph::new(scroll(lines, focus, body.height)), body);
    footer(
        f,
        foot,
        if done {
            "r rescan · q quit"
        } else {
            "q quit when finished"
        },
    );
}

#[cfg(test)]
mod tests {
    use crate::app::App;
    use crate::model::{Item, Removal, SourceId, Status};
    use crate::remove::RemoveEvent;
    use crate::scan::ScanEvent;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use std::path::PathBuf;

    fn item(id: u64, source: SourceId, label: &str, size: u64, removal: Removal) -> Item {
        Item {
            id,
            source,
            label: label.into(),
            path: None,
            size: Some(size),
            status: vec![Status::Merged, Status::Clean],
            lock: None,
            safe: true,
            removal,
            age_days: None,
        }
    }

    fn reviewing() -> App {
        let mut a = App::new(
            PathBuf::from("/Users/u/Workspace"),
            vec![SourceId::Worktrees, SourceId::Docker],
        );
        let wt = Removal::Command {
            argv: vec![
                "git".into(),
                "worktree".into(),
                "remove".into(),
                "/Users/u/.codex/worktrees/glowz-robots".into(),
            ],
            cwd: Some(PathBuf::from("/Users/u/Workspace/glowz")),
        };
        let prune = Removal::Command {
            argv: vec!["docker".into(), "image".into(), "prune".into(), "-f".into()],
            cwd: None,
        };
        let nm = Removal::RemoveDir(PathBuf::from("/Users/u/.codex/worktrees/old app"));
        a.on_scan(ScanEvent::Found(item(
            1,
            SourceId::Worktrees,
            "codex/glowz-robots",
            412_000_000,
            wt,
        )));
        a.on_scan(ScanEvent::Found(item(
            2,
            SourceId::Worktrees,
            "codex/old app",
            95_000_000,
            nm,
        )));
        a.on_scan(ScanEvent::Found(item(
            3,
            SourceId::Docker,
            "Dangling images",
            1_623_000_000,
            prune,
        )));
        for s in [SourceId::Worktrees, SourceId::Docker] {
            a.on_scan(ScanEvent::Done(s));
        }
        a.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        a
    }

    fn render(app: &App) -> Terminal<TestBackend> {
        let mut t = Terminal::new(TestBackend::new(100, 20)).unwrap();
        t.draw(|f| crate::ui::draw(f, app)).unwrap();
        t
    }

    #[test]
    fn snapshot_review_grouped_with_commands() {
        insta::assert_snapshot!(render(&reviewing()).backend());
    }

    #[test]
    fn snapshot_removing_mixed_progress() {
        let mut a = reviewing();
        a.on_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE));
        a.on_remove(RemoveEvent::Started(3));
        a.on_remove(RemoveEvent::Ok(3, 1_623_000_000));
        a.on_remove(RemoveEvent::Started(1));
        a.on_remove(RemoveEvent::Err(1, "changed since scan".into()));
        a.on_remove(RemoveEvent::Started(2));
        insta::assert_snapshot!(render(&a).backend());
    }

    fn done(fail: bool) -> App {
        let mut a = reviewing();
        a.on_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE));
        a.on_remove(RemoveEvent::Ok(3, 1_623_000_000));
        if fail {
            a.on_remove(RemoveEvent::Err(1, "changed since scan".into()));
        } else {
            a.on_remove(RemoveEvent::Ok(1, 412_000_000));
        }
        a.on_remove(RemoveEvent::Ok(2, 95_000_000));
        a.on_remove(RemoveEvent::Finished);
        a
    }

    #[test]
    fn snapshot_done_with_failures() {
        insta::assert_snapshot!(render(&done(true)).backend());
    }

    #[test]
    fn snapshot_done_without_failures() {
        insta::assert_snapshot!(render(&done(false)).backend());
    }
}
