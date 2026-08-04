// SPDX-License-Identifier: GPL-3.0-or-later
// Adapted from Smithay's `smallvil` example (MIT-licensed). See the NOTICE file.
use std::{cell::RefCell, ffi::OsString, rc::Rc, sync::Arc};

use smithay::{
    backend::{renderer::gles::GlesRenderer, session::libseat::LibSeatSession},
    desktop::{PopupManager, Space, Window, WindowSurfaceType, layer_map_for_output},
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
    utils::{Logical, Point, Rectangle},
    wayland::{
        compositor::{CompositorClientState, CompositorState},
        content_type::ContentTypeState,
        cursor_shape::CursorShapeManagerState,
        dmabuf::DmabufState,
        foreign_toplevel_list::ForeignToplevelListState,
        fractional_scale::FractionalScaleManagerState,
        input_method::InputMethodManagerState,
        output::OutputManagerState,
        pointer_constraints::PointerConstraintsState,
        presentation::PresentationState,
        relative_pointer::RelativePointerManagerState,
        selection::data_device::DataDeviceState,
        selection::primary_selection::PrimarySelectionState,
        selection::wlr_data_control::DataControlState,
        shell::{
            wlr_layer::{Layer as WlrLayer, WlrLayerShellState},
            xdg::{XdgShellState, decoration::XdgDecorationState},
        },
        shm::ShmState,
        single_pixel_buffer::SinglePixelBufferState,
        socket::ListeningSocketSource,
        text_input::TextInputManagerState,
        viewporter::ViewporterState,
        virtual_keyboard::VirtualKeyboardManagerState,
        xdg_activation::XdgActivationState,
        xdg_foreign::XdgForeignState,
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
    /// The red wireframe drawn while a non-opaque move or resize is under way; see
    /// [`crate::config::WindowsConfig`]. `None` whenever nothing is being rubber-banded,
    /// which under the default opaque settings is always.
    pub drag_outline: Option<crate::decoration::DragOutline>,

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
    /// Set when the desks' names, order or which is active changed and needs saving to
    /// `desks.toml`. Drained once per dispatch, for the same reason.
    ///
    /// Separate from `desks_changed`, which also fires for windows moving between desks:
    /// that changes nothing about what is saved, and rewriting the file every time a window
    /// maps would be a lot of writing for no difference.
    pub desks_dirty: bool,

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
    /// Clipboard managers: watch and set the clipboard and primary selection.
    pub data_control_state: DataControlState,
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
    /// Clients name the cursor they want (text caret, resize arrow) instead of drawing one,
    /// so the pointer looks the same everywhere.
    pub cursor_shape_state: CursorShapeManagerState,
    /// The running-window list bars and overviews read (title/app-id, read-only).
    pub foreign_toplevel_state: ForeignToplevelListState,
    /// The wlr window list taskbars drive: the same list, plus activate/minimize/maximize/close.
    pub foreign_toplevel_management:
        crate::foreign_toplevel_management::ForeignToplevelManagementState,
    /// The standard desk view pagers read (`ext-workspace-v1`).
    pub workspace_protocol: crate::workspace_protocol::WorkspaceProtocolState,
    /// Monitor power: which outputs are switched off, and the idle-blank countdown.
    pub power: crate::power::PowerState,
    /// Per-output color ramps set by a night-light tool.
    pub gamma: crate::gamma::GammaState,
    /// A 1x1 solid-color buffer, so a client wanting a plain fill need not allocate one.
    pub single_pixel_buffer_state: SinglePixelBufferState,
    /// Clients tag a surface as photo/video/game. Stored on the surface for a future
    /// presentation policy (tearing, refresh matching); nothing acts on it yet.
    pub content_type_state: ContentTypeState,
    /// Lets one client parent a surface to another's window: a portal or file picker opening
    /// as a dialog of the app that asked for it.
    pub xdg_foreign_state: XdgForeignState,
    /// Text fields tell the compositor where they are, so an IME can follow the caret.
    pub text_input_state: TextInputManagerState,
    /// The IME itself (kana->kanji conversion, candidate popup), e.g. `fcitx5`.
    pub input_method_state: InputMethodManagerState,
    /// Lets the IME (or an on-screen keyboard) inject the keys it does not consume.
    pub virtual_keyboard_state: VirtualKeyboardManagerState,
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
    /// `ext-image-copy-capture` and its capture sources: the standard successor to the above.
    pub image_capture: crate::image_capture::ImageCaptureState,
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
    /// Whether the compositor set the current cursor (for its frame, menu, or the desktop)
    /// rather than a client. Lets it hand the cursor back once when the pointer returns to a
    /// client surface, which gets no fresh `enter` to prompt its own `set_cursor`.
    pub cursor_from_chrome: bool,
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
        let image_capture = crate::image_capture::ImageCaptureState::new(&dh);
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
        // Input methods. `text-input` is for ordinary applications, so it is unrestricted; the
        // other two are for the IME itself and let a client watch every keystroke and inject
        // keys, so they are privileged. Every client passes the filter for now, for the same
        // reason the session lock does: wlRIX has no way to identify trusted clients yet
        // (`wp_security_context_v1` is not implemented). Revisit alongside that.
        let cursor_shape_state = CursorShapeManagerState::new::<Self>(&dh);
        let foreign_toplevel_state = ForeignToplevelListState::new::<Self>(&dh);
        let foreign_toplevel_management =
            crate::foreign_toplevel_management::ForeignToplevelManagementState::new();
        let _foreign_toplevel_management_global =
            crate::foreign_toplevel_management::ForeignToplevelManagementState::create_global(&dh);
        let workspace_protocol = crate::workspace_protocol::WorkspaceProtocolState::new();
        let _output_power_global = crate::power::PowerState::create_global(&dh);
        let _gamma_global = crate::gamma::GammaState::create_global(&dh);
        let single_pixel_buffer_state = SinglePixelBufferState::new::<Self>(&dh);
        let content_type_state = ContentTypeState::new::<Self>(&dh);
        // Only an unsandboxed client may create a security context; letting a sandboxed one
        // mint further contexts would defeat the point.
        let _security_context_global =
            smithay::wayland::security_context::SecurityContextState::new::<Self, _>(
                &dh,
                |client| !Self::client_is_sandboxed(client),
            );
        let _workspace_global =
            crate::workspace_protocol::WorkspaceProtocolState::create_global(&dh);
        let xdg_foreign_state = XdgForeignState::new::<Self>(&dh);
        let text_input_state = TextInputManagerState::new::<Self>(&dh);
        // The IME sees every keystroke and can inject keys, so a sandboxed client must not
        // reach it. Now that `wp_security_context_v1` tags them, that is finally expressible.
        let input_method_state = InputMethodManagerState::new::<Self, _>(&dh, |client| {
            !Self::client_is_sandboxed(client)
        });
        let virtual_keyboard_state = VirtualKeyboardManagerState::new::<Self, _>(&dh, |client| {
            !Self::client_is_sandboxed(client)
        });
        // Clipboard managers read *everything* copied, so a sandboxed client must not reach
        // this. Primary selection is handed in so `wl-paste --primary` works too.
        let data_control_state =
            DataControlState::new::<Self, _>(&dh, Some(&primary_selection_state), |client| {
                !Self::client_is_sandboxed(client)
            });
        let xdg_activation_state = XdgActivationState::new::<Self>(&dh);
        let xdg_decoration_state = XdgDecorationState::new::<Self>(&dh);
        let xwayland_shell_state = XWaylandShellState::new::<Self>(&dh);
        // Locking the screen is the session's business: a sandboxed app must not be able to
        // put a lock surface up (nor, by locking and unlocking, learn when the user is away).
        let session_lock_state = smithay::wayland::session_lock::SessionLockManagerState::new::<
            Self,
            _,
        >(&dh, |client| !Self::client_is_sandboxed(client));
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
            drag_outline: None,
            // Names and order come back from the last session; see `desks::restore`.
            desks: crate::desks::Desks::restore(),
            desks_protocol: crate::desks_protocol::DesksProtocolState::new(),
            display_config,
            outputs_dirty: false,
            desks_dirty: false,

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
            image_capture,
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
            data_control_state,
            presentation_state,
            viewporter_state,
            fractional_scale_state,
            relative_pointer_state,
            pointer_constraints_state,
            cursor_shape_state,
            foreign_toplevel_state,
            foreign_toplevel_management,
            workspace_protocol,
            power: crate::power::PowerState::default(),
            gamma: crate::gamma::GammaState::default(),
            single_pixel_buffer_state,
            content_type_state,
            xdg_foreign_state,
            text_input_state,
            input_method_state,
            virtual_keyboard_state,
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
            cursor_from_chrome: true,
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
            // No extra Xwayland arguments; `true` opens the abstract socket as well as the
            // filesystem one, which some older X11 clients still expect.
            std::iter::empty::<String>(),
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
                        let wm = X11Wm::start_wm(
                            data.loop_handle.clone(),
                            &data.display_handle,
                            x11_socket,
                            client.clone(),
                        );
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
    /// Fired by `SIGHUP`. That is the keyboard -- the keymap and repeat timing swap live, and
    /// smithay re-sends the keymap to every client for us -- and the idle blank timeout. A
    /// config that no longer parses keeps the running one: a typo during a reload must not
    /// wipe the keyboard out from under the user.
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

        // A changed `[idle] blank_after_secs` has to be picked up here. Without this the new
        // timeout sits in `self.config` doing nothing until the next keypress happens to call
        // `notice_activity_for_blanking`, so editing it and reloading looks like it did not
        // work -- and then works minutes later, which is worse than not working at all.
        self.arm_blank_timer();
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
        self.desks_dirty = true;
        self.desks_changed();
        self.request_redraw();
    }

    /// Create a new desk (not activated), returning its id.
    pub fn create_desk(&mut self) -> crate::desks::DeskId {
        let id = self.desks.create();
        tracing::info!(name = self.desks.name(id).unwrap_or(""), "created desk");
        self.desks_dirty = true;
        self.desks_changed();
        id
    }

    /// Delete a desk, unless it is the global or the last ordinary one.
    pub fn remove_desk(&mut self, id: crate::desks::DeskId) {
        if crate::desks::delete_desk(&mut self.space, &mut self.desks, id) {
            tracing::info!("removed desk");
            crate::focus::focus_topmost(self);
            self.desks_dirty = true;
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

    /// Called once per dispatch. Writes only when something about the desks themselves
    /// changed; see `desks_dirty`.
    pub fn save_desks_if_dirty(&mut self) {
        if !std::mem::take(&mut self.desks_dirty) {
            return;
        }
        crate::desks::save(&self.desks);
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

    /// What the pointer is over, front to back.
    ///
    /// Layer surfaces are in this: the overlay and top layers sit above every window, the
    /// bottom and background layers below them. That ordering is the protocol's, and it is
    /// what lets a background-layer client -- the desktop icons -- be clicked at all, while
    /// keeping it out of the way of the windows on top of it.
    pub fn surface_under(
        &self,
        pos: Point<f64, Logical>,
    ) -> Option<(WlSurface, Point<f64, Logical>)> {
        // A locked session routes the pointer to the locker alone; without this the
        // desktop would still be clickable underneath the lock screen.
        if self.lock.is_locked() {
            return crate::session_lock::surface_under(self, pos);
        }

        let window_hit = || {
            self.space
                .element_under(pos)
                .and_then(|(window, location)| {
                    window
                        .surface_under(pos - location.to_f64(), WindowSurfaceType::ALL)
                        .map(|(s, p)| (s, (p + location).to_f64()))
                })
        };

        // Off every output there are no layer surfaces to consider, so this is windows only.
        let Some((output, output_geo)) = self.output_at(pos) else {
            return window_hit();
        };

        // One guard, held across both probes below. `layer_map_for_output` hands back a
        // non-reentrant `MutexGuard`: taking a second one for the same output deadlocks the
        // whole event loop, which is what once left the machine on a black screen with dead
        // VT switching.
        let layers = layer_map_for_output(&output);
        let local = pos - output_geo.loc.to_f64();
        let layer_hit = |layer| {
            layers.layer_under(layer, local).and_then(|surface| {
                let layer_loc = layers.layer_geometry(surface)?.loc;
                surface
                    .surface_under(local - layer_loc.to_f64(), WindowSurfaceType::ALL)
                    .map(|(s, p)| (s, (p + layer_loc + output_geo.loc).to_f64()))
            })
        };

        layer_hit(WlrLayer::Overlay)
            .or_else(|| layer_hit(WlrLayer::Top))
            .or_else(window_hit)
            .or_else(|| layer_hit(WlrLayer::Bottom))
            .or_else(|| layer_hit(WlrLayer::Background))
    }

    /// The output containing `pos`, with its geometry.
    pub fn output_at(&self, pos: Point<f64, Logical>) -> Option<(Output, Rectangle<i32, Logical>)> {
        self.space.outputs().find_map(|output| {
            let geometry = self.space.output_geometry(output)?;
            geometry
                .contains(pos.to_i32_round())
                .then(|| (output.clone(), geometry))
        })
    }

    /// Whether an overlay- or top-layer surface covers `pos`.
    ///
    /// Those two layers sit above every window, so a press there belongs to the layer client:
    /// it must not reach the server-side frame of a window underneath, nor raise one.
    pub fn layer_covers_windows_at(&self, pos: Point<f64, Logical>) -> bool {
        let Some((output, output_geo)) = self.output_at(pos) else {
            return false;
        };
        let layers = layer_map_for_output(&output);
        let local = pos - output_geo.loc.to_f64();
        layers.layer_under(WlrLayer::Overlay, local).is_some()
            || layers.layer_under(WlrLayer::Top, local).is_some()
    }

    /// The layer surface under `pos` that is willing to take keyboard focus, if any.
    ///
    /// Only consulted when a press found no window: a layer surface that asked for
    /// `on-demand` (or `exclusive`) keyboard interactivity gets focus when clicked, the same
    /// click-to-focus rule windows follow. One that asked for `none` is left alone, so an
    /// inert backdrop never steals the keyboard.
    pub fn focusable_layer_under(&self, pos: Point<f64, Logical>) -> Option<WlSurface> {
        let (output, output_geo) = self.output_at(pos)?;
        let layers = layer_map_for_output(&output);
        let local = pos - output_geo.loc.to_f64();
        [
            WlrLayer::Overlay,
            WlrLayer::Top,
            WlrLayer::Bottom,
            WlrLayer::Background,
        ]
        .into_iter()
        .find_map(|layer| {
            let surface = layers.layer_under(layer, local)?;
            surface
                .can_receive_keyboard_focus()
                .then(|| surface.wl_surface().clone())
        })
    }
}

#[derive(Default)]
pub struct ClientState {
    pub compositor_state: CompositorClientState,
    /// Set for clients that connected through a sandbox's restricted socket
    /// (`wp_security_context_v1`), carrying the sandbox engine and app id it declared.
    /// `None` is an ordinary client on the session socket.
    ///
    /// This is what lets the privileged protocols tell "the session's own IME" from "some
    /// Flatpak app"; see [`Wlrix::client_is_sandboxed`].
    pub security_context: Option<smithay::wayland::security_context::SecurityContext>,
}

impl ClientData for ClientState {
    fn initialized(&self, _client_id: ClientId) {}
    fn disconnected(&self, _client_id: ClientId, _reason: DisconnectReason) {}
}
