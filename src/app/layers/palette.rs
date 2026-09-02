//! Layer 2 — the command palette. Today it serves the three search scopes;
//! the redesign lifts this shell to also drive the space switcher and the
//! move-to destination picker, and adds a keyboard-selected row.

use crate::app::*;

/// Whether the search palette searches every note or only the active one.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum SearchScope {
    /// Ctrl+Shift+F: all notes, one hit per note, opens + jumps to the match.
    Global,
    /// Ctrl+F: the active note only, one hit per occurrence, jumps in place.
    Document,
    /// Ctrl+O ("go to note"): all notes, matches titles only, opens the note.
    Title,
}

/// What the open palette is for.
///
/// One shell, three jobs. Search streams its rows in from a worker; the other
/// two build theirs synchronously from a small candidate set (spaces, or
/// destinations) — small enough that a background scan would be pure overhead.
#[derive(Clone, PartialEq)]
pub(crate) enum PaletteKind {
    Search(SearchScope),
    /// Cmd+K: pick a space, or create/import one.
    SpaceSwitcher,
    /// Cmd+Shift+M: move a note, folder or image somewhere else.
    MoveTo(MoveSubject),
}

/// What a move-to palette is moving.
#[derive(Clone, PartialEq)]
pub(crate) enum MoveSubject {
    Note(String),
    Folder(Uuid),
    Blob(String),
}

/// What choosing a row does.
#[derive(Clone, PartialEq)]
pub(crate) enum PaletteAction {
    /// Open a note, optionally jumping the caret to a char offset.
    OpenNote {
        id: String,
        offset: usize,
        jump: bool,
    },
    SwitchSpace(i64),
    /// Move the subject into `space`, at `folder` (None = the space root).
    MoveTo {
        space: i64,
        folder: Option<Uuid>,
    },
    /// Create a folder named after the query and move the subject into it.
    CreateFolderAndMove {
        space: i64,
        name: String,
    },
    NewSpace,
    ImportFolder,
}

/// One row. Kind-agnostic: whatever built it has already decided what the two
/// lines say and what picking it does.
pub(crate) struct PaletteRow {
    pub(crate) title: String,
    /// The second line: a path, an excerpt, a note count, a bound server.
    pub(crate) subtitle: String,
    /// Byte ranges in `subtitle` to highlight (search excerpts).
    pub(crate) highlights: Vec<(usize, usize)>,
    pub(crate) indicator: SyncIndicator,
    pub(crate) action: PaletteAction,
}

impl PaletteRow {
    /// The note this row would open, if it opens one. Test-only: the app
    /// matches on the whole action rather than picking a field out of it.
    #[cfg(test)]
    pub(crate) fn note_id(&self) -> Option<&str> {
        match &self.action {
            PaletteAction::OpenNote { id, .. } => Some(id),
            _ => None,
        }
    }
}

/// State for the open palette.
pub(crate) struct PaletteState {
    pub(crate) kind: PaletteKind,
    pub(crate) query: String,
    /// Last query we acted on, to detect edits between frames.
    pub(crate) last_query: String,
    /// Keyboard cursor. Reset on every query change, clamped when rows shrink.
    pub(crate) selected: usize,
    /// Focus the input on the next frame (set on open / re-open).
    pub(crate) focus_pending: bool,
    /// False on the frame the palette opened.
    ///
    /// The space switcher and move-to are opened by a *left click*, and on that
    /// frame the opening press is still in the event queue while the pane has
    /// no painted rect to test the pointer against — so click-away would
    /// dismiss the palette instantly. Search never hit this because it is
    /// opened from the keyboard.
    pub(crate) armed: bool,
    pub(crate) rows: Vec<PaletteRow>,
    /// Only search has a worker behind it.
    pub(crate) search: Option<SearchRuntime>,
}

/// The background-scan half of a search palette.
pub(crate) struct SearchRuntime {
    pub(crate) engine: SearchEngine,
    /// Bumped on every query change so the worker's stale results are dropped.
    pub(crate) generation: u64,
    /// The current generation's scan is still running.
    pub(crate) searching: bool,
    /// We haven't yet received the first result of the current generation.
    /// While set, the previous query's results stay on screen so refining the
    /// query doesn't flash an empty "Searching…" frame between keystrokes.
    pub(crate) awaiting_first: bool,
}

impl PaletteState {
    pub(crate) fn scope(&self) -> Option<SearchScope> {
        match self.kind {
            PaletteKind::Search(scope) => Some(scope),
            _ => None,
        }
    }

    /// Placeholder shown before anything is typed.
    fn hint(&self) -> &'static str {
        match &self.kind {
            PaletteKind::Search(SearchScope::Document) => "Search in this note",
            PaletteKind::Search(SearchScope::Global) => "Search all your notes",
            PaletteKind::Search(SearchScope::Title) => "Go to note by title",
            PaletteKind::SpaceSwitcher => "Switch space",
            PaletteKind::MoveTo(_) => "Move to\u{2026}",
        }
    }

    /// Title above the input, for the palettes that act on something specific.
    fn caption(&self) -> Option<&'static str> {
        match &self.kind {
            PaletteKind::MoveTo(_) => Some("Move to"),
            PaletteKind::SpaceSwitcher => Some("Spaces"),
            PaletteKind::Search(_) => None,
        }
    }
}

pub(crate) const SEARCH_PALETTE_WIDTH: f32 = 560.0;
/// Hard cap on the results body so the palette never grows past the viewport;
/// beyond this the list scrolls.
pub(crate) const SEARCH_RESULTS_MAX_HEIGHT: f32 = 380.0;
/// Rough per-result row height (two text lines + padding), used only to decide
/// when the results list needs to scroll — mirrors [`context_submenu`].
pub(crate) const SEARCH_RESULT_ROW_EST: f32 = 48.0;
/// Cap on results retained/shown so a one-character query over a huge corpus
/// can't grow an unbounded list (and the palette stays responsive).
pub(crate) const SEARCH_RESULT_LIMIT: usize = 100;

/// Layer 2 — the command palette.
///
/// One shell for search, the space switcher and move-to: a focused input over a
/// list of rows, driven by mouse or keyboard. Rows are rebuilt only when the
/// query changes — never per frame — and search additionally streams them in
/// from a background worker.
pub(crate) fn render_search_palette(ui: &mut IMUI, state: &mut EnkrState, pal: &Colors) {
    let Some(mut palette) = state.search.take() else {
        return;
    };
    let pal = *pal;
    let theme = *ui.theme();

    // Drain whatever the worker streamed in since the last frame. The previous
    // query's results stay visible until the new scan yields its first update
    // (`awaiting_first`), so refining a query never flashes an empty frame.
    if let Some(runtime) = palette.search.as_mut() {
        let SearchRuntime {
            engine,
            generation,
            searching,
            awaiting_first,
        } = &mut *runtime;
        let rows = &mut palette.rows;
        let titles_only = palette.kind == PaletteKind::Search(SearchScope::Title);
        engine.poll(*generation, |update| match update {
            SearchUpdate::Hit(hit) => {
                if *awaiting_first {
                    rows.clear();
                    *awaiting_first = false;
                }
                if rows.len() < SEARCH_RESULT_LIMIT {
                    rows.push(search_row(hit, titles_only));
                }
            }
            SearchUpdate::Done => {
                // Scan finished with no hits: now it's safe to drop stale rows.
                if *awaiting_first {
                    rows.clear();
                    *awaiting_first = false;
                }
                *searching = false;
            }
        });
    }

    // A shrinking list must not strand the cursor past the end.
    if palette.selected >= palette.rows.len() {
        palette.selected = palette.rows.len().saturating_sub(1);
    }

    let (screen_w, screen_h) = ui.window_size();
    // Fits the viewport rather than assuming one wide enough for it: at 390px
    // a fixed 560 hung ~180px off the right edge, taking the results and the
    // scrollbar with it. Same clamp `modal_frame` applies for the same reason.
    let width = SEARCH_PALETTE_WIDTH.min(screen_w - WINDOW_MARGIN * 2.0);
    let pos = Point::new(
        ((screen_w - width) * 0.5).max(WINDOW_MARGIN),
        (screen_h * 0.12).max(WINDOW_MARGIN),
    );
    // Cap the results body to whatever fits below the input on this viewport.
    let results_max_h = (screen_h - pos.y() - 110.0).clamp(0.0, SEARCH_RESULTS_MAX_HEIGHT);
    let row_count = palette.rows.len().max(1);
    let natural_h = row_count as f32 * (SEARCH_RESULT_ROW_EST + 2.0) + theme.gap_sm;
    let scroll_results = natural_h > results_max_h;

    // Keyboard first: this is a palette, so the arrows move a selection and
    // Enter takes it. Handled before the build so the highlight lands on the
    // row the user just moved to rather than a frame later.
    let row_total = palette.rows.len();
    if row_total > 0 {
        if ui.input(OSKey::Keyboard(OSKeyCode::KeyDownArrow), None) {
            palette.selected = (palette.selected + 1).min(row_total - 1);
        }
        if ui.input(OSKey::Keyboard(OSKeyCode::KeyUpArrow), None) {
            palette.selected = palette.selected.saturating_sub(1);
        }
    }

    let searching = palette.search.as_ref().is_some_and(|r| r.searching);
    let hint = palette.hint();
    let caption = palette.caption();
    let selected_index = palette.selected;
    let mut close = false;
    let mut chosen: Option<PaletteAction> = None;
    let mut hovered: Option<usize> = None;
    let mut results_handle = None;

    let pane = ui.floating_pane_at(pos, Some("###enkr_search_palette"), |ui| {
        if let Some(caption) = caption {
            ui.label(caption)
                .width(ui, UISize::ParentPct(1.0))
                .text_color(ui, pal.text_muted)
                .font_size(ui, theme.size_text - 2.0);
        }

        // Query input with a leading search glyph.
        let mut input_handle = None;
        let input_row = ui.row(|ui| {
            ui.icon_label(SEARCH_ICON)
                .width(ui, UISize::Pixels(24.0))
                .font_size(ui, 18.0)
                .text_color(ui, pal.icon);
            let input = ui.line_edit("###enkr_search_input", &mut palette.query, false);
            input
                .width(ui, UISize::Fill)
                .height(ui, UISize::Pixels(40.0))
                .font_size(ui, theme.size_text + 2.0);
            input_handle = Some(input);
        });
        input_row
            .width(ui, UISize::ParentPct(1.0))
            .height(ui, UISize::Pixels(44.0))
            .align(ui, MainAxisAlign::Start, CrossAxisAlign::Center)
            .gap(ui, theme.gap_sm);
        // Autofocus the input on (re)open via its resolved handle (the box is
        // nested under the floating pane, so its key isn't the root-seeded one).
        if palette.focus_pending {
            if let Some(input) = input_handle {
                ui.focus_box(input);
            }
            palette.focus_pending = false;
        }

        let results_box = ui.named_column("###enkr_search_results", |ui| {
            if palette.rows.is_empty() {
                let empty = if palette.query.trim().is_empty() {
                    hint
                } else if searching {
                    "Searching\u{2026}"
                } else {
                    "No matches"
                };
                search_hint(ui, &pal, empty);
            } else {
                for (i, row) in palette.rows.iter().enumerate() {
                    let is_selected = i == selected_index;
                    let handle = ui.clickable_column(&format!("###enkr_search_hit_{i}"), |ui| {
                        let title = ui.named_row(&format!("###enkr_palette_title_{i}"), |ui| {
                            ui.label(&row.title)
                                .width(ui, UISize::Fill)
                                .text_color(ui, pal.text)
                                .font_size(ui, theme.size_text);
                            indicator_dot(ui, row.indicator);
                        });
                        // Definite line heights. Left to hug their content
                        // these two get shrunk to share whatever the column
                        // has, which collapsed both to ~8px — and keeps
                        // `SEARCH_RESULT_ROW_EST` (the scroll estimate) honest.
                        title
                            .width(ui, UISize::ParentPct(1.0))
                            .height(ui, UISize::Pixels(theme.size_text + 6.0))
                            .align(ui, MainAxisAlign::Start, CrossAxisAlign::Center)
                            .gap(ui, 6.0);
                        if !row.subtitle.is_empty() {
                            let mut highlight = pal.accent;
                            highlight.a = 0.28;
                            ui.label(&row.subtitle)
                                .width(ui, UISize::ParentPct(1.0))
                                .height(ui, UISize::Pixels(theme.size_text + 2.0))
                                .text_color(ui, pal.text_muted)
                                .font_size(ui, theme.size_text - 2.0)
                                .clip(ui, true)
                                .text_highlights(ui, row.highlights.clone(), highlight);
                        }
                    });
                    // Mouse and keyboard agree: hovering moves the selection, so
                    // there is never a second, competing highlight.
                    let bg = if is_selected {
                        pal.selected_bg
                    } else if handle.hover() {
                        pal.hover_bg
                    } else {
                        transparent_like(pal.hover_bg)
                    };
                    handle
                        .width(ui, UISize::ParentPct(1.0))
                        .padding(ui, 6.0, 8.0, 6.0, 8.0)
                        .gap(ui, 2.0)
                        .corner_radius(ui, theme.radius)
                        .background(ui, bg)
                        .cursor(ui, OSCursor::Hand);
                    if handle.hover() {
                        hovered = Some(i);
                    }
                    if handle.clicked() {
                        chosen = Some(row.action.clone());
                    }
                }
            }
        });
        let results_box = results_box
            .width(ui, UISize::ParentPct(1.0))
            .gap(ui, 2.0)
            .padding(ui, theme.gap_sm, SCROLLBAR_GUTTER, 0.0, 0.0);
        if scroll_results {
            results_box
                .height(ui, UISize::Pixels(results_max_h))
                .scroll_y(ui, true)
                .clip(ui, true);
        }
        results_handle = Some(results_box);
    });

    pane.width(ui, UISize::Pixels(width))
        .padding_all(ui, theme.pad_lg)
        .gap(ui, theme.gap_md)
        .background(ui, theme.popover_bg)
        .border_color(ui, theme.border)
        .corner_radius(ui, theme.radius);

    if let Some(index) = hovered {
        palette.selected = index;
    }
    // Keep the keyboard cursor on screen once the list scrolls.
    if scroll_results && let Some(list) = results_handle {
        let row_h = SEARCH_RESULT_ROW_EST + 2.0;
        ui.scroll_to_y(
            list,
            (palette.selected as f32 * row_h - results_max_h * 0.5 + row_h * 0.5).max(0.0),
        );
    }

    // Enter takes the selected row (which starts at the best match, so Enter
    // straight after typing behaves as it always did).
    if ui.input(OSKey::Keyboard(OSKeyCode::KeyEnter), None)
        && let Some(row) = palette.rows.get(palette.selected)
    {
        chosen = Some(row.action.clone());
    }
    if palette.armed && ui.press_outside(&[pane]) {
        close = true;
    }
    palette.armed = true;

    // A query edit rebuilds the list. For search that means superseding the
    // in-flight scan; rows are *not* cleared here — they linger until the new
    // scan's first result arrives (see the poll above) to avoid a flicker.
    if palette.query != palette.last_query {
        palette.last_query = palette.query.clone();
        palette.selected = 0;
        match palette.kind.clone() {
            PaletteKind::Search(scope) => {
                if let Some(runtime) = palette.search.as_mut() {
                    runtime.generation += 1;
                    if palette.query.trim().is_empty() {
                        palette.rows.clear();
                        runtime.searching = false;
                        runtime.awaiting_first = false;
                    } else {
                        runtime.searching = true;
                        runtime.awaiting_first = true;
                        runtime.engine.query(
                            runtime.generation,
                            palette.query.clone(),
                            scope == SearchScope::Document,
                        );
                    }
                }
            }
            PaletteKind::SpaceSwitcher => {
                palette.rows = state.space_rows(&palette.query);
            }
            PaletteKind::MoveTo(subject) => {
                palette.rows = state.move_destinations(&subject, &palette.query);
            }
        }
    }

    if let Some(action) = chosen {
        // Pass the kind explicitly: the palette has been taken out of `state`
        // for the duration of this build, so the action cannot look its own
        // subject back up from there.
        state.apply_palette_action(action, &palette.kind);
        close = true;
    }
    if !close {
        state.search = Some(palette);
    }
}

/// A search hit as a palette row. Title search matches titles, so its excerpt
/// would just repeat the path — the row shows the location instead.
fn search_row(hit: SearchHit, titles_only: bool) -> PaletteRow {
    let (subtitle, highlights) = if titles_only {
        (String::new(), Vec::new())
    } else {
        (hit.excerpt, hit.match_ranges)
    };
    PaletteRow {
        title: hit.full_name,
        subtitle,
        highlights,
        indicator: SyncIndicator::LocalOnly,
        action: PaletteAction::OpenNote {
            id: hit.note_id,
            offset: hit.offset,
            jump: !titles_only,
        },
    }
}

pub(crate) fn search_hint(ui: &mut IMUI, pal: &Colors, text: &str) {
    let theme = *ui.theme();
    ui.label(text)
        .width(ui, UISize::ParentPct(1.0))
        .padding(ui, 6.0, 8.0, 6.0, 8.0)
        .text_color(ui, pal.text_muted)
        .font_size(ui, theme.size_text - 1.0);
}
