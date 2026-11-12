// clippy hates bitflags
#![allow(clippy::suspicious_arithmetic_impl, clippy::redundant_field_names)]

use super::VisibleRowIndex;
#[cfg(feature = "use_serde")]
use serde::{Deserialize, Serialize};
use std::time::{Duration, Instant};
use wezterm_dynamic::{FromDynamic, ToDynamic};

pub use termwiz::input::{KeyCode, Modifiers as KeyModifiers};

#[cfg_attr(feature = "use_serde", derive(Deserialize, Serialize))]
#[derive(Debug, Copy, Clone, PartialEq, Eq, Ord, PartialOrd, Hash, FromDynamic, ToDynamic)]
pub enum MouseButton {
    Left,
    Middle,
    Right,
    WheelUp(usize),
    WheelDown(usize),
    WheelLeft(usize),
    WheelRight(usize),
    None,
}

#[cfg_attr(feature = "use_serde", derive(Deserialize, Serialize))]
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum MouseEventKind {
    Press,
    Release,
    Move,
}

#[cfg_attr(feature = "use_serde", derive(Deserialize, Serialize))]
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct MouseEvent {
    pub kind: MouseEventKind,
    pub x: usize,
    pub y: VisibleRowIndex,
    pub x_pixel_offset: isize,
    pub y_pixel_offset: isize,
    pub button: MouseButton,
    pub modifiers: KeyModifiers,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct ClickPosition {
    pub column: usize,
    pub row: i64,
    pub x_pixel_offset: isize,
    pub y_pixel_offset: isize,
}

/// This is a little helper that keeps track of the "click streak",
/// which is the number of successive clicks of the same mouse button
/// within the `CLICK_INTERVAL`.  The streak is reset to 1 each time
/// the mouse button differs from the last click, or when the elapsed
/// time exceeds `CLICK_INTERVAL`, or when the cursor position
/// changes to a different character cell.
#[derive(Debug, Clone)]
pub struct LastMouseClick {
    pub button: MouseButton,
    pub position: ClickPosition,
    time: Instant,
    pub streak: usize,
}

/// The multi-click interval, measured in milliseconds
const CLICK_INTERVAL: u64 = 500;

impl LastMouseClick {
    pub fn new(button: MouseButton, position: ClickPosition) -> Self {
        Self {
            button,
            position,
            time: Instant::now(),
            streak: 1,
        }
    }

    pub fn add(&self, button: MouseButton, position: ClickPosition) -> Self {
        self.add_with_interval(
            button,
            position,
            Duration::from_millis(CLICK_INTERVAL),
        )
    }

    pub fn add_with_interval(
        &self,
        button: MouseButton,
        position: ClickPosition,
        interval: Duration,
    ) -> Self {
        let now = Instant::now();
        let streak = if button == self.button
            && position.column == self.position.column
            && position.row == self.position.row
            && now.duration_since(self.time) <= interval
        {
            self.streak + 1
        } else {
            1
        };
        Self {
            button,
            position,
            time: now,
            streak,
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::MouseButton;

    fn pos(col: usize) -> ClickPosition {
        ClickPosition {
            column: col,
            row: 0,
            x_pixel_offset: 0,
            y_pixel_offset: 0,
        }
    }

    #[test]
    fn streak_increments_within_interval() {
        let click = LastMouseClick::new(MouseButton::Left, pos(1));
        let next = click.add_with_interval(
            MouseButton::Left,
            pos(1),
            Duration::from_secs(60),
        );
        assert_eq!(next.streak, 2);
        let third = next.add_with_interval(
            MouseButton::Left,
            pos(1),
            Duration::from_secs(60),
        );
        assert_eq!(third.streak, 3);
    }

    #[test]
    fn streak_resets_on_expiry_position_and_button() {
        let click = LastMouseClick::new(MouseButton::Left, pos(1));
        // A zero-length interval can never contain a subsequent click
        assert_eq!(
            click
                .add_with_interval(MouseButton::Left, pos(1), Duration::ZERO)
                .streak,
            1
        );
        // Different cell resets
        assert_eq!(
            click
                .add_with_interval(
                    MouseButton::Left,
                    pos(2),
                    Duration::from_secs(60)
                )
                .streak,
            1
        );
        // Different button resets
        assert_eq!(
            click
                .add_with_interval(
                    MouseButton::Right,
                    pos(1),
                    Duration::from_secs(60)
                )
                .streak,
            1
        );
    }
}
