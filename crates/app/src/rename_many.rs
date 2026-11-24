//! The dialog for renaming several items at once: Find and Replace fields, each old name
//! beside its new one, and what stops the rename, if anything.

use std::collections::HashSet;
use std::ffi::OsString;
use std::os::unix::ffi::OsStringExt;
use std::path::PathBuf;

use gannet_core::rename_many::{Problem, new_names, problems};

use crate::field::Field;

/// What has the keyboard in the dialog, in the order Tab goes through them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Focus {
    Find,
    Replace,
    /// One of the dialog's buttons, by its place.
    Button(usize),
}

#[derive(Clone, Debug, PartialEq)]
pub struct RenameMany {
    /// The items, in the order they're listed, which is the order `{n}` counts in.
    pub paths: Vec<PathBuf>,
    pub names: Vec<Vec<u8>>,
    folder: Vec<bool>,
    /// Every name in the folder when the dialog opened.
    taken: HashSet<Vec<u8>>,
    pub find: Field,
    pub replace: Field,
    pub focus: Focus,
    /// Each item's new name, and what's wrong with it.
    pub new: Vec<Vec<u8>>,
    pub problems: Vec<Option<Problem>>,
    /// The first row of the preview shown.
    pub scroll: usize,
}

impl RenameMany {
    /// `items` are each item's path, name and whether it's a folder, in list order. `taken` is
    /// every name in the folder.
    pub fn new(items: Vec<(PathBuf, Vec<u8>, bool)>, taken: HashSet<Vec<u8>>) -> Self {
        let mut paths = Vec::with_capacity(items.len());
        let mut names = Vec::with_capacity(items.len());
        let mut folder = Vec::with_capacity(items.len());
        for (p, n, f) in items {
            paths.push(p);
            names.push(n);
            folder.push(f);
        }
        let mut d = RenameMany { paths, names, folder, taken, find: Field::new(""), replace: Field::new(""), focus: Focus::Find, new: Vec::new(), problems: Vec::new(), scroll: 0 };
        d.update();
        d
    }

    /// Works the new names out again, after a field changed.
    pub fn update(&mut self) {
        let names: Vec<&[u8]> = self.names.iter().map(Vec::as_slice).collect();
        self.new = new_names(&names, &self.folder, &self.find.text, &self.replace.text);
        self.problems = problems(&names, &self.new, |n| self.taken.contains(n));
    }

    pub fn field_mut(&mut self) -> Option<&mut Field> {
        match self.focus {
            Focus::Find => Some(&mut self.find),
            Focus::Replace => Some(&mut self.replace),
            Focus::Button(_) => None,
        }
    }

    /// How many items would get a new name.
    pub fn changing(&self) -> usize {
        self.names.iter().zip(&self.new).filter(|(a, b)| a != b).count()
    }

    /// Whether Rename can go ahead: something changes and nothing clashes.
    pub fn can_rename(&self) -> bool {
        self.changing() > 0 && self.problems.iter().all(Option::is_none)
    }

    /// What the dialog's called.
    pub fn title(&self) -> String {
        format!("Rename {}", crate::format::count(self.names.len(), "item", "items"))
    }

    /// What becomes of the item at `i`, for a screen reader: its new name, or that it keeps
    /// its name, and what's wrong with the new one.
    pub fn outcome(&self, i: usize) -> String {
        let shown = |b: &[u8]| String::from_utf8_lossy(b).into_owned();
        let (old, new) = (&self.names[i], &self.new[i]);
        let to = if old == new { "unchanged".to_string() } else { format!("becomes “{}”", shown(new)) };
        match self.problems[i] {
            None => to,
            Some(Problem::Invalid(why)) => format!("{to}, which can't be: {why}"),
            Some(Problem::Twice(j)) => format!("{to}, as “{}” would", shown(&self.names[j])),
            Some(Problem::Taken) => format!("{to}, which is already in this folder"),
        }
    }

    /// The line under the preview: the first thing stopping the rename, naming the items, or
    /// how many change. True when it's a problem.
    pub fn summary(&self) -> (String, bool) {
        let shown = |b: &[u8]| String::from_utf8_lossy(b).into_owned();
        if let Some((i, p)) = self.problems.iter().enumerate().find_map(|(i, p)| p.map(|p| (i, p))) {
            let (old, new) = (shown(&self.names[i]), shown(&self.new[i]));
            let line = match p {
                Problem::Invalid(why) => format!("“{old}”: {why}"),
                Problem::Twice(j) => format!("“{old}” and “{}” would both be named “{new}”", shown(&self.names[j])),
                Problem::Taken => format!("“{new}” is already in this folder, so “{old}” can't have that name"),
            };
            let named = if matches!(p, Problem::Twice(_)) { 2 } else { 1 };
            let more = self.problems.iter().filter(|p| p.is_some()).count().saturating_sub(named);
            let line = if more > 0 { format!("{line} (and {more} more)") } else { line };
            return (line, true);
        }
        let n = self.changing();
        let line = match (n, self.names.len() - n) {
            (0, _) => "Type in Find and Replace to change the names. {n} in Replace counts 1, 2, 3…".to_string(),
            (n, 0) => format!("{} will be renamed", crate::format::count(n, "item", "items")),
            (n, same) => format!("{} will be renamed, {same} unchanged", crate::format::count(n, "item", "items")),
        };
        (line, false)
    }

    /// The renames to make: only the items whose name changes.
    pub fn renames(&self) -> Vec<(PathBuf, OsString)> {
        self.paths.iter().zip(&self.names).zip(&self.new).filter(|((_, old), new)| old != new).map(|((p, _), new)| (p.clone(), OsString::from_vec(new.clone()))).collect()
    }

    /// Moves the keyboard on to the next part, or back with `back`, among `buttons` buttons.
    pub fn tab(&mut self, back: bool, buttons: usize) {
        let order: Vec<Focus> = [Focus::Find, Focus::Replace].into_iter().chain((0..buttons).map(Focus::Button)).collect();
        let at = order.iter().position(|f| *f == self.focus).unwrap_or(0);
        let n = order.len();
        self.focus = order[if back { (at + n - 1) % n } else { (at + 1) % n }];
    }

    /// Keeps the preview's scroll within the rows, `shown` at a time.
    pub fn clamp_scroll(&mut self, shown: usize) {
        self.scroll = self.scroll.min(self.names.len().saturating_sub(shown.max(1)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dialog(names: &[&str], folder: &[&str]) -> RenameMany {
        let items = names.iter().map(|n| (PathBuf::from("/w").join(n), n.as_bytes().to_vec(), false)).collect();
        RenameMany::new(items, folder.iter().map(|n| n.as_bytes().to_vec()).collect())
    }

    fn type_in(d: &mut RenameMany, find: &str, replace: &str) {
        d.find = Field::new(find);
        d.replace = Field::new(replace);
        d.update();
    }

    #[test]
    fn the_summary_says_what_changes_or_which_names_clash() {
        let mut d = dialog(&["a.jpg", "b.jpg", "c.png"], &["a.jpg", "b.jpg", "c.png", "x.png"]);
        assert!(!d.can_rename(), "nothing typed, nothing to do");
        assert!(d.summary().0.contains("{n}"), "says how to use it");
        type_in(&mut d, ".jpg", ".jpeg");
        assert_eq!(d.summary(), ("2 items will be renamed, 1 unchanged".into(), false));
        assert!(d.can_rename());
        assert_eq!(d.renames(), [(PathBuf::from("/w/a.jpg"), "a.jpeg".into()), (PathBuf::from("/w/b.jpg"), "b.jpeg".into())]);
        type_in(&mut d, "", "same");
        assert_eq!(d.summary(), ("“a.jpg” and “b.jpg” would both be named “same.jpg”".into(), true));
        assert!(!d.can_rename());
        type_in(&mut d, "c", "x");
        assert_eq!(d.summary(), ("“x.png” is already in this folder, so “c.png” can't have that name".into(), true));
        type_in(&mut d, "", "{n}");
        assert_eq!(d.summary(), ("3 items will be renamed".into(), false));
        assert_eq!(d.new, [b"1.jpg".to_vec(), b"2.jpg".to_vec(), b"3.png".to_vec()]);
    }

    #[test]
    fn tab_goes_round_the_fields_and_buttons() {
        let mut d = dialog(&["a", "b"], &[]);
        let mut seen = vec![d.focus];
        for _ in 0..4 {
            d.tab(false, 2);
            seen.push(d.focus);
        }
        assert_eq!(seen, [Focus::Find, Focus::Replace, Focus::Button(0), Focus::Button(1), Focus::Find]);
        d.tab(true, 2);
        assert_eq!(d.focus, Focus::Button(1));
    }
}
