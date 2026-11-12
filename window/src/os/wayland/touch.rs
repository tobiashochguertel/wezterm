use std::sync::{Arc, Mutex};
use std::time::Duration;

use smithay_client_toolkit::compositor::SurfaceData;
use smithay_client_toolkit::reexports::csd_frame::{DecorationsFrame, FrameClick};
use smithay_client_toolkit::seat::touch::TouchHandler;
use wayland_client::backend::ObjectId;
use wayland_client::protocol::wl_pointer::ButtonState;
use wayland_client::protocol::wl_surface::WlSurface;
use wayland_client::protocol::wl_touch::WlTouch;
use wayland_client::{Connection, Proxy, QueueHandle};

use super::state::WaylandState;
use super::{SurfaceUserData, WaylandConnection};

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
        self.last_seat = self.touch_seat.clone();
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
            // The first live touch point acts as the pointer; later
            // concurrent points on this surface are ignored until it ends
            if pending.primary_id.is_none() {
                pending.primary_id = Some(id);
                pending.coords = Some(position);
                pending.clicks.push(ButtonState::Pressed);
                self.dispatch_pending_touch(pending.window_id);
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
        self.last_seat = self.touch_seat.clone();
        let Some(target) = self.touch_to_surface.remove(&id) else {
            return;
        };

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
                    if pending.primary_id == Some(id) {
                        pending.primary_id = None;
                        pending.clicks.push(ButtonState::Released);
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
                    if pending.primary_id == Some(id) {
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
        for (_, target) in self.touch_to_surface.drain().collect::<Vec<(i32, TouchTarget)>>()
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
                        if pending.primary_id.take().is_some() {
                            pending.clicks.push(ButtonState::Released);
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
}

/// Per-window buffered touch state, mirroring PendingMouse.
#[derive(Clone, Debug)]
pub struct PendingTouch {
    window_id: usize,
    /// Touch id acting as the pointer for this surface, if any is down.
    primary_id: Option<i32>,
    /// Most recent surface-local coordinates of the primary point.
    coords: Option<(f64, f64)>,
    /// Queued press/release transitions to dispatch.
    clicks: Vec<ButtonState>,
}

impl PendingTouch {
    pub(super) fn create(window_id: usize) -> Arc<Mutex<Self>> {
        Arc::new(Mutex::new(Self {
            window_id,
            primary_id: None,
            coords: None,
            clicks: vec![],
        }))
    }

    pub(super) fn next_click(pending: &Arc<Mutex<Self>>) -> Option<ButtonState> {
        let mut pending = pending.lock().unwrap();
        if pending.clicks.is_empty() {
            None
        } else {
            Some(pending.clicks.remove(0))
        }
    }

    pub(super) fn coords(pending: &Arc<Mutex<Self>>) -> Option<(f64, f64)> {
        pending.lock().unwrap().coords.take()
    }
}
