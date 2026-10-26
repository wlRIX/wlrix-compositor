// SPDX-License-Identifier: GPL-3.0-or-later
//! Finding the command that starts the application behind a window.
//!
//! A saved session has to say how to bring each window back, and a window knows only its own
//! name: the xdg `app_id`, or an X11 `WM_CLASS`. The installed `.desktop` files are what map
//! a name to a command, by the same three-step match `wlrix-desktop` uses to tell which of its
//! launchers are running (`wlrix-desktop/src/running.rs`): `StartupWMClass`, the file's
//! basename, then the `Exec` program's name.
//!
//! The parser is a trimmed copy of `wlrix-desktop/src/desktop_entry.rs` -- the keys needed to
//! run an entry and nothing else. `Exec=` is split by the spec's own quoting rules and never
//! handed to a shell, for the reason given there.

use std::path::{Path, PathBuf};

/// The parts of a `[Desktop Entry]` group that decide what to run.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DesktopEntry {
    /// The file's basename without `.desktop`: its desktop id.
    pub id: String,
    pub exec: Option<String>,
    pub startup_wm_class: Option<String>,
    /// Whether `Categories` lists `TerminalEmulator`.
    pub terminal_emulator: bool,
    hidden: bool,
    application: bool,
}

impl DesktopEntry {
    /// Parse the `[Desktop Entry]` group out of a file's text. `None` if there is none, or it
    /// is not an application.
    pub fn parse(id: &str, text: &str) -> Option<Self> {
        let mut entry = Self {
            id: id.to_string(),
            ..Self::default()
        };
        let mut in_entry = false;
        let mut seen = false;
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some(header) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
                // The first `[Desktop Entry]` wins, as in `wlrix-desktop`.
                in_entry = header == "Desktop Entry" && !seen;
                seen |= in_entry;
                continue;
            }
            if !in_entry {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let value = unescape(value.trim());
            match key.trim() {
                "Type" => entry.application = value == "Application",
                "Exec" if entry.exec.is_none() => entry.exec = Some(value),
                "StartupWMClass" if !value.is_empty() => entry.startup_wm_class = Some(value),
                "Hidden" => entry.hidden = value == "true",
                "Categories" => {
                    entry.terminal_emulator = value.split(';').any(|c| c == "TerminalEmulator")
                }
                _ => {}
            }
        }
        (seen && entry.application && !entry.hidden).then_some(entry)
    }

    /// The `Exec` line as an argument vector, with every field code removed: a restored
    /// application is started with no files, as if from its launcher.
    pub fn argv(&self) -> Option<Vec<String>> {
        let exec = self
            .exec
            .as_deref()
            .filter(|exec| !exec.trim().is_empty())?;
        let argv = strip_field_codes(&split(exec).ok()?);
        (!argv.is_empty()).then_some(argv)
    }

    /// The program `Exec` runs, as a basename.
    fn program(&self) -> Option<String> {
        self.argv()?.first().map(|program| basename(program))
    }
}

/// Every application entry installed for this user, earlier directories first.
///
/// `$XDG_DATA_HOME` ahead of `$XDG_DATA_DIRS`, as the spec orders them; a user's own copy of
/// an entry shadows the system one, so the first entry with a given id is the one kept.
pub fn installed() -> Vec<DesktopEntry> {
    let mut entries: Vec<DesktopEntry> = Vec::new();
    for dir in application_dirs() {
        let Ok(read) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut files: Vec<PathBuf> = read
            .filter_map(Result::ok)
            .map(|file| file.path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "desktop"))
            .collect();
        // Directory order is the filesystem's; sorted so a lookup is the same every time.
        files.sort();
        for path in files {
            let Some(id) = path.file_stem().and_then(|stem| stem.to_str()) else {
                continue;
            };
            if entries.iter().any(|entry| entry.id == id) {
                continue;
            }
            if let Some(entry) = std::fs::read_to_string(&path)
                .ok()
                .and_then(|text| DesktopEntry::parse(id, &text))
            {
                entries.push(entry);
            }
        }
    }
    entries
}

fn application_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    match std::env::var_os("XDG_DATA_HOME").filter(|dir| !dir.is_empty()) {
        Some(dir) => dirs.push(PathBuf::from(dir)),
        None => {
            if let Some(home) = std::env::var_os("HOME") {
                dirs.push(PathBuf::from(home).join(".local/share"));
            }
        }
    }
    let data_dirs = std::env::var("XDG_DATA_DIRS")
        .ok()
        .filter(|dirs| !dirs.is_empty())
        .unwrap_or_else(|| "/usr/local/share:/usr/share".to_string());
    dirs.extend(data_dirs.split(':').map(PathBuf::from));
    dirs.into_iter()
        .map(|dir| dir.join("applications"))
        .collect()
}

/// The entry that launches a window named `names` (its `app_id` or X11 class first, then the
/// X11 instance if there is one).
///
/// Most trusted first: the desktop id exactly as given, which is what the spec says an
/// `app_id` is; then `StartupWMClass`; then the basename and `Exec` program, both compared
/// case-insensitively, because `Alacritty` and `alacritty` are the same application.
pub fn lookup<'a>(entries: &'a [DesktopEntry], names: &[&str]) -> Option<&'a DesktopEntry> {
    let names: Vec<&str> = names.iter().copied().filter(|n| !n.is_empty()).collect();
    let lowered: Vec<String> = names.iter().map(|name| name.to_lowercase()).collect();
    let matches = |value: &str| lowered.iter().any(|name| *name == value.to_lowercase());

    entries
        .iter()
        .find(|entry| names.contains(&entry.id.as_str()))
        .or_else(|| {
            entries
                .iter()
                .find(|entry| entry.startup_wm_class.as_deref().is_some_and(matches))
        })
        .or_else(|| entries.iter().find(|entry| matches(&entry.id)))
        .or_else(|| {
            entries
                .iter()
                .find(|entry| entry.program().as_deref().is_some_and(matches))
        })
}

/// The command that brings a window's application back.
///
/// The process's own command line when it is the program the entry runs, so whatever it was
/// opened with comes back too -- `wlrix-writer notes.txt` reopens `notes.txt`. Otherwise the
/// entry's `Exec`: a wrapper script, `flatpak run` or an Electron launcher all show up in
/// `/proc` as something other than what started them, and only the entry knows how to start
/// them again.
///
/// A terminal always gets its plain `Exec`. What was running in it is gone, and starting
/// `foot -e make` again on login would be a surprise rather than a restoration.
pub fn command_for(entry: &DesktopEntry, cmdline: Option<Vec<String>>) -> Option<Vec<String>> {
    let exec = entry.argv()?;
    if entry.terminal_emulator {
        return Some(exec);
    }
    match cmdline {
        Some(cmdline)
            if cmdline
                .first()
                .is_some_and(|program| basename(program) == basename(&exec[0])) =>
        {
            Some(cmdline)
        }
        _ => Some(exec),
    }
}

/// A process's argument vector, from `/proc`. `None` once it has exited.
pub fn cmdline(pid: i32) -> Option<Vec<String>> {
    let bytes = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    let argv: Vec<String> = bytes
        .split(|&byte| byte == 0)
        .filter(|arg| !arg.is_empty())
        .map(|arg| String::from_utf8_lossy(arg).into_owned())
        .collect();
    (!argv.is_empty()).then_some(argv)
}

fn basename(program: &str) -> String {
    Path::new(program)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(program)
        .to_string()
}

/// Undo the escape sequences the spec defines for string values.
fn unescape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars();
    while let Some(character) = chars.next() {
        if character != '\\' {
            out.push(character);
            continue;
        }
        match chars.next() {
            Some('s') => out.push(' '),
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some('\\') => out.push('\\'),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

/// Split an `Exec=` value into arguments, by the spec's quoting rules: double quotes group,
/// and inside them a backslash escapes `"`, `` ` ``, `$` and `\` only.
fn split(exec: &str) -> Result<Vec<String>, String> {
    let mut argv = Vec::new();
    let mut current = String::new();
    let mut started = false;
    let mut quoted = false;
    let mut chars = exec.chars();

    while let Some(character) = chars.next() {
        match character {
            '"' => {
                quoted = !quoted;
                started = true;
            }
            '\\' if quoted => match chars.next() {
                Some(escaped @ ('"' | '`' | '$' | '\\')) => {
                    current.push(escaped);
                    started = true;
                }
                Some(other) => {
                    current.push('\\');
                    current.push(other);
                    started = true;
                }
                None => return Err("Exec ends in a backslash".to_string()),
            },
            ' ' | '\t' if !quoted => {
                if started {
                    argv.push(std::mem::take(&mut current));
                    started = false;
                }
            }
            other => {
                current.push(other);
                started = true;
            }
        }
    }

    if quoted {
        return Err("Exec has an unclosed quote".to_string());
    }
    if started {
        argv.push(current);
    }
    Ok(argv)
}

/// Remove every field code. An argument that was only a field code goes with it; `%%` is a
/// literal percent.
fn strip_field_codes(argv: &[String]) -> Vec<String> {
    let mut out = Vec::with_capacity(argv.len());
    for argument in argv {
        let mut expanded = String::with_capacity(argument.len());
        let mut dropped = false;
        let mut chars = argument.chars();
        while let Some(character) = chars.next() {
            if character != '%' {
                expanded.push(character);
                continue;
            }
            match chars.next() {
                Some('%') => expanded.push('%'),
                Some(_) => dropped = true,
                None => expanded.push('%'),
            }
        }
        if dropped && expanded.is_empty() {
            continue;
        }
        out.push(expanded);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str, body: &str) -> DesktopEntry {
        DesktopEntry::parse(id, &format!("[Desktop Entry]\nType=Application\n{body}"))
            .expect("should parse")
    }

    fn argv(args: &[&str]) -> Vec<String> {
        args.iter().map(|arg| arg.to_string()).collect()
    }

    #[test]
    fn the_desktop_id_wins_over_everything_else() {
        let entries = [
            entry("other", "Exec=writer\nStartupWMClass=com.wlrix.writer\n"),
            entry("com.wlrix.writer", "Exec=wlrix-writer %F\n"),
        ];
        let found = lookup(&entries, &["com.wlrix.writer"]).expect("found");
        assert_eq!(found.id, "com.wlrix.writer");
    }

    #[test]
    fn startup_wm_class_then_basename_then_program() {
        let entries = [
            entry("Alacritty", "Exec=alacritty\nStartupWMClass=Alacritty\n"),
            entry("org.example.Foot", "Exec=/usr/bin/foot --server\n"),
            entry("xterm", "Exec=xterm\n"),
        ];
        assert_eq!(lookup(&entries, &["alacritty"]).unwrap().id, "Alacritty");
        assert_eq!(lookup(&entries, &["foot"]).unwrap().id, "org.example.Foot");
        // An X11 window whose class is unhelpful but whose instance is the program.
        assert_eq!(
            lookup(&entries, &["XTerm-ish", "xterm"]).unwrap().id,
            "xterm"
        );
        assert!(lookup(&entries, &["steam_app_570"]).is_none());
    }

    #[test]
    fn hidden_and_non_application_entries_are_skipped() {
        assert!(DesktopEntry::parse("a", "[Desktop Entry]\nType=Link\nURL=x\n").is_none());
        assert!(
            DesktopEntry::parse(
                "a",
                "[Desktop Entry]\nType=Application\nExec=a\nHidden=true\n"
            )
            .is_none()
        );
    }

    #[test]
    fn field_codes_are_stripped_from_exec() {
        let flatpak = entry(
            "org.mozilla.firefox",
            "Exec=/usr/bin/flatpak run --branch=stable --command=firefox org.mozilla.firefox @@u %u @@\n",
        );
        assert_eq!(
            flatpak.argv().unwrap(),
            argv(&[
                "/usr/bin/flatpak",
                "run",
                "--branch=stable",
                "--command=firefox",
                "org.mozilla.firefox",
                "@@u",
                "@@"
            ])
        );
        assert_eq!(
            entry("x", "Exec=\"/opt/My App/app\" --x=%%1 %F\n")
                .argv()
                .unwrap(),
            argv(&["/opt/My App/app", "--x=%1"])
        );
    }

    #[test]
    fn the_real_command_line_is_kept_when_it_is_the_same_program() {
        let writer = entry("com.wlrix.writer", "Exec=wlrix-writer %F\n");
        assert_eq!(
            command_for(
                &writer,
                Some(argv(&["/usr/bin/wlrix-writer", "/home/v/notes.txt"]))
            ),
            Some(argv(&["/usr/bin/wlrix-writer", "/home/v/notes.txt"]))
        );
    }

    #[test]
    fn a_different_program_falls_back_to_exec() {
        let code = entry("code", "Exec=/usr/bin/code %F\n");
        assert_eq!(
            command_for(
                &code,
                Some(argv(&[
                    "/usr/lib/electron/electron",
                    "/usr/lib/code/out/cli.js"
                ]))
            ),
            Some(argv(&["/usr/bin/code"]))
        );
        assert_eq!(command_for(&code, None), Some(argv(&["/usr/bin/code"])));
    }

    #[test]
    fn a_terminal_always_gets_a_fresh_shell() {
        let foot = entry("foot", "Exec=foot\nCategories=System;TerminalEmulator;\n");
        assert!(foot.terminal_emulator);
        assert_eq!(
            command_for(&foot, Some(argv(&["foot", "-e", "make"]))),
            Some(argv(&["foot"]))
        );
    }
}
