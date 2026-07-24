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
    fn unknown_key_is_rejected() {
        assert!(toml::from_str::<Config>("nonsense = true").is_err());
        assert!(
            toml::from_str::<Config>("[keyboard]\nlayuot = \"jp\"").is_err(),
            "a typo'd key must not be silently ignored"
        );
    }
}
