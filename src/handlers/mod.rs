// SPDX-License-Identifier: GPL-3.0-or-later
// Adapted from Smithay's `smallvil` example (MIT-licensed). See the NOTICE file.
mod compositor;
pub mod layer_shell;
mod xdg_shell;

use crate::Wlrix;

//
// Wl Seat
//

use smithay::input::{Seat, SeatHandler, SeatState};
use smithay::reexports::wayland_server::Resource;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::wayland::output::OutputHandler;
use smithay::wayland::selection::SelectionHandler;
use smithay::wayland::selection::data_device::{
    ClientDndGrabHandler, DataDeviceHandler, DataDeviceState, ServerDndGrabHandler,
    set_data_device_focus,
};
use smithay::{delegate_data_device, delegate_output, delegate_seat};

impl SeatHandler for Wlrix {
    type KeyboardFocus = WlSurface;
    type PointerFocus = WlSurface;
    type TouchFocus = WlSurface;

    fn seat_state(&mut self) -> &mut SeatState<Wlrix> {
        &mut self.seat_state
    }

    fn cursor_image(
        &mut self,
        _seat: &Seat<Self>,
        image: smithay::input::pointer::CursorImageStatus,
    ) {
        // A client set (or hid) its cursor; the backends render whatever is current.
        self.cursor_status = image;
    }

    fn focus_changed(&mut self, seat: &Seat<Self>, focused: Option<&WlSurface>) {
        let dh = &self.display_handle;
        let client = focused.and_then(|s| dh.get_client(s.id()).ok());
        set_data_device_focus(dh, seat, client);
    }
}

delegate_seat!(Wlrix);

//
// Wl Data Device
//

impl SelectionHandler for Wlrix {
    type SelectionUserData = ();
}

impl DataDeviceHandler for Wlrix {
    fn data_device_state(&self) -> &DataDeviceState {
        &self.data_device_state
    }
}

impl ClientDndGrabHandler for Wlrix {}
impl ServerDndGrabHandler for Wlrix {}

delegate_data_device!(Wlrix);

//
// Wl Output & Xdg Output
//

impl OutputHandler for Wlrix {}
delegate_output!(Wlrix);

//
// Linux dmabuf — lets clients hand us GPU buffers instead of shared memory,
// which is what makes hardware-accelerated clients (and direct scanout) possible.
//

use smithay::backend::allocator::dmabuf::Dmabuf;
use smithay::backend::renderer::ImportDma;
use smithay::delegate_dmabuf;
use smithay::wayland::dmabuf::{DmabufGlobal, DmabufHandler, DmabufState, ImportNotifier};

impl DmabufHandler for Wlrix {
    fn dmabuf_state(&mut self) -> &mut DmabufState {
        self.dmabuf_state
            .as_mut()
            .expect("dmabuf global advertised without dmabuf state")
    }

    fn dmabuf_imported(
        &mut self,
        _global: &DmabufGlobal,
        dmabuf: Dmabuf,
        notifier: ImportNotifier,
    ) {
        let imported = match self.renderer.as_ref() {
            // udev: test-import against the primary renderer so we only accept
            // buffers we can actually use.
            Some(renderer) => renderer.borrow_mut().import_dmabuf(&dmabuf, None).is_ok(),
            // winit: the renderer is owned by the winit backend and isn't shareable,
            // so we can't test-import here. Accept — the import is still performed
            // (and can still fail) per-frame at render time. Nested dev loop only.
            None => true,
        };

        if imported {
            let _ = notifier.successful::<Wlrix>();
        } else {
            notifier.failed();
        }
    }
}

delegate_dmabuf!(Wlrix);
