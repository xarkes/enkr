//! Sidebar drag & drop: what can be dragged, where it may land, and the
//! legality rule that gates a drop. The commit itself lives on `EnkrState`
//! (`apply_drop`); this module is the vocabulary and the validation.

use super::*;

/// A sidebar item being dragged (note or folder).
#[derive(Clone, PartialEq)]
pub(crate) enum DragItem {
    Note(String),
    Folder(Uuid),
}

/// Where a drag can be dropped: onto a space header or a folder row.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum DropTarget {
    Space(i64),
    Folder(Uuid),
}

/// An in-progress sidebar drag-and-drop of a note or folder.
pub(crate) struct DragState {
    pub(crate) item: DragItem,
    /// Drop target under the cursor as of the previous frame, kept for both
    /// highlighting it and committing the move when the button is released.
    pub(crate) target: Option<DropTarget>,
}

/// Read-only snapshot of the drag, handed to the row builders so they can
/// highlight the hovered drop target and suppress click handling on the frame a
/// drag is released.
pub(crate) struct DragView {
    pub(crate) active: bool,
    pub(crate) item: Option<DragItem>,
    pub(crate) target: Option<DropTarget>,
}

/// Whether `item` may be dropped on `target` — used to gate both the highlight
/// and the move, so invalid drops (a folder into its own subtree, or a no-op
/// onto the item's current home) never light up.
pub(crate) fn drop_allowed(db: &NoteDatabase, item: &DragItem, target: DropTarget) -> bool {
    match (item, target) {
        (DragItem::Note(id), DropTarget::Folder(folder)) => {
            db.note(id).is_some_and(|n| n.folder() != Some(folder))
        }
        (DragItem::Note(id), DropTarget::Space(space)) => db
            .note(id)
            .is_some_and(|n| n.space_id() != space || n.folder().is_some()),
        (DragItem::Folder(folder), DropTarget::Folder(target)) => {
            *folder != target && !db.folder_subtree(*folder).contains(&target)
        }
        (DragItem::Folder(folder), DropTarget::Space(space)) => db
            .folder(folder)
            .is_some_and(|f| f.space_id != space || f.parent.is_some()),
    }
}
