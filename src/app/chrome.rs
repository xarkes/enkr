//! The top bar: the active document's date, editable title and quick actions,
//! plus the web build's download strip. The redesign replaces the centred title
//! with a `Space / Folder / Title` breadcrumb and moves "New note" to the
//! sidebar.

use crate::app::*;

/// note's date, title and quick actions over the content area.
pub(crate) fn top_bar(ui: &mut IMUI, state: &mut EnkrState, pal: &Colors) {
    let pal = *pal;
    let title = state
        .notes
        .note_title(&state.active_note_id)
        .map(str::to_string)
        .unwrap_or_else(|| state.active_note_id.clone());
    let date = state
        .notes
        .note_updated(&state.active_note_id)
        .map(long_date)
        .unwrap_or_default();

    let narrow = is_narrow(ui);
    let bar = ui.named_row("###enkr_topbar", |ui| {
        // The bar sits *inside* the content column now, so it starts where the
        // content does and needs no spacer standing in for the sidebar. The
        // sidebar runs the full height of the window beside it.
        let right = ui.named_row("###enkr_topbar_right", |ui| {
            // On a narrow viewport the sidebar is a drawer, and this is the
            // only way to reach it (see `render_drawer`).
            if narrow
                && ui
                    .button_icon_plain(
                        &format!("{MENU_ICON}###enkr_drawer_toggle"),
                        Some("Show notes"),
                    )
                    .clicked()
            {
                state.drawer_open = !state.drawer_open;
                state.drawer_armed = false;
            }
            // `Folder / Subfolder /` leading the title, so a note nested in the
            // tree says where it lives. The space is not repeated here — the
            // sidebar header names it a few pixels away.
            let crumbs = state.active_note_folder_path();
            if !narrow && !crumbs.is_empty() {
                ui.label(&crumbs)
                    .text_color(ui, pal.text_faint)
                    .font_size(ui, 13.0);
                ui.label("/")
                    .text_color(ui, pal.text_faint)
                    .font_size(ui, 13.0);
            }

            // The title is editable and is the source of truth for the file name:
            // committing a change renames the note's file. The edit buffer is held
            // in state across frames (reseeded only when the active note changes)
            // so erasing doesn't snap back to the stored title. A blank buffer is
            // kept as-is while editing; only a non-empty title is committed, so the
            // file never loses its name. Styled as a plain centered label that
            // hugs its text — the I-beam hover cursor signals it's editable.
            // While an image is the content view, the top label shows and edits
            // the image's filename (renaming the blob); otherwise it's the
            // active note's title.
            let viewing_blob_id = state.view.image().and_then(|name| {
                state
                    .notes
                    .blob_by_name(state.active_space_id, name)
                    .map(|blob| blob.id.clone())
            });
            let style_title = |field: UIBoxHandle, ui: &mut IMUI| {
                // `TextContent(0.0)` makes the box hug the text so the line-edit's
                // own symmetric padding is all that's left/right of it.
                field
                    .width(ui, UISize::TextContent(0.0))
                    .text_color(ui, pal.text)
                    .font_size(ui, 14.0)
                    .background(ui, pal.app_bg)
                    .border_color(ui, pal.app_bg);
            };
            if let Some(blob_id) = viewing_blob_id {
                let current = state
                    .notes
                    .blob(&blob_id)
                    .map(|blob| blob.name.clone())
                    .unwrap_or_default();
                let buf = match &mut state.blob_title_edit {
                    Some((id, buf)) if *id == blob_id => buf,
                    slot => {
                        *slot = Some((blob_id.clone(), current.clone()));
                        &mut slot.as_mut().unwrap().1
                    }
                };
                let before = buf.clone();
                let field = ui.line_edit("###enkr_blob_title", buf, false);
                let edited = state.blob_title_edit.as_ref().map(|(_, b)| b.clone());
                style_title(field, ui);
                if let Some(edited) = edited
                    && edited != before
                    && !edited.trim().is_empty()
                {
                    state.notes.rename_blob(&blob_id, edited.trim());
                    // Keep the view (and content-panel lookup) on the renamed blob.
                    if let Some(new_name) = state.notes.blob(&blob_id).map(|b| b.name.clone()) {
                        state.view = View::Image(new_name);
                    }
                }
            } else {
                let active_note_id = state.active_note_id.clone();
                let title_buf = match &mut state.title_edit {
                    Some((note_id, buf)) if *note_id == active_note_id => buf,
                    slot => {
                        *slot = Some((active_note_id.clone(), title.clone()));
                        &mut slot.as_mut().unwrap().1
                    }
                };
                let before = title_buf.clone();
                let title_field = ui.line_edit("###enkr_note_title", title_buf, false);
                let edited = state.title_edit.as_ref().map(|(_, buf)| buf.clone());
                style_title(title_field, ui);
                if let Some(edited) = edited
                    && edited != before
                {
                    state.notes.set_note_title(&active_note_id, &edited);
                }
                // "New note" lands the caret here rather than in the body: the
                // title is what you actually type first, and it lives in the
                // top bar where it had to be hunted for and clicked.
                match state.new_note_focus {
                    NewNoteFocus::Title => {
                        ui.focus_box(title_field);
                        state.new_note_focus = NewNoteFocus::SelectTitle;
                    }
                    NewNoteFocus::SelectTitle => {
                        // Select the placeholder name so typing replaces it.
                        let len = state
                            .title_edit
                            .as_ref()
                            .map(|(_, buf)| buf.chars().count())
                            .unwrap_or(0);
                        ui.set_textarea_cursor(title_field, len, Some(0));
                        state.new_note_focus = NewNoteFocus::Naming;
                    }
                    NewNoteFocus::Naming => {
                        // Enter settles the name. Escape does too, but is
                        // handled by the app's Escape router — it consumes the
                        // event before this code runs (see `app::render`).
                        if title_field.signal().committed() {
                            state.new_note_focus = NewNoteFocus::Body;
                        }
                    }
                    NewNoteFocus::Idle | NewNoteFocus::Body => {}
                }
            }

            ui.named_column("###enkr_topbar_spacer_r", |_| {})
                .width(ui, UISize::Fill);

            // The date moves to the right, beside the document actions: the
            // breadcrumb is the answer to "where am I", which is the question
            // the left of a title bar should answer. Dropped entirely on a
            // narrow viewport — "September 2, 2026" is ~120px of a 390px bar,
            // and it is the least useful thing in it.
            if !narrow {
                ui.label(&date)
                    .text_color(ui, pal.text_faint)
                    .font_size(ui, 13.0);
            }

            // Source notes have no rendered view, so hide the markdown toggle.
            let source_only = state
                .notes
                .note(&state.active_note_id)
                .is_some_and(|note| note.is_source_only());
            if !source_only {
                // Not on the web build. The rendered view there is a whole
                // separate editing surface — a `contenteditable` host whose
                // content is the painted markdown, with every keystroke
                // intercepted and re-derived (`paint_dom.rs`'s
                // `attach_richtext_listeners`) — and it is not yet on par
                // with native's. Offering a toggle into a worse editor is
                // worse than not offering it, so the web build stays in
                // source mode (which `main.rs` already sets) until it is.
                #[cfg(not(target_arch = "wasm32"))]
                {
                    let rendered = ui.markdown_mode() == MarkdownMode::Rendered;
                    let (markdown_icon, markdown_tooltip) = if rendered {
                        (SOURCE_MARKDOWN_ICON, "Show markdown source")
                    } else {
                        (RENDER_MARKDOWN_ICON, "Render markdown")
                    };
                    if ui
                        .button_icon_plain(
                            &format!("{markdown_icon}###enkr_markdown_mode"),
                            Some(markdown_tooltip),
                        )
                        .clicked()
                    {
                        ui.set_markdown_mode(if rendered {
                            MarkdownMode::Source
                        } else {
                            MarkdownMode::Rendered
                        });
                    }
                }

                if ui
                    .button_icon_plain(
                        &format!("{IMAGE_ICON}###enkr_insert_image"),
                        Some("Insert image"),
                    )
                    .clicked()
                {
                    state.insert_image_from_file();
                }
            }

            if ui
                .button_icon_plain(
                    &format!("{SEARCH_ICON}###enkr_search"),
                    Some(&format!(
                        "Search all notes ({}+Shift+F)",
                        OSEventFlag::command_label()
                    )),
                )
                .clicked()
            {
                state.open_search(SearchScope::Global);
            }

            // Dropped on a narrow viewport, and only this one: Settings →
            // General has the same dark-mode toggle, and `MORE_ICON` beside
            // this opens Settings. Every other control here is the only way to
            // reach what it does, so trimming further would be removing
            // features rather than tidying a toolbar.
            if !narrow {
                let (theme_icon, theme_tooltip) = match state.theme_kind {
                    ThemeKind::Dark => (LIGHT_THEME_ICON, "Switch to light theme"),
                    ThemeKind::Light => (DARK_THEME_ICON, "Switch to dark theme"),
                };
                if ui
                    .button_icon_plain(&format!("{theme_icon}###enkr_theme"), Some(theme_tooltip))
                    .clicked()
                {
                    state.theme_kind = match state.theme_kind {
                        ThemeKind::Dark => ThemeKind::Light,
                        ThemeKind::Light => ThemeKind::Dark,
                    };
                }
            }
            if ui
                .button_icon_plain(&format!("{MORE_ICON}###enkr_topbar_more"), Some("Settings"))
                .clicked()
            {
                state.open_settings(SettingsSection::General);
            }
        });
        right
            .width(ui, UISize::Fill)
            .height(ui, UISize::ParentPct(1.0))
            .padding_all(ui, 14.0)
            .align(ui, MainAxisAlign::Start, CrossAxisAlign::Center)
            .gap(ui, 10.0);
    });
    bar.width(ui, UISize::ParentPct(1.0))
        .height(ui, UISize::Pixels(56.0))
        .gap(ui, 0.0)
        .background(ui, pal.app_bg);
}

/// Web build only: a thin strip above the app chrome naming the running
/// build (version + commit) and linking to the downloadable desktop app —
/// nothing native builds need, since they already are that app.
#[cfg(target_arch = "wasm32")]
pub(crate) fn web_download_banner(ui: &mut IMUI, pal: &Colors) {
    let mut bg = pal.accent;
    bg.a = 0.12;

    let bar = ui.named_row("###enkr_web_banner", |ui| {
        let profile = if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        };
        ui.label(&format!(
            "Enkr {} ({}) - {}",
            env!("CARGO_PKG_VERSION"),
            env!("ENKR_GIT_HASH"),
            profile
        ))
        .text_color(ui, pal.text_muted)
        .align(ui, MainAxisAlign::Start, CrossAxisAlign::Center)
        .font_size(ui, 12.0);

        ui.named_column("###enkr_web_banner_spacer", |_| {})
            .width(ui, UISize::Fill);

        let download = enkr_button(
            ui,
            "Download###enkr_web_banner_download",
            Some("Get the Enkr desktop app"),
            BtnVariant::Primary,
        )
        .height(ui, UISize::Pixels(24.0));
        if download.clicked()
            && let Some(window) = web_sys::window()
        {
            let _ = window.open_with_url_and_target("http://enkr.xark.es/dl/", "_blank");
        }
    });
    bar.width(ui, UISize::ParentPct(1.0))
        .height(ui, UISize::Pixels(32.0))
        .padding(ui, 0.0, 16.0, 0.0, 16.0)
        .align(ui, MainAxisAlign::Start, CrossAxisAlign::Center)
        .gap(ui, 10.0)
        .background(ui, bg)
        .border_color(ui, pal.border);
}
