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

`just install` puts a system default at `/etc/wlrix/compositor.toml` — `data/compositor.toml`, which sets the cursor
theme and nothing else. An existing one is never overwritten, and `uninstall` leaves it behind. Because the first file
found wins outright, a `~/.config/wlrix/compositor.toml` written from scratch **replaces** it rather than adding to it:
copy the system one and edit that, or let a settings panel seed it.

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
raise_on_click = true      # false: a click in the client area focuses but does not restack

[windows]
opaque_move = true         # false draws a red wireframe instead
opaque_resize = true

[cursor]
theme = "sgi"              # an XCursor theme name; wlrix-assets installs sgi
size = 32                  # nominal size; the nearest the theme carries is used

[idle]
blank_after_secs = 600     # deprecated -- see below; absent or 0 never blanks
```

Monitors are configured with `[[output]]` blocks, layered under the machine-written
`$XDG_STATE_HOME/wlrix/outputs.toml`; see `src/outputs.rs`.

```toml
[[output]]
name = "DP-4"              # the connector name, as `wlr-randr` prints it
mode = "2560x1440@240"
position = [0, 0]
scale = 1.0
adaptive_sync = true
hdr = true                 # PQ / BT.2020. udev only, and only where the panel does ST2084
sdr_white_nits = 203       # where the desktop's white lands; BT.2408 says 203
linear_blending = true     # alpha-composite in linear light; see below
```

`hdr` needs both halves of the hardware to agree: the connector must offer `Colorspace` with a
`BT2020_RGB` entry and `HDR_OUTPUT_METADATA`, and the panel's EDID must advertise ST2084. Asking for it on a display
that cannot do it is logged and ignored, not fatal. Every connector logs what it is capable of at startup either way, so
a monitor that is quietly not HDR-capable is visible rather than mysterious.

Two consequences worth knowing before turning it on. Direct scanout and the hardware cursor are disabled on an HDR
output — everything has to go through the encode shader, so nothing may be promoted to a DRM plane. And
`zwlr_gamma_control` (`gammastep`, `wlsunset`) is refused there: the CRTC's gamma table sits after the PQ encode, so a
night-light ramp would be operating on PQ code values. SDR outputs are unaffected on both counts.

`linear_blending` decides where alpha compositing happens on an HDR output, and it is a genuine trade rather than a
right answer. On, blending is physically correct: a half-transparent white over black lands at half the *light* instead
of half the code value. Off, it happens on encoded values, exactly as every SDR output here always has. The thing to
look at before deciding is **antialiased text**: glyph coverage blended in linear light comes out thinner on a dark
titlebar, because font rasterisers — cosmic-text included — are tuned against sRGB-space blending. It defaults on, which
has been checked on an OLED panel: titlebar text does come out slightly thinner than on the SDR screen beside it, but
stays readable, and CJK renders correctly. Set it to `false`
and restart if you prefer the heavier weight. (Bright specks around glyph edges are *not* this switch — that was a
pre-multiplication bug in `text.rs`, fixed.) SDR outputs are unaffected either way.

**An HDR monitor next to an SDR one will look brighter, and that is not a bug.** PQ is an *absolute* encoding: a code
value means a fixed number of nits, and the panel's own brightness control largely stops applying once it is in HDR
mode. The desktop's white is pinned at
`sdr_white_nits`, while the SDR monitor beside it emits whatever its brightness setting says — usually rather less.
Every compositor with per-output HDR has this (it is what KDE's "SDR brightness" slider is for). Match them by lowering
`sdr_white_nits` until the two whites agree, or by turning the SDR monitor up. 203 is the BT.2408 reference and the
right *default*, not a value that happens to match any particular panel.

### Saved state

Two files under `$XDG_STATE_HOME/wlrix/` are written by the compositor rather than by hand, atomically (a sibling temp
file renamed over the target), and a broken one is reported and ignored rather than being fatal:

| File           | What it remembers                                           |
|----------------|-------------------------------------------------------------|
| `outputs.toml` | each monitor's mode, position, scale, orientation and power |
| `desks.toml`   | the desks' names, their order, and which one was active     |

`desks.toml` deliberately holds no window information and no desk ids. Windows belong to processes that are gone by the
time it is read, so a restored desk comes back empty; ids are handed out fresh on load, because nothing outside one run
of the compositor refers to them — clients learn them from the protocol each time they bind. Saving them would be saving
an implementation detail and inviting it to be wrong.

### `[idle]` is deprecated

`wlrix-idle` owns idle policy for a wlRIX session and is started as part of the default session, so leave this section
out. It is still parsed so an existing config does not break, and it says so once in the log when it arms.

A timer inside the compositor can only see what the compositor sees. It cannot notice a controller — libinput classifies
a gamepad as a joystick and drops it — it cannot serve
`org.freedesktop.ScreenSaver`, so an application playing a film has no way to say "not now", and it cannot take a logind
delay inhibitor to lock before the machine suspends.

Do not run both. A blank a *client* asked for is deliberately left alone by input, so once this timer has fired behind
`wlrix-idle`'s back, nothing switches the monitors on again.

### Cursors

`[cursor] theme` is an XCursor theme *name* — a directory under `share/icons` on an XDG data directory, or under
`~/.icons`. `wlrix-assets` installs `sgi`, the IRIX pointer set, and the system default config names it. Each is
resolved in the same order:

| Setting | First            | Then            | Otherwise |
|---------|------------------|-----------------|-----------|
| `theme` | `[cursor] theme` | `XCURSOR_THEME` | `default` |
| `size`  | `[cursor] size`  | `XCURSOR_SIZE`  | `24`      |

The file comes first because it is the desktop's own answer; the environment is still consulted so that running this
compositor nested inside another desktop picks up the theme that desktop already exported, rather than a name that may
not be installed there. An empty string or a size of zero counts as absent at every step.

Whatever it settles on is reported to `wlrix-session` over the startup handshake as `XCURSOR_THEME`/`XCURSOR_SIZE`,
which puts it in the environment of every app the session starts and in the D-Bus and systemd activation environments.
That is the point of deciding it here: a toolkit loads its *own* pointer from those variables, so without them a GTK
window's cursor would be Adwaita's while the desktop's was the theme wlRIX ships. Clients that use `cursor-shape-v1`
get the compositor's images directly and need neither.

A theme that is not installed is reported once and a small built-in arrow is drawn, so the pointer is never invisible. A
shape the theme does not carry falls back to that theme's own arrow, cached under the shape asked for, so a miss costs
one lookup rather than one per frame. Legacy names are tried after modern ones (`left_ptr` for `default`, `xterm`
for `text`), which is what makes themes drawn for X11 — `sgi` among them — work unchanged.

`SIGHUP` picks up a changed `[cursor]` and only a changed one: reloading means re-reading every shape used so far, and a
reload for an unrelated setting should not cost that. **Clients keep the theme they were started with.** Their
environment was fixed when they launched, and nothing can reach into a running process to change it, so a theme change
is fully in force at the next login.

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

The *resizable* border keeps its eight sections — four corner Ls and four edge middles, with the arms ending aligned
with the titlebar buttons. Each is separately raised, so the join between two of them reads as one's shadow beside the
next one's highlight, and that seam has to cross the **whole** band. Which of a piece's four shadows run the full span
therefore depends on which way its row is laid out (`Run` in `decoration.rs`): the top and bottom sections butt left to
right, everything else top to bottom. A corner L is drawn as two overlapping rects with a patch over the join, because
its inward shadow belongs on the inside of the L and must start at the L's inner corner rather than running out to the
frame's edge.

What replaces all of that on a fixed-size window is an **unbroken ring**, not four beveled bands. A band shades all four
of its own edges, so the seam between the top band and the titlebar would carry on past the titlebar and across both top
corners, with the bottom band doing the same — which is what the first attempt looked like, and not what IRIX drew. A
ring shades only its outer and inner edges, leaving the corners plain face.

### The wlRIX shell apps

Two app ids are framed by rule rather than by what they ask for (`placement::shell_frame`):

| app       | frame                                                                     |
|-----------|---------------------------------------------------------------------------|
| greeter   | none at all — it must not be movable or dismissable                       |
| toolchest | a titlebar and nothing else: no border, no menu/minimize/maximize buttons |

The toolchest's titlebar is not decoration for its own sake. It is what makes the panel movable and gives it a window
menu, and it is why the client needs no chrome of its own — IRIX's toolchest was the same. Everything else goes: a
border would be a resize grip on a panel that does not resize, and the three buttons do things a toolchest does not do.
Its capabilities are empty to match, so the window menu grays out Minimize and Maximize rather than offering what the
titlebar has already taken away.

Its title is **centred**, not left-aligned. An ordinary window's title starts where the menu button stops, so the bar
reads as one line of controls; a toolchest has no buttons for a title to line up beside, and left-aligned it would sit
against an edge with nothing to relate to. A title too wide to center falls back to starting at the left and clipping
from the right, as a left-aligned one does — shifting it further left would clip the beginning of the name, which is the
part that says which window this is.

With no border there is no inner edge for the move wireframe to trace, so a non-opaque drag shows a single ring around
the titlebar and client together, plus the rule under the titlebar.

The Desks overview is an ordinary framed window and is not listed. `wlrix-desktop` is not here either, and never will
be: the desktop icons are a layer-shell background surface rather than a window, so there is no frame to suppress.

**What cannot be detected.** xdg-shell has no way for a client to refuse maximizing or minimizing;
`xdg_toplevel.wm_capabilities` runs the other way, compositor to client. X11's `_MOTIF_WM_HINTS` *functions* field
answers all three — it is what IRIX itself read — but smithay parses the property and exposes only its decorations field
(`X11Surface::is_decorated`), so reaching it means patching a pinned dependency. And Avalonia's Wayland backend does not
map `CanResize`/`CanMaximize`/`CanMinimize` onto anything at all (`Avalonia/src/Avalonia.Wayland/WindowImpl.cs`), so an
Avalonia window on Wayland says nothing here; the same window under XWayland does, through its size hints.
