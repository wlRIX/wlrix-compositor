// SPDX-License-Identifier: GPL-3.0-or-later
//! Night light: warming the picture after dark.
//!
//! The usual way to do this on Wayland is a client such as `wlsunset` loading a ramp into each
//! monitor's gamma table over `wlr-gamma-control` (see [`crate::gamma`]). That cannot work on an
//! HDR output, where the table would be applied to PQ code values, and there is no table at all
//! under the nested backend or on a virtual machine's display. So this is the compositor's own:
//! a per-channel gain, worked out here and multiplied in by the shaders that draw each output
//! (see [`crate::hdr_render`]), which is the same on every output there is.
//!
//! This module owns *what* the gain is and *when* -- the color math, the sun, the schedule and
//! the fades. It knows nothing about rendering; the backends ask [`NightLight::gains`] once per
//! frame and do the rest.
//!
//! Configured by `[night_light]` in `compositor.toml`; see
//! [`NightLightConfig`](crate::config::NightLightConfig).

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use smithay::reexports::calloop::{
    RegistrationToken,
    timer::{TimeoutAction, Timer},
};
use tracing::{info, warn};

use crate::Wlrix;
use crate::config::{NightLightConfig, NightLightMode};

/// The color temperature that is "no change": the white the desktop is drawn for.
const NEUTRAL_KELVIN: f64 = 6500.0;

/// The sun's elevation, in degrees, where the night light starts to fade in and where it is at
/// full strength. From a little before sunset to the end of civil twilight -- the band
/// `wlsunset` uses -- so the change follows the light outside rather than switching at a time.
const DAY_ELEVATION: f64 = 3.0;
const NIGHT_ELEVATION: f64 = -6.0;

/// How long a change of mode or temperature takes to cross. Long enough not to be a flash,
/// short enough that a click in the settings panel visibly did something.
const FADE: Duration = Duration::from_millis(1500);
/// How often the picture is redrawn during a fade.
const FADE_STEP: Duration = Duration::from_millis(33);
/// How long a newly chosen temperature is shown at full strength, so the slider in the settings
/// panel can be judged in daylight, when the schedule would otherwise show nothing.
const PREVIEW: Duration = Duration::from_secs(3);
/// How often the sun is looked at: through twilight, when it matters, and the rest of the time.
const TWILIGHT_TICK: Duration = Duration::from_secs(10);
const IDLE_TICK: Duration = Duration::from_secs(60);

/// A change smaller than this, on every channel, is not worth a frame.
const EPSILON: f32 = 1.0 / 1024.0;
/// A change smaller than this is applied at once instead of faded: the steps of twilight are
/// already a slow fade of their own, and fading each one would redraw the screen continuously
/// for no visible difference.
const SNAP: f32 = 0.02;

const IDENTITY: [f32; 3] = [1.0, 1.0, 1.0];

/// A blackbody's white point, as CIE 1931 xy.
///
/// Krystek's rational approximation of the Planckian locus in CIE 1960 UCS (1985), good to well
/// under what the eye can tell from 1000 K to 15000 K -- one formula for the whole range a
/// night light uses, where the commoner cubic fits need a piece per range.
///
/// In `f64` only so the published coefficients can be written down as published.
fn planckian_xy(kelvin: f64) -> (f64, f64) {
    let t = kelvin;
    let u = (0.860_117_757 + 1.541_182_54e-4 * t + 1.286_412_12e-7 * t * t)
        / (1.0 + 8.424_202_35e-4 * t + 7.081_451_63e-7 * t * t);
    let v = (0.317_398_726 + 4.228_062_45e-5 * t + 4.204_816_91e-8 * t * t)
        / (1.0 - 2.897_418_16e-5 * t + 1.614_560_53e-7 * t * t);
    let denominator = 2.0 * u - 8.0 * v + 4.0;
    (3.0 * u / denominator, 2.0 * v / denominator)
}

/// A blackbody's color in linear BT.709, at unit luminance, clamped into gamut.
fn planckian_rgb(kelvin: f64) -> [f64; 3] {
    let (x, y) = planckian_xy(kelvin);
    let (big_x, big_y, big_z) = (x / y, 1.0, (1.0 - x - y) / y);
    [
        3.240_454_2 * big_x - 1.537_138_5 * big_y - 0.498_531_4 * big_z,
        -0.969_266_0 * big_x + 1.876_010_8 * big_y + 0.041_556_0 * big_z,
        0.055_643_4 * big_x - 0.204_025_9 * big_y + 1.057_225_2 * big_z,
    ]
    .map(|channel| channel.max(0.0))
}

/// The per-channel gains, in linear light, that turn the desktop's white into `kelvin`'s.
///
/// Relative to 6500 K, so that temperature is exactly no change, and scaled so the largest gain
/// is 1: a night light only ever takes light away.
pub fn gains(kelvin: f32) -> [f32; 3] {
    let kelvin = f64::from(kelvin).clamp(1000.0, NEUTRAL_KELVIN);
    let neutral = planckian_rgb(NEUTRAL_KELVIN);
    let warm = planckian_rgb(kelvin);
    let relative: [f64; 3] = std::array::from_fn(|i| warm[i] / neutral[i]);
    let peak = relative.into_iter().fold(f64::MIN_POSITIVE, f64::max);
    relative.map(|channel| (channel / peak).clamp(0.0, 1.0) as f32)
}

/// The sun's elevation above the horizon, in degrees, at a moment and a place.
///
/// The NOAA Solar Calculator's formulas (after Meeus), without atmospheric refraction: good to
/// a small fraction of a degree, which is seconds of error on a fade that takes most of an hour.
/// `latitude` is degrees north and `longitude` degrees east.
pub fn solar_elevation(unix_secs: f64, latitude: f64, longitude: f64) -> f64 {
    // Julian centuries since J2000.0.
    let t = (unix_secs / 86_400.0 + 2_440_587.5 - 2_451_545.0) / 36_525.0;

    let mean_longitude = (280.466_46 + t * (36_000.769_83 + 0.000_303_2 * t)).rem_euclid(360.0);
    let mean_anomaly = (357.529_11 + t * (35_999.050_29 - 0.000_153_7 * t)).to_radians();
    let eccentricity = 0.016_708_634 - t * (0.000_042_037 + 0.000_000_126_7 * t);
    let center = mean_anomaly.sin() * (1.914_602 - t * (0.004_817 + 0.000_014 * t))
        + (2.0 * mean_anomaly).sin() * (0.019_993 - 0.000_101 * t)
        + (3.0 * mean_anomaly).sin() * 0.000_289;
    let omega = (125.04 - 1_934.136 * t).to_radians();
    let apparent_longitude =
        (mean_longitude + center - 0.005_69 - 0.004_78 * omega.sin()).to_radians();
    let mean_obliquity =
        23.0 + (26.0 + (21.448 - t * (46.815 + t * (0.000_59 - 0.001_813 * t))) / 60.0) / 60.0;
    let obliquity = (mean_obliquity + 0.002_56 * omega.cos()).to_radians();
    let declination = (obliquity.sin() * apparent_longitude.sin()).asin();

    // The equation of time, in minutes: how far the real sun runs ahead of a clock.
    let y = (obliquity / 2.0).tan().powi(2);
    let l0 = mean_longitude.to_radians();
    let equation_of_time = 4.0
        * (y * (2.0 * l0).sin() - 2.0 * eccentricity * mean_anomaly.sin()
            + 4.0 * eccentricity * y * mean_anomaly.sin() * (2.0 * l0).cos()
            - 0.5 * y * y * (4.0 * l0).sin()
            - 1.25 * eccentricity * eccentricity * (2.0 * mean_anomaly).sin())
        .to_degrees();

    let utc_minutes = unix_secs.rem_euclid(86_400.0) / 60.0;
    let solar_minutes = (utc_minutes + equation_of_time + 4.0 * longitude).rem_euclid(1_440.0);
    let hour_angle = (solar_minutes / 4.0 - 180.0).to_radians();

    let latitude = latitude.to_radians();
    let cos_zenith =
        latitude.sin() * declination.sin() + latitude.cos() * declination.cos() * hour_angle.cos();
    90.0 - cos_zenith.clamp(-1.0, 1.0).acos().to_degrees()
}

/// How much of the night light applies with the sun at `elevation`: 0 by day, 1 at night, and a
/// straight ramp through twilight between them.
///
/// Working from the elevation rather than from sunset and sunrise *times* is what makes the
/// polar cases fall out on their own: a sun that never sets never gets below the day threshold,
/// one that never rises never gets above the night one, and a summer night that only reaches
/// twilight gives a partial warming -- which is what the light outside is doing.
pub fn strength(elevation: f64) -> f32 {
    ((DAY_ELEVATION - elevation) / (DAY_ELEVATION - NIGHT_ELEVATION)).clamp(0.0, 1.0) as f32
}

/// The time zone's name, such as `America/New_York`, from the places a system keeps it.
///
/// The inputs are parameters so this can be tested without a machine configured a certain way:
/// the `TZ` environment variable, what `/etc/localtime` links to, and the text of
/// `/etc/timezone`, in the order libc itself prefers them.
pub fn zone_id(
    tz_env: Option<&str>,
    localtime_link: Option<&str>,
    etc_timezone: Option<&str>,
) -> Option<String> {
    // `TZ=:Europe/Paris` and `TZ=/usr/share/zoneinfo/Europe/Paris` both name a tzdata file. A
    // POSIX rule such as `EST5EDT` names no place, and falls through to the next source.
    let from_env = tz_env
        .map(|tz| tz.trim().trim_start_matches(':'))
        .map(|tz| tz.split_once("zoneinfo/").map_or(tz, |(_, id)| id))
        .filter(|tz| tz.contains('/') && !tz.starts_with('/'));
    let from_link = localtime_link.and_then(|link| link.split_once("zoneinfo/").map(|(_, id)| id));
    let from_file = etc_timezone.map(str::trim).filter(|id| !id.is_empty());

    from_env
        .or(from_link)
        // tzdata ships each zone a second time under `posix/` and `right/`.
        .map(|id| id.trim_start_matches("posix/").trim_start_matches("right/"))
        .or(from_file)
        .filter(|id| !id.is_empty())
        .map(str::to_owned)
}

/// Coordinates as tzdata writes them: ISO 6709, `±DDMM±DDDMM` or `±DDMMSS±DDDMMSS`.
///
/// Returns degrees north and degrees east.
pub fn parse_iso6709(text: &str) -> Option<(f64, f64)> {
    // The longitude starts at the second sign.
    let split = text
        .char_indices()
        .skip(1)
        .find(|(_, c)| matches!(c, '+' | '-'))
        .map(|(index, _)| index)?;
    let (latitude, longitude) = text.split_at(split);

    // One signed field: `degree_digits` of degrees, then minutes, then optional seconds.
    let field = |part: &str, degree_digits: usize| -> Option<f64> {
        let sign = match part.chars().next()? {
            '+' => 1.0,
            '-' => -1.0,
            _ => return None,
        };
        let digits = &part[1..];
        if !digits.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        let number = |range: std::ops::Range<usize>| digits.get(range)?.parse::<f64>().ok();
        let degrees = number(0..degree_digits)?;
        let minutes = number(degree_digits..degree_digits + 2)?;
        let seconds = match digits.len() - degree_digits {
            2 => 0.0,
            4 => number(degree_digits + 2..degree_digits + 4)?,
            _ => return None,
        };
        Some(sign * (degrees + minutes / 60.0 + seconds / 3600.0))
    };

    Some((field(latitude, 2)?, field(longitude, 3)?))
}

/// The coordinates tzdata lists for the zone `id`, from the text of `zone.tab` or
/// `zone1970.tab`. Both put the coordinates in the second tab-separated column and the zone in
/// the third.
pub fn lookup(tab_text: &str, id: &str) -> Option<(f64, f64)> {
    tab_text
        .lines()
        .filter(|line| !line.starts_with('#'))
        .find_map(|line| {
            let mut columns = line.split('\t');
            let coordinates = columns.nth(1)?;
            (columns.next()? == id).then(|| parse_iso6709(coordinates))?
        })
}

/// Where tzdata keeps its zone tables. `zone.tab` first: it lists every zone, where
/// `zone1970.tab` has merged away those that have agreed with a neighbor since 1970 --
/// `Europe/Amsterdam`, for one -- and a merged zone would otherwise find nothing.
const ZONE_TABLES: [&str; 2] = [
    "/usr/share/zoneinfo/zone.tab",
    "/usr/share/zoneinfo/zone1970.tab",
];

/// The system time zone's name, as this process sees it now.
fn system_zone_id() -> Option<String> {
    let tz = std::env::var("TZ").ok();
    let link = std::fs::read_link("/etc/localtime").ok();
    let file = std::fs::read_to_string("/etc/timezone").ok();
    zone_id(
        tz.as_deref(),
        link.as_deref().and_then(|path| path.to_str()),
        file.as_deref(),
    )
}

/// A stand-in location for a zone tzdata gives no coordinates for: on the equator, at the
/// longitude the UTC offset implies. That makes night roughly 18:00 to 06:00 by the wall clock,
/// which is what somebody with `UTC` or a bare POSIX rule for a time zone would guess anyway.
fn location_from_utc_offset() -> (f64, f64) {
    // SAFETY: `localtime_r` writes only into the `tm` it is handed, and both pointers are to
    // live locals. A null return leaves `tm` zeroed, which reads as an offset of 0.
    let offset_secs = unsafe {
        let now = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&now, &mut tm);
        tm.tm_gmtoff
    };
    (
        0.0,
        (offset_secs as f64 / 3600.0 * 15.0).clamp(-180.0, 180.0),
    )
}

/// A fade in progress, from one set of gains to another.
#[derive(Debug, Clone, Copy)]
struct Fade {
    from: [f32; 3],
    to: [f32; 3],
    start: Instant,
}

/// The night light's running state. The configuration itself stays in `Wlrix::config`.
#[derive(Debug)]
pub struct NightLight {
    /// The gains on screen now, in linear light.
    current: [f32; 3],
    fade: Option<Fade>,
    /// While set, the configured temperature is shown at full strength whatever the schedule
    /// says: the settings panel's slider being tried out.
    preview_until: Option<Instant>,
    timer: Option<RegistrationToken>,
    /// The last zone looked up and what it resolved to, so the zone table is read once per
    /// time zone rather than once per tick.
    zone: Option<(Option<String>, (f64, f64))>,
}

impl Default for NightLight {
    fn default() -> Self {
        Self {
            current: IDENTITY,
            fade: None,
            preview_until: None,
            timer: None,
            zone: None,
        }
    }
}

/// Whether two sets of gains differ by at least `threshold` on some channel.
fn differ(a: [f32; 3], b: [f32; 3], threshold: f32) -> bool {
    a.iter().zip(b).any(|(a, b)| (a - b).abs() >= threshold)
}

fn mix(from: [f32; 3], to: [f32; 3], amount: f32) -> [f32; 3] {
    std::array::from_fn(|i| from[i] + (to[i] - from[i]) * amount)
}

impl NightLight {
    /// The gains to draw with, in linear light, or `None` when there is nothing to do.
    ///
    /// `None` rather than three ones, because "nothing to do" is a different code path in the
    /// backends -- the one that still hands client buffers and the pointer straight to planes.
    pub fn gains(&self) -> Option<[f32; 3]> {
        differ(self.current, IDENTITY, EPSILON).then_some(self.current)
    }

    /// Where on Earth the sun is looked up for.
    fn coordinates(&mut self, config: &NightLightConfig) -> (f64, f64) {
        if let Some(manual) = config.manual_coordinates() {
            return manual;
        }
        let id = system_zone_id();
        if let Some((known, coordinates)) = &self.zone
            && *known == id
        {
            return *coordinates;
        }
        let listed = id.as_deref().and_then(|id| {
            ZONE_TABLES.iter().find_map(|path| {
                let text = std::fs::read_to_string(path).ok()?;
                lookup(&text, id)
            })
        });
        let coordinates = listed.unwrap_or_else(|| {
            let fallback = location_from_utc_offset();
            // Once per zone, since the result is cached under it.
            warn!(
                zone = id.as_deref().unwrap_or("unknown"),
                longitude = fallback.1,
                "the time zone has no coordinates in tzdata; night light is using the equator \
                 at the UTC offset's longitude. Set [night_light] location = \"manual\" for \
                 real sunset times."
            );
            fallback
        });
        info!(
            zone = id.as_deref().unwrap_or("unknown"),
            latitude = coordinates.0,
            longitude = coordinates.1,
            "night light location"
        );
        self.zone = Some((id, coordinates));
        coordinates
    }

    /// How strongly the schedule says to warm the picture right now, 0 to 1.
    fn scheduled_strength(&mut self, config: &NightLightConfig) -> f32 {
        match config.mode {
            NightLightMode::Off => 0.0,
            NightLightMode::On => 1.0,
            NightLightMode::Auto => {
                let (latitude, longitude) = self.coordinates(config);
                // The wall clock, not a monotonic one: after a suspend, or a change of date,
                // the sun is wherever it is now.
                let now = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map_or(0.0, |since| since.as_secs_f64());
                strength(solar_elevation(now, latitude, longitude))
            }
        }
    }

    /// Bring the gains up to date. Returns whether the picture changed, and how long until this
    /// needs calling again -- `None` if nothing will change until the configuration does.
    fn step(&mut self, config: &NightLightConfig, now: Instant) -> (bool, Option<Duration>) {
        if self.preview_until.is_some_and(|until| until <= now) {
            self.preview_until = None;
        }

        let scheduled = self.scheduled_strength(config);
        let strength = if self.preview_until.is_some() {
            1.0
        } else {
            scheduled
        };
        let target = mix(IDENTITY, gains(config.kelvin() as f32), strength);

        let before = self.current;
        let heading_for = self.fade.map_or(self.current, |fade| fade.to);
        if differ(heading_for, target, EPSILON) {
            if differ(self.current, target, SNAP) {
                self.fade = Some(Fade {
                    from: self.current,
                    to: target,
                    start: now,
                });
            } else {
                self.fade = None;
                self.current = target;
            }
        }

        if let Some(fade) = self.fade {
            let progress = now.duration_since(fade.start).as_secs_f32() / FADE.as_secs_f32();
            if progress >= 1.0 {
                self.current = fade.to;
                self.fade = None;
            } else {
                // Smoothstep: no visible start or stop.
                let eased = progress * progress * (3.0 - 2.0 * progress);
                self.current = mix(fade.from, fade.to, eased);
            }
        }

        let next = if self.fade.is_some() {
            Some(FADE_STEP)
        } else if let Some(until) = self.preview_until {
            Some(until.saturating_duration_since(now))
        } else if config.mode == NightLightMode::Auto {
            Some(if scheduled > 0.0 && scheduled < 1.0 {
                TWILIGHT_TICK
            } else {
                IDLE_TICK
            })
        } else {
            None
        };
        (self.current != before, next)
    }
}

impl Wlrix {
    /// Work out the night light for this moment and keep it up to date from here on.
    ///
    /// Called at startup, after a config reload, and whenever time may have jumped -- a resume,
    /// a VT switch back -- since the timer that would otherwise notice can be up to a minute
    /// late. Safe to call at any time: it replaces whatever timer was running.
    pub fn arm_night_light(&mut self) {
        if let Some(token) = self.night_light.timer.take() {
            self.loop_handle.remove(token);
        }
        let Some(wait) = self.step_night_light() else {
            return;
        };
        self.night_light.timer = self
            .loop_handle
            .insert_source(
                Timer::from_duration(wait),
                |_, _, state: &mut Wlrix| match state.step_night_light() {
                    Some(wait) => TimeoutAction::ToDuration(wait),
                    None => {
                        state.night_light.timer = None;
                        TimeoutAction::Drop
                    }
                },
            )
            .ok();
    }

    /// The night light as it should be at startup: already where the schedule puts it, with no
    /// fade from daylight -- logging in at midnight should not begin with a flash of blue.
    pub fn start_night_light(&mut self) {
        let config = self.config.night_light.clone();
        let strength = self.night_light.scheduled_strength(&config);
        self.night_light.current = mix(IDENTITY, gains(config.kelvin() as f32), strength);
        if config.mode != NightLightMode::Off {
            info!(mode = ?config.mode, kelvin = config.kelvin(), strength, "night light");
        }
        self.arm_night_light();
    }

    /// Pick up a reloaded `[night_light]`. `previous` is the section as it was before.
    pub fn reconfigure_night_light(&mut self, previous: &NightLightConfig) {
        let config = &self.config.night_light;
        if config == previous {
            return;
        }
        info!(mode = ?config.mode, kelvin = config.kelvin(), "night light reconfigured");
        // A new temperature is shown for a moment even if the schedule has the night light
        // off right now, so the settings panel's slider has something to show at midday.
        // Each further change pushes the deadline out, so a drag previews until it stops.
        if config.kelvin() != previous.kelvin() && config.mode != NightLightMode::Off {
            self.night_light.preview_until = Some(Instant::now() + PREVIEW);
        } else if config.mode == NightLightMode::Off {
            self.night_light.preview_until = None;
        }
        self.arm_night_light();
    }

    /// One step of the night light. Asks for a frame if the picture changed.
    fn step_night_light(&mut self) -> Option<Duration> {
        let (changed, next) = self
            .night_light
            .step(&self.config.night_light, Instant::now());
        if changed {
            self.request_redraw();
        }
        next
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::NightLightLocation;

    fn close(actual: [f32; 3], expected: [f32; 3], tolerance: f32) -> bool {
        !differ(actual, expected, tolerance)
    }

    /// Whether the night light is showing `expected`, to within float rounding.
    fn shows(night: &NightLight, expected: [f32; 3]) -> bool {
        night
            .gains()
            .is_some_and(|actual| close(actual, expected, 1e-5))
    }

    /// Seconds since the epoch for a UTC date and time (Howard Hinnant's `days_from_civil`).
    fn utc(year: i64, month: i64, day: i64, hour: i64, minute: i64) -> f64 {
        let y = if month <= 2 { year - 1 } else { year };
        let era = y.div_euclid(400);
        let yoe = y.rem_euclid(400);
        let doy = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + day - 1;
        let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
        let days = era * 146_097 + doe - 719_468;
        (days * 86_400 + hour * 3_600 + minute * 60) as f64
    }

    #[test]
    fn the_epoch_helper_agrees_with_known_dates() {
        assert_eq!(utc(1970, 1, 1, 0, 0), 0.0);
        assert_eq!(utc(2000, 1, 1, 12, 0), 946_728_000.0);
    }

    #[test]
    fn daylight_is_no_change_at_all() {
        assert_eq!(gains(6500.0), IDENTITY);
        // Above the range is held to it: a night light never cools the picture.
        assert_eq!(gains(9000.0), IDENTITY);
    }

    #[test]
    fn warmer_temperatures_take_away_green_and_more_blue() {
        assert!(
            close(gains(4500.0), [1.0, 0.771, 0.507], 5e-3),
            "{:?}",
            gains(4500.0)
        );
        assert!(
            close(gains(3000.0), [1.0, 0.507, 0.154], 5e-3),
            "{:?}",
            gains(3000.0)
        );
        assert!(
            close(gains(2000.0), [1.0, 0.273, 0.008], 5e-3),
            "{:?}",
            gains(2000.0)
        );

        let mut previous = gains(1000.0);
        for kelvin in (1100..=6500).step_by(100) {
            let now = gains(kelvin as f32);
            assert_eq!(now[0], 1.0, "{kelvin} K: red is always the peak");
            assert!(
                now[1] >= previous[1],
                "{kelvin} K: green rises with temperature"
            );
            assert!(
                now[2] >= previous[2],
                "{kelvin} K: blue rises with temperature"
            );
            assert!(now[2] <= now[1], "{kelvin} K: blue goes first");
            previous = now;
        }
    }

    #[test]
    fn the_sun_is_where_an_almanac_puts_it() {
        let near = |actual: f64, expected: f64| (actual - expected).abs() < 0.5;

        // New York's sunset on 2026-11-10 is 16:42 EST. Without refraction the geometric sun
        // is then a little below the horizon rather than on it.
        let sunset = solar_elevation(utc(2026, 11, 10, 21, 42), 40.7142, -74.0064);
        assert!(near(sunset, -0.8), "{sunset}");
        // Near solar noon at the summer solstice.
        let noon = solar_elevation(utc(2026, 6, 21, 17, 0), 40.7142, -74.0064);
        assert!(near(noon, 72.7), "{noon}");
        // Tokyo's first sunrise of 2026, 06:51 JST.
        let sunrise = solar_elevation(utc(2025, 12, 31, 21, 51), 35.6544, 139.7447);
        assert!(near(sunrise, -0.8), "{sunrise}");
        // The equinox on the equator and the prime meridian: almost overhead at noon.
        let overhead = solar_elevation(utc(2025, 3, 20, 12, 0), 0.0, 0.0);
        assert!(overhead > 87.0, "{overhead}");
    }

    #[test]
    fn the_polar_day_and_night_need_no_special_case() {
        // Svalbard. At midwinter noon the sun is far below the horizon: night all day.
        let midwinter_noon = solar_elevation(utc(2026, 12, 21, 11, 0), 78.0, 16.0);
        assert_eq!(strength(midwinter_noon), 1.0, "{midwinter_noon}");
        // And at midsummer midnight it is well above it: no night at all.
        let midsummer_midnight = solar_elevation(utc(2026, 6, 21, 23, 0), 78.0, 16.0);
        assert_eq!(strength(midsummer_midnight), 0.0, "{midsummer_midnight}");
    }

    #[test]
    fn strength_ramps_through_twilight() {
        assert_eq!(strength(45.0), 0.0);
        assert_eq!(strength(DAY_ELEVATION), 0.0);
        assert!((strength(-1.5) - 0.5).abs() < 1e-6);
        assert_eq!(strength(NIGHT_ELEVATION), 1.0);
        assert_eq!(strength(-40.0), 1.0);
    }

    #[test]
    fn the_zone_comes_from_the_environment_then_the_link_then_the_file() {
        let link = Some("/usr/share/zoneinfo/America/New_York");
        assert_eq!(
            zone_id(None, link, None).as_deref(),
            Some("America/New_York")
        );
        assert_eq!(
            zone_id(Some("Asia/Tokyo"), link, None).as_deref(),
            Some("Asia/Tokyo")
        );
        assert_eq!(
            zone_id(Some(":Asia/Tokyo"), link, None).as_deref(),
            Some("Asia/Tokyo")
        );
        assert_eq!(
            zone_id(Some("/usr/share/zoneinfo/Asia/Tokyo"), None, None).as_deref(),
            Some("Asia/Tokyo")
        );
        // A POSIX rule names no place, so the link is used instead.
        assert_eq!(
            zone_id(Some("EST5EDT"), link, None).as_deref(),
            Some("America/New_York")
        );
        // A relative link, as some distributions write it, and tzdata's duplicate trees.
        assert_eq!(
            zone_id(None, Some("../usr/share/zoneinfo/posix/Europe/Paris"), None).as_deref(),
            Some("Europe/Paris")
        );
        assert_eq!(
            zone_id(None, None, Some("Europe/Berlin\n")).as_deref(),
            Some("Europe/Berlin")
        );
        assert_eq!(zone_id(None, None, None), None);
        assert_eq!(zone_id(None, Some("/etc/somewhere/else"), Some(" ")), None);
    }

    #[test]
    fn tzdata_coordinates_are_parsed() {
        let near = |actual: Option<(f64, f64)>, latitude: f64, longitude: f64| {
            actual.is_some_and(|(lat, lon)| {
                (lat - latitude).abs() < 1e-3 && (lon - longitude).abs() < 1e-3
            })
        };
        assert!(near(parse_iso6709("+353916+1394441"), 35.6544, 139.7447));
        assert!(near(parse_iso6709("+404251-0740023"), 40.7142, -74.0064));
        assert!(near(parse_iso6709("-3352+15113"), -33.8667, 151.2167));
        assert!(near(parse_iso6709("+0000+00000"), 0.0, 0.0));

        for bad in [
            "",
            "+3539",
            "353916+1394441",
            "+35391+1394441",
            "+35x9+13944",
        ] {
            assert_eq!(parse_iso6709(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn a_zone_is_found_in_either_table() {
        let zone_tab = "# comment\n\
            JP\t+353916+1394441\tAsia/Tokyo\n\
            US\t+404251-0740023\tAmerica/New_York\tEastern (most areas)\n\
            XX\tnonsense\tBroken/Zone\n";
        let zone1970_tab = "AU,AQ\t-3352+15113\tAustralia/Sydney\tNew South Wales (most areas)\n";

        let tokyo = lookup(zone_tab, "Asia/Tokyo").unwrap();
        assert!((tokyo.0 - 35.6544).abs() < 1e-3 && (tokyo.1 - 139.7447).abs() < 1e-3);
        let new_york = lookup(zone_tab, "America/New_York").unwrap();
        assert!((new_york.1 + 74.0064).abs() < 1e-3);
        let sydney = lookup(zone1970_tab, "Australia/Sydney").unwrap();
        assert!((sydney.0 + 33.8667).abs() < 1e-3);

        assert_eq!(lookup(zone_tab, "Europe/Nowhere"), None);
        assert_eq!(lookup(zone_tab, "Broken/Zone"), None);
        // A prefix of a real name is not that zone.
        assert_eq!(lookup(zone_tab, "Asia/Tok"), None);
    }

    fn config(mode: NightLightMode, temperature: u32) -> NightLightConfig {
        NightLightConfig {
            mode,
            temperature: Some(temperature),
            location: NightLightLocation::Timezone,
            latitude: None,
            longitude: None,
        }
    }

    #[test]
    fn off_is_identity_and_needs_no_timer() {
        let mut night = NightLight::default();
        let (changed, next) = night.step(&config(NightLightMode::Off, 3000), Instant::now());
        assert!(!changed);
        assert_eq!(next, None);
        assert_eq!(night.gains(), None);
    }

    #[test]
    fn switching_on_fades_to_the_temperature_and_then_rests() {
        let mut night = NightLight::default();
        let on = config(NightLightMode::On, 3000);
        let start = Instant::now();

        let (_, next) = night.step(&on, start);
        assert_eq!(next, Some(FADE_STEP), "a fade is under way");

        let (changed, _) = night.step(&on, start + FADE / 2);
        assert!(changed);
        let halfway = night.gains().expect("partly warmed");
        assert!(halfway[2] < 1.0 && halfway[2] > gains(3000.0)[2]);

        let (changed, next) = night.step(&on, start + FADE);
        assert!(changed);
        assert_eq!(next, None, "always-on has nothing left to wait for");
        assert!(shows(&night, gains(3000.0)), "{:?}", night.gains());

        // And off again fades back to nothing.
        let off = config(NightLightMode::Off, 3000);
        night.step(&off, start + FADE);
        let (_, next) = night.step(&off, start + FADE * 2);
        assert_eq!(next, None);
        assert_eq!(night.gains(), None);
    }

    #[test]
    fn a_small_change_is_applied_without_a_fade() {
        let mut night = NightLight::default();
        let start = Instant::now();
        let warm = config(NightLightMode::On, 3000);
        night.step(&warm, start);
        night.step(&warm, start + FADE);

        // 10 K is far below the snap threshold.
        let (changed, next) = night.step(&config(NightLightMode::On, 3010), start + FADE);
        assert!(changed);
        assert_eq!(next, None);
        assert!(shows(&night, gains(3010.0)), "{:?}", night.gains());
    }

    #[test]
    fn a_preview_shows_full_strength_and_then_expires() {
        // Manual coordinates and `Off` would not preview; `On` already is full strength. So the
        // preview is checked through the state it sets rather than through the sun.
        let mut night = NightLight::default();
        let on = config(NightLightMode::On, 4000);
        let start = Instant::now();
        night.preview_until = Some(start + PREVIEW);

        night.step(&on, start);
        let (_, next) = night.step(&on, start + FADE);
        assert_eq!(next, Some(PREVIEW - FADE), "waiting out the preview");
        assert!(shows(&night, gains(4000.0)), "{:?}", night.gains());

        let (_, next) = night.step(&on, start + PREVIEW);
        assert_eq!(night.preview_until, None);
        assert_eq!(next, None);
    }
}
