//! Review, removal progress and summary screens.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

use super::list::{SIZE_W, frame, spinner};
use crate::app::{App, Progress, Screen};
use crate::model::{Item, SourceId, format_size_long};
use crate::remove::RemoveError;

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

/// `3 branches`, `1 branch`.
fn branches(n: usize) -> String {
    format!("{n} {}", if n == 1 { "branch" } else { "branches" })
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
    // Deleting a branch frees no space: sizes would only read `0 B`.
    let on_branches = app
        .review_groups()
        .iter()
        .all(|(source, _)| *source == SourceId::Branches);
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                "Review",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                if on_branches {
                    format!(" · {}", branches(n))
                } else {
                    format!(" · {n} items · {}", format_size_long(bytes))
                },
                title(),
            ),
        ])),
        head,
    );
    let mut lines = Vec::new();
    for (source, items) in app.review_groups() {
        lines.push(Line::styled(source.label().to_string(), title()));
        for item in items {
            let size = if source == SourceId::Branches {
                Span::styled("merged", green())
            } else {
                Span::styled(
                    item.size
                        .map(format_size_long)
                        .unwrap_or_else(|| "…".into()),
                    Style::default().fg(Color::White),
                )
            };
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
        Progress::Ok(_) if item.source == SourceId::Branches => (
            Span::styled("  ✓ ", green()),
            Span::styled("deleted", green()),
        ),
        Progress::Ok(bytes) => (
            Span::styled("  ✓ ", green()),
            Span::styled(format_size_long(*bytes), green()),
        ),
        Progress::Err(msg) => (
            Span::styled("  ✗ ", red()),
            Span::styled(msg.to_string(), red()),
        ),
    };
    right_aligned(vec![mark, label], right, width)
}

pub fn draw_progress(f: &mut Frame, app: &App, area: Rect) {
    if app.screen == Screen::Done {
        return draw_done(f, app, area);
    }
    let (head, body, foot) = layout(f, app, area);
    let (_, freed) = app.freed_total();
    let finished = app
        .progress
        .values()
        .filter(|p| matches!(p, Progress::Ok(_) | Progress::Err(_)))
        .count();
    let header = Line::from(vec![
        Span::styled(
            format!("{} Removing", spinner(app)),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            if app.branch_pass() {
                format!(" · {finished} of {}", app.removal.len())
            } else {
                format!(
                    " · {finished} of {} · {} freed so far",
                    app.removal.len(),
                    format_size_long(freed)
                )
            },
            title(),
        ),
    ]);
    f.render_widget(Paragraph::new(header), head);

    let mut lines = Vec::new();
    let mut focus = 0;
    for (source, items) in grouped(app) {
        lines.push(Line::styled(source.label().to_string(), title()));
        for item in items {
            if matches!(app.progress.get(&item.id), Some(Progress::Running)) {
                focus = lines.len();
            }
            lines.push(progress_line(app, item, body.width));
        }
    }
    f.render_widget(Paragraph::new(scroll(lines, focus, body.height)), body);
    footer(
        f,
        foot,
        if app.stopping() {
            "stopping after the current item · ctrl-c again to quit now"
        } else {
            "q stop after the current item"
        },
    );
}

/// The receipt: what was freed per source, the disk before and after, and
/// what was left alone with the reason and the next step.
fn draw_done(f: &mut Frame, app: &App, area: Rect) {
    let (head, body, foot) = layout(f, app, area);
    let (removed, freed) = app.freed_total();
    let branch_pass = app.branch_pass();
    let disk = match app.disk_free {
        _ if branch_pass => Span::raw(""),
        (Some(before), Some(after)) => Span::styled(
            format!(
                "disk free {} → {}",
                format_size_long(before),
                format_size_long(after)
            ),
            title(),
        ),
        _ => Span::raw(""),
    };
    f.render_widget(
        Paragraph::new(right_aligned(
            vec![Span::styled(
                if branch_pass {
                    format!("✓ Deleted {}", branches(removed))
                } else {
                    format!("✓ Freed {}", format_size_long(freed))
                },
                green().add_modifier(Modifier::BOLD),
            )],
            disk,
            head.width,
        )),
        head,
    );

    let by_source = if branch_pass {
        Vec::new()
    } else {
        app.freed_by_source()
    };
    let label_w = by_source
        .iter()
        .map(|(s, _, _)| s.label().chars().count())
        .max()
        .unwrap_or(0);
    let mut lines = Vec::new();
    for (source, n, bytes) in &by_source {
        let noun = if *n == 1 { "item" } else { "items" };
        lines.push(Line::from(vec![
            Span::styled(
                format!("  {:<label_w$}  ", source.label()),
                Style::default().fg(Color::White),
            ),
            Span::styled(
                format!(" {:>SIZE_W$}", format_size_long(*bytes)),
                Style::default().fg(Color::White),
            ),
            Span::styled(format!("   {n} {noun}"), dim()),
        ]));
    }

    let failed: Vec<&Item> = app
        .removal
        .iter()
        .filter(|i| matches!(app.progress.get(&i.id), Some(Progress::Err(_))))
        .collect();
    if failed.is_empty() {
        if !lines.is_empty() {
            lines.push(Line::default());
        }
        lines.push(Line::styled("Everything selected was removed.", dim()));
    } else {
        if !lines.is_empty() {
            lines.push(Line::default());
        }
        lines.push(Line::styled(
            format!("✗ {} not removed", failed.len()),
            red().add_modifier(Modifier::BOLD),
        ));
        let changed = failed.iter().any(|i| {
            matches!(
                app.progress.get(&i.id),
                Some(Progress::Err(RemoveError::Changed))
            )
        });
        for item in failed {
            lines.push(progress_line(app, item, body.width));
        }
        if changed {
            lines.push(Line::styled(
                "  Left alone on purpose: they changed after the scan.",
                dim(),
            ));
        }
        lines.push(Line::styled(
            if changed {
                "  Press r to scan again and review them."
            } else {
                "  Press r to scan again."
            },
            dim(),
        ));
    }
    let left = app.leftovers.len();
    if left > 0 {
        lines.push(Line::default());
        lines.push(Line::styled(
            if left == 1 {
                "1 merged branch was left behind by a removed worktree.".to_string()
            } else {
                format!("{left} merged branches were left behind by the removed worktrees.")
            },
            Style::default().fg(Color::White),
        ));
        lines.push(Line::styled(
            if left == 1 {
                "  Press b to review and delete it."
            } else {
                "  Press b to review and delete them."
            },
            dim(),
        ));
    }
    f.render_widget(Paragraph::new(lines), body);
    footer(
        f,
        foot,
        if left > 0 {
            "b branches · r rescan · q quit"
        } else {
            "r rescan · q quit"
        },
    );
}

#[cfg(test)]
mod tests {
    use crate::app::App;
    use crate::model::{Item, Removal, SourceId, Status};
    use crate::remove::{RemoveError, RemoveEvent};
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
            recheck: crate::model::Recheck::default(),
        }
    }

    fn reviewing() -> App {
        let mut a = App::new(
            PathBuf::from("/Users/u/Workspace"),
            vec![SourceId::Worktrees, SourceId::Docker],
        );
        let wt = Removal::Worktree {
            path: PathBuf::from("/Users/u/.codex/worktrees/glowz-robots"),
            repo: PathBuf::from("/Users/u/Workspace/glowz"),
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
        a.on_remove(RemoveEvent::Err(1, RemoveError::Changed));
        a.on_remove(RemoveEvent::Started(2));
        insta::assert_snapshot!(render(&a).backend());
    }

    #[test]
    fn snapshot_removing_while_stopping() {
        let mut a = reviewing();
        a.on_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE));
        a.on_remove(RemoveEvent::Started(3));
        a.on_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE));
        insta::assert_snapshot!(render(&a).backend());
    }

    fn done(fail: bool) -> App {
        let mut a = reviewing();
        a.on_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE));
        a.on_remove(RemoveEvent::Ok(3, 1_623_000_000));
        if fail {
            a.on_remove(RemoveEvent::Err(1, RemoveError::Changed));
        } else {
            a.on_remove(RemoveEvent::Ok(1, 412_000_000));
        }
        a.on_remove(RemoveEvent::Ok(2, 95_000_000));
        a.disk_free = (Some(41_000_000_000), Some(43_100_000_000));
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

    fn leftover(repo: &str, branch: &str) -> RemoveEvent {
        RemoveEvent::Leftover(crate::model::LeftoverBranch {
            repo: PathBuf::from(repo),
            branch: branch.into(),
            head: "5d69b47f".into(),
        })
    }

    /// Everything removed, two of the worktrees leaving a merged branch.
    fn done_with_leftovers() -> App {
        let mut a = reviewing();
        a.on_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE));
        a.on_remove(RemoveEvent::Ok(3, 1_623_000_000));
        a.on_remove(RemoveEvent::Ok(1, 412_000_000));
        a.on_remove(leftover("/Users/u/Workspace/glowz", "feat/robots-txt"));
        a.on_remove(RemoveEvent::Ok(2, 95_000_000));
        a.on_remove(leftover(
            "/Users/u/Workspace/glowz",
            "worktree-validated-tinkering-marble",
        ));
        a.disk_free = (Some(41_000_000_000), Some(43_100_000_000));
        a.on_remove(RemoveEvent::Finished);
        a
    }

    #[test]
    fn snapshot_done_offering_leftover_branches() {
        insta::assert_snapshot!(render(&done_with_leftovers()).backend());
    }

    #[test]
    fn snapshot_review_of_leftover_branches() {
        let mut a = done_with_leftovers();
        a.on_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::NONE));
        insta::assert_snapshot!(render(&a).backend());
    }

    #[test]
    fn snapshot_done_after_deleting_branches() {
        let mut a = done_with_leftovers();
        a.on_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::NONE));
        let crate::app::Action::StartRemoval(items) =
            a.on_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE))
        else {
            panic!("no removal");
        };
        a.on_remove(RemoveEvent::Ok(items[0].id, 0));
        a.on_remove(RemoveEvent::Err(items[1].id, RemoveError::Changed));
        a.on_remove(RemoveEvent::Finished);
        insta::assert_snapshot!(render(&a).backend());
    }
}
