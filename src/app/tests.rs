//! Tests for the app layer. Kept inside the crate rather than promoted to an
//! integration test so they can reach private state and helpers without
//! widening the public API purely for testing.

use super::{
    CLOSE_ICON, Color, Colors, DARK_THEME_ICON, DragItem, DropTarget, EnkrState, LIGHT_THEME_ICON,
    META_LAST_CURSOR, META_LAST_NOTE, MoveSubject, PaletteAction, RENDER_MARKDOWN_ICON,
    RenameTarget, SCROLLBAR_GUTTER, SETTINGS_ICON, SOURCE_MARKDOWN_ICON, SearchScope,
    SettingsSection, View, decode_rgba, image_size_edit, normalize_image_for_storage, render,
    settings_toggle, transparent_like,
};
use enkr_proto::wire::ImageMime;

#[test]
fn image_size_edit_rewrites_size_param() {
    // Adds `?h=` when absent.
    let text = "intro\n![](./blob/cat.png)\nmore";
    let (start, end, new_url) = image_size_edit(text, "./blob/cat.png", 'h', 240).unwrap();
    let chars: Vec<char> = text.chars().collect();
    let replaced: String = chars[start..end].iter().collect();
    assert_eq!(replaced, "./blob/cat.png");
    assert_eq!(new_url, "./blob/cat.png?h=240");

    // Replaces an existing `?w=` with `?h=` (switches pinned axis).
    let text2 = "![](./blob/cat.png?w=320)";
    let (s2, e2, new2) = image_size_edit(text2, "./blob/cat.png", 'h', 200).unwrap();
    assert_eq!(&text2[s2..e2], "./blob/cat.png?w=320");
    assert_eq!(new2, "./blob/cat.png?h=200");

    // No change → None.
    assert!(image_size_edit("![](./blob/cat.png?h=240)", "./blob/cat.png", 'h', 240).is_none());
    // Missing link → None.
    assert!(image_size_edit("no image here", "./blob/cat.png", 'h', 100).is_none());
}

#[test]
fn image_size_edit_char_offsets_handle_multibyte() {
    // A multibyte prefix means byte offsets != char offsets; the returned
    // range must be in chars so the Yrs buffer edit lands correctly.
    let text = "café 🎉\n![](./blob/x.png?h=100)";
    let (start, end, new_url) = image_size_edit(text, "./blob/x.png", 'h', 200).unwrap();
    let chars: Vec<char> = text.chars().collect();
    assert_eq!(
        chars[start..end].iter().collect::<String>(),
        "./blob/x.png?h=100"
    );
    assert_eq!(new_url, "./blob/x.png?h=200");
}

fn tiny_png() -> Vec<u8> {
    let mut img = image::RgbaImage::new(4, 4);
    for px in img.pixels_mut() {
        *px = image::Rgba([10, 20, 30, 255]);
    }
    let mut png = Vec::new();
    image::DynamicImage::ImageRgba8(img)
        .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
        .unwrap();
    png
}

#[test]
fn dragging_image_grip_resizes_h_param() {
    let mut harness = UiHarness::new(900.0, 600.0);
    let mut state = test_state();
    let space = state.active_space_id;
    let id = state
        .notes
        .create_blob_in(space, "pic.png", ImageMime::Png, tiny_png());
    let name = state.notes.blob(&id).unwrap().name.clone();
    let note_id = state.active_note_id.clone();
    {
        let note = state.notes.note_mut(&note_id).unwrap();
        let len = note.text().chars().count();
        if len > 0 {
            note.delete_range((0, len));
        }
        note.insert_text(0, &format!("![](./blob/{name}?h=200)"));
    }
    let key = format!("./blob/{name}");
    let mut bounds = None;
    for _ in 0..4 {
        let snap = harness.frame(|ui| {
            ui.set_markdown_mode(MarkdownMode::Rendered);
            render(ui, &mut state);
        });
        bounds = snap.try_node(&key).map(|n| n.bounds);
    }
    let bounds = bounds.expect("image node laid out");

    // Press in the bottom-right grip, then drag down to grow the height.
    let gx = bounds.x1 - 6.0;
    let gy = bounds.y1 - 6.0;
    harness.mouse_move(gx, gy);
    harness.mouse_down(OSKey::LeftMouseButton, gx, gy);
    harness.frame(|ui| {
        ui.set_markdown_mode(MarkdownMode::Rendered);
        render(ui, &mut state);
    });
    // Hold the button still for a couple of frames before moving — this is
    // the gap (neither pressed nor dragging) that must not drop the resize.
    for _ in 0..2 {
        harness.frame(|ui| {
            ui.set_markdown_mode(MarkdownMode::Rendered);
            render(ui, &mut state);
        });
    }
    for step in 1..=4 {
        harness.mouse_move(gx, gy + step as f32 * 30.0);
        harness.frame(|ui| {
            ui.set_markdown_mode(MarkdownMode::Rendered);
            render(ui, &mut state);
        });
    }
    harness.mouse_up(OSKey::LeftMouseButton, gx, gy + 120.0);

    let text = state.notes.note(&note_id).unwrap().text();
    let h: f32 = text
        .split("?h=")
        .nth(1)
        .and_then(|s| s.trim_end_matches(')').parse().ok())
        .unwrap_or(0.0);
    assert!(
        h > 200.0,
        "grip drag should grow h beyond 200, got {h} in {text:?}"
    );
}

#[test]
fn viewing_blob_hides_editor_and_clearing_restores_it() {
    let mut harness = UiHarness::new(900.0, 600.0);
    let mut state = test_state();
    let space = state.active_space_id;
    let id = state
        .notes
        .create_blob_in(space, "pic.png", ImageMime::Png, tiny_png());
    let name = state.notes.blob(&id).unwrap().name.clone();

    // Editor present in normal mode.
    harness.frame(|ui| render(ui, &mut state));
    assert!(state.editor_handle.is_some());

    // Viewing the image swaps the editor out for the image viewer.
    state.view = View::Image(name);
    harness.frame(|ui| render(ui, &mut state));
    assert!(
        state.editor_handle.is_none(),
        "editor should be hidden while viewing an image"
    );

    // Clearing the view brings the editor back.
    state.view = View::Editor;
    harness.frame(|ui| render(ui, &mut state));
    assert!(
        state.editor_handle.is_some(),
        "editor returns when not viewing an image"
    );

    // A stale view (blob deleted) self-heals back to the editor.
    state.view = View::Image("gone.png".to_string());
    harness.frame(|ui| render(ui, &mut state));
    assert!(state.view == View::Editor);
    assert!(state.editor_handle.is_some());
}

#[test]
fn image_link_is_requested_and_provided_to_registry() {
    let mut harness = UiHarness::new(900.0, 600.0);
    let mut state = test_state();

    let space = state.active_space_id;
    let id = state
        .notes
        .create_blob_in(space, "test.png", ImageMime::Png, tiny_png());
    let name = state.notes.blob(&id).unwrap().name.clone();
    let note_id = state.active_note_id.clone();
    {
        let note = state.notes.note_mut(&note_id).unwrap();
        let len = note.text().chars().count();
        if len > 0 {
            note.delete_range((0, len));
        }
        note.insert_text(0, &format!("![](./blob/{name}?w=200)"));
    }

    // Render in Rendered mode so build requests the image and image_pump
    // resolves it into the registry. has_image is true once provided
    // (independent of the GPU upload, which is headless here).
    for _ in 0..4 {
        harness.frame(|ui| {
            ui.set_markdown_mode(MarkdownMode::Rendered);
            render(ui, &mut state);
        });
    }
    assert!(
        harness.ui().has_image("./blob/test.png"),
        "editor never requested + the pump never provided the image"
    );
}

#[test]
fn decode_and_normalize_round_trip_png_and_jpeg() {
    // A 2x2 image encoded by the `image` crate must decode back to RGBA via
    // the same crate config (default-features off + png/jpeg/tiff).
    let mut img = image::RgbaImage::new(2, 2);
    for (i, px) in img.pixels_mut().enumerate() {
        *px = image::Rgba([i as u8 * 40, 10, 20, 255]);
    }
    let dynimg = image::DynamicImage::ImageRgba8(img);

    let mut png = Vec::new();
    dynimg
        .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
        .unwrap();
    assert_eq!(decode_rgba(&png).map(|(w, h, _)| (w, h)), Some((2, 2)));
    assert!(matches!(
        normalize_image_for_storage(&png),
        Some((_, ImageMime::Png))
    ));

    let mut jpg = Vec::new();
    dynimg
        .write_to(
            &mut std::io::Cursor::new(&mut jpg),
            image::ImageFormat::Jpeg,
        )
        .unwrap();
    assert_eq!(decode_rgba(&jpg).map(|(w, h, _)| (w, h)), Some((2, 2)));
    assert!(matches!(
        normalize_image_for_storage(&jpg),
        Some((_, ImageMime::Jpeg))
    ));
}
use crate::note::NoteDatabase;
use mae::{
    imui::{IMUI, MarkdownMode, UIBoxFlags},
    os::{OSEventFlag, OSKey, OSKeyCode},
    testkit::{UiDriver, UiHarness},
};

fn test_state() -> EnkrState {
    EnkrState::with_notes(NoteDatabase::new_in_memory())
}

#[test]
fn reopens_last_note_at_saved_cursor_after_restart() {
    let mut state = test_state();
    let note_id = state.notes.create_note();
    state.active_note_id = note_id.clone();
    state.active_cursor = 4;
    // Quitting persists the session (last note + caret) into the note DB.
    state.shutdown();
    assert_eq!(state.notes.meta_get(META_LAST_NOTE), Some(note_id.as_str()));
    assert_eq!(state.notes.meta_get(META_LAST_CURSOR), Some("4"));

    // Reopening with the same database restores that note and queues the
    // caret jump back to the saved position.
    let reopened = EnkrState::with_notes(state.notes);
    assert_eq!(reopened.active_note_id, note_id);
    assert_eq!(reopened.pending_jump, Some((note_id, 4)));
}

#[test]
fn falls_back_to_first_note_when_last_note_is_gone() {
    let mut db = NoteDatabase::new_in_memory();
    db.meta_set(META_LAST_NOTE, "deleted-note-id");
    db.meta_set(META_LAST_CURSOR, "3");
    let state = EnkrState::with_notes(db);
    assert!(!state.active_note_id.is_empty());
    assert_ne!(state.active_note_id, "deleted-note-id");
    // No stale caret jump when the remembered note no longer exists.
    assert_eq!(state.pending_jump, None);
}

#[test]
fn restored_cursor_clamps_to_document_length() {
    let mut db = NoteDatabase::new_in_memory();
    let id = db.create_note();
    db.note_mut(&id).unwrap().insert_text(0, "short"); // 5 chars
    db.meta_set(META_LAST_NOTE, &id);
    db.meta_set(META_LAST_CURSOR, "9999");
    let mut state = EnkrState::with_notes(db);

    let mut harness = UiHarness::new(900.0, 600.0);
    harness.frame(|ui| {
        state.set_repaint_waker(ui.repaint_waker());
        render(ui, &mut state);
    });

    // A position past the (possibly post-sync shrunk) document clamps to its end.
    let (_, editor) = state.editor_handle.expect("editor built");
    assert_eq!(harness.ui().textarea_cursor(editor), Some(5));
}

fn cmd_f_opens_document_search_and_cmd_shift_f_opens_global<D: UiDriver>(driver: &mut D) {
    driver.key_press_with_flags(OSKeyCode::KeyF, OSEventFlag::command());
    assert!(
        driver.exists("Search in this note"),
        "Cmd+F should open the palette"
    );

    driver.key_press_with_flags(
        OSKeyCode::KeyF,
        OSEventFlag::command().with(OSEventFlag::Shift),
    );
    assert!(driver.exists("Search all your notes"));
}

#[test]
fn cmd_f_opens_document_search_and_cmd_shift_f_opens_global_native() {
    let mut state = test_state();
    let mut driver = mae::testkit::NativeDriver::new(900.0, 600.0, move |ui| {
        state.set_repaint_waker(ui.repaint_waker());
        render(ui, &mut state);
    });
    cmd_f_opens_document_search_and_cmd_shift_f_opens_global(&mut driver);
}

// Was ignored as a known product gap, now fixed and running: nothing in
// #mae-root is focused on a freshly loaded page (it is a plain `<div>` with
// no tabindex), and the global keydown listener used to be attached to that
// container — so a keydown dispatched to the unfocused `<body>` never
// bubbled into it and every global shortcut silently did nothing until the
// first click. The listener lives on `document` now (`os/wasm.rs`).
#[cfg(feature = "cdp")]
#[test]
fn cmd_f_opens_document_search_and_cmd_shift_f_opens_global_cdp() {
    let mut driver = crate::testkit_support::launch_test_harness();
    cmd_f_opens_document_search_and_cmd_shift_f_opens_global(&mut driver);
}

#[test]
fn search_streams_matches_from_worker_and_opens_result() {
    use std::time::{Duration, Instant};

    let mut harness = UiHarness::new(900.0, 600.0);
    let mut state = test_state();
    let needle_a = state.notes.create_note();
    state
        .notes
        .note_mut(&needle_a)
        .unwrap()
        .insert_text(0, "Grocery list with pineapple and bread");
    let needle_b = state.notes.create_note();
    state
        .notes
        .note_mut(&needle_b)
        .unwrap()
        .insert_text(0, "Meeting notes about the pineapple launch");
    let miss = state.notes.create_note();
    state
        .notes
        .note_mut(&miss)
        .unwrap()
        .insert_text(0, "Totally unrelated content");

    // Wire the repaint waker so the background worker can be spawned.
    harness.frame(|ui| {
        state.set_repaint_waker(ui.repaint_waker());
        render(ui, &mut state);
    });

    state.open_search(SearchScope::Global);
    state.search.as_mut().unwrap().query = "PINEAPPLE".to_string();

    // The scan runs off-thread; pump frames until both matches stream in.
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        harness.frame(|ui| render(ui, &mut state));
        let count = state.search.as_ref().map_or(0, |s| s.rows.len());
        if count >= 2 {
            break;
        }
        assert!(Instant::now() < deadline, "search results never arrived");
        std::thread::sleep(Duration::from_millis(10));
    }

    {
        let results = &state.search.as_ref().unwrap().rows;
        assert_eq!(results.len(), 2, "only the two matching notes should hit");
        assert!(
            results
                .iter()
                .any(|row| row.note_id() == Some(needle_a.as_str()))
        );
        assert!(
            results
                .iter()
                .any(|row| row.note_id() == Some(needle_b.as_str()))
        );
        assert!(
            !results
                .iter()
                .any(|row| row.note_id() == Some(miss.as_str()))
        );
        assert!(
            results
                .iter()
                .all(|row| row.subtitle.to_lowercase().contains("pineapple"))
        );
        // Each hit carries the match range(s) for highlighting.
        assert!(results.iter().all(|row| !row.highlights.is_empty()));
    }

    // Enter opens the first result and closes the palette.
    harness.key_press(OSKeyCode::KeyEnter);
    harness.frame(|ui| render(ui, &mut state));
    assert!(
        state.search.is_none(),
        "opening a result closes the palette"
    );
    assert_eq!(state.active_note_id, needle_a);
}

#[test]
fn cmd_o_opens_go_to_note_and_matches_titles_only() {
    use std::time::{Duration, Instant};

    let mut harness = UiHarness::new(900.0, 600.0);
    let mut state = test_state();
    // A note whose title matches but whose body does not.
    let target = state.notes.create_note();
    state.notes.set_note_title(&target, "Quarterly Roadmap");
    state
        .notes
        .note_mut(&target)
        .unwrap()
        .insert_text(0, "unrelated body text");
    // A note whose body matches the query but whose title does not, to prove
    // title search ignores the body.
    let decoy = state.notes.create_note();
    state.notes.set_note_title(&decoy, "Scratch");
    state
        .notes
        .note_mut(&decoy)
        .unwrap()
        .insert_text(0, "this mentions roadmap in the body");

    harness.frame(|ui| {
        state.set_repaint_waker(ui.repaint_waker());
        render(ui, &mut state);
    });

    // Cmd/Ctrl+O opens the "go to note" palette.
    harness.key_press_with_flags(OSKeyCode::KeyO, OSEventFlag::command());
    let snap = harness.frame(|ui| render(ui, &mut state));
    assert!(state.search.is_some(), "Cmd+O should open the palette");
    assert!(snap.try_node("Go to note by title").is_some());

    state.search.as_mut().unwrap().query = "roadmap".to_string();
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        harness.frame(|ui| render(ui, &mut state));
        let s = state.search.as_ref().unwrap();
        if s.search.as_ref().is_none_or(|r| !r.searching) {
            break;
        }
        assert!(Instant::now() < deadline, "title search never finished");
        std::thread::sleep(Duration::from_millis(10));
    }

    let results = &state.search.as_ref().unwrap().rows;
    assert_eq!(results.len(), 1, "only the title match should surface");
    assert_eq!(results[0].note_id(), Some(target.as_str()));

    // Enter opens the note without queueing a body caret jump.
    harness.key_press(OSKeyCode::KeyEnter);
    harness.frame(|ui| render(ui, &mut state));
    assert!(
        state.search.is_none(),
        "opening a result closes the palette"
    );
    assert_eq!(state.active_note_id, target);
    assert!(
        state.pending_jump.is_none(),
        "title search should not jump the editor caret"
    );
}

#[test]
fn refining_query_keeps_prior_results_until_new_scan_resolves() {
    use std::time::{Duration, Instant};

    let mut harness = UiHarness::new(900.0, 600.0);
    let mut state = test_state();
    for i in 0..3 {
        let id = state.notes.create_note();
        state
            .notes
            .note_mut(&id)
            .unwrap()
            .insert_text(0, &format!("pineapple note number {i}"));
    }
    harness.frame(|ui| {
        state.set_repaint_waker(ui.repaint_waker());
        render(ui, &mut state);
    });

    state.open_search(SearchScope::Global);
    state.search.as_mut().unwrap().query = "pineapple".to_string();
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        harness.frame(|ui| render(ui, &mut state));
        if state.search.as_ref().map_or(0, |s| s.rows.len()) >= 3 {
            break;
        }
        assert!(Instant::now() < deadline, "initial results never arrived");
        std::thread::sleep(Duration::from_millis(10));
    }

    // Refine to a query with no matches. The frame that dispatches the new
    // scan must NOT clear the visible results (otherwise the list flickers
    // empty for a frame before the worker responds).
    state.search.as_mut().unwrap().query = "pineapplezzz".to_string();
    harness.frame(|ui| render(ui, &mut state));
    assert_eq!(
        state.search.as_ref().unwrap().rows.len(),
        3,
        "prior results should linger until the new scan resolves"
    );

    // Once the worker reports no matches, the stale results clear.
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        harness.frame(|ui| render(ui, &mut state));
        if state.search.as_ref().unwrap().rows.is_empty() {
            break;
        }
        assert!(Instant::now() < deadline, "stale results never cleared");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn search_result_row_fits_two_lines_and_highlights_match() {
    use std::time::{Duration, Instant};

    let mut harness = UiHarness::new(900.0, 600.0);
    let mut state = test_state();
    let id = state.notes.create_note();
    state
        .notes
        .note_mut(&id)
        .unwrap()
        .insert_text(0, "alpha bravo charlie");
    harness.frame(|ui| {
        state.set_repaint_waker(ui.repaint_waker());
        render(ui, &mut state);
    });

    state.open_search(SearchScope::Global);
    state.search.as_mut().unwrap().query = "bravo".to_string();
    let deadline = Instant::now() + Duration::from_secs(2);
    let snap = loop {
        let snap = harness.frame(|ui| render(ui, &mut state));
        if state.search.as_ref().map_or(0, |s| s.rows.len()) >= 1 {
            // One more frame so the populated row is laid out before snapshot.
            break harness.frame(|ui| render(ui, &mut state));
        }
        assert!(Instant::now() < deadline, "no results arrived");
        let _ = snap;
        std::thread::sleep(Duration::from_millis(10));
    };

    // Regression: the excerpt row used to default to ParentPct height and
    // collapse/overflow, leaving rows too short and overlapping. A row must fit
    // both its title line and its subtitle. Addressed by id rather than by
    // walking up from the title label — the title now sits inside its own row
    // (it shares a line with the sync dot), so "the label's parent" is no
    // longer the row.
    let row = snap.node("###enkr_search_hit_0");
    assert!(
        row.bounds.height() > 30.0,
        "result row should fit two text lines, got {}",
        row.bounds.height()
    );

    // The excerpt is one continuous label (not split into per-match
    // segments), so neighbouring text is never clipped or erased; the match
    // highlight is painted behind it (see `UIBoxHandle::text_highlights`).
    assert!(
        snap.nodes
            .iter()
            .any(|n| n.text.as_deref() == Some("alpha bravo charlie")),
        "excerpt should render as a single continuous label"
    );
}

#[test]
fn selecting_global_result_jumps_caret_to_match() {
    use std::time::{Duration, Instant};

    let mut harness = UiHarness::new(900.0, 600.0);
    let mut state = test_state();
    let id = state.notes.create_note();
    state
        .notes
        .note_mut(&id)
        .unwrap()
        .insert_text(0, "zero one two pineapple three");
    harness.frame(|ui| {
        state.set_repaint_waker(ui.repaint_waker());
        render(ui, &mut state);
    });

    state.open_search(SearchScope::Global);
    state.search.as_mut().unwrap().query = "pineapple".to_string();
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        harness.frame(|ui| render(ui, &mut state));
        if state.search.as_ref().map_or(0, |s| s.rows.len()) >= 1 {
            break;
        }
        assert!(Instant::now() < deadline, "no result arrived");
        std::thread::sleep(Duration::from_millis(10));
    }

    harness.key_press(OSKeyCode::KeyEnter);
    // Switch to the note, rebuild its editor, then apply the queued jump.
    for _ in 0..4 {
        harness.frame(|ui| render(ui, &mut state));
    }
    assert_eq!(state.active_note_id, id);
    let (_, editor) = state.editor_handle.expect("editor present");
    // "zero one two " is 13 chars; the caret lands on the match.
    assert_eq!(harness.ui().textarea_cursor(editor), Some(13));
    assert!(state.pending_jump.is_none(), "jump should be consumed");
}

#[test]
fn document_search_lists_each_occurrence_with_line_numbers() {
    use std::time::{Duration, Instant};

    let mut harness = UiHarness::new(900.0, 600.0);
    let mut state = test_state();
    let id = state.notes.create_note();
    state
        .notes
        .note_mut(&id)
        .unwrap()
        .insert_text(0, "foo here\nand foo again\nno match\nfoo end");
    state.select_note(id.clone());
    harness.frame(|ui| {
        state.set_repaint_waker(ui.repaint_waker());
        render(ui, &mut state);
    });

    state.open_search(SearchScope::Document);
    state.search.as_mut().unwrap().query = "foo".to_string();
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        harness.frame(|ui| render(ui, &mut state));
        if state.search.as_ref().map_or(0, |s| s.rows.len()) >= 3 {
            break;
        }
        assert!(Instant::now() < deadline, "occurrences never arrived");
        std::thread::sleep(Duration::from_millis(10));
    }

    let results = &state.search.as_ref().unwrap().rows;
    assert_eq!(results.len(), 3, "one hit per occurrence in the note");
    assert!(results.iter().all(|r| r.note_id() == Some(id.as_str())));
    assert!(results.iter().any(|r| r.title == "Line 1"));
    assert!(results.iter().any(|r| r.title == "Line 2"));
    assert!(results.iter().any(|r| r.title == "Line 4"));
}

fn escape_closes_search_palette<D: UiDriver>(driver: &mut D) {
    driver.key_press_with_flags(OSKeyCode::KeyF, OSEventFlag::command());
    assert!(driver.exists("Search in this note"));

    driver.key_press(OSKeyCode::KeyEscape);
    assert!(!driver.exists("Search in this note"));
}

#[test]
fn escape_closes_search_palette_native() {
    let mut state = test_state();
    let mut driver = mae::testkit::NativeDriver::new(900.0, 600.0, move |ui| {
        state.set_repaint_waker(ui.repaint_waker());
        render(ui, &mut state);
    });
    escape_closes_search_palette(&mut driver);
}

// Same unfocused-#mae-root gap as cmd_f_opens_document_search_and_cmd_shift_f_opens_global_cdp,
// fixed with it.
#[cfg(feature = "cdp")]
#[test]
fn escape_closes_search_palette_cdp() {
    let mut driver = crate::testkit_support::launch_test_harness();
    escape_closes_search_palette(&mut driver);
}

/// The sidebar shows the *active* space in its header and that space's notes
/// below. Other spaces are one click away in the switcher rather than always
/// on screen, so asserting they are all visible would now be asserting the old
/// layout back into existence.
fn space_rows_and_note_items_render<D: UiDriver>(driver: &mut D) {
    assert!(driver.exists("Space"), "active space names the switcher");
    assert!(
        !driver.exists("Work"),
        "other spaces live behind the switcher"
    );
    assert!(driver.exists("Product roadmap"), "its notes are listed");

    driver.click("###enkr_space_switcher");
    assert!(
        driver.exists("###enkr_search_input"),
        "the switcher palette should be open"
    );
    assert!(
        driver.exists("###enkr_search_hit_0"),
        "the switcher palette should have rows"
    );
    assert!(
        driver.exists("Work"),
        "the switcher palette lists every space"
    );
    // Palette rows are ordered as the spaces are; "Work" is the second.
    driver.click("###enkr_search_hit_1");
    assert!(
        driver.exists("Work"),
        "switching makes Work the active space"
    );
}
crate::driver_test!(
    space_rows_and_note_items_render,
    900.0,
    600.0,
    EnkrState::with_notes(NoteDatabase::demo())
);

// clicking_a_note_selects_it moved to enkr/tests/app_sync.rs (native+cdp).

#[test]
fn sidebar_note_idle_background_fades_from_hover_color() {
    let mut harness = UiHarness::new(900.0, 600.0);
    let mut state = EnkrState::with_notes(NoteDatabase::demo());
    let pal = Colors::for_kind(state.theme_kind);

    let snap = harness.frame(|ui| render(ui, &mut state));
    let title = snap
        .nodes
        .iter()
        .find(|node| node.text.as_deref() == Some("Product roadmap"))
        .expect("Product roadmap title");
    let header_key = title.parent_key.expect("note title parent row");
    let header = snap
        .nodes
        .iter()
        .find(|node| node.key == header_key)
        .expect("note item header row");
    let row_key = header.parent_key.expect("note header parent item");
    let row = snap
        .nodes
        .iter()
        .find(|node| node.key == row_key)
        .expect("Product roadmap note row");

    assert_color_eq(row.style.bg_color, transparent_like(pal.hover_bg));
}

fn settings_button_opens_settings_window<D: UiDriver>(driver: &mut D) {
    driver.click(SETTINGS_ICON);
    assert!(driver.exists("Settings"));
}
crate::driver_test!(
    settings_button_opens_settings_window,
    900.0,
    600.0,
    test_state()
);

#[test]
fn sidebar_footer_is_clickable_when_notes_list_scrolls() {
    let mut harness = UiHarness::new(900.0, 420.0);
    let mut state = test_state();
    for _ in 0..24 {
        state.notes.create_note();
    }

    let snap = harness.frame(|ui| render(ui, &mut state));
    assert!(
        snap.nodes.iter().any(|node| node.scroll_max.y() > 0.0),
        "notes list should scroll"
    );
    let pill = snap.node("###enkr_status_pill");
    assert!(
        pill.bounds.y1 <= 420.0,
        "status pill should remain in the visible window"
    );

    harness.click("###enkr_status_pill");
    harness.frame(|ui| render(ui, &mut state));

    assert_eq!(
        state.view,
        View::Settings(SettingsSection::Sync),
        "status pill should open the sync settings without scrolling the list first"
    );
}

fn settings_window_toggle_updates_state<D: UiDriver>(driver: &mut D) {
    driver.click(SETTINGS_ICON);
    driver.click("Editor"); // the wrap toggle lives in its own category now
    assert!(driver.exists("On"));
    driver.click("On");
    assert!(driver.exists("Off"));
}
crate::driver_test!(
    settings_window_toggle_updates_state,
    900.0,
    600.0,
    test_state()
);

/// Pasting an image into the editor stores it as a blob and inserts an
/// inline `![](./blob/<name>)` link at the caret — the same handling
/// native has always had for `os::clipboard_get_image`, which has no
/// synchronous equivalent in a browser.
///
/// CDP-only: it needs a real `ClipboardEvent` carrying a real `File`,
/// which only a browser has. The event is constructed and dispatched
/// from the page rather than driven through the OS clipboard because
/// headless Chrome will not grant clipboard *read* permission, and an
/// image has to be *read* back out to be pasted at all.
///
/// This did nothing before: `os::clipboard_get_image` is a no-op on
/// wasm32, so there was no path from a pasted picture to a blob.
#[cfg(feature = "cdp")]
#[test]
#[ignore = "needs www/build_test_harness.sh run first, plus a local chromium and python3 on PATH"]
fn pasting_an_image_inserts_a_blob_link() {
    // Smallest valid PNG (1x1) — enough for `imagesize` to read a real
    // header off, which the insert path needs for the link's size hint.
    const PNG_B64: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==";
    const EDITOR: &str = "(document.querySelector('[contenteditable=\"true\"]') || document.querySelector('textarea'))";

    let mut driver = crate::testkit_support::launch_test_harness();
    driver.click("Welcome");

    let dispatched = driver.debug_eval(&format!(
        "(function(){{
            const host = {EDITOR};
            if (!host) return 'NO EDITOR';
            const bin = atob('{PNG_B64}');
            const arr = new Uint8Array(bin.length);
            for (let i = 0; i < bin.length; i++) arr[i] = bin.charCodeAt(i);
            const dt = new DataTransfer();
            dt.items.add(new File([arr], 'x.png', {{type: 'image/png'}}));
            host.dispatchEvent(new ClipboardEvent('paste', {{
                clipboardData: dt, bubbles: true, cancelable: true
            }}));
            return 'ok';
        }})()"
    ));
    assert_eq!(
        dispatched.as_str(),
        Some("ok"),
        "could not dispatch the paste"
    );

    // Reading the file is asynchronous (`FileReader`), so the link
    // appears a few frames later rather than in the pasting frame.
    let text_expr = format!(
        "(function(){{ const e = {EDITOR}; return e ? (e.value ?? e.innerText) : ''; }})()"
    );
    let mut text = String::new();
    for _ in 0..40 {
        driver.debug_eval(
            "new Promise(d => requestAnimationFrame(() => requestAnimationFrame(() => d(0))))",
        );
        text = driver
            .debug_eval(&text_expr)
            .as_str()
            .unwrap_or_default()
            .to_string();
        if text.contains("./blob/") {
            break;
        }
    }
    assert!(
        text.contains("![](./blob/"),
        "pasting an image should insert an inline blob link — buffer starts: {:?}",
        text.chars().take(60).collect::<String>()
    );
}

/// A phone-width window puts the sidebar behind a drawer instead of beside
/// the editor.
///
/// The sidebar is a fixed 260px. On a 390px viewport that is two thirds of the
/// screen, leaving ~129px of note — and the top bar's icon buttons alone need
/// more than that, so mae's overflow pass (which shrinks, and then truncates
/// to zero — it has no wrap) quietly squashed the toolbar to nothing. Below
/// `NARROW_WIDTH` the sidebar stops being furniture and becomes something you
/// summon and dismiss.
///
/// Driven off the viewport alone, which is why this runs on the native
/// backend too: the same layout appears in a narrow desktop window, and a
/// rule that needs a phone to observe is a rule that never gets tested.
fn a_narrow_window_hides_the_sidebar_behind_a_drawer<D: UiDriver>(driver: &mut D) {
    assert!(
        !driver.exists("###enkr_notes_list"),
        "the note list should not be taking width from the editor here"
    );
    assert!(
        driver.exists("###enkr_drawer_toggle"),
        "and there should be a way to get it back"
    );

    driver.click("###enkr_drawer_toggle");
    assert!(
        driver.exists("###enkr_notes_list"),
        "the drawer should show the note list"
    );

    // Picking a note is what the drawer is for, so it closes behind you —
    // otherwise it would be covering the note it just opened.
    driver.click("Product roadmap");
    assert!(
        !driver.exists("###enkr_notes_list"),
        "opening a note should close the drawer"
    );

    // Escape is the other way out, for a drawer opened by mistake.
    driver.click("###enkr_drawer_toggle");
    assert!(driver.exists("###enkr_notes_list"));
    driver.key_press(OSKeyCode::KeyEscape);
    assert!(
        !driver.exists("###enkr_notes_list"),
        "Escape should close the drawer"
    );
}
crate::driver_test!(
    a_narrow_window_hides_the_sidebar_behind_a_drawer,
    390.0,
    844.0,
    EnkrState::with_notes(NoteDatabase::demo())
);

/// The other side of the breakpoint: at a normal window size the sidebar is
/// inline and there is no drawer to open. Without this the narrow layout could
/// quietly become the only layout.
fn a_wide_window_keeps_the_sidebar_inline<D: UiDriver>(driver: &mut D) {
    assert!(driver.exists("###enkr_notes_list"));
    assert!(!driver.exists("###enkr_drawer_toggle"));
}
crate::driver_test!(
    a_wide_window_keeps_the_sidebar_inline,
    900.0,
    600.0,
    EnkrState::with_notes(NoteDatabase::demo())
);

/// Tapping into a note must not zoom the page.
///
/// iOS Safari zooms in whenever a focused text field's font is under 16px —
/// mae's text is 14 — and then leaves the whole app scaled up and panned
/// sideways, so the field no longer spans the window and nothing lines up
/// again until the page is manually zoomed back out. The fix is to meet the
/// threshold rather than to disable zooming (`maximum-scale=1` would take the
/// *user's* pinch away, which is the opposite of what is wanted), so the
/// assertion is on the size the browser actually computes for a hosted field
/// on a touch device.
///
/// Chrome computes the same `(pointer: coarse)` rule under mobile emulation,
/// which is what makes this testable without a phone.
#[cfg(feature = "cdp")]
#[test]
fn a_text_field_on_a_touch_device_is_large_enough_not_to_zoom_the_page() {
    let mut driver = crate::testkit_support::launch_test_harness();
    // The note title is the smallest hosted field on screen, and the first
    // one a phone user taps.
    let title_font = "getComputedStyle(document.querySelector('[data-mae-key=\"###enkr_note_title\"]')).fontSize";

    // Read the mouse-driven size first, on the page as launched: the rule
    // only raises a size below the threshold on touch, it does not restyle
    // the app, and this is the "before" that gives the assertion below its
    // meaning. (Emulation is one-way here — `set_viewport` resizes but
    // leaves the emulated touchscreen in place.)
    let fine = font_px(&mut driver, title_font);
    assert!(
        fine < 16.0,
        "the app asks for a size under the threshold here, or this proves nothing: got {fine}px"
    );

    driver.emulate_mobile_device(390.0, 844.0);
    let coarse = font_px(&mut driver, title_font);
    assert!(
        coarse >= 16.0,
        "a hosted field on a touch device must be at least 16px or the page zooms on focus, got {coarse}px"
    );
}

/// `getComputedStyle(...).fontSize` in pixels, for the expression `expr`.
#[cfg(feature = "cdp")]
fn font_px(driver: &mut mae::testkit::cdp::CdpDriver, expr: &str) -> f64 {
    let value = driver.debug_eval(expr);
    value
        .as_str()
        .and_then(|s| s.trim_end_matches("px").parse().ok())
        .unwrap_or_else(|| panic!("no font size from {expr:?}: {value:?}"))
}

/// The note title is as wide as its title, at the size the browser draws it.
///
/// It asks to hug its text (`UISize::TextContent`), and on this backend that
/// was mae's *own* measurement of the text written out as a pixel width. mae
/// shapes with harfrust and the browser with its own engine, so the two were
/// always a pixel or two apart — and on a touch device they part company
/// completely, because the field is rendered at the 16px floor that keeps iOS
/// from zooming the page while mae measured the 14px the app asked for. The
/// title lost its last characters. `field-sizing: content` hands the job to
/// the only party that knows what it is about to draw.
#[cfg(feature = "cdp")]
#[test]
fn the_note_title_fits_the_text_the_browser_draws() {
    // `scrollWidth` past `clientWidth` is the browser saying "this does not
    // fit in the box you gave me".
    const OVERFLOW: &str = "(() => { \
       const el = document.querySelector('[data-mae-key=\"###enkr_note_title\"]'); \
       return el ? el.scrollWidth - el.clientWidth : -1; \
     })()";
    let mut driver = crate::testkit_support::launch_test_harness();

    let desktop = driver.debug_eval(OVERFLOW).as_f64().expect("note title");
    assert!(
        (0.0..=1.0).contains(&desktop),
        "the title should fit its own text on a desktop pointer, overflowing by {desktop}px"
    );

    // Where it really mattered: the touch font floor makes the text wider
    // than anything mae measured.
    driver.emulate_mobile_device(390.0, 844.0);
    let phone = driver.debug_eval(OVERFLOW).as_f64().expect("note title");
    assert!(
        (0.0..=1.0).contains(&phone),
        "the title should fit the 16px text a touch device draws, overflowing by {phone}px"
    );
}

/// A chromeless editor has no border on the web either.
///
/// The note editor asks for none (`TextAreaOptions::border(false)`, which
/// omits `DRAW_BORDER` outright — a transparent colour would not do, since
/// the painted border blends to the accent on focus), and native draws none.
/// The DOM backend passed a border to every hosted field regardless of the
/// flag, so on the web the editor sat in a box the native build has never
/// had.
#[cfg(feature = "cdp")]
#[test]
fn the_web_editor_has_no_border_around_it() {
    let mut driver = crate::testkit_support::launch_test_harness();
    let editor_border = "(() => { \
       const el = document.querySelector('[data-mae-key^=\"###enkr_editor_\"]'); \
       if (!el) return null; \
       const s = getComputedStyle(el); \
       return [s.borderTopWidth, s.borderRightWidth, s.borderBottomWidth, s.borderLeftWidth].join(' '); \
     })()";
    assert_eq!(
        driver.debug_eval(editor_border).as_str(),
        Some("0px 0px 0px 0px"),
        "the note editor should have no border on the DOM backend"
    );
}

/// Opening a popover costs a couple of frames, not a couple of dozen.
///
/// mae eases hover tints, focus rings, a pane appearing and scroll offsets by
/// interpolating them a step per frame and asking for another frame while
/// anything is still moving (`animate_visual_state`, `animate_scroll_offsets`).
/// On a GPU backend that *is* the animation. On the DOM backend it meant the
/// whole app was rebuilt and re-diffed at 60fps for the duration of every
/// fade — the browser can do that easing itself, off the main thread, for
/// nothing. Rust now writes the target value straight out and CSS eases it.
///
/// Counted rather than eyeballed: the app's own `requestAnimationFrame` calls
/// over one palette-opening click. The driver's `settle` uses the *unwrapped*
/// rAF, so its polling never inflates this.
#[cfg(feature = "cdp")]
#[test]
fn opening_a_popover_does_not_rebuild_for_the_length_of_its_fade() {
    let mut driver = crate::testkit_support::launch_test_harness();
    driver.debug_eval(
        "(() => { \
           window.__maeFrames = 0; \
           const prev = window.requestAnimationFrame; \
           window.requestAnimationFrame = (cb) => { window.__maeFrames++; return prev(cb); }; \
           return 0; \
         })()",
    );

    // One click, which opens the space palette — a floating pane with a
    // background and a border, i.e. everything that used to animate.
    driver.click("###enkr_space_switcher");
    assert!(
        driver.exists("###enkr_search_input"),
        "the palette should have opened"
    );

    let frames = driver
        .debug_eval("window.__maeFrames")
        .as_f64()
        .expect("frame counter should be readable");
    // 4 as this is written, against 17 with the easing still in Rust.
    assert!(
        frames <= 8.0,
        "opening a popover should cost a few frames, not one per frame of its fade — got {frames}"
    );
}

/// The app re-lays-out when the browser does, with mae's own loop frozen.
///
/// This is what a viewport change costs. mae solves the whole layout itself
/// and used to pin every box to the pixels that came out — so a browser-side
/// reflow (an on-screen keyboard, a rotation, a window resize, a font
/// finishing loading) showed last frame's layout until mae woke up, solved
/// again and rewrote every element. On a phone that is the visible lag when
/// the keyboard closes. A box that declared `Fill` or a percentage now says
/// so in CSS (`paint_dom.rs`'s `CssLen`), so the browser reflows it on the
/// spot — starting with the root, which fills its container instead of
/// restating the pixels mae measured out of it.
///
/// `requestAnimationFrame` is stubbed out first, so nothing mae does can be
/// what produced the new layout: every rebuild the app might schedule from
/// here on is dropped on the floor, and what is measured afterwards is the
/// browser's own work on what was already on the page. The sidebar is a fixed
/// width, so the content column beside it is what has to absorb the whole
/// change.
#[cfg(feature = "cdp")]
#[test]
fn the_layout_follows_the_viewport_without_a_rebuild() {
    const SHRINK_BY: f64 = 300.0;
    let mut driver = crate::testkit_support::launch_test_harness();
    const CONTENT_WIDTH: &str = "document.querySelector('[data-mae-key=\"###enkr_content_column\"]')\
                                 .getBoundingClientRect().width";

    let width = driver
        .debug_eval("window.innerWidth")
        .as_f64()
        .expect("window width");
    let height = driver
        .debug_eval("window.innerHeight")
        .as_f64()
        .expect("window height");
    let before = driver
        .debug_eval(CONTENT_WIDTH)
        .as_f64()
        .expect("content column");
    assert!(before > SHRINK_BY, "the column has to have room to lose");

    // From here on the app cannot draw at all.
    driver.debug_eval("window.requestAnimationFrame = () => 0; 0");
    driver.set_viewport((width - SHRINK_BY) as f32, height as f32);

    let after = driver
        .debug_eval(CONTENT_WIDTH)
        .as_f64()
        .expect("content column");
    assert!(
        (before - after - SHRINK_BY).abs() <= 2.0,
        "the content column should have absorbed the whole {SHRINK_BY}px on its own, \
         with no frame drawn: {before} -> {after}"
    );
}

/// On a phone the page lays out at the *identity's* width, and pinch-zoom
/// still works.
///
/// Neither was true: with no `<meta name="viewport">` a mobile browser lays
/// the page out in a ~980px viewport and scales the result down, so mae was
/// told the window was desktop-sized, laid out the desktop UI, and the
/// browser shrank it to illegibility — and `touch-action: none` on the
/// container meant it could not even be zoomed back up.
///
/// CDP-only, and it needs *mobile* emulation specifically: that is what makes
/// Chrome run the meta-viewport algorithm at all. A plain viewport override
/// sets the width directly and would pass whether or not the tag existed.
#[cfg(feature = "cdp")]
#[test]
fn the_web_page_lays_out_at_device_width() {
    let mut driver = crate::testkit_support::launch_test_harness();
    driver.emulate_mobile_device(390.0, 844.0);

    assert_eq!(
        driver.debug_eval("window.innerWidth").as_f64(),
        Some(390.0),
        "the page should lay out at the identity's width, not a default desktop one"
    );
    assert_eq!(
        driver
            .debug_eval("document.getElementById('mae-root').getBoundingClientRect().width")
            .as_f64(),
        Some(390.0),
        "and the app should fill it"
    );
    assert!(
        driver
            .debug_eval("getComputedStyle(document.getElementById('mae-root')).touchAction")
            .as_str()
            .unwrap_or_default()
            .contains("pinch-zoom"),
        "the browser must keep the pinch-zoom gesture"
    );
}

/// A one-finger drag scrolls a list, and the list is a real scroller.
///
/// mae used to scroll by transforming a wrapper inside a clipped box, with
/// nothing native under it: no scrollbar the browser drew, no momentum, no
/// rubber-banding, and — until `TouchPan` re-synthesised a drag into the
/// wheel's own `OSEvent::scroll` — no way to scroll by finger at all. The box
/// is `overflow: auto` now, so the browser does all of it and mae only
/// mirrors the offset it lands on (`paint_dom.rs`'s `attach_scroll_listener`).
///
/// Both halves are asserted: that it is a real scroller (an offset the
/// browser reports on the element itself, not a transform mae wrote), and
/// that the content tracks the finger. Not to the pixel any more, and that
/// is the point: the browser holds a gesture for its own touch slop before
/// deciding it is a pan (~15px in Chromium), so an 80px drag moves the list
/// by 80 *minus* that. mae's re-synthesised version had its own 8px slop and
/// then tracked exactly; a real scroller feels like every other scroller on
/// the device instead, which is what was wanted.
#[cfg(feature = "cdp")]
#[test]
fn a_touch_drag_scrolls_a_list() {
    // Short enough that the demo notes overflow the drawer's list. The list
    // has to genuinely have somewhere to scroll to, or this passes vacuously.
    let mut driver = crate::testkit_support::launch_test_harness();
    driver.emulate_mobile_device(390.0, 420.0);
    driver.click("###enkr_drawer_toggle");

    const LIST: &str = "(() => { \
           const list = document.querySelector('[data-mae-key=\"###enkr_notes_list\"]'); \
           if (!list) return 'no list'; \
           return JSON.stringify({top: list.scrollTop, \
             room: list.scrollHeight - list.clientHeight, \
             overflow: getComputedStyle(list).overflowY, \
             transform: [...list.children].some(c => c.style.transform)}); \
         })()";
    let read = |driver: &mut mae::testkit::cdp::CdpDriver| -> serde_json::Value {
        let raw = driver.debug_eval(LIST);
        serde_json::from_str(raw.as_str().expect("list on screen")).expect("list state")
    };

    let before = read(&mut driver);
    assert_eq!(
        before["overflow"], "auto",
        "the list should be a real scroller"
    );
    assert_eq!(before["transform"], false, "and not a transformed wrapper");
    assert_eq!(before["top"], 0.0, "starting unscrolled");
    assert!(
        before["room"].as_f64().unwrap_or(0.0) >= 80.0,
        "the list needs somewhere to scroll to, or this passes vacuously: {before}"
    );

    driver.touch_drag("###enkr_notes_list", 0.0, -80.0);
    let top = read(&mut driver)["top"].as_f64().unwrap_or(-1.0);
    assert!(
        (60.0..=80.0).contains(&top),
        "an 80px drag should scroll the list by 80px less the browser's own \
         touch slop, got {top}px"
    );
}

/// Arrowing down a long list scrolls it — the app moving a real scroller,
/// not mae moving something of its own.
///
/// The other half of handing scrolling to the browser. A wheel or a finger is
/// the browser's business end to end now, but the app still has one thing to
/// say about the offset — "keep the row I just selected on screen"
/// (`scroll_to_y`) — and that has to reach the element. mae keeps `scroll` as
/// a mirror of the browser's `scrollTop` and `scroll_target` as what it
/// wants, so the gap between them is exactly the programmatic move to push,
/// and a scroll the *user* performed closes that gap by arriving as both.
#[cfg(feature = "cdp")]
#[test]
fn keyboard_selection_scrolls_the_list_it_is_in() {
    const RESULTS: &str = "(() => { \
       const el = document.querySelector('[data-mae-key=\"###enkr_search_results\"]'); \
       if (!el) return null; \
       return JSON.stringify({top: el.scrollTop, room: el.scrollHeight - el.clientHeight}); \
     })()";
    let mut driver = crate::testkit_support::launch_test_harness();
    // A short window, so the four demo spaces are more rows than fit. The
    // space switcher rather than a search: it builds its rows synchronously,
    // where search streams them in from a worker the web build has none of.
    driver.emulate_mobile_device(390.0, 300.0);
    driver.key_press_with_flags(OSKeyCode::KeyK, OSEventFlag::command());
    assert!(
        driver.exists("###enkr_search_input"),
        "Cmd+K should open the space switcher"
    );

    let read = |driver: &mut mae::testkit::cdp::CdpDriver| -> serde_json::Value {
        let raw = driver.debug_eval(RESULTS);
        serde_json::from_str(raw.as_str().expect("row list on screen")).expect("list state")
    };
    let before = read(&mut driver);
    assert!(
        before["room"].as_f64().unwrap_or(0.0) >= 20.0,
        "the list needs to overflow, or this passes vacuously: {before}"
    );
    assert_eq!(before["top"], 0.0, "starting at the top");

    for _ in 0..3 {
        driver.key_press(OSKeyCode::KeyDownArrow);
    }
    let after = read(&mut driver);
    assert!(
        after["top"].as_f64().unwrap_or(0.0) > 0.0,
        "arrowing past the visible rows should have scrolled the list: {after}"
    );
}

/// Collaborator carets land on the right characters in the browser.
///
/// The DOM backend had no remote carets at all: native draws them inside its
/// GPU paint walk (`paint.rs::draw_remote_carets`), which this backend
/// replaces wholesale — so the web build showed presence badges saying
/// someone was in the note, and nothing at all saying *where*. They are drawn
/// now by mirroring the `<textarea>` into an overlay laid out identically and
/// asking the browser, via a `Range`, where each character actually is
/// (`paint_dom.rs`'s `paint_remote_carets`).
///
/// CDP-only, and structural rather than pixel-exact: the positions come from
/// the browser's own text layout, so there is no number to hardcode. What is
/// asserted is what a stub could not satisfy — that a caret sits at the end
/// of its own selection, on the same line, at a non-zero offset into the
/// text.
#[cfg(feature = "cdp")]
#[test]
fn remote_carets_are_placed_in_the_browsers_own_text_layout() {
    // `?demo=1` is the web equivalent of the native `--demo` flag: two
    // synthetic collaborators at a third and two thirds through the note, the
    // second with a selection (`app::demo_remote_carets`). A real second
    // client would work too, but its caret would be wherever *it* put it.
    let mut driver = crate::testkit_support::launch_test_harness_with_query("?demo=1");
    driver.click("Welcome");

    // Every marker inside the overlay: the caret bars, their badges, and one
    // band per line a selection spans.
    let markers = driver.debug_eval(
        "(() => { \
           const overlay = document.querySelector('.mae-remote-carets'); \
           if (!overlay) return null; \
           const mirror = overlay.firstElementChild; \
           return [...mirror.children].map(el => ({ \
             left: parseFloat(el.style.left), \
             top: parseFloat(el.style.top), \
             width: parseFloat(el.style.width) || 0, \
             label: el.textContent, \
           })); \
         })()",
    );
    let markers = markers
        .as_array()
        .expect("a caret overlay over the note editor")
        .clone();
    let field = |m: &serde_json::Value, k: &str| m[k].as_f64().unwrap_or(f64::NAN);

    let badges: Vec<&serde_json::Value> = markers
        .iter()
        .filter(|m| !m["label"].as_str().unwrap_or_default().is_empty())
        .collect();
    assert_eq!(
        badges
            .iter()
            .map(|m| m["label"].as_str().unwrap_or_default())
            .collect::<Vec<_>>(),
        vec!["A", "B"],
        "both demo collaborators should be badged, by initial"
    );
    assert!(
        badges.iter().all(|m| field(m, "top") > 0.0),
        "a badge on the first line would be clipped above the text; the demo \
         carets are placed further in than that"
    );

    // The second collaborator has a selection. Its caret sits at the end of
    // it: same line as the last band, at that band's right edge. Nothing but
    // a real measurement of the browser's own layout produces that.
    let bands: Vec<&serde_json::Value> = markers
        .iter()
        .filter(|m| m["label"].as_str().unwrap_or_default().is_empty() && field(m, "width") != 2.0)
        .collect();
    let last_band = bands
        .last()
        .expect("a selection band for the collaborator that has one");
    let caret_b = badges[1];
    let band_end = field(last_band, "left") + field(last_band, "width");
    assert!(
        (field(caret_b, "left") - band_end).abs() < 0.5,
        "the caret should sit where its selection ends — caret at {}, selection ends at {band_end}",
        field(caret_b, "left")
    );
    assert!(
        field(caret_b, "top") > field(bands[0], "top"),
        "the selection spans lines, so it should end below where it starts"
    );
}

/// The web build offers no way into rendered markdown, and its editor is a
/// plain `<textarea>` — never a `contenteditable` rich-text host.
///
/// The rendered view on the DOM backend is a second editing surface with its
/// own keystroke interception and caret placement, and it is not yet as good
/// as native's. A toggle into a worse editor is worse than no toggle, so the
/// top bar drops it there (`chrome.rs`) and `main.rs` pins source mode. The
/// element-kind half of the assertion is the one that matters: it is what
/// says no note *content* is being rendered, whatever a control elsewhere
/// might claim.
///
/// CDP-only: it is a statement about the web build specifically, and native
/// keeps the toggle — `top_bar_offers_the_markdown_toggle_on_native` is that
/// side.
#[cfg(feature = "cdp")]
#[test]
fn the_web_build_has_no_rendered_markdown_mode() {
    let mut driver = crate::testkit_support::launch_test_harness();
    driver.click("Welcome");
    assert!(
        !driver.exists("###enkr_markdown_mode"),
        "the web top bar should not offer the render-markdown toggle"
    );
    assert_eq!(
        driver
            .debug_eval("document.querySelectorAll('[contenteditable=\"true\"]').length")
            .as_i64(),
        Some(0),
        "the web editor should be a plain <textarea>, not a rich-text host"
    );
}

/// The native top bar *does* offer the markdown toggle — the other half of
/// `the_web_build_has_no_rendered_markdown_mode`, so cutting it from the web
/// build cannot quietly cut it from both.
#[test]
fn top_bar_offers_the_markdown_toggle_on_native() {
    let mut harness = UiHarness::new(900.0, 600.0);
    let mut state = EnkrState::with_notes(NoteDatabase::demo());
    let snap = harness.frame(|ui| render(ui, &mut state));
    assert!(snap.try_node("###enkr_markdown_mode").is_some());
}

/// The cryptographic identity must survive a page refresh on wasm32. It *is*
/// this identity's membership in every space it has been admitted to, so
/// a regenerated one silently orphans the device from all of them —
/// which is exactly what happened before `IdentityStore::LocalStorage`
/// existed (the web build used `IdentityStore::InMemory`, minting a
/// fresh key on every load).
///
/// CDP-only by nature: there is no `localStorage` to persist into, or
/// page to reload, in the native harness — the native side of this is
/// `sync::identity`'s own `identity_file_roundtrip_and_stability`.
///
/// Connecting is what creates the identity (`SyncClient::spawn`
/// resolves it before opening any socket), so the connection attempt
/// itself failing — `DEFAULT_SERVER` is a real external host this test
/// neither has nor wants — is irrelevant here, and deliberately not
/// asserted on.
#[cfg(feature = "cdp")]
#[test]
#[ignore = "needs www/build_test_harness.sh run first, plus a local chromium and python3 on PATH"]
fn device_identity_survives_a_page_reload() {
    const READ_KEY: &str = "window.localStorage.getItem('enkr_identity_key')";

    let mut driver = crate::testkit_support::launch_test_harness();
    assert!(
        driver.debug_eval(READ_KEY).is_null(),
        "a fresh browser profile should not have an identity key yet"
    );

    // The Connect button lives in Settings (the sync window is a
    // read-only status panel), alongside the server list and nickname.
    driver.click(SETTINGS_ICON);
    driver.click("Connect");
    let first = driver.debug_eval(READ_KEY);
    let first = first
        .as_str()
        .expect("connecting should have stored an identity key");
    assert_eq!(first.len(), 128, "64 key bytes, hex encoded");
    assert!(first.chars().all(|c| c.is_ascii_hexdigit()));

    driver.reload();
    assert_eq!(
        driver.debug_eval(READ_KEY).as_str(),
        Some(first),
        "the stored key must survive the reload untouched"
    );

    // And the reloaded app must *adopt* that key rather than mint a new
    // one over it — the actual regression, which an assertion on the
    // stored value alone would miss entirely.
    driver.click(SETTINGS_ICON);
    driver.click("Connect");
    assert_eq!(
        driver.debug_eval(READ_KEY).as_str(),
        Some(first),
        "connecting after a reload must reuse the stored identity, not replace it"
    );
}

fn topbar_markdown_button_toggles_render_mode<D: UiDriver>(driver: &mut D) {
    assert!(driver.exists(RENDER_MARKDOWN_ICON));
    driver.click(RENDER_MARKDOWN_ICON);
    assert!(driver.exists(SOURCE_MARKDOWN_ICON));
    driver.click(SOURCE_MARKDOWN_ICON);
    assert!(driver.exists(RENDER_MARKDOWN_ICON));
}

#[test]
fn topbar_markdown_button_toggles_render_mode_native() {
    let mut state = test_state();
    let mut driver =
        mae::testkit::NativeDriver::new(900.0, 600.0, move |ui| render(ui, &mut state));
    topbar_markdown_button_toggles_render_mode(&mut driver);
}

/// Clicking the *same* toolbar button again takes effect again.
///
/// Root-caused and fixed on the DOM backend, which used to drop the second
/// click (a third one then worked): it was delivered and the state did
/// change, but nothing scheduled the frame that would have *rendered* the
/// change, so the toolbar kept showing the previous icon until an unrelated
/// event drove another frame. See `signal_from_key_and_flags`'s DOM branch in
/// `mae`'s `imui/input.rs`.
///
/// The theme button, because it is a toolbar toggle the web build has (the
/// markdown one, which this used to use, is native-only now — see
/// `the_web_build_has_no_rendered_markdown_mode`). Which icon comes first
/// depends on the starting theme, so the scenario reads that rather than
/// assuming it.
fn a_toolbar_toggle_can_be_clicked_twice_in_a_row<D: UiDriver>(driver: &mut D) {
    let (first, second) = if driver.exists(DARK_THEME_ICON) {
        (DARK_THEME_ICON, LIGHT_THEME_ICON)
    } else {
        (LIGHT_THEME_ICON, DARK_THEME_ICON)
    };
    driver.click(first);
    assert!(
        driver.exists(second),
        "the first click should have switched the theme"
    );
    driver.click(second);
    assert!(
        driver.exists(first),
        "the second click on the same button should switch it back"
    );
}
crate::driver_test!(
    a_toolbar_toggle_can_be_clicked_twice_in_a_row,
    900.0,
    600.0,
    EnkrState::with_notes(NoteDatabase::demo())
);

#[test]
fn topbar_title_edits_rename_the_note_and_do_not_snap_back() {
    use mae::os::OSKeyCode;

    let mut harness = UiHarness::new(900.0, 600.0);
    let mut state = test_state();
    let id = state.active_note_id.clone();
    assert_eq!(state.notes.note(&id).unwrap().title(), "Welcome");

    let snap = harness.frame(|ui| render(ui, &mut state));
    // The editable title field is the line edit in the 56px top bar.
    let field = snap
        .nodes
        .iter()
        .find(|node| node.flags.contains(UIBoxFlags::LINE_EDIT) && node.bounds.y0 < 56.0)
        .expect("editable title field in the top bar");
    // Click near the right edge so the caret lands at the end of the title.
    harness.click_at(field.bounds.x1 - 2.0, field.center().y());
    harness.frame(|ui| render(ui, &mut state));

    // Erase the trailing character.
    harness.key_press(OSKeyCode::KeyBackspace);
    harness.frame(|ui| render(ui, &mut state));
    assert_eq!(state.notes.note(&id).unwrap().title(), "Welcom");
    // The title is the source of truth for the file name, so it's renamed too.
    assert_eq!(state.notes.note(&id).unwrap().file_path(), "Welcom.md");

    // Regression: the buffer used to be reseeded from the note every frame, so
    // an idle frame brought the erased character back. It must stay erased.
    harness.frame(|ui| render(ui, &mut state));
    assert_eq!(state.notes.note(&id).unwrap().title(), "Welcom");
}

#[test]
fn topbar_title_shows_and_renames_the_viewed_image() {
    use mae::os::OSKeyCode;

    let mut harness = UiHarness::new(900.0, 600.0);
    let mut state = test_state();
    let space = state.active_space_id;
    let id = state
        .notes
        .create_blob_in(space, "photo.png", ImageMime::Png, tiny_png());
    let name = state.notes.blob(&id).unwrap().name.clone();
    state.view = View::Image(name);

    // The top label now shows the image filename.
    let snap = harness.frame(|ui| render(ui, &mut state));
    let field = snap
        .nodes
        .iter()
        .find(|node| node.flags.contains(UIBoxFlags::LINE_EDIT) && node.bounds.y0 < 56.0)
        .expect("editable filename field in the top bar");

    // Click at the end and erase a character → renames the blob.
    harness.click_at(field.bounds.x1 - 2.0, field.center().y());
    harness.frame(|ui| render(ui, &mut state));
    harness.key_press(OSKeyCode::KeyBackspace);
    harness.frame(|ui| render(ui, &mut state));
    assert_eq!(state.notes.blob(&id).unwrap().name, "photo.pn");
    // The view follows the rename, and stays put on an idle frame.
    assert_eq!(state.view.image(), Some("photo.pn"));
    harness.frame(|ui| render(ui, &mut state));
    assert_eq!(state.notes.blob(&id).unwrap().name, "photo.pn");
}

fn settings_window_does_not_contain_markdown_render_toggle<D: UiDriver>(driver: &mut D) {
    driver.click(SETTINGS_ICON);
    driver.click("Editor");
    assert!(driver.exists("Wrap long lines"));
    assert!(!driver.exists("Render markdown"));
}
crate::driver_test!(
    settings_window_does_not_contain_markdown_render_toggle,
    900.0,
    600.0,
    test_state()
);

#[test]
fn settings_detail_pane_scrolls_on_small_viewports() {
    // The draggable window is gone — a view cannot be moved, and does not need
    // to be. What still has to hold is that a cramped viewport scrolls the
    // detail pane rather than overflowing it.
    let mut harness = UiHarness::new(400.0, 260.0);
    let mut state = test_state();
    state.open_settings(SettingsSection::Sync);

    let snap = harness.frame(|ui| render(ui, &mut state));
    assert!(
        snap.nodes.iter().any(|node| node.scroll_max.y() > 0.0),
        "settings detail pane should scroll instead of overflowing"
    );
    // The category rail stays reachable at this size.
    assert!(snap.try_node("###enkr_settings_cat_General").is_some());
}

#[test]
fn settings_back_button_shows_hover_tooltip() {
    // The close cross became a back arrow when Settings became a view; the
    // tooltip behaviour it was guarding is what matters, not the glyph.
    let mut harness = UiHarness::new(900.0, 600.0);
    let mut state = test_state();
    state.open_settings(SettingsSection::General);

    let snap = harness.frame(|ui| render(ui, &mut state));
    let back = snap.node("###enkr_settings_back");
    let center = back.center();

    harness.mouse_move(center.x(), center.y());
    harness.frame(|ui| render(ui, &mut state));
    let snap = harness.frame(|ui| render(ui, &mut state));

    assert!(
        snap.nodes.iter().any(|node| {
            node.text.as_deref() == Some("Back")
                && !node.flags.contains(UIBoxFlags::MOUSE_CLICKABLE)
        }),
        "hovering the back button should build the Back tooltip"
    );
}

#[test]
fn settings_toggle_reports_clicked() {
    let mut harness = UiHarness::new(400.0, 160.0);

    harness.frame(|ui| {
        settings_toggle(ui, "test_setting", "Test", true);
    });
    harness.click("On");
    let snap = harness.frame(|ui: &mut IMUI| {
        ui.set_markdown_mode(MarkdownMode::Source);
        settings_toggle(ui, "test_setting", "Test", true);
    });

    assert!(snap.node("On").signal.clicked());
}

fn assert_color_eq(actual: Color, expected: Color) {
    let epsilon = 0.001;
    assert!((actual.r - expected.r).abs() <= epsilon);
    assert!((actual.g - expected.g).abs() <= epsilon);
    assert!((actual.b - expected.b).abs() <= epsilon);
    assert!((actual.a - expected.a).abs() <= epsilon);
}

fn note_folder(state: &EnkrState, note: &str) -> Option<super::Uuid> {
    state
        .notes
        .summaries()
        .into_iter()
        .find(|s| s.id == note)
        .and_then(|s| s.folder)
}

fn note_space(state: &EnkrState, note: &str) -> i64 {
    state
        .notes
        .summaries()
        .into_iter()
        .find(|s| s.id == note)
        .map(|s| s.space_id)
        .expect("note summary")
}

#[test]
fn drop_note_on_folder_moves_it_into_the_folder() {
    let mut state = test_state();
    let space = state.notes.default_space_id();
    let folder = state.notes.create_folder(space, "Folder").unwrap();
    let note = state.notes.create_note_in(space);

    state.apply_drop(DragItem::Note(note.clone()), DropTarget::Folder(folder));

    assert_eq!(note_folder(&state, &note), Some(folder));
}

#[test]
fn drop_note_on_another_space_moves_it_there() {
    let mut state = test_state();
    let src = state.notes.default_space_id();
    let dst = state.notes.create_space_named("Other");
    let note = state.notes.create_note_in(src);

    state.apply_drop(DragItem::Note(note.clone()), DropTarget::Space(dst));

    assert_eq!(note_space(&state, &note), dst);
}

#[test]
fn drop_note_on_its_own_space_returns_it_to_the_root() {
    let mut state = test_state();
    let space = state.notes.default_space_id();
    let folder = state.notes.create_folder(space, "Folder").unwrap();
    let note = state.notes.create_note_in(space);
    state.notes.set_note_folder(&note, Some(folder));

    state.apply_drop(DragItem::Note(note.clone()), DropTarget::Space(space));

    assert_eq!(note_folder(&state, &note), None);
    assert_eq!(note_space(&state, &note), space);
}

#[test]
fn drop_folder_on_folder_reparents_it() {
    let mut state = test_state();
    let space = state.notes.default_space_id();
    let parent = state.notes.create_folder(space, "Parent").unwrap();
    let child = state.notes.create_folder(space, "Child").unwrap();

    state.apply_drop(DragItem::Folder(child), DropTarget::Folder(parent));

    assert_eq!(state.notes.folder(&child).unwrap().parent, Some(parent));
}

#[test]
fn dropping_a_folder_into_its_own_subtree_is_rejected() {
    let mut state = test_state();
    let space = state.notes.default_space_id();
    let parent = state.notes.create_folder(space, "Parent").unwrap();
    let child = state.notes.create_folder(space, "Child").unwrap();
    state.notes.set_folder_parent(&child, Some(parent));

    // Parent can't become a child of its own descendant.
    state.apply_drop(DragItem::Folder(parent), DropTarget::Folder(child));

    assert_eq!(state.notes.folder(&parent).unwrap().parent, None);
}

#[test]
fn dragging_a_note_onto_a_folder_moves_it_in() {
    let mut harness = UiHarness::new(900.0, 600.0);
    let mut state = test_state();
    let space = state.notes.default_space_id();
    let folder = state.notes.create_folder(space, "Target").unwrap();
    let note = state.notes.create_note_in(space);
    state.notes.set_note_title(&note, "DragMe");
    state.active_space_id = space;

    // Lay out the sidebar so the row positions are known.
    let snap = harness.frame(|ui| {
        state.set_repaint_waker(ui.repaint_waker());
        render(ui, &mut state);
    });
    let note_pos = snap.node("DragMe").center();
    let folder_pos = snap.node("Target").center();

    // Press on the note, then drag the cursor over the folder. Drag/hover
    // signals lag one frame, so pump a couple of frames mid-drag.
    harness.mouse_move(note_pos.x(), note_pos.y());
    harness.mouse_down(OSKey::LeftMouseButton, note_pos.x(), note_pos.y());
    harness.frame(|ui| render(ui, &mut state));
    harness.mouse_move(folder_pos.x(), folder_pos.y());
    harness.frame(|ui| render(ui, &mut state));
    harness.frame(|ui| render(ui, &mut state));
    assert!(state.drag.is_some(), "a drag should be in progress");

    // Release over the folder to commit the move.
    harness.mouse_up(OSKey::LeftMouseButton, folder_pos.x(), folder_pos.y());
    harness.frame(|ui| render(ui, &mut state));

    assert!(state.drag.is_none(), "drag cleared after release");
    assert_eq!(
        note_folder(&state, &note),
        Some(folder),
        "note moved into the folder it was dropped on"
    );
}

/// A fresh install — one that has never been through onboarding — opens on the
/// welcome screen rather than dropping straight into an editor.
#[test]
fn welcome_shows_on_first_launch_and_start_offline_persists() {
    let mut harness = UiHarness::new(900.0, 700.0);
    let mut notes = NoteDatabase::new_in_memory();
    // `new_in_memory` marks itself onboarded (it is a fixture, never a real
    // first install), so clear the flag to model one.
    notes.meta_set("onboarded", "");
    let mut state = EnkrState::with_notes(notes);
    assert_eq!(state.view, View::Welcome);

    let snap = harness.frame(|ui| render(ui, &mut state));
    assert!(snap.try_node("###enkr_welcome_offline").is_some());
    // The chrome is hidden: a full-window view owns the body outright.
    assert!(snap.try_node("###enkr_space_switcher").is_none());

    harness.click("###enkr_welcome_offline");
    harness.frame(|ui| render(ui, &mut state));
    assert_eq!(state.view, View::Editor);
    assert!(
        state.notes.meta_get("onboarded").is_some(),
        "the choice should survive a restart"
    );

    // And it does not come back on the next launch.
    let reopened = EnkrState::with_notes(NoteDatabase::new_in_memory());
    assert_eq!(reopened.view, View::Editor);
}

/// The welcome screen names the server it will actually use, and offers the
/// custom field as an override rather than a requirement.
///
/// The field used to be empty with no indication that connecting would do
/// anything sensible — it does, because `connect_from_welcome` falls back to
/// `active_server`, which is `DEFAULT_SERVER` on a fresh install. This makes
/// that visible.
#[test]
fn welcome_shows_the_server_it_will_use_and_hints_the_override() {
    let mut harness = UiHarness::new(900.0, 700.0);
    let mut notes = NoteDatabase::new_in_memory();
    notes.meta_set("onboarded", "");
    let mut state = EnkrState::with_notes(notes);
    assert_eq!(state.active_server, crate::app::state::DEFAULT_SERVER);

    // Sync lives behind the Online tab now.
    harness.frame(|ui| render(ui, &mut state));
    harness.click("###enkr_welcome_tab_online");
    let snap = harness.frame(|ui| render(ui, &mut state));
    assert!(
        snap.try_node(crate::app::state::DEFAULT_SERVER).is_some(),
        "the server that will be used should be named on screen"
    );

    // Empty means "use the one above", so the hint stands in for the value
    // without becoming it.
    let field = snap.node("###enkr_welcome_server");
    assert_eq!(
        field.text.as_deref(),
        Some("Or paste another server's URL"),
        "an empty server field should show its placeholder"
    );
    assert!(
        state.add_server_input.is_empty(),
        "the placeholder must not leak into the buffer"
    );

    // Typing replaces the hint rather than appending to it.
    harness.click("###enkr_welcome_server");
    harness.frame(|ui| render(ui, &mut state));
    harness.type_text("ws://example.test/ws");
    let snap = harness.frame(|ui| render(ui, &mut state));
    assert_eq!(state.add_server_input, "ws://example.test/ws");
    assert_eq!(
        snap.node("###enkr_welcome_server").text.as_deref(),
        Some("ws://example.test/ws")
    );
}

/// The three first-run answers are named side by side, and only the chosen one
/// is shown.
///
/// They used to be stacked down one page, which read as a checklist — people
/// scrolled past "start offline" looking for the rest of the setup. Naming all
/// three at once makes it a choice; showing one body at a time keeps it short.
#[test]
fn welcome_offers_the_three_answers_side_by_side() {
    let mut harness = UiHarness::new(900.0, 700.0);
    let mut notes = NoteDatabase::new_in_memory();
    notes.meta_set("onboarded", "");
    let mut state = EnkrState::with_notes(notes);

    let snap = harness.frame(|ui| render(ui, &mut state));
    // All three are offered...
    let tabs = [
        "###enkr_welcome_tab_offline",
        "###enkr_welcome_tab_online",
        "###enkr_welcome_tab_import",
    ];
    for tab in tabs {
        assert!(snap.try_node(tab).is_some(), "missing tab {tab}");
    }
    // ...on one row, so they read as alternatives rather than steps.
    let row = snap.node("###enkr_welcome_picker").bounds;
    for tab in tabs {
        let bounds = snap.node(tab).bounds;
        assert!(
            bounds.y0 >= row.y0 - 1.0 && bounds.y1 <= row.y1 + 1.0,
            "{tab} is not on the picker row"
        );
    }

    // Offline is the default answer, and its body is the only one present.
    assert!(snap.try_node("###enkr_welcome_offline").is_some());
    assert!(snap.try_node("###enkr_welcome_connect").is_none());
    assert!(snap.try_node("###enkr_welcome_import").is_none());

    harness.click("###enkr_welcome_tab_online");
    let snap = harness.frame(|ui| render(ui, &mut state));
    assert!(snap.try_node("###enkr_welcome_connect").is_some());
    assert!(snap.try_node("###enkr_welcome_offline").is_none());

    harness.click("###enkr_welcome_tab_import");
    let snap = harness.frame(|ui| render(ui, &mut state));
    assert!(snap.try_node("###enkr_welcome_import").is_some());
    assert!(snap.try_node("###enkr_welcome_connect").is_none());
}

/// Switching panels fades the new one in rather than snapping to it, and the
/// fade finishes — an animation that stalls part-way would leave the body
/// permanently dimmed, since the app only repaints on demand.
#[test]
fn welcome_panel_fades_in_after_a_tab_change() {
    let mut harness = UiHarness::new(900.0, 700.0);
    let mut notes = NoteDatabase::new_in_memory();
    notes.meta_set("onboarded", "");
    let mut state = EnkrState::with_notes(notes);
    harness.frame(|ui| render(ui, &mut state));

    harness.click("###enkr_welcome_tab_online");
    harness.frame(|ui| render(ui, &mut state));
    assert!(
        state.welcome_fade < 1.0,
        "the new panel should start transparent, was {}",
        state.welcome_fade
    );

    for _ in 0..120 {
        if state.welcome_fade >= 1.0 {
            break;
        }
        harness.frame(|ui| render(ui, &mut state));
    }
    assert_eq!(
        state.welcome_fade, 1.0,
        "the fade never completed; the panel would stay dimmed"
    );
}

/// The card is horizontally centred and anchored to the top, and scrolls when
/// it does not fit.
///
/// Top-anchored rather than vertically centred: centring made the card's
/// position a function of its height, so switching tabs slid the title and the
/// picker under the pointer. See `welcome_header_holds_still_across_tabs`.
#[test]
fn welcome_card_is_top_anchored_and_scrolls_when_it_does_not_fit() {
    let measure = |h: f32| {
        let mut harness = UiHarness::new(900.0, h);
        let mut notes = NoteDatabase::new_in_memory();
        notes.meta_set("onboarded", ""); // empty unsets
        let mut state = EnkrState::with_notes(notes);
        let snap = harness.frame(|ui| render(ui, &mut state));
        let card = snap.node("###enkr_welcome_column").bounds;
        let root = snap.node("###enkr_welcome").bounds;
        let scrolls = snap.node("###enkr_welcome").scroll_max.y() > 0.0;
        (card, root, scrolls)
    };

    // Roomy window: pinned to the top, horizontally centred.
    let (card, root, scrolls) = measure(900.0);
    assert!(!scrolls, "should not need to scroll in a tall window");
    assert!(
        card.y0 <= root.y0 + 1.0,
        "card should start at the top, was {} below it",
        card.y0 - root.y0
    );
    let left = card.x0 - root.x0;
    let right = root.x1 - card.x1;
    assert!(
        (left - right).abs() < SCROLLBAR_GUTTER + 2.0,
        "card should be horizontally centred: {left} left vs {right} right"
    );

    // Cramped window: scrolls, still from the top.
    let (card, root, scrolls) = measure(300.0);
    assert!(scrolls, "a short window should scroll the welcome screen");
    assert!(
        card.y0 <= root.y0 + 1.0,
        "scrolled content should start at the top"
    );
}

/// The title and the picker do not move when the body behind them changes size.
///
/// This is the regression the top anchoring exists for: Offline's body is short
/// and Online's is tall, so with a vertically centred card the picker slid out
/// from under the pointer between one click and the next.
#[test]
fn welcome_header_holds_still_across_tabs() {
    let mut harness = UiHarness::new(900.0, 900.0);
    let mut notes = NoteDatabase::new_in_memory();
    notes.meta_set("onboarded", "");
    let mut state = EnkrState::with_notes(notes);

    let snap = harness.frame(|ui| render(ui, &mut state));
    let picker_before = snap.node("###enkr_welcome_picker").bounds;
    let card_before = snap.node("###enkr_welcome_column").bounds;

    // Online's body is the tallest, so if anything moves it moves here.
    harness.click("###enkr_welcome_tab_online");
    let snap = harness.frame(|ui| render(ui, &mut state));
    let picker_after = snap.node("###enkr_welcome_picker").bounds;
    let card_after = snap.node("###enkr_welcome_column").bounds;
    assert!(
        snap.node("###enkr_welcome_column").bounds.height() > card_before.height() + 1.0,
        "precondition: the Online body should be taller, so this test can fail"
    );
    assert!(
        (picker_after.y0 - picker_before.y0).abs() < 1.0,
        "the picker moved {} px when the body grew",
        picker_after.y0 - picker_before.y0
    );
    assert!(
        (card_after.y0 - card_before.y0).abs() < 1.0,
        "the card top moved when the body grew"
    );
}

/// A view change fades in rather than cutting, and — just as importantly —
/// finishes. A fade that never settles would hold `request_repaint` on forever
/// and keep the app at full frame rate while idle.
#[test]
fn changing_view_fades_in_and_then_settles() {
    let mut harness = UiHarness::new(900.0, 600.0);
    let mut state = test_state();

    // Steady state: fully opaque, nothing animating.
    let snap = harness.frame(|ui| render(ui, &mut state));
    assert_eq!(snap.node("###enkr_sidebar").opacity, 1.0);

    state.open_settings(SettingsSection::General);
    let snap = harness.frame(|ui| render(ui, &mut state));
    let first = snap.node("###enkr_settings_view").opacity;
    assert!(
        first > 0.0 && first < 1.0,
        "the new view should arrive part-faded, got {first}"
    );

    // It converges, and does so quickly enough to feel immediate.
    let mut opacity = first;
    for _ in 0..120 {
        let snap = harness.frame(|ui| render(ui, &mut state));
        let next = snap.node("###enkr_settings_view").opacity;
        assert!(next >= opacity - 0.001, "fade should not go backwards");
        opacity = next;
        if opacity >= 1.0 {
            break;
        }
    }
    assert_eq!(opacity, 1.0, "fade should settle exactly at 1.0");
}

/// The move-to palette spans every space and names destinations by full path.
/// The old hover submenus listed bare folder names within one space, so two
/// folders called the same thing were indistinguishable and a folder in
/// another space was unreachable in a single action.
#[test]
fn move_to_palette_lists_destinations_across_spaces_by_path() {
    // `demo()` has several spaces; `test_state()` has one, and cross-space
    // destinations are the whole point here.
    let mut state = EnkrState::with_notes(NoteDatabase::demo());
    let work = state
        .notes
        .spaces()
        .iter()
        .find(|s| s.name == "Work")
        .expect("demo has a Work space")
        .id;
    let projects = state.notes.create_folder(work, "Projects").unwrap();
    state.notes.create_folder_in(work, Some(projects), "Q3");
    let note = state.notes.create_note();

    state.open_move_to(MoveSubject::Note(note.clone()));
    let rows = &state.search.as_ref().unwrap().rows;
    let titles: Vec<&str> = rows.iter().map(|r| r.title.as_str()).collect();

    // Nested folders read as paths, and other spaces are reachable.
    assert!(titles.contains(&"Work / Projects / Q3"), "got {titles:?}");
    assert!(titles.contains(&"Work"), "space roots are destinations too");
    // The note's current home is not offered — moving there is a no-op.
    assert!(
        !titles.contains(&"Space"),
        "the note's own space root should be filtered out: {titles:?}"
    );

    // A synced destination says so, because moving there changes the audience.
    let space_row = rows.iter().find(|r| r.title == "Work").unwrap();
    assert!(space_row.subtitle.contains("this installation"));
}

/// Filtering rebuilds the list, and picking a destination moves the subject.
#[test]
fn move_to_palette_filters_and_moves() {
    let mut harness = UiHarness::new(900.0, 600.0);
    let mut state = EnkrState::with_notes(NoteDatabase::demo());
    let work = state
        .notes
        .spaces()
        .iter()
        .find(|s| s.name == "Work")
        .unwrap()
        .id;
    let note = state.notes.create_note();
    state.open_move_to(MoveSubject::Note(note.clone()));

    state.search.as_mut().unwrap().query = "Work".to_string();
    harness.frame(|ui| render(ui, &mut state));
    let rows = &state.search.as_ref().unwrap().rows;
    assert!(
        rows.iter()
            .all(|r| r.title.contains("Work") || r.title.starts_with("New folder")),
        "query should filter to matching paths"
    );

    harness.click("###enkr_search_hit_0");
    harness.frame(|ui| render(ui, &mut state));
    assert!(
        state.search.is_none(),
        "picking a destination closes the palette"
    );
    assert_eq!(
        state.notes.note(&note).unwrap().space_id(),
        work,
        "the note should have moved"
    );
}

/// Arrow keys move the selection and Enter takes it — the palette is
/// keyboard-first, which it was not before (Enter only ever opened the *first*
/// result, and there was no visible cursor at all).
#[test]
fn palette_arrow_keys_move_the_selection_and_enter_takes_it() {
    let mut harness = UiHarness::new(900.0, 600.0);
    let mut state = test_state();
    state.open_space_switcher();
    harness.frame(|ui| render(ui, &mut state));

    let first = state.search.as_ref().unwrap().selected;
    harness.key_press(OSKeyCode::KeyDownArrow);
    harness.frame(|ui| render(ui, &mut state));
    let after_down = state.search.as_ref().unwrap().selected;
    assert_eq!(after_down, first + 1, "Down should advance the selection");

    harness.key_press(OSKeyCode::KeyUpArrow);
    harness.frame(|ui| render(ui, &mut state));
    assert_eq!(state.search.as_ref().unwrap().selected, first);

    // Enter takes whatever is selected.
    let target = match state.search.as_ref().unwrap().rows[first].action {
        PaletteAction::SwitchSpace(id) => id,
        _ => panic!("expected a space row"),
    };
    harness.key_press(OSKeyCode::KeyEnter);
    harness.frame(|ui| render(ui, &mut state));
    assert!(state.search.is_none(), "Enter closes the palette");
    assert_eq!(state.active_space_id, target);
}

/// Renaming happens in the row itself. Enter keeps the new name, Escape puts
/// the old one back — and neither needs a dialog.
#[test]
fn inline_rename_commits_on_enter_and_reverts_on_escape() {
    let mut harness = UiHarness::new(900.0, 600.0);
    let mut state = test_state();
    let space = state.active_space_id;
    let original = state.notes.space_name(space).unwrap().to_string();

    state.begin_rename(RenameTarget::Space(space));
    harness.frame(|ui| render(ui, &mut state)); // focuses the field
    harness.frame(|ui| render(ui, &mut state));
    harness.type_text("Renamed");
    harness.key_press(OSKeyCode::KeyEnter);
    harness.frame(|ui| render(ui, &mut state));
    assert_eq!(state.notes.space_name(space), Some("Renamed"));
    assert!(state.inline_edit.is_none(), "committing ends the edit");

    // Escape reverts, leaving the committed name alone.
    state.begin_rename(RenameTarget::Space(space));
    harness.frame(|ui| render(ui, &mut state));
    harness.frame(|ui| render(ui, &mut state));
    harness.type_text("Discarded");
    state.dismiss_top();
    harness.frame(|ui| render(ui, &mut state));
    assert_eq!(state.notes.space_name(space), Some("Renamed"));
    assert!(state.inline_edit.is_none());
    let _ = original;
}

/// The new name has to be *on screen* once Enter is pressed, not once
/// something else happens to wake the loop.
///
/// The window only redraws when something asks it to, and a rename is applied
/// during the build that reports the commit — after the row was already drawn
/// as an open edit field. Nothing used to ask for the frame that would show
/// the result, so the field sat there, still holding the typed name, until an
/// unrelated event (a mouse move, a sync notification) drove another frame.
#[test]
fn committing_a_rename_redraws_without_waiting_for_another_event() {
    let mut harness = UiHarness::new(900.0, 600.0);
    let mut state = test_state();
    let space = state.active_space_id;
    let folder = state.notes.create_folder(space, "Drafts").unwrap();

    state.begin_rename(RenameTarget::Folder(folder));
    harness.frame(|ui| render(ui, &mut state)); // focuses the field
    harness.frame(|ui| render(ui, &mut state)); // selects the existing name
    harness.type_text("Archive");

    // Let every animation the open field started settle, so the only thing
    // that can ask for a frame afterwards is the commit itself — the state a
    // field the user has been typing in for a moment is really in.
    for _ in 0..600 {
        harness.frame(|ui| render(ui, &mut state));
        if !harness.ui_mut().take_repaint_request() {
            break;
        }
    }
    assert!(
        !harness.ui_mut().take_repaint_request(),
        "an open rename field should not be asking for frames on its own"
    );

    harness.key_press(OSKeyCode::KeyEnter);
    harness.frame(|ui| render(ui, &mut state));
    assert_eq!(
        state.notes.folder(&folder).map(|f| f.name.clone()),
        Some("Archive".to_string()),
        "Enter commits the rename"
    );
    assert!(
        harness.ui_mut().take_repaint_request(),
        "the committing frame must ask for the frame that shows the new name"
    );

    // And that frame is the one showing it: the row is a plain label again.
    let snapshot = harness.frame(|ui| render(ui, &mut state));
    assert!(
        snapshot.try_node("###enkr_folder_rename_field").is_none(),
        "the edit field should be gone"
    );
    assert!(
        snapshot.try_node("Archive").is_some(),
        "the row should read as the new name"
    );
}

/// Clicking anywhere outside an open palette closes it — including on a
/// sidebar row, which is clickable in its own right.
///
/// The dismissal test reads the frame's presses rather than the event queue:
/// the queue is consumed, and a palette is built *after* the view it floats
/// over, so the row behind it took the press first and the palette stayed open
/// on screen no matter where you clicked.
#[test]
fn clicking_a_row_behind_an_open_palette_closes_it() {
    let mut harness = UiHarness::new(900.0, 600.0);
    let mut state = EnkrState::with_notes(NoteDatabase::demo());
    harness.frame(|ui| render(ui, &mut state));

    harness.click("###enkr_space_switcher");
    harness.frame(|ui| render(ui, &mut state));
    harness.frame(|ui| render(ui, &mut state));
    assert!(
        state.search.is_some(),
        "the switcher palette should be open"
    );

    // A note row in the sidebar, well clear of the palette.
    harness.click("Product roadmap");
    harness.frame(|ui| render(ui, &mut state));
    assert!(
        state.search.is_none(),
        "a click on the row behind the palette should close it"
    );
}

/// One click on a palette row is enough.
///
/// On the DOM backend it was two: pointer state is keyed by `UiKey` but the
/// node table by `DomKey`, so a removed node left its `left_pressed` behind,
/// and the next box built with that id re-armed the exclusive active key —
/// swallowing the following genuine click. Runs on both backends because only
/// the browser one could regress this way.
fn one_click_on_a_palette_row_switches_space<D: UiDriver>(driver: &mut D) {
    driver.click("###enkr_space_switcher");
    assert!(driver.exists("Work"), "the switcher palette should be open");
    // Exactly one click on the row — no retry.
    driver.click("###enkr_search_hit_1");
    assert!(
        !driver.exists("###enkr_search_input"),
        "picking a row should close the palette on the first click"
    );
    assert!(driver.exists("Work"), "and switch to that space");
}
crate::driver_test!(
    one_click_on_a_palette_row_switches_space,
    900.0,
    600.0,
    EnkrState::with_notes(NoteDatabase::demo())
);

/// Hover must not outlive the pointer. Two ways it used to:
/// a hovered box being removed (no `pointerleave` ever fires for it), and the
/// pointer simply moving to empty space (the DOM path only ever *set*
/// `hot_key`). Either stranded a highlight — and its tooltip — on screen.
#[test]
fn hover_clears_when_the_pointer_leaves_and_when_the_row_disappears() {
    let mut harness = UiHarness::new(900.0, 600.0);
    let mut state = EnkrState::with_notes(NoteDatabase::demo());
    harness.frame(|ui| render(ui, &mut state));

    let pill = harness.snapshot().node("###enkr_status_pill").center();
    harness.mouse_move(pill.x(), pill.y());
    harness.frame(|ui| render(ui, &mut state));
    assert!(
        harness
            .snapshot()
            .node("###enkr_status_pill")
            .signal
            .hovering(),
        "the pill should be hot under the pointer"
    );

    // Move to empty space: nothing should still be hot.
    harness.mouse_move(600.0, 560.0);
    harness.frame(|ui| render(ui, &mut state));
    assert!(
        !harness
            .snapshot()
            .node("###enkr_status_pill")
            .signal
            .hovering(),
        "hover should not outlive the pointer"
    );

    // Hover a row, then make it disappear underneath the pointer.
    let row = harness.snapshot().node("###enkr_space_switcher").center();
    harness.mouse_move(row.x(), row.y());
    harness.frame(|ui| render(ui, &mut state));
    state.open_settings(SettingsSection::General); // hides the whole sidebar
    harness.frame(|ui| render(ui, &mut state));
    harness.frame(|ui| render(ui, &mut state));
    assert!(
        harness
            .snapshot()
            .try_node("###enkr_space_switcher")
            .is_none(),
        "the switcher is gone with the sidebar"
    );
    // The category rows must be able to become hot, which a stranded
    // `hot_key` would prevent.
    let cat = harness
        .snapshot()
        .node("###enkr_settings_cat_Editor")
        .center();
    harness.mouse_move(cat.x(), cat.y());
    harness.frame(|ui| render(ui, &mut state));
    assert!(
        harness
            .snapshot()
            .node("###enkr_settings_cat_Editor")
            .signal
            .hovering(),
        "a new row must still be able to take hover after the old one vanished"
    );
}

/// A tooltip must not outlive the pointer that summoned it.
///
/// The settings gear is removed from the tree the instant it is clicked (the
/// Settings view replaces the sidebar), so no `pointerleave` ever reaches it.
/// Its `UiKey` is a hash of the widget id, so when the sidebar comes back the
/// rebuilt gear inherits that stale hover and re-raises its tooltip with the
/// pointer somewhere else entirely.
fn hover_tooltip_follows_the_pointer<D: UiDriver>(driver: &mut D) {
    driver.hover("###enkr_settings_button");
    assert!(
        driver.exists("Settings"),
        "hovering the gear shows its tooltip"
    );

    // Clicking removes the gear while the pointer is still on it.
    driver.click("###enkr_settings_button");
    // Move the pointer well away, onto the settings category rail.
    driver.hover("###enkr_settings_cat_Editor");
    // Back to the editor: the gear is rebuilt with the same id.
    driver.click("###enkr_settings_back");

    assert!(
        !driver.exists("Settings"),
        "the gear's tooltip should not reappear with the pointer elsewhere"
    );
}
crate::driver_test!(
    hover_tooltip_follows_the_pointer,
    900.0,
    600.0,
    EnkrState::with_notes(NoteDatabase::demo())
);

/// Switching Settings categories must not re-run the view's entrance.
///
/// The category rail goes through the same `set_view` as arriving from the
/// editor, so the fade restarted on every category click — the whole window
/// pulsing when you press "Editor", which is not what a tab does.
#[test]
fn switching_settings_categories_does_not_restart_the_fade() {
    let mut harness = UiHarness::new(900.0, 600.0);
    let mut state = test_state();
    harness.frame(|ui| render(ui, &mut state));

    // Arriving at Settings does animate.
    state.open_settings(SettingsSection::General);
    assert_eq!(state.view_fade, 0.0, "arriving fades in");
    for _ in 0..120 {
        harness.frame(|ui| render(ui, &mut state));
        if state.view_fade >= 1.0 {
            break;
        }
    }
    assert_eq!(state.view_fade, 1.0, "and settles");

    let depth = state.view_stack.len();
    harness.click("###enkr_settings_cat_Editor");
    harness.frame(|ui| render(ui, &mut state));
    assert_eq!(state.view, View::Settings(SettingsSection::Editor));
    assert_eq!(
        state.view_fade, 1.0,
        "a category switch is not an arrival — it must not re-fade"
    );
    assert_eq!(
        state.view_stack.len(),
        depth,
        "nor a navigation step: Escape should leave Settings, not step back a category"
    );
}

/// "New note" puts the caret in the title, and Enter hands it to the body.
///
/// A new note is nameless and naming it is the first thing anyone does, but the
/// title lives in the top bar — so it had to be found and clicked before it
/// could be typed, breaking the gesture in half. Focus starts there and moves
/// on once the name is settled, so "make a note called X and write in it" is
/// one uninterrupted run of keystrokes.
///
/// Run on the DOM backend too, because "focused" means something stronger
/// there and was not true at all: `focus_box` moved mae's own `focus_key`
/// (so the field drew its focus ring) while the browser's real focus stayed
/// on the "New note" `<button>` — where Enter and Space are a native
/// activation, so typing a name created more notes instead of naming this
/// one. See `paint_dom.rs`'s `sync_hosted_focus`.
fn creating_a_note_focuses_the_title_then_the_body<D: UiDriver>(driver: &mut D) {
    driver.click("###enkr_new_note_btn");
    // Focus is requested on one frame and applied on the next, with the
    // name selected on the one after that.
    for _ in 0..4 {
        if driver.focused_id().as_deref() == Some("###enkr_note_title") {
            break;
        }
        driver.settle();
    }
    assert_eq!(
        driver.focused_id().as_deref(),
        Some("###enkr_note_title"),
        "a new note should start with the caret in its title"
    );
    // The name is selected on the frame *after* focus lands, and typing
    // before that would append to the placeholder instead of replacing it.
    driver.settle();

    // Typing names the note without anything else being clicked — and
    // *replaces* the placeholder rather than appending to it, which is only
    // true if the selection reached the real field as well.
    //
    // Settled for rather than asserted outright: the browser applies the
    // keystrokes on its own schedule, and under a loaded machine (every
    // `::cdp` test in this file runs its own Chrome) the last one can land a
    // frame after `type_text` returns. The assertion is unchanged — this only
    // decides when to make it.
    driver.type_text("Groceries");
    for _ in 0..4 {
        if driver.text_of("###enkr_note_title").as_deref() == Some("Groceries") {
            break;
        }
        driver.settle();
    }
    assert_eq!(
        driver.text_of("###enkr_note_title").as_deref(),
        Some("Groceries"),
        "typing after New note should name it"
    );

    // Enter settles the name and moves to the body. The body's id carries
    // the note id, which a browser-driven scenario cannot read out of the
    // app — assert on the prefix, which is what "the caret is in the note
    // body" actually means.
    driver.key_press(OSKeyCode::KeyEnter);
    for _ in 0..4 {
        if focused_note_body(driver) {
            break;
        }
        driver.settle();
    }
    assert!(
        focused_note_body(driver),
        "Enter in the title should hand the caret to the note body, not leave it on {:?}",
        driver.focused_id()
    );
}
crate::driver_test!(
    creating_a_note_focuses_the_title_then_the_body,
    900.0,
    600.0,
    EnkrState::with_notes(NoteDatabase::demo())
);

/// Escape settles the name too, rather than leaving the caret stranded in the
/// title. It is the natural "I'm done naming, leave it as it is" key, and a
/// user who presses it should still land in the body.
///
/// Escape specifically is why the DOM backend stopped swallowing every key
/// that lands on a hosted `<input>`: it edits nothing there, so forwarding
/// it cannot double-apply, and swallowing it lost the app's own dismiss key
/// outright (see `os/wasm.rs`'s `key_owned_by_hosted_editor`).
fn escape_in_a_new_notes_title_moves_to_the_body<D: UiDriver>(driver: &mut D) {
    driver.click("###enkr_new_note_btn");
    for _ in 0..4 {
        if driver.focused_id().as_deref() == Some("###enkr_note_title") {
            break;
        }
        driver.settle();
    }
    // See the same wait in `creating_a_note_focuses_the_title_then_the_body`.
    driver.settle();
    driver.type_text("Notes");

    driver.key_press(OSKeyCode::KeyEscape);
    for _ in 0..4 {
        if focused_note_body(driver) {
            break;
        }
        driver.settle();
    }
    assert!(
        focused_note_body(driver),
        "Escape in the title should hand the caret to the note body, not leave it on {:?}",
        driver.focused_id()
    );
}
crate::driver_test!(
    escape_in_a_new_notes_title_moves_to_the_body,
    900.0,
    600.0,
    EnkrState::with_notes(NoteDatabase::demo())
);

/// Is the note body (`###enkr_editor_<note id>`) what currently has focus?
fn focused_note_body<D: UiDriver>(driver: &mut D) -> bool {
    driver
        .focused_id()
        .is_some_and(|id| id.starts_with("###enkr_editor_"))
}

/// Typing an emoji leaves the caret where the emoji ends, so the next
/// character lands after it.
///
/// Runs on the DOM backend for the reason it exists: a browser counts every
/// text offset it reports — `selectionStart` here — in UTF-16 code units,
/// while every offset in mae is a char index. An emoji is one char and *two*
/// code units, so the read-back caret came out one position further along per
/// emoji and mae then pushed that wrong caret back onto the real field, which
/// is what made typing on mobile look broken. The same read-back used the
/// UTF-16 number to slice the UTF-8 buffer, so any non-ASCII character at all
/// — an accent, not just an emoji — panicked the wasm module outright and the
/// note stopped accepting input from there on.
fn typing_an_emoji_keeps_the_caret_in_step<D: UiDriver>(driver: &mut D) {
    driver.click("###enkr_new_note_btn");
    for _ in 0..4 {
        if driver.focused_id().as_deref() == Some("###enkr_note_title") {
            break;
        }
        driver.settle();
    }
    // The name is selected on the frame after focus lands; typing before that
    // appends to the placeholder instead of replacing it.
    driver.settle();

    // An accent (2 UTF-8 bytes, 1 UTF-16 unit) and an emoji (4 bytes, 2
    // units) — the two ways a browser offset and a Rust offset disagree.
    driver.type_text("café\u{1F389}b");
    for _ in 0..6 {
        if driver.text_of("###enkr_note_title").as_deref() == Some("café\u{1F389}b") {
            break;
        }
        driver.settle();
    }
    assert_eq!(
        driver.text_of("###enkr_note_title").as_deref(),
        Some("café\u{1F389}b"),
        "every character typed should land, in order"
    );

    // Insert *between* the emoji and what follows it: the caret has to come
    // back from the field as the char index right after the emoji, not one
    // past it.
    driver.key_press(OSKeyCode::KeyLeftArrow);
    driver.type_text("X");
    for _ in 0..6 {
        if driver.text_of("###enkr_note_title").as_deref() == Some("café\u{1F389}Xb") {
            break;
        }
        driver.settle();
    }
    assert_eq!(
        driver.text_of("###enkr_note_title").as_deref(),
        Some("café\u{1F389}Xb"),
        "typing after an emoji should insert right after it"
    );
}
crate::driver_test!(
    typing_an_emoji_keeps_the_caret_in_step,
    900.0,
    600.0,
    EnkrState::with_notes(NoteDatabase::demo())
);

/// The space palette counts a space's notes from the store, not from the
/// render loop's per-frame buffer.
///
/// `summaries` is `mem::take`n during a frame and is empty outside one, so the
/// palette — which builds its rows when it *opens*, off the render path — read
/// every space as holding nothing and listed them all as "0 notes".
#[test]
fn the_space_list_counts_the_notes_a_space_actually_holds() {
    let mut state = test_state();
    let space = state.active_space_id;
    let before = state.notes.note_count_in_space(space);
    state.notes.create_note_in(space);
    state.notes.create_note_in(space);

    let rows = state.space_rows("");
    let row = rows
        .iter()
        .find(|row| matches!(row.action, PaletteAction::SwitchSpace(id) if id == space))
        .expect("the active space is listed");
    assert!(
        row.subtitle.starts_with(&format!("{} notes", before + 2)),
        "space listed as {:?} while holding {} notes",
        row.subtitle,
        before + 2
    );
}

/// Notes are held in the same order the database loads them in, so the sidebar
/// does not reshuffle across a restart.
///
/// New notes used to be appended, which meant the order during a session was
/// "whenever this arrived" and the order after a restart was
/// `(file_path, created, id)` — two different lists for the same notes, and two
/// clients syncing the same space disagreed as well.
#[test]
fn notes_are_kept_in_the_order_they_will_reload_in() {
    let mut db = NoteDatabase::new_in_memory();
    let space = db.default_space_id();
    // Created in an order that is deliberately not the sorted one.
    for name in ["zeta", "alpha", "middle"] {
        let id = db.create_note_in(space);
        db.set_note_title(&id, name);
    }

    let live: Vec<(String, String)> = db
        .summaries()
        .iter()
        .map(|s| (s.file_path.clone(), s.id.clone()))
        .collect();
    let mut canonical = live.clone();
    canonical.sort();
    assert_eq!(
        live, canonical,
        "the in-memory order differs from the order these notes reload in"
    );
}
