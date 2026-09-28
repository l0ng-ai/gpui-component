/// The character used to mask password input fields.
pub(super) const MASK_CHAR: char = '•';

mod blink_cursor;
mod change;
mod clear_button;
mod cursor;
mod display_map;
mod edit_log;
mod editing;
mod element;
mod gutter_marker;
mod indent;
mod input;
mod lsp;
mod mask_pattern;
mod mode;
mod movement;
mod multi_selection;
mod number_input;
mod otp_input;
pub(crate) mod popovers;
mod rope_ext;
mod search;
mod selection;
mod state;

pub(crate) use clear_button::*;
pub use cursor::*;
#[cfg(target_family = "wasm")]
pub use display_map::folding::Tree;
pub use display_map::{BufferPoint, DisplayMap, DisplayPoint, FoldRange};
pub use edit_log::{EDIT_LOG_LEN, LineEdit};
pub use editing::{
    CODE_EDITOR_CONTEXT, CopyLineDown, CopyLineUp, DeleteLine, InsertLineAbove, InsertLineBelow,
    MoveLineDown, MoveLineUp, MoveToMatchingBracket, SelectLine, ToggleBlockComment,
    ToggleLineComment,
};
pub use gutter_marker::{
    GutterMarker, GutterMarkerClick, GutterMarkerKind, adjust_markers_for_edit,
};
pub use indent::TabSize;
pub use input::*;
pub use lsp::*;
pub use lsp_types::Position;
pub use mask_pattern::MaskPattern;
pub use multi_selection::{
    AddCursorAbove, AddCursorBelow, SelectAllOccurrences, SelectNextOccurrence,
};
pub use number_input::{NumberInput, NumberInputEvent, NumberStep, StepAction};
pub use otp_input::*;
pub use rope_ext::{InputEdit, Point, RopeExt, RopeLines};
pub use ropey::Rope;
pub use state::*;
