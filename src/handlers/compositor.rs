// SPDX-License-Identifier: GPL-3.0-or-later
// Adapted from Smithay's `smallvil` example (MIT-licensed). See the NOTICE file.
use crate::{Wlrix, grabs::resize_grab, state::ClientState};
use smithay::wayland::seat::WaylandFocus;
use smithay::xwayland::XWaylandClientData;
use smithay::{
    backend::renderer::utils::on_commit_buffer_handler,
    delegate_compositor, delegate_shm,
    reexports::wayland_server::{
        Client,
        protocol::{wl_buffer, wl_surface::WlSurface},
    },
    wayland::{
        buffer::BufferHandler,
        compositor::{
            CompositorClientState, CompositorHandler, CompositorState, get_parent,
            is_sync_subsurface,
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
        xdg_shell::handle_commit(&mut self.popups, &mut self.space, surface, pointer);
        super::layer_shell::handle_commit(&self.space, surface);
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

delegate_compositor!(Wlrix);
delegate_shm!(Wlrix);
