//! Rendering. Every function here only reads the `App`.

pub mod list;
pub mod review;

use ratatui::Frame;
use ratatui::layout::{Alignment, Rect};
use ratatui::widgets::Paragraph;

use crate::app::{App, Screen};

pub const MIN_W: u16 = 40;
pub const MIN_H: u16 = 10;
pub const NARROW_W: u16 = 80;

pub fn draw(f: &mut Frame, app: &App) {
    let area = f.area();
    if area.width < MIN_W || area.height < MIN_H {
        let y = area.y + area.height / 2;
        let msg = Paragraph::new("Terminal too small").alignment(Alignment::Center);
        f.render_widget(
            msg,
            Rect {
                x: area.x,
                y,
                width: area.width,
                height: 1.min(area.height),
            },
        );
        return;
    }
    match app.screen {
        Screen::List => list::draw(f, app, area),
        Screen::Review => review::draw_review(f, app, area),
        Screen::Removing | Screen::Done => review::draw_progress(f, app, area),
    }
}
