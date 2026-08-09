// SPDX-License-Identifier: GPL-3.0-or-later
//! Per-monitor display settings: the resolution, position, scale and orientation of
//! each output, and whether it is on.
//!
//! Two files hold these, in the same `[[output]]` shape:
//!
//! - **`compositor.toml`** (hand-edited) may carry an `[[output]]` block as a *default*
//!   for a monitor -- e.g. "when the KVM's `Virtual-1` shows up, use 1920x1080". This is
//!   parsed as part of [`crate::config::Config`] and is never rewritten.
//! - **`outputs.toml`** (machine-written, under `$XDG_STATE_HOME/wlrix/`) is what the
//!   compositor saves whenever the display changes through `wlr-output-management`. It is
//!   the running record of "how the screens are arranged right now".
//!
//! At startup the two are merged per field, the state file winning, so an auto-saved
//! position sits happily alongside a hand-set default mode. Anything neither file pins
//! falls back to the connector's preferred mode and the left-to-right auto-layout.
//!
//! An output is keyed by its connector name (`DP-1`, `Virtual-1`, ...), the only stable
//! identity available -- EDID make/model/serial are not read (see `Cargo.toml`).

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use smithay::{output::Scale, utils::Transform};
use tracing::warn;

/// Where the machine-written state lives, relative to the state directory.
const STATE_NAME: &str = "wlrix/outputs.toml";

/// One monitor's saved settings. Every field past `name` is optional: an absent field
/// means "no opinion", to be filled by the other file or the built-in default.
#[derive(Debug, Default, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OutputConfig {
    /// The connector name, e.g. `DP-1` or `Virtual-1`.
    pub name: String,
    /// Resolution and refresh as `WIDTHxHEIGHT@HZ`, or `WIDTHxHEIGHT` for the fastest
    /// mode at that size.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    /// Top-left corner in the global layout, `[x, y]`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub position: Option<[i32; 2]>,
    /// Output scale (1.0, 1.5, 2.0, ...).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scale: Option<f64>,
    /// Orientation: `normal`, `90`, `180`, `270`, `flipped`, `flipped-90`,
    /// `flipped-180`, `flipped-270`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transform: Option<String>,
    /// Whether the output is switched on. Absent is treated as on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    /// Whether adaptive sync (VRR) is on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adaptive_sync: Option<bool>,
    /// Whether the output is driven in HDR (PQ / BT.2020). Absent is treated as off.
    ///
    /// Only honored where the connector has the properties *and* the panel advertises ST2084;
    /// asking for it on a display that cannot do it is logged and ignored, not fatal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hdr: Option<bool>,
    /// Reference white for SDR content on an HDR output, in cd/m². Ignored unless `hdr` is on.
    ///
    /// Absent means [`crate::hdr::DEFAULT_SDR_WHITE_NITS`]. Raise it if the desktop looks dim
    /// next to an SDR screen, lower it if plain white burns.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sdr_white_nits: Option<f32>,
    /// Whether to alpha-composite in linear light on this output. Ignored unless `hdr` is on.
    ///
    /// Absent means on, which is the physically correct choice. Turn it off if antialiased text
    /// looks wrong: glyph coverage blended in linear light comes out thinner on a dark titlebar,
    /// because font rasterisers are tuned against sRGB-space blending.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub linear_blending: Option<bool>,
}

impl OutputConfig {
    /// Overlay another entry's set fields onto this one. Used to merge the state file
    /// (the overlay) over a hand-set default.
    fn overlay(&mut self, other: &OutputConfig) {
        if other.mode.is_some() {
            self.mode = other.mode.clone();
        }
        if other.position.is_some() {
            self.position = other.position;
        }
        if other.scale.is_some() {
            self.scale = other.scale;
        }
        if other.transform.is_some() {
            self.transform = other.transform.clone();
        }
        if other.enabled.is_some() {
            self.enabled = other.enabled;
        }
        if other.adaptive_sync.is_some() {
            self.adaptive_sync = other.adaptive_sync;
        }
        if other.hdr.is_some() {
            self.hdr = other.hdr;
        }
        if other.sdr_white_nits.is_some() {
            self.sdr_white_nits = other.sdr_white_nits;
        }
        if other.linear_blending.is_some() {
            self.linear_blending = other.linear_blending;
        }
    }

    /// The transform this entry names, if it names a valid one.
    pub fn transform(&self) -> Option<Transform> {
        self.transform.as_deref().and_then(parse_transform)
    }

    /// The scale this entry sets, as a smithay [`Scale`].
    pub fn scale(&self) -> Option<Scale> {
        self.scale.map(Scale::Fractional)
    }
}

/// The merged, per-connector display config the backend consults when an output appears.
pub type DisplayConfig = BTreeMap<String, OutputConfig>;

/// The `outputs.toml` file: a list of `[[output]]` tables.
#[derive(Debug, Default, Deserialize, Serialize)]
struct OutputsFile {
    #[serde(default, rename = "output")]
    outputs: Vec<OutputConfig>,
}

/// Build the startup display config: hand-set `compositor.toml` defaults with the
/// machine-written `outputs.toml` state overlaid on top, per field.
pub fn resolve(defaults: &[OutputConfig]) -> DisplayConfig {
    let mut map: DisplayConfig = BTreeMap::new();
    for entry in defaults {
        map.insert(entry.name.clone(), entry.clone());
    }
    for entry in load_state() {
        map.entry(entry.name.clone())
            .or_insert_with(|| OutputConfig {
                name: entry.name.clone(),
                ..Default::default()
            })
            .overlay(&entry);
    }
    map
}

/// Write the current display arrangement to `outputs.toml`.
///
/// Best-effort: a missing state directory, a serialization failure or an I/O error is
/// logged and swallowed. The file is replaced atomically -- written to a sibling temp
/// file and renamed -- so a crash mid-write cannot leave a half-written config that the
/// next start would reject.
pub fn save(entries: &[OutputConfig]) {
    let Some(path) = state_file() else {
        warn!("no state directory ($XDG_STATE_HOME / $HOME); not saving display state");
        return;
    };

    let file = OutputsFile {
        outputs: entries.to_vec(),
    };
    let text = match toml::to_string(&file) {
        Ok(text) => text,
        Err(err) => {
            warn!("could not serialize display state: {err}");
            return;
        }
    };

    if let Err(err) = write_replace(&path, text.as_bytes()) {
        warn!("could not write {}: {err}", path.display());
    }
}

/// Replace a file atomically: write a sibling temp file, then rename over the target,
/// creating the parent directory if need be.
fn write_replace(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)
}

/// Read `outputs.toml`. A missing or broken file yields nothing -- like the main config,
/// a bad state file is reported and ignored rather than being fatal.
fn load_state() -> Vec<OutputConfig> {
    let Some(path) = state_file() else {
        return Vec::new();
    };
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        // Not-found is the ordinary first-run case; only real errors are worth a line.
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(err) => {
            warn!("could not read {}: {err}", path.display());
            return Vec::new();
        }
    };
    match toml::from_str::<OutputsFile>(&text) {
        Ok(file) => file.outputs,
        Err(err) => {
            warn!("{} is not valid: {err}", path.display());
            Vec::new()
        }
    }
}

/// The path to `outputs.toml`, if a state directory can be found.
pub fn state_file() -> Option<PathBuf> {
    state_dir().map(|dir| dir.join(STATE_NAME))
}

/// `$XDG_STATE_HOME`, or `~/.local/state` as the spec says to assume.
fn state_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("XDG_STATE_HOME")
        && !dir.is_empty()
    {
        return Some(PathBuf::from(dir));
    }
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local").join("state"))
}

/// Parse `WIDTHxHEIGHT@HZ` or `WIDTHxHEIGHT` into `(width, height, refresh_hz)`, with the
/// refresh absent when the spec omits it.
pub fn parse_mode(spec: &str) -> Option<(i32, i32, Option<u32>)> {
    let (resolution, refresh) = match spec.split_once('@') {
        Some((resolution, hz)) => (resolution, Some(hz)),
        None => (spec, None),
    };
    let (width, height) = resolution.split_once('x')?;
    let width: i32 = width.trim().parse().ok()?;
    let height: i32 = height.trim().parse().ok()?;
    let refresh = match refresh {
        Some(hz) => Some(hz.trim().trim_end_matches("Hz").trim().parse().ok()?),
        None => None,
    };
    Some((width, height, refresh))
}

/// Format a mode for the state file. `refresh_mhz` is milli-Hz, as smithay carries it;
/// the file records whole Hz, which is what a person writes in a config.
pub fn format_mode(width: i32, height: i32, refresh_mhz: i32) -> String {
    let hz = (refresh_mhz as f64 / 1000.0).round() as i32;
    format!("{width}x{height}@{hz}")
}

/// The eight named orientations, parsed leniently (both `flipped-90` and `flipped90`).
pub fn parse_transform(name: &str) -> Option<Transform> {
    match name.trim().to_ascii_lowercase().replace('-', "").as_str() {
        "normal" | "0" => Some(Transform::Normal),
        "90" => Some(Transform::_90),
        "180" => Some(Transform::_180),
        "270" => Some(Transform::_270),
        "flipped" => Some(Transform::Flipped),
        "flipped90" => Some(Transform::Flipped90),
        "flipped180" => Some(Transform::Flipped180),
        "flipped270" => Some(Transform::Flipped270),
        _ => None,
    }
}

/// The name for a transform, for the state file. Inverse of [`parse_transform`].
pub fn format_transform(transform: Transform) -> &'static str {
    match transform {
        Transform::Normal => "normal",
        Transform::_90 => "90",
        Transform::_180 => "180",
        Transform::_270 => "270",
        Transform::Flipped => "flipped",
        Transform::Flipped90 => "flipped-90",
        Transform::Flipped180 => "flipped-180",
        Transform::Flipped270 => "flipped-270",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_round_trips() {
        assert_eq!(parse_mode("1920x1080@60"), Some((1920, 1080, Some(60))));
        assert_eq!(parse_mode("1280x800"), Some((1280, 800, None)));
        assert_eq!(parse_mode("1920x1080@59Hz"), Some((1920, 1080, Some(59))));
        assert_eq!(parse_mode("garbage"), None);
        assert_eq!(parse_mode("1920x"), None);
        // 59_951 mHz rounds to 60 Hz.
        assert_eq!(format_mode(1920, 1080, 59_951), "1920x1080@60");
    }

    #[test]
    fn transform_round_trips() {
        for name in [
            "normal",
            "90",
            "180",
            "270",
            "flipped",
            "flipped-90",
            "flipped-180",
            "flipped-270",
        ] {
            let parsed = parse_transform(name).expect("named transform parses");
            assert_eq!(format_transform(parsed), name);
        }
        assert_eq!(parse_transform("flipped90"), Some(Transform::Flipped90));
        assert_eq!(parse_transform("sideways"), None);
    }

    #[test]
    fn state_overlays_defaults_per_field() {
        let defaults = vec![OutputConfig {
            name: "DP-1".into(),
            mode: Some("2560x1440@144".into()),
            scale: Some(1.0),
            hdr: Some(true),
            sdr_white_nits: Some(180.0),
            ..Default::default()
        }];
        // The state file pins a position and re-scales, but says nothing about the mode or HDR.
        let state = OutputsFile {
            outputs: vec![OutputConfig {
                name: "DP-1".into(),
                position: Some([100, 0]),
                scale: Some(2.0),
                ..Default::default()
            }],
        };
        let mut map: DisplayConfig = BTreeMap::new();
        for entry in &defaults {
            map.insert(entry.name.clone(), entry.clone());
        }
        for entry in state.outputs {
            map.entry(entry.name.clone()).or_default().overlay(&entry);
        }
        let dp1 = &map["DP-1"];
        assert_eq!(dp1.mode.as_deref(), Some("2560x1440@144")); // kept from default
        assert_eq!(dp1.position, Some([100, 0])); // from state
        assert_eq!(dp1.scale, Some(2.0)); // state wins
        assert_eq!(dp1.hdr, Some(true)); // kept from default
        assert_eq!(dp1.sdr_white_nits, Some(180.0)); // kept from default
    }

    #[test]
    fn unknown_output_key_is_rejected() {
        assert!(toml::from_str::<OutputsFile>("[[output]]\nname = \"DP-1\"\nrez = \"x\"").is_err());
    }

    #[test]
    fn saved_state_reads_back_identically() {
        // What a write-back snapshot looks like: an enabled primary and a disabled
        // secondary. Serializing and reparsing must yield the same entries -- that is the
        // whole contract between `save` and startup `load_state`.
        let written = OutputsFile {
            outputs: vec![
                OutputConfig {
                    name: "DP-1".into(),
                    mode: Some("2560x1440@144".into()),
                    position: Some([0, 0]),
                    scale: Some(1.0),
                    transform: Some("normal".into()),
                    adaptive_sync: Some(true),
                    hdr: Some(true),
                    sdr_white_nits: Some(203.0),
                    ..Default::default()
                },
                OutputConfig {
                    name: "HDMI-A-1".into(),
                    mode: Some("1920x1080@60".into()),
                    position: Some([2560, 0]),
                    scale: Some(1.0),
                    transform: Some("90".into()),
                    enabled: Some(false),
                    ..Default::default()
                },
            ],
        };
        let text = toml::to_string(&written).expect("serializes");
        let read: OutputsFile = toml::from_str(&text).expect("reparses");
        assert_eq!(read.outputs, written.outputs);
    }

    #[test]
    fn absent_fields_stay_absent_through_serialization() {
        // An enabled output with no VRR and no HDR leaves those fields out, so a bare entry
        // keeps meaning "on, no adaptive sync, SDR" when read back. Listing every field
        // explicitly rather than using `..Default::default()` is deliberate: adding a field
        // without deciding what its absence means then fails to compile here.
        let entry = OutputConfig {
            name: "DP-1".into(),
            mode: Some("1280x800@60".into()),
            position: Some([0, 0]),
            scale: Some(1.0),
            transform: Some("normal".into()),
            enabled: None,
            adaptive_sync: None,
            hdr: None,
            sdr_white_nits: None,
            linear_blending: None,
        };
        let text = toml::to_string(&OutputsFile {
            outputs: vec![entry],
        })
        .unwrap();
        assert!(!text.contains("enabled"));
        assert!(!text.contains("adaptive_sync"));
        assert!(!text.contains("hdr"));
        assert!(!text.contains("sdr_white_nits"));
        assert!(!text.contains("linear_blending"));
    }
}
