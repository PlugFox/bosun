//! A line said once (#186): a condition that lasts is logged when it starts and when its text
//! changes, not at every poll. [`Said::clear`] marks it over, so the next one is said again,
//! even with the same text.

/// The text of the last line said, if the condition still holds.
#[derive(Debug, Default)]
pub(crate) struct Said(Option<String>);

impl Said {
    /// `true` when `text` differs from the last text kept (or none is kept); `text` is kept.
    pub(crate) fn first(&mut self, text: &str) -> bool {
        if self.0.as_deref() == Some(text) {
            return false;
        }
        self.0 = Some(text.to_owned());
        true
    }

    /// The condition is over: the next text is said again.
    pub(crate) fn clear(&mut self) {
        self.0 = None;
    }
}

#[cfg(test)]
mod tests {
    use super::Said;

    #[test]
    fn a_text_is_said_once_until_it_changes_or_is_cleared() {
        let mut said = Said::default();
        assert!(said.first("a"));
        assert!(!said.first("a"));
        assert!(said.first("b"));
        assert!(said.first("a"));
        said.clear();
        assert!(said.first("a"));
        assert!(!said.first("a"));
    }
}
