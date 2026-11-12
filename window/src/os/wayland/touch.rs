use std::sync::{Arc, Mutex};

use smithay_client_toolkit::seat::touch::TouchHandler;
use wayland_client::backend::ObjectId;
use wayland_client::protocol::wl_pointer::ButtonState;
use wayland_client::protocol::wl_surface::WlSurface;
use wayland_client::protocol::wl_touch::WlTouch;
use wayland_client::{Connection, Proxy, QueueHandle};

use super::WaylandConnection;
use super::state::WaylandState;

/// Maps the primary touch point of each surface onto the regular mouse
/// event pipeline: down -> Press(Left), motion -> Move, up -> Release(Left).
/// Secondary touch points are tracked (for cancel/up bookkeeping) but
/// otherwise ignored for now.
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
        let surface_id = surface.id();
        self.touch_to_surface.insert(id, surface_id.clone());
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
        let Some(surface_id) = self.touch_to_surface.remove(&id) else {
            return;
        };
        if let Some(pending) = self.surface_to_touch.get(&surface_id) {
            let mut pending = pending.lock().unwrap();
            if pending.primary_id == Some(id) {
                pending.primary_id = None;
                pending.clicks.push(ButtonState::Released);
                self.dispatch_pending_touch(pending.window_id);
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
        let Some(surface_id) = self.touch_to_surface.get(&id).cloned() else {
            return;
        };
        if let Some(pending) = self.surface_to_touch.get(&surface_id) {
            let mut pending = pending.lock().unwrap();
            if pending.primary_id == Some(id) {
                pending.coords = Some(position);
                self.dispatch_pending_touch(pending.window_id);
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
        // still-pressed primaries so windows aren't left with a stuck
        // held mouse button.
        for (_, surface_id) in self
            .touch_to_surface
            .drain()
            .collect::<Vec<(i32, ObjectId)>>()
        {
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
