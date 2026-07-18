# wlrix-compositor

The wlRIX Wayland compositor. Recreates the window-management look and feel of
IRIX's **4dwm** / Indigo Magic Desktop (window borders, focus behavior, the
Rooms/desks model) on modern Linux graphics.

- **Language:** Rust (edition 2024), built on [Smithay](https://github.com/Smithay/smithay) 0.7
- **License:** GPL-3.0-or-later (see `LICENSE`; attribution in `NOTICE`)
- **References:** Smithay's `anvil` / `smallvil` examples.

## Status

Scaffold. `cargo build && cargo run` prints a banner; no compositor logic yet.

## Build

Requires a Rust toolchain and these system libraries (Arch package names shown):
`wayland` `libinput` `seatd`/`libseat` `mesa` (gbm/EGL/GLESv2) `libxkbcommon`
`libdrm` `systemd`/`libudev`. (M0 only needs wayland + mesa/EGL + libxkbcommon.)

```sh
cargo build
```

## Run

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
