// SPDX-License-Identifier: GPL-3.0-or-later
//! wlRIX compositor - scaffold entry point.
//!
//! Establishes the crate and toolchain. The real compositor will be built on
//! Smithay; for now this only proves the crate builds and runs.

fn main() {
    println!(
        "wlrix-compositor {} - scaffold; Smithay integration pending.",
        env!("CARGO_PKG_VERSION")
    );
}
