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
//! ```
//!
//! Read from the user's config directory first, then `/etc/wlrix`; the first file found
//! wins outright rather than merging, so what a user sees in their own file is the whole
//! of what they get. This mirrors `wlrix-session`'s config, deliberately: one shape of
//! file across the stack.
//!
//! Unknown keys are an error. A silently ignored typo in a config file is a bad
//! afternoon, and the cost of being strict is a clear message instead.

use std::path::{Path, PathBuf};

use serde::Deserialize;
use smithay::input::keyboard::XkbConfig;
use tracing::warn;

/// Where the config lives, relative to a config directory.
const CONFIG_NAME: &str = "wlrix/compositor.toml";
/// Consulted when the user has no config of their own.
const SYSTEM_CONFIG_DIR: &str = "/etc";

/// smithay's defaults when `add_keyboard` is handed `XkbConfig::default()`: an empty
/// field means "let libxkbcommon pick", which for delay/rate we spell out so a reload
/// and a fresh start agree.
const DEFAULT_REPEAT_DELAY: i32 = 200;
const DEFAULT_REPEAT_RATE: i32 = 25;

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// The keyboard layout, model and repeat behavior.
    #[serde(default)]
    pub keyboard: KeyboardConfig,
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

/// How keyboard focus is handed out.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FocusConfig {
    #[serde(default)]
    pub policy: FocusPolicy,
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
    /// focuses and raises, so a buried window can still be brought to the front.
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
    toml::from_str::<Config>(&text)
        .map(|_| ())
        .map_err(|err| err.to_string())
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
    fn unknown_key_is_rejected() {
        assert!(toml::from_str::<Config>("nonsense = true").is_err());
        assert!(
            toml::from_str::<Config>("[keyboard]\nlayuot = \"jp\"").is_err(),
            "a typo'd key must not be silently ignored"
        );
    }
}
