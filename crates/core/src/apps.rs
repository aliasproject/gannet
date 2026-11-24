//! The apps installed, from their desktop entries, and which of them open a type of file, from
//! the `mimeapps.list` files, as the freedesktop Desktop Entry and MIME Applications specs have
//! it. Also making an app the default for a type. Nothing here starts an app: it gives the
//! command lines to run.

use std::collections::HashMap;
use std::ffi::OsString;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use crate::thumbnailers::split_exec;

/// Bigger than any real entry or list, so a file that isn't one costs nothing to turn away.
const MAX_FILE: u64 = 256 * 1024;
/// Folders inside `applications`, and entries in all, that are worth reading.
const MAX_DEPTH: usize = 4;
const MAX_ENTRIES: usize = 20_000;

pub const DEFAULTS: &str = "Default Applications";
const ADDED: &str = "Added Associations";
const REMOVED: &str = "Removed Associations";

/// One app, from its desktop entry.
#[derive(Clone, Debug, PartialEq)]
pub struct App {
    /// The desktop file ID, like `org.gnome.TextEditor.desktop`.
    pub id: String,
    pub name: String,
    /// The command line, before its `%` codes are filled in.
    pub exec: Vec<String>,
    pub icon: Option<String>,
    /// Whether it runs in a terminal.
    pub terminal: bool,
    /// The folder to start it in, when it names one.
    pub dir: Option<PathBuf>,
    /// The entry's file, for `%k`.
    pub file: PathBuf,
    pub mimes: Vec<String>,
    /// Whether it belongs in a list of apps: not `NoDisplay`, and meant for this desktop.
    pub shown: bool,
}

/// One `mimeapps.list`: each section's types and the desktop file IDs for each.
#[derive(Debug, Default)]
struct List {
    defaults: HashMap<String, Vec<String>>,
    added: HashMap<String, Vec<String>>,
    removed: HashMap<String, Vec<String>>,
}

#[derive(Debug, Default)]
pub struct Apps {
    list: Vec<App>,
    by_id: HashMap<String, usize>,
    /// Most important first.
    lists: Vec<List>,
}

/// The `applications` folders, most important first: the user's, then each of the system's.
pub fn app_dirs(data_home: &Path, data_dirs: &[PathBuf]) -> Vec<PathBuf> {
    std::iter::once(data_home).chain(data_dirs.iter().map(PathBuf::as_path)).map(|d| d.join("applications")).collect()
}

/// The `mimeapps.list` files to read, most important first, as the spec orders them: the
/// user's config, the system's config, then the user's and system's data folders, each with
/// the desktop's own file before the shared one. `desktops` are `XDG_CURRENT_DESKTOP`'s names.
pub fn list_files(config_home: &Path, config_dirs: &[PathBuf], data_home: &Path, data_dirs: &[PathBuf], desktops: &[String]) -> Vec<PathBuf> {
    let configs = std::iter::once(config_home.to_path_buf()).chain(config_dirs.iter().cloned());
    let datas = app_dirs(data_home, data_dirs);
    let mut out = Vec::new();
    for dir in configs.chain(datas) {
        for d in desktops.iter().filter(|d| !d.is_empty() && !d.contains('/')) {
            out.push(dir.join(format!("{}-mimeapps.list", d.to_ascii_lowercase())));
        }
        out.push(dir.join("mimeapps.list"));
    }
    out
}

/// The file "Always use" writes a type's default to: the user's own list, or, when the
/// desktop's own list in the user's config already names a default for the type, that one,
/// as it's read first and would win. `lists` are `list_files`'s, most important first.
pub fn user_list(config_home: &Path, lists: &[PathBuf], mime: &str) -> PathBuf {
    let own = config_home.join("mimeapps.list");
    lists.iter().take_while(|l| **l != own).filter(|l| l.parent() == Some(config_home)).find(|l| read_capped(l).is_some_and(|t| parse_list(&t).defaults.contains_key(mime))).cloned().unwrap_or(own)
}

impl Apps {
    /// Reads every desktop entry in `app_dirs` (most important first, where the first entry for
    /// an ID wins) and every list in `lists`. `lang` is the locale names are wanted in, like
    /// `de_DE.UTF-8`. `path` is where programs are looked for, as an entry whose program isn't
    /// installed is left out.
    pub fn load(app_dirs: &[PathBuf], lists: &[PathBuf], desktops: &[String], lang: Option<&str>, path: &[PathBuf]) -> Self {
        let mut out = Apps::default();
        let mut seen = 0;
        for dir in app_dirs {
            let mut found = Vec::new();
            collect(dir, dir, 0, &mut found, &mut seen);
            found.sort();
            for (id, file) in found {
                // An ID the more important folder had, even hidden, can't come back.
                if out.by_id.contains_key(&id) {
                    continue;
                }
                let Some(text) = read_capped(&file) else { continue };
                let entry = parse_entry(&text, &id, &file, desktops, lang, path);
                // A hidden or broken entry still hides the less important ones with its ID.
                out.by_id.insert(id, usize::MAX);
                if let Some(app) = entry {
                    let i = out.list.len();
                    out.by_id.insert(app.id.clone(), i);
                    out.list.push(app);
                }
            }
        }
        out.by_id.retain(|_, i| *i != usize::MAX);
        out.lists = lists.iter().filter_map(|f| read_capped(f)).map(|t| parse_list(&t)).collect();
        out
    }

    pub fn get(&self, id: &str) -> Option<&App> {
        self.by_id.get(id).and_then(|&i| self.list.get(i))
    }

    /// An app by an ID with or without `.desktop`, as the app chooser portal names them.
    pub fn find(&self, id: &str) -> Option<&App> {
        self.get(id).or_else(|| self.get(&format!("{id}.desktop")))
    }

    /// The apps for a type, the default first, then the rest in the order the lists and entries
    /// give them. `types` is the type then the types it's a kind of, nearest first, as
    /// `MimeDb::ancestors` gives them.
    pub fn for_type(&self, types: &[String]) -> Vec<&App> {
        let mut out: Vec<usize> = Vec::new();
        let mut push = |i: usize| {
            if !out.contains(&i) {
                out.push(i);
            }
        };
        if let Some(i) = self.listed_default(types) {
            push(i);
        }
        for t in types {
            for i in self.associated(t) {
                push(i);
            }
        }
        out.into_iter().filter_map(|i| self.list.get(i)).collect()
    }

    /// The app that opens a type: the first named as its default, or its parents', that's
    /// installed, or else the first app for it.
    pub fn default_for(&self, types: &[String]) -> Option<&App> {
        self.for_type(types).into_iter().next()
    }

    /// The app a list names as the default for a type, or for its nearest parent that has one,
    /// if it's installed.
    pub fn named_default(&self, types: &[String]) -> Option<&App> {
        self.listed_default(types).and_then(|i| self.list.get(i))
    }

    /// The apps for every one of several types, in the first type's order.
    pub fn for_all(&self, each: &[Vec<String>]) -> Vec<&App> {
        let Some((first, rest)) = each.split_first() else { return Vec::new() };
        let others: Vec<Vec<&App>> = rest.iter().map(|t| self.for_type(t)).collect();
        self.for_type(first).into_iter().filter(|a| others.iter().all(|o| o.iter().any(|b| b.id == a.id))).collect()
    }

    /// Every app meant to be listed, by name.
    pub fn all_shown(&self) -> Vec<&App> {
        let mut v: Vec<&App> = self.list.iter().filter(|a| a.shown).collect();
        v.sort_by_cached_key(|a| (a.name.to_lowercase(), a.id.clone()));
        v
    }

    fn listed_default(&self, types: &[String]) -> Option<usize> {
        types.iter().find_map(|t| self.lists.iter().flat_map(|l| l.defaults.get(t).into_iter().flatten()).find_map(|id| self.by_id.get(id).copied()))
    }

    /// The apps the lists add for exactly this type, then those whose entries name it, less
    /// those a list removes. A list's removals hide what less important lists and the
    /// entries give, not what it or more important ones add.
    fn associated(&self, mime: &str) -> Vec<usize> {
        let mut removed: Vec<&str> = Vec::new();
        let mut out: Vec<usize> = Vec::new();
        for l in &self.lists {
            for id in l.added.get(mime).into_iter().flatten() {
                if let Some(&i) = self.by_id.get(id)
                    && !removed.contains(&id.as_str())
                    && !out.contains(&i)
                {
                    out.push(i);
                }
            }
            removed.extend(l.removed.get(mime).into_iter().flatten().map(String::as_str));
        }
        for (i, app) in self.list.iter().enumerate() {
            if app.mimes.iter().any(|m| m == mime) && !removed.contains(&app.id.as_str()) && !out.contains(&i) {
                out.push(i);
            }
        }
        out
    }
}

/// Every `.desktop` file under `dir`, with its ID: its path inside `root` with `/` as `-`.
fn collect(root: &Path, dir: &Path, depth: usize, out: &mut Vec<(String, PathBuf)>, seen: &mut usize) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        *seen += 1;
        if *seen > MAX_ENTRIES {
            return;
        }
        let path = e.path();
        let Ok(meta) = std::fs::metadata(&path) else { continue };
        if meta.is_dir() {
            if depth < MAX_DEPTH {
                collect(root, &path, depth + 1, out, seen);
            }
        } else if meta.is_file() && path.extension().is_some_and(|x| x == "desktop") {
            let Ok(rel) = path.strip_prefix(root) else { continue };
            let Some(rel) = rel.to_str() else { continue };
            out.push((rel.replace('/', "-"), path));
        }
    }
}

fn read_capped(path: &Path) -> Option<String> {
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file() || meta.len() > MAX_FILE {
        return None;
    }
    std::fs::read_to_string(path).ok()
}

/// A key file's groups, each as its key and value pairs, in order. Comments and anything that
/// isn't a key go.
fn groups(text: &str) -> Vec<(&str, Vec<(&str, &str)>)> {
    let mut out: Vec<(&str, Vec<(&str, &str)>)> = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            out.push((name, Vec::new()));
        } else if let (Some((key, value)), Some(g)) = (line.split_once('='), out.last_mut()) {
            g.1.push((key.trim(), value.trim()));
        }
    }
    out
}

/// A string value as written, with its escapes turned back into what they stand for.
fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
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

/// A list value: items between semicolons, empty ones dropped.
fn items(s: &str) -> Vec<String> {
    s.split(';').map(str::trim).filter(|i| !i.is_empty()).map(str::to_string).collect()
}

/// How well a key's locale, the part in `Name[...]`, suits `lang`: lower is better, `None`
/// is not at all. The spec's order is lang_COUNTRY@MODIFIER, lang_COUNTRY, lang@MODIFIER, lang.
fn locale_rank(key_locale: &str, lang: &str) -> Option<u8> {
    let (rest, modifier) = lang.split_once('@').map_or((lang, None), |(r, m)| (r, Some(m)));
    let rest = rest.split('.').next().unwrap_or(rest);
    let (language, country) = rest.split_once('_').map_or((rest, None), |(l, c)| (l, Some(c)));
    let candidates = [
        country.zip(modifier).map(|(c, m)| format!("{language}_{c}@{m}")),
        country.map(|c| format!("{language}_{c}")),
        modifier.map(|m| format!("{language}@{m}")),
        Some(language.to_string()),
    ];
    candidates.iter().position(|c| c.as_deref() == Some(key_locale)).map(|p| p as u8)
}

/// Whether a program can be run: an executable file at its path, or by its name in `path`.
fn installed(program: &str, path: &[PathBuf]) -> bool {
    let runnable = |p: &Path| std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0);
    let p = Path::new(program);
    if p.is_absolute() {
        return runnable(p);
    }
    p.components().count() == 1 && path.iter().any(|d| runnable(&d.join(p)))
}

/// The app an entry describes. `None` for one that's hidden, isn't an app, has no command,
/// or whose program isn't installed.
fn parse_entry(text: &str, id: &str, file: &Path, desktops: &[String], lang: Option<&str>, path: &[PathBuf]) -> Option<App> {
    let (_, keys) = groups(text).into_iter().find(|(g, _)| *g == "Desktop Entry")?;
    let get = |k: &str| keys.iter().find(|(key, _)| *key == k).map(|(_, v)| *v);
    let yes = |k: &str| get(k).is_some_and(|v| v == "true");
    if get("Type") != Some("Application") || yes("Hidden") {
        return None;
    }
    let exec = split_exec(&unescape(get("Exec")?)).filter(|e| e.first().is_some_and(|p| !p.is_empty()))?;
    if get("TryExec").is_some_and(|t| !installed(&unescape(t), path)) || !installed(&exec[0], path) {
        return None;
    }
    // The best translation of the name there is, or the name itself.
    let mut name = get("Name").map(unescape);
    if let Some(lang) = lang.filter(|l| !l.is_empty() && *l != "C" && *l != "POSIX") {
        let best = keys.iter().filter_map(|(k, v)| Some((locale_rank(k.strip_prefix("Name[")?.strip_suffix(']')?, lang)?, *v))).min_by_key(|(r, _)| *r);
        if let Some((_, v)) = best {
            name = Some(unescape(v));
        }
    }
    let name = name.filter(|n| !n.trim().is_empty())?;
    let on_desktop = |k: &str| get(k).map(|v| items(v).iter().any(|d| desktops.iter().any(|c| c.eq_ignore_ascii_case(d))));
    let meant_here = on_desktop("OnlyShowIn").unwrap_or(true) && !on_desktop("NotShowIn").unwrap_or(false);
    Some(App {
        id: id.to_string(),
        name,
        exec,
        icon: get("Icon").map(unescape).filter(|i| !i.is_empty()),
        terminal: yes("Terminal"),
        dir: get("Path").map(|p| PathBuf::from(unescape(p))).filter(|p| p.is_absolute()),
        file: file.to_path_buf(),
        mimes: get("MimeType").map(items).unwrap_or_default(),
        shown: !yes("NoDisplay") && meant_here,
    })
}

fn parse_list(text: &str) -> List {
    let mut l = List::default();
    for (group, keys) in groups(text) {
        let section = match group {
            DEFAULTS => &mut l.defaults,
            ADDED => &mut l.added,
            REMOVED => &mut l.removed,
            _ => continue,
        };
        for (mime, ids) in keys {
            // A type named twice in one section keeps its first line, as the spec's readers do.
            section.entry(mime.to_string()).or_insert_with(|| items(ids));
        }
    }
    l
}

impl App {
    /// Whether its command line has a place for files. One without, like a settings app, can't
    /// open a file.
    pub fn takes_files(&self) -> bool {
        self.exec.iter().skip(1).any(|w| ["%f", "%F", "%u", "%U"].iter().any(|c| w.contains(c)))
    }

    /// The command lines that open `files` with this app: one for them all if it takes several,
    /// one for each if it takes one, and one with none if it takes none. `terminal` goes in
    /// front for an app that runs in one, like `xdg-terminal-exec`.
    pub fn commands(&self, files: &[PathBuf], terminal: &[String]) -> Vec<Vec<OsString>> {
        let words = &self.exec;
        let takes_many = words.iter().any(|w| w == "%F" || w == "%U");
        // A list code inside a word, like `--files=%F`, can only hold one file there.
        let takes_one = !takes_many && self.takes_files();
        let groups: Vec<&[PathBuf]> = if takes_many || files.is_empty() {
            vec![files]
        } else if takes_one {
            files.chunks(1).collect()
        } else {
            vec![&[]]
        };
        groups.into_iter().map(|g| {
            let mut out: Vec<OsString> = if self.terminal { terminal.iter().map(OsString::from).collect() } else { Vec::new() };
            out.extend(self.fill(g));
            out
        }).collect()
    }

    /// The command line with its codes filled in for `files`.
    fn fill(&self, files: &[PathBuf]) -> Vec<OsString> {
        let uri = |p: &PathBuf| OsString::from(crate::uri::to_file_uri(p));
        let mut out: Vec<OsString> = Vec::new();
        for (n, word) in self.exec.iter().enumerate() {
            // A word that's only a code can be any bytes, as a path can.
            match word.as_str() {
                "%F" => out.extend(files.iter().map(OsString::from)),
                "%U" => out.extend(files.iter().map(uri)),
                "%f" => out.extend(files.first().map(OsString::from)),
                "%u" => out.extend(files.first().map(uri)),
                "%i" => {
                    if let Some(icon) = &self.icon {
                        out.push("--icon".into());
                        out.push(icon.into());
                    }
                }
                "%d" | "%D" | "%n" | "%N" | "%v" | "%m" if n > 0 => {}
                _ => {
                    let mut filled = String::new();
                    let mut chars = word.chars();
                    while let Some(c) = chars.next() {
                        if c != '%' {
                            filled.push(c);
                            continue;
                        }
                        match chars.next() {
                            Some('%') => filled.push('%'),
                            Some('c') => filled.push_str(&self.name),
                            Some('k') => filled.push_str(&self.file.to_string_lossy()),
                            Some('f' | 'F') => filled.push_str(&files.first().map_or_else(String::new, |p| p.to_string_lossy().into_owned())),
                            Some('u' | 'U') => filled.push_str(&files.first().map_or_else(String::new, |p| crate::uri::to_file_uri(p))),
                            // Codes that are gone from the spec, or unknown, come out.
                            _ => {}
                        }
                    }
                    out.push(filled.into());
                }
            }
        }
        out
    }
}

/// Makes `id` the default app for `mime` in the user's list, keeping everything else in it.
/// A list that's a link, as a dotfiles folder makes it, is written where it leads.
pub fn set_default(list: &Path, mime: &str, id: &str) -> std::io::Result<()> {
    let target = if list.symlink_metadata().is_ok_and(|m| m.file_type().is_symlink()) { std::fs::canonicalize(list)? } else { list.to_path_buf() };
    let text = match std::fs::read_to_string(&target) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e),
    };
    let updated = with_default(&text, mime, id);
    let dir = target.parent().ok_or_else(|| std::io::Error::other("the list has no folder"))?;
    std::fs::create_dir_all(dir)?;
    // Numbered too, as two Open With windows from one list can each write a default at once.
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let tmp = dir.join(format!(".mimeapps.list.gannet-{}-{}", std::process::id(), NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)));
    std::fs::write(&tmp, updated)?;
    std::fs::rename(&tmp, &target).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

/// The list's text with `mime=id;` as the type's default line, in place of any it had.
fn with_default(text: &str, mime: &str, id: &str) -> String {
    let line = format!("{mime}={id};");
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
    let header = format!("[{DEFAULTS}]");
    let Some(start) = lines.iter().position(|l| l.trim() == header) else {
        if lines.last().is_some_and(|l| !l.trim().is_empty()) {
            lines.push(String::new());
        }
        lines.push(header);
        lines.push(line);
        return lines.join("\n") + "\n";
    };
    let end = lines[start + 1..].iter().position(|l| l.trim().starts_with('[')).map_or(lines.len(), |p| start + 1 + p);
    let same = |l: &String| l.split_once('=').is_some_and(|(k, _)| k.trim() == mime);
    match lines[start + 1..end].iter().position(same) {
        Some(p) => lines[start + 1 + p] = line,
        None => lines.insert(start + 1, line),
    }
    // Any later line for the type in the section would be ignored, but it would confuse.
    let mut i = start + 1;
    let mut kept_one = false;
    while i < lines.len() && !lines[i].trim().starts_with('[') {
        if same(&lines[i]) {
            if kept_one {
                lines.remove(i);
                continue;
            }
            kept_one = true;
        }
        i += 1;
    }
    lines.join("\n") + "\n"
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::Scratch;

    fn entry(name: &str, exec: &str, mimes: &str) -> String {
        format!("[Desktop Entry]\nType=Application\nName={name}\nExec={exec}\nMimeType={mimes}\n")
    }

    /// A folder of programs, so entries' commands count as installed.
    fn programs(s: &Scratch, names: &[&str]) -> Vec<PathBuf> {
        let bin = s.dir("bin");
        for n in names {
            let p = s.file(&format!("bin/{n}"), b"#!/bin/sh\n");
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        vec![bin]
    }

    fn ids(apps: &[&App]) -> Vec<String> {
        apps.iter().map(|a| a.id.clone()).collect()
    }

    fn types(t: &[&str]) -> Vec<String> {
        t.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn entries_are_read_with_the_users_first_and_ids_from_their_paths() {
        let s = Scratch::new();
        let path = programs(&s, &["edit", "view", "old", "kde-thing"]);
        s.file("user/applications/edit.desktop", entry("My Editor", "edit %F", "text/plain;").as_bytes());
        s.file("sys/applications/edit.desktop", entry("System Editor", "edit %F", "text/plain;text/markdown;").as_bytes());
        s.file("sys/applications/vendor/view.desktop", entry("Viewer", "view %u", "image/png;").as_bytes());
        s.file("sys/applications/gone.desktop", entry("Gone", "missing-program %f", "text/plain;").as_bytes());
        s.file("sys/applications/tried.desktop", format!("{}TryExec=not-here\n", entry("Tried", "edit", "text/plain;")).as_bytes());
        s.file("sys/applications/link.desktop", b"[Desktop Entry]\nType=Link\nName=Web\nURL=https://example.com\n");
        s.file("user/applications/old.desktop", b"[Desktop Entry]\nType=Application\nName=Old\nExec=old\nHidden=true\n");
        s.file("sys/applications/old.desktop", entry("Old", "old %f", "text/plain;").as_bytes());
        s.file("sys/applications/kde.desktop", format!("{}OnlyShowIn=KDE;\n", entry("KDE Thing", "kde-thing %f", "text/plain;")).as_bytes());
        s.file("sys/applications/notes.txt", entry("Not one", "edit", "text/plain;").as_bytes());
        let dirs = app_dirs(&s.path().join("user"), &[s.path().join("sys")]);
        let apps = Apps::load(&dirs, &[], &["Hyprland".into()], None, &path);
        assert_eq!(apps.get("edit.desktop").unwrap().name, "My Editor", "the user's own wins");
        assert_eq!(apps.get("edit.desktop").unwrap().mimes, ["text/plain"]);
        assert_eq!(apps.get("vendor-view.desktop").unwrap().name, "Viewer", "a subfolder's entry has the folder in its ID");
        assert!(apps.get("gone.desktop").is_none(), "its program isn't installed");
        assert!(apps.get("tried.desktop").is_none(), "its TryExec isn't installed");
        assert!(apps.get("link.desktop").is_none(), "not an app");
        assert!(apps.get("old.desktop").is_none(), "hidden by the user's entry");
        assert!(apps.get("kde.desktop").is_some_and(|a| !a.shown), "for another desktop: it opens files but isn't listed");
        assert!(apps.find("vendor-view").is_some(), "found without .desktop, as the portal names them");
        assert_eq!(ids(&apps.all_shown()), ["edit.desktop", "vendor-view.desktop"]);
    }

    #[test]
    fn names_come_in_the_users_language() {
        let s = Scratch::new();
        let path = programs(&s, &["edit"]);
        s.file("a/applications/e.desktop", b"[Desktop Entry]\nType=Application\nName=Editor\nName[de]=Bearbeiter\nName[de_AT]=Editor AT\nName[sr@latin]=Urednik\nExec=edit\n[Desktop Action new]\nName=Ignored\n");
        let name = |lang: Option<&str>| Apps::load(&[s.path().join("a/applications")], &[], &[], lang, &path).get("e.desktop").unwrap().name.clone();
        assert_eq!(name(None), "Editor");
        assert_eq!(name(Some("de_DE.UTF-8")), "Bearbeiter");
        assert_eq!(name(Some("de_AT.UTF-8")), "Editor AT");
        assert_eq!(name(Some("sr_RS.UTF-8@latin")), "Urednik");
        assert_eq!(name(Some("fr_FR.UTF-8")), "Editor");
        assert_eq!(name(Some("C")), "Editor");
    }

    #[test]
    fn the_lists_set_defaults_and_add_and_remove_apps() {
        let s = Scratch::new();
        let path = programs(&s, &["a", "b", "c", "d", "e"]);
        for (id, mimes) in [("a", "text/plain;"), ("b", "text/plain;text/markdown;"), ("c", "image/png;"), ("d", "text/markdown;"), ("e", "text/plain;")] {
            s.file(&format!("data/applications/{id}.desktop"), entry(&id.to_uppercase(), &format!("{id} %f"), mimes).as_bytes());
        }
        s.file("config/hyprland-mimeapps.list", b"[Default Applications]\ntext/markdown=missing.desktop;d.desktop;\n");
        s.file("config/mimeapps.list", b"[Default Applications]\ntext/markdown=b.desktop\ntext/plain=e.desktop\n\n[Added Associations]\ntext/markdown=c.desktop;\n[Removed Associations]\ntext/plain=a.desktop;\n");
        s.file("data/applications/mimeapps.list", b"[Added Associations]\ntext/plain=a.desktop;c.desktop;\n");
        let lists = list_files(&s.path().join("config"), &[], &s.path().join("data"), &[], &["Hyprland".into()]);
        assert_eq!(lists[0], s.path().join("config/hyprland-mimeapps.list"));
        let apps = Apps::load(&app_dirs(&s.path().join("data"), &[]), &lists, &["Hyprland".into()], None, &path);
        assert_eq!(ids(&apps.for_type(&types(&["text/markdown"]))), ["d.desktop", "c.desktop", "b.desktop"], "the desktop's own list first, skipping what isn't installed");
        assert_eq!(apps.default_for(&types(&["text/markdown"])).unwrap().id, "d.desktop");
        assert_eq!(apps.named_default(&types(&["text/x-c", "text/plain"])).unwrap().id, "e.desktop", "a parent's");
        assert!(apps.named_default(&types(&["image/png"])).is_none(), "none named");
        assert_eq!(ids(&apps.for_type(&types(&["text/plain"]))), ["e.desktop", "c.desktop", "b.desktop"], "a.desktop removed, even where a less important list adds it");
        assert_eq!(ids(&apps.for_type(&types(&["text/x-c", "text/plain"]))), ["e.desktop", "c.desktop", "b.desktop"], "a parent's default and apps");
        assert_eq!(ids(&apps.for_type(&types(&["image/png"]))), ["c.desktop"], "no default named, so the first");
        assert!(apps.for_type(&types(&["video/mp4"])).is_empty());
        assert_eq!(ids(&apps.for_all(&[types(&["text/markdown"]), types(&["text/plain"])])), ["c.desktop", "b.desktop"], "only what opens both");
    }

    #[test]
    fn command_lines_are_filled_in() {
        let app = |exec: &str| App {
            id: "x.desktop".into(),
            name: "X App".into(),
            exec: split_exec(&unescape(exec)).unwrap(),
            icon: Some("x-icon".into()),
            terminal: false,
            dir: None,
            file: "/usr/share/applications/x.desktop".into(),
            mimes: vec![],
            shown: true,
        };
        let files = [PathBuf::from("/d/a b.txt"), PathBuf::from("/d/é")];
        let os = |v: &[&str]| v.iter().map(OsString::from).collect::<Vec<_>>();
        assert_eq!(app("x %F").commands(&files, &[]), [os(&["x", "/d/a b.txt", "/d/é"])]);
        assert_eq!(app("x --new %U").commands(&files, &[]), [os(&["x", "--new", "file:///d/a%20b.txt", "file:///d/%C3%A9"])]);
        assert_eq!(app("x %f").commands(&files, &[]), [os(&["x", "/d/a b.txt"]), os(&["x", "/d/é"])], "one at a time");
        assert_eq!(app("x").commands(&files, &[]), [os(&["x"])], "takes no files");
        assert!(!app("x --about").takes_files() && app("x %U").takes_files() && app("x --files=%F").takes_files());
        assert_eq!(app("x --files=%F").commands(&files, &[]), [os(&["x", "--files=/d/a b.txt"]), os(&["x", "--files=/d/é"])], "a list code inside a word holds one");
        assert_eq!(app("x %i %c %k %% %d --file=%f").commands(&files[..1], &[]), [os(&["x", "--icon", "x-icon", "X App", "/usr/share/applications/x.desktop", "%", "--file=/d/a b.txt"])]);
        assert_eq!(app(r#""/opt/my app/x" "--title=a \\"b\\"" %F"#).commands(&files[..1], &[]), [os(&["/opt/my app/x", "--title=a \"b\"", "/d/a b.txt"])], "quoting and escapes");
        let mut t = app("nvim %F");
        t.terminal = true;
        assert_eq!(t.commands(&files[..1], &["xdg-terminal-exec".into()]), [os(&["xdg-terminal-exec", "nvim", "/d/a b.txt"])]);
    }

    #[test]
    fn a_default_is_written_keeping_the_rest() {
        assert_eq!(with_default("", "text/plain", "e.desktop"), "[Default Applications]\ntext/plain=e.desktop;\n");
        let had = "[Added Associations]\ntext/plain=a.desktop;\n\n[Default Applications]\nimage/png=v.desktop\ntext/plain=old.desktop;\ntext/plain=older.desktop\n[Removed Associations]\ntext/plain=b.desktop\n";
        assert_eq!(with_default(had, "text/plain", "e.desktop"), "[Added Associations]\ntext/plain=a.desktop;\n\n[Default Applications]\nimage/png=v.desktop\ntext/plain=e.desktop;\n[Removed Associations]\ntext/plain=b.desktop\n");
        assert_eq!(with_default("[Added Associations]\nx/y=a;", "text/plain", "e.desktop"), "[Added Associations]\nx/y=a;\n\n[Default Applications]\ntext/plain=e.desktop;\n");
        assert_eq!(with_default("[Default Applications]\nimage/png=v.desktop\n", "text/plain", "e.desktop"), "[Default Applications]\ntext/plain=e.desktop;\nimage/png=v.desktop\n");

        let s = Scratch::new();
        let config = s.path().join("config");
        let lists = list_files(&config, &[s.path().join("etc")], &s.path().join("data"), &[], &["Hyprland".into()]);
        let list = user_list(&config, &lists, "text/plain");
        assert_eq!(list, config.join("mimeapps.list"), "the user's own list, made if it isn't there");
        // The desktop's own list is read first, so a default there is changed there.
        s.file("config/hyprland-mimeapps.list", b"[Default Applications]\nimage/png=v.desktop\n");
        s.file("etc/hyprland-mimeapps.list", b"[Default Applications]\ntext/plain=sys.desktop\n");
        assert_eq!(user_list(&config, &lists, "image/png"), config.join("hyprland-mimeapps.list"));
        assert_eq!(user_list(&config, &lists, "text/plain"), list, "not the system's");
        set_default(&list, "text/plain", "e.desktop").unwrap();
        assert_eq!(std::fs::read_to_string(&list).unwrap(), "[Default Applications]\ntext/plain=e.desktop;\n", "the folder is made");
        // A list kept in a dotfiles folder stays a link.
        let real = s.file("dotfiles/mimeapps.list", b"[Default Applications]\nimage/png=v.desktop\n");
        let linked = s.path().join("config2/mimeapps.list");
        std::fs::create_dir_all(linked.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&real, &linked).unwrap();
        set_default(&linked, "text/plain", "e.desktop").unwrap();
        assert!(linked.symlink_metadata().unwrap().file_type().is_symlink());
        assert_eq!(std::fs::read_to_string(&real).unwrap(), "[Default Applications]\ntext/plain=e.desktop;\nimage/png=v.desktop\n");
        let apps = Apps::load(&[], &[list], &[], None, &[]);
        assert!(apps.default_for(&types(&["text/plain"])).is_none(), "named but not installed");
    }
}
