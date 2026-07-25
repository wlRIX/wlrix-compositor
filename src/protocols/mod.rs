// SPDX-License-Identifier: GPL-3.0-or-later
//! wlRIX-specific Wayland protocols, generated from local XML at compile time.
//!
//! The `wayland-scanner` proc-macros read the XML relative to `CARGO_MANIFEST_DIR` and emit
//! server-side bindings, exactly as the `wayland-protocols` crate builds its own modules --
//! so there is no `build.rs`. Sourcing `wayland_server` from Smithay's re-export keeps the
//! generated types identical to the ones the rest of the compositor uses.

/// The `wlrix-desks` protocol: desks (virtual desktops), their windows, live geometry, and
/// desk/window commands. See `src/protocols/wlrix-desks.xml`.
#[allow(
    dead_code,
    non_camel_case_types,
    unused_unsafe,
    unused_variables,
    non_upper_case_globals,
    non_snake_case,
    unused_imports,
    missing_docs,
    clippy::all,
    clippy::pedantic
)]
pub mod wlrix_desks {
    // The generated code references `wayland_server` and `wayland_backend` by crate name;
    // alias both from Smithay's re-exports so the types unify with the rest of the compositor
    // and no direct wayland-server/backend dependency is needed.
    use smithay::reexports::wayland_server;
    use smithay::reexports::wayland_server::backend as wayland_backend;
    use wayland_server::protocol::*;

    pub mod __interfaces {
        use smithay::reexports::wayland_server::backend as wayland_backend;
        use smithay::reexports::wayland_server::protocol::__interfaces::*;
        wayland_scanner::generate_interfaces!("src/protocols/wlrix-desks.xml");
    }
    use self::__interfaces::*;

    wayland_scanner::generate_server_code!("src/protocols/wlrix-desks.xml");
}
