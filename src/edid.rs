// SPDX-License-Identifier: GPL-3.0-or-later
//! Who made a monitor and what it is called, from its EDID.
//!
//! `wlr-output-management` advertises a make, model and serial for every head, and a display
//! settings panel is the place a person reads them -- "NEC Corporation EA274WMi" says which
//! screen is which in a way `DP-4` does not. `libdisplay-info` would answer this, but it is
//! deliberately off (see the `smithay-drm-extras` note in `Cargo.toml`), and [`crate::hdr`]
//! already reads the EDID by hand; the identity is three fields of the base block.

use std::sync::OnceLock;

/// The 8 bytes every EDID starts with.
const EDID_MAGIC: [u8; 8] = [0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x00];
/// The base block, which is all identity needs.
const BLOCK: usize = 128;
/// Where the four 18-byte descriptors of the base block start.
const DESCRIPTORS: [usize; 4] = [0x36, 0x48, 0x5a, 0x6c];
/// Display descriptor tag: product name.
const TAG_NAME: u8 = 0xfc;
/// Display descriptor tag: serial number, as text.
const TAG_SERIAL: u8 = 0xff;

/// Where distributions install the PNP ID registry, most common first.
const PNP_IDS: [&str; 3] = [
    "/usr/share/hwdata/pnp.ids",
    "/usr/share/misc/pnp.ids",
    "/usr/local/share/hwdata/pnp.ids",
];

/// A monitor's identity, as its EDID gives it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    /// The manufacturer's name, or its three-letter PNP ID when the registry does not know it.
    pub make: String,
    /// The product name, or the product code in hex when the EDID carries no name.
    pub model: String,
    /// The serial number, when the EDID carries a real one.
    pub serial: Option<String>,
}

/// Read a monitor's identity from its EDID, or `None` if this is not one.
pub fn identity(edid: &[u8]) -> Option<Identity> {
    identity_with(edid, manufacturer_name)
}

/// [`identity`], with the PNP registry lookup supplied, so tests do not depend on what the
/// machine running them has installed.
fn identity_with(edid: &[u8], name_of: impl Fn(&str) -> Option<String>) -> Option<Identity> {
    if edid.len() < BLOCK || edid[..8] != EDID_MAGIC {
        return None;
    }

    let code = pnp_id(u16::from_be_bytes([edid[8], edid[9]]))?;
    let make = name_of(&code).unwrap_or(code);

    let model = descriptor_text(edid, TAG_NAME)
        .unwrap_or_else(|| format!("0x{:04X}", u16::from_le_bytes([edid[10], edid[11]])));

    // The text serial when there is one. The numeric one is a fallback, and 0 and 0x01010101
    // are the two values manufacturers use to mean "not set".
    let serial = descriptor_text(edid, TAG_SERIAL).or_else(|| {
        let number = u32::from_le_bytes([edid[12], edid[13], edid[14], edid[15]]);
        (number != 0 && number != 0x0101_0101).then(|| number.to_string())
    });

    Some(Identity {
        make,
        model,
        serial,
    })
}

/// The three-letter manufacturer code: three 5-bit letters, `A` being 1, with the top bit clear.
fn pnp_id(packed: u16) -> Option<String> {
    let letter = |shift: u16| {
        let value = ((packed >> shift) & 0x1f) as u8;
        (1..=26)
            .contains(&value)
            .then(|| char::from(b'A' + value - 1))
    };
    Some([letter(10)?, letter(5)?, letter(0)?].iter().collect())
}

/// The text of the base block's display descriptor carrying `tag`, if there is one with any
/// text in it.
///
/// A descriptor holds up to 13 bytes, ended early by a line feed and padded with spaces.
fn descriptor_text(edid: &[u8], tag: u8) -> Option<String> {
    DESCRIPTORS.iter().find_map(|&offset| {
        let descriptor = &edid[offset..offset + 18];
        // Display descriptors start with a zero pixel clock; anything else is a timing.
        if descriptor[0] != 0 || descriptor[1] != 0 || descriptor[3] != tag {
            return None;
        }
        let text = &descriptor[5..18];
        let end = text.iter().position(|&b| b == b'\n').unwrap_or(text.len());
        let text: String = text[..end]
            .iter()
            .map(|&b| {
                if b.is_ascii_graphic() || b == b' ' {
                    char::from(b)
                } else {
                    '?'
                }
            })
            .collect();
        let text = text.trim();
        (!text.is_empty()).then(|| text.to_owned())
    })
}

/// The manufacturer's name for a PNP ID, from the system's registry.
///
/// The registry is read once, on first use, and kept. It is around 2,500 lines and a monitor
/// is looked up only when it is plugged in, so keeping it costs little and saves rereading it
/// on every hotplug.
fn manufacturer_name(code: &str) -> Option<String> {
    static REGISTRY: OnceLock<Vec<(String, String)>> = OnceLock::new();
    let registry = REGISTRY.get_or_init(|| {
        PNP_IDS
            .iter()
            .find_map(|path| std::fs::read_to_string(path).ok())
            .map(|text| parse_registry(&text))
            .unwrap_or_default()
    });
    registry
        .iter()
        .find(|(known, _)| known == code)
        .map(|(_, name)| name.clone())
}

/// `pnp.ids` is one `CODE<TAB>Name` per line.
fn parse_registry(text: &str) -> Vec<(String, String)> {
    text.lines()
        .filter_map(|line| {
            let (code, name) = line.split_once('\t')?;
            let name = name.trim();
            (code.len() == 3 && !name.is_empty()).then(|| (code.to_owned(), name.to_owned()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The same panel [`crate::hdr`]'s tests use.
    const MPG271QX: &[u8] = include_bytes!("../tests/data/mpg271qx.edid");

    fn registry(code: &str) -> Option<String> {
        parse_registry("MSI\tMicrostep\nNEC\tNEC Corporation\n")
            .into_iter()
            .find(|(known, _)| known == code)
            .map(|(_, name)| name)
    }

    #[test]
    fn mpg271qx_is_named() {
        let identity = identity_with(MPG271QX, registry).expect("a valid EDID");
        assert_eq!(identity.make, "Microstep");
        assert_eq!(identity.model, "MPG271QX OLED");
        // Its serial descriptor is empty and its numeric serial is the 0x01010101 filler.
        assert_eq!(identity.serial, None);
    }

    #[test]
    fn an_unknown_manufacturer_keeps_its_code() {
        let identity = identity_with(MPG271QX, |_| None).unwrap();
        assert_eq!(identity.make, "MSI");
    }

    #[test]
    fn serial_comes_from_the_descriptor_when_there_is_one() {
        let mut edid = MPG271QX.to_vec();
        // The fixture's fourth descriptor is the empty serial; give it text.
        let text = &mut edid[0x6c + 5..0x6c + 18];
        assert_eq!(text[0], b'\n', "fixture layout changed");
        text[..9].copy_from_slice(b"3X100335\n");
        assert_eq!(
            identity_with(&edid, registry).unwrap().serial.as_deref(),
            Some("3X100335")
        );
    }

    #[test]
    fn the_numeric_serial_is_the_fallback() {
        let mut edid = MPG271QX.to_vec();
        edid[12..16].copy_from_slice(&1234u32.to_le_bytes());
        assert_eq!(
            identity_with(&edid, registry).unwrap().serial.as_deref(),
            Some("1234")
        );
    }

    #[test]
    fn a_missing_name_falls_back_to_the_product_code() {
        let mut edid = MPG271QX.to_vec();
        // Retag the name descriptor as something else.
        assert_eq!(edid[0x5a + 3], TAG_NAME, "fixture layout changed");
        edid[0x5a + 3] = 0x10;
        assert_eq!(identity_with(&edid, registry).unwrap().model, "0x3CD7");
    }

    #[test]
    fn garbage_is_not_an_edid() {
        assert!(identity(&[]).is_none());
        assert!(identity(&[0u8; 128]).is_none());
    }
}
