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

[windows]
opaque_move = true         # false draws a red wireframe instead
opaque_resize = true

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

### Opaque move and resize

With `opaque_move` and `opaque_resize` on — the default — a window is dragged and stretched as itself, redrawing at each
new geometry. Turning either off gives IRIX's other mode: the window stays where it is and a **red wireframe** of where
its frame would land follows the pointer, with the change applied on release. The wireframe traces the same `frame_rect`
the real decoration uses, so what it promises is where the window arrives. A bordered window gets two concentric
outlines — the frame's outer edge and the inner edge where the border stops — plus a rule across the bottom of the
titlebar, taken from the same `titlebar_rect` the drawn frame uses, so the titlebar reads as its own closed box and the
outline says which way up the window is. An undecorated window gets the single outline.

The two are separate settings because they cost different things. A non-opaque *move* saves only compositing; a
non-opaque *resize* also means the client is configured **once**, on release, instead of on every motion event — which
is the difference between a smooth drag and a stuttering one for an application that re-lays-out expensively. IRIX
offered the choice because opaque dragging was ruinous on the hardware of the day; it is kept here because it is part of
the desktop's feel.

Both are read when the grab starts, so a setting changed mid-drag cannot leave half a move in each mode. The wireframe
color comes from the generated palette (`dragOutline`), so a theme restyles it along with everything else.

## Window capabilities

4dwm drew only the controls a window could actually use, and `frame::capabilities` is the same idea: a fixed-size dialog
has no business showing a maximize button that does nothing. What it works out feeds the titlebar (which buttons exist),
the border (whether the corner grips are drawn and whether it resizes), the window menu (which items are greyed), and
`minimize_window`/`maximize_window` themselves — so a control that was drawn away is not reachable by keybind either.

| capability  | how it is known                                                                             |
|-------------|---------------------------------------------------------------------------------------------|
| resizable   | `min_size == max_size` on an axis, from xdg-shell or X11 `WM_NORMAL_HINTS`. Per axis.       |
| maximizable | derived: a window fixed in both axes cannot grow into a maximized one.                      |
| minimizable | not a dialog — no `xdg_toplevel` parent, not X11 `_NET_WM_WINDOW_TYPE_DIALOG` or transient. |

Zero means "unconstrained" in both protocols, so a *maximum* on its own is a ceiling rather than a fixed size and the
handles stay. A window that fixes only its width keeps its top and bottom handles, and its corners degrade from diagonal
resize to the axis that is left.

A fixed-size window keeps its border — it is still the window's edge, still occludes what is under it, and a middle-drag
still moves the window — but loses the corner sections, because those *are* the resize grips, and drawing grips on a
window that cannot be dragged invites a drag that will not happen. The border keeps the plain arrow and a left press on
it does nothing.

**What cannot be detected.** xdg-shell has no way for a client to refuse maximizing or minimizing;
`xdg_toplevel.wm_capabilities` runs the other way, compositor to client. X11's `_MOTIF_WM_HINTS` *functions* field
answers all three — it is what IRIX itself read — but smithay parses the property and exposes only its decorations field
(`X11Surface::is_decorated`), so reaching it means patching a pinned dependency. And Avalonia's Wayland backend does not
map `CanResize`/`CanMaximize`/`CanMinimize` onto anything at all (`Avalonia/src/Avalonia.Wayland/WindowImpl.cs`), so an
Avalonia window on Wayland says nothing here; the same window under XWayland does, through its size hints.
