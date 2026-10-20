// SPDX-License-Identifier: GPL-3.0-or-later
//! The laptop's own panel and its lid: switched off while the lid is closed on a docked laptop.
//!
//! Closing the lid with a monitor plugged in is how a laptop is used as a desktop. logind does
//! nothing then (`HandleLidSwitchDocked=ignore`), and `wlrix-idle`'s docked action is nothing by
//! default too -- so without this the panel would stay lit behind the lid, still part of the
//! desktop, with windows on it that nobody can see. Here it is switched off as if from the
//! Displays panel, which moves its windows to the monitors left, and back on when the lid opens.
//!
//! What closing the lid does to the *session* -- suspend, lock, blank -- is `wlrix-idle`'s, not
//! this. This only decides which outputs exist.
//!
//! # Not saved
//!
//! A panel switched off for the lid is not one the person switched off: the saved layout keeps
//! it on, so the next login with the lid open has it. And it is never the last output: a closed
//! lid with nothing else plugged in leaves the panel alone, and unplugging the last monitor
//! while the lid is shut brings the panel back -- a session with no outputs at all would have
//! nowhere to put a window or a lock screen.
//!
//! The lid's state comes from libinput's switch events, which it sends at startup as well when
//! the lid is already shut. The nested backend has no lid.

/// Whether a connector is the laptop's own panel: embedded DisplayPort, LVDS or DSI, by the
/// names the DRM backend gives outputs (`eDP-1`).
pub fn is_internal(name: &str) -> bool {
    ["eDP-", "LVDS-", "DSI-"]
        .iter()
        .any(|prefix| name.starts_with(prefix))
}

/// What to switch, by output name.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Plan {
    pub disable: Vec<String>,
    pub enable: Vec<String>,
}

/// Work out the change, from the lid, the outputs now on, and the ones this switched off.
pub fn plan(closed: bool, enabled: &[String], switched_off: &[String]) -> Plan {
    let elsewhere = enabled.iter().any(|name| !is_internal(name));
    if closed && elsewhere {
        Plan {
            disable: enabled
                .iter()
                .filter(|name| is_internal(name))
                .cloned()
                .collect(),
            enable: Vec::new(),
        }
    } else {
        // Open -- or shut with nothing else to show the session on, so the panel stays, or
        // comes back.
        Plan {
            disable: Vec::new(),
            enable: switched_off.to_vec(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|name| (*name).to_owned()).collect()
    }

    #[test]
    fn the_panel_is_told_apart_by_its_connector() {
        assert!(is_internal("eDP-1"));
        assert!(is_internal("LVDS-1"));
        assert!(is_internal("DSI-1"));
        assert!(!is_internal("DP-1"));
        assert!(!is_internal("HDMI-A-1"));
    }

    #[test]
    fn closing_a_docked_laptop_turns_the_panel_off() {
        let plan = plan(true, &names(&["eDP-1", "DP-2"]), &[]);
        assert_eq!(plan.disable, names(&["eDP-1"]));
        assert!(plan.enable.is_empty());
    }

    #[test]
    fn closing_an_undocked_laptop_leaves_it_alone() {
        assert_eq!(plan(true, &names(&["eDP-1"]), &[]), Plan::default());
    }

    #[test]
    fn opening_or_losing_the_last_monitor_brings_it_back() {
        let off = names(&["eDP-1"]);
        assert_eq!(plan(false, &names(&["DP-2"]), &off).enable, off);
        // The monitor was unplugged with the lid still shut.
        assert_eq!(plan(true, &[], &off).enable, off);
        // Still docked and shut: nothing to do.
        assert_eq!(plan(true, &names(&["DP-2"]), &off), Plan::default());
    }
}
