//! The new names for renaming several items at once, and what's wrong with any of them.
//!
//! Two fields make the names. With text in Find, every place it appears in a name is
//! replaced with Replace. With Find empty, Replace is the whole new name, and a file keeps
//! its extension. Either way, `{n}` in Replace is a running number, 1, 2, 3 and so on in the
//! order the items are listed, padded with zeros to the same width so they sort in order.
//! Names are bytes, so one that isn't UTF-8 is only changed where the text matched.

use std::collections::HashMap;

use crate::ops::validate_name_bytes;

/// What Replace turns into a running number.
pub const COUNTER: &str = "{n}";

/// The new name for each item. `names[i]` is an item's name, and `folder[i]` says whether it's
/// a folder, which has no extension to keep.
pub fn new_names(names: &[&[u8]], folder: &[bool], find: &str, replace: &str) -> Vec<Vec<u8>> {
    let width = names.len().max(1).to_string().len();
    names
        .iter()
        .enumerate()
        .map(|(i, &name)| {
            let with = replace.replace(COUNTER, &format!("{:0width$}", i + 1)).into_bytes();
            if !find.is_empty() {
                replace_all(name, find.as_bytes(), &with)
            } else if replace.is_empty() {
                name.to_vec()
            } else {
                let keep = if folder.get(i).copied().unwrap_or(false) { &[][..] } else { extension(name) };
                [&with[..], keep].concat()
            }
        })
        .collect()
}

/// `name` with each `find` in it replaced, left to right, not overlapping.
fn replace_all(name: &[u8], find: &[u8], with: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(name.len());
    let mut rest = name;
    while let Some(at) = rest.windows(find.len()).position(|w| w == find) {
        out.extend_from_slice(&rest[..at]);
        out.extend_from_slice(with);
        rest = &rest[at + find.len()..];
    }
    out.extend_from_slice(rest);
    out
}

/// A file's extension with its dot, as rename leaves it selected: from the last dot, unless
/// that's the first character, as in `.bashrc`.
fn extension(name: &[u8]) -> &[u8] {
    match name.iter().rposition(|&b| b == b'.') {
        Some(i) if i > 0 => &name[i..],
        _ => &[],
    }
}

/// Why an item can't have its new name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Problem {
    /// The name itself won't do, and why.
    Invalid(&'static str),
    /// Another item in the list would have the same name: the first such other item.
    Twice(usize),
    /// Something else in the folder already has this name.
    Taken,
}

/// What's wrong with each new name, if anything. `taken` says whether a name is already used in
/// the folder. An item's own name isn't taken from it, and a name another item in the list is
/// leaving doesn't count as taken, so items can swap names.
pub fn problems(names: &[&[u8]], new: &[Vec<u8>], taken: impl Fn(&[u8]) -> bool) -> Vec<Option<Problem>> {
    let mut holders: HashMap<&[u8], Vec<usize>> = HashMap::new();
    for (i, n) in new.iter().enumerate() {
        holders.entry(n.as_slice()).or_default().push(i);
    }
    let leaving: std::collections::HashSet<&[u8]> = names.iter().zip(new).filter(|(old, new)| **old != new.as_slice()).map(|(old, _)| *old).collect();
    new.iter()
        .enumerate()
        .map(|(i, n)| {
            let unchanged = names.get(i).is_some_and(|old| *old == n.as_slice());
            if !unchanged && let Err(why) = validate_name_bytes(n) {
                return Some(Problem::Invalid(why));
            }
            if let Some(&other) = holders[n.as_slice()].iter().find(|&&j| j != i) {
                return Some(Problem::Twice(other));
            }
            (!unchanged && taken(n) && !leaving.contains(n.as_slice())).then_some(Problem::Taken)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(names: &[&str], find: &str, replace: &str) -> Vec<String> {
        let names: Vec<&[u8]> = names.iter().map(|n| n.as_bytes()).collect();
        let folder: Vec<bool> = names.iter().map(|n| n.ends_with(b"/")).collect();
        let names: Vec<&[u8]> = names.iter().map(|n| n.strip_suffix(b"/").unwrap_or(n)).collect();
        new_names(&names, &folder, find, replace).into_iter().map(|n| String::from_utf8(n).unwrap()).collect()
    }

    #[test]
    fn find_replaces_every_place_it_appears() {
        assert_eq!(run(&["IMG_001.jpg", "IMG_IMG.jpg", "other.png"], "IMG", "Trip"), ["Trip_001.jpg", "Trip_Trip.jpg", "other.png"]);
        assert_eq!(run(&["aaa"], "aa", "b"), ["ba"], "left to right, not overlapping");
        assert_eq!(run(&["a.JPG"], ".JPG", ".jpg"), ["a.jpg"], "the extension too");
        assert_eq!(run(&["a b c"], " ", ""), ["abc"], "replacing with nothing removes it");
        assert_eq!(run(&["a", "b"], "", ""), ["a", "b"], "nothing to do");
    }

    #[test]
    fn an_empty_find_names_the_whole_stem_and_keeps_the_extension() {
        assert_eq!(run(&["x.jpg", "y.tar.gz", ".bashrc", "notes", "dir.d/"], "", "Trip {n}"), ["Trip 1.jpg", "Trip 2.gz", "Trip 3", "Trip 4", "Trip 5"]);
    }

    #[test]
    fn the_counter_is_padded_to_the_count() {
        let names: Vec<String> = (0..12).map(|i| format!("f{i}.png")).collect();
        let names: Vec<&str> = names.iter().map(String::as_str).collect();
        let new = run(&names, "", "{n}");
        assert_eq!(new[0], "01.png");
        assert_eq!(new[11], "12.png");
        assert_eq!(run(&["a-x", "b-x"], "x", "{n}{n}"), ["a-11", "b-22"], "with Find too, and more than once");
    }

    #[test]
    fn names_that_arent_utf8_change_only_where_it_matched() {
        let odd: &[u8] = b"caf\xe9 IMG.jpg";
        let new = new_names(&[odd], &[false], "IMG", "pic");
        assert_eq!(new[0], b"caf\xe9 pic.jpg");
        assert_eq!(new_names(&[odd], &[false], "zzz", "y")[0], odd, "untouched when it doesn't match");
    }

    fn check(names: &[&str], new: &[&str], taken: &[&str]) -> Vec<Option<Problem>> {
        let names: Vec<&[u8]> = names.iter().map(|n| n.as_bytes()).collect();
        let new: Vec<Vec<u8>> = new.iter().map(|n| n.as_bytes().to_vec()).collect();
        problems(&names, &new, |n| taken.iter().any(|t| t.as_bytes() == n))
    }

    #[test]
    fn clashes_say_which_item_or_that_the_name_is_taken() {
        let folder = ["a", "b", "c", "other"];
        assert_eq!(check(&["a", "b"], &["x", "x"], &folder), [Some(Problem::Twice(1)), Some(Problem::Twice(0))]);
        assert_eq!(check(&["a", "b"], &["other", "y"], &folder), [Some(Problem::Taken), None]);
        assert_eq!(check(&["a", "b"], &["b", "a"], &folder), [None, None], "a swap is fine");
        assert_eq!(check(&["a", "b", "c"], &["b", "c", "d"], &folder), [None, None, None], "so is a chain");
        assert_eq!(check(&["a", "b"], &["b", "b"], &folder), [Some(Problem::Twice(1)), Some(Problem::Twice(0))], "b staying put keeps its name");
        assert_eq!(check(&["a", "b"], &["a", "b"], &folder), [None, None], "unchanged names are fine");
        assert_eq!(check(&["a"], &["x/y"], &folder), [Some(Problem::Invalid("A name can't contain “/”"))]);
        assert_eq!(check(&["a"], &[""], &folder), [Some(Problem::Invalid("The name can't be empty"))]);
    }
}
