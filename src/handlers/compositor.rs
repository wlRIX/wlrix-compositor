// SPDX-License-Identifier: GPL-3.0-or-later
// Adapted from Smithay's `smallvil` example (MIT-licensed). See the NOTICE file.
use crate::{Wlrix, grabs::resize_grab, state::ClientState};
use smithay::wayland::seat::WaylandFocus;
use smithay::xwayland::XWaylandClientData;
use smithay::{
    backend::renderer::utils::on_commit_buffer_handler,
    reexports::wayland_server::{
        Client,
        protocol::{wl_buffer, wl_surface::WlSurface},
    },
    wayland::{
        buffer::BufferHandler,
        compositor::{
            CompositorClientState, CompositorHandler, CompositorState, SurfaceAttributes,
            get_parent, is_sync_subsurface, with_states,
        },
        shm::{ShmHandler, ShmState},
    },
};

use super::xdg_shell;

impl CompositorHandler for Wlrix {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.compositor_state
    }

    fn client_compositor_state<'a>(&self, client: &'a Client) -> &'a CompositorClientState {
        // XWayland's own client is created by smithay and carries its data, not ours.
        // This runs inside the Wayland dispatch, which cannot unwind, so getting it
        // wrong aborts the compositor rather than raising an error.
        if let Some(data) = client.get_data::<XWaylandClientData>() {
            return &data.compositor_state;
        }
        if let Some(data) = client.get_data::<ClientState>() {
            return &data.compositor_state;
        }
        panic!("client has no compositor state")
    }

    fn commit(&mut self, surface: &WlSurface) {
        on_commit_buffer_handler::<Self>(surface);
        if !is_sync_subsurface(surface) {
            let mut root = surface.clone();
            while let Some(parent) = get_parent(&root) {
                root = parent;
            }
            if let Some(window) = self
                .space
                .elements()
                .find(|w| w.wl_surface().as_deref() == Some(&root))
            {
                window.on_commit();
            }
        };

        let pointer = self.pointer_location();
        let newly_placed =
            xdg_shell::handle_commit(&mut self.popups, &mut self.space, surface, pointer);
        // A window that has just opened joins a desk (the global desk for the shell apps,
        // else the active one) and takes focus, so it can be typed into without having to
        // be clicked first.
        if let Some(window) = newly_placed {
            let app_id = crate::placement::app_id(&window);
            crate::desks::assign_new_window(&self.desks, &window, app_id.as_deref());
            crate::focus::focus_window(self, &window);
            self.desks_changed();
            // Silent under the default theme, which has no window sounds -- IRIX had none --
            // but a theme that has one plays it. Whether it plays is wlrix-sound's to decide.
            crate::sound::play("window-new");
        }
        super::layer_shell::handle_commit(self, surface);
        // A drag icon moves by the offset its client attached the buffer with: that is how a
        // client puts the grab point under the pointer, rather than the icon's corner.
        if let Some(icon) = self
            .dnd_icon
            .as_mut()
            .filter(|icon| &icon.surface == surface)
        {
            icon.offset += take_buffer_delta(surface);
        }
        // A client committed new content: the screen may have changed.
        self.request_redraw();
        resize_grab::handle_commit(&mut self.space, surface);
    }
}

impl BufferHandler for Wlrix {
    fn buffer_destroyed(&mut self, _buffer: &wl_buffer::WlBuffer) {}
}

impl ShmHandler for Wlrix {
    fn shm_state(&self) -> &ShmState {
        &self.shm_state
    }
}

/// The offset the surface's last commit attached its buffer with, taken so it is counted once.
///
/// `wl_surface.attach`'s x and y (or `wl_surface.offset`) move the surface relative to where it
/// was. Only the drag icon reads it.
pub fn take_buffer_delta(
    surface: &WlSurface,
) -> smithay::utils::Point<i32, smithay::utils::Logical> {
    with_states(surface, |states| {
        states
            .cached_state
            .get::<SurfaceAttributes>()
            .current()
            .buffer_delta
            .take()
            .unwrap_or_default()
    })
}
