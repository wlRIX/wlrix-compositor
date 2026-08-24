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
/// The narrowest the panel is drawn -- the width 4Dwm's menu had, and what a menu with no
/// accelerators to print still gets.
///
/// It is a floor, not the width: the accelerator column pushes past it on a stock
/// configuration, because `Maximize` and `Alt+F10` together need more room than a 156px panel
/// has between its insets. Measured, so the panel comes out at 177px there. See
/// [`measure_width`].
const MIN_WIDTH: i32 = 156;
/// Margin between the panel edge and the item rows.
const MARGIN: i32 = 3;
/// Inset of an item's label from the left of its row, and of its accelerator from the right.
pub const LABEL_INSET: i32 = 14;
/// The least space left between a label and the accelerator printed after it. Motif menus keep
/// the two columns visibly apart rather than letting a long label run up against its key.
const ACCEL_GAP: i32 = 24;
/// Label size in logical pixels.
pub const LABEL_PX: f32 = 14.0;

/// What choosing an item does.
///
/// `Hash` so [`crate::keybinds::Bindings`] can key the menu's accelerator column by it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MenuAction {
    /// Un-minimize a minimized window, or return a maximized one to its previous size.
    Restore,
    /// Move the window with the pointer until the next click.
    Move,
    /// Resize the window with the pointer, from the edge it is nearest.
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
    /// The key combination bound to this item, printed right-aligned after the label. `None`
    /// when nothing is bound to the action -- which a user can arrange, so this is not the
    /// unreachable case it looks like.
    pub accel: Option<String>,
    pub enabled: bool,
    height: i32,
}

impl Entry {
    fn separator() -> Self {
        Self {
            action: None,
            label: "",
            accel: None,
            enabled: false,
            height: SEPARATOR_H,
        }
    }

    fn item(
        action: MenuAction,
        label: &'static str,
        enabled: bool,
        bindings: &crate::keybinds::Bindings,
    ) -> Self {
        Self {
            action: Some(action),
            label,
            accel: bindings.menu_combo(action).map(|combo| combo.to_string()),
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
    /// The panel's width, worked out once when the menu is built and stored.
    ///
    /// Measured, because the accelerator column is as wide as whatever the user has bound, and
    /// a long combination printed into a fixed 156px would run into the label beside it. Stored
    /// rather than recomputed so that hit-testing still needs no font metrics: `row_at` reads
    /// this number, and the measuring happens once, at post time, where the text renderer is
    /// already to hand.
    width: i32,
    pub entries: Vec<Entry>,
    /// The row the pointer is over, if it is over a choosable one.
    pub hovered: Option<usize>,
}

impl WindowMenu {
    /// Build the menu for `window`, with each item enabled according to the window's state and
    /// labeled with whatever is currently bound to it.
    ///
    /// The origin given is a preference; it is not clamped onto an output here, because the
    /// panel's width is not known until the rows have been measured. [`Wlrix::open_window_menu`]
    /// builds, then clamps with [`WindowMenu::place`].
    pub fn new(
        window: &Window,
        origin: Point<i32, Logical>,
        bindings: &crate::keybinds::Bindings,
        text: &mut crate::text::TextRenderer,
    ) -> Self {
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
            Entry::item(
                MenuAction::Restore,
                "Restore",
                minimized || maximized,
                bindings,
            ),
            Entry::item(MenuAction::Move, "Move", true, bindings),
            // A minimized window is an icon in the grid: it can be moved there, but there is
            // no frame to size. Otherwise it comes down to whether any axis can move at all.
            Entry::item(
                MenuAction::Size,
                "Size",
                !minimized && capabilities.resizable.any(),
                bindings,
            ),
            Entry::item(
                MenuAction::Minimize,
                "Minimize",
                !minimized && capabilities.minimizable,
                bindings,
            ),
            Entry::item(
                MenuAction::Maximize,
                "Maximize",
                !maximized && capabilities.maximizable,
                bindings,
            ),
            Entry::item(MenuAction::Raise, "Raise", true, bindings),
            Entry::item(MenuAction::Lower, "Lower", true, bindings),
            Entry::separator(),
            Entry::item(MenuAction::Close, "Close", true, bindings),
        ];
        let width = measure_width(&entries, text);
        Self {
            window: window.clone(),
            origin,
            width,
            entries,
            hovered: None,
        }
    }

    /// Move the built panel to `origin`, which the caller has clamped onto an output now that
    /// the width is known.
    pub fn place(&mut self, origin: Point<i32, Logical>) {
        self.origin = origin;
    }

    /// The panel rectangle, bevel included.
    pub fn panel(&self) -> Rectangle<i32, Logical> {
        panel_rect(&self.entries, self.origin, self.width)
    }

    /// The rectangle of row `index`, inset from the panel edges.
    pub fn row(&self, index: usize) -> Rectangle<i32, Logical> {
        row_rect(&self.entries, self.origin, self.width, index)
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
        // Built first, then placed: the panel is as wide as its widest row, and that is not
        // known until the accelerators have been measured.
        let mut menu = WindowMenu::new(window, at, &self.keybinds, &mut self.text_renderer);
        if let Some(output) = output {
            let area = crate::placement::work_area(&self.space, &output);
            menu.place(WindowMenu::clamp(at, menu.panel().size, area));
        }
        self.window_menu = Some(menu);
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
            // The menu draws this disabled for a minimized window and for one that cannot
            // resize, but a keybind reaches the same action without a menu to gray out --
            // `start_menu_resize` refuses both cases itself.
            MenuAction::Size => {
                if !minimized {
                    self.start_menu_resize(window, serial);
                }
            }
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
fn panel_rect(
    entries: &[Entry],
    origin: Point<i32, Logical>,
    width: i32,
) -> Rectangle<i32, Logical> {
    let height: i32 = entries.iter().map(|entry| entry.height).sum();
    Rectangle::new(origin, Size::from((width, height + 2 * MARGIN)))
}

/// The rectangle of row `index`, inset from the panel edges.
fn row_rect(
    entries: &[Entry],
    origin: Point<i32, Logical>,
    width: i32,
    index: usize,
) -> Rectangle<i32, Logical> {
    let top: i32 = entries[..index].iter().map(|entry| entry.height).sum();
    Rectangle::new(
        Point::from((origin.x + MARGIN, origin.y + MARGIN + top)),
        Size::from((width - 2 * MARGIN, entries[index].height)),
    )
}

/// The panel width these rows need: wide enough that no label runs into the accelerator
/// printed after it, and never narrower than [`MIN_WIDTH`].
///
/// Measuring means rasterizing, which is why the text renderer is threaded in. It is cached by
/// (string, size, color), and these strings are drawn a moment later anyway, so only the first
/// menu of a session pays for it -- and only again when a binding changes the text.
///
/// Measured at [`LABEL_PX`] rather than at the output's scaled size, because this is a logical
/// width. A fractional scale makes it approximate; [`ACCEL_GAP`] is the slack that absorbs
/// that, and erring wide only leaves a slightly roomier menu.
fn measure_width(entries: &[Entry], text: &mut crate::text::TextRenderer) -> i32 {
    let mut measure = |string: &str| text.measure(string, LABEL_PX);
    let widest = entries
        .iter()
        .filter_map(|entry| {
            let accel = entry.accel.as_deref()?;
            Some(measure(entry.label) + ACCEL_GAP + measure(accel))
        })
        .max()
        .unwrap_or(0);
    // The row is inset from the panel by the margin, and the text from the row by the label
    // inset -- at both ends, the label from the left and the accelerator from the right.
    MIN_WIDTH.max(widest + 2 * LABEL_INSET + 2 * MARGIN)
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
        let bindings = crate::keybinds::Bindings::default();
        vec![
            Entry::item(MenuAction::Restore, "Restore", false, &bindings),
            Entry::item(MenuAction::Move, "Move", true, &bindings),
            Entry::separator(),
            Entry::item(MenuAction::Close, "Close", true, &bindings),
        ]
    }

    #[test]
    fn rows_stack_in_order_inside_the_panel() {
        let origin = Point::from((10, 20));
        let rows = entries();
        let panel = panel_rect(&rows, origin, MIN_WIDTH);
        assert_eq!(panel.loc, origin);
        // Two items, a separator and one more item, plus the top and bottom margins.
        assert_eq!(panel.size.h, ITEM_H * 3 + SEPARATOR_H + 2 * MARGIN);

        let first = row_rect(&rows, origin, MIN_WIDTH, 0);
        let second = row_rect(&rows, origin, MIN_WIDTH, 1);
        assert_eq!(
            first.loc,
            Point::from((origin.x + MARGIN, origin.y + MARGIN))
        );
        assert_eq!(second.loc.y, first.loc.y + ITEM_H);
        // Every row sits within the panel's width.
        for index in 0..rows.len() {
            let row = row_rect(&rows, origin, MIN_WIDTH, index);
            assert!(row.loc.x >= panel.loc.x);
            assert!(row.loc.x + row.size.w <= panel.loc.x + panel.size.w);
        }
        // The separator is shorter than an item row.
        assert_eq!(row_rect(&rows, origin, MIN_WIDTH, 2).size.h, SEPARATOR_H);
    }

    /// Each item carries whatever is bound to it, and a separator carries nothing.
    #[test]
    fn items_are_labelled_with_the_key_bound_to_them() {
        let rows = entries();
        assert_eq!(rows[0].accel.as_deref(), Some("Alt+F5"), "Restore");
        assert_eq!(rows[1].accel.as_deref(), Some("Alt+F7"), "Move");
        assert!(rows[2].is_separator());
        assert_eq!(rows[2].accel, None, "a separator has no accelerator");
        assert_eq!(rows[3].accel.as_deref(), Some("Alt+F4"), "Close");
    }

    /// What the menu prints follows the config, not the built-in table -- the whole point of
    /// looking the combination up rather than writing it beside the label.
    #[test]
    fn a_rebound_item_prints_its_new_key() {
        let combo = |text: &str| text.parse::<crate::keybinds::Combo>().unwrap();
        let close = crate::keybinds::Action::Window(MenuAction::Close);
        let bindings = crate::keybinds::Bindings::resolve(&[
            (combo("Alt+F4"), None),
            (combo("Ctrl+Shift+W"), Some(close)),
        ]);
        let entry = Entry::item(MenuAction::Close, "Close", true, &bindings);
        assert_eq!(entry.accel.as_deref(), Some("Ctrl+Shift+w"));
    }

    /// An action the user has unbound outright gets no accelerator, rather than an empty one
    /// or the default it no longer has.
    #[test]
    fn an_unbound_item_prints_no_key() {
        let combo = "Alt+F4".parse::<crate::keybinds::Combo>().unwrap();
        let bindings = crate::keybinds::Bindings::resolve(&[(combo, None)]);
        let entry = Entry::item(MenuAction::Close, "Close", true, &bindings);
        assert_eq!(entry.accel, None);
    }

    /// Every row of the *real* menu -- not the short set the geometry tests use -- laid out
    /// with the default bindings.
    ///
    /// `Entry::item` rather than `WindowMenu::new`, which needs a mapped `Window`.
    fn every_item() -> Vec<Entry> {
        let bindings = crate::keybinds::Bindings::default();
        [
            (MenuAction::Restore, "Restore"),
            (MenuAction::Move, "Move"),
            (MenuAction::Size, "Size"),
            (MenuAction::Minimize, "Minimize"),
            (MenuAction::Maximize, "Maximize"),
            (MenuAction::Raise, "Raise"),
            (MenuAction::Lower, "Lower"),
            (MenuAction::Close, "Close"),
        ]
        .into_iter()
        .map(|(action, label)| Entry::item(action, label, true, &bindings))
        .collect()
    }

    fn measure(text: &mut crate::text::TextRenderer, string: &str) -> i32 {
        text.measure(string, LABEL_PX)
    }

    /// Every row gets its label, its gap and its whole accelerator inside the panel.
    ///
    /// This is the property that matters, and it is *not* "the defaults fit 156px" -- they do
    /// not. `Maximize` and `Alt+F10` are the widest pair and need more than the classic panel
    /// has between its insets, which is what [`measure_width`] is for. Asserting a fixed number
    /// here would only pin down today's font.
    #[test]
    fn every_default_item_fits_the_panel_it_is_measured_for() {
        let Ok(mut text) = crate::text::TextRenderer::new() else {
            return; // no fonts installed
        };
        let rows = every_item();
        let width = measure_width(&rows, &mut text);
        assert!(
            width >= MIN_WIDTH,
            "the panel never shrinks below {MIN_WIDTH}"
        );

        let available = width - 2 * LABEL_INSET - 2 * MARGIN;
        for row in &rows {
            let label = measure(&mut text, row.label);
            let accel = measure(&mut text, row.accel.as_deref().expect("a default is bound"));
            assert!(
                label + ACCEL_GAP + accel <= available,
                "{:?} + {:?} needs {} of {available}",
                row.label,
                row.accel,
                label + ACCEL_GAP + accel
            );
        }
    }

    /// With nothing bound at all there is no accelerator column, and the panel is the width it
    /// was before any of this existed.
    #[test]
    fn a_menu_with_no_bindings_is_the_classic_width() {
        let Ok(mut text) = crate::text::TextRenderer::new() else {
            return; // no fonts installed
        };
        let unbound: Vec<Entry> = every_item()
            .into_iter()
            .map(|mut entry| {
                entry.accel = None;
                entry
            })
            .collect();
        assert_eq!(measure_width(&unbound, &mut text), MIN_WIDTH);
    }

    /// A long combination widens the panel rather than colliding with the label beside it.
    #[test]
    fn a_long_binding_widens_the_panel() {
        let Ok(mut text) = crate::text::TextRenderer::new() else {
            return; // no fonts installed
        };
        let combo = |text: &str| text.parse::<crate::keybinds::Combo>().unwrap();
        let bindings = crate::keybinds::Bindings::resolve(&[(
            combo("Ctrl+Alt+Shift+Super+BackSpace"),
            Some(crate::keybinds::Action::Window(MenuAction::Minimize)),
        )]);
        let rows = vec![Entry::item(
            MenuAction::Minimize,
            "Minimize",
            true,
            &bindings,
        )];
        let width = measure_width(&rows, &mut text);
        assert!(
            width > MIN_WIDTH,
            "a panel of {width} should have grown past {MIN_WIDTH}"
        );
    }
}
