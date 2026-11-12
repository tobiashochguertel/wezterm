use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_io::Timer;
use smithay_client_toolkit::compositor::SurfaceData;
use smithay_client_toolkit::reexports::csd_frame::{DecorationsFrame, FrameClick};
use smithay_client_toolkit::seat::touch::TouchHandler;
use wayland_client::backend::ObjectId;
use wayland_client::protocol::wl_pointer::ButtonState;
use wayland_client::protocol::wl_surface::WlSurface;
use wayland_client::protocol::wl_touch::WlTouch;
use wayland_client::{Connection, Proxy, QueueHandle};
use wezterm_input_types::MousePress;

use super::state::WaylandState;
use super::{SurfaceUserData, WaylandConnection};

/// How long the primary touch point must stay down and roughly in
/// place before it is converted into a held right button.
const LONG_PRESS_DELAY: Duration = Duration::from_millis(500);
/// Squared slop radius (in surface-local units) for the long-press
/// conversion: moving further than this cancels it and stays a drag.
const LONG_PRESS_SLOP: f64 = 10.0;
/// Surface-local drag distance that maps to one mouse wheel notch
/// during a two-finger scroll.
const SCROLL_PX_PER_NOTCH: f64 = 40.0;

/// Where a live touch id went down.
#[derive(Clone, Debug)]
pub(super) enum TouchTarget {
    /// Down landed on a decorations subsurface:
    /// (subsurface id, owning window id).
    Frame(ObjectId, usize),
    /// Down landed on a window's base surface.
    Content(ObjectId),
}

/// Maps the primary touch point of each surface onto the regular mouse
/// event pipeline: down -> Press(Left), motion -> Move, up -> Release(Left).
/// Secondary touch points are tracked (for cancel/up bookkeeping) but
/// otherwise ignored for now.
///
/// Touches that land on the client side decorations (title bar, borders)
/// are routed to the decorations frame instead, so touch can move/resize
/// the window and activate the frame buttons, mirroring pointer input.
impl TouchHandler for WaylandState {
    fn down(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _touch: &WlTouch,
        serial: u32,
        _time: u32,
        surface: WlSurface,
        id: i32,
        position: (f64, f64),
    ) {
        *self.last_serial.borrow_mut() = serial;
        if let Some(seat) = self.touch_seat.clone() {
            self.press_grab = Some((serial, seat));
        }
        let surface_id = surface.id();

        // Decoration subsurfaces route into the CSD frame, which hit-tests
        // the click and turns it into move/resize/button actions.
        if let Some(parent_surface) = surface
            .data::<SurfaceData>()
            .and_then(|data| data.parent_surface())
        {
            let window_id = SurfaceUserData::from_wl(parent_surface).window_id;
            let windows = self.windows.borrow();
            if let Some(window) = windows.get(&window_id) {
                let mut inner = window.borrow_mut();
                inner.window_frame.click_point_moved(
                    Duration::ZERO,
                    &surface_id,
                    position.0,
                    position.1,
                );
                if let Some(action) =
                    inner.window_frame.on_click(Duration::ZERO, FrameClick::Normal, true)
                {
                    if let Some(seat) = self.touch_seat.clone() {
                        inner.frame_action(&seat, serial, action);
                    }
                }
                self.touch_to_surface
                    .insert(id, TouchTarget::Frame(surface_id, window_id));
                return;
            }
        }

        self.touch_to_surface
            .insert(id, TouchTarget::Content(surface_id.clone()));
        if let Some(pending) = self.surface_to_touch.get(&surface_id) {
            let mut pending = pending.lock().unwrap();
            if pending.content_down(id, position) {
                // New primary point: arm the long-press conversion with
                // the owning window's config and schedule its timer.
                let window_id = pending.window_id;
                let (delay, slop) = {
                    let windows = self.windows.borrow();
                    windows
                        .get(&window_id)
                        .map(|w| {
                            let inner = w.borrow();
                            (
                                Duration::from_millis(
                                    inner.config.touch_long_press_ms,
                                ),
                                inner.config.touch_long_press_slop,
                            )
                        })
                        .unwrap_or((LONG_PRESS_DELAY, LONG_PRESS_SLOP))
                };
                let armed = pending.arm_long_press(delay, slop);
                drop(pending);
                if armed {
                    Self::schedule_long_press(window_id, delay);
                }
                self.dispatch_pending_touch(window_id);
            } else {
                // A second finger: the scroll speed is read once from
                // the owning window's config.
                pending.scroll_multiplier = {
                    let windows = self.windows.borrow();
                    windows
                        .get(&pending.window_id)
                        .map(|w| w.borrow().config.touch_scroll_multiplier)
                        .unwrap_or(1.0)
                };
            }
        }
    }

    fn up(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _touch: &WlTouch,
        serial: u32,
        _time: u32,
        id: i32,
    ) {
        *self.last_serial.borrow_mut() = serial;
        let Some(target) = self.touch_to_surface.remove(&id) else {
            return;
        };
        if self.touch_to_surface.is_empty() {
            self.press_grab = None;
        }

        match target {
            TouchTarget::Frame(_surface_id, window_id) => {
                let windows = self.windows.borrow();
                if let Some(window) = windows.get(&window_id) {
                    let mut inner = window.borrow_mut();
                    if let Some(action) =
                        inner.window_frame.on_click(Duration::ZERO, FrameClick::Normal, false)
                    {
                        if let Some(seat) = self.touch_seat.clone() {
                            inner.frame_action(&seat, serial, action);
                        }
                    }
                    inner.window_frame.click_point_left();
                }
            }
            TouchTarget::Content(surface_id) => {
                if let Some(pending) = self.surface_to_touch.get(&surface_id) {
                    let mut pending = pending.lock().unwrap();
                    if pending.content_up(id) {
                        self.dispatch_pending_touch(pending.window_id);
                    }
                }
            }
        }
    }

    fn motion(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _touch: &WlTouch,
        _time: u32,
        id: i32,
        position: (f64, f64),
    ) {
        let Some(target) = self.touch_to_surface.get(&id).cloned() else {
            return;
        };

        match target {
            TouchTarget::Frame(surface_id, window_id) => {
                let windows = self.windows.borrow();
                if let Some(window) = windows.get(&window_id) {
                    window.borrow_mut().window_frame.click_point_moved(
                        Duration::ZERO,
                        &surface_id,
                        position.0,
                        position.1,
                    );
                }
            }
            TouchTarget::Content(surface_id) => {
                if let Some(pending) = self.surface_to_touch.get(&surface_id) {
                    let mut pending = pending.lock().unwrap();
                    if pending.content_motion(id, position) {
                        self.dispatch_pending_touch(pending.window_id);
                    }
                }
            }
        }
    }

    fn shape(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _touch: &WlTouch,
        _id: i32,
        _major: f64,
        _minor: f64,
    ) {
    }

    fn orientation(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _touch: &WlTouch,
        _id: i32,
        _orientation: f64,
    ) {
    }

    fn cancel(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _touch: &WlTouch) {
        // The compositor cancelled the sequence (e.g. it took over a
        // gesture). Drop our touch points, but synthesize releases for any
        // still-pressed primaries so windows and the decorations frame
        // aren't left with a stuck held button.
        self.press_grab = None;
        for (id, target) in self.touch_to_surface.drain().collect::<Vec<(i32, TouchTarget)>>()
        {
            match target {
                TouchTarget::Frame(_surface_id, window_id) => {
                    let windows = self.windows.borrow();
                    if let Some(window) = windows.get(&window_id) {
                        let mut inner = window.borrow_mut();
                        inner
                            .window_frame
                            .on_click(Duration::ZERO, FrameClick::Normal, false);
                        inner.window_frame.click_point_left();
                    }
                }
                TouchTarget::Content(surface_id) => {
                    if let Some(pending) = self.surface_to_touch.get(&surface_id) {
                        let mut pending = pending.lock().unwrap();
                        if pending.content_up(id) {
                            self.dispatch_pending_touch(pending.window_id);
                        }
                    }
                }
            }
        }
    }
}

impl WaylandState {
    /// Schedule the window owning `window_id` to flush its buffered touch
    /// events. Mirrors the PointerEventKind::queue -> with_window_inner
    /// pattern used for pointer input: events are pushed under the lock
    /// here and dispatched later on the main thread.
    fn dispatch_pending_touch(&self, window_id: usize) {
        WaylandConnection::with_window_inner(window_id, move |inner| {
            inner.dispatch_pending_touch();
            Ok(())
        });
    }

    /// Arm the long-press detector for a touch that just went down on
    /// `window_id`. If the primary point is still held and hasn't left
    /// the slop radius when the delay expires, the held left button is
    /// converted into a held right button: Release(Left)+Press(Right).
    /// That mirrors how touch platforms map press-and-hold to a
    /// secondary click and unlocks right-click-only behavior such as
    /// the tab navigator and application mouse reporting. Dragging
    /// while held continues as a right-drag, and lifting the finger
    /// releases the right button instead of the left.
    fn schedule_long_press(window_id: usize, delay: Duration) {
        promise::spawn::spawn_into_main_thread(async move {
            Timer::after(delay).await;
            WaylandConnection::with_window_inner(window_id, |inner| {
                let pending = Arc::clone(&inner.pending_touch);
                let fired = pending.lock().unwrap().try_long_press();
                if fired {
                    inner.dispatch_pending_touch();
                }
                Ok(())
            });
        })
        .detach();
    }
}

/// Per-window buffered touch state, mirroring PendingMouse.
#[derive(Clone, Debug)]
pub struct PendingTouch {
    window_id: usize,
    /// Touch id acting as the pointer for this surface, if any is down.
    primary_id: Option<i32>,
    /// Most recent surface-local coordinates of the primary point.
    coords: Option<(f64, f64)>,
    /// Queued (state, button) transitions to dispatch.
    clicks: Vec<(ButtonState, MousePress)>,
    /// Where the primary point went down; anchors the long-press
    /// slop radius.
    down_pos: Option<(f64, f64)>,
    /// Squared slop radius (from `touch_long_press_slop`) read when
    /// the primary went down.
    long_press_slop_sq: f64,
    /// The long-press conversion timer may still fire.
    long_press_armed: bool,
    /// The long-press timer converted this touch into a held right
    /// button; the matching release must be for the right button.
    right_held: bool,
    /// Last known position of every live touch point on this surface,
    /// used to compute two-finger scroll deltas.
    last_positions: std::collections::HashMap<i32, (f64, f64)>,
    /// Fractional scroll distance not yet emitted as whole wheel
    /// notches.
    scroll_acc: f64,
    /// `touch_scroll_multiplier` for the owning window, read when the
    /// second touch point went down. 0 disables two-finger scroll.
    scroll_multiplier: f64,
    /// Queued wheel notches to dispatch (positive = WheelUp).
    wheels: Vec<i16>,
}

impl PendingTouch {
    /// `id` went down at `pos` on this content surface. Records the
    /// point; if it becomes the primary point, queues `Press(Left)`
    /// and returns true — the caller should then apply the window's
    /// config via `arm_long_press` and schedule the conversion timer.
    /// Otherwise this is a secondary point: any pending long-press is
    /// cancelled and subsequent motion scrolls (see `content_motion`).
    fn content_down(&mut self, id: i32, pos: (f64, f64)) -> bool {
        self.last_positions.insert(id, pos);
        if self.primary_id.is_none() {
            self.primary_id = Some(id);
            self.coords = Some(pos);
            self.down_pos = Some(pos);
            self.right_held = false;
            self.clicks.push((ButtonState::Pressed, MousePress::Left));
            true
        } else {
            self.long_press_armed = false;
            self.scroll_acc = 0.0;
            false
        }
    }

    /// Configure the pending long-press conversion. `delay` of zero
    /// disables it. Returns whether the conversion is armed.
    fn arm_long_press(&mut self, delay: Duration, slop: f64) -> bool {
        self.long_press_slop_sq = slop * slop;
        self.long_press_armed = delay > Duration::ZERO;
        self.long_press_armed
    }

    /// `id` moved to `pos`. With a single point this is a primary drag
    /// (updates coords for the next Move event and cancels the
    /// long-press once the slop radius is left). With two or more
    /// points, vertical motion of the moving point — averaged across
    /// the live points — accumulates into wheel notches: content
    /// follows the fingers, so dragging down scrolls back toward older
    /// output (positive WheelUp notches). Returns true when the queued
    /// output should be dispatched.
    fn content_motion(&mut self, id: i32, pos: (f64, f64)) -> bool {
        let count = self.last_positions.len();
        let Some(last) = self.last_positions.get(&id).copied() else {
            return false;
        };
        self.last_positions.insert(id, pos);
        if count >= 2 {
            let dy = pos.1 - last.1;
            self.scroll_acc += dy * self.scroll_multiplier / count as f64;
            while self.scroll_acc.abs() >= SCROLL_PX_PER_NOTCH {
                let notch = self.scroll_acc.signum() as i16;
                self.wheels.push(notch);
                self.scroll_acc -= SCROLL_PX_PER_NOTCH * self.scroll_acc.signum();
            }
            true
        } else if self.primary_id == Some(id) {
            if let Some(down) = self.down_pos {
                let dx = pos.0 - down.0;
                let dy = pos.1 - down.1;
                if dx * dx + dy * dy > self.long_press_slop_sq {
                    self.long_press_armed = false;
                }
            }
            self.coords = Some(pos);
            true
        } else {
            false
        }
    }

    /// `id` was lifted. Dropping it from the point set; if it was the
    /// primary, queues the matching button release — `Release(Right)`
    /// when the long-press conversion fired, `Release(Left)` otherwise.
    /// Also used for `wl_touch.cancel`, which drops every point.
    fn content_up(&mut self, id: i32) -> bool {
        self.last_positions.remove(&id);
        if self.primary_id.take() == Some(id) {
            self.long_press_armed = false;
            let button = if self.right_held {
                self.right_held = false;
                MousePress::Right
            } else {
                MousePress::Left
            };
            self.clicks.push((ButtonState::Released, button));
            true
        } else {
            false
        }
    }

    /// The long-press delay elapsed. If the gesture is still armed
    /// (primary held, no drag, no second finger), convert the held
    /// left button into a held right button: Release(Left)+Press(Right)
    /// and return true so the caller dispatches.
    fn try_long_press(&mut self) -> bool {
        if self.long_press_armed && self.primary_id.is_some() {
            self.long_press_armed = false;
            self.right_held = true;
            self.clicks.push((ButtonState::Released, MousePress::Left));
            self.clicks.push((ButtonState::Pressed, MousePress::Right));
            true
        } else {
            false
        }
    }

    pub(super) fn create(window_id: usize) -> Arc<Mutex<Self>> {
        Arc::new(Mutex::new(Self {
            window_id,
            primary_id: None,
            coords: None,
            clicks: vec![],
            down_pos: None,
            long_press_slop_sq: LONG_PRESS_SLOP * LONG_PRESS_SLOP,
            long_press_armed: false,
            right_held: false,
            last_positions: std::collections::HashMap::new(),
            scroll_acc: 0.0,
            scroll_multiplier: 1.0,
            wheels: vec![],
        }))
    }

    pub(super) fn next_click(
        pending: &Arc<Mutex<Self>>,
    ) -> Option<(ButtonState, MousePress)> {
        let mut pending = pending.lock().unwrap();
        if pending.clicks.is_empty() {
            None
        } else {
            Some(pending.clicks.remove(0))
        }
    }

    pub(super) fn next_wheel(pending: &Arc<Mutex<Self>>) -> Option<i16> {
        let mut pending = pending.lock().unwrap();
        if pending.wheels.is_empty() {
            None
        } else {
            Some(pending.wheels.remove(0))
        }
    }

    pub(super) fn coords(pending: &Arc<Mutex<Self>>) -> Option<(f64, f64)> {
        pending.lock().unwrap().coords.take()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pending() -> PendingTouch {
        PendingTouch {
            window_id: 1,
            primary_id: None,
            coords: None,
            clicks: vec![],
            down_pos: None,
            long_press_slop_sq: LONG_PRESS_SLOP * LONG_PRESS_SLOP,
            long_press_armed: false,
            right_held: false,
            last_positions: std::collections::HashMap::new(),
            scroll_acc: 0.0,
            scroll_multiplier: 1.0,
            wheels: vec![],
        }
    }

    #[test]
    fn tap_queues_left_press_and_release() {
        let mut p = pending();
        assert!(p.content_down(0, (10.0, 20.0)));
        assert_eq!(
            p.clicks,
            vec![(ButtonState::Pressed, MousePress::Left)]
        );
        assert!(p.content_up(0));
        assert_eq!(
            p.clicks,
            vec![
                (ButtonState::Pressed, MousePress::Left),
                (ButtonState::Released, MousePress::Left),
            ]
        );
    }

    #[test]
    fn second_point_cancels_long_press() {
        let mut p = pending();
        assert!(p.content_down(0, (10.0, 10.0)));
        assert!(p.arm_long_press(Duration::from_millis(500), 10.0));
        assert!(!p.content_down(1, (10.0, 30.0)));
        assert!(!p.try_long_press());
    }

    #[test]
    fn long_press_converts_to_right_button() {
        let mut p = pending();
        p.content_down(0, (10.0, 10.0));
        p.arm_long_press(Duration::from_millis(500), 10.0);
        assert!(p.try_long_press());
        assert_eq!(
            p.clicks,
            vec![
                (ButtonState::Pressed, MousePress::Left),
                (ButtonState::Released, MousePress::Left),
                (ButtonState::Pressed, MousePress::Right),
            ]
        );
        // Lifting the finger releases the right button, not the left
        assert!(p.content_up(0));
        assert_eq!(
            p.clicks.last(),
            Some(&(ButtonState::Released, MousePress::Right))
        );
    }

    #[test]
    fn drag_beyond_slop_cancels_long_press() {
        let mut p = pending();
        p.content_down(0, (10.0, 10.0));
        p.arm_long_press(Duration::from_millis(500), 10.0);
        assert!(p.content_motion(0, (10.0, 25.0)));
        assert!(!p.try_long_press());
        // ...but a small move inside the radius keeps it armed
        let mut q = pending();
        q.content_down(0, (10.0, 10.0));
        q.arm_long_press(Duration::from_millis(500), 10.0);
        q.content_motion(0, (13.0, 13.0));
        assert!(q.try_long_press());
    }

    #[test]
    fn zero_delay_disables_long_press() {
        let mut p = pending();
        p.content_down(0, (10.0, 10.0));
        assert!(!p.arm_long_press(Duration::ZERO, 10.0));
        assert!(!p.try_long_press());
    }

    #[test]
    fn two_finger_drag_emits_wheel_notches() {
        let mut p = pending();
        p.content_down(0, (100.0, 100.0));
        p.content_down(1, (100.0, 140.0));
        // Both fingers drag down 200px -> +5 notches of WheelUp
        // (200 * 1.0 / 2 points each = 200 accumulated, 40 per notch)
        for i in 1..=10 {
            p.content_motion(0, (100.0, 100.0 + i as f64 * 20.0));
            p.content_motion(1, (100.0, 140.0 + i as f64 * 20.0));
        }
        assert_eq!(p.wheels.iter().sum::<i16>(), 5);
        // Dragging back up produces WheelDown
        for i in 1..=10 {
            p.content_motion(0, (100.0, 300.0 - i as f64 * 20.0));
            p.content_motion(1, (100.0, 340.0 - i as f64 * 20.0));
        }
        assert_eq!(p.wheels.iter().sum::<i16>(), 0);
    }

    #[test]
    fn single_finger_drag_does_not_scroll() {
        let mut p = pending();
        p.content_down(0, (100.0, 100.0));
        p.content_motion(0, (100.0, 300.0));
        assert!(p.wheels.is_empty());
        assert_eq!(p.coords, Some((100.0, 300.0)));
    }

    #[test]
    fn scroll_multiplier_scales_speed() {
        let mut p = pending();
        p.scroll_multiplier = 2.0;
        p.content_down(0, (0.0, 0.0));
        p.content_down(1, (0.0, 50.0));
        p.content_motion(0, (0.0, 200.0));
        assert_eq!(p.wheels.iter().sum::<i16>(), 5);
        // ...and 0 disables scrolling
        let mut q = pending();
        q.scroll_multiplier = 0.0;
        q.content_down(0, (0.0, 0.0));
        q.content_down(1, (0.0, 50.0));
        q.content_motion(0, (0.0, 400.0));
        assert!(q.wheels.is_empty());
    }

    #[test]
    fn cancel_releases_matching_button() {
        // content_up doubles as the per-point cancel path
        let mut p = pending();
        p.content_down(0, (0.0, 0.0));
        p.arm_long_press(Duration::from_millis(500), 10.0);
        p.try_long_press();
        assert!(p.content_up(0));
        assert_eq!(
            p.clicks.last(),
            Some(&(ButtonState::Released, MousePress::Right))
        );
        assert!(!p.right_held);
    }
}
