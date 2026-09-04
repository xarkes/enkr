//! The Enkr note application: state, rendering, and sync UI. Lives in the
//! library (rather than `main.rs`) so the GUI test harness can drive the real
//! app through simulated clicks and keystrokes.

use crate::note::{Note, NoteDatabase, NoteSummary};
use crate::search::{SearchDoc, SearchEngine, SearchHit, SearchUpdate};
use crate::sync::app::{AppSync, NoticeLevel, Presence, SyncIndicator};
use crate::sync::{IdentityStore, MemberRole, SyncConfig};
use enkr_proto::wire::ImageMime;

use mae::imui::ImageResize;
// Native-only: the web build has no markdown-mode toggle to offer at all —
// see `chrome.rs`'s top-bar buttons.
#[cfg(not(target_arch = "wasm32"))]
use mae::imui::MarkdownMode;
use mae::{
    file_explorer::{FileExplorer, FileExplorerOutcome},
    imui::{
        Axis, Color, CrossAxisAlign, IMUI, MainAxisAlign, Padding, Point, RemoteCaret,
        RepaintWaker, TextAreaOptions, ThemeKind, ToastLevel, UIBoxHandle, UISize, UITheme,
    },
    os::{OSCursor, OSEventFlag, OSKey, OSKeyCode},
};
use std::path::PathBuf;
use uuid::Uuid;

mod chrome;
mod dnd;
mod layers;
mod paths;
mod sidebar;
mod state;
mod style;
mod views;

pub(crate) use chrome::*;
pub(crate) use dnd::*;
pub(crate) use layers::*;
pub(crate) use paths::*;
pub(crate) use sidebar::*;
pub(crate) use state::*;
pub use state::{EnkrState, META_RECOVERY_ACKED};
pub(crate) use style::*;
pub use style::{DARK_THEME_ICON, RENDER_MARKDOWN_ICON, SEARCH_ICON, SETTINGS_ICON};
pub(crate) use views::*;
const SPLITTER_WIDTH: f32 = 1.0;
const SPLITTER_HIT_PADDING_X: f32 = 5.0;
const SIDEBAR_MIN_WIDTH: f32 = 180.0;
const SIDEBAR_MAX_WIDTH: f32 = 520.0;
/// Below this window width the sidebar stops sharing the width with the
/// editor and becomes a drawer over it (see `render_drawer`).
///
/// Driven by the *viewport*, not by whether the device has a touch screen: a
/// desktop window dragged this narrow has exactly the same problem, and a
/// rule that can be reproduced by resizing a window is one that can be
/// tested without a phone.
const NARROW_WIDTH: f32 = 640.0;

/// Is the window too narrow to show the sidebar beside the content?
pub(crate) fn is_narrow(ui: &IMUI) -> bool {
    ui.window_size().0 < NARROW_WIDTH
}

pub fn render(ui: &mut IMUI, state: &mut EnkrState) {
    // Drive the framework accent (used by e.g. the inline-image resize grip)
    // from Enkr's own primary/accent color, so they stay in sync.
    let mut theme = UITheme::for_kind(state.theme_kind);
    let colors = Colors::for_kind(state.theme_kind);
    theme.accent = colors.accent;
    // The framework clears each frame to `app_bg`, so this is also what shows
    // through anything drawn below full opacity — a view fading in, say. The
    // stock theme's `app_bg` is deliberately transparent (the demo can sit on a
    // see-through window); Enkr is opaque, so give the framework the real
    // colour or the fade dissolves toward black.
    theme.app_bg = colors.app_bg;
    ui.set_theme(theme);
    // App shortcuts use the platform command modifier (⌘ on macOS, Ctrl else).
    let cmd = OSEventFlag::command();
    if ui.input(OSKey::Keyboard(OSKeyCode::KeyQ), Some(cmd)) {
        state.shutdown();
        ui.request_quit();
        return;
    }
    if ui.input(OSKey::Keyboard(OSKeyCode::KeyI), Some(cmd)) {
        state.open_import_picker();
    }
    if ui.input(OSKey::Keyboard(OSKeyCode::KeyE), Some(cmd)) {
        state.open_export_picker();
    }
    // Exact-flag matching so Cmd+F and Cmd+Shift+F don't both fire.
    if ui.input_exact(
        OSKey::Keyboard(OSKeyCode::KeyF),
        Some(cmd.with(OSEventFlag::Shift)),
    ) {
        state.open_search(SearchScope::Global);
    }
    if ui.input_exact(OSKey::Keyboard(OSKeyCode::KeyF), Some(cmd)) {
        state.open_search(SearchScope::Document);
    }
    // Cmd+P is "go to file" in every editor a person has used, so it aliases
    // Cmd+O rather than opening the space switcher.
    if ui.input(OSKey::Keyboard(OSKeyCode::KeyO), Some(cmd))
        || ui.input(OSKey::Keyboard(OSKeyCode::KeyP), Some(cmd))
    {
        state.open_search(SearchScope::Title);
    }
    if ui.input(OSKey::Keyboard(OSKeyCode::KeyK), Some(cmd)) {
        state.open_space_switcher();
    }
    if ui.input_exact(
        OSKey::Keyboard(OSKeyCode::KeyM),
        Some(cmd.with(OSEventFlag::Shift)),
    ) && !state.active_note_id.is_empty()
    {
        state.open_move_to(MoveSubject::Note(state.active_note_id.clone()));
    }
    if ui.input(OSKey::Keyboard(OSKeyCode::KeyEscape), None) {
        // Escape belongs to whatever is on top, so a menu or palette opened
        // while naming a new note still closes first. Only when there is
        // nothing to dismiss does it mean "the name is fine as it is" and hand
        // the caret to the body. This has to live here rather than beside the
        // title field: `ui.input` *consumes* the event, and this router runs
        // first, so a check in `chrome` would never see the key at all.
        if !state.dismiss_top() && state.new_note_focus == NewNoteFocus::Naming {
            state.new_note_focus = NewNoteFocus::Body;
        }
    }
    state.ensure_sync_started();
    // Event-driven sync pump: drains completed work; cheap no-op when idle.
    // While anything is in flight keep frames coming so debounces fire.
    // The caret guard pins the local caret/selection to its *logical*
    // position across whatever remote updates the pump applies.
    let caret_guard = capture_caret_guard(state, ui);
    if let Some(sync) = state.sync.as_mut() {
        sync.pump(&mut state.notes);
        for notice in sync.take_notices() {
            let level = match notice.level {
                NoticeLevel::Info => ToastLevel::Info,
                NoticeLevel::Warning => ToastLevel::Warning,
                NoticeLevel::Danger => ToastLevel::Danger,
            };
            ui.toast(level, notice.message);
        }
        if sync.has_pending() {
            ui.request_repaint();
        }
    }
    restore_caret_guard(state, ui, caret_guard);
    state.autosave_due(ui);
    state.ensure_active_note();
    // One summaries rebuild per frame, into the retained buffer, before anything
    // reads it. Previously every consumer built its own vector — and each one
    // materialized every note's Yrs body to derive the preview.
    let mut summaries = std::mem::take(&mut state.summaries);
    state.notes.summaries_into(&mut summaries);
    state.summaries = summaries;

    let pal = Colors::for_kind(state.theme_kind);

    // A full-window view owns the body outright: no chrome, no layers of its
    // own. Document views (editor, image) keep the sidebar and breadcrumb,
    // because you reach an image by clicking a sidebar row and need that row to
    // still be there afterwards.
    if !state.view.has_chrome() {
        let fade = state.tick_view_fade(ui);
        let view = match state.view.clone() {
            View::Settings(section) => settings_view(ui, state, &pal, section),
            View::Welcome => welcome_view(ui, state, &pal),
            View::Editor | View::Image(_) => unreachable!("has_chrome covers these"),
        };
        // Arriving at a destination fades it up rather than cutting to it. One
        // call on the root — opacity inherits down the whole subtree.
        view.opacity(ui, fade);
        render_menu(ui, state, &pal);
        render_file_explorer(ui, state);
        render_delete_space_confirm(ui, state);
        render_share_dialog(ui, state);
        render_recovery_dialog(ui, state);
        image_pump(ui, state);
        return;
    }

    let fade = state.tick_view_fade(ui);
    let root = ui.column(|ui| {
        #[cfg(target_arch = "wasm32")]
        web_download_banner(ui, &pal);

        let mut splitter_handle: Option<UIBoxHandle> = None;
        let narrow = is_narrow(ui);
        // The sidebar is a full-height column against the window edge, and the
        // top bar lives inside the content column beside it. It used to be the
        // other way round — a full-width bar with an empty spacer over the
        // sidebar — which left the sidebar starting 56px down, with a band of
        // background above it that belonged to nothing.
        //
        // Narrow viewports leave both the sidebar and the splitter out of this
        // row entirely: the sidebar is drawn as a drawer over the content
        // instead (`render_drawer`, in the layer list below), and a splitter
        // has nothing left to split.
        let body = ui.row(|ui| {
            if !narrow {
                // Clamped to the window, not just to `SIDEBAR_MAX_WIDTH`: the
                // width is a remembered drag, and dragging the *window* narrow
                // afterwards would otherwise leave a 520px sidebar over a
                // 600px window.
                let width = state
                    .side_width
                    .min((ui.window_size().0 - SIDEBAR_MIN_WIDTH).max(SIDEBAR_MIN_WIDTH));
                sidebar(ui, state, &pal, width);

                let splitter = ui.button("##enkr_splitter", Some("Drag to resize"));
                splitter_handle = Some(splitter);
                let splitter_color = if splitter.dragging() || splitter.hover() {
                    pal.accent
                } else {
                    pal.border
                };
                splitter
                    .width(ui, UISize::Pixels(SPLITTER_WIDTH))
                    .height(ui, UISize::ParentPct(1.0))
                    .padding_all(ui, 0.0)
                    .corner_radius(ui, 0.0)
                    .background(ui, splitter_color)
                    .border_color(ui, splitter_color)
                    .cursor(ui, OSCursor::ResizeH)
                    .hit_padding_x(ui, SPLITTER_HIT_PADDING_X);
            }

            let content = ui.named_column("###enkr_content_column", |ui| {
                top_bar(ui, state, &pal);
                content_panel(ui, state, &pal);
            });
            content
                .width(ui, UISize::Fill)
                .height(ui, UISize::Fill)
                .gap(ui, 0.0);
        });
        body.width(ui, UISize::ParentPct(1.0))
            .height(ui, UISize::Fill)
            .gap(ui, 0.0);

        if let Some(splitter) = splitter_handle {
            if splitter.pressed()
                && let (Some(press_pos), body_bounds) =
                    (splitter.signal().left_press_pos, ui.bounds(body))
            {
                let local_press_x = press_pos.x() - body_bounds.x0;
                let splitter_center_x = state.side_width + SPLITTER_WIDTH * 0.5;
                state.splitter_drag_offset = local_press_x - splitter_center_x;
            }

            if splitter.dragging() && ui.mouse_down() {
                if let (Some(mouse), body_bounds) = (ui.mouse_position(), ui.bounds(body)) {
                    let local_mouse_x = mouse.x() - body_bounds.x0;
                    let new_w = (local_mouse_x - state.splitter_drag_offset - SPLITTER_WIDTH * 0.5)
                        .clamp(SIDEBAR_MIN_WIDTH, SIDEBAR_MAX_WIDTH);
                    state.side_width = new_w;
                }
            } else {
                state.splitter_drag_offset = 0.0;
            }
        }
    });

    root.width(ui, UISize::ParentPct(1.0))
        .height(ui, UISize::Fill)
        .gap(ui, 0.0)
        .opacity(ui, fade)
        .background(ui, pal.app_bg);

    // Layers, in fixed paint order. Z-order is a property of this list, not of
    // where each surface happens to be opened from. The drawer is first
    // because it is the *lowest* of them: a menu opened from a sidebar row has
    // to paint above the drawer it was opened from.
    render_drawer(ui, state, &pal);
    render_menu(ui, state, &pal);
    render_search_palette(ui, state, &pal);
    render_delete_space_confirm(ui, state);
    render_file_explorer(ui, state);
    render_share_dialog(ui, state);
    render_recovery_dialog(ui, state);
    render_drag_ghost(ui, state, &pal);
    apply_pending_jump(ui, state);
    capture_active_cursor(ui, state);
    image_pump(ui, state);
}

#[cfg(test)]
mod tests;
