//! Presentation vocabulary shared by every view and layer: the colour set, the
//! button/toggle/heading primitives, the status indicators, and date
//! formatting. Nothing here reads or mutates application state.

use super::*;

pub(crate) const SPACE_ICON: &str = "\u{e865}";
pub(crate) const FOLDER_ICON: &str = "\u{e2c7}";
pub(crate) const FOLDER_OPEN_ICON: &str = "\u{e2c8}";
pub(crate) const LIGHT_THEME_ICON: &str = "\u{e518}";
pub const DARK_THEME_ICON: &str = "\u{e51c}";
pub(crate) const IMAGE_ICON: &str = "\u{e3f4}";
pub(crate) const WARNING_ICON: &str = "\u{e002}";
pub const SETTINGS_ICON: &str = "\u{e8b8}";
pub(crate) const CLOSE_ICON: &str = "\u{e5cd}";
pub const SEARCH_ICON: &str = "\u{e8b6}";
pub const RENDER_MARKDOWN_ICON: &str = "\u{e8f4}";
pub(crate) const SOURCE_MARKDOWN_ICON: &str = "\u{e86f}";
pub(crate) const MORE_ICON: &str = "\u{e5d4}";
/// `expand_more` — the space switcher's dropdown affordance.
pub(crate) const CHEVRON_ICON: &str = "\u{e5cf}";
/// `arrow_back` — leaving a full-window view.
pub(crate) const BACK_ICON: &str = "\u{e5c4}";
/// `menu` — opens the sidebar as a drawer, on viewports too narrow to keep
/// it on screen (see `render_drawer`).
pub(crate) const MENU_ICON: &str = "\u{e5d2}";

/// Right gutter reserved inside scrolling popover bodies so the overlay
/// scrollbar floats beside the controls instead of over the right-aligned
/// toggles/buttons. Covers the hover-expanded bar (8px) plus its 2px edge inset.
pub(crate) const SCROLLBAR_GUTTER: f32 = 10.0;

/// Stable palette for collaborator presence badges, indexed by
/// [`Presence::color_slot`].
pub(crate) const PRESENCE_COLORS: [&str; 6] = [
    "#e0529b", "#4f6ef7", "#34a853", "#f0a030", "#9b59d0", "#2bb3c0",
];

pub(crate) fn presence_color(slot: usize) -> Color {
    Color::new(PRESENCE_COLORS[slot % PRESENCE_COLORS.len()])
}

/// Flat color palette for the Enkr note app, tuned to match the design prototype.
/// Kept local to the app so the shared demo theme is unaffected.
#[derive(Clone, Copy)]
pub(crate) struct Colors {
    pub(crate) app_bg: Color,
    pub(crate) sidebar_bg: Color,
    pub(crate) content_bg: Color,
    pub(crate) border: Color,
    pub(crate) text: Color,
    pub(crate) text_muted: Color,
    pub(crate) text_faint: Color,
    pub(crate) accent: Color,
    pub(crate) accent_text: Color,
    pub(crate) selected_bg: Color,
    pub(crate) hover_bg: Color,
    pub(crate) icon: Color,
}

impl Colors {
    pub(crate) fn for_kind(kind: ThemeKind) -> Self {
        match kind {
            ThemeKind::Light => Self {
                app_bg: Color::new("#ffffff"),
                sidebar_bg: Color::new("#f7f8fa"),
                content_bg: Color::new("#ffffff"),
                border: Color::new("#ebecf0"),
                text: Color::new("#1f2329"),
                text_muted: Color::new("#6b7280"),
                text_faint: Color::new("#a6abb3"),
                accent: Color::new("#4f6ef7"),
                accent_text: Color::new("#ffffff"),
                selected_bg: Color::new("#eaeefe"),
                hover_bg: Color::new("#eef0f4"),
                icon: Color::new("#8b9099"),
            },
            ThemeKind::Dark => Self {
                app_bg: Color::new("#1e2127"),
                sidebar_bg: Color::new("#191c21"),
                content_bg: Color::new("#1e2127"),
                border: Color::new("#2c313a"),
                text: Color::new("#e6e8eb"),
                text_muted: Color::new("#9aa0aa"),
                text_faint: Color::new("#6b7280"),
                accent: Color::new("#5b7cfa"),
                accent_text: Color::new("#ffffff"),
                selected_bg: Color::new("#2a3142"),
                hover_bg: Color::new("#252a32"),
                icon: Color::new("#9aa0aa"),
            },
        }
    }
}

pub(crate) fn transparent_like(mut color: Color) -> Color {
    color.a = 0.0;
    color
}

/// Translucent accent wash painted behind a sidebar row that is the current
/// drag-and-drop target, so the drop destination reads clearly.
pub(crate) fn drop_target_bg(pal: &Colors) -> Color {
    let mut color = pal.accent;
    color.a = 0.22;
    color
}

pub(crate) fn section_header(ui: &mut IMUI, pal: &Colors, label: &str) {
    ui.label(label)
        .width(ui, UISize::ParentPct(1.0))
        .height(ui, UISize::Pixels(30.0))
        .padding_all(ui, 6.0)
        .text_color(ui, pal.text_faint)
        .font_size(ui, 11.0);
}

/// Sync indicator color, or None when no indicator should be drawn.
pub(crate) fn indicator_color(indicator: SyncIndicator) -> Option<Color> {
    match indicator {
        SyncIndicator::LocalOnly => None,
        SyncIndicator::Offline => Some(Color::new("#9aa0aa")),
        SyncIndicator::Synchronizing => Some(Color::new("#f0a030")),
        SyncIndicator::Synchronized => Some(Color::new("#34a853")),
        SyncIndicator::Errored => Some(Color::new("#e05252")),
    }
}

pub(crate) fn indicator_tooltip(indicator: SyncIndicator) -> &'static str {
    match indicator {
        SyncIndicator::LocalOnly => "Local only",
        SyncIndicator::Offline => "Offline",
        SyncIndicator::Synchronizing => "Synchronizing",
        SyncIndicator::Synchronized => "Synchronized",
        SyncIndicator::Errored => "Sync error",
    }
}

/// Small status dot for a space/note row.
pub(crate) fn indicator_dot(ui: &mut IMUI, indicator: SyncIndicator) {
    let Some(color) = indicator_color(indicator) else {
        return;
    };
    let _ = indicator_tooltip(indicator);
    ui.label("")
        .width(ui, UISize::Pixels(8.0))
        .height(ui, UISize::Pixels(8.0))
        .padding_all(ui, 0.0)
        .corner_radius(ui, 4.0)
        .background(ui, color);
}

/// Colored circles with collaborator initials (PLAN §6 client integration).
pub(crate) fn presence_badges(ui: &mut IMUI, presence: &[Presence]) {
    for p in presence.iter().take(4) {
        let initial: String = p
            .nickname
            .chars()
            .take(1)
            .collect::<String>()
            .to_uppercase();
        ui.label(&initial)
            .width(ui, UISize::Pixels(16.0))
            .height(ui, UISize::Pixels(16.0))
            .padding_all(ui, 0.0)
            .corner_radius(ui, 8.0)
            .font_size(ui, 10.0)
            // Center the glyph in the circle on both axes (labels default to
            // top-left placement; without this the initial sits in the corner).
            .text_center(ui, true)
            .background(ui, presence_color(p.color_slot()))
            .text_color(ui, Color::new("#ffffff"));
    }
}

pub(crate) const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// `"May 18, 2024"` from an ISO timestamp, for the top bar.
pub(crate) fn long_date(updated: &str) -> String {
    let Some((year, month, day)) = parse_iso_date(updated) else {
        return String::new();
    };
    let month_name = MONTHS.get((month - 1) as usize).copied().unwrap_or("");
    format!("{month_name} {day}, {year}")
}

/// `"May 18"` from an ISO timestamp, for the compact note list.
pub(crate) fn short_date(updated: &str) -> String {
    let Some((_, month, day)) = parse_iso_date(updated) else {
        return String::new();
    };
    let month_name = MONTHS.get((month - 1) as usize).copied().unwrap_or("");
    format!("{month_name} {day}")
}

/// Parse the `YYYY-MM-DD` prefix of an ISO timestamp into `(year, month, day)`.
pub(crate) fn parse_iso_date(updated: &str) -> Option<(i32, u32, u32)> {
    let date = updated.split('T').next()?;
    let mut parts = date.split('-');
    let year = parts.next()?.parse().ok()?;
    let month = parts.next()?.parse().ok()?;
    let day = parts.next()?.parse::<u32>().ok()?;
    Some((year, month, day))
}

/// A labelled on/off control. Returns true on the frame it is toggled.
/// Visual weight of an [`enkr_button`].
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum BtnVariant {
    /// Filled accent — the main action / selected state.
    Primary,
    /// Surface fill + border — the default.
    Secondary,
    /// Surface fill + red text — destructive actions.
    Danger,
}

/// The one Enkr button: a modern, consistently-styled pill with its label
/// centered on both axes. `id` is the usual `"Label###key"` string. Returns the
/// handle so callers can size it (e.g. `.width(ui, UISize::Fill)`) or read
/// `.clicked()`; height defaults to a comfortable 30px and can be overridden.
///
/// `Primary` uses the app palette's accent (same look as the "New note"
/// button); `Secondary`/`Danger` use the dialog chrome surfaces so they blend
/// with the popover they sit in.
pub(crate) fn enkr_button(
    ui: &mut IMUI,
    id: &str,
    tooltip: Option<&str>,
    variant: BtnVariant,
) -> UIBoxHandle {
    let theme = *ui.theme();
    let pal = Colors::for_kind(theme.kind);
    let (bg, fg, border) = match variant {
        BtnVariant::Primary => (pal.accent, pal.accent_text, pal.accent),
        BtnVariant::Secondary => (theme.surface_bg, theme.text, theme.border),
        BtnVariant::Danger => (theme.surface_bg, Color::new("#e05252"), theme.border),
    };
    ui.button(id, tooltip)
        .width(ui, UISize::TextContent(0.0))
        .height(ui, UISize::Pixels(30.0))
        .padding(ui, 0.0, 12.0, 0.0, 12.0)
        .corner_radius(ui, 8.0)
        .background(ui, bg)
        .border_color(ui, border)
        .text_color(ui, fg)
        .text_center(ui, true)
}

pub(crate) fn settings_toggle(ui: &mut IMUI, id: &str, label: &str, value: bool) -> bool {
    let theme = *ui.theme();
    let mut clicked = false;
    let row = ui.row(|ui| {
        ui.label(label)
            .width(ui, UISize::Fill)
            .height(ui, UISize::Pixels(28.0))
            .text_color(ui, theme.text);

        let (text, variant) = if value {
            ("On", BtnVariant::Primary)
        } else {
            ("Off", BtnVariant::Secondary)
        };
        let toggle = enkr_button(ui, &format!("{text}###{id}_toggle"), None, variant)
            .width(ui, UISize::Pixels(56.0))
            .height(ui, UISize::Pixels(26.0));
        clicked = toggle.clicked();
    });
    row.width(ui, UISize::ParentPct(1.0))
        .height(ui, UISize::Pixels(34.0))
        .align(ui, MainAxisAlign::Start, CrossAxisAlign::Center)
        .gap(ui, theme.gap_md);
    clicked
}

/// A non-interactive section header inside the settings window.
/// Human-readable byte count for quota and usage lines.
///
/// Deliberately coarse — one decimal at GB scale. This sits in a settings
/// detail line, where "3.2 GB" is the useful answer and "3,435,973,836 bytes"
/// is not.
pub(crate) fn format_bytes(bytes: i64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    const GB: f64 = MB * 1024.0;
    let n = bytes as f64;
    if n >= GB {
        format!("{:.1} GB", n / GB)
    } else if n >= MB {
        format!("{:.1} MB", n / MB)
    } else if n >= KB {
        format!("{:.0} KB", n / KB)
    } else {
        format!("{bytes} B")
    }
}

/// Wall clock in milliseconds, to compare against a server-supplied expiry.
pub(crate) fn now_ms() -> i64 {
    web_time::SystemTime::now()
        .duration_since(web_time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// A fixed vertical gap. `gap()` on the parent is uniform, so a one-off needs
/// its own box.
pub(crate) fn spacer(ui: &mut IMUI, id: &str, height: f32) {
    ui.named_row(id, |_| {})
        .width(ui, UISize::ParentPct(1.0))
        .height(ui, UISize::Pixels(height));
}

pub(crate) fn settings_heading(ui: &mut IMUI, label: &str) {
    let theme = *ui.theme();
    ui.label(label)
        .width(ui, UISize::ParentPct(1.0))
        .height(ui, UISize::Pixels(26.0))
        .text_color(ui, theme.text_muted);
}
