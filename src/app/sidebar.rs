//! The left sidebar: spaces, the folder/note tree, images, and the footer
//! actions — plus the row builders they share and the drag ghost.

use crate::app::*;

/// Left sidebar: spaces list with counts, then the notes in the active space.
///
/// `width` rather than `state.side_width` directly: on a narrow viewport the
/// same sidebar is built inside a drawer that is sized against the *window*
/// instead (see `render_drawer`), and the user's chosen split width has no
/// say there.
pub(crate) fn sidebar(ui: &mut IMUI, state: &mut EnkrState, pal: &Colors, width: f32) {
    let pal = *pal;
    let side_width = width;

    // Borrow the frame's summaries out of `state` for the duration of the
    // build: `state` is used mutably throughout, and moving the Vec keeps this
    // allocation-free (it is refilled once per frame in `render`).
    let summaries = std::mem::take(&mut state.summaries);
    let sidebar = ui.named_column("###enkr_sidebar", |ui| {
        // Pre-pass: gather sync indicators + presence outside the UI closures
        // (presence prunes expired entries, so it needs &mut sync).
        struct SpaceRowData {
            id: i64,
            name: String,
            indicator: SyncIndicator,
            presence: Vec<Presence>,
        }
        let spaces: Vec<SpaceRowData> = state
            .notes
            .spaces()
            .iter()
            .map(|space| (space.id, space.name.clone()))
            .collect::<Vec<_>>()
            .into_iter()
            .map(|(id, name)| {
                let (indicator, presence) = match state.sync.as_mut() {
                    Some(sync) => (
                        sync.space_indicator(&state.notes, id),
                        sync.space_presence(&state.notes, id),
                    ),
                    None => (SyncIndicator::LocalOnly, Vec::new()),
                };
                SpaceRowData {
                    id,
                    name,
                    indicator,
                    presence,
                }
            })
            .collect();

        // Drag-and-drop snapshot from the previous frame: highlights the hovered
        // drop target and suppresses click handling on the frame a drag is
        // released. `drag_begin`/`hover_target` accumulate this frame's state.
        let drag_active = state.drag.is_some();
        let drag_view = DragView {
            active: drag_active,
            item: state.drag.as_ref().map(|d| d.item.clone()),
            target: state.drag.as_ref().and_then(|d| d.target),
        };
        let mut drag_begin: Option<DragItem> = None;
        let mut hover_target: Option<DropTarget> = None;

        // The active space, as a switcher button. Replaces the old always-
        // expanded SPACES list, which competed with the note tree for vertical
        // space and pushed it off screen once you had a handful of spaces.
        let active = spaces.iter().find(|s| s.id == state.active_space_id);
        let active_id = active.map(|s| s.id);
        let renaming_space = active_id
            .map(RenameTarget::Space)
            .is_some_and(|t| state.renaming(&t));
        let mut space_committed = false;
        let switcher = space_switcher(
            ui,
            &pal,
            active.map(|s| s.name.as_str()).unwrap_or("No space"),
            active
                .map(|s| s.indicator)
                .unwrap_or(SyncIndicator::LocalOnly),
            active.map(|s| s.presence.as_slice()).unwrap_or(&[]),
            state.space_switcher_open(),
            renaming_space.then(|| state.inline_edit.as_mut()).flatten(),
            &mut space_committed,
        );
        if space_committed {
            state.commit_rename();
        }
        // A click lands in the rename field while renaming, not on the trigger.
        if switcher.clicked() && !renaming_space {
            // The searchable palette, not a dropdown: with more than a handful
            // of spaces, typing two letters beats scanning a list.
            state.open_space_switcher();
        }
        // Right-click acts on the space itself (rename / sync / share / delete),
        // matching how right-click worked on the old space rows — the header is
        // now the only place the active space appears.
        if switcher.right_clicked()
            && let Some(active) = active.map(|s| s.id)
        {
            state.open_menu(
                Menu::Space(active),
                ui.mouse_position().unwrap_or(Point::new(60.0, 60.0)),
            );
        }
        // A dragged note dropped on the header returns it to the space root.
        if drag_active
            && switcher.signal().mouse_over()
            && let Some(active) = active.map(|s| s.id)
        {
            let target = DropTarget::Space(active);
            if drag_view
                .item
                .as_ref()
                .is_some_and(|item| drop_allowed(&state.notes, item, target))
            {
                hover_target = Some(target);
            }
        }

        // A new note belongs next to the tree it appears in, not in the top bar
        // over the editor.
        let new_note = enkr_button(
            ui,
            "+  New note###enkr_new_note_btn",
            Some("Create a new note"),
            BtnVariant::Primary,
        )
        .width(ui, UISize::ParentPct(1.0))
        .height(ui, UISize::Pixels(30.0));
        if new_note.clicked() {
            state.create_note_and_select();
        }

        let active_space = state.active_space_id;
        let active_note = state.active_note_id.clone();
        let notes: Vec<(NoteSummary, SyncIndicator, Vec<Presence>)> = summaries
            .iter()
            .filter(|s| s.space_id == active_space)
            .cloned()
            .map(|summary| {
                let note = state.notes.note(&summary.id);
                let (indicator, presence) = match (state.sync.as_mut(), note) {
                    (Some(sync), Some(note)) => {
                        let indicator = sync.note_indicator(note);
                        let presence = note
                            .remote_doc()
                            .map(|doc| sync.presence(&doc))
                            .unwrap_or_default();
                        (indicator, presence)
                    }
                    _ => (SyncIndicator::LocalOnly, Vec::new()),
                };
                (summary, indicator, presence)
            })
            .collect();
        // Borrowed out for the build, like `summaries`: the tree rows are built
        // inside a closure that already holds `&state.notes`, and an inline edit
        // needs `&mut`.
        let mut inline_edit = state.inline_edit.take();
        let inline_target = inline_edit.as_ref().map(|edit| edit.target.clone());
        let mut commit_rename = false;
        let notes_db = &state.notes;
        let mut fold_folder = None;
        let mut select_note = None;
        let mut view_blob = None;
        let mut blob_menu: Option<(Menu, Point)> = None;
        let mut note_menu: Option<(Menu, Point)> = None;
        let mut folder_menu: Option<(Menu, Point)> = None;
        let list = ui.named_column("###enkr_notes_list", |ui| {
            // Folders first, each with its notes indented under it.
            for folder in notes_db
                .folders_in_space(active_space)
                .filter(|folder| folder.parent.is_none())
            {
                render_folder_branch(
                    ui,
                    &pal,
                    notes_db,
                    &notes,
                    active_note.as_str(),
                    active_space,
                    folder.id,
                    0,
                    &drag_view,
                    &mut fold_folder,
                    &mut select_note,
                    &mut note_menu,
                    &mut folder_menu,
                    &mut drag_begin,
                    &mut hover_target,
                    &mut inline_edit,
                    &inline_target,
                    &mut commit_rename,
                );
            }
            // Then the space root: unassigned notes (or stale folder ids).
            for (summary, indicator, presence) in &notes {
                if summary.folder.is_some_and(|id| {
                    notes_db
                        .folder(&id)
                        .is_some_and(|folder| folder.space_id == active_space)
                }) {
                    continue;
                }
                let selected = summary.id == active_note;
                let item = note_item(ui, &pal, summary, selected, *indicator, presence, 0.0);
                if item.clicked() && !drag_view.active {
                    select_note = Some(summary.id.clone());
                }
                if item.right_clicked() {
                    note_menu = Some((
                        Menu::Note(summary.id.clone()),
                        ui.mouse_position().unwrap_or(Point::new(60.0, 160.0)),
                    ));
                }
                if item.dragging() {
                    drag_begin = Some(DragItem::Note(summary.id.clone()));
                }
            }

            // Images (blobs) in the space, listed after the notes. A click opens
            // the image in the content view; right-click renames/deletes it.
            let blobs: Vec<(String, String)> = notes_db
                .blobs_in_space(active_space)
                .map(|blob| (blob.id.clone(), blob.name.clone()))
                .collect();
            if !blobs.is_empty() {
                section_header(ui, &pal, "IMAGES");
                for (id, name) in &blobs {
                    let renaming = inline_target
                        .as_ref()
                        .is_some_and(|t| *t == RenameTarget::Blob(name.clone()));
                    let mut committed = false;
                    let row = blob_item(
                        ui,
                        &pal,
                        name,
                        renaming.then(|| inline_edit.as_mut()).flatten(),
                        &mut committed,
                    );
                    commit_rename |= committed;
                    if row.clicked() && !drag_view.active {
                        view_blob = Some(name.clone());
                    }
                    if row.right_clicked() {
                        blob_menu = Some((
                            Menu::Blob(id.clone()),
                            ui.mouse_position().unwrap_or(Point::new(60.0, 200.0)),
                        ));
                    }
                }
            }
        });
        state.inline_edit = inline_edit;
        if commit_rename {
            state.commit_rename();
        }
        list.width(ui, UISize::ParentPct(1.0))
            .height(ui, UISize::Fill)
            .gap(ui, 2.0)
            .scroll_y(ui, true)
            .clip(ui, true);

        // The tree's empty area is the space root as a drop target. Removing
        // the always-expanded SPACES list took away the only surface you could
        // drop onto to pull a note *out* of a folder; this restores that
        // gesture without spending permanent vertical space on it. Cross-space
        // moves go through the move-to palette instead.
        if drag_active && hover_target.is_none() && list.signal().mouse_over() {
            let root = DropTarget::Space(active_space);
            if drag_view
                .item
                .as_ref()
                .is_some_and(|item| drop_allowed(&state.notes, item, root))
            {
                hover_target = Some(root);
            }
        }

        if let Some((folder, folded)) = fold_folder {
            state.notes.set_folder_folded(&folder, folded);
        }
        if let Some(note_id) = select_note {
            state.set_view(View::Editor);
            state.active_space_id = active_space;
            state.select_note(note_id);
        }
        if let Some(name) = view_blob {
            state.set_view(View::Image(name));
        }
        // A right-click anywhere in the tree replaces whatever menu was open.
        for (menu, pos) in [blob_menu, note_menu, folder_menu].into_iter().flatten() {
            state.open_menu(menu, pos);
        }

        // Reconcile the drag: while a source row is mid-drag, keep it alive and
        // remember the cursor's target; otherwise the button was released this
        // frame, so commit the move onto whatever target sits under the cursor.
        if let Some(item) = drag_begin {
            state.drag = Some(DragState {
                item,
                target: hover_target,
            });
        } else if let Some(drag) = state.drag.take() {
            if let Some(target) = hover_target {
                state.apply_drop(drag.item, target);
            }
        }

        sidebar_footer(ui, state, &pal);
    });
    state.summaries = summaries;
    sidebar
        .width(ui, UISize::Pixels(side_width))
        .height(ui, UISize::ParentPct(1.0))
        .padding_all(ui, 12.0)
        .gap(ui, 4.0)
        .background(ui, pal.sidebar_bg)
        .clip(ui, true);
}

/// The sidebar as a drawer over the content, for viewports too narrow to
/// show both at once (see `NARROW_WIDTH`).
///
/// Same `sidebar` build, different mount: a floating pane against the left
/// edge over a scrim, rather than a column taking width from the editor. On a
/// 390px phone the inline sidebar would take 260 of it and leave 129px of
/// note — so at that size the sidebar stops being furniture and becomes
/// something you summon (`MENU_ICON` in the top bar) and dismiss.
///
/// Dismissal is the same rule the context menu uses — a press outside the
/// pane, via `IMUI::press_outside`, with an `armed` guard for the frame the
/// opening press is still in the queue — plus Escape through
/// `EnkrState::dismiss_top`, plus opening a note (`select_note` clears it).
pub(crate) fn render_drawer(ui: &mut IMUI, state: &mut EnkrState, pal: &Colors) {
    // A window widened past the breakpoint puts the sidebar back inline, so
    // the drawer has to let go of it rather than covering a layout that
    // already has one.
    if !state.drawer_open || !is_narrow(ui) {
        state.drawer_open = false;
        state.drawer_armed = false;
        return;
    }
    let pal = *pal;
    let (screen_w, screen_h) = ui.window_size();
    // Always leaves a strip of the content visible: a drawer that covered
    // everything would read as a new screen, with nothing to tap to get back.
    let width = state.side_width.min(screen_w - DRAWER_PEEK);

    let mut scrim_color = Color::new("#000000");
    scrim_color.a = 0.32;
    let scrim = ui.floating_pane_at(Point::new(0.0, 0.0), Some("###enkr_drawer_scrim"), |_| {});
    scrim
        .width(ui, UISize::Pixels(screen_w))
        .height(ui, UISize::Pixels(screen_h))
        .padding_all(ui, 0.0)
        .corner_radius(ui, 0.0)
        .background(ui, scrim_color)
        .border_color(ui, scrim_color);

    let pane = ui.floating_pane_at(Point::new(0.0, 0.0), Some("###enkr_drawer"), |ui| {
        sidebar(ui, state, &pal, width);
    });
    pane.width(ui, UISize::Pixels(width))
        .height(ui, UISize::Pixels(screen_h))
        .padding_all(ui, 0.0)
        .corner_radius(ui, 0.0)
        .background(ui, pal.sidebar_bg)
        .border_color(ui, pal.border);

    // The scrim is deliberately *not* in the pane list: pressing it is a press
    // outside the drawer, which is exactly what should close it.
    if state.drawer_armed && ui.press_outside(&[pane]) {
        state.drawer_open = false;
    } else {
        state.drawer_armed = true;
    }
}

/// How much of the content stays visible beside an open drawer.
const DRAWER_PEEK: f32 = 56.0;

/// A small label pinned to the cursor while a sidebar note/folder is being
/// dragged, so the user can see what they're moving. Drawn on the overlay layer
/// above the sidebar rows.
pub(crate) fn render_drag_ghost(ui: &mut IMUI, state: &EnkrState, pal: &Colors) {
    let Some(drag) = state.drag.as_ref() else {
        return;
    };
    let Some(mouse) = ui.mouse_position() else {
        return;
    };
    let label = match &drag.item {
        DragItem::Note(id) => state.notes.note_title(id).map(str::to_string),
        DragItem::Folder(id) => state.notes.folder(id).map(|f| f.name.clone()),
    };
    let Some(label) = label else {
        return;
    };
    let pos = Point::new(mouse.x() + 12.0, mouse.y() + 12.0);
    let ghost = ui.floating_pane_at(pos, Some("###enkr_drag_ghost"), |ui| {
        ui.label(&label)
            .text_color(ui, pal.accent_text)
            .font_size(ui, 13.0);
    });
    ghost
        .padding(ui, 4.0, 10.0, 4.0, 10.0)
        .background(ui, pal.accent)
        .border_color(ui, pal.accent)
        .corner_radius(ui, 6.0);
}

pub(crate) fn render_folder_branch(
    ui: &mut IMUI,
    pal: &Colors,
    notes_db: &NoteDatabase,
    notes: &[(NoteSummary, SyncIndicator, Vec<Presence>)],
    active_note: &str,
    active_space: i64,
    folder_id: Uuid,
    depth: usize,
    drag: &DragView,
    fold_folder: &mut Option<(Uuid, bool)>,
    select_note: &mut Option<String>,
    note_menu: &mut Option<(Menu, Point)>,
    folder_menu: &mut Option<(Menu, Point)>,
    drag_begin: &mut Option<DragItem>,
    hover_target: &mut Option<DropTarget>,
    inline_edit: &mut Option<InlineEdit>,
    inline_target: &Option<RenameTarget>,
    commit_rename: &mut bool,
) {
    let Some(folder) = notes_db.folder(&folder_id) else {
        return;
    };
    if folder.space_id != active_space {
        return;
    }

    let expanded = !folder.folded;
    let target = DropTarget::Folder(folder.id);
    let renaming = inline_target
        .as_ref()
        .is_some_and(|t| *t == RenameTarget::Folder(folder.id));
    let mut committed = false;
    let row = folder_row(
        ui,
        pal,
        &folder.id,
        &folder.name,
        expanded,
        depth,
        drag.target == Some(target),
        renaming.then(|| inline_edit.as_mut()).flatten(),
        &mut committed,
    );
    *commit_rename |= committed;
    // Click toggles the fold, but not on the frame a drag is released over it.
    if row.clicked() && !drag.active && !renaming {
        *fold_folder = Some((folder.id, expanded));
    }
    if row.right_clicked() {
        *folder_menu = Some((
            Menu::Folder {
                id: folder.id,
                space: active_space,
            },
            ui.mouse_position().unwrap_or(Point::new(60.0, 160.0)),
        ));
    }
    if row.dragging() {
        *drag_begin = Some(DragItem::Folder(folder.id));
    }
    // A folder is a drop target (move a note in, or reparent another folder).
    if drag.active
        && row.signal().mouse_over()
        && drag
            .item
            .as_ref()
            .is_some_and(|item| drop_allowed(notes_db, item, target))
    {
        *hover_target = Some(target);
    }
    if !expanded {
        return;
    }

    for child in notes_db
        .folders_in_space(active_space)
        .filter(|child| child.parent == Some(folder.id))
    {
        render_folder_branch(
            ui,
            pal,
            notes_db,
            notes,
            active_note,
            active_space,
            child.id,
            depth + 1,
            drag,
            fold_folder,
            select_note,
            note_menu,
            folder_menu,
            drag_begin,
            hover_target,
            inline_edit,
            inline_target,
            commit_rename,
        );
    }

    let note_indent = ((depth + 1) as f32) * 14.0;
    for (summary, indicator, presence) in notes
        .iter()
        .filter(|(summary, _, _)| summary.folder == Some(folder.id))
    {
        let selected = summary.id == active_note;
        let item = note_item(
            ui,
            pal,
            summary,
            selected,
            *indicator,
            presence,
            note_indent,
        );
        if item.clicked() && !drag.active {
            *select_note = Some(summary.id.clone());
        }
        if item.right_clicked() {
            *note_menu = Some((
                Menu::Note(summary.id.clone()),
                ui.mouse_position().unwrap_or(Point::new(60.0, 160.0)),
            ));
        }
        if item.dragging() {
            *drag_begin = Some(DragItem::Note(summary.id.clone()));
        }
    }
}

/// A collapsible folder row in the note list. Click toggles, right-click
/// opens the folder menu (handled by the caller via the returned handle).
#[allow(clippy::too_many_arguments)]
pub(crate) fn folder_row(
    ui: &mut IMUI,
    pal: &Colors,
    id: &Uuid,
    name: &str,
    expanded: bool,
    depth: usize,
    drop_highlight: bool,
    edit: Option<&mut InlineEdit>,
    committed: &mut bool,
) -> UIBoxHandle {
    let pal = *pal;
    let icon = if expanded {
        FOLDER_OPEN_ICON
    } else {
        FOLDER_ICON
    };
    let row = ui.clickable_row(&format!("###enkr_folder_{id}"), |ui| {
        ui.icon_label(icon)
            .width(ui, UISize::Pixels(22.0))
            .font_size(ui, 16.0)
            .text_color(ui, pal.icon);
        *committed = row_name(ui, &pal, "###enkr_folder_rename_field", name, 13.0, edit);
    });
    let bg = if drop_highlight {
        drop_target_bg(&pal)
    } else if row.hover() {
        pal.hover_bg
    } else {
        transparent_like(pal.hover_bg)
    };
    row.width(ui, UISize::ParentPct(1.0))
        .height(ui, UISize::Pixels(30.0))
        .padding(ui, 6.0, 6.0, 6.0, 6.0 + depth as f32 * 14.0)
        .corner_radius(ui, 8.0)
        .background(ui, bg)
        .align(ui, MainAxisAlign::Start, CrossAxisAlign::Center)
        .gap(ui, 8.0);
    row
}

/// A note list item: title + date on the first line, preview on the second.
pub(crate) fn note_item(
    ui: &mut IMUI,
    pal: &Colors,
    summary: &NoteSummary,
    selected: bool,
    indicator: SyncIndicator,
    presence: &[Presence],
    indent: f32,
) -> UIBoxHandle {
    let pal = *pal;
    let id = &summary.id;
    let date = short_date(&summary.updated);
    let item = ui.clickable_column(&format!("###enkr_note_{id}"), |ui| {
        let header = ui.named_row(&format!("###enkr_note_head_{id}"), |ui| {
            ui.label(&summary.title)
                .width(ui, UISize::Fill)
                .height(ui, UISize::Pixels(18.0))
                .text_color(ui, pal.text)
                .font_size(ui, 14.0);
            presence_badges(ui, presence);
            indicator_dot(ui, indicator);
            ui.label(&date)
                .text_color(ui, pal.text_faint)
                .font_size(ui, 12.0);
        });
        header
            .width(ui, UISize::ParentPct(1.0))
            .height(ui, UISize::Pixels(18.0))
            .align(ui, MainAxisAlign::Start, CrossAxisAlign::Center)
            .gap(ui, 8.0);

        ui.label(&summary.preview)
            .width(ui, UISize::ParentPct(1.0))
            .height(ui, UISize::Pixels(16.0))
            .text_color(ui, pal.text_muted)
            .font_size(ui, 12.0);
    });
    let hovering = item.hover();
    let bg = if selected {
        pal.selected_bg
    } else if hovering {
        pal.hover_bg
    } else {
        transparent_like(pal.hover_bg)
    };
    item.width(ui, UISize::ParentPct(1.0))
        .height(ui, UISize::Pixels(54.0))
        .padding(ui, 8.0, 8.0, 8.0, 8.0 + indent)
        .corner_radius(ui, 8.0)
        .background(ui, bg)
        .gap(ui, 4.0)
        .clip(ui, true);
    item
}

/// A compact sidebar row for an image blob in the active space. Clicking it
/// opens the image in the content view; right-click renames/deletes it.
pub(crate) fn blob_item(
    ui: &mut IMUI,
    pal: &Colors,
    name: &str,
    edit: Option<&mut InlineEdit>,
    committed: &mut bool,
) -> UIBoxHandle {
    let pal = *pal;
    let item = ui.clickable_row(&format!("###enkr_blob_{name}"), |ui| {
        ui.icon_label(IMAGE_ICON)
            .width(ui, UISize::Pixels(18.0))
            .height(ui, UISize::Pixels(18.0))
            .font_size(ui, 15.0)
            .text_color(ui, pal.text_muted);
        *committed = row_name(ui, &pal, "###enkr_blob_rename_field", name, 13.0, edit);
    });
    let bg = if item.hover() {
        pal.hover_bg
    } else {
        transparent_like(pal.hover_bg)
    };
    item.width(ui, UISize::ParentPct(1.0))
        .height(ui, UISize::Pixels(28.0))
        .padding_all(ui, 6.0)
        .corner_radius(ui, 6.0)
        .background(ui, bg)
        .align(ui, MainAxisAlign::Start, CrossAxisAlign::Center)
        .gap(ui, 6.0)
        .clip(ui, true);
    item
}

/// Bottom sidebar toolbar with secondary actions.
/// The sidebar's status pill: connection state, this identity's nickname, and
/// the single settings entry point.
///
/// Replaces a strip of five unlabelled glyphs (new space / import / export /
/// sync / settings). Those actions moved to the space menu and Settings; what
/// belongs here permanently is the one thing the chrome never showed before —
/// whether you are actually connected. `connected()`, `has_pending()` and
/// `last_error()` were all already computed and only visible inside a window
/// you had to know to open.
pub(crate) fn sidebar_footer(ui: &mut IMUI, state: &mut EnkrState, pal: &Colors) {
    let pal = *pal;
    let theme = *ui.theme();
    let (label, color) = state.connection_status(&pal);

    let footer = ui.named_row("###enkr_sidebar_footer", |ui| {
        let pill = ui.clickable_row("###enkr_status_pill", |ui| {
            ui.label("\u{25cf}")
                .width(ui, UISize::Pixels(10.0))
                .font_size(ui, 11.0)
                .text_color(ui, color);
            ui.label(&label)
                .width(ui, UISize::Fill)
                .font_size(ui, 12.0)
                .text_color(ui, pal.text_muted);
        });
        let bg = if pill.hover() {
            pal.hover_bg
        } else {
            transparent_like(pal.hover_bg)
        };
        pill.width(ui, UISize::Fill)
            .height(ui, UISize::Pixels(28.0))
            .padding(ui, 4.0, 6.0, 4.0, 6.0)
            .corner_radius(ui, theme.radius)
            .background(ui, bg)
            .align(ui, MainAxisAlign::Start, CrossAxisAlign::Center)
            .gap(ui, 6.0)
            .cursor(ui, OSCursor::Hand);
        if pill.clicked() {
            // Straight to the sync category — the pill is the connection
            // indicator, so it opens the place connections are configured.
            state.open_settings(SettingsSection::Sync);
            if let Some(sync) = state.sync.as_mut()
                && sync.connected()
            {
                sync.refresh_remote_spaces();
            }
        }

        if ui
            .button_icon_plain(
                &format!("{SETTINGS_ICON}###enkr_settings_button"),
                Some("Settings"),
            )
            .clicked()
        {
            state.open_settings(SettingsSection::General);
        }
    });
    footer
        .width(ui, UISize::ParentPct(1.0))
        .height(ui, UISize::Pixels(34.0))
        .align(ui, MainAxisAlign::Start, CrossAxisAlign::Center)
        .gap(ui, 4.0);
}

/// A row's name — or a live text field, when that row is being renamed.
///
/// Returns `true` on the frame the edit is committed (Enter, via the widget's
/// own commit signal, or focus moving away), so the caller can apply it. This
/// is the whole of inline rename: no dialog, no separate state machine per
/// row type.
pub(crate) fn row_name(
    ui: &mut IMUI,
    pal: &Colors,
    id: &str,
    name: &str,
    font_size: f32,
    edit: Option<&mut InlineEdit>,
) -> bool {
    let pal = *pal;
    let Some(edit) = edit else {
        ui.label(name)
            .width(ui, UISize::Fill)
            .text_color(ui, pal.text)
            .font_size(ui, font_size);
        return false;
    };

    let field = ui.line_edit(id, &mut edit.buffer, false);
    field
        .width(ui, UISize::Fill)
        .height(ui, UISize::Pixels(22.0))
        .font_size(ui, font_size)
        .text_color(ui, pal.text)
        .background(ui, pal.app_bg)
        .border_color(ui, pal.accent);
    if edit.focus_pending {
        ui.focus_box(field);
        edit.focus_pending = false;
        return false;
    }
    if edit.select_pending {
        // Select the existing name so typing replaces it — renaming usually
        // means "call it something else", not "add to what it is called".
        // Finder and VS Code both do this. Deferred one frame because the
        // field has no edit state to select within until it has been built.
        let len = edit.buffer.chars().count();
        ui.set_textarea_cursor(field, len, Some(0));
        edit.select_pending = false;
        return false;
    }
    // Enter commits (see `UISignal::COMMIT`), and so does clicking away — an
    // edit left open by wandering off should keep what was typed, not discard
    // it. Escape reverts, through the layer router.
    field.signal().committed() || !field.signal().mouse_over() && ui.press_outside(&[field])
}

/// The sidebar header: the active space, as a dropdown trigger.
#[allow(clippy::too_many_arguments)]
pub(crate) fn space_switcher(
    ui: &mut IMUI,
    pal: &Colors,
    name: &str,
    indicator: SyncIndicator,
    presence: &[Presence],
    open: bool,
    edit: Option<&mut InlineEdit>,
    committed: &mut bool,
) -> UIBoxHandle {
    let pal = *pal;
    let row = ui.clickable_row("###enkr_space_switcher", |ui| {
        ui.icon_label(SPACE_ICON)
            .width(ui, UISize::Pixels(24.0))
            .font_size(ui, 18.0)
            .text_color(ui, pal.accent);
        *committed = row_name(ui, &pal, "###enkr_space_rename_field", name, 14.0, edit);
        presence_badges(ui, presence);
        indicator_dot(ui, indicator);
        ui.icon_label(CHEVRON_ICON)
            .width(ui, UISize::Pixels(18.0))
            .font_size(ui, 16.0)
            .text_color(ui, pal.icon);
    });
    let bg = if open || row.hover() {
        pal.hover_bg
    } else {
        transparent_like(pal.hover_bg)
    };
    row.width(ui, UISize::ParentPct(1.0))
        .height(ui, UISize::Pixels(36.0))
        .padding_all(ui, 7.0)
        .corner_radius(ui, 8.0)
        .background(ui, bg)
        .align(ui, MainAxisAlign::Start, CrossAxisAlign::Center)
        .gap(ui, 6.0)
        .cursor(ui, OSCursor::Hand);
    row
}
