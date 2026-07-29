// SPDX-License-Identifier: GPL-3.0-or-later
//! Server-side-decoration input: which windows get a frame, hit-testing the pointer against
//! frames, and turning a press on a frame into a move, a resize, or a button action.
//!
//! The drawing lives in [`crate::decoration`] (pure geometry + quads) and [`crate::render`];
//! this is the interactive half.

use std::time::{Duration, Instant};

use smithay::{
    desktop::Window,
    input::pointer::{CursorIcon, CursorImageStatus, Focus, GrabStartData as PointerGrabStartData},
    utils::{Logical, Point, Rectangle, Serial},
    wayland::{compositor::with_states, shell::xdg::XdgToplevelSurfaceData},
};

use crate::{
    Wlrix,
    decoration::{self, FramePart},
    grabs::{MoveSurfaceGrab, ResizeSurfaceGrab, move_grab::MoveEnd, resize_grab::ResizeEdge},
};

// Pointer button codes, from the Linux kernel's `linux/input-event-codes.h`.
pub const BTN_LEFT: u32 = 0x110;
pub const BTN_RIGHT: u32 = 0x111;
pub const BTN_MIDDLE: u32 = 0x112;

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
    ///
    /// Stops at the first window covering `point` at all, rather than at the first *frame* hit:
    /// a window's client area hides the frames of everything below it. Without that, a border
    /// belonging to a buried window is found through the window covering it -- which shows the
    /// wrong resize cursor and, worse, would resize the buried window on click.
    pub fn frame_under(&self, point: Point<f64, Logical>) -> Option<(Window, FramePart)> {
        for window in self.space.elements().rev() {
            let Some(client) = self.space.element_geometry(window) else {
                continue;
            };
            match hit_window(client, frame_style(window), point) {
                Hit::Part(part) => return Some((window.clone(), part)),
                Hit::Occluded => return None,
                Hit::Miss => continue,
            }
        }
        None
    }

    /// Point the cursor at whatever is under it, for the parts of the screen no client owns.
    ///
    /// Clients set their own cursor when the pointer enters their surface, but the 4Dwm frame is
    /// drawn by the compositor: nothing would otherwise change the cursor over a border, leaving
    /// whatever the last client asked for. So the borders get their resize arrows here, and bare
    /// desktop gets the plain arrow back.
    ///
    /// Skipped entirely while a grab is active: a move or resize keeps the cursor it started
    /// with, and no client is receiving pointer events to set one anyway.
    pub fn update_frame_cursor(&mut self, point: Point<f64, Logical>) {
        let grabbed = self
            .seat
            .get_pointer()
            .is_some_and(|pointer| pointer.is_grabbed());
        if grabbed {
            return;
        }

        // An open window menu is compositor chrome drawn *over* whatever is beneath it, so it
        // is checked before the surface below can claim the pointer.
        let over_menu = self
            .window_menu
            .as_ref()
            .is_some_and(|menu| menu.contains(point));

        if over_menu {
            self.set_chrome_cursor(CursorIcon::Default);
            return;
        }
        match self.frame_under(point) {
            Some((_, part)) => self.set_chrome_cursor(frame_cursor(part)),
            // A client sets its own cursor when the pointer enters its surface, but coming back
            // from the frame it gets no fresh `enter`, so a resize arrow would stick. Hand the
            // arrow back **once** and then leave the cursor alone, letting the client's next
            // `set_cursor` win -- re-asserting it every motion would fight the client.
            None if self.surface_under(point).is_some() => {
                if self.cursor_from_chrome {
                    self.cursor_from_chrome = false;
                    self.cursor_status = CursorImageStatus::Named(CursorIcon::Default);
                    self.request_redraw();
                }
            }
            // Bare desktop, or a minimized icon: the plain arrow.
            None => self.set_chrome_cursor(CursorIcon::Default),
        }
    }

    /// Set a cursor the compositor owns, remembering that it did so.
    fn set_chrome_cursor(&mut self, icon: CursorIcon) {
        self.cursor_from_chrome = true;
        if self.cursor_status != CursorImageStatus::Named(icon) {
            self.cursor_status = CursorImageStatus::Named(icon);
            self.request_redraw();
        }
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
            // The window menu, posted where it was asked for.
            BTN_RIGHT => {
                let at = self
                    .seat
                    .get_pointer()
                    .expect("seat has a pointer")
                    .current_location()
                    .to_i32_round();
                self.open_window_menu(window, at);
            }
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
            end: MoveEnd::ButtonRelease,
        };
        pointer.set_grab(self, grab, serial, Focus::Clear);
        true
    }

    /// Start a menu-driven move: with no button held, the window follows the pointer until the
    /// next click puts it down.
    pub fn start_menu_move(&mut self, window: &Window, serial: Serial) {
        let pointer = self.seat.get_pointer().expect("seat has a pointer");
        let Some(loc) = self.space.element_location(window) else {
            // Not mapped (minimized, or on another desk): there is nothing to drag.
            return;
        };
        let grab = MoveSurfaceGrab {
            start_data: PointerGrabStartData {
                focus: None,
                button: BTN_LEFT,
                location: pointer.current_location(),
            },
            window: window.clone(),
            initial_window_location: loc,
            end: MoveEnd::NextClick,
        };
        pointer.set_grab(self, grab, serial, Focus::Clear);
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

    /// Handle a left press on the window-menu button: a single click posts the window menu under
    /// the button, a double click closes the window (both 4Dwm).
    fn press_menu_button(&mut self, window: &Window) {
        const DOUBLE_CLICK: Duration = Duration::from_millis(400);
        let now = Instant::now();
        let double = self
            .last_menu_click
            .take()
            .is_some_and(|(w, t)| &w == window && now.duration_since(t) < DOUBLE_CLICK);
        if double {
            self.close_window_menu();
            self.close_window(window);
            return;
        }
        self.last_menu_click = Some((window.clone(), now));
        self.decoration_pressed = Some((window.clone(), FramePart::MenuButton));
        // Under the button: the frame's left edge, just below the titlebar.
        if let Some(client) = self.space.element_geometry(window) {
            let at = Point::from((client.loc.x - decoration::BORDER, client.loc.y));
            self.open_window_menu(window, at);
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

/// What one window, considered top-down, does with a point.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Hit {
    /// The point is on this window's frame.
    Part(FramePart),
    /// The point is inside this window but not on its frame, so it hides everything below.
    Occluded,
    /// The point is outside this window; keep looking further down.
    Miss,
}

/// Decide what `point` hits for a single window. `style` is `None` for an undecorated window,
/// which has no frame to hit but still occludes what is beneath it.
fn hit_window(
    client: Rectangle<i32, Logical>,
    style: Option<decoration::FrameStyle>,
    point: Point<f64, Logical>,
) -> Hit {
    if let Some(style) = style
        && let Some(part) = decoration::hit_test(client, style, point)
    {
        return Hit::Part(part);
    }
    if client.to_f64().contains(point) {
        return Hit::Occluded;
    }
    Hit::Miss
}

/// The cursor for a part of the frame: a resize arrow along the borders and corners, and the
/// plain arrow for the titlebar and its buttons.
fn frame_cursor(part: FramePart) -> CursorIcon {
    let FramePart::Resize(edge) = part else {
        return CursorIcon::Default;
    };
    match (edge.top, edge.bottom, edge.left, edge.right) {
        // Corners first: a corner is both a vertical and a horizontal edge.
        (true, _, true, _) | (_, true, _, true) => CursorIcon::NwseResize,
        (true, _, _, true) | (_, true, true, _) => CursorIcon::NeswResize,
        (true, _, _, _) | (_, true, _, _) => CursorIcon::NsResize,
        (_, _, true, _) | (_, _, _, true) => CursorIcon::EwResize,
        // A border hit always has an edge; nothing sensible to point at otherwise.
        _ => CursorIcon::Default,
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

#[cfg(test)]
mod tests {
    use super::*;

    fn edge(top: bool, bottom: bool, left: bool, right: bool) -> FramePart {
        FramePart::Resize(decoration::ResizeEdge {
            top,
            bottom,
            left,
            right,
        })
    }

    /// The diagonals are the easy ones to swap: NWSE runs top-left to bottom-right, NESW runs
    /// top-right to bottom-left. Getting them the wrong way round points the arrow across the
    /// corner the pointer is not on.
    #[test]
    fn borders_map_to_their_resize_arrows() {
        assert_eq!(
            frame_cursor(edge(true, false, false, false)),
            CursorIcon::NsResize
        );
        assert_eq!(
            frame_cursor(edge(false, true, false, false)),
            CursorIcon::NsResize
        );
        assert_eq!(
            frame_cursor(edge(false, false, true, false)),
            CursorIcon::EwResize
        );
        assert_eq!(
            frame_cursor(edge(false, false, false, true)),
            CursorIcon::EwResize
        );

        assert_eq!(
            frame_cursor(edge(true, false, true, false)),
            CursorIcon::NwseResize
        );
        assert_eq!(
            frame_cursor(edge(false, true, false, true)),
            CursorIcon::NwseResize
        );
        assert_eq!(
            frame_cursor(edge(true, false, false, true)),
            CursorIcon::NeswResize
        );
        assert_eq!(
            frame_cursor(edge(false, true, true, false)),
            CursorIcon::NeswResize
        );
    }

    fn style() -> decoration::FrameStyle {
        decoration::FrameStyle {
            titlebar: true,
            border: true,
            menu_btn: true,
            min_btn: true,
            max_btn: true,
        }
    }

    /// A 400x300 client at (100, 100); its frame wraps outside that.
    fn client() -> Rectangle<i32, Logical> {
        Rectangle::new(
            Point::from((100, 100)),
            smithay::utils::Size::from((400, 300)),
        )
    }

    /// The rule that keeps a buried window's border from being found through the window on top
    /// of it: anything inside a window that is not its frame *hides* what is below, rather than
    /// letting the search fall through. Getting this wrong resized the wrong window on click.
    #[test]
    fn a_covering_window_hides_what_is_beneath_it() {
        let c = client();
        // Inside the client area: not a frame hit, but it occludes.
        assert_eq!(
            hit_window(c, Some(style()), Point::from((300.0, 250.0))),
            Hit::Occluded
        );
        // Well outside: the search continues to the window below.
        assert_eq!(
            hit_window(c, Some(style()), Point::from((900.0, 900.0))),
            Hit::Miss
        );
        // On the left border: a real frame hit.
        let left = c.loc.x as f64 - 2.0;
        assert!(matches!(
            hit_window(c, Some(style()), Point::from((left, 250.0))),
            Hit::Part(FramePart::Resize(_))
        ));
    }

    /// An undecorated window (the toolchest, the greeter) has no frame, but must still hide the
    /// frames of windows below it.
    #[test]
    fn an_undecorated_window_still_occludes() {
        let c = client();
        assert_eq!(
            hit_window(c, None, Point::from((300.0, 250.0))),
            Hit::Occluded
        );
        assert_eq!(hit_window(c, None, Point::from((900.0, 900.0))), Hit::Miss);
    }

    /// The titlebar and its buttons are not resize handles.
    #[test]
    fn the_titlebar_keeps_the_plain_arrow() {
        for part in [
            FramePart::Titlebar,
            FramePart::MenuButton,
            FramePart::MinimizeButton,
            FramePart::MaximizeButton,
        ] {
            assert_eq!(frame_cursor(part), CursorIcon::Default);
        }
    }
}
