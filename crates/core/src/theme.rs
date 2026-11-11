//! The system theme. On aliasOS that's `~/.local/state/aliasos/current/theme/colors.toml`,
//! which `aliasos-theme-apply` replaces by moving a new folder into place. It always uses
//! that path, whatever `XDG_STATE_HOME` says, so this does too. Anywhere
//! else, it's the desktop's light or dark preference and a built-in palette.

use std::path::{Path, PathBuf};

use crate::watch::Watch;

/// `0xRRGGBB`.
pub type Rgb = u32;

/// Blends `a` towards `b`: `t` of 0 is `a`, 1 is `b`.
pub fn blend(a: Rgb, b: Rgb, t: f32) -> Rgb {
    let mix = |shift: u32| {
        let (x, y) = (((a >> shift) & 255) as f32, ((b >> shift) & 255) as f32);
        ((x + (y - x) * t).round() as u32) << shift
    };
    mix(16) | mix(8) | mix(0)
}

/// Relative luminance, 0 to 1.
pub fn luminance(c: Rgb) -> f32 {
    let lin = |shift: u32| {
        let v = ((c >> shift) & 255) as f32 / 255.0;
        if v <= 0.04045 {
            v / 12.92
        } else {
            ((v + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * lin(16) + 0.7152 * lin(8) + 0.0722 * lin(0)
}

fn contrast(a: Rgb, b: Rgb) -> f32 {
    let (x, y) = (luminance(a), luminance(b));
    (x.max(y) + 0.05) / (x.min(y) + 0.05)
}

fn readable_on(bg: Rgb) -> Rgb {
    if contrast(bg, 0xffffff) >= contrast(bg, 0x000000) {
        0xffffff
    } else {
        0x000000
    }
}

/// Every color the UI draws with, all derived from a theme's palette so any theme
/// looks coherent with no per-theme work.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Colors {
    pub dark: bool,
    pub base: Rgb,
    pub surface: Rgb,
    pub hover: Rgb,
    pub border: Rgb,
    pub foreground: Rgb,
    pub muted: Rgb,
    pub accent: Rgb,
    pub on_accent: Rgb,
    pub selection: Rgb,
    pub on_selection: Rgb,
    pub danger: Rgb,
    pub warning: Rgb,
}

/// Parses `colors.toml` by hand: only `key = "#rrggbb"` lines matter, and this file is
/// read before the first frame, so it's not worth a TOML dependency. A theme can come
/// from any git repository, so anything malformed is ignored.
pub fn parse_colors(text: &str) -> Option<Colors> {
    let get = |key: &str| -> Option<Rgb> {
        text.lines().find_map(|line| {
            let line = line.trim();
            if line.starts_with('#') {
                return None;
            }
            let (k, v) = line.split_once('=')?;
            if k.trim() != key {
                return None;
            }
            let v = v.trim().strip_prefix('"')?;
            let v = &v[..v.find('"')?];
            parse_hex(v)
        })
    };
    let bg = get("background")?;
    let fg = get("foreground")?;
    let accent = get("accent")
        .or_else(|| get("color4"))
        .or_else(|| get("blue"))
        .unwrap_or_else(|| blend(fg, bg, 0.3));
    let selection = get("selection_background")
        .or_else(|| get("selection"))
        .unwrap_or_else(|| blend(bg, accent, 0.35));
    let on_selection = get("selection_foreground").unwrap_or_else(|| {
        if contrast(selection, fg) >= 3.0 {
            fg
        } else {
            readable_on(selection)
        }
    });
    let mode = text.lines().find_map(|l| {
        let (k, v) = l.split_once('=')?;
        (k.trim() == "mode").then(|| v.trim().trim_matches('"').to_string())
    });
    let dark = match mode.as_deref() {
        Some("light") => false,
        Some("dark") => true,
        _ => luminance(bg) < 0.4,
    };
    Some(Colors {
        dark,
        base: bg,
        surface: blend(bg, fg, 0.04),
        hover: blend(bg, fg, 0.08),
        border: blend(bg, fg, 0.16),
        foreground: fg,
        muted: blend(fg, bg, 0.40),
        accent,
        on_accent: readable_on(accent),
        selection,
        on_selection,
        danger: get("color1")
            .or_else(|| get("red"))
            .unwrap_or(if luminance(bg) < 0.4 {
                0xe06c75
            } else {
                0xc0392b
            }),
        warning: get("color3")
            .or_else(|| get("yellow"))
            .unwrap_or(if luminance(bg) < 0.4 {
                0xe5c07b
            } else {
                0xb7791f
            }),
    })
}

fn parse_hex(v: &str) -> Option<Rgb> {
    let h = v.strip_prefix('#')?;
    if h.len() != 6 || !h.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    u32::from_str_radix(h, 16).ok()
}

const LIGHT: &str = "background = \"#fafafa\"\nforeground = \"#1f2328\"\naccent = \"#2f6feb\"\nselection_background = \"#cfe0fc\"\nselection_foreground = \"#1f2328\"\n";
const DARK: &str = "background = \"#1b1d21\"\nforeground = \"#d8dbe0\"\naccent = \"#5b9bf5\"\nselection_background = \"#2c466e\"\nselection_foreground = \"#eef1f5\"\n";

pub fn builtin(dark: bool) -> Colors {
    parse_colors(if dark { DARK } else { LIGHT }).expect("built-in themes parse")
}

#[derive(Clone, Debug)]
pub struct ThemePaths {
    pub state_home: PathBuf,
    pub config_home: PathBuf,
}

impl ThemePaths {
    pub fn for_home(home: &Path, config_home: &Path) -> Self {
        Self {
            state_home: home.join(".local/state"),
            config_home: config_home.to_path_buf(),
        }
    }

    fn current(&self) -> PathBuf {
        self.state_home.join("aliasos/current")
    }

    fn colors_file(&self) -> PathBuf {
        self.current().join("theme/colors.toml")
    }

    /// The deepest parts of the theme's path that exist. The whole chain matters: the
    /// `theme` folder gets replaced, and on a fresh system its parents may appear later.
    /// Higher folders are only watched while nothing below them exists, because other
    /// apps write to the state folder and each write would wake us.
    pub fn watch_dirs(&self) -> Vec<PathBuf> {
        let current = self.current();
        let theme = current.join("theme");
        if current.is_dir() {
            let mut v = vec![current];
            if theme.is_dir() {
                v.push(theme);
            }
            return v;
        }
        match current.ancestors().skip(1).find(|p| p.is_dir()) {
            Some(p) => vec![p.to_path_buf()],
            None => Vec::new(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Source {
    AliasOs,
    Desktop,
    Default,
}

/// Reads the theme. Costs a few small file reads, so it's fine before the first frame.
pub fn load(paths: &ThemePaths) -> (Colors, Source) {
    if let Some(c) = read_aliasos(paths) {
        return (c, Source::AliasOs);
    }
    if let Some(dark) = gtk_settings_dark(&paths.config_home) {
        return (builtin(dark), Source::Desktop);
    }
    (builtin(false), Source::Default)
}

fn read_aliasos(paths: &ThemePaths) -> Option<Colors> {
    let mut colors = parse_colors(&std::fs::read_to_string(paths.colors_file()).ok()?)?;
    // aliasOS marks light themes with this file too.
    if paths
        .current()
        .join("theme/light.mode")
        .symlink_metadata()
        .is_ok()
    {
        colors.dark = false;
    }
    Some(colors)
}

/// The colors after a change on disk: `Some` only for a complete aliasOS theme. Anything
/// else means keep what's showing. `aliasos-theme-apply` deletes the theme folder file
/// by file and then moves the new one in, so missing or partial files are normal for a
/// moment, and the move fires another event. With no aliasOS at all, nothing in the
/// state folder has anything to say about the desktop's preference.
pub fn reload(paths: &ThemePaths) -> Option<Colors> {
    read_aliasos(paths)
}

/// The GTK settings file, read as a cheap first guess. `gsettings` is the better answer
/// but means starting a process, so the app asks that after the first frame.
fn gtk_settings_dark(config_home: &Path) -> Option<bool> {
    for file in ["gtk-4.0/settings.ini", "gtk-3.0/settings.ini"] {
        let Ok(text) = std::fs::read_to_string(config_home.join(file)) else {
            continue;
        };
        let value = |key: &str| {
            text.lines().find_map(|l| {
                let (k, v) = l.split_once('=')?;
                (k.trim() == key).then(|| v.trim())
            })
        };
        if let Some(v) = value("gtk-application-prefer-dark-theme") {
            return Some(v == "1" || v.eq_ignore_ascii_case("true"));
        }
        // A name with "dark" in it says dark. Any other name says nothing.
        if value("gtk-theme-name").is_some_and(|v| v.to_ascii_lowercase().contains("dark")) {
            return Some(true);
        }
    }
    None
}

/// The answer from `gsettings`, if it's around. Blocks on a process, so call this off
/// the UI thread.
pub fn query_gsettings_dark() -> Option<bool> {
    let out = std::process::Command::new("gsettings")
        .args(["get", "org.gnome.desktop.interface", "color-scheme"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    parse_color_scheme(&String::from_utf8_lossy(&out.stdout))
}

fn parse_color_scheme(text: &str) -> Option<bool> {
    match text.trim().trim_matches('\'') {
        "prefer-dark" => Some(true),
        "prefer-light" => Some(false),
        // "default" says nothing either way.
        _ => None,
    }
}

/// Watches the theme's path. Replaced folders are new inodes, so watches get rebuilt
/// from scratch on every change.
pub struct ThemeWatch {
    pub watch: Watch,
    paths: ThemePaths,
}

impl ThemeWatch {
    pub fn new(paths: ThemePaths) -> rustix::io::Result<Self> {
        let w = Self {
            watch: Watch::new()?,
            paths,
        };
        w.rearm();
        Ok(w)
    }

    fn rearm(&self) {
        // Add first, then look again: a folder created meanwhile needs watching too.
        for _ in 0..4 {
            let dirs = self.paths.watch_dirs();
            self.watch.set_dirs(&dirs);
            if self.paths.watch_dirs() == dirs {
                break;
            }
        }
    }

    /// Call when the watch's fd is readable. True if the theme should be read again.
    /// Rearming happens before the read, so a change right after isn't missed.
    pub fn changed(&mut self) -> bool {
        let any = self.watch.drain();
        if any {
            self.rearm();
        }
        any
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::Scratch;
    use std::fs;

    const SAMPLE: &str = "# comment with = \"#000000\"\naccent = \"#5E92C6\"\nforeground = \"#C5CED6\"\nbackground = \"#0B111A\"\nselection_foreground = \"#E3EAF0\"\nselection_background = \"#1E3047\"\ncolor1 = \"#C25F54\"\ncolor3 = \"#D19A4E\"\n";

    fn paths(s: &Scratch) -> ThemePaths {
        ThemePaths {
            state_home: s.path().join("state"),
            config_home: s.path().join("config"),
        }
    }

    fn write_theme(s: &Scratch, text: &str) {
        s.file("state/aliasos/current/theme/colors.toml", text.as_bytes());
    }

    #[test]
    fn parses_an_aliasos_theme() {
        let c = parse_colors(SAMPLE).unwrap();
        assert_eq!(c.base, 0x0B111A);
        assert_eq!(c.foreground, 0xC5CED6);
        assert_eq!(c.accent, 0x5E92C6);
        assert_eq!(c.selection, 0x1E3047);
        assert_eq!(c.on_selection, 0xE3EAF0);
        assert_eq!(c.danger, 0xC25F54);
        assert!(c.dark);
    }

    #[test]
    fn semantic_names_stand_in_for_numbered_colors() {
        let c = parse_colors("background = \"#ffffff\"\nforeground = \"#000000\"\nred = \"#aa0000\"\nblue = \"#0000aa\"\nyellow = \"#aaaa00\"\n").unwrap();
        assert_eq!(c.danger, 0xaa0000);
        assert_eq!(c.accent, 0x0000aa);
        assert_eq!(c.warning, 0xaaaa00);
        assert!(!c.dark);
    }

    #[test]
    fn every_shipped_key_form_parses_and_inline_comments_are_ignored() {
        let c = parse_colors("background = \"#101010\" # dark\nforeground=\"#eeeeee\"\n").unwrap();
        assert_eq!(c.base, 0x101010);
        assert_eq!(c.foreground, 0xeeeeee);
    }

    #[test]
    fn malformed_themes_are_rejected_without_panicking() {
        for bad in [
            "",
            "background = \"#xyz\"\nforeground = \"#000000\"",
            "background = \"#12345\"\nforeground = \"#000000\"",
            "background = #000000\nforeground = \"#000000\"",
            "background = \"#ééé\"\nforeground = \"#000000\"",
            "background = \"#000000",
            "=\n=\"\n\"",
        ] {
            assert!(parse_colors(bad).is_none(), "{bad:?}");
        }
    }

    #[test]
    fn selection_stays_readable_when_the_theme_gives_no_foreground_for_it() {
        let c = parse_colors("background = \"#000000\"\nforeground = \"#eeeeee\"\nselection_background = \"#ffffff\"\n").unwrap();
        assert_eq!(c.on_selection, 0x000000);
    }

    #[test]
    fn aliasos_theme_wins_when_present() {
        let s = Scratch::new();
        write_theme(&s, SAMPLE);
        s.file(
            "config/gtk-4.0/settings.ini",
            b"[Settings]\ngtk-application-prefer-dark-theme=0\n",
        );
        let (c, src) = load(&paths(&s));
        assert_eq!(src, Source::AliasOs);
        assert_eq!(c.base, 0x0B111A);
    }

    #[test]
    fn no_theme_at_all_still_gives_readable_colors() {
        let s = Scratch::new();
        let (c, src) = load(&paths(&s));
        assert_eq!(src, Source::Default);
        assert!(!c.dark);
        assert!(contrast(c.base, c.foreground) > 7.0);
    }

    #[test]
    fn gtk_settings_pick_light_or_dark() {
        let s = Scratch::new();
        s.file(
            "config/gtk-4.0/settings.ini",
            b"[Settings]\ngtk-application-prefer-dark-theme=1\n",
        );
        let (c, src) = load(&paths(&s));
        assert_eq!(src, Source::Desktop);
        assert!(c.dark);
        let s = Scratch::new();
        s.file(
            "config/gtk-3.0/settings.ini",
            b"[Settings]\ngtk-theme-name=Adwaita-dark\n",
        );
        assert!(load(&paths(&s)).0.dark);
        let s = Scratch::new();
        s.file(
            "config/gtk-3.0/settings.ini",
            b"[Settings]\ngtk-icon-theme-name=Adwaita\n",
        );
        assert_eq!(load(&paths(&s)).1, Source::Default);
    }

    #[test]
    fn a_broken_aliasos_theme_falls_back() {
        let s = Scratch::new();
        write_theme(&s, "nonsense");
        assert_eq!(load(&paths(&s)).1, Source::Default);
    }

    #[test]
    fn gsettings_output_parses() {
        assert_eq!(parse_color_scheme("'prefer-dark'\n"), Some(true));
        assert_eq!(parse_color_scheme("'prefer-light'\n"), Some(false));
        assert_eq!(parse_color_scheme("'default'\n"), None);
    }

    /// The real sequence from `aliasos-theme-apply`: build a new folder, delete the old
    /// one, move the new one in.
    fn swap_theme(s: &Scratch, text: &str) {
        let cur = s.path().join("state/aliasos/current");
        let next = cur.join("next-theme");
        fs::create_dir_all(&next).unwrap();
        fs::write(next.join("colors.toml"), text).unwrap();
        fs::remove_dir_all(cur.join("theme")).unwrap();
        fs::rename(&next, cur.join("theme")).unwrap();
    }

    #[test]
    fn notices_a_swapped_theme_then_a_second_swap() {
        let s = Scratch::new();
        write_theme(&s, SAMPLE);
        let p = paths(&s);
        let mut w = ThemeWatch::new(p.clone()).unwrap();
        assert!(!w.changed());
        swap_theme(&s, "background = \"#111111\"\nforeground = \"#222222\"\n");
        assert!(w.changed());
        assert_eq!(reload(&p).unwrap().base, 0x111111);
        w.changed();
        swap_theme(&s, "background = \"#333333\"\nforeground = \"#444444\"\n");
        assert!(w.changed(), "the watch must survive the first swap");
        assert_eq!(reload(&p).unwrap().base, 0x333333);
    }

    #[test]
    fn an_edit_inside_the_replaced_folder_is_noticed() {
        let s = Scratch::new();
        write_theme(&s, SAMPLE);
        let p = paths(&s);
        let mut w = ThemeWatch::new(p.clone()).unwrap();
        swap_theme(&s, "background = \"#111111\"\nforeground = \"#222222\"\n");
        assert!(w.changed());
        w.changed();
        // A watch still on the old inode would miss this.
        write_theme(&s, "background = \"#555555\"\nforeground = \"#666666\"\n");
        assert!(w.changed());
        assert_eq!(reload(&p).unwrap().base, 0x555555);
    }

    #[test]
    fn a_theme_appearing_later_is_noticed() {
        let s = Scratch::new();
        s.dir("state");
        let p = paths(&s);
        let mut w = ThemeWatch::new(p.clone()).unwrap();
        s.dir("state/aliasos");
        assert!(w.changed());
        s.dir("state/aliasos/current");
        assert!(w.changed());
        write_theme(&s, SAMPLE);
        assert!(w.changed());
        assert_eq!(reload(&p).unwrap().base, 0x0B111A);
    }

    #[test]
    fn the_gap_during_a_swap_keeps_the_current_colors() {
        let s = Scratch::new();
        write_theme(&s, SAMPLE);
        let p = paths(&s);
        let cur = s.path().join("state/aliasos/current");
        fs::rename(cur.join("theme"), cur.join("aside")).unwrap();
        assert!(reload(&p).is_none());
        // Mid-delete: the folder is still there but its files are going.
        fs::create_dir(cur.join("theme")).unwrap();
        assert!(reload(&p).is_none());
        fs::remove_dir(cur.join("theme")).unwrap();
        fs::rename(cur.join("aside"), cur.join("theme")).unwrap();
        assert_eq!(reload(&p).unwrap().base, 0x0B111A);
    }

    #[test]
    fn reload_says_nothing_without_an_aliasos_theme() {
        let s = Scratch::new();
        s.dir("state");
        // No colors here, and the app may have a better answer from gsettings.
        assert!(reload(&paths(&s)).is_none());
        write_theme(&s, "half written");
        assert!(reload(&paths(&s)).is_none());
    }

    #[test]
    fn the_dark_preference_wins_over_an_earlier_theme_name() {
        let s = Scratch::new();
        s.file(
            "config/gtk-4.0/settings.ini",
            b"[Settings]\ngtk-theme-name=Adwaita\ngtk-application-prefer-dark-theme=1\n",
        );
        assert!(load(&paths(&s)).0.dark);
        // A plain theme name says nothing, so the older settings file still gets a say.
        let s = Scratch::new();
        s.file(
            "config/gtk-4.0/settings.ini",
            b"[Settings]\ngtk-theme-name=Adwaita\n",
        );
        s.file(
            "config/gtk-3.0/settings.ini",
            b"[Settings]\ngtk-application-prefer-dark-theme=1\n",
        );
        assert!(load(&paths(&s)).0.dark);
    }

    #[test]
    fn a_missing_state_folder_watches_the_nearest_parent() {
        let s = Scratch::new();
        let p = paths(&s);
        assert_eq!(p.watch_dirs(), vec![s.path().to_path_buf()]);
        let mut w = ThemeWatch::new(p.clone()).unwrap();
        s.dir("state");
        assert!(w.changed());
        s.dir("state/aliasos");
        assert!(w.changed());
    }

    #[test]
    fn the_aliasos_path_ignores_xdg_state_home() {
        let p = ThemePaths::for_home(Path::new("/h"), Path::new("/c"));
        assert_eq!(
            p.colors_file(),
            Path::new("/h/.local/state/aliasos/current/theme/colors.toml")
        );
    }

    #[test]
    fn hex_rejects_a_sign() {
        assert!(parse_colors("background = \"#+12345\"\nforeground = \"#000000\"\n").is_none());
    }

    #[test]
    fn the_theme_can_say_light_or_dark_outright() {
        let mid = "background = \"#8a8a8a\"\nforeground = \"#000000\"\n";
        assert!(
            !parse_colors(&format!("{mid}mode = \"light\"\n"))
                .unwrap()
                .dark
        );
        assert!(
            parse_colors(&format!("{mid}mode = \"dark\"\n"))
                .unwrap()
                .dark
        );
        let s = Scratch::new();
        write_theme(&s, "background = \"#222222\"\nforeground = \"#eeeeee\"\n");
        s.file("state/aliasos/current/theme/light.mode", b"");
        assert!(!load(&paths(&s)).0.dark);
    }

    #[test]
    fn writes_elsewhere_in_the_state_folder_dont_wake_us() {
        let s = Scratch::new();
        write_theme(&s, SAMPLE);
        let mut w = ThemeWatch::new(paths(&s)).unwrap();
        s.file("state/some-other-app/log", b"x");
        s.file("state/log", b"x");
        assert!(!w.changed());
    }
}
