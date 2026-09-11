// SPDX-License-Identifier: GPL-3.0-or-later
//! `_GTK_SHOW_WINDOW_MENU`: the X11 half of "right-click my own titlebar".
//!
//! A Wayland client that draws its own decorations asks for the compositor's window menu with
//! `xdg_toplevel.show_window_menu`, which [`crate::handlers`] answers. The same GTK application
//! under XWayland cannot: there is no such X11 request in the ICCCM or in EWMH, so GTK invented
//! one. The client sends a `ClientMessage` of type `_GTK_SHOW_WINDOW_MENU` to the **root**
//! window, naming its own window and the pointer position in root coordinates, and the window
//! manager is expected to post a menu there.
//!
//! Two halves, and missing either one makes the right-click do nothing at all:
//!
//! - **The atom has to be advertised in `_NET_SUPPORTED`.** GDK checks the root window's
//!   property first and returns without sending anything if the atom is absent (gdkwindow-x11.c,
//!   `gdk_x11_window_show_window_menu`). So an unadvertised compositor never even receives the
//!   message it is failing to handle.
//! - **The message has to be received.** Smithay's `XwmHandler` has no hook for it -- unknown
//!   client messages are traced and dropped -- and `X11Wm` does not lend out its connection, so
//!   this opens a second one.
//!
//! ## Why a second X11 connection is not the hack it looks like
//!
//! GTK sends the message with `SubstructureRedirect | SubstructureNotify`, and X delivers a sent
//! event to every client selecting any of those masks on the target window. Only the *redirect*
//! half is exclusive to the window manager; `SubstructureNotify` is not, and selecting it here
//! takes nothing away from smithay's window manager, which keeps the redirect half and every
//! message it already handles. This connection is a listener: it selects one mask, owns one
//! unmapped window so the reader thread can be woken to exit, and issues no other request.
//!
//! The alternative was patching smithay, which is pinned to an upstream revision rather than a
//! fork. That is the better long-term answer and a much larger one.
//!
//! ## Coordinates
//!
//! Root coordinates, and they are already the compositor's. XWayland is rootless here: smithay's
//! window manager configures each X11 window at the position the `Space` holds it at, so the X
//! root's coordinate space and the compositor's logical space are the same one. Nothing is
//! converted, unlike the Wayland side, where the position arrives surface-local and the client's
//! own shadow inset has to come off.

use std::sync::Arc;

use smithay::reexports::calloop::{LoopHandle, RegistrationToken, channel::Event as ChannelEvent};
use smithay::reexports::x11rb::{
    connection::Connection,
    protocol::{
        Event,
        xproto::{
            AtomEnum, ChangeWindowAttributesAux, ConnectionExt, EventMask, PropMode, WindowClass,
        },
    },
    rust_connection::RustConnection,
    // `change_property32` lives on this trait rather than on the protocol one.
    wrapper::ConnectionExt as _,
};
use smithay::utils::x11rb::X11Source;
use smithay::utils::{Logical, Point};

use crate::Wlrix;

/// Listen for `_GTK_SHOW_WINDOW_MENU` on the X11 display XWayland just brought up.
///
/// Returns the loop registration, so a later XWayland can replace an earlier one's listener
/// rather than leaving it behind on a dead connection.
///
/// Every failure here is reported and survivable: it costs X11 clients their titlebar
/// right-click and nothing else, which is not worth refusing to run XWayland over.
pub fn watch(
    display: u32,
    handle: &LoopHandle<'static, Wlrix>,
) -> Result<RegistrationToken, String> {
    let (connection, screen) = RustConnection::connect(Some(&format!(":{display}")))
        .map_err(|err| format!("could not connect to the X11 display: {err}"))?;
    let connection = Arc::new(connection);
    let root = connection.setup().roots[screen].root;

    let atom = |name: &str| -> Result<u32, String> {
        let cookie = connection
            .intern_atom(false, name.as_bytes())
            .map_err(|err| format!("could not ask for the atom {name}: {err}"))?;
        cookie
            .reply()
            .map(|reply| reply.atom)
            .map_err(|err| format!("could not intern {name}: {err}"))
    };
    let show_window_menu = atom("_GTK_SHOW_WINDOW_MENU")?;
    let net_supported = atom("_NET_SUPPORTED")?;
    // The reader thread is woken to exit by a message of this type to the window below. Named
    // for this compositor so it cannot collide with anything else on the display.
    let close_type = atom("_WLRIX_X11_MENU_CLOSE")?;

    // Appended, not written: smithay's window manager fills `_NET_SUPPORTED` in when it starts,
    // and replacing the property would take away every hint it just advertised. This runs after
    // `X11Wm::start_wm` for that reason.
    connection
        .change_property32(
            PropMode::APPEND,
            root,
            net_supported,
            AtomEnum::ATOM,
            &[show_window_menu],
        )
        .map_err(|err| format!("could not advertise _GTK_SHOW_WINDOW_MENU: {err}"))?;

    connection
        .change_window_attributes(
            root,
            &ChangeWindowAttributesAux::new().event_mask(EventMask::SUBSTRUCTURE_NOTIFY),
        )
        .map_err(|err| format!("could not watch the root window: {err}"))?;

    // An unmapped, input-only window of our own: `X11Source` sends itself a message here to
    // wake its reader thread when it is dropped, and the window has to be one we created.
    let close_window = connection
        .generate_id()
        .map_err(|err| format!("could not allocate an X11 window id: {err}"))?;
    connection
        .create_window(
            smithay::reexports::x11rb::COPY_DEPTH_FROM_PARENT,
            close_window,
            root,
            0,
            0,
            1,
            1,
            0,
            WindowClass::INPUT_ONLY,
            0,
            &Default::default(),
        )
        .map_err(|err| format!("could not create the wake-up window: {err}"))?;

    connection
        .flush()
        .map_err(|err| format!("could not flush the X11 connection: {err}"))?;

    let source = X11Source::new(Arc::clone(&connection), close_window, close_type);
    handle
        .insert_source(source, move |event, _, state| {
            let ChannelEvent::Msg(Event::ClientMessage(message)) = event else {
                // `Closed` means the reader thread stopped, which means the connection did --
                // XWayland going away. The next one to start replaces this registration.
                return;
            };
            if message.type_ != show_window_menu {
                return;
            }
            state.open_x11_window_menu(message.window, position_of(message.data.as_data32()));
        })
        .map_err(|err| format!("could not watch for X11 window menus: {err}"))
}

/// The position out of a `_GTK_SHOW_WINDOW_MENU` message's five data words.
///
/// `data[0]` is the device id, which this compositor has no use for -- there is one pointer, and
/// the menu goes where the message says rather than where any particular device is. The position
/// is `data[1]` and `data[2]`, in root coordinates. Reading from the wrong index would put every
/// X11 menu at a plausible-looking but wrong place, which is why this is worth a test rather than
/// two subscripts inline.
fn position_of(data: [u32; 5]) -> Point<i32, Logical> {
    Point::from((data[1] as i32, data[2] as i32))
}

impl Wlrix {
    /// Post the window menu for the X11 window with this id.
    fn open_x11_window_menu(&mut self, window_id: u32, at: Point<i32, Logical>) {
        let Some(window) = self.window_for_x11(window_id) else {
            // An id we do not know: an override-redirect window, or one already gone. Both are
            // normal races rather than faults.
            return;
        };
        self.open_window_menu(&window, at);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_position_is_the_second_and_third_words_and_not_the_first() {
        // What GDK sends (gdkwindow-x11.c): data.l[0] the device id, data.l[1] and data.l[2] the
        // root coordinates. Starting at data[0] would read the device id as an x coordinate --
        // a small number that looks like a position, so the menu would appear near the screen's
        // left edge and nothing would look obviously broken.
        assert_eq!(position_of([13, 880, 231, 0, 0]), Point::from((880, 231)));
    }

    #[test]
    fn a_position_is_read_as_signed() {
        // A window dragged off the left of the leftmost monitor has negative root coordinates,
        // which arrive as a wrapped u32. Taken as unsigned the menu would be posted four billion
        // pixels away and never seen again.
        assert_eq!(
            position_of([13, (-40i32) as u32, (-12i32) as u32, 0, 0]),
            Point::from((-40, -12))
        );
    }
}
