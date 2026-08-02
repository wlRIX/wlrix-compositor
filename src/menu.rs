// SPDX-License-Identifier: GPL-3.0-or-later
//! The window menu: the 4Dwm menu posted from a window's menu button, from a right click on its
//! decorations, or from a right click on its minimized icon.
//!
//! This module owns the menu's contents, layout and hit-testing. The drawing primitives (raised
//! panel, gold selection, etched separator) live in [`crate::decoration`], the compositing in
//! [`crate::render`], and the pointer wiring in [`crate::input`].
//!
//! Which items can be chosen depends on the window: the enabled flags are worked out once, when
//! the menu is posted, so what is drawn and what a click does cannot disagree.

use smithay::{
    desktop::Window,
    utils::{Logical, Point, Rectangle, Serial, Size},
};

use crate::desks;

/// Height of an ordinary item row.
const ITEM_H: i32 = 22;
/// Height of a separator row.
const SEPARATOR_H: i32 = 7;
/// Panel width. Fixed rather than measured from the labels, so hit-testing needs no font metrics.
const WIDTH: i32 = 156;
/// Margin between the panel edge and the item rows.
const MARGIN: i32 = 3;
/// Left inset of an item's label.
pub const LABEL_INSET: i32 = 14;
/// Label size in logical pixels.
pub const LABEL_PX: f32 = 14.0;

/// What choosing an item does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuAction {
    /// Un-minimize a minimized window, or return a maximized one to its previous size.
    Restore,
    /// Move the window with the pointer until the next click.
    Move,
    /// Resize the window with the pointer. Not implemented yet, so always disabled.
    Size,
    Minimize,
    Maximize,
    Raise,
    Lower,
    Close,
}

/// One row of the menu. A separator is a row with no action.
pub struct Entry {
    pub action: Option<MenuAction>,
    pub label: &'static str,
    pub enabled: bool,
    height: i32,
}

impl Entry {
    fn separator() -> Self {
        Self {
            action: None,
            label: "",
            enabled: false,
            height: SEPARATOR_H,
        }
    }

    fn item(action: MenuAction, label: &'static str, enabled: bool) -> Self {
        Self {
            action: Some(action),
            label,
            enabled,
            height: ITEM_H,
        }
    }

    /// Whether this row is a separator rather than a choosable item.
    pub fn is_separator(&self) -> bool {
        self.action.is_none()
    }
}

/// A posted window menu.
pub struct WindowMenu {
    /// The window the menu acts on. Held as a clone so the menu survives the window being
    /// unmapped (a minimized window's menu is posted from its icon).
    pub window: Window,
    /// The panel's top-left, already clamped onto the output.
    origin: Point<i32, Logical>,
    pub entries: Vec<Entry>,
    /// The row the pointer is over, if it is over a choosable one.
    pub hovered: Option<usize>,
}

impl WindowMenu {
    /// Build the menu for `window`, with each item enabled according to the window's state.
    pub fn new(window: &Window, origin: Point<i32, Logical>) -> Self {
        let (minimized, maximized) = {
            let state = desks::window_state(window).borrow();
            (state.minimized, state.maximized)
        };
        // What the window itself says it will allow. The titlebar drops the buttons it cannot
        // use; the menu grays out the same items, so the two never disagree about what the
        // window can do.
        let capabilities = crate::frame::capabilities(window);
        let entries = vec![
            // Restore undoes whichever of the two states the window is in; with neither there is
            // nothing to restore.
            Entry::item(MenuAction::Restore, "Restore", minimized || maximized),
            Entry::item(MenuAction::Move, "Move", true),
            // Resizing from the menu is not wired up yet.
            Entry::item(MenuAction::Size, "Size", false),
            Entry::item(
                MenuAction::Minimize,
                "Minimize",
                !minimized && capabilities.minimizable,
            ),
            Entry::item(
                MenuAction::Maximize,
                "Maximize",
                !maximized && capabilities.maximizable,
            ),
            Entry::item(MenuAction::Raise, "Raise", true),
            Entry::item(MenuAction::Lower, "Lower", true),
            Entry::separator(),
            Entry::item(MenuAction::Close, "Close", true),
        ];
        Self {
            window: window.clone(),
            origin,
            entries,
            hovered: None,
        }
    }

    /// The panel rectangle, bevel included.
    pub fn panel(&self) -> Rectangle<i32, Logical> {
        panel_rect(&self.entries, self.origin)
    }

    /// The rectangle of row `index`, inset from the panel edges.
    pub fn row(&self, index: usize) -> Rectangle<i32, Logical> {
        row_rect(&self.entries, self.origin, index)
    }

    /// The row under `point`, whether or not it can be chosen. `None` outside the panel.
    fn row_at(&self, point: Point<f64, Logical>) -> Option<usize> {
        if !self.panel().to_f64().contains(point) {
            return None;
        }
        (0..self.entries.len()).find(|&index| self.row(index).to_f64().contains(point))
    }

    /// The action `point` would choose: a row that is both an item and enabled.
    pub fn action_at(&self, point: Point<f64, Logical>) -> Option<MenuAction> {
        let entry = &self.entries[self.row_at(point)?];
        entry.enabled.then_some(entry.action).flatten()
    }

    /// Whether `point` is anywhere on the panel, so a press there belongs to the menu.
    pub fn contains(&self, point: Point<f64, Logical>) -> bool {
        self.panel().to_f64().contains(point)
    }

    /// Track the pointer, highlighting the row it is over. Returns whether the highlight moved,
    /// so the caller can avoid redrawing for nothing.
    pub fn hover(&mut self, point: Point<f64, Logical>) -> bool {
        let hovered = self
            .row_at(point)
            .filter(|&index| self.entries[index].enabled);
        let changed = hovered != self.hovered;
        self.hovered = hovered;
        changed
    }

    /// Place a menu of this size so it fits within `area`, preferring `at` as its top-left.
    pub fn clamp(
        at: Point<i32, Logical>,
        panel: Size<i32, Logical>,
        area: Rectangle<i32, Logical>,
    ) -> Point<i32, Logical> {
        let x = at.x.min(area.loc.x + area.size.w - panel.w).max(area.loc.x);
        let y = at.y.min(area.loc.y + area.size.h - panel.h).max(area.loc.y);
        Point::from((x, y))
    }

    /// The panel size a menu for `window` would have, for placing it before it is built.
    pub fn size_for(window: &Window) -> Size<i32, Logical> {
        WindowMenu::new(window, Point::from((0, 0))).panel().size
    }
}

impl crate::Wlrix {
    /// Post the window menu for `window` with its top-left at `at`, clamped onto the output so
    /// the whole panel is reachable. Replaces any menu already open.
    pub fn open_window_menu(&mut self, window: &Window, at: Point<i32, Logical>) {
        let output = self
            .space
            .outputs_for_element(window)
            .into_iter()
            .next()
            .or_else(|| self.space.outputs().next().cloned());
        let origin = match output {
            Some(output) => {
                let area = crate::placement::work_area(&self.space, &output);
                WindowMenu::clamp(at, WindowMenu::size_for(window), area)
            }
            None => at,
        };
        self.window_menu = Some(WindowMenu::new(window, origin));
        self.request_redraw();
    }

    /// Follow the pointer over an open menu, highlighting the row under it.
    pub fn hover_window_menu(&mut self, point: Point<f64, Logical>) {
        if let Some(menu) = self.window_menu.as_mut()
            && menu.hover(point)
        {
            self.request_redraw();
        }
    }

    /// Take the menu down if it belongs to `window`, which is going away. Also drops the
    /// double-click record, so a recycled window cannot inherit a half-finished click.
    pub fn forget_window_menu(&mut self, window: &Window) {
        if self
            .window_menu
            .as_ref()
            .is_some_and(|menu| &menu.window == window)
        {
            self.close_window_menu();
        }
        if self
            .last_menu_click
            .as_ref()
            .is_some_and(|(w, _)| w == window)
        {
            self.last_menu_click = None;
        }
    }

    /// Take down the menu, if one is open. Returns whether there was one.
    pub fn close_window_menu(&mut self) -> bool {
        let had = self.window_menu.take().is_some();
        if had {
            self.request_redraw();
        }
        had
    }

    /// Carry out `action` on `window`, as chosen from its menu. `at` is where the menu was
    /// clicked, which a move starts tracking from.
    pub fn activate_menu_action(
        &mut self,
        window: &Window,
        action: MenuAction,
        serial: Serial,
        at: Point<f64, Logical>,
    ) {
        let (minimized, maximized) = {
            let state = desks::window_state(window).borrow();
            (state.minimized, state.maximized)
        };
        match action {
            // Restore undoes whichever state the window is in; minimized first, since a window
            // can be both (minimized while maximized) and un-minimizing is what "restore" means
            // for the icon the menu was posted from.
            MenuAction::Restore => {
                if minimized {
                    self.restore_window(window);
                } else if maximized {
                    self.unmaximize_window(window);
                }
            }
            // A minimized window has no frame to drag, so its icon moves in the grid instead --
            // the same "follows the pointer until the next click" gesture, applied to the tile.
            MenuAction::Move => {
                if minimized {
                    self.start_menu_icon_move(window, at);
                } else {
                    self.start_menu_move(window, serial);
                }
            }
            // Not wired up yet; the item is drawn disabled so this is unreachable.
            MenuAction::Size => {}
            MenuAction::Minimize => self.minimize_window(window),
            // Maximizing a minimized window brings it back first; leaving it hidden but flagged
            // maximized would be a state with nothing on screen to show for it.
            MenuAction::Maximize => {
                if minimized {
                    self.restore_window(window);
                }
                self.maximize_window(window);
            }
            MenuAction::Raise => self.raise_window(window),
            MenuAction::Lower => self.lower_window(window),
            MenuAction::Close => self.close_window(window),
        }
    }
}

/// The panel rectangle for a set of rows laid out from `origin`.
fn panel_rect(entries: &[Entry], origin: Point<i32, Logical>) -> Rectangle<i32, Logical> {
    let height: i32 = entries.iter().map(|entry| entry.height).sum();
    Rectangle::new(origin, Size::from((WIDTH, height + 2 * MARGIN)))
}

/// The rectangle of row `index`, inset from the panel edges.
fn row_rect(
    entries: &[Entry],
    origin: Point<i32, Logical>,
    index: usize,
) -> Rectangle<i32, Logical> {
    let top: i32 = entries[..index].iter().map(|entry| entry.height).sum();
    Rectangle::new(
        Point::from((origin.x + MARGIN, origin.y + MARGIN + top)),
        Size::from((WIDTH - 2 * MARGIN, entries[index].height)),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn area() -> Rectangle<i32, Logical> {
        Rectangle::new(Point::from((0, 0)), Size::from((1280, 800)))
    }

    #[test]
    fn a_menu_is_clamped_onto_the_output() {
        let panel = Size::from((156, 200));
        // Well inside: unchanged.
        assert_eq!(
            WindowMenu::clamp(Point::from((100, 100)), panel, area()),
            Point::from((100, 100))
        );
        // Off the right and bottom edges: pulled back so the whole panel fits.
        assert_eq!(
            WindowMenu::clamp(Point::from((1270, 790)), panel, area()),
            Point::from((1280 - 156, 800 - 200))
        );
        // Off the top-left: pushed back to the area's origin.
        assert_eq!(
            WindowMenu::clamp(Point::from((-50, -50)), panel, area()),
            Point::from((0, 0))
        );
    }

    /// The same rows a real menu has, built without needing a window.
    fn entries() -> Vec<Entry> {
        vec![
            Entry::item(MenuAction::Restore, "Restore", false),
            Entry::item(MenuAction::Move, "Move", true),
            Entry::separator(),
            Entry::item(MenuAction::Close, "Close", true),
        ]
    }

    #[test]
    fn rows_stack_in_order_inside_the_panel() {
        let origin = Point::from((10, 20));
        let rows = entries();
        let panel = panel_rect(&rows, origin);
        assert_eq!(panel.loc, origin);
        // Two items, a separator and one more item, plus the top and bottom margins.
        assert_eq!(panel.size.h, ITEM_H * 3 + SEPARATOR_H + 2 * MARGIN);

        let first = row_rect(&rows, origin, 0);
        let second = row_rect(&rows, origin, 1);
        assert_eq!(
            first.loc,
            Point::from((origin.x + MARGIN, origin.y + MARGIN))
        );
        assert_eq!(second.loc.y, first.loc.y + ITEM_H);
        // Every row sits within the panel's width.
        for index in 0..rows.len() {
            let row = row_rect(&rows, origin, index);
            assert!(row.loc.x >= panel.loc.x);
            assert!(row.loc.x + row.size.w <= panel.loc.x + panel.size.w);
        }
        // The separator is shorter than an item row.
        assert_eq!(row_rect(&rows, origin, 2).size.h, SEPARATOR_H);
    }
}
