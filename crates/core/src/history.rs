//! Back and forward history for one window.

use std::path::{Path, PathBuf};

#[derive(Default)]
pub struct History {
    back: Vec<Stop>,
    forward: Vec<Stop>,
}

/// A place to return to: the folder, and the entry that had the cursor when we left.
#[derive(Clone, Debug, PartialEq)]
pub struct Stop {
    pub dir: PathBuf,
    pub cursor_name: Option<Vec<u8>>,
}

impl History {
    /// Records leaving `from` for somewhere new, which drops any forward stops.
    pub fn visit(&mut self, from: Stop) {
        if self.back.last().map(|s| s.dir.as_path()) != Some(from.dir.as_path()) {
            self.back.push(from);
        }
        self.forward.clear();
    }

    pub fn go_back(&mut self, current: Stop) -> Option<Stop> {
        let to = self.back.pop()?;
        self.forward.push(current);
        Some(to)
    }

    pub fn go_forward(&mut self, current: Stop) -> Option<Stop> {
        let to = self.forward.pop()?;
        self.back.push(current);
        Some(to)
    }

    pub fn can_go_back(&self) -> bool {
        !self.back.is_empty()
    }

    pub fn can_go_forward(&self) -> bool {
        !self.forward.is_empty()
    }
}

/// The folder above `dir`, with the name of `dir` so it can get the cursor.
pub fn parent_stop(dir: &Path) -> Option<Stop> {
    use std::os::unix::ffi::OsStrExt;
    let parent = dir.parent()?;
    Some(Stop { dir: parent.to_path_buf(), cursor_name: dir.file_name().map(|n| n.as_bytes().to_vec()) })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stop(p: &str) -> Stop {
        Stop { dir: p.into(), cursor_name: None }
    }

    #[test]
    fn back_and_forward_walk_the_visits() {
        let mut h = History::default();
        h.visit(stop("/a"));
        h.visit(stop("/b"));
        assert_eq!(h.go_back(stop("/c")).unwrap().dir, Path::new("/b"));
        assert_eq!(h.go_back(stop("/b")).unwrap().dir, Path::new("/a"));
        assert!(h.go_back(stop("/a")).is_none());
        assert_eq!(h.go_forward(stop("/a")).unwrap().dir, Path::new("/b"));
        assert_eq!(h.go_forward(stop("/b")).unwrap().dir, Path::new("/c"));
        assert!(!h.can_go_forward());
    }

    #[test]
    fn a_new_visit_drops_forward_stops() {
        let mut h = History::default();
        h.visit(stop("/a"));
        h.go_back(stop("/b"));
        h.visit(stop("/a"));
        assert!(!h.can_go_forward());
    }

    #[test]
    fn parent_of_root_is_none() {
        assert!(parent_stop(Path::new("/")).is_none());
        let s = parent_stop(Path::new("/x/y")).unwrap();
        assert_eq!(s.dir, Path::new("/x"));
        assert_eq!(s.cursor_name.as_deref(), Some(&b"y"[..]));
    }
}
