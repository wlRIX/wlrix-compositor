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
            EventLoop, Interest, LoopHandle, LoopSignal, Mode, PostAction, generic::Generic,
            ping::Ping,
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
        xwayland_shell::XWaylandShellState,
    },
    xwayland::{X11Wm, XWayland, XWaylandEvent},
};

pub struct Wlrix {
    pub start_time: std::time::Instant,
    pub socket_name: OsString,
    pub display_handle: DisplayHandle,

    /// The loaded config: keyboard keymap/repeat, and display defaults. Kept so a
    /// `SIGHUP` reload can re-read and re-apply it live.
    pub config: crate::config::Config,

    /// The window frame part currently held down by the pointer, drawn sunken until
    /// release. Only one frame is interacted with at a time.
    pub decoration_pressed: Option<(Window, crate::decoration::FramePart)>,
    /// The last press on a window-menu button (window + when), to detect a double click -- which
    /// closes the window, 4Dwm-style.
    pub last_menu_click: Option<(Window, std::time::Instant)>,
    /// The posted window menu, if one is open. Only one can be open at a time.
    pub window_menu: Option<crate::menu::WindowMenu>,
    /// Rasterizes and caches window-title text for the server-side titlebars.
    pub text_renderer: crate::text::TextRenderer,
    /// The minimized-window icon being dragged across the grid, if any.
    pub icon_drag: Option<crate::minimized::IconDrag>,

    /// Virtual desktops ("desks"). Windows not on the active or global desk are held out
    /// of `space` here; see [`crate::desks`].
    pub desks: crate::desks::Desks,
    /// The `wlrix-desks` protocol resources bound by clients (the Desks Overview app).
    pub desks_protocol: crate::desks_protocol::DesksProtocolState,

    /// Per-connector display settings the backend applies when an output appears:
    /// `compositor.toml` defaults with the machine-written `outputs.toml` overlaid.
    /// Keyed by connector name; empty under the nested backend, which has one window.
    pub display_config: crate::outputs::DisplayConfig,

    /// Set when the display arrangement changed under `wlr-output-management` and needs
    /// saving to `outputs.toml`. Drained once per redraw batch so a burst of changes
    /// writes the file once, not per output.
    pub outputs_dirty: bool,

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

    /// XWayland: the X11 window manager, once XWayland has started.
    pub xwm: Option<X11Wm>,
    /// The X display number, for `DISPLAY`.
    pub xdisplay: Option<u32>,
    /// Handshake protocol XWayland uses to associate X11 windows with surfaces.
    pub xwayland_shell_state: XWaylandShellState,

    /// For inserting event sources after startup, such as XWayland's.
    pub loop_handle: LoopHandle<'static, Wlrix>,
    pub popups: PopupManager,

    pub seat: Seat<Self>,

    /// Outputs that exist but are switched off. Kept so they can still be advertised
    /// as disabled heads -- a head a client cannot see is a head it cannot turn on.
    pub disabled_outputs: Vec<Output>,
    /// Enable/disable requests accepted from a client, awaiting the backend.
    pub pending_output_toggles: Vec<(Output, bool)>,

    /// Screen captures a client has handed a buffer for, waiting on the renderer.
    pub pending_screencopy: Vec<crate::screencopy::PendingCapture>,
    pub session_lock_state: smithay::wayland::session_lock::SessionLockManagerState,
    pub idle: crate::idle::IdleState,
    pub vrr: crate::vrr::VrrState,
    /// udev/DRM backend state; `None` under the nested backend.
    pub udev: Option<crate::backend::udev::UdevState>,
    /// Nested backend; `None` under udev. Held here rather than in the event source so
    /// the redraw ping can ask the window to repaint.
    pub winit: Option<crate::backend::winit::WinitBackend>,
    /// VRR changes waiting on the backend, which alone can set the DRM property.
    pub pending_vrr_changes: Vec<(Output, bool)>,
    pub lock: crate::session_lock::LockState,

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
    pub fn new(
        event_loop: &mut EventLoop<'static, Wlrix>,
        display: Display<Self>,
        config: crate::config::Config,
    ) -> Self {
        let start_time = std::time::Instant::now();

        let dh = display.handle();

        let compositor_state = CompositorState::new::<Self>(&dh);
        let xdg_shell_state = XdgShellState::new::<Self>(&dh);
        let layer_shell_state = WlrLayerShellState::new::<Self>(&dh);
        let output_management = crate::output_management::OutputManagementState::new();
        let _output_management_global =
            crate::output_management::OutputManagementState::create_global(&dh);
        let _screencopy_global = crate::screencopy::ScreencopyState::create_global(&dh);
        let _desks_global = crate::desks_protocol::DesksProtocolState::create_global(&dh);
        let _idle_notifier_global = crate::idle::IdleNotifierState::create_global(&dh);
        let _idle_inhibit_state =
            smithay::wayland::idle_inhibit::IdleInhibitManagerState::new::<Self>(&dh);
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
        let xwayland_shell_state = XWaylandShellState::new::<Self>(&dh);
        // Any client may lock for now. Restricting this to the session's own locker
        // needs a way to identify trusted clients, which wlRIX does not have yet.
        let session_lock_state =
            smithay::wayland::session_lock::SessionLockManagerState::new::<Self, _>(&dh, |_| true);
        let popups = PopupManager::default();

        // A seat is a group of keyboards, pointer and touch devices.
        // A seat typically has a pointer and maintains a keyboard focus and a pointer focus.
        let mut seat: Seat<Self> = seat_state.new_wl_seat(&dh, "winit");

        // The one keyboard, with the configured keymap and repeat. `config` is still a
        // local here (moved into `Self` below), so the `&str`s borrowed by `.xkb()` are
        // valid for the call, which is all `add_keyboard` needs -- it compiles the keymap
        // and keeps none of the borrow. A bad layout/model combo would otherwise panic
        // startup, so fall back to the system default keymap and carry on.
        let kb = &config.keyboard;
        if let Err(err) = seat.add_keyboard(kb.xkb(), kb.delay(), kb.rate()) {
            tracing::warn!(
                ?err,
                layout = kb.layout.as_deref().unwrap_or(""),
                model = kb.model.as_deref().unwrap_or(""),
                "keyboard config did not compile; falling back to the default keymap"
            );
            seat.add_keyboard(Default::default(), kb.delay(), kb.rate())
                .expect("the default keymap must compile");
        }

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

        // Merge the hand-set defaults with the machine-written state now, so the backend
        // has the arrangement ready the moment a connector lights up.
        let display_config = crate::outputs::resolve(&config.outputs);

        Self {
            start_time,
            display_handle: dh,
            config,
            decoration_pressed: None,
            last_menu_click: None,
            window_menu: None,
            text_renderer: crate::text::TextRenderer::new(),
            icon_drag: None,
            desks: crate::desks::Desks::new(),
            desks_protocol: crate::desks_protocol::DesksProtocolState::new(),
            display_config,
            outputs_dirty: false,

            space,
            loop_signal,
            socket_name,

            compositor_state,
            xdg_shell_state,
            layer_shell_state,
            output_management,
            disabled_outputs: Vec::new(),
            pending_output_toggles: Vec::new(),
            pending_screencopy: Vec::new(),
            session_lock_state,
            idle: crate::idle::IdleState::default(),
            vrr: crate::vrr::VrrState::default(),
            udev: None,
            winit: None,
            pending_vrr_changes: Vec::new(),
            lock: crate::session_lock::LockState::default(),
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
            xwm: None,
            xdisplay: None,
            xwayland_shell_state,
            loop_handle: event_loop.handle(),
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
        event_loop: &mut EventLoop<'static, Wlrix>,
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
                        display.get_mut().dispatch_clients(state).unwrap();
                    }
                    Ok(PostAction::Continue)
                },
            )
            .unwrap();

        socket_name
    }

    /// Start XWayland, so X11 applications can run.
    ///
    /// XWayland announces itself once it is ready; only then can the X11 window
    /// manager attach and `DISPLAY` be published.
    pub fn start_xwayland(&mut self) {
        let (xwayland, client) = match XWayland::spawn(
            &self.display_handle,
            None,
            std::iter::empty::<(String, String)>(),
            true,
            std::process::Stdio::null(),
            std::process::Stdio::null(),
            |_| (),
        ) {
            Ok(spawned) => spawned,
            Err(err) => {
                tracing::warn!(
                    ?err,
                    "could not start XWayland; X11 applications will not run"
                );
                return;
            }
        };

        let result = self
            .loop_handle
            .insert_source(xwayland, move |event, _, data| {
                match event {
                    XWaylandEvent::Ready {
                        x11_socket,
                        display_number,
                    } => {
                        let wm =
                            X11Wm::start_wm(data.loop_handle.clone(), x11_socket, client.clone());
                        match wm {
                            Ok(wm) => {
                                data.xwm = Some(wm);
                                data.xdisplay = Some(display_number);
                                // The session is waiting for this to start X11-capable
                                // apps with a usable DISPLAY.
                                crate::handshake::announce(
                                    "DISPLAY",
                                    &format!(":{display_number}"),
                                );
                                // SAFETY: single-threaded, and X11 clients are spawned
                                // after this point.
                                unsafe {
                                    std::env::set_var("DISPLAY", format!(":{display_number}"));
                                }
                                tracing::info!(display = display_number, "XWayland ready");
                            }
                            Err(err) => {
                                tracing::error!(?err, "failed to attach the X11 window manager")
                            }
                        }
                    }
                    XWaylandEvent::Error => {
                        tracing::warn!("XWayland crashed on startup");
                    }
                }
            });

        if let Err(err) = result {
            tracing::error!(?err, "failed to watch XWayland");
        }
    }

    /// Re-advertise the layout to wlr-output-management clients, enabled and
    /// disabled heads alike.
    pub fn advertise_outputs(&mut self, display: &DisplayHandle) {
        let enabled: Vec<Output> = self.space.outputs().cloned().collect();
        let disabled = self.disabled_outputs.clone();
        self.output_management
            .outputs_changed(display, &enabled, &disabled, &self.vrr);
    }

    /// Re-read the config and apply what can change while running.
    ///
    /// Fired by `SIGHUP`. Today that is the keyboard: the keymap and repeat timing swap
    /// live, and smithay re-sends the keymap to every client for us. A config that no
    /// longer parses keeps the running one -- a typo during a reload must not wipe the
    /// keyboard out from under the user.
    ///
    /// The borrow dance matters: `set_xkb_config` wants `&mut self`, so the `XkbConfig`
    /// it is handed must not borrow `self`. It borrows a *local* clone of the new keyboard
    /// config instead, and `self.config` is only reassigned afterwards.
    pub fn reload_config(&mut self) {
        let loaded = crate::config::load();
        loaded.source.report();

        let keyboard_config = loaded.config.keyboard.clone();
        // An owned, Arc-backed handle: its borrow of `self` ends on this line, leaving
        // `self` free to pass to `set_xkb_config`.
        if let Some(keyboard) = self.seat.get_keyboard() {
            keyboard.change_repeat_info(keyboard_config.rate(), keyboard_config.delay());
            if let Err(err) = keyboard.set_xkb_config(self, keyboard_config.xkb()) {
                tracing::warn!(
                    ?err,
                    "new keyboard config did not compile; keeping the current keymap"
                );
            }
        }

        self.config = loaded.config;
    }

    /// Switch to the desk at `index` in the desk order, if there is one.
    pub fn switch_desk_index(&mut self, index: usize) {
        if let Some(&id) = self.desks.order().get(index) {
            self.switch_desk(id);
        }
    }

    /// Switch the active desk to `id`.
    pub fn switch_desk(&mut self, id: crate::desks::DeskId) {
        // No orphan relocation here: the restored windows are re-mapped at their saved
        // positions, and `outputs_for_element` does not reflect a fresh `map_element` until
        // the next `space.refresh()` -- so relocating now would wrongly treat every restored
        // window as off-screen and cascade it to the corner.
        crate::desks::switch_to(&mut self.space, &mut self.desks, id);
        crate::focus::focus_topmost(self);
        self.desks_changed();
        self.request_redraw();
    }

    /// Create a new desk (not activated), returning its id.
    pub fn create_desk(&mut self) -> crate::desks::DeskId {
        let id = self.desks.create();
        tracing::info!(name = self.desks.name(id).unwrap_or(""), "created desk");
        self.desks_changed();
        id
    }

    /// Delete a desk, unless it is the global or the last ordinary one.
    pub fn remove_desk(&mut self, id: crate::desks::DeskId) {
        if crate::desks::delete_desk(&mut self.space, &mut self.desks, id) {
            tracing::info!("removed desk");
            crate::focus::focus_topmost(self);
            self.desks_changed();
            self.request_redraw();
        }
    }

    /// Delete the active desk (a temporary keybind helper).
    pub fn delete_active_desk(&mut self) {
        self.remove_desk(self.desks.active());
    }

    /// Save the current display arrangement to `outputs.toml`, if it changed.
    ///
    /// Called once at the end of a redraw batch. The whole arrangement is snapshotted --
    /// every output, on or off -- so the file is always a complete picture that startup
    /// can restore verbatim. Failure to write is logged, never fatal: a compositor that
    /// refuses to draw because it could not save a preference is worse than one that
    /// forgets the preference.
    pub fn save_display_state_if_dirty(&mut self) {
        if !std::mem::take(&mut self.outputs_dirty) {
            return;
        }
        crate::outputs::save(&self.snapshot_outputs());
    }

    /// A complete snapshot of every output's current settings, enabled and disabled.
    fn snapshot_outputs(&self) -> Vec<crate::outputs::OutputConfig> {
        let enabled = self
            .space
            .outputs()
            .map(|output| self.output_entry(output, true));
        let disabled = self
            .disabled_outputs
            .iter()
            .map(|output| self.output_entry(output, false));
        enabled.chain(disabled).collect()
    }

    /// One output's current settings as a saveable entry.
    fn output_entry(&self, output: &Output, enabled: bool) -> crate::outputs::OutputConfig {
        let location = output.current_location();
        crate::outputs::OutputConfig {
            name: output.name(),
            mode: output
                .current_mode()
                .map(|mode| crate::outputs::format_mode(mode.size.w, mode.size.h, mode.refresh)),
            position: Some([location.x, location.y]),
            scale: Some(output.current_scale().fractional_scale()),
            transform: Some(
                crate::outputs::format_transform(output.current_transform()).to_owned(),
            ),
            // A disabled head must record that it is off; an enabled one leaves the
            // field out, since "on" is the default a bare entry already means.
            enabled: (!enabled).then_some(false),
            // Likewise VRR: only worth recording when it is on.
            adaptive_sync: self.vrr.enabled(output).then_some(true),
        }
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
        // A locked session routes the pointer to the locker alone; without this the
        // desktop would still be clickable underneath the lock screen.
        if self.lock.is_locked() {
            return crate::session_lock::surface_under(self, pos);
        }
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
