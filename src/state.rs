// SPDX-License-Identifier: GPL-3.0-or-later
// Adapted from Smithay's `smallvil` example (MIT-licensed). See the NOTICE file.
use std::{cell::RefCell, ffi::OsString, rc::Rc, sync::Arc};

use smithay::{
    backend::{renderer::gles::GlesRenderer, session::libseat::LibSeatSession},
    desktop::{PopupManager, Space, Window, WindowSurfaceType},
    input::{Seat, SeatState, pointer::CursorImageStatus},
    output::{Mode as WlMode, Output},
    reexports::{
        calloop::{
            EventLoop, Interest, LoopSignal, Mode, PostAction, generic::Generic, ping::Ping,
        },
        wayland_server::{
            Display, DisplayHandle,
            backend::{ClientData, ClientId, DisconnectReason},
            protocol::wl_surface::WlSurface,
        },
    },
    utils::{Logical, Point},
    wayland::{
        compositor::{CompositorClientState, CompositorState},
        dmabuf::DmabufState,
        fractional_scale::FractionalScaleManagerState,
        output::OutputManagerState,
        pointer_constraints::PointerConstraintsState,
        presentation::PresentationState,
        relative_pointer::RelativePointerManagerState,
        selection::data_device::DataDeviceState,
        selection::primary_selection::PrimarySelectionState,
        shell::{
            wlr_layer::WlrLayerShellState,
            xdg::{XdgShellState, decoration::XdgDecorationState},
        },
        shm::ShmState,
        socket::ListeningSocketSource,
        viewporter::ViewporterState,
        xdg_activation::XdgActivationState,
    },
};

use crate::CalloopData;

pub struct Wlrix {
    pub start_time: std::time::Instant,
    pub socket_name: OsString,
    pub display_handle: DisplayHandle,

    pub space: Space<Window>,
    pub loop_signal: LoopSignal,

    // Smithay State
    pub compositor_state: CompositorState,
    pub xdg_shell_state: XdgShellState,
    /// wlr-layer-shell: desktop components (toolchest, desks, background) anchor here
    /// rather than being ordinary toplevels.
    pub layer_shell_state: WlrLayerShellState,
    pub shm_state: ShmState,
    pub output_manager_state: OutputManagerState,
    pub seat_state: SeatState<Wlrix>,
    pub data_device_state: DataDeviceState,
    /// Middle-click paste, which Unix users expect alongside the clipboard.
    pub primary_selection_state: PrimarySelectionState,
    /// Frame timing feedback, for clients that pace themselves to the display.
    pub presentation_state: PresentationState,
    /// Surface scaling and cropping.
    pub viewporter_state: ViewporterState,
    /// Non-integer output scaling, for HiDPI clients.
    pub fractional_scale_state: FractionalScaleManagerState,
    /// Raw pointer deltas, which the compositor already produces; games need these.
    pub relative_pointer_state: RelativePointerManagerState,
    /// Pointer lock and confinement, for games and 3D applications.
    pub pointer_constraints_state: PointerConstraintsState,
    /// Lets an application ask for focus when launched by another.
    pub xdg_activation_state: XdgActivationState,
    /// Decoration negotiation. Clients draw their own until wlRIX draws 4Dwm frames.
    pub xdg_decoration_state: XdgDecorationState,
    pub popups: PopupManager,

    pub seat: Seat<Self>,

    /// Outputs that exist but are switched off. Kept so they can still be advertised
    /// as disabled heads -- a head a client cannot see is a head it cannot turn on.
    pub disabled_outputs: Vec<Output>,
    /// Enable/disable requests accepted from a client, awaiting the backend.
    pub pending_output_toggles: Vec<(Output, bool)>,

    /// Mode changes accepted from a client, waiting for the backend to apply them.
    /// The DRM state lives in the backend, which the protocol handlers cannot reach,
    /// so they are queued here and drained when the backend next wakes.
    pub pending_mode_changes: Vec<(Output, WlMode)>,

    /// wlr-output-management: monitor enumeration and (later) configuration.
    pub output_management: crate::output_management::OutputManagementState,

    /// Signals the active backend that something changed and an output needs redrawing.
    /// Set by whichever backend is running; see [`Wlrix::request_redraw`].
    pub redraw_ping: Option<Ping>,

    /// Current pointer cursor: a client-set surface, a named theme cursor, or hidden.
    pub cursor_status: CursorImageStatus,
    /// Loads the cursor theme and turns `cursor_status` into render elements.
    pub pointer_renderer: crate::cursor::PointerRenderer,

    /// libseat session, present only under the udev backend; enables VT switching.
    pub session: Option<LibSeatSession>,

    /// `linux-dmabuf-v1` state; `Some` once a backend advertises the global.
    pub dmabuf_state: Option<DmabufState>,
    /// The primary GPU's renderer, shared with the backend so the dmabuf handler can
    /// test-import client buffers. Single-threaded, hence `Rc<RefCell<_>>`.
    pub renderer: Option<Rc<RefCell<GlesRenderer>>>,
}

impl Wlrix {
    pub fn new(event_loop: &mut EventLoop<CalloopData>, display: Display<Self>) -> Self {
        let start_time = std::time::Instant::now();

        let dh = display.handle();

        let compositor_state = CompositorState::new::<Self>(&dh);
        let xdg_shell_state = XdgShellState::new::<Self>(&dh);
        let layer_shell_state = WlrLayerShellState::new::<Self>(&dh);
        let output_management = crate::output_management::OutputManagementState::new();
        let _output_management_global =
            crate::output_management::OutputManagementState::create_global(&dh);
        let shm_state = ShmState::new::<Self>(&dh, vec![]);
        let output_manager_state = OutputManagerState::new_with_xdg_output::<Self>(&dh);
        let mut seat_state = SeatState::new();
        let data_device_state = DataDeviceState::new::<Self>(&dh);
        let primary_selection_state = PrimarySelectionState::new::<Self>(&dh);
        // CLOCK_MONOTONIC: the clock presentation timestamps are reported against.
        let presentation_state = PresentationState::new::<Self>(&dh, libc::CLOCK_MONOTONIC as u32);
        let viewporter_state = ViewporterState::new::<Self>(&dh);
        let fractional_scale_state = FractionalScaleManagerState::new::<Self>(&dh);
        let relative_pointer_state = RelativePointerManagerState::new::<Self>(&dh);
        let pointer_constraints_state = PointerConstraintsState::new::<Self>(&dh);
        let xdg_activation_state = XdgActivationState::new::<Self>(&dh);
        let xdg_decoration_state = XdgDecorationState::new::<Self>(&dh);
        let popups = PopupManager::default();

        // A seat is a group of keyboards, pointer and touch devices.
        // A seat typically has a pointer and maintains a keyboard focus and a pointer focus.
        let mut seat: Seat<Self> = seat_state.new_wl_seat(&dh, "winit");

        // Notify clients that we have a keyboard, for the sake of the example we assume that keyboard is always present.
        // You may want to track keyboard hot-plug in real compositor.
        seat.add_keyboard(Default::default(), 200, 25).unwrap();

        // Notify clients that we have a pointer (mouse)
        // Here we assume that there is always pointer plugged in
        seat.add_pointer();

        // A space represents a two-dimensional plane. Windows and Outputs can be mapped onto it.
        //
        // Windows get a position and stacking order through mapping.
        // Outputs become views of a part of the Space and can be rendered via Space::render_output.
        let space = Space::default();

        let socket_name = Self::init_wayland_listener(display, event_loop);

        // Get the loop signal, used to stop the event loop
        let loop_signal = event_loop.get_signal();

        Self {
            start_time,
            display_handle: dh,

            space,
            loop_signal,
            socket_name,

            compositor_state,
            xdg_shell_state,
            layer_shell_state,
            output_management,
            disabled_outputs: Vec::new(),
            pending_output_toggles: Vec::new(),
            pending_mode_changes: Vec::new(),
            shm_state,
            output_manager_state,
            seat_state,
            data_device_state,
            primary_selection_state,
            presentation_state,
            viewporter_state,
            fractional_scale_state,
            relative_pointer_state,
            pointer_constraints_state,
            xdg_activation_state,
            xdg_decoration_state,
            popups,
            seat,
            redraw_ping: None,
            cursor_status: CursorImageStatus::default_named(),
            pointer_renderer: crate::cursor::PointerRenderer::new(),
            session: None,
            dmabuf_state: None,
            renderer: None,
        }
    }

    fn init_wayland_listener(
        display: Display<Wlrix>,
        event_loop: &mut EventLoop<CalloopData>,
    ) -> OsString {
        // Creates a new listening socket, automatically choosing the next available `wayland` socket name.
        let listening_socket = ListeningSocketSource::new_auto().unwrap();

        // Get the name of the listening socket.
        // Clients will connect to this socket.
        let socket_name = listening_socket.socket_name().to_os_string();

        let loop_handle = event_loop.handle();

        loop_handle
            .insert_source(listening_socket, move |client_stream, _, state| {
                // Inside the callback, you should insert the client into the display.
                //
                // You may also associate some data with the client when inserting the client.
                state
                    .display_handle
                    .insert_client(client_stream, Arc::new(ClientState::default()))
                    .unwrap();
            })
            .expect("Failed to init the wayland event source.");

        // You also need to add the display itself to the event loop, so that client events will be processed by wayland-server.
        loop_handle
            .insert_source(
                Generic::new(display, Interest::READ, Mode::Level),
                |_, display, state| {
                    // Safety: we don't drop the display
                    unsafe {
                        display
                            .get_mut()
                            .dispatch_clients(&mut state.state)
                            .unwrap();
                    }
                    Ok(PostAction::Continue)
                },
            )
            .unwrap();

        socket_name
    }

    /// Re-advertise the layout to wlr-output-management clients, enabled and
    /// disabled heads alike.
    pub fn advertise_outputs(&mut self, display: &DisplayHandle) {
        let enabled: Vec<Output> = self.space.outputs().cloned().collect();
        let disabled = self.disabled_outputs.clone();
        self.output_management
            .outputs_changed(display, &enabled, &disabled);
    }

    /// Where the pointer currently is, in space coordinates.
    pub fn pointer_location(&self) -> Point<f64, Logical> {
        self.seat
            .get_pointer()
            .map(|pointer| pointer.current_location())
            .unwrap_or_default()
    }

    /// Ask the backend to redraw.
    ///
    /// Rendering is damage-driven: nothing is drawn until something actually changes,
    /// so every source of change must call this. Missing a call means a stale screen,
    /// so it is better to request one spuriously -- a redraw with no damage is cheap
    /// and puts the output straight back to idle.
    pub fn request_redraw(&self) {
        if let Some(ping) = self.redraw_ping.as_ref() {
            ping.ping();
        }
    }

    pub fn surface_under(
        &self,
        pos: Point<f64, Logical>,
    ) -> Option<(WlSurface, Point<f64, Logical>)> {
        self.space
            .element_under(pos)
            .and_then(|(window, location)| {
                window
                    .surface_under(pos - location.to_f64(), WindowSurfaceType::ALL)
                    .map(|(s, p)| (s, (p + location).to_f64()))
            })
    }
}

#[derive(Default)]
pub struct ClientState {
    pub compositor_state: CompositorClientState,
}

impl ClientData for ClientState {
    fn initialized(&self, _client_id: ClientId) {}
    fn disconnected(&self, _client_id: ClientId, _reason: DisconnectReason) {}
}
