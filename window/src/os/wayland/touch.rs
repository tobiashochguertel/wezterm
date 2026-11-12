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
            pending.last_positions.insert(id, position);
            // The first live touch point acts as the pointer; later
            // concurrent points join the surface's point set — with two
            // or more down, vertical motion scrolls (see `motion`).
            if pending.primary_id.is_none() {
                pending.primary_id = Some(id);
                pending.coords = Some(position);
                pending.down_pos = Some(position);
                pending.right_held = false;
                pending.clicks.push((ButtonState::Pressed, MousePress::Left));
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
                pending.long_press_slop_sq = slop * slop;
                // A delay of 0 disables the long-press conversion
                pending.long_press_armed = delay > Duration::ZERO;
                drop(pending);
                if delay > Duration::ZERO {
                    Self::schedule_long_press(window_id, delay);
                }
                self.dispatch_pending_touch(window_id);
            } else {
                // A second finger turns pending gestures into a scroll:
                // the long-press hold no longer applies, and the scroll
                // speed is read once from the owning window's config.
                pending.long_press_armed = false;
                pending.scroll_acc = 0.0;
                let window_id = pending.window_id;
                pending.scroll_multiplier = {
                    let windows = self.windows.borrow();
                    windows
                        .get(&window_id)
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
                    pending.last_positions.remove(&id);
                    if pending.primary_id == Some(id) {
                        pending.primary_id = None;
                        pending.long_press_armed = false;
                        let button = if pending.right_held {
                            pending.right_held = false;
                            MousePress::Right
                        } else {
                            MousePress::Left
                        };
                        pending.clicks.push((ButtonState::Released, button));
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
                    let count = pending.last_positions.len();
                    let Some(last) = pending.last_positions.get(&id).copied()
                    else {
                        return;
                    };
                    pending.last_positions.insert(id, position);
                    if count >= 2 {
                        // Two-finger drag: convert the average vertical
                        // motion of the live points into wheel scroll.
                        // Content follows the fingers — dragging down
                        // scrolls back toward older output (WheelUp).
                        let dy = position.1 - last.1;
                        pending.scroll_acc +=
                            dy * pending.scroll_multiplier / count as f64;
                        while pending.scroll_acc.abs() >= SCROLL_PX_PER_NOTCH {
                            let notch = pending.scroll_acc.signum() as i16;
                            pending.wheels.push(notch);
                            pending.scroll_acc -=
                                SCROLL_PX_PER_NOTCH * pending.scroll_acc.signum();
                        }
                        self.dispatch_pending_touch(pending.window_id);
                    } else if pending.primary_id == Some(id) {
                        if let Some(down) = pending.down_pos {
                            let dx = position.0 - down.0;
                            let dy = position.1 - down.1;
                            if dx * dx + dy * dy > pending.long_press_slop_sq {
                                pending.long_press_armed = false;
                            }
                        }
                        pending.coords = Some(position);
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
                        pending.last_positions.remove(&id);
                        if pending.primary_id.take().is_some() {
                            pending.long_press_armed = false;
                            let button = if pending.right_held {
                                pending.right_held = false;
                                MousePress::Right
                            } else {
                                MousePress::Left
                            };
                            pending.clicks.push((ButtonState::Released, button));
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
                let fired = {
                    let mut pending = pending.lock().unwrap();
                    if pending.long_press_armed && pending.primary_id.is_some() {
                        pending.long_press_armed = false;
                        pending.right_held = true;
                        pending
                            .clicks
                            .push((ButtonState::Released, MousePress::Left));
                        pending.clicks.push((ButtonState::Pressed, MousePress::Right));
                        true
                    } else {
                        false
                    }
                };
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
