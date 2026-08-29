// SPDX-License-Identifier: GPL-3.0-or-later
//! Global keybinds: the table that turns a key combination into a compositor action.
//!
//! ```toml
//! # ~/.config/wlrix/compositor.toml
//! [keybinds]
//! "Ctrl+Q" = "close"                 # add a binding
//! "Ctrl+Alt+BackSpace" = "none"      # take a default away
//! "Super+3" = "switch-desk 3"
//! ```
//!
//! Unlike the config *file*, which is used whole rather than merged, the entries in this
//! section are applied **over** [`DEFAULTS`]: a default stays bound unless the user's file
//! names the same combination, either to rebind it or to switch it off with `none`. Making
//! this section replace the defaults outright would mean re-listing eight window bindings to
//! add a ninth, which is not a trade anyone would want to make twice.
//!
//! The defaults are 4Dwm's: the window menu's eight items on Alt+F*n*, in the same order the
//! menu lists them. The rest are what the compositor had hard-coded before this module
//! existed -- see [`DEFAULTS`] for which of those are meant to be retired.
//!
//! **Ctrl+Alt+F*n* is not here.** VT switching reaches the compositor as an
//! `XF86Switch_VT_n` keysym rather than as a combination, and it is the way back to a login
//! prompt when the desktop has wedged; [`crate::input`] handles it ahead of this table and
//! does not let it be rebound.

use std::{collections::HashMap, fmt, str::FromStr};

use smithay::input::keyboard::{Keysym, KeysymHandle, ModifiersState, xkb};

use crate::menu::MenuAction;

/// The built-in bindings, as they would be written in the config file.
///
/// Parsed rather than constructed, so the table reads the way a user's own section does and
/// the parser is exercised by the same test that checks the table
/// (`every_default_parses`). A malformed entry here is a failing test, not a surprise at
/// startup.
///
/// The first block is the window menu, keyed exactly as 4Dwm keyed it. The second is the
/// session. The third is scaffolding from before the `wlrix-desks` protocol drove desks:
/// retiring those is now an edit to this table rather than a change to the input code, and
/// `Ctrl+Alt+BackSpace` goes the same way once the environment is stable enough not to need
/// a way out.
pub const DEFAULTS: &[(&str, &str)] = &[
    // The window menu, top to bottom.
    ("Alt+F5", "restore"),
    ("Alt+F7", "move"),
    ("Alt+F8", "size"),
    ("Alt+F9", "minimize"),
    ("Alt+F10", "maximize"),
    ("Alt+F1", "raise"),
    ("Alt+F3", "lower"),
    ("Alt+F4", "close"),
    // Screenshots. `Print` is what libxkbcommon calls the key and what `xev` prints for it.
    // These spawn `wlrix-screenshot`; the shapes match what every desktop binds Print to, so
    // they need no learning.
    ("Print", "screenshot"),
    ("Alt+Print", "screenshot-window"),
    ("Shift+Print", "screenshot-screen"),
    // Session. Temporary: to be dropped from this table as the desktop stabilizes.
    ("Ctrl+Alt+BackSpace", "quit"),
    // Keyboard. The compositor's own layout toggle, complementing any `grp:` xkb option --
    // one works from a keybind, the other from a modifier held down.
    ("Super+space", "cycle-layout"),
    // Temporary: desks and window ops, from before `wlrix-desks` drove them.
    ("Super+1", "switch-desk 1"),
    ("Super+2", "switch-desk 2"),
    ("Super+3", "switch-desk 3"),
    ("Super+4", "switch-desk 4"),
    ("Super+5", "switch-desk 5"),
    ("Super+6", "switch-desk 6"),
    ("Super+7", "switch-desk 7"),
    ("Super+8", "switch-desk 8"),
    ("Super+9", "switch-desk 9"),
    ("Super+Ctrl+1", "move-to-desk 1"),
    ("Super+Ctrl+2", "move-to-desk 2"),
    ("Super+Ctrl+3", "move-to-desk 3"),
    ("Super+Ctrl+4", "move-to-desk 4"),
    ("Super+Ctrl+5", "move-to-desk 5"),
    ("Super+Ctrl+6", "move-to-desk 6"),
    ("Super+Ctrl+7", "move-to-desk 7"),
    ("Super+Ctrl+8", "move-to-desk 8"),
    ("Super+Ctrl+9", "move-to-desk 9"),
    ("Super+Shift+Up", "create-desk"),
    ("Super+Shift+Down", "delete-desk"),
    ("Super+f", "maximize-toggle"),
    ("Super+m", "minimize"),
    ("Super+Shift+m", "restore-all"),
    ("Super+l", "lower"),
];

/// A key combination: the modifiers that must be held, and the key itself.
///
/// The modifiers are stored as the four a user can name. `caps_lock` and `num_lock` are
/// deliberately absent -- they are locks rather than held modifiers, and a binding that
/// stopped working because Caps Lock was on would be a mystery to whoever hit it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Combo {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub logo: bool,
    /// The keysym, always normalized through a case-insensitive lookup so that `"Alt+F4"`,
    /// `"alt+f4"` and `"ALT+F4"` are one entry in the table rather than three.
    pub sym: Keysym,
}

impl Combo {
    /// The combination the given modifiers and key would form.
    fn with(mods: &ModifiersState, sym: Keysym) -> Self {
        Self {
            ctrl: mods.ctrl,
            alt: mods.alt,
            shift: mods.shift,
            logo: mods.logo,
            sym,
        }
    }
}

impl FromStr for Combo {
    type Err = String;

    /// Parse `Mod+Mod+Key`.
    ///
    /// Modifier names are matched case-insensitively and space around the separators is
    /// ignored, so `"ctrl + alt + BackSpace"` is the same combination as
    /// `"Ctrl+Alt+BackSpace"`. The key is any name libxkbcommon knows, which is the same set
    /// `xev` prints -- `BackSpace`, `Return`, `Print`, `XF86AudioRaiseVolume` and so on.
    ///
    /// A literal `+` has to be written by its keysym name, `plus`: splitting on the separator
    /// leaves nothing to name the key with otherwise, and `"Ctrl+plus"` is both unambiguous
    /// and what `xev` calls it.
    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let mut combo = Self {
            ctrl: false,
            alt: false,
            shift: false,
            logo: false,
            sym: Keysym::NoSymbol,
        };
        let mut key: Option<&str> = None;

        for part in text.split('+') {
            let part = part.trim();
            if part.is_empty() {
                return Err(format!(
                    "{text:?} has an empty part; write a literal plus as `plus`"
                ));
            }
            match part.to_ascii_lowercase().as_str() {
                "ctrl" | "control" => combo.ctrl = true,
                "alt" => combo.alt = true,
                "shift" => combo.shift = true,
                "super" | "logo" | "meta" | "win" => combo.logo = true,
                // Not a modifier, so it is the key -- and there can only be one.
                _ => {
                    if let Some(first) = key.replace(part) {
                        return Err(format!(
                            "{text:?} names two keys, {first:?} and {part:?}; a binding has one"
                        ));
                    }
                }
            }
        }

        let Some(key) = key else {
            return Err(format!("{text:?} names no key, only modifiers"));
        };
        // Case-insensitive rather than exact-then-insensitive, which is what libxkbcommon's
        // own documentation suggests: the point here is that `m` and `M` must be the *same*
        // table entry, since the shift is spelled out separately as a modifier. A user
        // writing `"Super+Shift+M"` has to be able to unbind the default written `Super+Shift+m`.
        combo.sym = xkb::keysym_from_name(key, xkb::KEYSYM_CASE_INSENSITIVE);
        if combo.sym == Keysym::NoSymbol {
            return Err(format!("{key:?} is not a key name (in {text:?})"));
        }
        Ok(combo)
    }
}

impl fmt::Display for Combo {
    /// The canonical spelling, which is what a message about a binding should quote rather
    /// than whatever the user typed.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (held, name) in [
            (self.ctrl, "Ctrl"),
            (self.alt, "Alt"),
            (self.shift, "Shift"),
            (self.logo, "Super"),
        ] {
            if held {
                write!(f, "{name}+")?;
            }
        }
        f.write_str(&xkb::keysym_get_name(self.sym))
    }
}

/// What a binding does when it fires.
///
/// The window operations are the window menu's own [`MenuAction`] rather than a parallel set,
/// so a binding and the menu item of the same name run the same code and cannot drift apart
/// -- including the details, like `Restore` undoing whichever of minimized and maximized the
/// window is in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// One of the window menu's eight items, applied to the focused window.
    Window(MenuAction),
    /// Maximize the focused window, or un-maximize it if it already is.
    MaximizeToggle,
    /// Bring every minimized window back.
    RestoreAll,
    /// Switch to the desk at this index. Zero-based here; the config file counts from one.
    SwitchDesk(usize),
    /// Send the focused window to the desk at this index. Zero-based, as above.
    MoveToDesk(usize),
    /// Create a desk and switch to it.
    CreateDesk,
    /// Delete the active desk.
    DeleteDesk,
    /// Cycle to the next configured keyboard layout. Does nothing with only one configured.
    CycleLayout,
    /// Take a screenshot: run `wlrix-screenshot`, with what to start from.
    Screenshot(ShotMode),
    /// Stop the compositor, ending the session.
    Quit,
}

/// What a screenshot binding starts the selection at.
///
/// A fieldless enum, so [`Action`] stays `Copy`. A general `spawn <command>` action would be
/// the obvious way to reach a screenshot tool and would carry a `String`, which costs the
/// binding table its `Copy` and the compositor an allocation on every keypress -- for a
/// generality nothing has asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShotMode {
    /// Let the user drag a region out. What bare `Print` does.
    Region,
    /// Start with the focused window selected, frame and all.
    ActiveWindow,
    /// Start with the whole desktop selected.
    Screen,
}

impl FromStr for Action {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        // `switch-desk 3` and friends: the name, then its argument.
        let mut words = text.split_whitespace();
        let name = words.next().unwrap_or_default();
        let argument = words.next();
        if words.next().is_some() {
            return Err(format!("{text:?} has more than one argument"));
        }

        // The index a desk action names, counted from one in the file and from zero here --
        // `desks.order()` is a list, and off-by-one at the boundary is cheaper than
        // off-by-one at every use.
        let index = || -> Result<usize, String> {
            let argument = argument
                .ok_or_else(|| format!("{name:?} needs a desk number, e.g. \"{name} 1\""))?;
            match argument.parse::<usize>() {
                Ok(0) | Err(_) => Err(format!(
                    "{argument:?} is not a desk number (in {text:?}); they count from 1"
                )),
                Ok(number) => Ok(number - 1),
            }
        };

        let action = match name {
            "restore" => Action::Window(MenuAction::Restore),
            "move" => Action::Window(MenuAction::Move),
            "size" => Action::Window(MenuAction::Size),
            "minimize" => Action::Window(MenuAction::Minimize),
            "maximize" => Action::Window(MenuAction::Maximize),
            "raise" => Action::Window(MenuAction::Raise),
            "lower" => Action::Window(MenuAction::Lower),
            "close" => Action::Window(MenuAction::Close),
            "maximize-toggle" => Action::MaximizeToggle,
            "restore-all" => Action::RestoreAll,
            "switch-desk" => Action::SwitchDesk(index()?),
            "move-to-desk" => Action::MoveToDesk(index()?),
            "create-desk" => Action::CreateDesk,
            "delete-desk" => Action::DeleteDesk,
            "cycle-layout" => Action::CycleLayout,
            "screenshot" => Action::Screenshot(ShotMode::Region),
            "screenshot-window" => Action::Screenshot(ShotMode::ActiveWindow),
            "screenshot-screen" => Action::Screenshot(ShotMode::Screen),
            "quit" => Action::Quit,
            "" => return Err("a binding needs an action; use \"none\" to unbind".to_string()),
            other => return Err(format!("{other:?} is not an action")),
        };

        // Caught here rather than ignored: `"close 2"` means the writer expected something of
        // the argument, and silently dropping it would be the config-file trap this codebase
        // rejects everywhere else.
        if argument.is_some() && !matches!(action, Action::SwitchDesk(_) | Action::MoveToDesk(_)) {
            return Err(format!("{name:?} takes no argument (in {text:?})"));
        }
        Ok(action)
    }
}

/// Parse what the right-hand side of a `[keybinds]` entry names.
///
/// `none` is not an [`Action`] but the absence of one: it takes a default binding away rather
/// than binding the combination to something that does nothing. Keeping it out of the enum
/// means the dispatch in [`crate::input`] has no "do nothing" arm to forget about.
pub fn parse_binding(text: &str) -> Result<Option<Action>, String> {
    if text.trim().eq_ignore_ascii_case("none") {
        return Ok(None);
    }
    text.parse().map(Some)
}

/// The resolved binding table: the defaults with the config's entries applied over them.
#[derive(Debug, Clone)]
pub struct Bindings {
    table: HashMap<Combo, Action>,
    /// The combination the window menu prints beside each of its items, worked out once here.
    ///
    /// Separate from `table` because it is the *reverse* lookup, and because more than one
    /// combination can be bound to one action -- the defaults bind both `Alt+F9` and `Super+m`
    /// to Minimize. A menu has room for one, so which one has to be decided somewhere, and
    /// deciding it once at resolve time keeps it stable: a `HashMap` scanned per post would
    /// print a different key each time the menu opened.
    menu: HashMap<MenuAction, Combo>,
}

impl Default for Bindings {
    fn default() -> Self {
        Self::resolve(&[])
    }
}

impl Bindings {
    /// Apply `configured` over [`DEFAULTS`]. `None` unbinds, which is what `none` in the file
    /// means; anything else replaces whatever that combination was bound to.
    pub fn resolve(configured: &[(Combo, Option<Action>)]) -> Self {
        let defaults: Vec<(Combo, Action)> = DEFAULTS
            .iter()
            .map(|(combo, action)| {
                (
                    combo.parse().expect("a default binding must parse"),
                    action.parse().expect("a default action must parse"),
                )
            })
            .collect();
        let mut table: HashMap<Combo, Action> = defaults.iter().copied().collect();
        for (combo, action) in configured {
            match action {
                Some(action) => table.insert(*combo, *action),
                None => table.remove(combo),
            };
        }

        // Which combination each menu item advertises. Candidates are tried in the order they
        // were written -- the defaults first, the config after -- and the first one still bound
        // to the action wins.
        //
        // Defaults first is what puts `Alt+F9` beside Minimize rather than `Super+m`, both of
        // which are bound to it: the window-menu block heads `DEFAULTS`, and those are the
        // keys 4Dwm printed. It also means a combination the user *adds* alongside a default
        // does not displace the default from the menu, while one that *replaces* it does --
        // the default is no longer in the table to be found.
        let mut menu = HashMap::new();
        let candidates = defaults
            .iter()
            .map(|(combo, _)| combo)
            .chain(configured.iter().map(|(combo, _)| combo));
        for combo in candidates {
            if let Some(Action::Window(action)) = table.get(combo) {
                menu.entry(*action).or_insert(*combo);
            }
        }

        Self { table, menu }
    }

    /// The action this key press is bound to, if any.
    ///
    /// The modifiers have to match **exactly**: a binding that does not name Shift does not
    /// fire while Shift is held. The hard-coded chain this replaced tested only the modifiers
    /// it cared about, so Ctrl+Alt+Shift+BackSpace quit the session and Super+Ctrl+Shift+1
    /// moved a window to a desk. Neither was intended.
    ///
    /// The key matches on either the modified keysym or the raw Latin one. The modified sym
    /// is what the user sees printed on the key; the raw one is what makes a binding written
    /// `Super+Shift+m` fire (its modified sym is a capital `M`, while the table holds the
    /// lower-case `m` the config names), and what makes bindings survive a non-Latin layout --
    /// see smithay's `raw_latin_sym_or_raw_current_sym`.
    pub fn action(&self, mods: &ModifiersState, handle: &KeysymHandle<'_>) -> Option<Action> {
        let modified = handle.modified_sym();
        if let Some(action) = self.table.get(&Combo::with(mods, modified)) {
            return Some(*action);
        }
        let raw = handle.raw_latin_sym_or_raw_current_sym()?;
        if raw == modified {
            return None;
        }
        self.table.get(&Combo::with(mods, raw)).copied()
    }

    /// The combination the window menu prints beside `action`, if anything is bound to it.
    ///
    /// What is *bound*, not what the defaults say: a rebound Close reads as the key that now
    /// closes it, and an action the user has unbound outright gets nothing. See `menu` on
    /// [`Bindings`] for how one is picked when several are bound.
    pub fn menu_combo(&self, action: MenuAction) -> Option<Combo> {
        self.menu.get(&action).copied()
    }

    /// How many combinations are bound. For tests and for the startup log line.
    pub fn len(&self) -> usize {
        self.table.len()
    }

    /// Whether nothing at all is bound -- every default having been switched off.
    pub fn is_empty(&self) -> bool {
        self.table.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn combo(text: &str) -> Combo {
        text.parse().expect(text)
    }

    /// The table is parsed at startup, so a typo in it would be a panic on a user's machine.
    /// This is the test that keeps that from ever being how it is found out.
    #[test]
    fn every_default_parses() {
        for (keys, action) in DEFAULTS {
            keys.parse::<Combo>()
                .unwrap_or_else(|err| panic!("default binding {keys:?}: {err}"));
            action
                .parse::<Action>()
                .unwrap_or_else(|err| panic!("default action {action:?}: {err}"));
        }
        // And no combination is listed twice, which the table being a map would hide.
        let mut seen = std::collections::HashSet::new();
        for (keys, _) in DEFAULTS {
            let combo: Combo = keys.parse().unwrap();
            assert!(seen.insert(combo), "{keys:?} is bound twice in DEFAULTS");
        }
    }

    #[test]
    fn a_combination_parses_however_it_is_spelled() {
        let expected = Combo {
            ctrl: false,
            alt: true,
            shift: false,
            logo: false,
            sym: xkb::keysym_from_name("F4", xkb::KEYSYM_NO_FLAGS),
        };
        // Case and spacing are the user's business, not the table's.
        assert_eq!(combo("Alt+F4"), expected);
        assert_eq!(combo("alt+f4"), expected);
        assert_eq!(combo("ALT + F4"), expected);
        // Order is not significant either.
        assert_eq!(combo("F4+Alt"), expected);
    }

    #[test]
    fn the_modifier_aliases_are_accepted() {
        assert_eq!(combo("Control+q"), combo("Ctrl+q"));
        for name in ["Super", "Logo", "Meta", "Win"] {
            assert_eq!(combo(&format!("{name}+space")), combo("Super+space"));
        }
    }

    /// The whole point of normalizing the keysym: these have to be one table entry, or
    /// `"Super+Shift+M" = "none"` would not take away the default written `Super+Shift+m`.
    #[test]
    fn letter_case_is_not_part_of_the_combination() {
        assert_eq!(combo("Super+Shift+m"), combo("Super+Shift+M"));
    }

    #[test]
    fn a_bad_combination_is_rejected_with_its_reason() {
        for text in ["Alt+Nonsense", "Alt+", "Alt", "Ctrl++", "Alt+F4+F5", ""] {
            assert!(
                text.parse::<Combo>().is_err(),
                "{text:?} should not have parsed"
            );
        }
        // The message names the part that was wrong, since serde will not.
        let err = "Alt+Nonsense".parse::<Combo>().unwrap_err();
        assert!(err.contains("Nonsense"), "{err}");
    }

    #[test]
    fn a_combination_prints_the_way_it_is_written() {
        assert_eq!(combo("alt+f4").to_string(), "Alt+F4");
        assert_eq!(
            combo("ctrl + alt + backspace").to_string(),
            "Ctrl+Alt+BackSpace"
        );
        assert_eq!(combo("Super+Shift+M").to_string(), "Shift+Super+m");
    }

    #[test]
    fn actions_parse() {
        assert_eq!(
            "close".parse::<Action>().unwrap(),
            Action::Window(MenuAction::Close)
        );
        assert_eq!(
            "size".parse::<Action>().unwrap(),
            Action::Window(MenuAction::Size)
        );
        assert_eq!("quit".parse::<Action>().unwrap(), Action::Quit);
        // Desks count from one in the file and from zero in `desks.order()`.
        assert_eq!(
            "switch-desk 3".parse::<Action>().unwrap(),
            Action::SwitchDesk(2)
        );
        assert_eq!(
            "move-to-desk 1".parse::<Action>().unwrap(),
            Action::MoveToDesk(0)
        );
    }

    #[test]
    fn the_screenshot_actions_parse() {
        assert_eq!(
            "screenshot".parse::<Action>().unwrap(),
            Action::Screenshot(ShotMode::Region)
        );
        assert_eq!(
            "screenshot-window".parse::<Action>().unwrap(),
            Action::Screenshot(ShotMode::ActiveWindow)
        );
        assert_eq!(
            "screenshot-screen".parse::<Action>().unwrap(),
            Action::Screenshot(ShotMode::Screen)
        );
    }

    /// `Print` is a key name like any other, and the three shapes are bound out of the box.
    #[test]
    fn print_is_bound_to_a_screenshot() {
        let bindings = Bindings::default();
        assert_eq!(
            bindings.table.get(&combo("Print")),
            Some(&Action::Screenshot(ShotMode::Region))
        );
        assert_eq!(
            bindings.table.get(&combo("Alt+Print")),
            Some(&Action::Screenshot(ShotMode::ActiveWindow))
        );
        assert_eq!(
            bindings.table.get(&combo("Shift+Print")),
            Some(&Action::Screenshot(ShotMode::Screen))
        );
    }

    #[test]
    fn a_bad_action_is_rejected() {
        for text in [
            "nonsense",
            "switch-desk",
            "switch-desk 0",
            "switch-desk x",
            "switch-desk 1 2",
            "close 2",
            "screenshot 2",
            "",
        ] {
            assert!(
                text.parse::<Action>().is_err(),
                "{text:?} should not have parsed"
            );
        }
    }

    #[test]
    fn none_unbinds_rather_than_binding() {
        assert_eq!(parse_binding("none").unwrap(), None);
        assert_eq!(parse_binding("None").unwrap(), None);
        assert_eq!(parse_binding("quit").unwrap(), Some(Action::Quit));
        assert!(parse_binding("nonsense").is_err());
    }

    #[test]
    fn the_defaults_are_bound_when_nothing_is_configured() {
        let bindings = Bindings::default();
        assert_eq!(bindings.len(), DEFAULTS.len());
        assert!(!bindings.is_empty());
    }

    /// The merge, which is the whole difference between this section and the config file it
    /// sits in: the file is used whole, this section is layered over what is built in.
    #[test]
    fn configured_bindings_are_applied_over_the_defaults() {
        let quit = combo("Ctrl+Alt+BackSpace");
        let close = combo("Alt+F4");
        let fresh = combo("Ctrl+Alt+q");

        let bindings = Bindings::resolve(&[
            // Take one default away...
            (quit, None),
            // ...rebind another...
            (close, Some(Action::Quit)),
            // ...and add one that was not there.
            (fresh, Some(Action::Window(MenuAction::Close))),
        ]);

        assert_eq!(bindings.table.get(&quit), None, "`none` must unbind");
        assert_eq!(bindings.table.get(&close), Some(&Action::Quit));
        assert_eq!(
            bindings.table.get(&fresh),
            Some(&Action::Window(MenuAction::Close))
        );
        // Every other default is untouched -- the point of merging rather than replacing.
        assert_eq!(
            bindings.table.get(&combo("Alt+F9")),
            Some(&Action::Window(MenuAction::Minimize))
        );
        assert_eq!(bindings.len(), DEFAULTS.len() - 1 + 1);
    }

    /// Two defaults are bound to Minimize -- `Alt+F9` and the temporary `Super+m` -- and the
    /// menu has room for one. The window-menu block heads `DEFAULTS`, so that is the one shown:
    /// 4Dwm printed `Alt+F9` there, and a menu that printed the scaffolding binding instead
    /// would be advertising the key that is due to be retired.
    #[test]
    fn the_menu_shows_the_4dwm_key_where_two_are_bound() {
        let bindings = Bindings::default();
        assert_eq!(
            bindings.menu_combo(MenuAction::Minimize),
            Some(combo("Alt+F9"))
        );
        assert_eq!(
            bindings.menu_combo(MenuAction::Lower),
            Some(combo("Alt+F3"))
        );
        assert_eq!(
            bindings.menu_combo(MenuAction::Close),
            Some(combo("Alt+F4"))
        );
    }

    /// Whichever combination it picks, it must pick the same one every time -- the table is a
    /// `HashMap`, and a menu whose accelerator changed between posts would be a poltergeist.
    #[test]
    fn the_menu_combination_is_stable_across_resolutions() {
        let first = Bindings::default();
        for _ in 0..8 {
            let again = Bindings::default();
            for action in [MenuAction::Minimize, MenuAction::Lower, MenuAction::Close] {
                assert_eq!(first.menu_combo(action), again.menu_combo(action));
            }
        }
    }

    /// Replacing a default puts the new key in the menu; *adding* one alongside leaves the
    /// default there, since the default is still bound and is still the canonical spelling.
    #[test]
    fn the_menu_follows_what_is_actually_bound() {
        let close = Action::Window(MenuAction::Close);

        let replaced = Bindings::resolve(&[
            (combo("Alt+F4"), None),
            (combo("Ctrl+Shift+w"), Some(close)),
        ]);
        assert_eq!(
            replaced.menu_combo(MenuAction::Close),
            Some(combo("Ctrl+Shift+w"))
        );

        let added = Bindings::resolve(&[(combo("Ctrl+Shift+w"), Some(close))]);
        assert_eq!(added.menu_combo(MenuAction::Close), Some(combo("Alt+F4")));

        let unbound = Bindings::resolve(&[(combo("Alt+F4"), None)]);
        assert_eq!(unbound.menu_combo(MenuAction::Close), None);
    }

    #[test]
    fn unbinding_something_that_was_never_bound_is_not_an_error() {
        let bindings = Bindings::resolve(&[(combo("Ctrl+Alt+F12"), None)]);
        assert_eq!(bindings.len(), DEFAULTS.len());
    }
}
