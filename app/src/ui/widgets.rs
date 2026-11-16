//! Small layout helpers and the ticker that wakes the UI for backend events.

use noa_gpui_platform::noa_gpui_core::style::{
    AlignItems, Dimension, FlexDirection, FlexWrap, LengthPercentage, Position, Rect, Size, length,
    percent,
};
use noa_gpui_platform::noa_gpui_core::widgets::Container;
use noa_gpui_platform::noa_gpui_core::{Event, EventCx, PaintCx, Role, Style, Widget};

use super::tokens;

/// Sent by [`Ticker`] when the timer it was given is due.
#[derive(Debug)]
pub struct Tick;

/// An invisible widget: the target of the window's keyboard shortcuts, and a clock
/// for the elapsed time shown while the daemon starts (arm it with
/// `UpdateCx::request_timer`).
pub struct Ticker;

impl Widget for Ticker {
    fn style(&self) -> Style {
        Style {
            position: Position::Absolute,
            size: Size {
                width: length(0.0),
                height: length(0.0),
            },
            ..Default::default()
        }
    }

    fn paint(&mut self, _cx: &mut PaintCx) {}

    fn event(&mut self, cx: &mut EventCx, event: &Event) {
        if let Event::Timer { .. } = event {
            cx.emit(Tick);
        }
    }
}

fn gap(px: f32) -> Size<LengthPercentage> {
    Size {
        width: length(px),
        height: length(px),
    }
}

fn pad(px: f32) -> Rect<LengthPercentage> {
    Rect {
        left: length(px),
        right: length(px),
        top: length(px),
        bottom: length(px),
    }
}

/// The monospaced font for IDs, ports, sizes and logs (noa-gpui picks the system's:
/// SF Mono or Menlo on macOS).
pub const MONO: &str = "monospace";

/// A horizontal run of controls, vertically centered.
pub fn row(spacing: f32) -> Container {
    Container::new(Style {
        flex_direction: FlexDirection::Row,
        align_items: AlignItems::CENTER,
        gap: gap(spacing),
        flex_shrink: 0.0,
        ..Default::default()
    })
}

/// A horizontal run of controls that wraps onto a new line when narrow.
pub fn wrap_row(spacing: f32) -> Container {
    Container::new(Style {
        flex_direction: FlexDirection::Row,
        flex_wrap: FlexWrap::Wrap,
        align_items: AlignItems::CENTER,
        gap: gap(spacing),
        flex_shrink: 0.0,
        ..Default::default()
    })
}

pub fn column(spacing: f32) -> Container {
    Container::new(Style {
        flex_direction: FlexDirection::Column,
        gap: gap(spacing),
        ..Default::default()
    })
}

/// A column that takes the remaining space of its parent.
pub fn fill_column(spacing: f32, padding: f32) -> Container {
    Container::new(Style {
        flex_direction: FlexDirection::Column,
        gap: gap(spacing),
        padding: pad(padding),
        flex_grow: 1.0,
        flex_basis: length(0.0),
        min_size: Size {
            width: length(0.0),
            height: length(0.0),
        },
        ..Default::default()
    })
}

/// Pushes the controls after it to the far end of a row.
pub fn spacer() -> Container {
    Container::new(Style {
        flex_grow: 1.0,
        ..Default::default()
    })
}

pub fn full() -> Size<Dimension> {
    Size {
        width: percent(1.0),
        height: percent(1.0),
    }
}

/// A wrapping row of controls with the page's side padding (headers, toolbars and
/// notes over a list that runs to the window's edges).
pub fn bar(spacing: f32) -> Container {
    Container::new(Style {
        flex_direction: FlexDirection::Row,
        flex_wrap: FlexWrap::Wrap,
        align_items: AlignItems::CENTER,
        gap: gap(spacing),
        padding: padding_xy(tokens::PAGE_X, tokens::PAGE_Y),
        flex_shrink: 0.0,
        ..Default::default()
    })
}

/// A horizontal line across its parent, `px` thick.
pub fn rule(px: f32, color: Role) -> Container {
    Container::new(Style {
        size: Size {
            width: percent(1.0),
            height: length(px),
        },
        flex_shrink: 0.0,
        ..Default::default()
    })
    .background(color)
}

/// A vertical line down its parent, `px` thick.
pub fn vrule(px: f32, color: Role) -> Container {
    Container::new(Style {
        size: Size {
            width: length(px),
            height: percent(1.0),
        },
        flex_shrink: 0.0,
        ..Default::default()
    })
    .background(color)
}

/// A pane beside a list: `fraction` of the row's width (at least `min` px), its
/// children stacked vertically after a dividing line on its left.
pub fn side_pane(fraction: f32, min: f32) -> Container {
    Container::new(side_pane_style(fraction, min))
}

pub fn side_pane_style(fraction: f32, min: f32) -> Style {
    Style {
        flex_direction: FlexDirection::Row,
        gap: gap(tokens::SPACE_4),
        size: Size {
            width: percent(fraction),
            height: percent(1.0),
        },
        min_size: Size {
            width: length(min),
            height: length(0.0),
        },
        flex_shrink: 0.0,
        ..Default::default()
    }
}

/// A row that takes the remaining height of its parent (a list and its side pane).
pub fn fill_row(spacing: f32) -> Container {
    Container::new(Style {
        flex_direction: FlexDirection::Row,
        gap: gap(spacing),
        flex_grow: 1.0,
        flex_basis: length(0.0),
        min_size: Size {
            width: length(0.0),
            height: length(0.0),
        },
        ..Default::default()
    })
}

/// Padding of `vertical` px above and below and `horizontal` px at the sides.
pub fn padding_xy(horizontal: f32, vertical: f32) -> Rect<LengthPercentage> {
    Rect {
        left: length(horizontal),
        right: length(horizontal),
        top: length(vertical),
        bottom: length(vertical),
    }
}

pub fn spacing(px: f32) -> Size<LengthPercentage> {
    gap(px)
}
