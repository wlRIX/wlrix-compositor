// SPDX-License-Identifier: GPL-3.0-or-later
//! HDR output: what a panel can show, and the metadata the kernel needs to switch it there.
//!
//! Driving a display in HDR is two separate things, and this module holds the first:
//!
//! 1. **Telling the panel.** The connector carries a `Colorspace` property and an
//!    `HDR_OUTPUT_METADATA` blob. Setting them is what makes the monitor leave SDR mode and
//!    start interpreting the signal as PQ / BT.2020. The blob's layout is a kernel ABI, so it
//!    is mirrored here byte for byte.
//! 2. **Meaning it.** The framebuffer then has to *contain* PQ-encoded BT.2020, which is the
//!    encode pass in the udev backend. Doing (1) without (2) is what a washed-out screen looks
//!    like -- neither is useful alone.
//!
//! What a panel can do comes from its EDID, which is parsed here rather than through
//! `libdisplay-info`: that dependency is deliberately off (see the `smithay-drm-extras` note in
//! `Cargo.toml`), and only one data block is actually needed. The connector's EDID is already
//! readable through the `drm` crate, so this costs no new dependency.
//!
//! Capability itself is cached per output name, the way [`crate::vrr`] caches adaptive sync: only
//! the hardware backend can answer it, and the protocol and policy code have to be able to ask
//! without reaching into the backend. Under the nested backend there is no connector, so nothing
//! is supported -- which is the honest answer there.

use std::collections::HashMap;

use smithay::output::Output;

/// Reference white for SDR content shown on an HDR output, in cd/m².
///
/// ITU-R BT.2408's "HDR reference white". Lower and the desktop looks dim next to an SDR screen;
/// higher and ordinary white burns on an OLED. Overridable per output.
pub const DEFAULT_SDR_WHITE_NITS: f32 = 203.0;

/// A CIE 1931 xy chromaticity.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Chromaticity {
    pub x: f32,
    pub y: f32,
}

impl Chromaticity {
    const fn new(x: f32, y: f32) -> Self {
        Self { x, y }
    }

    /// In the units DRM's HDR metadata uses: 0.00002 per step.
    fn to_drm(self) -> DrmChromaticity {
        DrmChromaticity {
            x: (self.x * 50_000.0).round() as u16,
            y: (self.y * 50_000.0).round() as u16,
        }
    }
}

/// What a display says it can actually show: its primaries and its luminance range.
///
/// This is what goes into the mastering-display metadata the panel is handed. For a *display*
/// (rather than a mastering monitor) the honest thing to send is the panel's own capability,
/// which is what the EDID reports.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Mastering {
    pub red: Chromaticity,
    pub green: Chromaticity,
    pub blue: Chromaticity,
    pub white: Chromaticity,
    /// Peak luminance, cd/m².
    pub max_luminance: f32,
    /// Black level, cd/m². Often ~0 on an OLED.
    pub min_luminance: f32,
    /// Sustained full-frame luminance, cd/m². Usually well below the peak.
    pub max_frame_average: f32,
}

impl Mastering {
    /// BT.2020 primaries at D65, with the luminance range PQ was defined against.
    ///
    /// The fallback for a panel that advertises ST2084 but carries no static-metadata block:
    /// claiming the container's full range is the conventional "no opinion" answer, and leaves
    /// tone mapping to the display.
    pub const BT2020: Self = Self {
        red: Chromaticity::new(0.708, 0.292),
        green: Chromaticity::new(0.170, 0.797),
        blue: Chromaticity::new(0.131, 0.046),
        white: Chromaticity::new(0.3127, 0.3290),
        max_luminance: 10_000.0,
        min_luminance: 0.0,
        max_frame_average: 10_000.0,
    };
}

// -- EDID ------------------------------------------------------------------------------------

/// The 8 bytes every EDID starts with.
const EDID_MAGIC: [u8; 8] = [0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x00];
/// One EDID block. An EDID is a base block plus zero or more extensions, all this size.
const BLOCK: usize = 128;
/// Extension block tag for a CTA-861 block, the one that carries the HDR data block.
const CTA_TAG: u8 = 0x02;
/// CTA data block type 7: the payload's first byte is an *extended* tag.
const CTA_EXTENDED_TAG: u8 = 7;
/// Extended tag 6: HDR Static Metadata Data Block.
const CTA_HDR_STATIC_METADATA: u8 = 0x06;
/// Bit 2 of the transfer-function byte: SMPTE ST2084, i.e. PQ. Without it the panel has no HDR
/// mode worth switching into, whatever else it claims.
const ET_SMPTE_ST2084: u8 = 1 << 2;

/// The panel's HDR capability, from its EDID, or `None` if it does not do PQ.
///
/// Returning `None` for a display that has no ST2084 bit is the point: `Colorspace` and
/// `HDR_OUTPUT_METADATA` may well exist on the connector regardless, so the property list alone
/// does not prove the monitor on the end of the cable can do anything with them.
pub fn edid_hdr_static_metadata(edid: &[u8]) -> Option<Mastering> {
    if edid.len() < BLOCK || edid[..8] != EDID_MAGIC {
        return None;
    }

    // The extension count is a claim; trust the buffer we actually got instead, since a short
    // read here would otherwise index past the end.
    let blocks = edid.len() / BLOCK;
    let hdr = (1..blocks).find_map(|index| {
        let block = &edid[index * BLOCK..(index + 1) * BLOCK];
        (block[0] == CTA_TAG).then(|| cta_hdr_static_metadata(block))?
    })?;

    // Byte 0 is the set of transfer functions, byte 1 the static-metadata descriptor types.
    // Luminance bytes 2..4 are optional -- the block is allowed to stop short of them.
    if hdr.first().copied().unwrap_or(0) & ET_SMPTE_ST2084 == 0 {
        return None;
    }

    let mut mastering = Mastering {
        max_luminance: Mastering::BT2020.max_luminance,
        min_luminance: Mastering::BT2020.min_luminance,
        max_frame_average: Mastering::BT2020.max_frame_average,
        ..edid_chromaticity(edid)
    };
    if let Some(&code) = hdr.get(2) {
        mastering.max_luminance = cta_max_luminance(code);
    }
    if let Some(&code) = hdr.get(3) {
        mastering.max_frame_average = cta_max_luminance(code);
    }
    // Min is expressed as a fraction of max, so it has to be read after it.
    if let Some(&code) = hdr.get(4) {
        mastering.min_luminance = cta_min_luminance(code, mastering.max_luminance);
    }
    Some(mastering)
}

/// The payload of a CTA-861 block's HDR Static Metadata Data Block, without its tag bytes.
fn cta_hdr_static_metadata(block: &[u8]) -> Option<&[u8]> {
    // Byte 2 is where the detailed timings start, so it is also where the data block collection
    // ends. Below 4 there is no collection at all.
    let end = usize::from(block[2]).min(BLOCK);
    let mut index = 4;
    // A block claiming no detailed timings (`end` below 4) has no collection either, and this
    // never runs. The payload slice is bounds-checked separately, so a length that runs off
    // the end of the block gives up rather than panicking.
    while index < end {
        let header = block[index];
        let length = usize::from(header & 0x1f);
        let payload = block.get(index + 1..index + 1 + length)?;
        if header >> 5 == CTA_EXTENDED_TAG
            && payload.first() == Some(&CTA_HDR_STATIC_METADATA)
            && let Some(rest) = payload.get(1..)
        {
            return Some(rest);
        }
        index += 1 + length;
    }
    None
}

/// CTA-861's luminance coding: a logarithmic curve from 50 cd/m² up.
fn cta_max_luminance(code: u8) -> f32 {
    50.0 * 2f32.powf(f32::from(code) / 32.0)
}

/// CTA-861's minimum luminance, which is coded as a fraction of the maximum.
fn cta_min_luminance(code: u8, max: f32) -> f32 {
    max * (f32::from(code) / 255.0).powi(2) / 100.0
}

/// The primaries and white point from the EDID base block.
///
/// Each coordinate is 10 bits: eight in its own byte, with the low two packed into a shared pair
/// of bytes at 0x19/0x1a. The awkward layout is why this is written out rather than looped.
fn edid_chromaticity(edid: &[u8]) -> Mastering {
    let low = |byte: u8, shift: u8| u16::from((byte >> shift) & 0b11);
    let coord = |high: u8, low: u16| (u16::from(high) << 2 | low) as f32 / 1024.0;

    let rg = edid[0x19];
    let bw = edid[0x1a];
    Mastering {
        red: Chromaticity::new(coord(edid[0x1b], low(rg, 6)), coord(edid[0x1c], low(rg, 4))),
        green: Chromaticity::new(coord(edid[0x1d], low(rg, 2)), coord(edid[0x1e], low(rg, 0))),
        blue: Chromaticity::new(coord(edid[0x1f], low(bw, 6)), coord(edid[0x20], low(bw, 4))),
        white: Chromaticity::new(coord(edid[0x21], low(bw, 2)), coord(edid[0x22], low(bw, 0))),
        ..Mastering::BT2020
    }
}

// -- The kernel blob -------------------------------------------------------------------------

/// `hdr_metadata_infoframe.eotf`: SMPTE ST2084, i.e. PQ.
const EOTF_SMPTE_ST2084: u8 = 2;
/// `hdr_metadata_infoframe.metadata_type`: static metadata type 1, the only one defined.
const STATIC_METADATA_TYPE1: u8 = 1;

/// A chromaticity as the kernel wants it: 0.00002 per step.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct DrmChromaticity {
    x: u16,
    y: u16,
}

/// `struct hdr_metadata_infoframe` from `include/uapi/drm/drm_mode.h`.
///
/// Kernel ABI: field order and widths must match exactly. `#[repr(C)]` reproduces the C layout,
/// which happens to need no padding -- every member is `u8` or `u16`.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct HdrMetadataInfoframe {
    eotf: u8,
    metadata_type: u8,
    /// Red, green, blue -- in that order, which is not the order the EDID stores them in.
    display_primaries: [DrmChromaticity; 3],
    white_point: DrmChromaticity,
    /// cd/m².
    max_display_mastering_luminance: u16,
    /// 0.0001 cd/m², so a dark OLED black is representable.
    min_display_mastering_luminance: u16,
    /// Maximum content light level, cd/m².
    max_cll: u16,
    /// Maximum frame-average light level, cd/m².
    max_fall: u16,
}

/// `struct hdr_output_metadata` from `include/uapi/drm/drm_mode.h`, the `HDR_OUTPUT_METADATA`
/// property blob.
///
/// The kernel checks the blob's length against `sizeof(struct hdr_output_metadata)`, which is 32
/// -- four bytes of `metadata_type`, the 26-byte infoframe, and two bytes of tail padding from
/// the struct's 4-byte alignment. A blob built from just the infoframe is rejected.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HdrOutputMetadata {
    metadata_type: u32,
    hdmi_metadata_type1: HdrMetadataInfoframe,
}

impl HdrOutputMetadata {
    /// The blob that puts a panel into PQ mode, describing what it can show.
    pub fn st2084(mastering: &Mastering) -> Self {
        Self {
            metadata_type: u32::from(STATIC_METADATA_TYPE1),
            hdmi_metadata_type1: HdrMetadataInfoframe {
                eotf: EOTF_SMPTE_ST2084,
                metadata_type: STATIC_METADATA_TYPE1,
                display_primaries: [
                    mastering.red.to_drm(),
                    mastering.green.to_drm(),
                    mastering.blue.to_drm(),
                ],
                white_point: mastering.white.to_drm(),
                max_display_mastering_luminance: mastering.max_luminance.round() as u16,
                min_display_mastering_luminance: (mastering.min_luminance * 10_000.0).round()
                    as u16,
                max_cll: mastering.max_luminance.round() as u16,
                max_fall: mastering.max_frame_average.round() as u16,
            },
        }
    }
}

// -- Cached capability -----------------------------------------------------------------------

/// What is known about each output's HDR, keyed by output name.
///
/// Same shape as [`crate::vrr::VrrState`], and for the same reason: only the hardware backend can
/// discover any of it, but the protocol and render paths need to consult it.
#[derive(Default)]
pub struct HdrState {
    supported: HashMap<String, bool>,
    active: HashMap<String, bool>,
    mastering: HashMap<String, Mastering>,
    sdr_white: HashMap<String, f32>,
    /// Whether this output composites in linear light. See [`crate::hdr_render::WorkingSpace`].
    linear: HashMap<String, bool>,
}

impl HdrState {
    /// Whether this output can be driven in HDR at all: the connector has the properties *and*
    /// the panel advertises PQ.
    ///
    /// False until the backend says otherwise, so an output that has never been asked is never
    /// reported as capable.
    pub fn supported(&self, output: &Output) -> bool {
        self.supported.get(&output.name()).copied().unwrap_or(false)
    }

    /// Whether this output is being driven in HDR right now. This is what the render path
    /// branches on and what the color-management protocol reports.
    pub fn active(&self, output: &Output) -> bool {
        self.active.get(&output.name()).copied().unwrap_or(false)
    }

    /// The panel's colorimetry, once the backend has read its EDID.
    pub fn mastering(&self, output: &Output) -> Option<Mastering> {
        self.mastering.get(&output.name()).copied()
    }

    /// Reference white for SDR content on this output, in cd/m².
    pub fn sdr_white(&self, output: &Output) -> f32 {
        self.sdr_white
            .get(&output.name())
            .copied()
            .unwrap_or(DEFAULT_SDR_WHITE_NITS)
    }

    pub fn set_supported(&mut self, output: &Output, supported: bool) {
        self.supported.insert(output.name(), supported);
    }

    pub fn set_active(&mut self, output: &Output, active: bool) {
        self.active.insert(output.name(), active);
    }

    pub fn set_mastering(&mut self, output: &Output, mastering: Mastering) {
        self.mastering.insert(output.name(), mastering);
    }

    pub fn set_sdr_white(&mut self, output: &Output, nits: f32) {
        self.sdr_white.insert(output.name(), nits);
    }

    /// Whether this output alpha-composites in linear light. On unless configured off: it is the
    /// correct answer, and the reason to turn it off is a matter of taste about text.
    pub fn linear_blending(&self, output: &Output) -> bool {
        self.linear.get(&output.name()).copied().unwrap_or(true)
    }

    pub fn set_linear_blending(&mut self, output: &Output, linear: bool) {
        self.linear.insert(output.name(), linear);
    }

    /// Drop an output that has gone away, so a different monitor plugged into the same
    /// connector does not inherit the old one's capability.
    pub fn forget(&mut self, output: &Output) {
        self.supported.remove(&output.name());
        self.active.remove(&output.name());
        self.mastering.remove(&output.name());
        self.sdr_white.remove(&output.name());
        self.linear.remove(&output.name());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A real EDID from one of the dev machine's MSI MPG271QX OLED panels -- the display this
    /// work was written against. Parsing a synthetic EDID would prove nothing about the layout
    /// traps (packed chromaticity bits, the extended-tag data block walk).
    const MPG271QX: &[u8] = include_bytes!("../tests/data/mpg271qx.edid");

    /// The blob is a kernel ABI. If this size is wrong the property is rejected outright, and
    /// the failure looks like "HDR just doesn't work" rather than anything about layout.
    #[test]
    fn metadata_blob_matches_the_kernel_struct() {
        assert_eq!(std::mem::size_of::<HdrOutputMetadata>(), 32);
        assert_eq!(std::mem::size_of::<HdrMetadataInfoframe>(), 26);
        assert_eq!(std::mem::size_of::<DrmChromaticity>(), 4);
    }

    #[test]
    fn mpg271qx_reports_pq_and_its_luminance_range() {
        let mastering = edid_hdr_static_metadata(MPG271QX).expect("panel advertises ST2084");

        // Codes 102/102/2 in the HDR Static Metadata Data Block.
        assert!((mastering.max_luminance - 455.5).abs() < 0.5);
        assert!((mastering.max_frame_average - 455.5).abs() < 0.5);
        assert!(mastering.min_luminance < 0.001);

        // Chromaticity, cross-checked against `edid-decode`.
        assert!((mastering.red.x - 0.6855).abs() < 0.001);
        assert!((mastering.red.y - 0.3037).abs() < 0.001);
        assert!((mastering.blue.x - 0.1435).abs() < 0.001);
        assert!((mastering.blue.y - 0.0576).abs() < 0.001);
        // Near D65, as any sane panel is.
        assert!((mastering.white.x - 0.3127).abs() < 0.01);
        assert!((mastering.white.y - 0.3290).abs() < 0.01);
    }

    #[test]
    fn mpg271qx_blob_carries_st2084() {
        let mastering = edid_hdr_static_metadata(MPG271QX).unwrap();
        let blob = HdrOutputMetadata::st2084(&mastering);
        assert_eq!(blob.metadata_type, 1);
        assert_eq!(blob.hdmi_metadata_type1.eotf, EOTF_SMPTE_ST2084);
        assert_eq!(
            blob.hdmi_metadata_type1.metadata_type,
            STATIC_METADATA_TYPE1
        );
        assert_eq!(blob.hdmi_metadata_type1.max_cll, 456);
        // 0.6855 * 50000, in the kernel's 0.00002 units.
        assert_eq!(blob.hdmi_metadata_type1.display_primaries[0].x, 34277);
    }

    #[test]
    fn garbage_is_not_an_edid() {
        assert!(edid_hdr_static_metadata(&[]).is_none());
        assert!(edid_hdr_static_metadata(&[0u8; 128]).is_none());
        // A valid base block with no extensions has no CTA block, so no HDR block.
        let mut base = [0u8; 128];
        base[..8].copy_from_slice(&EDID_MAGIC);
        assert!(edid_hdr_static_metadata(&base).is_none());
    }

    /// An SDR panel's CTA block has no HDR data block at all -- it must not be mistaken for one
    /// claiming the BT.2020 fallback.
    #[test]
    fn a_panel_without_st2084_is_not_hdr() {
        let mut edid = MPG271QX.to_vec();
        // Clear the transfer-function byte of the HDR Static Metadata Data Block, leaving the
        // block otherwise intact: the panel now advertises no EOTFs at all.
        let et = 0x80 + 47;
        assert_eq!(edid[et], 0x05, "fixture layout changed");
        edid[et] = 0x00;
        assert!(edid_hdr_static_metadata(&edid).is_none());
    }
}
