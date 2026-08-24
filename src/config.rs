// SPDX-License-Identifier: GPL-3.0-or-later
//! The compositor config file.
//!
//! ```toml
//! # ~/.config/wlrix/compositor.toml
//! [keyboard]
//! layout = "jp"          # a comma-separated list, e.g. "jp,us", enables the toggle key
//! model = "jp106"
//! variant = ""
//! options = "grp:alt_shift_toggle"
//! repeat_delay = 200
//! repeat_rate = 25
//!
//! [focus]
//! policy = "pointer"     # or "click", the default
//!
//! [windows]
//! opaque_move = false    # show a red wireframe instead of the window itself
//! opaque_resize = false
//!
//! [cursor]
//! theme = "sgi"          # an XCursor theme name, as installed under share/icons
//! size = 32
//!
//! [keybinds]
//! "Ctrl+Q" = "close"     # see `crate::keybinds`; these layer over the built-in defaults
//! ```
//!
//! Read from the user's config directory first, then `/etc/wlrix`; the first file found
//! wins outright rather than merging, so what a user sees in their own file is the whole
//! of what they get. This mirrors `wlrix-session`'s config, deliberately: one shape of
//! file across the stack.
//!
//! `[keybinds]` is the one section whose *entries* are merged rather than replaced -- see
//! [`crate::keybinds`] for why. The rule above is about which file wins, and still holds:
//! only one file's `[keybinds]` is ever read.
//!
//! Unknown keys are an error. A silently ignored typo in a config file is a bad
//! afternoon, and the cost of being strict is a clear message instead.

use std::path::{Path, PathBuf};

use serde::Deserialize;
use smithay::input::keyboard::XkbConfig;
use tracing::warn;

use crate::keybinds;

/// Where the config lives, relative to a config directory.
const CONFIG_NAME: &str = "wlrix/compositor.toml";
/// Consulted when the user has no config of their own.
const SYSTEM_CONFIG_DIR: &str = "/etc";

/// smithay's defaults when `add_keyboard` is handed `XkbConfig::default()`: an empty
/// field means "let libxkbcommon pick", which for delay/rate we spell out so a reload
/// and a fresh start agree.
const DEFAULT_REPEAT_DELAY: i32 = 200;
const DEFAULT_REPEAT_RATE: i32 = 25;

/// What an unconfigured pointer falls back to. `default` is the theme name every XCursor loader
/// treats as "whatever this machine calls its default"; 24 is the size the toolkits assume when
/// nothing says otherwise, and what this compositor used before there was a `[cursor]` section.
const DEFAULT_CURSOR_THEME: &str = "default";
const DEFAULT_CURSOR_SIZE: u32 = 24;

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// The keyboard layout, model and repeat behavior.
    #[serde(default)]
    pub keyboard: KeyboardConfig,
    /// Which color scheme the chrome is drawn in.
    #[serde(default)]
    pub appearance: AppearanceConfig,
    /// Hand-set per-monitor defaults. The machine-written `outputs.toml` is layered on
    /// top of these at startup; see [`crate::outputs`].
    #[serde(default, rename = "output")]
    pub outputs: Vec<crate::outputs::OutputConfig>,
    /// What the compositor does when the session is left alone.
    #[serde(default)]
    pub idle: IdleConfig,
    /// How a window comes to have the keyboard.
    #[serde(default)]
    pub focus: FocusConfig,
    /// How windows behave while being moved and resized.
    #[serde(default)]
    pub windows: WindowsConfig,
    /// Which pointer theme is drawn, and at what size.
    #[serde(default)]
    pub cursor: CursorConfig,
    /// Key combinations, layered over the built-in defaults.
    #[serde(default)]
    pub keybinds: KeybindsConfig,
}

/// The `[keybinds]` section: what the user's file says about key combinations.
///
/// A map rather than a struct, since the keys are combinations the user invents, so
/// `deny_unknown_fields` has nothing to say about it. Everything is resolved here, at parse
/// time, rather than being carried around as strings -- which is what makes `--check-config`
/// (and so `wlrix-settings-daemon`, which runs it before every write) reject a misspelled
/// combination or action instead of writing a file the compositor would silently ignore half
/// of.
///
/// A `Vec` rather than a map, because the entries are applied in turn over the defaults and
/// nothing here needs to look one up. Order does not matter: TOML rejects a table with the
/// same key twice, so two entries can only differ in spelling -- and `"Alt+F4"` and
/// `"alt+f4"` normalize to one [`keybinds::Combo`], which resolving then applies twice to the
/// same effect.
#[derive(Debug, Default, Clone)]
pub struct KeybindsConfig(pub Vec<(keybinds::Combo, Option<keybinds::Action>)>);

impl TryFrom<toml::Table> for KeybindsConfig {
    type Error = String;

    /// `toml::Table` rather than `BTreeMap<String, String>`, so that a value which is not a
    /// string is reported against the combination that carried it. Serde's own message for a
    /// map with a bad value names neither.
    fn try_from(table: toml::Table) -> Result<Self, Self::Error> {
        let mut binds = Vec::with_capacity(table.len());
        for (combination, action) in table {
            let combo: keybinds::Combo = combination.parse()?;
            let action = action.as_str().ok_or_else(|| {
                format!("the binding for {combo} must be an action name, in quotes")
            })?;
            // The combination is named again here because the parse error talks about the
            // action alone, and "\"nonsense\" is not an action" in a file with thirty
            // bindings is not a useful thing to be told.
            let action = keybinds::parse_binding(action)
                .map_err(|err| format!("the binding for {combo}: {err}"))?;
            binds.push((combo, action));
        }
        Ok(Self(binds))
    }
}

// Hand-written rather than `#[serde(try_from)]` on the field, because `toml::Table` is
// already what the parser has in hand and going through serde's map visitor would only turn
// it back into one.
impl<'de> Deserialize<'de> for KeybindsConfig {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let table = toml::Table::deserialize(deserializer)?;
        Self::try_from(table).map_err(serde::de::Error::custom)
    }
}

/// Window-management behavior that is not about focus.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WindowsConfig {
    /// Whether a window is dragged **as itself**, redrawing its contents at each new position.
    ///
    /// `false` gives IRIX's other mode: the window stays where it is and a red wireframe of
    /// where its frame would land follows the pointer, with the move applied on release. IRIX
    /// offered the choice because opaque dragging of a large window was ruinous on the hardware
    /// of the day; it is kept because it is part of the desktop's feel, not because it is
    /// needed for speed.
    #[serde(default = "yes")]
    pub opaque_move: bool,
    /// The same for resizing: `false` rubber-bands the frame and configures the client once, on
    /// release, rather than on every motion event.
    #[serde(default = "yes")]
    pub opaque_resize: bool,
}

/// `serde`'s `default` wants a function, and both flags default to on -- which is what the
/// compositor did before there was anything to set.
fn yes() -> bool {
    true
}

impl Default for WindowsConfig {
    fn default() -> Self {
        Self {
            opaque_move: true,
            opaque_resize: true,
        }
    }
}

/// The pointer theme, and the size it is asked for at.
///
/// Both fields are optional and both fall back to the XCursor environment variables before the
/// built-in defaults, in that order: a value here, then `XCURSOR_THEME`/`XCURSOR_SIZE`, then
/// `default` at 24. The environment comes second rather than first because this file is the
/// desktop's own answer -- a session that ships a theme should get it -- but running the
/// compositor nested inside another desktop, where the host has already exported a theme, should
/// still pick that up rather than a name that may not be installed there.
///
/// The compositor is the only thing that decides this. What it settles on is reported to
/// `wlrix-session` over the handshake as `XCURSOR_THEME`/`XCURSOR_SIZE`, which puts it in the
/// environment of every app the session starts, so a GTK window's own pointer matches the one
/// drawn over the desktop instead of being whatever that toolkit's default is.
#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CursorConfig {
    /// An XCursor theme name -- a directory under `share/icons` on an XDG data directory, or
    /// under `~/.icons`. wlRIX installs `sgi`, the IRIX pointer set, and its system default
    /// config names it. A theme that is not installed is reported and the built-in arrow drawn.
    pub theme: Option<String>,
    /// The nominal size, in logical pixels, of the images to pick out of the theme.
    ///
    /// Nominal, not literal: a theme carries whichever sizes its author drew, and the nearest
    /// is used. `sgi` has 32 and nothing else, which is why the default config asks for 32 --
    /// asking for 24 would get the same images and then tell every client to resample them.
    pub size: Option<u32>,
}

impl CursorConfig {
    /// The theme name to load: this file, then `XCURSOR_THEME`, then `default`.
    pub fn theme(&self) -> String {
        resolve_theme(
            self.theme.as_deref(),
            std::env::var("XCURSOR_THEME").ok().as_deref(),
        )
    }

    /// The size to ask the theme for: this file, then `XCURSOR_SIZE`, then 24.
    pub fn size(&self) -> u32 {
        resolve_size(self.size, std::env::var("XCURSOR_SIZE").ok().as_deref())
    }
}

/// The two sources in order, with the environment passed in rather than read here so the
/// precedence can be tested without a test mutating the process's environment out from under
/// every other test in the binary.
///
/// An empty string is treated as absent, the same way `[keyboard] rules = ""` means "let the
/// system decide": a settings panel that clears a text field writes `""`, and that should mean
/// what leaving the key out means rather than a theme whose name is nothing.
fn resolve_theme(configured: Option<&str>, environment: Option<&str>) -> String {
    let named = |name: &&str| !name.trim().is_empty();
    configured
        .filter(named)
        .or(environment.filter(named))
        .unwrap_or(DEFAULT_CURSOR_THEME)
        .trim()
        .to_string()
}

/// The same for the size.
///
/// Zero is not a size and a `XCURSOR_SIZE` that will not parse is not one either; both fall
/// through to the next source rather than being used or clamped, because a theme asked for size
/// 0 matches its smallest image and the pointer silently becomes a speck.
fn resolve_size(configured: Option<u32>, environment: Option<&str>) -> u32 {
    let usable = |size: &u32| *size > 0;
    configured
        .filter(usable)
        .or_else(|| {
            environment
                .and_then(|value| value.trim().parse().ok())
                .filter(usable)
        })
        .unwrap_or(DEFAULT_CURSOR_SIZE)
}

/// How keyboard focus is handed out.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FocusConfig {
    #[serde(default)]
    pub policy: FocusPolicy,
    /// Whether clicking a window's *client area* also raises it. Default `true`.
    ///
    /// Turning it off separates focusing from restacking: a click still gives the window the
    /// keyboard, but the stacking order only changes deliberately -- by clicking the window's
    /// 4Dwm frame, or through Raise/Lower in its window menu. That suits working with windows
    /// deliberately overlapped, where an accidental click in a text field should not reshuffle
    /// what is on top of what.
    ///
    /// Frame clicks always raise regardless, since a click on a titlebar is unambiguous about
    /// wanting the window; middle-drag is the exception, because moving a window is not the
    /// same as asking for it to come forward.
    #[serde(default = "yes")]
    pub raise_on_click: bool,
}

impl Default for FocusConfig {
    /// Hand-written rather than derived: `raise_on_click` defaults to *true*, and a derived
    /// `Default` would quietly make it false for anyone whose config has no `[focus]` section
    /// at all -- a different default depending on whether the section is present.
    fn default() -> Self {
        Self {
            policy: FocusPolicy::default(),
            raise_on_click: true,
        }
    }
}

/// Which window has the keyboard.
///
/// Motif calls these `explicit` and `pointer`, and IRIX's 4Dwm inherited both; wlRIX says
/// `click` for the first because that is what everyone else calls it now.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FocusPolicy {
    /// Click a window to focus it, and it is raised at the same time. The modern default.
    #[default]
    Click,
    /// The window under the pointer has the keyboard, decorations included. Clicking still
    /// focuses, and raises unless `raise_on_click` is off, so a buried window can still be
    /// brought to the front.
    Pointer,
}

/// Idle behavior the compositor applies itself, as opposed to what a client such as
/// `wlrix-idle` asks for over `ext-idle-notify`.
///
/// **Deprecated.** `wlrix-idle` owns idle policy for a wlRIX session, and it is started as
/// part of the default session, so on an ordinary install this section is not needed and
/// should be left out. It is still parsed so that an existing config does not break.
///
/// The reason it lost the job is that a timer inside the compositor can only ever see what
/// the compositor sees. It cannot notice a controller -- libinput classifies a gamepad as a
/// joystick and drops it -- it cannot serve `org.freedesktop.ScreenSaver`, so an application
/// playing a film has no way to say "not now", and it cannot take a logind delay inhibitor
/// to lock before the machine suspends. All three want a session process, not a compositor.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IdleConfig {
    /// Switch the monitors off after this many seconds without input. Absent or `0` never
    /// blanks. An idle inhibitor (a video player, say) holds it off, and any input switches
    /// the screens back on.
    ///
    /// Only the udev backend can really switch a connector off; nested, this is tracked but
    /// nothing happens on screen.
    ///
    /// Do not set this while `wlrix-idle` is running -- see [`IdleConfig`]. Two timers on one
    /// screen fail in a way that is hard to diagnose: a blank a client asked for is left
    /// alone by input, so once this one has fired against `wlrix-idle`'s back, nothing
    /// switches the screens on again.
    pub blank_after_secs: Option<u64>,
}

/// The keyboard's xkb keymap and key-repeat timing.
///
/// Every field is optional; an absent `[keyboard]` section, or an absent field within
/// it, reproduces exactly what the compositor did before it read any config -- the
/// system default keymap (US on most installs) and smithay's repeat timing.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KeyboardConfig {
    /// The xkb rules file. Empty picks the system default.
    pub rules: Option<String>,
    /// The keyboard model, e.g. `jp106` for a Japanese 106-key board.
    pub model: Option<String>,
    /// A comma-separated list of layouts, e.g. `jp` or `jp,us`. More than one enables
    /// the layout-cycle key.
    pub layout: Option<String>,
    /// A comma-separated list of variants, one per layout.
    pub variant: Option<String>,
    /// xkb options, e.g. `grp:alt_shift_toggle` for an xkb-internal layout switch.
    pub options: Option<String>,
    /// Milliseconds a key is held before it starts repeating.
    pub repeat_delay: Option<i32>,
    /// Repeats per second once repetition starts.
    pub repeat_rate: Option<i32>,
}

impl KeyboardConfig {
    /// Borrow the owned strings into an [`XkbConfig`] for `add_keyboard`/`set_xkb_config`.
    ///
    /// The borrow lasts only as long as the returned value: both smithay entry points
    /// compile the keymap during the call and keep none of the `&str`s afterward.
    pub fn xkb(&self) -> XkbConfig<'_> {
        XkbConfig {
            rules: self.rules.as_deref().unwrap_or(""),
            model: self.model.as_deref().unwrap_or(""),
            layout: self.layout.as_deref().unwrap_or(""),
            variant: self.variant.as_deref().unwrap_or(""),
            options: self.options.clone(),
        }
    }

    pub fn delay(&self) -> i32 {
        self.repeat_delay.unwrap_or(DEFAULT_REPEAT_DELAY)
    }

    pub fn rate(&self) -> i32 {
        self.repeat_rate.unwrap_or(DEFAULT_REPEAT_RATE)
    }
}

/// A config file, and where it came from.
pub struct Loaded {
    pub config: Config,
    pub source: Source,
}

/// Where the compositor's settings came from.
///
/// "No file" and "a file we could not use" both end in the defaults, but they are not
/// the same thing to whoever is reading the log: one is the ordinary case on a fresh
/// install, the other means a file they wrote is being ignored.
pub enum Source {
    /// No config file anywhere.
    None,
    /// Read and used.
    File(PathBuf),
    /// Found, but unusable. The reason has already been reported.
    Rejected(PathBuf),
}

impl Source {
    /// Log where the config came from. A rejected file is a warning -- a file the user
    /// wrote is being ignored, and the only sign is this line.
    pub fn report(&self) {
        match self {
            Source::File(path) => tracing::info!(path = %path.display(), "loaded config"),
            Source::Rejected(path) => {
                warn!(path = %path.display(), "config rejected; using built-in defaults")
            }
            Source::None => {}
        }
    }
}

/// Read the compositor config.
///
/// A broken config is reported and then ignored rather than being fatal. The compositor
/// is often started with nothing watching its output, and refusing to start leaves a
/// black screen and no way in to fix the typo; coming up with defaults at least gives a
/// desktop to repair it from.
pub fn load() -> Loaded {
    let Some(path) = find() else {
        return Loaded {
            config: Config::default(),
            source: Source::None,
        };
    };

    let rejected = |path: PathBuf| Loaded {
        config: Config::default(),
        source: Source::Rejected(path),
    };

    match std::fs::read_to_string(&path) {
        Ok(text) => match toml::from_str::<Config>(&text) {
            Ok(config) => Loaded {
                config,
                source: Source::File(path),
            },
            Err(err) => {
                warn!("{} is not valid: {err}", path.display());
                rejected(path)
            }
        },
        Err(err) => {
            warn!("could not read {}: {err}", path.display());
            rejected(path)
        }
    }
}

/// Parse a candidate config file, for `--check-config`.
///
/// The point is that this program's own serde types are the authority on what its config file
/// may contain. `wlrix-settings-daemon` writes a temporary file and runs this against it before
/// renaming it into place, so a settings app cannot produce a config the compositor would
/// refuse -- which matters more here than it looks like it should, because
/// `deny_unknown_fields` means one wrong key costs the *whole* file and the user silently gets
/// built-in defaults.
///
/// Returns the parser's own message, which says which line and which key, and is far more
/// useful than anything this could say about it.
pub fn check(path: &Path) -> Result<(), String> {
    let text = std::fs::read_to_string(path)
        .map_err(|err| format!("could not read {}: {err}", path.display()))?;
    let config: Config = toml::from_str(&text).map_err(|err| err.to_string())?;

    // Warn, not fail. A scheme name this build does not ship parses perfectly well and the
    // compositor starts on the default -- so refusing the file would be wrong. But the whole
    // point of `--check-config` is that a settings panel runs it before writing, and "the
    // file is valid and the setting does nothing" is the kind of quiet failure worth a line.
    let (_, unknown) = wlrix_ui::palette::resolve(config.appearance.palette.as_deref());
    if let Some(why) = unknown {
        eprintln!("{}: {why}", path.display());
    }
    Ok(())
}

/// The first config file that exists: the user's, then the system's.
fn find() -> Option<PathBuf> {
    config_dirs()
        .into_iter()
        .map(|dir| dir.join(CONFIG_NAME))
        .find(|path| path.is_file())
}

/// Directories to look in, most specific first.
fn config_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(dir) = user_config_dir() {
        dirs.push(dir);
    }
    dirs.push(Path::new(SYSTEM_CONFIG_DIR).to_path_buf());
    dirs
}

/// `$XDG_CONFIG_HOME`, or `~/.config` as the spec says to assume.
fn user_config_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("XDG_CONFIG_HOME")
        && !dir.is_empty()
    {
        return Some(PathBuf::from(dir));
    }
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The three ways `[focus]` can appear, all of which have to agree on the default.
    ///
    /// `FocusConfig::default` is written out by hand because `raise_on_click` defaults to true;
    /// deriving it would give false, and only for configs with no `[focus]` section -- so the
    /// behavior would depend on whether an empty section happened to be present. This is that
    /// trap, nailed down.
    #[test]
    fn raising_on_click_defaults_on_however_focus_is_configured() {
        let absent: Config = toml::from_str("").unwrap();
        assert!(absent.focus.raise_on_click);

        let empty: Config = toml::from_str("[focus]").unwrap();
        assert!(empty.focus.raise_on_click);

        let other_field: Config = toml::from_str("[focus]\npolicy = \"pointer\"").unwrap();
        assert!(other_field.focus.raise_on_click);
        assert_eq!(other_field.focus.policy, FocusPolicy::Pointer);

        let off: Config = toml::from_str("[focus]\nraise_on_click = false").unwrap();
        assert!(!off.focus.raise_on_click);
    }

    #[test]
    fn empty_config_is_all_defaults() {
        let config: Config = toml::from_str("").unwrap();
        assert!(config.keyboard.layout.is_none());
        assert_eq!(config.keyboard.delay(), DEFAULT_REPEAT_DELAY);
        assert_eq!(config.keyboard.rate(), DEFAULT_REPEAT_RATE);
        // An all-empty xkb config is what `Default` gives smithay.
        let xkb = config.keyboard.xkb();
        assert_eq!(xkb.layout, "");
        assert_eq!(xkb.model, "");
    }

    #[test]
    fn keyboard_section_is_read() {
        let config: Config = toml::from_str(
            r#"
            [keyboard]
            layout = "jp,us"
            model = "jp106"
            options = "grp:alt_shift_toggle"
            repeat_delay = 300
            repeat_rate = 40
            "#,
        )
        .unwrap();
        let kb = &config.keyboard;
        assert_eq!(kb.layout.as_deref(), Some("jp,us"));
        assert_eq!(kb.model.as_deref(), Some("jp106"));
        assert_eq!(kb.delay(), 300);
        assert_eq!(kb.rate(), 40);
        assert_eq!(kb.xkb().options.as_deref(), Some("grp:alt_shift_toggle"));
    }

    #[test]
    fn focus_is_click_until_it_is_asked_not_to_be() {
        // The whole point of the default: an existing config, or none at all, keeps behaving
        // exactly as it did before there was a policy to set.
        let config: Config = toml::from_str("").unwrap();
        assert_eq!(config.focus.policy, FocusPolicy::Click);
        let config: Config = toml::from_str("[focus]").unwrap();
        assert_eq!(config.focus.policy, FocusPolicy::Click);
    }

    #[test]
    fn focus_policy_is_read() {
        let config: Config = toml::from_str("[focus]\npolicy = \"pointer\"").unwrap();
        assert_eq!(config.focus.policy, FocusPolicy::Pointer);
        let config: Config = toml::from_str("[focus]\npolicy = \"click\"").unwrap();
        assert_eq!(config.focus.policy, FocusPolicy::Click);
    }

    #[test]
    fn an_unknown_focus_policy_is_rejected() {
        // Not silently treated as click: a settings app writing `explicit` (Motif's name for
        // the same thing) should be told, not ignored.
        assert!(toml::from_str::<Config>("[focus]\npolicy = \"explicit\"").is_err());
        assert!(toml::from_str::<Config>("[focus]\npolicy = \"Pointer\"").is_err());
        assert!(toml::from_str::<Config>("[focus]\npolicey = \"pointer\"").is_err());
    }

    #[test]
    fn moves_and_resizes_are_opaque_until_they_are_asked_not_to_be() {
        // The default has to reproduce what the compositor did before there was a setting.
        for text in ["", "[windows]", "[windows]\nopaque_move = true"] {
            let config: Config = toml::from_str(text).unwrap();
            assert!(config.windows.opaque_move, "{text:?}");
            assert!(config.windows.opaque_resize, "{text:?}");
        }
        assert!(WindowsConfig::default().opaque_move);
        assert!(WindowsConfig::default().opaque_resize);
    }

    #[test]
    fn the_two_opaque_settings_are_independent() {
        // IRIX let them be set separately, and they are: opaque moves are cheap, opaque
        // resizes make a client re-lay-out on every motion event.
        let config: Config = toml::from_str("[windows]\nopaque_resize = false").unwrap();
        assert!(config.windows.opaque_move);
        assert!(!config.windows.opaque_resize);

        let config: Config = toml::from_str("[windows]\nopaque_move = false").unwrap();
        assert!(!config.windows.opaque_move);
        assert!(config.windows.opaque_resize);
    }

    #[test]
    fn a_typo_in_the_windows_section_is_rejected() {
        assert!(toml::from_str::<Config>("[windows]\nopaque_moove = false").is_err());
        assert!(toml::from_str::<Config>("[windows]\nopaque_move = \"no\"").is_err());
    }

    #[test]
    fn cursor_section_is_read() {
        let config: Config = toml::from_str("[cursor]\ntheme = \"sgi\"\nsize = 32").unwrap();
        assert_eq!(config.cursor.theme(), "sgi");
        assert_eq!(config.cursor.size(), 32);
    }

    /// What the installed `/etc/wlrix/compositor.toml` says, parsed by the types that read it.
    ///
    /// The file is data in another directory, and nothing else would notice it drifting from
    /// these structs -- a rejected system default is the whole desktop falling back to built-in
    /// values, silently, on every machine that has not written its own config.
    #[test]
    fn the_installed_default_config_is_accepted() {
        let text = include_str!("../data/compositor.toml");
        let config: Config = toml::from_str(text).expect("data/compositor.toml must parse");
        assert_eq!(config.cursor.theme(), "sgi");
        assert_eq!(
            config.cursor.size(),
            32,
            "the sgi theme only carries 32x32 images"
        );
    }

    /// The config file first, then the XCursor environment, then the built-in default -- and
    /// "empty" counts as absent at every step, since that is what a settings panel writes when
    /// its text field is cleared.
    #[test]
    fn the_cursor_theme_prefers_the_config_then_the_environment() {
        assert_eq!(resolve_theme(Some("sgi"), Some("Adwaita")), "sgi");
        assert_eq!(resolve_theme(None, Some("Adwaita")), "Adwaita");
        assert_eq!(resolve_theme(None, None), DEFAULT_CURSOR_THEME);
        assert_eq!(resolve_theme(Some(""), Some("Adwaita")), "Adwaita");
        assert_eq!(resolve_theme(Some(" "), None), DEFAULT_CURSOR_THEME);
        assert_eq!(resolve_theme(Some(" sgi "), None), "sgi");
    }

    /// The same order for the size, plus the two values that are not sizes: zero, which would
    /// match a theme's smallest image and leave the pointer a speck, and an `XCURSOR_SIZE` that
    /// is not a number at all.
    #[test]
    fn the_cursor_size_prefers_the_config_then_the_environment() {
        assert_eq!(resolve_size(Some(32), Some("24")), 32);
        assert_eq!(resolve_size(None, Some("48")), 48);
        assert_eq!(resolve_size(None, None), DEFAULT_CURSOR_SIZE);
        assert_eq!(resolve_size(Some(0), Some("48")), 48);
        assert_eq!(resolve_size(None, Some("0")), DEFAULT_CURSOR_SIZE);
        assert_eq!(resolve_size(None, Some("big")), DEFAULT_CURSOR_SIZE);
    }

    #[test]
    fn a_typo_in_the_cursor_section_is_rejected() {
        assert!(toml::from_str::<Config>("[cursor]\nthem = \"sgi\"").is_err());
        assert!(toml::from_str::<Config>("[cursor]\nsize = \"32\"").is_err());
        // Negative sizes are not a `u32`, so serde refuses them before any of this is reached.
        assert!(toml::from_str::<Config>("[cursor]\nsize = -1").is_err());
    }

    #[test]
    fn keybinds_are_read() {
        let config: Config = toml::from_str(
            r#"
            [keybinds]
            "Ctrl+Alt+BackSpace" = "none"
            "Ctrl+Alt+Q" = "quit"
            "Super+4" = "switch-desk 4"
            "#,
        )
        .unwrap();
        let binds = &config.keybinds.0;
        assert_eq!(binds.len(), 3);
        // A map, so the order out is not the order in; look them up by combination.
        let find = |text: &str| {
            let wanted: crate::keybinds::Combo = text.parse().unwrap();
            binds
                .iter()
                .find(|(combo, _)| *combo == wanted)
                .map(|(_, action)| *action)
        };
        assert_eq!(find("Ctrl+Alt+BackSpace"), Some(None), "`none` unbinds");
        assert_eq!(
            find("Ctrl+Alt+q"),
            Some(Some(crate::keybinds::Action::Quit))
        );
        assert_eq!(
            find("Super+4"),
            Some(Some(crate::keybinds::Action::SwitchDesk(3)))
        );
    }

    /// Resolution happens during parsing rather than later, which is what lets
    /// `--check-config` refuse a bad binding -- and so what stops `wlrix-settings-daemon`
    /// writing one. A section carried around as strings would pass the check and then be
    /// half-ignored at runtime.
    #[test]
    fn a_bad_keybind_is_rejected_by_the_parser() {
        for text in [
            "[keybinds]\n\"Alt+Nonsense\" = \"close\"",
            "[keybinds]\n\"Alt\" = \"close\"",
            "[keybinds]\n\"Alt+F4\" = \"nonsense\"",
            "[keybinds]\n\"Alt+F4\" = \"switch-desk 0\"",
            "[keybinds]\n\"Alt+F4\" = 4",
            "[keybinds]\n\"Alt+F4\" = true",
        ] {
            assert!(
                toml::from_str::<Config>(text).is_err(),
                "{text:?} should not have parsed"
            );
        }
        // The message names the binding it is complaining about, since a file with thirty of
        // them otherwise leaves the user hunting.
        let err = toml::from_str::<Config>("[keybinds]\n\"Alt+F4\" = \"nonsense\"")
            .unwrap_err()
            .to_string();
        assert!(err.contains("Alt+F4"), "{err}");
        assert!(err.contains("nonsense"), "{err}");
    }

    #[test]
    fn no_keybinds_section_means_the_defaults_alone() {
        for text in ["", "[keybinds]"] {
            let config: Config = toml::from_str(text).unwrap();
            assert!(config.keybinds.0.is_empty(), "{text:?}");
            // Nothing configured, so resolving gives exactly the built-in table.
            let bindings = crate::keybinds::Bindings::resolve(&config.keybinds.0);
            assert_eq!(bindings.len(), crate::keybinds::DEFAULTS.len());
        }
    }

    #[test]
    fn unknown_key_is_rejected() {
        assert!(toml::from_str::<Config>("nonsense = true").is_err());
        assert!(
            toml::from_str::<Config>("[keyboard]\nlayuot = \"jp\"").is_err(),
            "a typo'd key must not be silently ignored"
        );
    }
}

/// Which color scheme to draw the chrome in.
///
/// Its own section rather than a bare key, because a scheme is not the only thing that will
/// ever go here -- and because `wlrix-desktop` names its section the same, so the two files
/// read alike.
///
/// Deliberately *not* a settings-daemon key yet. One scheme has to reach the compositor, the
/// desktop and the applications at once, and the daemon ties a key to a single owner to
/// signal; giving it more than one is its own change.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppearanceConfig {
    /// A scheme id from `wlrix-ui`: `classic`, `classic-g10`, `classic-g24`, `gotham`.
    #[serde(default)]
    pub palette: Option<String>,
}

/// The scheme `config` names, or the default if it names nothing this build ships.
///
/// Never fails, and deliberately: refusing to start over a misspelled scheme name would leave
/// somebody with no session at all, and the fallback is a perfectly usable desktop.
pub fn resolve_palette(config: &Config) -> &'static wlrix_ui::palette::Palette {
    let (palette, unknown) = wlrix_ui::palette::resolve(config.appearance.palette.as_deref());
    if let Some(why) = unknown {
        tracing::warn!("{why}; using {}", palette.id);
    }
    palette
}
