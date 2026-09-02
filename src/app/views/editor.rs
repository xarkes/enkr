//! The document views: the markdown editor and the image viewer, plus the
//! machinery that keeps the caret honest across remote edits and the image
//! pipeline that feeds inline pictures to the renderer.
//!
//! Both keep the sidebar and breadcrumb — they are panes of the workspace, not
//! full-window destinations like Settings.

use crate::app::*;

/// CRDT anchors for the local caret/selection, captured before applying
/// remote updates and resolved back afterwards so the caret sticks to its
/// logical position (Google-Docs-style) instead of its numeric index.
pub(crate) struct CaretGuard {
    pub(crate) handle: UIBoxHandle,
    pub(crate) caret: yrs::StickyIndex,
    pub(crate) caret_idx: usize,
    pub(crate) selection: Option<(yrs::StickyIndex, usize)>,
}

pub(crate) fn capture_caret_guard(state: &EnkrState, ui: &IMUI) -> Option<CaretGuard> {
    state.sync.as_ref()?;
    let (note_id, handle) = state.editor_handle.as_ref()?;
    if *note_id != state.active_note_id {
        return None;
    }
    let note = state.notes.note(note_id)?;
    note.remote_doc()?;
    let caret_idx = ui.textarea_cursor(*handle)?;
    let caret = note.caret_anchor(caret_idx)?;
    let selection = ui
        .textarea_selection(*handle)
        .and_then(|(anchor, _)| note.caret_anchor(anchor).map(|sticky| (sticky, anchor)));
    Some(CaretGuard {
        handle: *handle,
        caret,
        caret_idx,
        selection,
    })
}

pub(crate) fn restore_caret_guard(state: &EnkrState, ui: &mut IMUI, guard: Option<CaretGuard>) {
    let Some(guard) = guard else {
        return;
    };
    let Some(note) = state.notes.note(&state.active_note_id) else {
        return;
    };
    let Some(new_caret) = note.caret_from_anchor(&guard.caret) else {
        return;
    };
    let old_anchor = guard.selection.as_ref().map(|(_, old)| *old);
    let new_anchor = guard
        .selection
        .as_ref()
        .map(|(sticky, old)| note.caret_from_anchor(sticky).unwrap_or(*old));
    let unchanged = new_caret == guard.caret_idx && new_anchor == old_anchor;
    if !unchanged {
        ui.set_textarea_cursor(guard.handle, new_caret, new_anchor);
    }
}

/// Default display height for a freshly inserted image link (`h=` is the
/// default pinned dimension).
pub(crate) const DEFAULT_INSERT_IMAGE_HEIGHT: u32 = 240;

/// Decode encoded image bytes (PNG/JPEG/TIFF) to `(width, height, rgba)`.
/// Native only — image support is a base-app scope cut for the web build
/// (see `image_pump`'s wasm32 stub).
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn decode_rgba(bytes: &[u8]) -> Option<(u32, u32, Vec<u8>)> {
    let img = image::load_from_memory(bytes).ok()?;
    let rgba = img.to_rgba8();
    let (w, h) = rgba.dimensions();
    Some((w, h, rgba.into_raw()))
}

/// Re-encode arbitrary image bytes (e.g. a TIFF clipboard) to PNG for storage.
/// Native only — see `decode_rgba`.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn reencode_png(bytes: &[u8]) -> Option<Vec<u8>> {
    let img = image::load_from_memory(bytes).ok()?;
    let mut out = Vec::new();
    img.write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png)
        .ok()?;
    Some(out)
}

/// Normalise an image for storage: keep PNG/JPEG verbatim, transcode anything
/// else (e.g. TIFF, BMP) to PNG. Returns `None` if the bytes aren't an image.
/// Native only — see `decode_rgba`.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn normalize_image_for_storage(bytes: &[u8]) -> Option<(Vec<u8>, ImageMime)> {
    match image::guess_format(bytes) {
        Ok(image::ImageFormat::Png) => Some((bytes.to_vec(), ImageMime::Png)),
        Ok(image::ImageFormat::Jpeg) => Some((bytes.to_vec(), ImageMime::Jpeg)),
        _ => reencode_png(bytes).map(|png| (png, ImageMime::Png)),
    }
}

/// Bridge between the editor's image registry and the blob store: fulfils
/// `./blob/<name>` image requests with decoded RGBA, and turns a pasted image
/// into a stored blob + inserted markdown link.
///
/// Native only. The wasm32 counterpart below does the same three jobs (fulfil
/// requests, turn newly-picked bytes into a blob + link, apply resizes) but
/// never decodes pixels — the DOM backend hands a `<img>` element the
/// original bytes directly and lets the browser decode them (see
/// `IMUI::provide_image_encoded`), and clipboard image paste stays a no-op on
/// wasm32 (`os::clipboard_get_image`), so there's no `take_pasted_image` leg.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn image_pump(ui: &mut IMUI, state: &mut EnkrState) {
    let space_id = state.active_space_id;

    // Badge inline images whose content the relay doesn't have yet (pending,
    // offline, or a permanently-failed upload), in synced spaces only. Marked
    // each frame (mae clears it at begin_frame); usually a no-op (nothing owed).
    if state.notes.space_remote(space_id).is_some() {
        for blob in state.notes.blobs_in_space(space_id) {
            if blob.needs_push {
                ui.mark_image_unsynced(format!("./blob/{}", blob.name));
            }
        }
    }

    // Fulfil image requests recorded by the editor this frame.
    for key in ui.take_requested_images() {
        let Some(name) = key.strip_prefix("./blob/") else {
            continue;
        };
        // Local blobs render immediately. A synced blob whose content hasn't
        // been fetched yet is pulled by id once its index-doc metadata has been
        // adopted (see `adopt_remote_blobs`); until then it shows a placeholder.
        let Some(blob) = state.notes.blob_by_name(space_id, name) else {
            continue;
        };
        if blob.bytes.is_empty() {
            continue;
        }
        if let Some((w, h, rgba)) = decode_rgba(&blob.bytes) {
            ui.provide_image(key.clone(), w, h, &rgba);
            ui.request_repaint();
        }
    }

    // A pasted image becomes a blob (PNG/JPEG kept as-is, anything else — e.g.
    // a TIFF clipboard — transcoded to PNG) + an inserted link at the caret.
    if let Some(bytes) = ui.take_pasted_image()
        && let Some((data, mime)) = normalize_image_for_storage(&bytes)
    {
        let name = match mime {
            ImageMime::Jpeg => "pasted-image.jpg",
            ImageMime::Png => "pasted-image.png",
        };
        let blob_id = state.notes.create_blob_in(space_id, name, mime, data);
        if let Some(name) = state.notes.blob(&blob_id).map(|b| b.name.clone()) {
            insert_image_link(ui, state, &name);
            state.upload_blob_if_synced(&blob_id);
        }
    }

    // Links queued by the toolbar "Insert image" action.
    for name in std::mem::take(&mut state.pending_image_inserts) {
        insert_image_link(ui, state, &name);
    }

    // A corner-drag resize rewrites the pinned `?h=`/`?w=` of the image's link.
    if let Some((key, resize)) = ui.take_image_resize() {
        let (param, value) = match resize {
            ImageResize::Width(w) => ('w', w.round() as u32),
            ImageResize::Height(h) => ('h', h.round() as u32),
        };
        apply_image_resize(ui, state, &key, param, value);
    }
}

/// wasm32 counterpart to the native `image_pump` above — see its doc comment
/// for how the two differ.
#[cfg(target_arch = "wasm32")]
pub(crate) fn image_pump(ui: &mut IMUI, state: &mut EnkrState) {
    let space_id = state.active_space_id;

    // Badge inline images whose content the relay doesn't have yet — same as
    // native, this part has nothing to do with pixel decode.
    if state.notes.space_remote(space_id).is_some() {
        for blob in state.notes.blobs_in_space(space_id) {
            if blob.needs_push {
                ui.mark_image_unsynced(format!("./blob/{}", blob.name));
            }
        }
    }

    // Fulfil image requests recorded by the editor this frame. Only the
    // intrinsic size is computed on the Rust side (from the format header,
    // via `imagesize` — no pixel decode); the bytes go to the browser as-is
    // and it decodes them itself once the `<img>`'s `src` is set (`paint_dom
    // .rs::paint_image`).
    for key in ui.take_requested_images() {
        let Some(name) = key.strip_prefix("./blob/") else {
            continue;
        };
        let Some(blob) = state.notes.blob_by_name(space_id, name) else {
            continue;
        };
        if blob.bytes.is_empty() {
            continue;
        }
        let Ok(size) = imagesize::blob_size(&blob.bytes) else {
            continue;
        };
        let mime = match blob.mime {
            ImageMime::Png => "image/png",
            ImageMime::Jpeg => "image/jpeg",
        };
        ui.provide_image_encoded(
            key.clone(),
            size.width as u32,
            size.height as u32,
            mime,
            &blob.bytes,
        );
        ui.request_repaint();
    }

    // A file picked via `insert_image_from_file`'s browser dialog, or an
    // image pasted into the editor (`IMUI::take_pasted_image`, fed by the
    // browser's own `paste` event on this backend), becomes a blob (stored
    // exactly as the browser handed it to us — no re-encode) + an inserted
    // link at the caret. Same handling as native's pasted-image leg.
    // `picked_image` is taken into its own statement (not the `if let`
    // scrutinee) so the `RefMut` borrow drops before the block below needs
    // `state` mutably. A pasted image has no filename of its own, so it
    // gets the same generic one native uses.
    let picked = state.picked_image.borrow_mut().take().or_else(|| {
        ui.take_pasted_image()
            .map(|bytes| ("pasted".to_string(), bytes))
    });
    if let Some((name, bytes)) = picked {
        let mime = if bytes.starts_with(b"\x89PNG") {
            ImageMime::Png
        } else {
            ImageMime::Jpeg
        };
        let name = if mime == ImageMime::Png && !name.to_lowercase().ends_with(".png") {
            format!("{name}.png")
        } else {
            name
        };
        let blob_id = state.notes.create_blob_in(space_id, &name, mime, bytes);
        if let Some(final_name) = state.notes.blob(&blob_id).map(|b| b.name.clone()) {
            insert_image_link(ui, state, &final_name);
            state.upload_blob_if_synced(&blob_id);
        }
    }

    // Links queued by the toolbar "Insert image" action.
    for name in std::mem::take(&mut state.pending_image_inserts) {
        insert_image_link(ui, state, &name);
    }

    // A corner-drag resize rewrites the pinned `?h=`/`?w=` of the image's
    // link — pure Rust-side mouse geometry, identical on every backend.
    if let Some((key, resize)) = ui.take_image_resize() {
        let (param, value) = match resize {
            ImageResize::Width(w) => ('w', w.round() as u32),
            ImageResize::Height(h) => ('h', h.round() as u32),
        };
        apply_image_resize(ui, state, &key, param, value);
    }
}

/// Rewrite the pinned size param of the `![](key…)` link for `target` in the
/// active note to `param=value` (`w`/`h`), as a targeted buffer edit (so it
/// syncs + undoes).
pub(crate) fn apply_image_resize(
    ui: &mut IMUI,
    state: &mut EnkrState,
    target: &str,
    param: char,
    value: u32,
) {
    let note_id = state.active_note_id.clone();
    let Some(note) = state.notes.note_mut(&note_id) else {
        return;
    };
    let text = note.text();
    let Some((char_start, char_end, new_url)) = image_size_edit(&text, target, param, value) else {
        return;
    };
    note.delete_range((char_start, char_end));
    note.insert_text(char_start, &new_url);
    ui.request_repaint();
}

/// Compute the buffer edit that sets the `?<param>=<value>` size of the
/// `(target…)` image link in `text` (replacing any existing `?w=`/`?h=`).
/// Returns `(char_start, char_end, new_url)` — the char range of the existing
/// URL and its replacement — or `None` if the link isn't found or unchanged.
/// Char offsets (not byte) so the edit applies cleanly to the Yrs buffer.
pub(crate) fn image_size_edit(
    text: &str,
    target: &str,
    param: char,
    value: u32,
) -> Option<(usize, usize, String)> {
    let needle = format!("({target}");
    let open = text.find(&needle)?;
    let url_byte_start = open + 1;
    let rel_close = text[url_byte_start..].find(')')?;
    let url_byte_end = url_byte_start + rel_close;
    let url = &text[url_byte_start..url_byte_end];
    let base = url.split('?').next().unwrap_or(url);
    let new_url = format!("{base}?{param}={value}");
    if new_url == url {
        return None;
    }
    let char_start = text[..url_byte_start].chars().count();
    let char_end = text[..url_byte_end].chars().count();
    Some((char_start, char_end, new_url))
}

/// Insert `![](./blob/<name>?h=NNN)` on its own line at the editor caret.
pub(crate) fn insert_image_link(ui: &mut IMUI, state: &mut EnkrState, name: &str) {
    let Some((note_id, editor)) = state.editor_handle.as_ref().map(|(n, h)| (n.clone(), *h)) else {
        return;
    };
    if note_id != state.active_note_id {
        return;
    }
    let cursor = ui.textarea_cursor(editor).unwrap_or(0);
    let Some(note) = state.notes.note_mut(&note_id) else {
        return;
    };
    let text = note.text();
    let at_line_start = cursor == 0 || text.chars().nth(cursor.saturating_sub(1)) == Some('\n');
    let mut snippet = String::new();
    if !at_line_start {
        snippet.push('\n');
    }
    snippet.push_str(&format!(
        "![](./blob/{name}?h={DEFAULT_INSERT_IMAGE_HEIGHT})"
    ));
    snippet.push('\n');
    let inserted = snippet.chars().count();
    note.insert_text(cursor, &snippet);
    ui.reveal_textarea_cursor(editor, cursor + inserted);
    ui.request_repaint();
}

/// Track the active note's caret position each frame so the last-session memory
/// (persisted on shutdown) reopens the note where the user left off.
pub(crate) fn capture_active_cursor(ui: &IMUI, state: &mut EnkrState) {
    let Some((note_id, editor)) = state.editor_handle.as_ref().map(|(n, h)| (n.clone(), *h)) else {
        return;
    };
    if note_id == state.active_note_id
        && let Some(cursor) = ui.textarea_cursor(editor)
    {
        state.active_cursor = cursor;
    }
}

/// Once the target note's editor is on screen, move its caret to a queued
/// search-result offset and scroll it into view. A cross-note jump waits a
/// frame for `content_panel` to rebuild the editor for the newly-selected note.
pub(crate) fn apply_pending_jump(ui: &mut IMUI, state: &mut EnkrState) {
    let Some((note_id, offset)) = state.pending_jump.clone() else {
        return;
    };
    let Some((editor_note, editor)) = state.editor_handle.as_ref().map(|(n, h)| (n.clone(), *h))
    else {
        return;
    };
    if editor_note != note_id || state.active_note_id != note_id {
        return;
    }
    let len = state
        .notes
        .note(&note_id)
        .map_or(0, |note| note.text().chars().count());
    ui.reveal_textarea_cursor(editor, offset.min(len));
    state.pending_jump = None;
}

/// Top application bar: branding + new-note action over the sidebar, then the active

/// Fake collaborator carets for `--demo`: one with a long name (exercises the
/// hover name tag) and one with a short name plus an active selection. Positions
/// are placed a third and two thirds into the note so their badges clear the
/// first line (where they'd otherwise be clipped above the text area).
#[cfg(debug_assertions)]
pub(crate) fn demo_remote_carets(text_len: usize) -> Vec<RemoteCaret> {
    if text_len == 0 {
        return Vec::new();
    }
    let at = |frac: f32| ((text_len as f32 * frac) as usize).min(text_len);
    vec![
        RemoteCaret {
            cursor: at(0.35),
            selection: None,
            color: presence_color(0),
            label: "Ada Lovelace".to_string(),
        },
        RemoteCaret {
            cursor: at(0.65),
            selection: Some((at(0.58), at(0.65))),
            color: presence_color(2),
            label: "Bo".to_string(),
        },
    ]
}

/// The main content area: the active note rendered as markdown, with live
/// collaborator carets and presence pings when the note is synced.
pub(crate) fn content_panel(ui: &mut IMUI, state: &mut EnkrState, pal: &Colors) {
    let pal = *pal;
    // Viewing an image blob replaces the editor with a full-area image
    // viewer. Works on every target — `image_viewer_panel` is just a
    // standalone `ui.image` box, resolved via `image_pump` (decoded RGBA on
    // native, browser-decoded bytes on wasm32/DOM; see `IMUI::
    // provide_image_encoded`).
    if let Some(name) = state.view.image().map(str::to_string) {
        if let Some(blob) = state.notes.blob_by_name(state.active_space_id, &name) {
            // "Not uploaded" = a synced space still owes the relay this blob's
            // content (pending, offline, or permanently quarantined).
            let not_uploaded = blob.needs_push && state.notes.space_remote(blob.space_id).is_some();
            state.editor_handle = None;
            image_viewer_panel(ui, &name, not_uploaded, &pal);
            return;
        }
        // The blob was renamed/deleted out from under the view.
        state.set_view(View::Editor);
    }
    let active_note_id = state.active_note_id.clone();
    let wrap_x = state.wrap_x;
    let remote_doc = state
        .notes
        .note(&active_note_id)
        .and_then(|note| note.remote_doc());
    // Read-only when the active note lives in a synced space where this device
    // is only a Reader. Local-only spaces and not-yet-known roles stay editable
    // so the editor never flashes read-only before membership loads.
    let read_only = state
        .notes
        .note(&active_note_id)
        .map(|note| note.space_id())
        .and_then(|local| state.notes.space_remote(local))
        .zip(state.sync.as_ref())
        .is_some_and(|(remote, sync)| !sync.can_write(remote));
    // Track which doc we're focused on so presence moves with the active note
    // (and clears on the doc we left). Runs even when the new note isn't synced
    // — `remote_doc` is then `None`, which still sends the leave.
    if let Some(sync) = state.sync.as_mut() {
        sync.focus_doc(remote_doc);
    }
    let panel = ui.named_column("###enkr_content", |ui| {
        let mut editor_handle = None;
        if let Some(note) = state.notes.note_mut(&active_note_id) {
            editor_handle = Some(md_editor_viewer(
                ui,
                "###enkr_md_editor_viewer",
                note,
                wrap_x,
                read_only,
                &pal,
            ));
        }
        if let Some((_, editor)) = editor_handle {
            state.editor_handle = Some((active_note_id.clone(), editor));
            // Second half of the "New note" hand-off: the name is settled, so
            // the caret moves to the body and the user carries on typing
            // without reaching for the mouse.
            if state.new_note_focus == NewNoteFocus::Body {
                ui.focus_box(editor);
                state.new_note_focus = NewNoteFocus::Idle;
            }
            // --demo: paint synthetic collaborator carets (no sync needed) so
            // the badge centering and hover name tag are visible single-handed.
            #[cfg(debug_assertions)]
            if state.demo_presence && state.sync.is_none() {
                let text_len = state
                    .notes
                    .note(&active_note_id)
                    .map(|note| note.text().chars().count())
                    .unwrap_or(0);
                ui.set_remote_carets(editor, demo_remote_carets(text_len));
            }
        }
        let (Some((_, editor)), Some(doc), Some(sync)) =
            (editor_handle, remote_doc, state.sync.as_mut())
        else {
            return;
        };
        let Some(note) = state.notes.note(&active_note_id) else {
            return;
        };
        // Share our caret + selection as CRDT anchors (throttled inside, and
        // held back while local edits are still in flight — see presence_ping).
        let caret = ui.textarea_cursor(editor);
        let selection_anchor = ui.textarea_selection(editor).map(|(anchor, _)| anchor);
        sync.presence_ping(doc, note, caret, selection_anchor);
        // Overlay collaborator carets/selections, resolving their anchors
        // against our replica (they track concurrent edits between pings).
        let carets: Vec<RemoteCaret> = sync
            .presence(&doc)
            .iter()
            .filter_map(|p| {
                let cursor = note.caret_from_anchor(p.caret.as_ref()?)?;
                let selection = p
                    .selection_anchor
                    .as_ref()
                    .and_then(|sticky| note.caret_from_anchor(sticky))
                    .map(|anchor| (anchor.min(cursor), anchor.max(cursor)))
                    .filter(|(start, end)| start < end);
                Some(RemoteCaret {
                    cursor,
                    selection,
                    color: presence_color(p.color_slot()),
                    label: p.nickname.clone(),
                })
            })
            .collect();
        ui.set_remote_carets(editor, carets);
    });
    // No panel padding: the editor fills the area so its scrollbar sits flush at the
    // right edge. The text inset is supplied by the editor's own padding instead.
    panel
        .width(ui, UISize::Fill)
        .height(ui, UISize::ParentPct(1.0))
        .background(ui, pal.content_bg);
}

/// Full-area viewer for an image blob (contain-fit, centered). Shown instead of
/// the note editor while the active view is [`View::Image`]. A caption bar names it
/// and offers a way back to the note.
pub(crate) fn image_viewer_panel(ui: &mut IMUI, name: &str, not_uploaded: bool, pal: &Colors) {
    let pal = *pal;
    let warning = ui.theme().warning;
    let key = format!("./blob/{name}");
    let panel = ui.named_column("###enkr_image_view", |ui| {
        ui.image("###enkr_image_view_img", &key)
            .width(ui, UISize::Fill)
            .height(ui, UISize::Fill);
        let caption = ui.named_row("###enkr_image_caption", |ui| {
            ui.label(name)
                .width(ui, UISize::Fill)
                .text_color(ui, pal.text_muted)
                .font_size(ui, 12.0);
            // "Not uploaded" badge: the blob's content isn't on the relay, so
            // peers can't see this image (see the durability gate).
            if not_uploaded {
                ui.icon_label(WARNING_ICON)
                    .text_color(ui, warning)
                    .font_size(ui, 15.0);
                ui.label("Not uploaded")
                    .text_color(ui, warning)
                    .font_size(ui, 12.0);
            }
        });
        caption
            .width(ui, UISize::ParentPct(1.0))
            .height(ui, UISize::Pixels(20.0))
            .gap(ui, 6.0)
            .align(ui, MainAxisAlign::Start, CrossAxisAlign::Center);
    });
    panel
        .width(ui, UISize::Fill)
        .height(ui, UISize::ParentPct(1.0))
        .padding_all(ui, 24.0)
        .gap(ui, 12.0)
        .background(ui, pal.content_bg);
}

/// Returns `(panel, editor)` handles — the editor one is needed for caret
/// introspection and remote-caret overlays.
pub(crate) fn md_editor_viewer(
    ui: &mut IMUI,
    id: &str,
    note: &mut Note,
    wrap_x: bool,
    read_only: bool,
    pal: &Colors,
) -> (UIBoxHandle, UIBoxHandle) {
    let pal = *pal;
    let editor_id = format!("###enkr_editor_{}", note.id());
    let mut editor_handle = None;
    let handle = ui.named_column(id, |ui| {
        // When wrapping is off, allow horizontal scrolling so long lines stay reachable.
        // Padding must be supplied through the options (not a post-hoc `.padding`
        // builder): the wrap width is computed inside the textarea while it emits its
        // lines, so the inset has to be known before then. Setting it afterwards would
        // wrap against the default inset and overflow the right padding. Left and right
        // insets are kept equal so the text is horizontally centred; the scrollbar
        // floats over the edge without reserving layout space, so it needs no
        // asymmetric compensation.
        let options = TextAreaOptions::new()
            .wrap_x(wrap_x)
            .scroll_x(!wrap_x)
            .scroll_y(true)
            .border(false)
            .read_only(read_only)
            .padding(Padding {
                top: 28.0,
                right: 36.0,
                bottom: 28.0,
                left: 36.0,
            });
        // Source notes (imported non-markdown text files) are shown verbatim;
        // only markdown notes get the markdown styling/render pass.
        let editor = if note.is_source_only() {
            ui.textarea_with_options(&editor_id, note, options)
        } else {
            ui.markdown_textarea_with_options(&editor_id, note, options)
        };
        // Sit flush in the content area: no border (even when focused), just the
        // rendered note, with the scrollbar at the far right edge.
        editor
            .height(ui, UISize::Fill)
            .background(ui, pal.content_bg);
        editor_handle = Some(editor);
    });

    let handle = handle
        .width(ui, UISize::ParentPct(1.0))
        .height(ui, UISize::Fill);
    (handle, editor_handle.expect("editor built"))
}
