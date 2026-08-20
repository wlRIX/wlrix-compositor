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
    reexports::wayland_server::protocol::wl_surface::WlSurface,
    utils::{Logical, Point, Rectangle, Serial, Size},
    wayland::{
        compositor::with_states,
        shell::xdg::{SurfaceCachedState, XdgToplevelSurfaceData},
    },
    xwayland::xwm::WmWindowType,
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

/// What a window says it will allow, as far as the protocols let it say anything.
///
/// 4Dwm drew only the controls a window could actually use, and this is the same idea: a
/// fixed-size dialog has no business showing a maximize button that does nothing.
///
/// ## What can and cannot be known
///
/// **Resizing** is the one both protocols express, and everything else here is derived from
/// it. A Wayland toplevel that calls `set_min_size` and `set_max_size` with the same value has
/// fixed that axis; an X11 window says the same through `WM_NORMAL_HINTS`.
///
/// **Maximizing** follows: a window fixed in both axes cannot grow into a maximized one.
/// xdg-shell has no separate way to refuse it -- `xdg_toplevel.wm_capabilities` runs the other
/// way, compositor to client -- so this is the whole of it.
///
/// **Minimizing** cannot be refused by either protocol. What *is* knowable is whether the
/// window is a dialog: a Wayland toplevel with a parent, or an X11 window typed `DIALOG` or
/// transient for another. Motif and 4Dwm gave those no minimize button, because a dialog
/// belongs to its parent and iconifies with it rather than on its own, so that is the rule
/// used here.
///
/// X11's `_MOTIF_WM_HINTS` carries exactly the answer for all three in its *functions* field,
/// which is what IRIX itself read. smithay parses the property but exposes only the
/// decorations field (`X11Surface::is_decorated`), so the functions are out of reach without
/// patching a pinned dependency.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capabilities {
    pub resizable: decoration::Resizable,
    pub maximizable: bool,
    pub minimizable: bool,
}

impl Capabilities {
    /// What a window that has said nothing gets: everything.
    pub fn unrestricted() -> Self {
        Self {
            resizable: decoration::Resizable::BOTH,
            maximizable: true,
            minimizable: true,
        }
    }

    /// Nothing but moving and closing: what a frame with no border and no buttons offers.
    pub fn none() -> Self {
        Self {
            resizable: decoration::Resizable::NONE,
            maximizable: false,
            minimizable: false,
        }
    }
}

/// A window's frame and what it may be asked to do.
///
/// One answer for both, because they have to agree: the titlebar drops the buttons the window
/// cannot use and the window menu grays out the same items, and a control drawn away but still
/// reachable is worse than one that was never removed.
pub struct Frame {
    /// `None` for a window that draws its own chrome, or has none.
    pub style: Option<decoration::FrameStyle>,
    pub capabilities: Capabilities,
}

/// Work out how `window` is framed.
pub fn frame_of(window: &Window) -> Frame {
    // An override-redirect X11 surface (a menu, a tooltip) positions and decorates itself.
    if window
        .x11_surface()
        .is_some_and(|surface| surface.is_override_redirect())
    {
        return Frame {
            style: None,
            capabilities: Capabilities::none(),
        };
    }

    // A fullscreen window covers its whole output, so there is nowhere for a frame to go and
    // nothing it could usefully do there. Its *capabilities* are left alone rather than
    // zeroed: there are no buttons to draw them on, but they still gate the ops, and a
    // taskbar minimizing a fullscreen window is an ordinary thing to want.
    if crate::desks::window_state(window).borrow().fullscreen {
        return Frame {
            style: None,
            capabilities: read_capabilities(window),
        };
    }

    // The wlRIX shell apps are framed by rule rather than by what they ask for.
    match crate::placement::app_id(window)
        .as_deref()
        .and_then(crate::placement::shell_frame)
    {
        Some(crate::placement::ShellFrame::Bare) => {
            return Frame {
                style: None,
                capabilities: Capabilities::none(),
            };
        }
        Some(crate::placement::ShellFrame::TitlebarOnly) => {
            return Frame {
                style: Some(decoration::FrameStyle {
                    titlebar: true,
                    border: false,
                    menu_btn: false,
                    min_btn: false,
                    max_btn: false,
                    resizable: decoration::Resizable::NONE,
                    // Nothing on the bar but the name, so it sits in the middle of it.
                    title_align: decoration::TitleAlign::Centered,
                }),
                // Movable and closable through the window menu, and nothing else -- which is
                // what the missing buttons already say.
                capabilities: Capabilities::none(),
            };
        }
        None => {}
    }

    let capabilities = read_capabilities(window);

    // A window that decorates itself gets the border and nothing else. Drawing our titlebar
    // over its own is what put two of them on pavucontrol: the border still frames the window
    // as part of the desktop, gives it the corner resize grips and moves it on a middle-drag,
    // while the client's own bar carries the title and buttons -- so nothing is doubled and
    // nothing is lost.
    if draws_own_decorations(window) {
        return Frame {
            style: Some(decoration::FrameStyle {
                titlebar: false,
                border: true,
                menu_btn: false,
                min_btn: false,
                max_btn: false,
                resizable: capabilities.resizable,
                // No bar for a title to sit on; the field still has to say something.
                title_align: decoration::TitleAlign::Left,
            }),
            capabilities,
        };
    }

    Frame {
        style: Some(decoration::FrameStyle {
            titlebar: true,
            border: true,
            menu_btn: true,
            // A button that cannot do anything is not drawn: `right_buttons` then slides
            // minimize outward into the slot maximize would have had, so the titlebar has no
            // gap in it.
            min_btn: capabilities.minimizable,
            max_btn: capabilities.maximizable,
            resizable: capabilities.resizable,
            title_align: decoration::TitleAlign::Left,
        }),
        capabilities,
    }
}

/// Marker recording that a client engaged with `xdg-decoration` for this surface.
///
/// Its *absence* is the signal that matters, so this is a marker rather than a mode: a client
/// that never creates a `zxdg_toplevel_decoration_v1` object never reaches any handler, and
/// there is nothing else to distinguish it from one that did.
struct NegotiatedDecorations;

/// Note that this surface's client asked about decorations at all.
///
/// Called from every `XdgDecorationHandler` entry point. What it answered does not matter --
/// wlRIX replies server-side to all of them -- only that the client took part.
pub fn mark_negotiated_decorations(surface: &WlSurface) {
    with_states(surface, |states| {
        states
            .data_map
            .insert_if_missing_threadsafe(|| NegotiatedDecorations);
    });
}

/// Whether this window draws its own chrome, and so should not be given a titlebar.
///
/// The two shells answer this in completely different ways.
///
/// **Wayland**: xdg-decoration says a toplevel with no `zxdg_toplevel_decoration_v1` object has
/// "the decoration mode assumed to be client-side". That is not a corner case -- GTK has never
/// implemented the protocol, so every GTK application lands here and every one of them draws a
/// headerbar.
///
/// **X11**: nothing to do with xdg-decoration. The client says so through `_MOTIF_WM_HINTS`,
/// which smithay surfaces as `is_decorated` -- named the other way round from how it reads, and
/// true when the *client* is doing the decorating.
fn draws_own_decorations(window: &Window) -> bool {
    if let Some(x11) = window.x11_surface() {
        return x11.is_decorated();
    }
    let Some(toplevel) = window.toplevel() else {
        return false;
    };
    with_states(toplevel.wl_surface(), |states| {
        states.data_map.get::<NegotiatedDecorations>().is_none()
    })
}

/// Read a window's capabilities off whichever shell it speaks.
fn read_capabilities(window: &Window) -> Capabilities {
    if let Some(x11) = window.x11_surface() {
        let resizable = resizable_from(x11.min_size(), x11.max_size());
        // A dialog belongs to the window it was opened from.
        let dialog = matches!(x11.window_type(), Some(WmWindowType::Dialog))
            || x11.is_transient_for().is_some();
        return Capabilities {
            resizable,
            maximizable: resizable.any(),
            minimizable: !dialog,
        };
    }

    let Some(toplevel) = window.toplevel() else {
        return Capabilities::unrestricted();
    };
    let (min, max) = with_states(toplevel.wl_surface(), |states| {
        let mut guard = states.cached_state.get::<SurfaceCachedState>();
        let data = guard.current();
        (data.min_size, data.max_size)
    });
    let resizable = resizable_from(Some(min), Some(max));
    Capabilities {
        resizable,
        maximizable: resizable.any(),
        minimizable: toplevel.parent().is_none(),
    }
}

/// Turn a min/max size pair into the axes that are still free.
///
/// Zero means "unconstrained" in both xdg-shell and X11's size hints, so an axis is only fixed
/// when it has a real maximum *and* a matching minimum. A maximum on its own is a ceiling, not
/// a fixed size -- the window can still be made smaller.
fn resizable_from(
    min: Option<Size<i32, Logical>>,
    max: Option<Size<i32, Logical>>,
) -> decoration::Resizable {
    let (min, max) = match (min, max) {
        (Some(min), Some(max)) => (min, max),
        _ => return decoration::Resizable::BOTH,
    };
    decoration::Resizable {
        horizontal: !(max.w > 0 && max.w == min.w),
        vertical: !(max.h > 0 && max.h == min.h),
    }
}

/// The frame a window gets, or `None` for one that draws its own chrome or has none. See
/// [`frame_of`].
pub fn frame_style(window: &Window) -> Option<decoration::FrameStyle> {
    frame_of(window).style
}

/// What a window may be asked to do. See [`frame_of`].
pub fn capabilities(window: &Window) -> Capabilities {
    frame_of(window).capabilities
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
        // An overlay- or top-layer surface is above every window, so a frame beneath one must
        // not claim the pointer -- no resize arrow over a panel covering a titlebar.
        let frame = (!self.layer_covers_windows_at(point))
            .then(|| self.frame_under(point))
            .flatten();
        match frame {
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
    ///
    /// Every button focuses the window; only middle leaves the stacking order alone. Moving a
    /// window is not the same as asking for it to come forward -- dragging one out of the way to
    /// see what is behind it should not put it in front of that -- and it is what makes a
    /// non-opaque move show its wireframe over the other windows while the window itself stays
    /// where it was in the order.
    pub fn press_frame(&mut self, window: &Window, part: FramePart, serial: Serial, button: u32) {
        if button == BTN_MIDDLE {
            crate::focus::focus_window_in_place(self, window);
        } else {
            crate::focus::focus_window(self, window);
        }

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
                // A border with nothing to resize. The press is still the frame's -- it
                // focused the window on the way in, and it must not fall through to the
                // client -- but there is no drag to start.
                FramePart::Border => {}
            },
            _ => {}
        }
        self.request_redraw();
    }

    /// Start dragging `window` by the pointer. Returns whether the grab began (it does not for a
    /// window with no location, e.g. one not currently mapped).
    pub(crate) fn start_move(&mut self, window: &Window, serial: Serial, button: u32) -> bool {
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
            opaque: self.config.windows.opaque_move,
            current_location: loc,
        };
        pointer.set_grab(self, grab, serial, Focus::Clear);
        self.hold_grab_cursor(CursorIcon::Grabbing);
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
            opaque: self.config.windows.opaque_move,
            current_location: loc,
        };
        pointer.set_grab(self, grab, serial, Focus::Clear);
        self.hold_grab_cursor(CursorIcon::Grabbing);
    }

    /// Start a menu- or keybind-driven resize: with no button held, the window follows the
    /// pointer until the next click settles it.
    ///
    /// Returns whether the resize began. It does not for a window with no location (not
    /// mapped -- minimized, or on another desk), nor for one that is fixed in both axes,
    /// which has nothing to resize.
    pub fn start_menu_resize(&mut self, window: &Window, serial: Serial) -> bool {
        let pointer = self.seat.get_pointer().expect("seat has a pointer");
        let Some(loc) = self.space.element_location(window) else {
            return false;
        };
        let initial = Rectangle::new(loc, window.geometry().size);
        let Some(edge) = resize_edge_towards(
            initial,
            pointer.current_location(),
            capabilities(window).resizable,
        ) else {
            return false;
        };
        let grab = ResizeSurfaceGrab::start(
            PointerGrabStartData {
                focus: None,
                // Nothing is held; the grab ends on the next press rather than on a release,
                // so this is only what `start_data` has to carry, not a button to wait for.
                button: BTN_LEFT,
                location: pointer.current_location(),
            },
            window.clone(),
            resize_edges(edge),
            crate::grabs::resize_grab::ResizeEnd::NextClick,
            initial,
            self.config.windows.opaque_resize,
        );
        pointer.set_grab(self, grab, serial, Focus::Clear);
        // The arrow for the edge that was chosen, which is the only thing on screen saying
        // which one it is -- there was no border press to make it obvious.
        self.hold_grab_cursor(frame_cursor(FramePart::Resize(edge)));
        true
    }

    /// Start resizing `window` from a border.
    ///
    /// Both kinds of window, Wayland and X11: the grab asks each in its own protocol, and the
    /// difference is confined to [`ResizeSurfaceGrab::apply`].
    pub(crate) fn start_resize(
        &mut self,
        window: &Window,
        edge: decoration::ResizeEdge,
        serial: Serial,
        button: u32,
    ) {
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
            crate::grabs::resize_grab::ResizeEnd::ButtonRelease,
            initial,
            self.config.windows.opaque_resize,
        );
        pointer.set_grab(self, grab, serial, Focus::Clear);
        // The same arrow the border was already showing, so the drag looks continuous with the
        // press that began it.
        self.hold_grab_cursor(frame_cursor(FramePart::Resize(edge)));
    }

    /// Keep `icon` on screen for as long as the grab that just started runs.
    ///
    /// **After `set_grab`, never before.** Starting a grab with `Focus::Clear` makes smithay drop
    /// the pointer focus there and then, and dropping focus calls `cursor_image` with the default
    /// arrow -- so a cursor set first would be overwritten a line later.
    fn hold_grab_cursor(&mut self, icon: CursorIcon) {
        self.grab_cursor = Some(icon);
        self.cursor_from_chrome = true;
        self.cursor_status = CursorImageStatus::Named(icon);
        self.request_redraw();
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
///
/// **Each of the eight is named on its own** -- `NwResize` for the top-left corner rather than
/// `NwseResize` for that diagonal, `NResize` for the top edge rather than `NsResize` -- because
/// the axis names describe a *direction of travel* and cannot say which end of it the pointer is
/// on. A theme drawing them as symmetric double-headed arrows renders both the same and the
/// distinction never shows; one drawing them as the arrows they are does not.
///
/// The IRIX set is the second kind: `sgi`'s top-left corner is an arrow pointing up-left and its
/// bottom-right is a different image pointing down-right, and asking for `NwseResize` at both got
/// the up-left one at the bottom-right corner -- an arrow pointing away across the window. The
/// same for the edges, which IRIX drew as a single arrow against a bar. Adwaita, where this was
/// developed, symlinks each diagonal pair to one drawing, which is why it looked right there.
///
/// Every theme carries these eight names, and DMZ -- the freedesktop reference set -- maps them
/// onto its eight directional drawings exactly like this, so no theme loses by being asked
/// precisely.
fn frame_cursor(part: FramePart) -> CursorIcon {
    let FramePart::Resize(edge) = part else {
        return CursorIcon::Default;
    };
    match (edge.top, edge.bottom, edge.left, edge.right) {
        // Corners first: a corner is both a vertical and a horizontal edge.
        (true, _, true, _) => CursorIcon::NwResize,
        (true, _, _, true) => CursorIcon::NeResize,
        (_, true, true, _) => CursorIcon::SwResize,
        (_, true, _, true) => CursorIcon::SeResize,
        (true, ..) => CursorIcon::NResize,
        (_, true, ..) => CursorIcon::SResize,
        (_, _, true, _) => CursorIcon::WResize,
        (_, _, _, true) => CursorIcon::EResize,
        // A border hit always has an edge; nothing sensible to point at otherwise.
        _ => CursorIcon::Default,
    }
}

/// Which edge a menu- or keybind-driven resize takes hold of.
///
/// A border drag says which edge by where it was pressed. Choosing "Size" from the menu, or
/// pressing its binding, says nothing at all, so the edge has to come from somewhere else.
///
/// 4Dwm warped the pointer to the window's center and picked the edge from the direction it
/// was first moved. Warping the pointer is hostile on a multi-head desktop -- it can throw
/// the cursor onto another monitor entirely -- so the direction is read from where the
/// pointer already sits relative to the center instead. It is the same gesture without the
/// jump: point at the edge you mean, then press the key.
///
/// The 2:1 rule is what separates an edge from a corner. Offsets are normalized against half
/// the window's width and height first, so the split is by *proportion* rather than by
/// pixels: a wide, short window would otherwise be nearly all corner.
///
/// `None` when the window is fixed in both axes and there is no edge worth taking. An axis
/// fixed on its own is simply dropped, which turns what would have been a corner into the
/// one edge that can still move.
fn resize_edge_towards(
    rect: Rectangle<i32, Logical>,
    pointer: Point<f64, Logical>,
    resizable: decoration::Resizable,
) -> Option<decoration::ResizeEdge> {
    if !resizable.any() {
        return None;
    }

    // Halved and floored at 1: a zero-sized window would divide by zero, and a 1px one would
    // put every pointer position infinitely far from the center.
    let half_w = (rect.size.w as f64 / 2.0).max(1.0);
    let half_h = (rect.size.h as f64 / 2.0).max(1.0);
    let x = (pointer.x - (rect.loc.x as f64 + half_w)) / half_w;
    let y = (pointer.y - (rect.loc.y as f64 + half_h)) / half_h;

    // One axis has to beat the other twofold to claim the edge on its own; short of that,
    // both are taken and the result is a corner.
    let horizontal = resizable.horizontal && x.abs() * 2.0 >= y.abs();
    let vertical = resizable.vertical && y.abs() * 2.0 >= x.abs();

    let mut edge = decoration::ResizeEdge {
        top: vertical && y < 0.0,
        bottom: vertical && y >= 0.0,
        left: horizontal && x < 0.0,
        right: horizontal && x >= 0.0,
    };
    // Nothing chosen. This is a window fixed in one axis, pointed at along that axis --
    // a fixed-width panel with the pointer directly above it, say: the axis the direction
    // named cannot move, and the axis that can move was not named. Fall back to the corner
    // 4Dwm's own size gesture starts from, restricted to the axis that is left.
    if edge == decoration::ResizeEdge::default() {
        edge.right = resizable.horizontal;
        edge.bottom = resizable.vertical;
    }
    Some(edge)
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

    /// A 400x300 window at (100, 100), so its center is (300, 250).
    fn window_rect() -> Rectangle<i32, Logical> {
        Rectangle::new(Point::from((100, 100)), Size::from((400, 300)))
    }

    fn towards(x: f64, y: f64, resizable: decoration::Resizable) -> decoration::ResizeEdge {
        resize_edge_towards(window_rect(), Point::from((x, y)), resizable)
            .expect("a resizable window has an edge")
    }

    /// The gesture 4Dwm's Size is: point at the edge you mean, then press the key.
    #[test]
    fn a_size_gesture_takes_the_edge_it_is_pointed_at() {
        let both = decoration::Resizable::BOTH;
        // Straight out from the center along one axis: that edge alone.
        assert_eq!(
            towards(0.0, 250.0, both),
            decoration::ResizeEdge {
                left: true,
                ..Default::default()
            }
        );
        assert_eq!(
            towards(600.0, 250.0, both),
            decoration::ResizeEdge {
                right: true,
                ..Default::default()
            }
        );
        assert_eq!(
            towards(300.0, 0.0, both),
            decoration::ResizeEdge {
                top: true,
                ..Default::default()
            }
        );
        assert_eq!(
            towards(300.0, 500.0, both),
            decoration::ResizeEdge {
                bottom: true,
                ..Default::default()
            }
        );
    }

    /// Diagonally out, where neither axis beats the other twofold, is a corner.
    #[test]
    fn a_diagonal_size_gesture_takes_a_corner() {
        let both = decoration::Resizable::BOTH;
        assert_eq!(
            towards(0.0, 0.0, both),
            decoration::ResizeEdge {
                top: true,
                left: true,
                ..Default::default()
            }
        );
        assert_eq!(
            towards(600.0, 500.0, both),
            decoration::ResizeEdge {
                bottom: true,
                right: true,
                ..Default::default()
            }
        );
    }

    /// Proportional rather than in pixels, which is the reason for normalizing by half the
    /// window's size. This window is wider than it is tall, so the same pixel offset in each
    /// axis is a *smaller* fraction horizontally -- and the vertical edge wins outright.
    #[test]
    fn the_edge_split_goes_by_proportion_not_pixels() {
        let both = decoration::Resizable::BOTH;
        // 50px right of center is 0.25 of a half-width; 150px below is 1.0 of a half-height.
        assert_eq!(
            towards(350.0, 400.0, both),
            decoration::ResizeEdge {
                bottom: true,
                ..Default::default()
            }
        );
    }

    /// An axis the window has fixed is never offered, so the gesture cannot promise a resize
    /// the client would refuse. A corner degrades to the edge that can still move.
    #[test]
    fn a_fixed_axis_is_dropped_from_the_edge() {
        let fixed_width = decoration::Resizable {
            horizontal: false,
            vertical: true,
        };
        // Up and to the left would be the top-left corner; the width is fixed, so it is the
        // top edge alone.
        assert_eq!(
            towards(0.0, 0.0, fixed_width),
            decoration::ResizeEdge {
                top: true,
                ..Default::default()
            }
        );
        // Pointed straight out along the axis that cannot move: neither the named axis nor
        // the free one was chosen, so it falls back to the free one's far edge.
        assert_eq!(
            towards(0.0, 250.0, fixed_width),
            decoration::ResizeEdge {
                bottom: true,
                ..Default::default()
            }
        );
    }

    /// Fixed in both axes: no edge at all, and `start_menu_resize` refuses on that basis --
    /// which is what keeps the Alt+F8 binding from doing anything to a fixed-size dialog,
    /// where there is no menu drawing the item greyed to stop it.
    #[test]
    fn a_window_fixed_in_both_axes_has_no_size_gesture() {
        assert_eq!(
            resize_edge_towards(
                window_rect(),
                Point::from((0.0, 0.0)),
                decoration::Resizable::NONE
            ),
            None
        );
    }

    /// A degenerate window must not divide by zero or come back with nothing.
    #[test]
    fn a_zero_sized_window_still_yields_an_edge() {
        let rect = Rectangle::new(Point::from((0, 0)), Size::from((0, 0)));
        assert!(
            resize_edge_towards(rect, Point::from((0.0, 0.0)), decoration::Resizable::BOTH)
                .is_some()
        );
    }

    fn edge(top: bool, bottom: bool, left: bool, right: bool) -> FramePart {
        FramePart::Resize(decoration::ResizeEdge {
            top,
            bottom,
            left,
            right,
        })
    }

    /// Each border and corner points at *itself*.
    ///
    /// The corners are the easy ones to get wrong, and were: an axis name (`NwseResize`) covers
    /// both ends of a diagonal, so the top-left and bottom-right corners were asking for one
    /// cursor between them. Under a theme that draws a symmetric double-headed arrow -- Adwaita,
    /// which this was written against -- that is invisible. Under one that draws the arrows
    /// directionally, as the IRIX set does, the bottom-right corner showed an arrow pointing
    /// up-left, away across the window.
    #[test]
    fn borders_map_to_their_resize_arrows() {
        // Edges: an arrow for the edge under the pointer, not for its axis.
        assert_eq!(
            frame_cursor(edge(true, false, false, false)),
            CursorIcon::NResize
        );
        assert_eq!(
            frame_cursor(edge(false, true, false, false)),
            CursorIcon::SResize
        );
        assert_eq!(
            frame_cursor(edge(false, false, true, false)),
            CursorIcon::WResize
        );
        assert_eq!(
            frame_cursor(edge(false, false, false, true)),
            CursorIcon::EResize
        );

        // Corners: four distinct arrows, one per corner.
        assert_eq!(
            frame_cursor(edge(true, false, true, false)),
            CursorIcon::NwResize
        );
        assert_eq!(
            frame_cursor(edge(false, true, false, true)),
            CursorIcon::SeResize
        );
        assert_eq!(
            frame_cursor(edge(true, false, false, true)),
            CursorIcon::NeResize
        );
        assert_eq!(
            frame_cursor(edge(false, true, true, false)),
            CursorIcon::SwResize
        );
    }

    /// No two of the eight share a cursor. The bug this replaces was two corners sharing one, and
    /// a table of eight assertions above cannot notice a ninth arm quietly returning a duplicate.
    #[test]
    fn each_border_and_corner_has_its_own_arrow() {
        let corners_and_edges = [
            edge(true, false, false, false),
            edge(false, true, false, false),
            edge(false, false, true, false),
            edge(false, false, false, true),
            edge(true, false, true, false),
            edge(true, false, false, true),
            edge(false, true, true, false),
            edge(false, true, false, true),
        ];
        let mut seen = Vec::new();
        for part in corners_and_edges {
            let icon = frame_cursor(part);
            assert!(
                !seen.contains(&icon),
                "{icon:?} is used for two frame parts"
            );
            seen.push(icon);
        }
    }

    fn style() -> decoration::FrameStyle {
        decoration::FrameStyle {
            titlebar: true,
            border: true,
            menu_btn: true,
            min_btn: true,
            max_btn: true,
            resizable: decoration::Resizable::BOTH,
            title_align: decoration::TitleAlign::Left,
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

    /// The same rule read the other way, which is what pointer focus leans on.
    ///
    /// `focus::window_under` asks for a frame *before* asking the space what is under the
    /// point, because a border can overhang a lower window's client area and is drawn on top
    /// there. Asking the space first would answer with the window underneath, so the pointer
    /// would focus the wrong one while sitting on a visible border of the right one.
    #[test]
    fn a_border_is_found_even_where_it_overhangs_a_lower_window() {
        let upper = client();
        // The point is on the upper window's left border, and would be well inside a window
        // sitting behind it.
        let on_border = Point::from((upper.loc.x as f64 - 2.0, 250.0));
        assert!(matches!(
            hit_window(upper, Some(style()), on_border),
            Hit::Part(FramePart::Resize(_))
        ));
        // `frame_under` walks from the top down and stops at the first hit, so the lower
        // window is never reached -- but if it were, it would claim the point.
        let lower = Rectangle::new(Point::from((0, 0)), smithay::utils::Size::from((800, 600)));
        assert_eq!(hit_window(lower, Some(style()), on_border), Hit::Occluded);
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

#[cfg(test)]
mod capability_tests {
    use super::*;
    use crate::decoration::Resizable;

    fn size(w: i32, h: i32) -> Size<i32, Logical> {
        Size::from((w, h))
    }

    #[test]
    fn a_window_that_says_nothing_can_do_everything() {
        // Zero means "unconstrained" in both xdg-shell and X11 size hints, and a client that
        // never called `set_min_size`/`set_max_size` reports zeroes.
        assert_eq!(
            resizable_from(Some(size(0, 0)), Some(size(0, 0))),
            Resizable::BOTH
        );
        assert_eq!(resizable_from(None, None), Resizable::BOTH);
        assert_eq!(resizable_from(Some(size(100, 80)), None), Resizable::BOTH);
    }

    #[test]
    fn matching_min_and_max_fix_the_window() {
        // What `CanResize = false` amounts to on the wire.
        assert_eq!(
            resizable_from(Some(size(400, 300)), Some(size(400, 300))),
            Resizable::NONE
        );
    }

    #[test]
    fn one_axis_can_be_fixed_without_the_other() {
        assert_eq!(
            resizable_from(Some(size(400, 100)), Some(size(400, 900))),
            Resizable {
                horizontal: false,
                vertical: true
            }
        );
        assert_eq!(
            resizable_from(Some(size(100, 300)), Some(size(900, 300))),
            Resizable {
                horizontal: true,
                vertical: false
            }
        );
    }

    #[test]
    fn a_maximum_on_its_own_is_a_ceiling_not_a_fixed_size() {
        // The window can still be made *smaller*, so the handles stay. Getting this wrong
        // would strip the border off every window that merely caps its size.
        assert_eq!(
            resizable_from(Some(size(0, 0)), Some(size(800, 600))),
            Resizable::BOTH
        );
        assert_eq!(
            resizable_from(Some(size(200, 150)), Some(size(800, 600))),
            Resizable::BOTH
        );
    }

    #[test]
    fn a_minimum_on_its_own_is_a_floor_not_a_fixed_size() {
        assert_eq!(
            resizable_from(Some(size(400, 300)), Some(size(0, 0))),
            Resizable::BOTH
        );
    }

    #[test]
    fn a_zero_maximum_never_fixes_an_axis_even_against_a_zero_minimum() {
        // The trap in the arithmetic: `max == min` is true for (0, 0), which would make every
        // ordinary window fixed. The `max > 0` guard is what stops it.
        assert_eq!(
            resizable_from(Some(size(0, 300)), Some(size(0, 300))),
            Resizable {
                horizontal: true,
                vertical: false
            }
        );
    }
}
