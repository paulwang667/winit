//! The text a focused field holds, as UIKit's text input sees it.
//!
//! An app that edits text itself (`WindowExtIOS::set_text_input_state`)
//! hands winit a copy of its field; the view's `UITextInput` methods read
//! and edit this copy, and each edit is reported back as
//! [`Ime::TextState`](crate::event::Ime::TextState) — the same model the
//! Android GameActivity backend uses. Offsets here are bytes into `text`;
//! UIKit counts UTF-16 code units, and the conversions live here so they
//! can be tested off-device.

/// The mirrored field: its text, selection and composing (marked) range,
/// all byte offsets into `text` on `char` boundaries.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct TextMirror {
    pub text: String,
    pub selection: (usize, usize),
    pub marked: Option<(usize, usize)>,
}

/// What an `insertText:` means.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum InsertAction {
    /// Edit the mirror.
    Edit,
    /// The return key with nothing being composed: an Enter press, so the
    /// app runs the field's action (send, search) or inserts a newline.
    Enter,
}

impl TextMirror {
    /// The app's state, with every offset clamped onto a `char` boundary.
    pub fn from_state(
        text: String,
        selection: (usize, usize),
        marked: Option<(usize, usize)>,
    ) -> Self {
        let snap = |b: usize| floor_char(&text, b);
        let order = |(a, b): (usize, usize)| (snap(a.min(b)), snap(a.max(b)));
        let selection = order(selection);
        let marked = marked.map(order);
        Self { text, selection, marked }
    }

    /// The text's length in UTF-16 code units.
    pub fn utf16_len(&self) -> usize {
        self.text.encode_utf16().count()
    }

    /// The byte offset of UTF-16 offset `utf16`: inside a surrogate pair it
    /// snaps to the pair's `char`, past the end it clamps.
    pub fn byte_at(&self, utf16: usize) -> usize {
        let mut units = 0;
        for (byte, c) in self.text.char_indices() {
            let next = units + c.len_utf16();
            if utf16 < next {
                return byte;
            }
            units = next;
        }
        self.text.len()
    }

    /// The UTF-16 offset of byte offset `byte`.
    pub fn utf16_at(&self, byte: usize) -> usize {
        self.text[..floor_char(&self.text, byte)].encode_utf16().count()
    }

    /// Replaces UTF-16 range `range` with `text`; the caret lands after it
    /// and any composition ends.
    pub fn replace(&mut self, range: (usize, usize), text: &str) -> bool {
        let (a, b) = (self.byte_at(range.0.min(range.1)), self.byte_at(range.0.max(range.1)));
        self.splice((a, b), text, None)
    }

    /// Types `text`: over the composition when there is one (committing
    /// it), else over the selection.
    pub fn insert(&mut self, text: &str) -> bool {
        if let Some(marked) = self.marked {
            // Committing with the return key keeps what was composed.
            let text = if text == "\n" {
                self.text[marked.0..marked.1].to_owned()
            } else {
                text.to_owned()
            };
            return self.splice(marked, &text, None);
        }
        self.splice(self.selection, text, None)
    }

    /// Sets the composition to `text`, replacing the current one (or the
    /// selection); `selected` is a UTF-16 range within `text`.
    pub fn set_marked(&mut self, text: &str, selected: (usize, usize)) -> bool {
        let at = self.marked.unwrap_or(self.selection);
        let changed = self.splice(at, text, None);
        let start = at.0;
        let end = start + text.len();
        let within = TextMirror { text: text.to_owned(), ..Default::default() };
        let sel = (
            start + within.byte_at(selected.0.min(selected.1)),
            start + within.byte_at(selected.0.max(selected.1)),
        );
        let marked = (!text.is_empty()).then_some((start, end));
        let moved = self.marked != marked || self.selection != sel;
        self.marked = marked;
        self.selection = sel;
        changed || moved
    }

    /// Ends the composition, keeping its text.
    pub fn unmark(&mut self) -> bool {
        self.marked.take().is_some()
    }

    /// The backspace key: removes the selection, or the `char` before the
    /// caret. Ends any composition.
    pub fn delete_backward(&mut self) -> bool {
        let (a, b) = self.selection;
        let range = if a != b {
            (a, b)
        } else {
            match self.text[..a].chars().next_back() {
                Some(c) => (a - c.len_utf8(), a),
                None => return self.unmark(),
            }
        };
        self.splice(range, "", None)
    }

    /// Selects UTF-16 range `range`.
    pub fn set_selection(&mut self, range: (usize, usize)) -> bool {
        let sel = (self.byte_at(range.0.min(range.1)), self.byte_at(range.0.max(range.1)));
        let changed = self.selection != sel;
        self.selection = sel;
        changed
    }

    /// What typing `text` means right now.
    pub fn classify_insert(&self, text: &str) -> InsertAction {
        match (text, self.marked) {
            ("\n", None) => InsertAction::Enter,
            _ => InsertAction::Edit,
        }
    }

    /// Replaces byte range `range` with `text`, caret after it, composition
    /// set to `marked`. Whether anything changed.
    fn splice(
        &mut self,
        range: (usize, usize),
        text: &str,
        marked: Option<(usize, usize)>,
    ) -> bool {
        let before = self.clone();
        self.text.replace_range(range.0..range.1, text);
        let caret = range.0 + text.len();
        self.selection = (caret, caret);
        self.marked = marked;
        *self != before
    }
}

/// The largest `char` boundary at or below `byte` (clamped to the end).
fn floor_char(text: &str, byte: usize) -> usize {
    let mut b = byte.min(text.len());
    while !text.is_char_boundary(b) {
        b -= 1;
    }
    b
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mirror(text: &str, caret: usize) -> TextMirror {
        TextMirror { text: text.into(), selection: (caret, caret), marked: None }
    }

    #[test]
    fn replace_counts_utf16_not_bytes() {
        let mut m = mirror("你好", 6);
        assert!(m.replace((1, 2), "们"));
        assert_eq!(m.text, "你们");
        assert_eq!(m.selection, (6, 6), "caret after the inserted text");
    }

    #[test]
    fn set_marked_then_insert_commits() {
        let mut m = mirror("a", 1);
        assert!(m.set_marked("ni", (2, 2)));
        assert_eq!((m.text.as_str(), m.marked, m.selection), ("ani", Some((1, 3)), (3, 3)));
        assert!(m.set_marked("nihao", (5, 5)));
        assert_eq!((m.text.as_str(), m.marked), ("anihao", Some((1, 6))));
        assert!(m.insert("你好"));
        assert_eq!((m.text.as_str(), m.marked, m.selection), ("a你好", None, (7, 7)));
    }

    #[test]
    fn set_marked_then_unmark_keeps_text() {
        let mut m = mirror("", 0);
        m.set_marked("ab", (2, 2));
        assert!(m.unmark());
        assert_eq!((m.text.as_str(), m.marked, m.selection), ("ab", None, (2, 2)));
    }

    #[test]
    fn delete_backward_removes_one_char_or_the_selection() {
        let mut m = mirror("a😀", 5);
        assert!(m.delete_backward());
        assert_eq!((m.text.as_str(), m.selection), ("a", (1, 1)));
        let mut m = TextMirror { text: "hello".into(), selection: (1, 4), marked: None };
        assert!(m.delete_backward());
        assert_eq!((m.text.as_str(), m.selection), ("ho", (1, 1)));
        let mut m = mirror("", 0);
        assert!(!m.delete_backward(), "nothing to delete");
    }

    #[test]
    fn offsets_inside_a_surrogate_pair_snap_to_the_char() {
        let m = mirror("a😀b", 0);
        assert_eq!(m.utf16_len(), 4);
        assert_eq!(m.byte_at(2), 1, "inside the pair snaps to its start");
        assert_eq!(m.byte_at(3), 5);
        assert_eq!(m.byte_at(99), 6, "past the end clamps");
        assert_eq!(m.utf16_at(5), 3);
    }

    #[test]
    fn set_selection_takes_utf16_offsets() {
        let mut m = mirror("你好", 0);
        assert!(m.set_selection((2, 1)));
        assert_eq!(m.selection, (3, 6), "ordered, in bytes");
        assert!(!m.set_selection((1, 2)), "unchanged");
    }

    #[test]
    fn newline_without_marked_text_is_enter() {
        let m = mirror("hi", 2);
        assert_eq!(m.classify_insert("\n"), InsertAction::Enter);
        assert_eq!(m.classify_insert("x"), InsertAction::Edit);
    }

    #[test]
    fn newline_while_composing_commits() {
        let mut m = mirror("", 0);
        m.set_marked("ni", (2, 2));
        assert_eq!(m.classify_insert("\n"), InsertAction::Edit);
        assert!(m.insert("\n"));
        assert_eq!((m.text.as_str(), m.marked), ("ni", None), "commits, inserts no newline");
    }

    #[test]
    fn an_app_push_cancels_composition() {
        let mut m = mirror("", 0);
        m.set_marked("ni", (2, 2));
        m = TextMirror::from_state("hello".into(), (5, 5), None);
        assert_eq!(m.marked, None);
        assert!(m.insert("!"));
        assert_eq!(m.text, "hello!");
    }
}
