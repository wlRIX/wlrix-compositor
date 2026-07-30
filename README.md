# wlrix-compositor

The wlRIX Wayland compositor. Recreates the window-management look and feel of
IRIX's **4dwm** / Indigo Magic Desktop (window borders, focus behavior, the
Rooms/desks model) on modern Linux graphics.

- **Language:** Rust (edition 2024), built on [Smithay](https://github.com/Smithay/smithay), pinned to a git revision (see `Cargo.toml`)
- **License:** GPL-3.0-or-later (see `LICENSE`; attribution in `NOTICE`)
- **References:** Smithay's `anvil` / `smallvil` examples.

## Build

Requires a Rust toolchain and these system libraries (Arch package names shown):
`wayland` `libinput` `seatd`/`libseat` `mesa` (gbm/EGL/GLESv2) `libxkbcommon`
`libdrm` `systemd`/`libudev`.

```sh
cargo build
```

## Run

### Nested

From inside a Wayland or X11 session:

```sh
cargo run                    # opens a nested compositor window
cargo run -- -c alacritty    # and auto-spawns a client into it
```

The compositor prints its Wayland socket (e.g. `wayland-1`). Point a client at it:

```sh
WAYLAND_DISPLAY=wayland-1 wayland-info
WAYLAND_DISPLAY=wayland-1 <some-wayland-client>
```

### Standalone

Switch to a free VT (e.g., Ctrl+Alt+F3), log in, and run the binary with **no** host
display in the environment, so the selector chooses the udev backend:

```sh
env -u WAYLAND_DISPLAY -u DISPLAY ./target/debug/wlrix-compositor
```

Your monitors should light up showing the wlRIX clear color. It prints its Wayland
socket; from another VT (or an ssh session) point a client at it:

```sh
WAYLAND_DISPLAY=wayland-1 <some-wayland-client>
```

Keybindings: **Ctrl+Alt+F<n>** switches VT (the compositor releases DRM so you can
get back to your login), **Ctrl+Alt+Backspace** quits.
