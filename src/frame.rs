// SPDX-License-Identifier: GPL-3.0-or-later
//! Server-side-decoration input: which windows get a frame, hit-testing the pointer against
//! frames, and turning a press on a frame into a move, a resize, or a button action.
//!
//! The drawing lives in [`crate::decoration`] (pure geometry + quads) and [`crate::render`];
//! this is the interactive half.

use smithay::{
    desktop::Window,
    input::pointer::{Focus, GrabStartData as PointerGrabStartData},
    utils::{Logical, Point, Rectangle, Serial},
};

use crate::{
    Wlrix,
    decoration::{self, FramePart},
    grabs::{MoveSurfaceGrab, ResizeSurfaceGrab, resize_grab::ResizeEdge},
};

/// The frame a window gets, or `None` for windows that decorate themselves (override-redirect
/// X11 menus/tooltips). Every ordinary toplevel gets the full 4Dwm frame for now; per-app
/// rules come later.
pub fn frame_style(window: &Window) -> Option<decoration::FrameStyle> {
    if window
        .x11_surface()
        .is_some_and(|surface| surface.is_override_redirect())
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

    /// Begin the interaction for a pointer press that landed on a window's frame: the titlebar
    /// moves the window, the borders resize it, and the buttons arm (drawn sunken) until
    /// release.
    pub fn press_frame(&mut self, window: &Window, part: FramePart, serial: Serial, button: u32) {
        crate::focus::focus_window(self, window);
        let pointer = self.seat.get_pointer().expect("seat has a pointer");
        let location = pointer.current_location();
        let start_data = PointerGrabStartData {
            focus: None,
            button,
            location,
        };

        match part {
            FramePart::Titlebar => {
                if let Some(loc) = self.space.element_location(window) {
                    let grab = MoveSurfaceGrab {
                        start_data,
                        window: window.clone(),
                        initial_window_location: loc,
                    };
                    pointer.set_grab(self, grab, serial, Focus::Clear);
                }
            }
            FramePart::Resize(edge) => {
                // The resize grab drives an xdg configure, so only Wayland toplevels can be
                // frame-resized; X11 windows fall through (their resize is not wired up).
                if window.toplevel().is_some()
                    && let Some(loc) = self.space.element_location(window)
                {
                    let initial = Rectangle::new(loc, window.geometry().size);
                    let grab = ResizeSurfaceGrab::start(
                        start_data,
                        window.clone(),
                        resize_edges(edge),
                        initial,
                    );
                    pointer.set_grab(self, grab, serial, Focus::Clear);
                }
            }
            FramePart::MenuButton | FramePart::MinimizeButton | FramePart::MaximizeButton => {
                self.decoration_pressed = Some((window.clone(), part));
            }
        }
        self.request_redraw();
    }

    /// Finish a frame-button press on release: act only if the pointer is still over the same
    /// button (moving off cancels, IRIX-style).
    pub fn release_frame(&mut self, point: Point<f64, Logical>) {
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
