use std::fmt::Debug;

use crate::{
    history::HistoryItem,
    input::{Selection, multi_selection::SelectionSet},
};

#[derive(Debug, PartialEq, Clone)]
pub struct Change {
    pub(crate) old_range: Selection,
    pub(crate) old_text: String,
    pub(crate) new_range: Selection,
    pub(crate) new_text: String,
    /// Every cursor before the undo step this change starts, for undo to put
    /// back. Only the step's first change's is used.
    pub(crate) selections_before: Option<SelectionSet>,
    /// Every cursor after the step, for redo. Only the step's last change's
    /// is used.
    pub(crate) selections_after: Option<SelectionSet>,
    version: usize,
}

impl Change {
    pub fn new(
        old_range: impl Into<Selection>,
        old_text: &str,
        new_range: impl Into<Selection>,
        new_text: &str,
    ) -> Self {
        Self {
            old_range: old_range.into(),
            old_text: old_text.to_string(),
            new_range: new_range.into(),
            new_text: new_text.to_string(),
            selections_before: None,
            selections_after: None,
            version: 0,
        }
    }
}

impl HistoryItem for Change {
    fn version(&self) -> usize {
        self.version
    }

    fn set_version(&mut self, version: usize) {
        self.version = version;
    }
}
