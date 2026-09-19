// SPDX-License-Identifier: GPL-3.0-or-later
//! The fixed pictures on minimized-window icons.
//!
//! A minimized window is shown as a 4Dwm icon tile (see [`crate::minimized`]) with a picture in
//! its well. 4Dwm's pictures were **fixed artwork, one per application**, and so are these: the
//! tile for a terminal always shows the terminal's drawing, whatever the window happened to
//! contain when it was minimized.
//!
//! This replaced a live snapshot of the window, taken with the renderer into an offscreen
//! texture. That was unreliable in a way artwork cannot be: the capture raced the client's
//! buffer, so a window minimized before it had painted -- or one whose buffer had already gone
//! -- gave an empty tile, and a client keeping a CSD margin or a surface larger than its
//! geometry came out miscropped. It also letterboxed nearly every window, since almost nothing
//! is 85x67.
//!
//! **Lookup** is by the window's application id (the xdg `app_id`, or the X11 class -- see
//! [`crate::placement::window_app_id`]), against `wlrix/images` on every XDG data directory.
//! IRIX let a client name its own picture through `WM_CLASS`; matching the app id is the same
//! idea with the name the window already has. `default.png` answers for everything unmatched,
//! so a tile is never blank.
//!
//! The user's own directory comes first, so dropping a file in
//! `~/.local/share/wlrix/images/<app id>.png` overrides what the system ships. `SIGHUP` drops
//! the caches (see [`crate::Wlrix::reload_config`]), which is how a newly dropped file is
//! picked up without a restart.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use smithay::{
    backend::{allocator::Fourcc, renderer::element::memory::MemoryRenderBuffer},
    utils::{Physical, Size, Transform},
};

/// The picture every window without one of its own gets.
const DEFAULT_IMAGE: &str = "default.png";

/// The subdirectory of an XDG data directory the artwork lives in.
const IMAGE_SUBDIR: &str = "wlrix/images";

/// Resolved paths past which the map is dropped wholesale.
///
/// A window can set its `app_id` as often as it likes, so this is bounded by what clients say
/// rather than by what is installed. The same guard, for the same reason, as the title-text
/// cache in [`crate::text`].
const MAX_RESOLVED: usize = 256;

/// The pictures for minimized-window icons, resolved and decoded once.
///
/// Held on [`crate::Wlrix`] beside the text renderer, and borrowed by the render pass. Keyed by
/// application id rather than by window, so two terminals share one decode.
#[derive(Default)]
pub struct IconImages {
    /// The decoder and its own (path, size) cache.
    images: wlrix_ui::image::Images,
    /// Application id -> the file that answers for it, or `None` when nothing does.
    ///
    /// Resolved once and remembered: the search walks several directories and, on a miss, reads
    /// one of them, and the render pass asks for every visible icon on every frame.
    resolved: HashMap<String, Option<PathBuf>>,
    /// Uploadable buffers, keyed by file and by the physical size they were scaled to. The
    /// size is part of the key because a change of output scale asks for a different one.
    buffers: HashMap<(PathBuf, i32, i32), MemoryRenderBuffer>,
}

impl IconImages {
    /// The picture for `app_id`, scaled to fill `size` physical pixels exactly.
    ///
    /// `None` when neither a matching file nor `default.png` is installed anywhere -- the tile
    /// then shows its bare well, which is what an uncaptured window used to show.
    pub fn buffer(
        &mut self,
        app_id: &str,
        size: Size<i32, Physical>,
    ) -> Option<MemoryRenderBuffer> {
        if size.w <= 0 || size.h <= 0 {
            return None;
        }
        let path = self.resolve(app_id)?.clone();
        let key = (path, size.w, size.h);
        if let Some(buffer) = self.buffers.get(&key) {
            return Some(buffer.clone());
        }

        // `load_cover`, not `load`: the well has a color of its own, so artwork that merely
        // fitted inside it would frame every icon in a border the tile never asked for.
        let image = self.images.load_cover(&key.0, size.w, size.h)?;
        // Native-endian ARGB is `Argb8888` on a little-endian machine, and premultiplied --
        // see `wlrix_ui::image::Image::pixels`, and `crate::cursor`'s `CURSOR_FOURCC` for the
        // same conversion and the bug that comes of getting it backwards.
        let pixels: Vec<u8> = image
            .pixels()
            .iter()
            .flat_map(|pixel| pixel.to_ne_bytes())
            .collect();
        let buffer = MemoryRenderBuffer::from_slice(
            &pixels,
            Fourcc::Argb8888,
            (size.w, size.h),
            1,
            Transform::Normal,
            None,
        );
        Some(self.buffers.entry(key).or_insert(buffer).clone())
    }

    /// Forget every resolved path and decoded buffer.
    ///
    /// Called on `SIGHUP`, which is what makes a file dropped into the user's image directory
    /// take effect without a restart. Unlike the text cache this has nothing to do with the
    /// palette: a picture is a picture whatever the scheme.
    pub fn clear(&mut self) {
        self.images.clear();
        self.resolved.clear();
        self.buffers.clear();
    }

    /// The file that answers for `app_id`, remembered across frames.
    fn resolve(&mut self, app_id: &str) -> Option<&PathBuf> {
        if !self.resolved.contains_key(app_id) {
            if self.resolved.len() >= MAX_RESOLVED {
                self.resolved.clear();
            }
            self.resolved
                .insert(app_id.to_string(), find(&image_dirs(), app_id));
        }
        self.resolved.get(app_id)?.as_ref()
    }
}

/// The file for `app_id` in `dirs`, or the default, or nothing.
///
/// Three passes rather than one per directory, so that precedence is by *how well the name
/// matches* first and by directory second: an exact match anywhere beats a case-insensitive
/// one, and both beat the default. Within a pass the directories are in order, so the user's
/// copy wins over the system's.
fn find(dirs: &[PathBuf], app_id: &str) -> Option<PathBuf> {
    if !app_id.is_empty() {
        let name = format!("{app_id}.png");
        if let Some(path) = dirs.iter().map(|dir| dir.join(&name)).find(|p| p.is_file()) {
            return Some(path);
        }
        // Applications disagree with their artwork about case -- `Blender.png` and `Lutris.png`
        // are `WM_CLASS` names, which are conventionally capitalized, while the app ids of the
        // same programs elsewhere are not. Only reached on a miss, and only one `read_dir` per
        // directory when it is.
        if let Some(path) = dirs.iter().find_map(|dir| matching(dir, &name)) {
            return Some(path);
        }
    }
    dirs.iter()
        .map(|dir| dir.join(DEFAULT_IMAGE))
        .find(|path| path.is_file())
}

/// An entry of `dir` whose name equals `name` ignoring case.
fn matching(dir: &Path, name: &str) -> Option<PathBuf> {
    std::fs::read_dir(dir).ok()?.flatten().find_map(|entry| {
        let path = entry.path();
        (path.file_name()?.to_str()?.eq_ignore_ascii_case(name) && path.is_file()).then_some(path)
    })
}

/// Directories holding minimize-icon artwork, most specific first.
///
/// `$XDG_DATA_HOME` then `$XDG_DATA_DIRS`, with the spec's defaults for either, each joined
/// with `wlrix/images`. Not a compiled-in `/usr/share`: the components install under `PREFIX`,
/// and a prefix other than `/usr` is a supported arrangement everywhere else in wlRIX.
///
/// The same walk as `wlrix-settings-daemon`'s `data_dirs`, copied rather than shared for the
/// reason `wlrix-bg`'s `xdg` module states: these repos build standalone.
fn image_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(home) = std::env::var_os("XDG_DATA_HOME").filter(|value| !value.is_empty()) {
        dirs.push(PathBuf::from(home));
    } else if let Some(home) = std::env::var_os("HOME").filter(|value| !value.is_empty()) {
        dirs.push(PathBuf::from(home).join(".local").join("share"));
    }
    let system = std::env::var_os("XDG_DATA_DIRS")
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "/usr/local/share:/usr/share".into());
    dirs.extend(std::env::split_paths(&system));
    dirs.into_iter().map(|dir| dir.join(IMAGE_SUBDIR)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scratch directory tree that cleans up after itself.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("wlrix-icon-image-{name}"));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("scratch dir");
            Self(dir)
        }

        /// An empty file at `name` under this tree; resolution never opens it.
        fn touch(&self, name: &str) -> PathBuf {
            let path = self.0.join(name);
            std::fs::create_dir_all(path.parent().expect("a parent")).expect("dirs");
            std::fs::write(&path, b"").expect("file");
            path
        }

        fn dir(&self, name: &str) -> PathBuf {
            let path = self.0.join(name);
            std::fs::create_dir_all(&path).expect("dirs");
            path
        }

        /// A relative symlink at `name` pointing at `target` in the same directory.
        fn link(&self, name: &str, target: &str) -> PathBuf {
            let path = self.0.join(name);
            std::os::unix::fs::symlink(target, &path).expect("symlink");
            path
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Run `body` with the data-directory variables set to `home` and `system`.
    ///
    /// Serialized by a mutex: these are process-wide, and cargo runs tests in threads.
    fn with_data_dirs<T>(home: Option<&str>, system: Option<&str>, body: impl FnOnce() -> T) -> T {
        use std::sync::Mutex;
        static LOCK: Mutex<()> = Mutex::new(());
        let guard = LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());

        let saved = (
            std::env::var_os("XDG_DATA_HOME"),
            std::env::var_os("XDG_DATA_DIRS"),
        );
        let set = |name: &str, value: Option<&str>| unsafe {
            match value {
                Some(value) => std::env::set_var(name, value),
                None => std::env::remove_var(name),
            }
        };
        set("XDG_DATA_HOME", home);
        set("XDG_DATA_DIRS", system);
        let out = body();
        unsafe {
            match &saved.0 {
                Some(value) => std::env::set_var("XDG_DATA_HOME", value),
                None => std::env::remove_var("XDG_DATA_HOME"),
            }
            match &saved.1 {
                Some(value) => std::env::set_var("XDG_DATA_DIRS", value),
                None => std::env::remove_var("XDG_DATA_DIRS"),
            }
        }
        drop(guard);
        out
    }

    #[test]
    fn the_user_directory_comes_before_the_system_ones() {
        let dirs = with_data_dirs(
            Some("/home/someone/.local/share"),
            Some("/opt/x:/usr/share"),
            image_dirs,
        );
        assert_eq!(
            dirs,
            vec![
                PathBuf::from("/home/someone/.local/share/wlrix/images"),
                PathBuf::from("/opt/x/wlrix/images"),
                PathBuf::from("/usr/share/wlrix/images"),
            ]
        );
    }

    #[test]
    fn unset_variables_fall_back_to_what_the_spec_says_to_assume() {
        // An empty value counts as unset, which is what the spec says and what every other
        // XDG lookup in wlRIX does -- `XDG_DATA_DIRS=` in a stripped session environment must
        // not mean "look nowhere".
        for value in [None, Some("")] {
            let dirs = with_data_dirs(value, value, image_dirs);
            let home = std::env::var_os("HOME").map(PathBuf::from);
            let mut expected = Vec::new();
            if let Some(home) = home {
                expected.push(home.join(".local/share/wlrix/images"));
            }
            expected.push(PathBuf::from("/usr/local/share/wlrix/images"));
            expected.push(PathBuf::from("/usr/share/wlrix/images"));
            assert_eq!(dirs, expected, "for {value:?}");
        }
    }

    #[test]
    fn an_exact_name_wins_and_the_user_copy_wins_over_the_system_one() {
        let scratch = Scratch::new("exact");
        let (user, system) = (scratch.dir("user"), scratch.dir("system"));
        let mine = scratch.touch("user/com.wlrix.terminal.png");
        scratch.touch("system/com.wlrix.terminal.png");
        scratch.touch("system/default.png");

        let dirs = vec![user, system];
        assert_eq!(find(&dirs, "com.wlrix.terminal"), Some(mine));
    }

    #[test]
    fn a_name_that_differs_only_in_case_still_matches() {
        // `Blender.png` and `Lutris.png` are `WM_CLASS` names and capitalized; the same
        // programs report a lowercase app id elsewhere. Both directions have to work.
        let scratch = Scratch::new("case");
        let dir = scratch.dir("images");
        let capitalized = scratch.touch("images/Blender.png");
        let lowercase = scratch.touch("images/steam.png");

        let dirs = vec![dir];
        assert_eq!(find(&dirs, "blender"), Some(capitalized));
        assert_eq!(find(&dirs, "Steam"), Some(lowercase));
    }

    #[test]
    fn an_exact_match_in_a_later_directory_beats_a_cased_one_in_an_earlier_directory() {
        // Precedence is by how well the name matches first, by directory second. The other
        // way round, a stray `FIREFOX.PNG` in the user's directory would shadow the exact
        // `firefox.png` the system ships.
        let scratch = Scratch::new("precedence");
        let (user, system) = (scratch.dir("user"), scratch.dir("system"));
        scratch.touch("user/FIREFOX.PNG");
        let exact = scratch.touch("system/firefox.png");

        assert_eq!(find(&[user, system], "firefox"), Some(exact));
    }

    #[test]
    fn an_unmatched_application_gets_the_default() {
        let scratch = Scratch::new("default");
        let dir = scratch.dir("images");
        scratch.touch("images/com.wlrix.terminal.png");
        let fallback = scratch.touch("images/default.png");

        let dirs = vec![dir];
        assert_eq!(find(&dirs, "org.example.unknown"), Some(fallback.clone()));
        // An X11 window with no class at all, and a client that simply never set one.
        assert_eq!(find(&dirs, ""), Some(fallback));
    }

    #[test]
    fn nothing_installed_is_no_picture_rather_than_a_panic() {
        let scratch = Scratch::new("empty");
        let dirs = vec![scratch.dir("images"), scratch.0.join("does-not-exist")];
        assert_eq!(find(&dirs, "com.wlrix.terminal"), None);
        assert_eq!(find(&dirs, ""), None);
    }

    #[test]
    fn an_alias_is_a_symlink_and_resolves_through_it() {
        // `wlrix-assets` ships `Alacritty.png -> com.wlrix.terminal.png`: one drawing under a
        // second name, so a terminal with no artwork of its own looks like the wlRIX one
        // rather than falling back to the default. Both passes go through `is_file`, which
        // follows links -- but that is the whole behavior, so it is worth pinning.
        let scratch = Scratch::new("alias");
        let dir = scratch.dir("images");
        scratch.touch("images/com.wlrix.terminal.png");
        let alias = scratch.link("images/Alacritty.png", "com.wlrix.terminal.png");
        scratch.touch("images/default.png");

        let dirs = [dir];
        assert_eq!(find(&dirs, "Alacritty"), Some(alias.clone()));
        // And through the case-insensitive pass too, which reads the directory rather than
        // probing a name.
        assert_eq!(find(&dirs, "alacritty"), Some(alias));
    }

    #[test]
    fn a_dangling_alias_falls_through_rather_than_resolving_to_nothing() {
        // A link whose target was renamed away. `is_file` is false for it, so the search must
        // keep going and land on the default -- not hand back a path the decoder will fail on
        // and then cache as a blank tile.
        let scratch = Scratch::new("dangling");
        let dir = scratch.dir("images");
        scratch.link("images/Alacritty.png", "gone.png");
        let fallback = scratch.touch("images/default.png");

        assert_eq!(find(&[dir], "Alacritty"), Some(fallback));
    }

    #[test]
    fn a_directory_of_the_right_name_is_not_a_picture() {
        // `install -d` on the wrong path, or a user who made `default.png` a directory: the
        // search has to keep walking rather than hand back something that cannot be decoded.
        let scratch = Scratch::new("directory");
        let (first, second) = (scratch.dir("first"), scratch.dir("second"));
        scratch.dir("first/default.png");
        let real = scratch.touch("second/default.png");

        assert_eq!(find(&[first, second], "anything"), Some(real));
    }
}
