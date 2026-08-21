// SPDX-License-Identifier: GPL-3.0-or-later
//! The minimized-window icon grid.
//!
//! A minimized window is held out of the `Space` (see [`crate::window_ops::minimize_window`])
//! and shown instead as a small 4Dwm icon tile on the desktop of the primary output. Tiles fill
//! a grid left-to-right and top-to-bottom, IRIX-style: a newly minimized window takes the first
//! free cell, and prefers the cell it last held if that is still free, so restoring and
//! re-minimizing a window keeps it in place. The user can drag a tile to another cell to
//! rearrange, and a single click restores the window.
//!
//! The drawing (tile quads, label) lives in [`crate::decoration`] and [`crate::render`]; this
//! module owns the layout, the per-window cell assignment, and the pointer interaction.

use smithay::{
    desktop::Window,
    utils::{Logical, Point, Rectangle},
};

use crate::{Wlrix, decoration, desks};

/// How far the pointer must move after pressing a tile before it counts as a drag rather than a
/// click. Below this, a press-release restores the window.
const DRAG_THRESHOLD: f64 = 6.0;

/// Grid columns the compositor leaves alone when it picks a cell itself.
///
/// The toolchest opens at the top left and sits over roughly this much of the grid, so a window
/// minimized into the first free cell would land underneath it. A count rather than the
/// toolchest's real geometry: the grid is fixed-size cells and the toolchest is a fixed-width
/// panel, so a column count says the same thing without the icons having to be re-laid-out every
/// time that window moves or closes.
///
/// Only *assignment* honors this. The user can still drag a tile into these columns, and one
/// dropped there keeps the cell across a restore and re-minimize -- that is a choice, and the
/// remembered cell exists to respect choices.
const RESERVED_COLS: i32 = 2;

/// What ends a tile move. Mirrors [`crate::grabs::move_grab::MoveEnd`], which does the same job
/// for a window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IconMove {
    /// Press-drag-release: a click restores the window, a drag past [`DRAG_THRESHOLD`] moves the
    /// tile, and letting go drops it.
    Drag,
    /// Chosen from the icon's window menu: no button is held, so the tile follows the pointer
    /// until the next click drops it.
    NextClick,
}

/// An in-progress tile move: the window whose tile is being moved, where in the tile the press
/// landed (so it tracks the pointer without jumping), the current pointer position, and whether
/// the pointer has yet moved far enough to be a drag rather than a click.
pub struct IconDrag {
    pub window: Window,
    /// Pointer offset from the tile's origin at press time.
    grab_offset: Point<f64, Logical>,
    /// Where the press started, to measure the drag threshold against.
    press: Point<f64, Logical>,
    /// The latest pointer position.
    current: Point<f64, Logical>,
    moved: bool,
    end: IconMove,
}

impl IconDrag {
    /// The tile's top-left while moving. The offset is measured from where the move began, so the
    /// tile travels with the pointer rather than jumping to it.
    pub fn tile_origin(&self) -> Point<i32, Logical> {
        (self.current - self.grab_offset).to_i32_round()
    }

    /// Whether the tile is being moved rather than clicked. A menu-driven move is a move from the
    /// outset -- there is no button held to distinguish a click from a drag.
    pub fn is_drag(&self) -> bool {
        self.moved || self.end == IconMove::NextClick
    }
}

/// The icon grid laid over a work area: fixed-size tiles from the top-left, wrapping to a new
/// row when the width runs out.
pub struct Grid {
    area: Rectangle<i32, Logical>,
    cols: i32,
}

impl Grid {
    /// The grid for a work `area`. At least one column, however narrow the output.
    pub fn new(area: Rectangle<i32, Logical>) -> Self {
        let usable = area.size.w - 2 * decoration::ICON_MARGIN;
        let cols = (usable / decoration::ICON_TILE_W).max(1);
        Self { area, cols }
    }

    /// The tile rectangle for a slot index, row-major from the top-left.
    pub fn slot_rect(&self, slot: usize) -> Rectangle<i32, Logical> {
        let slot = slot as i32;
        let col = slot % self.cols;
        let row = slot / self.cols;
        let x = self.area.loc.x + decoration::ICON_MARGIN + col * decoration::ICON_TILE_W;
        let y = self.area.loc.y + decoration::ICON_MARGIN + row * decoration::ICON_TILE_H;
        Rectangle::new(
            (x, y).into(),
            (decoration::ICON_TILE_W, decoration::ICON_TILE_H).into(),
        )
    }

    /// How many columns to skip when assigning, given how wide this grid is.
    ///
    /// Nothing is reserved on a grid too narrow to spare the columns. An output that can only
    /// fit two columns of tiles should still be able to minimize a window somewhere -- and with
    /// no assignable cell at all, the search for one would never end.
    fn reserved_cols(&self) -> i32 {
        if self.cols > RESERVED_COLS {
            RESERVED_COLS
        } else {
            0
        }
    }

    /// Whether the compositor may put a newly minimized window in `slot` of its own accord.
    pub fn is_assignable(&self, slot: usize) -> bool {
        (slot as i32 % self.cols) >= self.reserved_cols()
    }

    /// The cell a newly minimized window should take: the lowest free one the compositor is
    /// allowed to assign, which is never in a reserved column.
    ///
    /// Terminates because `reserved_cols` leaves at least one assignable column in every row,
    /// and the rows go on for ever -- the grid is not clipped to the bottom of the screen.
    pub fn first_assignable(&self, occupied: &[usize]) -> usize {
        (0..)
            .find(|slot| !occupied.contains(slot) && self.is_assignable(*slot))
            .expect("every row has an assignable column")
    }

    /// The slot whose tile contains `point`, if any. Tiles butt against one another, so every
    /// point from the margin rightwards and downwards is on some tile until the columns run out.
    pub fn slot_at(&self, point: Point<f64, Logical>) -> Option<usize> {
        let local_x = point.x - (self.area.loc.x + decoration::ICON_MARGIN) as f64;
        let local_y = point.y - (self.area.loc.y + decoration::ICON_MARGIN) as f64;
        if local_x < 0.0 || local_y < 0.0 {
            return None;
        }
        let col = (local_x / decoration::ICON_TILE_W as f64) as i32;
        let row = (local_y / decoration::ICON_TILE_H as f64) as i32;
        if col >= self.cols {
            return None;
        }
        Some((row * self.cols + col) as usize)
    }
}

impl Wlrix {
    /// The output the icon grid lives on: the primary (first) output.
    fn icon_output(&self) -> Option<smithay::output::Output> {
        self.space.outputs().next().cloned()
    }

    /// The icon grid over the primary output's work area, if there is an output.
    pub fn icon_grid(&self) -> Option<Grid> {
        let output = self.icon_output()?;
        Some(Grid::new(crate::placement::work_area(&self.space, &output)))
    }

    /// The minimized windows currently shown as icons -- those on a visible desk (the active
    /// desk or the global desk) -- each with the slot it occupies. Order is by slot.
    pub fn minimized_icons(&self) -> Vec<(Window, usize)> {
        let active = self.desks.active();
        let mut icons: Vec<(Window, usize)> = self
            .desks
            .hidden()
            .iter()
            .filter(|w| {
                let state = desks::window_state(w).borrow();
                let desk = state.desk;
                state.minimized && (desk == active || desk.is_global())
            })
            .filter_map(|w| {
                desks::window_state(w)
                    .borrow()
                    .icon_slot
                    .map(|slot| (w.clone(), slot))
            })
            .collect();
        icons.sort_by_key(|(_, slot)| *slot);
        icons
    }

    /// The slots taken by the currently shown icons, except `exclude`.
    fn occupied_slots(&self, exclude: &Window) -> Vec<usize> {
        self.minimized_icons()
            .into_iter()
            .filter(|(w, _)| w != exclude)
            .map(|(_, slot)| slot)
            .collect()
    }

    /// Give `window` a grid cell as it is minimized: its remembered cell if still free, else the
    /// lowest free cell. Stored on the window so a later re-minimize can prefer it again.
    pub fn assign_icon_slot(&mut self, window: &Window) {
        let occupied = self.occupied_slots(window);
        let preferred = desks::window_state(window).borrow().icon_slot;
        let grid = self.icon_grid();
        let slot = match preferred {
            // A cell the window already had is kept whatever column it is in, including one the
            // user dragged it into.
            Some(slot) if !occupied.contains(&slot) => slot,
            // A fresh cell skips the columns the toolchest sits over; see `RESERVED_COLS`. With
            // no grid there is no output, and so no column count to skip by.
            _ => match &grid {
                Some(grid) => grid.first_assignable(&occupied),
                None => (0..)
                    .find(|slot| !occupied.contains(slot))
                    .expect("0.. is infinite"),
            },
        };
        desks::window_state(window).borrow_mut().icon_slot = Some(slot);
    }

    /// The minimized window whose tile is under `point` on the primary output, if any.
    pub fn icon_under(&self, point: Point<f64, Logical>) -> Option<Window> {
        self.icon_in_grid(&self.icon_grid()?, point)
    }

    /// The same, against a grid the caller already has.
    ///
    /// [`Wlrix::surface_under`] needs this: building the grid goes through the work area, which
    /// locks the output's layer map, and that caller is holding the very same non-reentrant
    /// guard. It builds its grid from the guard it holds and asks here instead.
    pub fn icon_in_grid(&self, grid: &Grid, point: Point<f64, Logical>) -> Option<Window> {
        let slot = grid.slot_at(point)?;
        self.minimized_icons()
            .into_iter()
            .find(|(_, s)| *s == slot)
            .map(|(w, _)| w)
    }

    /// The window whose tile is following the pointer, and where that tile is right now. `None`
    /// until a press has passed [`DRAG_THRESHOLD`] -- until then the tile is still in its cell.
    pub fn dragged_icon(&self) -> Option<(Window, Rectangle<i32, Logical>)> {
        let drag = self.icon_drag.as_ref().filter(|drag| drag.is_drag())?;
        Some((
            drag.window.clone(),
            Rectangle::new(
                drag.tile_origin(),
                (decoration::ICON_TILE_W, decoration::ICON_TILE_H).into(),
            ),
        ))
    }

    /// Begin a press on a minimized icon: record it so motion can turn into a drag and release
    /// can restore or drop it. `point` is the pointer position at press.
    pub fn press_icon(&mut self, window: &Window, point: Point<f64, Logical>) {
        self.begin_icon_move(window, point, IconMove::Drag);
    }

    /// Start a menu-driven tile move: with no button held, the tile follows the pointer until the
    /// next click drops it. The icon counterpart of [`Wlrix::start_menu_move`].
    pub fn start_menu_icon_move(&mut self, window: &Window, point: Point<f64, Logical>) {
        self.begin_icon_move(window, point, IconMove::NextClick);
    }

    /// Begin moving `window`'s tile from `point`, ending as `end` says.
    fn begin_icon_move(&mut self, window: &Window, point: Point<f64, Logical>, end: IconMove) {
        let slot = desks::window_state(window).borrow().icon_slot.unwrap_or(0);
        let Some(grid) = self.icon_grid() else {
            return;
        };
        let tile = grid.slot_rect(slot);
        self.icon_drag = Some(IconDrag {
            window: window.clone(),
            grab_offset: point - tile.loc.to_f64(),
            press: point,
            current: point,
            moved: false,
            end,
        });
        self.request_redraw();
    }

    /// Whether a tile is mid-move waiting for a click to drop it.
    pub fn icon_move_awaits_click(&self) -> bool {
        self.icon_drag
            .as_ref()
            .is_some_and(|drag| drag.end == IconMove::NextClick)
    }

    /// Update a tile drag as the pointer moves. No-op if no tile is being dragged.
    pub fn drag_icon(&mut self, point: Point<f64, Logical>) {
        let Some(drag) = self.icon_drag.as_mut() else {
            return;
        };
        drag.current = point;
        let delta = point - drag.press;
        if delta.x.hypot(delta.y) > DRAG_THRESHOLD {
            drag.moved = true;
        }
        self.request_redraw();
    }

    /// Finish a tile press: a click (no drag) restores the window; a drag drops it on the target
    /// cell. `point` is the release position.
    ///
    /// A menu-driven move ignores releases -- including the release of the very click that chose
    /// "Move" -- and is ended by [`Wlrix::drop_icon`] on the next press instead.
    pub fn release_icon(&mut self, point: Point<f64, Logical>) {
        if self.icon_move_awaits_click() {
            return;
        }
        let Some(drag) = self.icon_drag.take() else {
            return;
        };
        if !drag.is_drag() {
            self.restore_window(&drag.window);
            self.request_redraw();
            return;
        }
        self.place_icon(drag, point);
    }

    /// Drop a tile that has been following the pointer, on the click that ends its move.
    pub fn drop_icon(&mut self, point: Point<f64, Logical>) {
        let Some(drag) = self.icon_drag.take() else {
            return;
        };
        self.place_icon(drag, point);
    }

    /// Put a moved tile down: it takes the cell under its center, swapping with whatever tile is
    /// already there.
    fn place_icon(&mut self, mut drag: IconDrag, point: Point<f64, Logical>) {
        drag.current = point;
        if let Some(grid) = self.icon_grid() {
            let centre = drag.tile_origin().to_f64()
                + Point::from((
                    decoration::ICON_TILE_W as f64 / 2.0,
                    decoration::ICON_TILE_H as f64 / 2.0,
                ));
            if let Some(target) = grid.slot_at(centre) {
                self.move_icon_to_slot(&drag.window, target);
            }
        }
        self.request_redraw();
    }

    /// Move `window`'s tile to `target`, swapping cells with any window already there so no two
    /// tiles share a cell.
    fn move_icon_to_slot(&mut self, window: &Window, target: usize) {
        let from = desks::window_state(window).borrow().icon_slot;
        let occupant = self
            .minimized_icons()
            .into_iter()
            .find(|(w, slot)| *slot == target && w != window)
            .map(|(w, _)| w);
        if let (Some(occupant), Some(from)) = (occupant.as_ref(), from) {
            desks::window_state(occupant).borrow_mut().icon_slot = Some(from);
        }
        desks::window_state(window).borrow_mut().icon_slot = Some(target);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A work area wide enough for four tiles across, plus the margins either side.
    fn grid() -> Grid {
        let width = 2 * decoration::ICON_MARGIN + 4 * decoration::ICON_TILE_W;
        Grid::new(Rectangle::new((0, 0).into(), (width, 600).into()))
    }

    #[test]
    fn columns_fit_the_work_area_width() {
        assert_eq!(grid().cols, 4);
        // A hair narrower than two tiles still leaves one column, never zero.
        let narrow = Grid::new(Rectangle::new((0, 0).into(), (10, 600).into()));
        assert_eq!(narrow.cols, 1);
    }

    #[test]
    fn slots_wrap_left_to_right_then_down() {
        let g = grid();
        let first = g.slot_rect(0);
        assert_eq!(
            first.loc,
            (decoration::ICON_MARGIN, decoration::ICON_MARGIN).into()
        );
        // Slot 3 is the last in row 0; slot 4 wraps to the start of row 1.
        assert_eq!(g.slot_rect(3).loc.y, g.slot_rect(0).loc.y);
        assert_eq!(g.slot_rect(4).loc.x, first.loc.x);
        assert_eq!(g.slot_rect(4).loc.y, first.loc.y + decoration::ICON_TILE_H);
    }

    /// The toolchest sits over the first two columns, so the compositor never puts a tile there
    /// on its own. Every row is affected, not just the first -- the reserved columns are a strip
    /// down the left of the grid, not a prefix of the slot numbering.
    #[test]
    fn the_left_two_columns_are_not_assigned() {
        let g = grid();
        for slot in [0usize, 1, 4, 5, 8, 9] {
            assert!(
                !g.is_assignable(slot),
                "slot {slot} is in a reserved column"
            );
        }
        for slot in [2usize, 3, 6, 7, 10, 11] {
            assert!(g.is_assignable(slot), "slot {slot} should be assignable");
        }
    }

    /// What minimizing a run of windows actually produces on a four-column grid: the third and
    /// fourth cells of each row, skipping the pair the toolchest covers.
    #[test]
    fn windows_are_assigned_down_the_free_columns() {
        let g = grid();
        let mut occupied: Vec<usize> = Vec::new();
        for _ in 0..6 {
            let slot = g.first_assignable(&occupied);
            occupied.push(slot);
        }
        assert_eq!(occupied, [2, 3, 6, 7, 10, 11]);
    }

    /// A gap left by a restored window is filled before the grid grows, as it was before -- the
    /// reservation changes which cells count, not the first-free rule.
    #[test]
    fn a_freed_cell_is_reused_before_a_new_row() {
        let g = grid();
        assert_eq!(g.first_assignable(&[2, 6, 7]), 3);
        assert_eq!(g.first_assignable(&[2, 3, 7]), 6);
    }

    /// The user can still drop a tile there by hand: only assignment is restricted, and
    /// hit-testing has to keep finding those cells or the tile could not be dropped or clicked.
    #[test]
    fn reserved_columns_are_still_reachable_by_hand() {
        let g = grid();
        for slot in [0usize, 1, 4] {
            let tile = g.slot_rect(slot);
            let centre = tile.loc.to_f64()
                + Point::from((
                    decoration::ICON_TILE_W as f64 / 2.0,
                    decoration::ICON_TILE_H as f64 / 2.0,
                ));
            assert_eq!(g.slot_at(centre), Some(slot));
        }
    }

    /// A grid with no room to spare reserves nothing. Otherwise there would be no assignable
    /// cell at all, and the search for one -- which walks `0..` -- would never end.
    #[test]
    fn a_narrow_grid_reserves_nothing() {
        for width in [10, 2 * decoration::ICON_TILE_W + 40] {
            let g = Grid::new(Rectangle::new((0, 0).into(), (width, 600).into()));
            assert!(g.cols <= RESERVED_COLS, "{width}px gave {} columns", g.cols);
            assert!(g.is_assignable(0), "{width}px should still assign slot 0");
        }
        // One column past the reserved pair is enough to start reserving.
        let width = 2 * decoration::ICON_MARGIN + 3 * decoration::ICON_TILE_W;
        let g = Grid::new(Rectangle::new((0, 0).into(), (width, 600).into()));
        assert_eq!(g.cols, 3);
        assert!(!g.is_assignable(0) && !g.is_assignable(1) && g.is_assignable(2));
    }

    #[test]
    fn slot_at_is_the_inverse_of_slot_rect() {
        let g = grid();
        for slot in [0usize, 1, 3, 4, 7] {
            let tile = g.slot_rect(slot);
            let centre = tile.loc.to_f64()
                + Point::from((
                    decoration::ICON_TILE_W as f64 / 2.0,
                    decoration::ICON_TILE_H as f64 / 2.0,
                ));
            assert_eq!(g.slot_at(centre), Some(slot));
        }
    }

    #[test]
    fn the_margin_is_not_a_slot() {
        let g = grid();
        assert_eq!(g.slot_at((1.0, 1.0).into()), None);
    }

    /// Tiles butt against each other, so the column boundary is the only thing between them --
    /// the last pixel of one tile and the first of the next are both live.
    #[test]
    fn adjacent_tiles_have_no_gap_between_them() {
        let g = grid();
        let mid_y = (decoration::ICON_MARGIN + decoration::ICON_TILE_H / 2) as f64;
        let boundary = (decoration::ICON_MARGIN + decoration::ICON_TILE_W) as f64;
        assert_eq!(g.slot_rect(1).loc.x, boundary as i32);
        assert_eq!(g.slot_at((boundary - 0.5, mid_y).into()), Some(0));
        assert_eq!(g.slot_at((boundary, mid_y).into()), Some(1));
    }
}
