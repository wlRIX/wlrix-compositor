# wlrix-compositor

The wlRIX Wayland compositor. Recreates the window-management look and feel of IRIX's **4dwm** / Indigo Magic Desktop
(window borders, focus behavior, the Rooms/desks model) on modern Linux graphics.

- **Language:** Rust (edition 2024), built on [Smithay](https://github.com/Smithay/smithay), pinned to a git revision
  (see `Cargo.toml`)
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

Switch to a free VT (e.g., Ctrl+Alt+F3), log in, and run the binary with **no** host display in the environment, so the
selector chooses the udev backend:

```sh
env -u WAYLAND_DISPLAY -u DISPLAY ./target/debug/wlrix-compositor
```

Your monitors should light up showing the wlRIX clear color. It prints its Wayland socket; from another VT (or an ssh
session) point a client at it:

```sh
WAYLAND_DISPLAY=wayland-1 <some-wayland-client>
```

Keybindings: **Ctrl+Alt+F<n>** switches VT (the compositor releases DRM so you can get back to your login),
**Ctrl+Alt+Backspace** quits.

## Configuration

Read from `$XDG_CONFIG_HOME/wlrix/compositor.toml`, falling back to `/etc/wlrix/compositor.toml`. The first file found
is used whole rather than merged. Unknown keys are an error, so a typo is reported rather than quietly ignored; a file
that will not parse is reported and the built-in defaults are used, because refusing to start would leave a black screen
and no way in to fix it.

`SIGHUP` re-reads it — the settings apps find this compositor through the pidfile in
`$XDG_RUNTIME_DIR/wlrix-compositor.pid`.

```toml
[keyboard]
layout = "jp,us"           # more than one enables Super+Space to cycle
model = "jp106"
options = "grp:alt_shift_toggle"
repeat_delay = 200
repeat_rate = 25

[focus]
policy = "click"           # or "pointer"

[idle]
blank_after_secs = 600     # absent or 0 never blanks
```

Monitors are configured with `[[output]]` blocks, layered under the machine-written
`$XDG_STATE_HOME/wlrix/outputs.toml`; see `src/outputs.rs`.

### Keyboard focus

`policy = "click"` is the modern default: a press on a window focuses **and raises** it.

`policy = "pointer"` is IRIX's other mode — Motif called the pair `explicit` and `pointer`, and 4Dwm inherited both. The
window under the cursor has the keyboard, its 4Dwm frame included, so crossing onto a border or titlebar focuses that
window. Two things about it are decisions rather than consequences:

- **It does not raise.** A window that leapt to the front the instant the cursor crossed it would make a partly-covered
  window impossible to type into without disturbing the stack, and sweeping the pointer across the screen would
  reshuffle everything it passed. Clicking still raises, which is the only way to bring a buried window forward.
- **Focus stays put over bare desktop.** Moving off a window onto the desktop, a minimized icon, or the gap between two
  windows leaves the keyboard where it was. Strict Motif pointer focus drops focus to the root; here that would mean
  keystrokes disappearing whenever the cursor crossed a few pixels of background, and it would fight `wlrix-desktop`,
  which is a layer surface that takes focus of its own when clicked. A *click* on the desktop still clears focus, so
  there is a deliberate way to let go.

Focus does not follow the pointer while a move or resize grab is running, while a window menu or a minimized-icon move
is in progress, under a session lock, or where an overlay/top layer surface covers the windows. When a window closes or
a desk switches, focus goes to whatever the cursor is over rather than to the topmost window.
