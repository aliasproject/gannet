//! A single line of editable text. Offsets are bytes on character boundaries.

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Field {
    pub text: String,
    pub caret: usize,
    /// The other end of the selection; equals `caret` when nothing's selected.
    pub anchor: usize,
}

impl Field {
    pub fn new(text: &str) -> Self {
        Self {
            text: text.to_string(),
            caret: text.len(),
            anchor: text.len(),
        }
    }

    pub fn has_selection(&self) -> bool {
        self.caret != self.anchor
    }

    pub fn selection(&self) -> (usize, usize) {
        (self.caret.min(self.anchor), self.caret.max(self.anchor))
    }

    pub fn select_all(&mut self) {
        self.anchor = 0;
        self.caret = self.text.len();
    }

    /// Selects the part of a file name before its extension, as rename should.
    pub fn select_stem(&mut self) {
        self.anchor = 0;
        self.caret = match self.text.rfind('.') {
            Some(i) if i > 0 => i,
            _ => self.text.len(),
        };
    }

    pub fn insert(&mut self, s: &str) {
        self.delete_selection();
        // Control characters would be invisible in a file name.
        let clean: String = s.chars().filter(|c| !c.is_control()).collect();
        self.text.insert_str(self.caret, &clean);
        self.caret += clean.len();
        self.anchor = self.caret;
    }

    fn delete_selection(&mut self) {
        let (a, b) = self.selection();
        self.text.replace_range(a..b, "");
        self.caret = a;
        self.anchor = a;
    }

    fn prev(&self, i: usize) -> usize {
        self.text[..i]
            .char_indices()
            .next_back()
            .map_or(0, |(p, _)| p)
    }

    fn next(&self, i: usize) -> usize {
        self.text[i..]
            .chars()
            .next()
            .map_or(i, |c| i + c.len_utf8())
    }

    fn word_left(&self, mut i: usize) -> usize {
        let is_word = |c: char| c.is_alphanumeric();
        while i > 0 && !self.text[self.prev(i)..i].chars().all(is_word) {
            i = self.prev(i);
        }
        while i > 0 && self.text[self.prev(i)..i].chars().all(is_word) {
            i = self.prev(i);
        }
        i
    }

    fn word_right(&self, mut i: usize) -> usize {
        let is_word = |c: char| c.is_alphanumeric();
        while i < self.text.len() && !self.text[i..self.next(i)].chars().all(is_word) {
            i = self.next(i);
        }
        while i < self.text.len() && self.text[i..self.next(i)].chars().all(is_word) {
            i = self.next(i);
        }
        i
    }

    pub fn backspace(&mut self, word: bool) {
        if !self.has_selection() {
            self.anchor = if word {
                self.word_left(self.caret)
            } else {
                self.prev(self.caret)
            };
        }
        self.delete_selection();
    }

    pub fn delete(&mut self, word: bool) {
        if !self.has_selection() {
            self.anchor = if word {
                self.word_right(self.caret)
            } else {
                self.next(self.caret)
            };
        }
        self.delete_selection();
    }

    pub fn left(&mut self, word: bool, extend: bool) {
        let to = if self.has_selection() && !extend && !word {
            self.selection().0
        } else if word {
            self.word_left(self.caret)
        } else {
            self.prev(self.caret)
        };
        self.move_to(to, extend);
    }

    pub fn right(&mut self, word: bool, extend: bool) {
        let to = if self.has_selection() && !extend && !word {
            self.selection().1
        } else if word {
            self.word_right(self.caret)
        } else {
            self.next(self.caret)
        };
        self.move_to(to, extend);
    }

    pub fn move_to(&mut self, to: usize, extend: bool) {
        self.caret = to.min(self.text.len());
        if !extend {
            self.anchor = self.caret;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typing_replaces_the_selection() {
        let mut f = Field::new("report.pdf");
        f.select_stem();
        assert_eq!(f.selection(), (0, 6));
        f.insert("x");
        assert_eq!(f.text, "x.pdf");
        assert_eq!(f.caret, 1);
    }

    #[test]
    fn stems_ignore_a_leading_dot() {
        let mut f = Field::new(".bashrc");
        f.select_stem();
        assert_eq!(f.selection(), (0, 7));
        let mut f = Field::new("noext");
        f.select_stem();
        assert_eq!(f.selection(), (0, 5));
    }

    #[test]
    fn multibyte_characters_stay_whole() {
        let mut f = Field::new("aé日");
        f.backspace(false);
        assert_eq!(f.text, "aé");
        f.left(false, false);
        assert_eq!(f.caret, 1);
        f.delete(false);
        assert_eq!(f.text, "a");
        f.delete(false);
        assert_eq!(f.text, "a");
        f.move_to(0, false);
        f.backspace(false);
        assert_eq!(f.text, "a");
    }

    #[test]
    fn word_motions() {
        let mut f = Field::new("foo bar-baz");
        f.backspace(true);
        assert_eq!(f.text, "foo bar-");
        f.backspace(true);
        assert_eq!(f.text, "foo ");
        f.move_to(0, false);
        f.right(true, false);
        assert_eq!(f.caret, 3);
    }

    #[test]
    fn arrows_collapse_a_selection_to_its_edge() {
        let mut f = Field::new("abcd");
        f.select_all();
        f.left(false, false);
        assert_eq!((f.caret, f.anchor), (0, 0));
        f.select_all();
        f.right(false, false);
        assert_eq!((f.caret, f.anchor), (4, 4));
        f.left(false, true);
        assert_eq!(f.selection(), (3, 4));
    }

    #[test]
    fn control_characters_cant_be_inserted() {
        let mut f = Field::new("");
        f.insert("a\nb\tc\u{7f}");
        assert_eq!(f.text, "abc");
    }
}
