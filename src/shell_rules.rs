// SPDX-License-Identifier: GPL-3.0-or-later
//! wlRIX shell components, recognised by xdg-shell `app_id`.
//!
//! The toolchest and desks are desktop furniture rather than ordinary windows, so they
//! would normally be wlr-layer-shell surfaces. They are not: Avalonia's Wayland backend
//! implements only xdg-shell, and NWayland ships no wlr protocol bindings, so wlRIX's
//! own apps arrive as ordinary xdg toplevels.
//!
//! Because wlRIX ships its compositor and apps together, we recognise our components by
//! `app_id` and apply the placement and stacking rules layer-shell would otherwise
//! provide. Third-party clients still use the real layer-shell implementation.
//!
//! This is a deliberate stopgap — see `handlers::layer_shell`. Once the Avalonia backend
//! grows layer-shell support, these components should move to it and this module can go.

use smithay::{
    desktop::{Space, Window},
    utils::{Logical, Point, Rectangle, Size},
};

/// Where a shell component sits on its output.
///
/// The full set of corners is kept even though only the left-hand ones have components
/// today — `anchor_position` already handles all four, and a partial enum would be a
/// strange API.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Anchor {
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
}

/// How the compositor treats a recognized shell component.
#[derive(Debug, Clone, Copy)]
pub struct ShellRule {
    pub anchor: Anchor,
    /// Keep stacked above ordinary windows.
    pub always_on_top: bool,
}

/// Marker stored in a [`Window`]'s user data once it is recognized as a component.
pub struct ShellComponent(pub ShellRule);

/// The rule for an `app_id`, if it names a wlRIX shell component.
pub fn rule_for(app_id: &str) -> Option<ShellRule> {
    match app_id {
        // The IRIX toolchest lives in the top-left corner and stays above windows.
        "com.wlrix.toolchest" => Some(ShellRule {
            anchor: Anchor::TopLeft,
            always_on_top: true,
        }),
        // Desks overview sits bottom-left.
        "com.wlrix.desks" => Some(ShellRule {
            anchor: Anchor::BottomLeft,
            always_on_top: true,
        }),
        _ => None,
    }
}

/// Position a component of `size` within `area` according to its anchor.
fn anchor_position(
    anchor: Anchor,
    area: Rectangle<i32, Logical>,
    size: Size<i32, Logical>,
) -> Point<i32, Logical> {
    let right = area.loc.x + (area.size.w - size.w).max(0);
    let bottom = area.loc.y + (area.size.h - size.h).max(0);
    match anchor {
        Anchor::TopLeft => (area.loc.x, area.loc.y).into(),
        Anchor::TopRight => (right, area.loc.y).into(),
        Anchor::BottomLeft => (area.loc.x, bottom).into(),
        Anchor::BottomRight => (right, bottom).into(),
    }
}

/// Re-anchor every recognized shell component and keep the always-on-top ones raised.
///
/// Called after commits, since a component's size is only known once it has drawn.
pub fn arrange(space: &mut Space<Window>) {
    // Collect first: we cannot mutate the space while iterating it.
    let placements: Vec<(Window, Point<i32, Logical>, bool)> = space
        .elements()
        .filter_map(|window| {
            let rule = window.user_data().get::<ShellComponent>()?.0;
            let output = space
                .outputs_for_element(window)
                .first()
                .cloned()
                .or_else(|| space.outputs().next().cloned())?;
            let area = space.output_geometry(&output)?;
            let position = anchor_position(rule.anchor, area, window.geometry().size);
            Some((window.clone(), position, rule.always_on_top))
        })
        .collect();

    for (window, position, always_on_top) in placements {
        // Idempotent: this runs on every commit, so only act when something moved.
        if space.element_location(&window) == Some(position) {
            continue;
        }
        tracing::debug!(?position, "anchoring shell component");
        space.map_element(window.clone(), position, false);
        if always_on_top {
            space.raise_element(&window, false);
        }
    }
}

/// Raise always-on-top components back above ordinary windows.
///
/// Call after raising a normal window, so clicking a window cannot bury the toolchest.
pub fn raise_always_on_top(space: &mut Space<Window>) {
    let components: Vec<Window> = space
        .elements()
        .filter(|window| {
            window
                .user_data()
                .get::<ShellComponent>()
                .is_some_and(|component| component.0.always_on_top)
        })
        .cloned()
        .collect();

    for window in components {
        space.raise_element(&window, false);
    }
}
