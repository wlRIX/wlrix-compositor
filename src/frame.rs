// SPDX-License-Identifier: GPL-3.0-or-later
//! Server-side-decoration input: which windows get a frame, hit-testing the pointer against
//! frames, and turning a press on a frame into a move, a resize, or a button action.
//!
//! The drawing lives in [`crate::decoration`] (pure geometry + quads) and [`crate::render`];
//! this is the interactive half.

use std::time::{Duration, Instant};

use smithay::{
    desktop::Window,
    input::pointer::{Focus, GrabStartData as PointerGrabStartData},
    utils::{Logical, Point, Rectangle, Serial},
    wayland::{compositor::with_states, shell::xdg::XdgToplevelSurfaceData},
};

use crate::{
    Wlrix,
    decoration::{self, FramePart},
    grabs::{MoveSurfaceGrab, ResizeSurfaceGrab, resize_grab::ResizeEdge},
};

// Pointer button codes, from the Linux kernel's `linux/input-event-codes.h`.
const BTN_LEFT: u32 = 0x110;
const BTN_RIGHT: u32 = 0x111;
const BTN_MIDDLE: u32 = 0x112;

/// The frame a window gets, or `None` for windows that decorate themselves (override-redirect
/// X11 menus/tooltips) and for the undecorated wlRIX shell apps (toolchest, greeter). Every
/// other toplevel gets the full 4Dwm frame.
pub fn frame_style(window: &Window) -> Option<decoration::FrameStyle> {
    if window
        .x11_surface()
        .is_some_and(|surface| surface.is_override_redirect())
    {
        return None;
    }
    if crate::placement::app_id(window)
        .as_deref()
        .is_some_and(crate::placement::is_undecorated)
    {
        return None;
    }
    Some(decoration::FrameStyle {
        titlebar: true,
        border: true,
        menu_btn: true,
        min_btn: true,
        max_btn: true,
    })
}

/// A window's title, for its titlebar.
pub fn window_title(window: &Window) -> String {
    if let Some(x11) = window.x11_surface() {
        return x11.title();
    }
    let Some(toplevel) = window.toplevel() else {
        return String::new();
    };
    with_states(toplevel.wl_surface(), |states| {
        states
            .data_map
            .get::<XdgToplevelSurfaceData>()
            .and_then(|data| data.lock().ok().and_then(|data| data.title.clone()))
            .unwrap_or_default()
    })
}

impl Wlrix {
    /// The topmost window whose 4Dwm frame is under `point`, and which part of it.
    pub fn frame_under(&self, point: Point<f64, Logical>) -> Option<(Window, FramePart)> {
        for window in self.space.elements().rev() {
            let Some(style) = frame_style(window) else {
                continue;
            };
            let Some(client) = self.space.element_geometry(window) else {
                continue;
            };
            if let Some(part) = decoration::hit_test(client, style, point) {
                return Some((window.clone(), part));
            }
        }
        None
    }

    /// Begin the interaction for a pointer press that landed on a window's frame, dispatched by
    /// mouse button (4Dwm-style):
    /// - **Left** drives the frame: the titlebar moves the window (and sinks like a button while
    ///   dragged), the borders resize it, and the buttons arm (drawn sunken) until release.
    /// - **Middle** moves the window from anywhere on the frame, buttons included.
    /// - **Right** is reserved for the window menu (not yet implemented).
    pub fn press_frame(&mut self, window: &Window, part: FramePart, serial: Serial, button: u32) {
        crate::focus::focus_window(self, window);

        match button {
            BTN_MIDDLE => {
                self.start_move(window, serial, button);
            }
            BTN_RIGHT => {}
            BTN_LEFT => match part {
                FramePart::Titlebar => {
                    // Sink the titlebar while it is dragged, like a pressed button.
                    if self.start_move(window, serial, button) {
                        self.decoration_pressed = Some((window.clone(), FramePart::Titlebar));
                    }
                }
                FramePart::Resize(edge) => self.start_resize(window, edge, serial, button),
                FramePart::MenuButton => self.press_menu_button(window),
                FramePart::MinimizeButton | FramePart::MaximizeButton => {
                    self.decoration_pressed = Some((window.clone(), part));
                }
            },
            _ => {}
        }
        self.request_redraw();
    }

    /// Start dragging `window` by the pointer. Returns whether the grab began (it does not for a
    /// window with no location, e.g. one not currently mapped).
    fn start_move(&mut self, window: &Window, serial: Serial, button: u32) -> bool {
        let pointer = self.seat.get_pointer().expect("seat has a pointer");
        let Some(loc) = self.space.element_location(window) else {
            return false;
        };
        let grab = MoveSurfaceGrab {
            start_data: PointerGrabStartData {
                focus: None,
                button,
                location: pointer.current_location(),
            },
            window: window.clone(),
            initial_window_location: loc,
        };
        pointer.set_grab(self, grab, serial, Focus::Clear);
        true
    }

    /// Start resizing `window` from a border. The resize grab drives an xdg configure, so only
    /// Wayland toplevels can be frame-resized; X11 windows fall through (resize not wired up).
    fn start_resize(
        &mut self,
        window: &Window,
        edge: decoration::ResizeEdge,
        serial: Serial,
        button: u32,
    ) {
        if window.toplevel().is_none() {
            return;
        }
        let pointer = self.seat.get_pointer().expect("seat has a pointer");
        let Some(loc) = self.space.element_location(window) else {
            return;
        };
        let initial = Rectangle::new(loc, window.geometry().size);
        let grab = ResizeSurfaceGrab::start(
            PointerGrabStartData {
                focus: None,
                button,
                location: pointer.current_location(),
            },
            window.clone(),
            resize_edges(edge),
            initial,
        );
        pointer.set_grab(self, grab, serial, Focus::Clear);
    }

    /// Handle a left press on the window-menu button: a double click closes the window (4Dwm); a
    /// single click just arms it (a single-click menu is a later stage).
    fn press_menu_button(&mut self, window: &Window) {
        const DOUBLE_CLICK: Duration = Duration::from_millis(400);
        let now = Instant::now();
        let double = self
            .last_menu_click
            .take()
            .is_some_and(|(w, t)| &w == window && now.duration_since(t) < DOUBLE_CLICK);
        if double {
            self.close_window(window);
        } else {
            self.last_menu_click = Some((window.clone(), now));
            self.decoration_pressed = Some((window.clone(), FramePart::MenuButton));
        }
    }

    /// Finish a frame-button press on release: act only if the pointer is still over the same
    /// button (moving off cancels, IRIX-style). Only the left button arms a frame part, so only
    /// its release completes one; other buttons leave any armed part untouched.
    pub fn release_frame(&mut self, point: Point<f64, Logical>, button: u32) {
        if button != BTN_LEFT {
            return;
        }
        let Some((window, part)) = self.decoration_pressed.take() else {
            return;
        };
        if self.frame_under(point) == Some((window.clone(), part)) {
            match part {
                FramePart::MinimizeButton => self.minimize_window(&window),
                FramePart::MaximizeButton => self.toggle_maximize_window(&window),
                // The window menu (menu button) is a later stage.
                _ => {}
            }
        }
        self.request_redraw();
    }
}

fn resize_edges(edge: decoration::ResizeEdge) -> ResizeEdge {
    let mut edges = ResizeEdge::empty();
    edges.set(ResizeEdge::TOP, edge.top);
    edges.set(ResizeEdge::BOTTOM, edge.bottom);
    edges.set(ResizeEdge::LEFT, edge.left);
    edges.set(ResizeEdge::RIGHT, edge.right);
    edges
}
