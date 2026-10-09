//! Voice profile editor: the learned band shape (line, with its usual spread
//! shaded), the mic right now (bars, normalised the same way, so you see
//! exactly what gets compared), drag to reshape, right-click to ignore a band.

use iced::mouse;
use iced::widget::canvas::{self, Action, Event, Frame, Geometry, Path, Stroke, Text};
use iced::{Color, Point, Rectangle, Renderer, Size, Theme, alignment};

use super::Message;
use super::theme::{ACCENT, BORDER, FAINT, GREEN, MUTED, RED, alpha};
use crate::audio::ProfileView;
use crate::audio::dsp::profile::{BANDS, EDIT_RANGE_DB, band_centers};

const LEFT: f32 = 30.0;
const RIGHT: f32 = 6.0;
const TOP: f32 = 8.0;
const BOTTOM: f32 = 18.0;
/// Frequencies labelled under the chart.
const LABELS: [(f32, &str); 6] =
    [(100.0, "100"), (500.0, "500"), (1000.0, "1k"), (2000.0, "2k"), (4000.0, "4k"), (7000.0, "7k")];

pub struct ProfileChart<'a> {
    pub view: &'a ProfileView,
}

#[derive(Default)]
pub struct State {
    dragging: bool,
    hover: Option<usize>,
}

struct Plot {
    x: f32,
    y: f32,
    w: f32,
    h: f32,
}

impl Plot {
    fn new(size: Size) -> Self {
        Self { x: LEFT, y: TOP, w: (size.width - LEFT - RIGHT).max(1.0), h: (size.height - TOP - BOTTOM).max(1.0) }
    }

    fn col(&self) -> f32 {
        self.w / BANDS as f32
    }

    fn x_of(&self, band: usize) -> f32 {
        self.x + (band as f32 + 0.5) * self.col()
    }

    fn y_of(&self, db: f32) -> f32 {
        let db = db.clamp(-EDIT_RANGE_DB, EDIT_RANGE_DB);
        self.y + (EDIT_RANGE_DB - db) / (2.0 * EDIT_RANGE_DB) * self.h
    }

    fn db_at(&self, y: f32) -> f32 {
        EDIT_RANGE_DB - (y - self.y) / self.h * 2.0 * EDIT_RANGE_DB
    }

    fn band_at(&self, x: f32) -> Option<usize> {
        let b = ((x - self.x) / self.col()).floor();
        (b >= 0.0 && b < BANDS as f32).then_some(b as usize)
    }
}

/// Cursor relative to the canvas, even outside it (drags keep going).
fn local(cursor: mouse::Cursor, bounds: Rectangle) -> Option<Point> {
    cursor.position().map(|p| Point::new(p.x - bounds.x, p.y - bounds.y))
}

impl canvas::Program<Message> for ProfileChart<'_> {
    type State = State;

    fn update(&self, state: &mut State, event: &Event, bounds: Rectangle, cursor: mouse::Cursor) -> Option<Action<Message>> {
        let plot = Plot::new(bounds.size());
        let inside = cursor.position_in(bounds);
        match event {
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)) => {
                let p = inside?;
                let band = plot.band_at(p.x)?;
                state.dragging = true;
                Some(Action::publish(Message::ProfileBand(band, plot.db_at(p.y))).and_capture())
            }
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Right)) => {
                let band = plot.band_at(inside?.x)?;
                Some(Action::publish(Message::ProfileIgnore(band)).and_capture())
            }
            Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)) if state.dragging => {
                state.dragging = false;
                Some(Action::capture())
            }
            Event::Mouse(mouse::Event::CursorMoved { .. }) => {
                let hover = inside.and_then(|p| plot.band_at(p.x));
                if state.dragging {
                    // Sweeping across bands paints each one it passes.
                    let p = local(cursor, bounds)?;
                    let band = plot.band_at(p.x.clamp(plot.x, plot.x + plot.w - 0.1))?;
                    state.hover = Some(band);
                    return Some(Action::publish(Message::ProfileBand(band, plot.db_at(p.y))).and_capture());
                }
                if hover != state.hover {
                    state.hover = hover;
                    return Some(Action::request_redraw());
                }
                None
            }
            _ => None,
        }
    }

    fn draw(&self, state: &State, renderer: &Renderer, _: &Theme, bounds: Rectangle, _: mouse::Cursor) -> Vec<Geometry> {
        let v = self.view;
        let plot = Plot::new(bounds.size());
        let mut frame = Frame::new(renderer, bounds.size());
        let col = plot.col();

        // Grid and dB labels.
        for db in [-20.0f32, -10.0, 0.0, 10.0, 20.0] {
            let y = plot.y_of(db);
            let line = Path::line(Point::new(plot.x, y), Point::new(plot.x + plot.w, y));
            let width = if db == 0.0 { 1.0 } else { 0.5 };
            frame.stroke(&line, Stroke::default().with_color(BORDER).with_width(width));
            frame.fill_text(Text {
                content: if db > 0.0 { format!("+{db:.0}") } else { format!("{db:.0}") },
                position: Point::new(plot.x - 6.0, y),
                color: FAINT,
                size: 10.0.into(),
                align_x: alignment::Horizontal::Right.into(),
                align_y: alignment::Vertical::Center,
                ..Text::default()
            });
        }

        // Ignored bands and the hovered one.
        for b in 0..BANDS {
            let x0 = plot.x + b as f32 * col;
            if v.ignored[b] {
                frame.fill_rectangle(Point::new(x0, plot.y), Size::new(col, plot.h), alpha(RED, 0.10));
            } else if state.hover == Some(b) {
                frame.fill_rectangle(Point::new(x0, plot.y), Size::new(col, plot.h), alpha(Color::WHITE, 0.04));
            }
        }

        // The mic right now, as bars from the average (0 dB).
        let zero = plot.y_of(0.0);
        let bar = if v.speaking { alpha(GREEN, 0.55) } else { alpha(MUTED, 0.28) };
        for b in (0..BANDS).filter(|b| !v.ignored[*b]) {
            let y = plot.y_of(v.live[b]);
            let (top, h) = if y < zero { (y, zero - y) } else { (zero, y - zero) };
            frame.fill_rectangle(Point::new(plot.x + b as f32 * col + col * 0.2, top), Size::new(col * 0.6, h.max(1.0)), bar);
        }

        // Learned shape: spread shaded, mean as a line with handles.
        let points: Vec<usize> = (0..BANDS).filter(|b| !v.ignored[*b]).collect();
        if points.len() >= 2 {
            let spread = Path::new(|p| {
                for (n, &b) in points.iter().enumerate() {
                    let pt = Point::new(plot.x_of(b), plot.y_of(v.mean[b] + v.spread[b]));
                    if n == 0 { p.move_to(pt) } else { p.line_to(pt) }
                }
                for &b in points.iter().rev() {
                    p.line_to(Point::new(plot.x_of(b), plot.y_of(v.mean[b] - v.spread[b])));
                }
                p.close();
            });
            frame.fill(&spread, alpha(ACCENT, 0.14));
            let line = Path::new(|p| {
                for (n, &b) in points.iter().enumerate() {
                    let pt = Point::new(plot.x_of(b), plot.y_of(v.mean[b]));
                    if n == 0 { p.move_to(pt) } else { p.line_to(pt) }
                }
            });
            frame.stroke(&line, Stroke::default().with_color(ACCENT).with_width(2.0));
        }
        for b in 0..BANDS {
            let c = Point::new(plot.x_of(b), plot.y_of(if v.ignored[b] { 0.0 } else { v.mean[b] }));
            if v.ignored[b] {
                let d = 3.5;
                let x = Path::new(|p| {
                    p.move_to(Point::new(c.x - d, c.y - d));
                    p.line_to(Point::new(c.x + d, c.y + d));
                    p.move_to(Point::new(c.x + d, c.y - d));
                    p.line_to(Point::new(c.x - d, c.y + d));
                });
                frame.stroke(&x, Stroke::default().with_color(alpha(RED, 0.8)).with_width(1.5));
            } else {
                let r = if state.hover == Some(b) { 5.0 } else { 3.0 };
                frame.fill(&Path::circle(c, r), ACCENT);
            }
        }

        // Frequency labels under the nearest band.
        let centers = band_centers();
        for (hz, label) in LABELS {
            let b = (0..BANDS)
                .min_by(|a, b| (centers[*a] - hz).abs().total_cmp(&(centers[*b] - hz).abs()))
                .unwrap_or(0);
            frame.fill_text(Text {
                content: label.into(),
                position: Point::new(plot.x_of(b), plot.y + plot.h + 4.0),
                color: FAINT,
                size: 10.0.into(),
                align_x: alignment::Horizontal::Center.into(),
                ..Text::default()
            });
        }
        if !v.usable {
            frame.fill_text(Text {
                content: "Still learning: talk for a few seconds".into(),
                position: Point::new(plot.x + plot.w / 2.0, plot.y + 4.0),
                color: MUTED,
                size: 11.0.into(),
                align_x: alignment::Horizontal::Center.into(),
                ..Text::default()
            });
        }
        vec![frame.into_geometry()]
    }

    fn mouse_interaction(&self, state: &State, bounds: Rectangle, cursor: mouse::Cursor) -> mouse::Interaction {
        if state.dragging {
            mouse::Interaction::Grabbing
        } else if cursor.position_in(bounds).is_some() && state.hover.is_some() {
            mouse::Interaction::Pointer
        } else {
            mouse::Interaction::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use iced::widget::canvas::Program;

    // 24 bands of 20 px, 2 px per dB: easy numbers.
    const BOUNDS: Rectangle = Rectangle {
        x: 0.0,
        y: 0.0,
        width: LEFT + RIGHT + 480.0,
        height: TOP + BOTTOM + 120.0,
    };

    fn at(band: usize, db: f32) -> mouse::Cursor {
        let x = LEFT + band as f32 * 20.0 + 10.0;
        let y = TOP + (EDIT_RANGE_DB - db) * 2.0;
        mouse::Cursor::Available(Point::new(x, y))
    }

    fn send(chart: &ProfileChart, state: &mut State, event: mouse::Event, cursor: mouse::Cursor) -> Option<Message> {
        chart.update(state, &Event::Mouse(event), BOUNDS, cursor)?.into_inner().0
    }

    #[test]
    fn click_drag_and_right_click_edit_the_right_bands() {
        let view = ProfileView::default();
        let chart = ProfileChart { view: &view };
        let mut state = State::default();

        let press = send(&chart, &mut state, mouse::Event::ButtonPressed(mouse::Button::Left), at(5, 10.0));
        assert!(matches!(press, Some(Message::ProfileBand(5, db)) if (db - 10.0).abs() < 0.01), "{press:?}");

        // Sweeping right while held paints the next band.
        let moved = send(&chart, &mut state, mouse::Event::CursorMoved { position: Point::ORIGIN }, at(6, -12.0));
        assert!(matches!(moved, Some(Message::ProfileBand(6, db)) if (db + 12.0).abs() < 0.01), "{moved:?}");

        // Released: moving no longer edits.
        send(&chart, &mut state, mouse::Event::ButtonReleased(mouse::Button::Left), at(6, -12.0));
        let hover = send(&chart, &mut state, mouse::Event::CursorMoved { position: Point::ORIGIN }, at(7, 0.0));
        assert!(hover.is_none(), "{hover:?}");

        let right = send(&chart, &mut state, mouse::Event::ButtonPressed(mouse::Button::Right), at(20, 0.0));
        assert!(matches!(right, Some(Message::ProfileIgnore(20))), "{right:?}");
    }

    #[test]
    fn clicks_outside_the_plot_do_nothing() {
        let view = ProfileView::default();
        let chart = ProfileChart { view: &view };
        let mut state = State::default();
        let left_margin = mouse::Cursor::Available(Point::new(LEFT / 2.0, 50.0));
        assert!(send(&chart, &mut state, mouse::Event::ButtonPressed(mouse::Button::Left), left_margin).is_none());
        assert!(!state.dragging);
    }
}
