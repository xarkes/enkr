//! Layer 1 — anchored menus. The context-menu shell (`context_menu`,
//! `context_submenu`, `menu_item`) plus the four right-click menus built on it.
//! Dismissal and z-order are the caller's; this module only draws.

use crate::app::*;

/// Which context menu is open. One at a time — the menu layer holds a single
/// `Option`, so opening one implicitly replaces any other.
#[derive(Clone, PartialEq)]
pub(crate) enum Menu {
    Space(i64),
    Note(String),
    Folder { id: Uuid, space: i64 },
    Blob(String),
}

/// The one submenu a menu may have open. Menus nest exactly one level; a
/// deeper tree would need real z-order, which mae does not provide.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Submenu {
    /// "Sync this space…" → the server picker.
    SpaceServers,
}

/// Where a menu is pinned. Frame-independent on purpose: `UIBoxHandle` indices
/// are per-frame, so an open menu stores geometry, not a handle.
#[derive(Clone, Copy, PartialEq)]
pub(crate) enum Anchor {
    /// At a point — where the pointer was when the menu was summoned.
    At(Point),
}

impl Anchor {
    pub(crate) fn point(self) -> Point {
        match self {
            Anchor::At(p) => p,
        }
    }
}

/// Layer 1's entire state: the open menu, its anchor, and its one submenu.
pub(crate) struct MenuState {
    pub(crate) menu: Menu,
    pub(crate) anchor: Anchor,
    pub(crate) submenu: Option<Submenu>,
    /// False on the frame the menu opened. A left-click-opened menu would
    /// otherwise be dismissed by the very press that opened it: the press is
    /// still in this frame's event queue, and the pane has no painted rect yet
    /// to test the pointer against.
    pub(crate) armed: bool,
}

pub(crate) const CONTEXT_MENU_WIDTH: f32 = 220.0;
pub(crate) const CONTEXT_MENU_GAP: f32 = 2.0;
/// Conservative per-row height (>= the real `menu_item` height) used only to
/// decide when a submenu needs to scroll.
pub(crate) const CONTEXT_MENU_ITEM_EST: f32 = 28.0;

/// Keep a [`CONTEXT_MENU_WIDTH`]-wide popover on screen: flip it to the left
/// of its anchor when it would overflow the right edge, then clamp.
///
/// Menus are placed at the raw pointer position (`ui.mouse_position()` at the
/// moment of the right-click) with no regard for what is left of the window.
/// A 220px menu opened in the right-hand two thirds of a 390px viewport hung
/// off the edge; a submenu, anchored a further `CONTEXT_MENU_WIDTH` to the
/// right, was off-screen from the moment it opened. Flipping rather than only
/// clamping is what keeps a submenu beside its parent instead of on top of it.
fn menu_pos(ui: &IMUI, pos: Point) -> Point {
    let (screen_w, _) = ui.window_size();
    let x = if pos.x() + CONTEXT_MENU_WIDTH + WINDOW_MARGIN > screen_w {
        pos.x() - CONTEXT_MENU_WIDTH
    } else {
        pos.x()
    };
    Point::new(
        x.clamp(
            WINDOW_MARGIN,
            (screen_w - CONTEXT_MENU_WIDTH - WINDOW_MARGIN).max(WINDOW_MARGIN),
        ),
        pos.y(),
    )
}

/// Chrome for a right-click context-menu popover at `pos`: a compact column of
/// full-width [`menu_item`]s with the shared popover styling. Returns the pane
/// handle so callers can hit-test it for click-away dismissal.
pub(crate) fn context_menu(
    ui: &mut IMUI,
    id: &str,
    pos: Point,
    body: impl FnOnce(&mut IMUI),
) -> UIBoxHandle {
    let theme = *ui.theme();
    let pane = ui.floating_pane_at(menu_pos(ui, pos), Some(id), body);
    pane.width(ui, UISize::Pixels(CONTEXT_MENU_WIDTH))
        .padding_all(ui, theme.pad_sm)
        .gap(ui, CONTEXT_MENU_GAP)
        .background(ui, theme.popover_bg)
        .border_color(ui, theme.border)
        .corner_radius(ui, theme.radius);
    pane
}

/// Like [`context_menu`] but for submenus holding a dynamic number of entries:
/// caps its height to the space below `pos` and scrolls when `item_count` rows
/// wouldn't fit, reserving the scrollbar gutter so it doesn't overlap items.
pub(crate) fn context_submenu(
    ui: &mut IMUI,
    id: &str,
    pos: Point,
    item_count: usize,
    body: impl FnOnce(&mut IMUI),
) -> UIBoxHandle {
    let theme = *ui.theme();
    let (_, screen_h) = ui.window_size();
    let natural =
        theme.pad_sm * 2.0 + item_count.max(1) as f32 * (CONTEXT_MENU_ITEM_EST + CONTEXT_MENU_GAP);
    let max_h =
        (screen_h - pos.y() - WINDOW_MARGIN).max(CONTEXT_MENU_ITEM_EST * 3.0 + theme.pad_sm * 2.0);
    let scroll = natural > max_h;
    let right_pad = theme.pad_sm + if scroll { SCROLLBAR_GUTTER } else { 0.0 };

    let pane = ui.floating_pane_at(menu_pos(ui, pos), Some(id), body);
    let pane = pane
        .width(ui, UISize::Pixels(CONTEXT_MENU_WIDTH))
        .padding(ui, theme.pad_sm, right_pad, theme.pad_sm, theme.pad_sm)
        .gap(ui, CONTEXT_MENU_GAP)
        .background(ui, theme.popover_bg)
        .border_color(ui, theme.border)
        .corner_radius(ui, theme.radius);
    if scroll {
        pane.height(ui, UISize::Pixels(max_h))
            .scroll_y(ui, true)
            .clip(ui, true)
    } else {
        pane
    }
}

/// A single full-width, vertically-compact context-menu item (flat: no border,
/// blends with the popover until hovered). Returns the handle for `.clicked()` /
/// `.hover()`.
pub(crate) fn menu_item(ui: &mut IMUI, id: &str, tooltip: Option<&str>) -> UIBoxHandle {
    let theme = *ui.theme();
    ui.button(id, tooltip)
        .width(ui, UISize::ParentPct(1.0))
        .height(ui, UISize::TextContent(2.0))
        .padding(ui, 4.0, 8.0, 4.0, 8.0)
        .corner_radius(ui, theme.radius)
        .background(ui, theme.popover_bg)
        .border_color(ui, transparent_like(theme.border))
        .font_size(ui, theme.size_text - 1.0)
}

/// Right-click menu on a space row: sync to a chosen server / share.
/// Draws the space menu. Returns `(panes, close, submenu)` — dismissal and
/// z-order are the layer's job (`render_menu`), not each menu's.
fn space_menu(
    ui: &mut IMUI,
    state: &mut EnkrState,
    pal: &Colors,
    space_id: i64,
    pos: Point,
    open_submenu: Option<Submenu>,
) -> (Vec<UIBoxHandle>, bool, Option<Submenu>) {
    let pal = *pal;
    let theme = *ui.theme();
    let remote = state.notes.space_remote(space_id);
    let bound_server = state.notes.space_server(space_id).map(str::to_string);
    let mut server_submenu = open_submenu == Some(Submenu::SpaceServers);
    let mut close = false;

    // y of the "Sync this space…" row so the server submenu opens level with it.
    let mut sync_anchor = pos.y();
    let pane = context_menu(ui, "###enkr_space_menu", pos, |ui| {
        if menu_item(ui, "Rename\u{2026}###enkr_space_rename", None).clicked() {
            state.begin_rename(RenameTarget::Space(space_id));
            close = true;
        }
        if menu_item(
            ui,
            "New folder\u{2026}###enkr_menu_new_folder",
            Some("Create a folder in this space"),
        )
        .clicked()
        {
            state.create_folder_and_rename(space_id, None);
            close = true;
        }
        if remote.is_none() {
            let sync_item = menu_item(
                ui,
                "Sync this space\u{2026} >###enkr_menu_sync",
                Some("Choose a server to sync this space to"),
            );
            sync_anchor = ui.bounds(sync_item).y0;
            if sync_item.hover() || sync_item.clicked() {
                server_submenu = true;
            }
        } else {
            // Already synced: show its bound server (read-only).
            let label = bound_server.as_deref().unwrap_or("a sync server");
            ui.label(&format!("Synced @ {label}"))
                .width(ui, UISize::ParentPct(1.0))
                .text_color(ui, pal.text_muted)
                .font_size(ui, theme.size_text - 1.0);
        }
        if let Some(remote) = remote {
            let share_item = menu_item(
                ui,
                "Share\u{2026}###enkr_menu_share",
                Some("Invite another device"),
            );
            if share_item.hover() {
                server_submenu = false;
            }
            if share_item.clicked() {
                state.share_dialog = Some(ShareDialog {
                    remote_space: remote,
                    input: String::new(),
                    role: MemberRole::Writer,
                    error: None,
                });
                close = true;
            }
        }
        let delete_item = menu_item(
            ui,
            "Delete###enkr_space_delete",
            Some("Delete this local space and its notes"),
        );
        if delete_item.hover() {
            server_submenu = false;
        }
        if delete_item.clicked() {
            state.delete_space(space_id);
            close = true;
        }
    });

    // The "Sync this space…" server picker (PLAN-account.md §6): the default
    // server is first/pre-selected; picking one binds + switches + pushes.
    let submenu = if remote.is_none() && server_submenu {
        let servers = state.server_list();
        let active = state.active_server.clone();
        let submenu_pos = Point::new(pos.x() + CONTEXT_MENU_WIDTH, sync_anchor - theme.pad_sm);
        let mut chosen: Option<String> = None;
        let pane = context_submenu(
            ui,
            "###enkr_space_server_submenu",
            submenu_pos,
            servers.len(),
            |ui| {
                for server in &servers {
                    let suffix = if *server == active { "  (active)" } else { "" };
                    if menu_item(
                        ui,
                        &format!("{server}{suffix}###enkr_sync_to_{server}"),
                        None,
                    )
                    .clicked()
                    {
                        chosen = Some(server.clone());
                    }
                }
            },
        );
        if let Some(server) = chosen {
            state.sync_space_to_server(space_id, server);
            close = true;
        }
        Some(pane)
    } else {
        None
    };

    let mut panes = vec![pane];
    panes.extend(submenu);
    (
        panes,
        close,
        server_submenu.then_some(Submenu::SpaceServers),
    )
}

/// Right-click menu on a note item.
///
/// "Move to…" is one item now rather than two hover-driven submenus. Those
/// listed bare folder names and bare space names, so two folders called "Notes"
/// in different spaces were indistinguishable, and a folder in *another* space
/// was unreachable in a single action. The palette shows full paths and spans
/// every space.
fn note_menu(
    ui: &mut IMUI,
    state: &mut EnkrState,
    note_id: String,
    pos: Point,
) -> (Vec<UIBoxHandle>, bool) {
    if state.notes.note(&note_id).is_none() {
        // The note vanished under an open menu (deleted, or moved away by a
        // peer): close rather than draw against a dangling id.
        return (Vec::new(), true);
    }
    let mut close = false;
    let mut move_to = false;

    let pane = context_menu(ui, "###enkr_note_menu", pos, |ui| {
        if menu_item(
            ui,
            "Move to\u{2026}###enkr_note_move",
            Some("Move this note to another folder or space"),
        )
        .clicked()
        {
            move_to = true;
        }
        if menu_item(ui, "Delete note###enkr_note_delete", None).clicked() {
            state.delete_note(&note_id);
            close = true;
        }
    });

    if move_to {
        state.open_move_to(MoveSubject::Note(note_id));
        close = true;
    }
    (vec![pane], close)
}

/// Right-click menu on a folder row: rename / delete.
fn folder_menu(
    ui: &mut IMUI,
    state: &mut EnkrState,
    folder_id: Uuid,
    space_id: i64,
    pos: Point,
) -> (Vec<UIBoxHandle>, bool) {
    let mut close = false;
    let mut move_to = false;

    let pane = context_menu(ui, "###enkr_folder_menu", pos, |ui| {
        if menu_item(
            ui,
            "New note###enkr_folder_new_note",
            Some("Create a note inside this folder"),
        )
        .clicked()
        {
            state.create_note_in_folder_and_select(space_id, folder_id);
            close = true;
        }
        if menu_item(
            ui,
            "New folder\u{2026}###enkr_folder_new_child",
            Some("Create a folder inside this folder"),
        )
        .clicked()
        {
            state.create_folder_and_rename(space_id, Some(folder_id));
            close = true;
        }
        if menu_item(
            ui,
            "Move to\u{2026}###enkr_folder_move",
            Some("Move this folder to another folder or space"),
        )
        .clicked()
        {
            move_to = true;
        }
        if menu_item(ui, "Rename\u{2026}###enkr_folder_rename", None).clicked() {
            state.begin_rename(RenameTarget::Folder(folder_id));
            close = true;
        }
        if menu_item(
            ui,
            "Delete###enkr_folder_delete",
            Some("Notes move back to the space root"),
        )
        .clicked()
        {
            state.delete_folder(space_id, folder_id);
            close = true;
        }
    });
    if move_to {
        state.open_move_to(MoveSubject::Folder(folder_id));
        close = true;
    }
    (vec![pane], close)
}

/// Right-click menu on a sidebar image row: rename / delete.
fn blob_menu(
    ui: &mut IMUI,
    state: &mut EnkrState,
    blob_id: String,
    pos: Point,
) -> (Vec<UIBoxHandle>, bool) {
    let mut close = false;
    let mut move_to = false;

    let pane = context_menu(ui, "###enkr_blob_menu", pos, |ui| {
        if menu_item(ui, "Rename\u{2026}###enkr_blob_rename_item", None).clicked()
            && let Some(name) = state.notes.blob(&blob_id).map(|blob| blob.name.clone())
        {
            state.begin_rename(RenameTarget::Blob(name));
            close = true;
        }
        if menu_item(
            ui,
            "Move to\u{2026}###enkr_blob_move",
            Some("Move this image to another folder or space"),
        )
        .clicked()
        {
            move_to = true;
        }
        if menu_item(ui, "Delete###enkr_blob_delete", None).clicked() {
            // Leave the viewer if it's showing this image.
            if let Some(name) = state.notes.blob(&blob_id).map(|blob| blob.name.clone())
                && state.view.image() == Some(name.as_str())
            {
                state.set_view(View::Editor);
            }
            state.delete_blob(&blob_id);
            close = true;
        }
    });
    // The palette moves blobs by *name* (that is what a `./blob/<name>` link
    // refers to), so resolve the id we were opened with.
    if move_to && let Some(name) = state.notes.blob(&blob_id).map(|blob| blob.name.clone()) {
        state.open_move_to(MoveSubject::Blob(name));
        close = true;
    }
    (vec![pane], close)
}

/// Layer 1. Draws whichever menu is open, then applies the single dismissal
/// rule for all of them: a press outside every pane of the open chain closes
/// it. `armed` skips that test on the opening frame, when the press that
/// summoned the menu is still in the queue and the pane has no rect yet.
pub(crate) fn render_menu(ui: &mut IMUI, state: &mut EnkrState, pal: &Colors) {
    let Some(open) = state.menu.as_ref() else {
        return;
    };
    let pos = open.anchor.point();
    let submenu = open.submenu;
    let menu = open.menu.clone();

    let (panes, close, next_submenu) = match menu {
        Menu::Space(space_id) => space_menu(ui, state, pal, space_id, pos, submenu),
        Menu::Note(note_id) => {
            let (panes, close) = note_menu(ui, state, note_id, pos);
            (panes, close, None)
        }
        Menu::Folder { id, space } => {
            let (panes, close) = folder_menu(ui, state, id, space, pos);
            (panes, close, None)
        }
        Menu::Blob(blob_id) => {
            let (panes, close) = blob_menu(ui, state, blob_id, pos);
            (panes, close, None)
        }
    };

    let armed = state.menu.as_ref().is_some_and(|m| m.armed);
    let dismissed = armed && ui.press_outside(&panes);
    if close || dismissed {
        state.menu = None;
    } else if let Some(open) = state.menu.as_mut() {
        open.submenu = next_submenu;
        open.armed = true;
    }
}
