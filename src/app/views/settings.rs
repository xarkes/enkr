//! The Settings **View**: a category list beside a scrolling detail pane, in
//! place of the 360x480 draggable window that used to mix editor preferences,
//! server configuration, identity and renderer diagnostics in one column — and
//! in place of the separate Synchronization window, which is now the
//! "Sync & Devices" category.
//!
//! A view rather than a modal because settings is *browsing*: you arrive, look
//! around, and leave. The cost is that the sidebar is hidden while you are here,
//! so you can no longer watch a fetch land behind the window — `take_notices`
//! raises a toast for that instead, and toasts survive a view change.

use crate::app::*;

/// Width of the category rail.
const CATEGORY_WIDTH: f32 = 176.0;

/// A Settings category. Ordered as listed: the things a person changes most
/// often first, renderer diagnostics last.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum SettingsSection {
    General,
    Editor,
    Sync,
    Data,
    Advanced,
}

impl SettingsSection {
    pub(crate) const ALL: [SettingsSection; 5] = [
        SettingsSection::General,
        SettingsSection::Editor,
        SettingsSection::Sync,
        SettingsSection::Data,
        SettingsSection::Advanced,
    ];

    pub(crate) fn title(self) -> &'static str {
        match self {
            SettingsSection::General => "General",
            SettingsSection::Editor => "Editor",
            SettingsSection::Sync => "Sync & Devices",
            SettingsSection::Data => "Data",
            SettingsSection::Advanced => "Advanced",
        }
    }
}

/// The whole Settings view: back button, category rail, detail pane.
pub(crate) fn settings_view(
    ui: &mut IMUI,
    state: &mut EnkrState,
    pal: &Colors,
    section: SettingsSection,
) -> UIBoxHandle {
    let pal = *pal;
    let theme = *ui.theme();
    let mut go_back = false;
    let mut pick: Option<SettingsSection> = None;

    let root = ui.named_column("###enkr_settings_view", |ui| {
        // Header: back, then the section title.
        let header = ui.named_row("###enkr_settings_header", |ui| {
            if ui
                .button_icon_plain(&format!("{BACK_ICON}###enkr_settings_back"), Some("Back"))
                .clicked()
            {
                go_back = true;
            }
            ui.label("Settings")
                .width(ui, UISize::Fill)
                .text_color(ui, pal.text)
                .font_size(ui, theme.size_text + 3.0);
        });
        header
            .width(ui, UISize::ParentPct(1.0))
            .height(ui, UISize::Pixels(56.0))
            .padding(ui, 0.0, 16.0, 0.0, 12.0)
            .align(ui, MainAxisAlign::Start, CrossAxisAlign::Center)
            .gap(ui, 8.0)
            .background(ui, pal.app_bg);

        // Beside the detail pane normally; a strip above it when the window
        // is too narrow to give up 176px of it. At 390px the rail left ~156px
        // of usable detail width, which is narrower than several of the rows
        // it has to hold.
        let narrow = is_narrow(ui);
        let body_axis = if narrow { Axis::Y } else { Axis::X };
        let rail_axis = if narrow { Axis::X } else { Axis::Y };
        let body = ui.named_container("###enkr_settings_body", body_axis, |ui| {
            // Category rail.
            let rail = ui.named_container("###enkr_settings_rail", rail_axis, |ui| {
                for candidate in SettingsSection::ALL {
                    let selected = candidate == section;
                    // Down a column the label fills the rail's width, so
                    // the whole row is the hit target. Across a strip there
                    // is no width to fill — a `Fill` child inside a row that
                    // hugs its children resolves to nothing, which piled all
                    // five labels on top of each other — so it hugs its own
                    // text instead.
                    let label_width = if narrow {
                        UISize::TextContent(0.0)
                    } else {
                        UISize::Fill
                    };
                    let row =
                        ui.clickable_row(&format!("###enkr_settings_cat_{candidate:?}"), |ui| {
                            ui.label(candidate.title())
                                .width(ui, label_width)
                                .text_color(ui, if selected { pal.text } else { pal.text_muted })
                                .font_size(ui, 13.0);
                        });
                    let bg = if selected {
                        pal.selected_bg
                    } else if row.hover() {
                        pal.hover_bg
                    } else {
                        transparent_like(pal.hover_bg)
                    };
                    // Full width down a column; hugging its label across a
                    // strip, where five equal shares would be ~70px each.
                    let row_width = if narrow {
                        UISize::ChildrenSum
                    } else {
                        UISize::ParentPct(1.0)
                    };
                    row.width(ui, row_width)
                        .height(ui, UISize::Pixels(30.0))
                        .padding(ui, 4.0, 8.0, 4.0, 8.0)
                        .corner_radius(ui, theme.radius)
                        .background(ui, bg)
                        .align(ui, MainAxisAlign::Start, CrossAxisAlign::Center)
                        .cursor(ui, OSCursor::Hand);
                    if row.clicked() {
                        pick = Some(candidate);
                    }
                }
            });
            if narrow {
                // Scrolls horizontally rather than shrinking: mae's overflow
                // pass squashes a too-full row down to nothing (it has no
                // wrap), and a scroll container is exempt from that.
                rail.width(ui, UISize::ParentPct(1.0))
                    .height(ui, UISize::Pixels(44.0))
                    .padding(ui, 7.0, 8.0, 7.0, 8.0)
                    .gap(ui, 4.0)
                    .scroll_x(ui, true)
                    .clip(ui, true)
                    .background(ui, pal.sidebar_bg);
            } else {
                rail.width(ui, UISize::Pixels(CATEGORY_WIDTH))
                    .height(ui, UISize::ParentPct(1.0))
                    .padding(ui, 8.0, 8.0, 8.0, 12.0)
                    .gap(ui, 2.0)
                    .background(ui, pal.sidebar_bg);
            }

            let detail = ui.named_column("###enkr_settings_detail", |ui| {
                settings_heading(ui, section.title());
                match section {
                    SettingsSection::General => section_general(ui, state, &pal),
                    SettingsSection::Editor => section_editor(ui, state),
                    SettingsSection::Sync => section_sync(ui, state),
                    SettingsSection::Data => section_data(ui, state),
                    SettingsSection::Advanced => section_advanced(ui),
                }
            });
            // `Fill` along the body's own axis either way: the width beside
            // the rail, the remaining height below it.
            let (detail_w, detail_h) = if narrow {
                (UISize::ParentPct(1.0), UISize::Fill)
            } else {
                (UISize::Fill, UISize::ParentPct(1.0))
            };
            detail
                .width(ui, detail_w)
                .height(ui, detail_h)
                .padding(ui, 12.0, 24.0 + SCROLLBAR_GUTTER, 24.0, 24.0)
                .gap(ui, theme.gap_sm)
                .scroll_y(ui, true)
                .clip(ui, true)
                .background(ui, pal.content_bg);
        });
        body.width(ui, UISize::ParentPct(1.0))
            .height(ui, UISize::Fill)
            .gap(ui, 0.0);
    });
    let root = root
        .width(ui, UISize::ParentPct(1.0))
        .height(ui, UISize::Fill)
        .gap(ui, 0.0)
        .background(ui, pal.app_bg);

    if let Some(section) = pick {
        state.set_view(View::Settings(section));
    }
    if go_back {
        state.dismiss_top();
    }
    root
}

/// Where the data lives, which identity this installation uses, and what build this is — the
/// questions a first-run user actually has, and the ones you need again when
/// something goes wrong.
fn section_general(ui: &mut IMUI, state: &mut EnkrState, pal: &Colors) {
    let theme = *ui.theme();
    let dark = state.theme_kind == ThemeKind::Dark;
    if settings_toggle(ui, "enkr_set_dark", "Dark theme", dark) {
        state.theme_kind = if dark {
            ThemeKind::Light
        } else {
            ThemeKind::Dark
        };
    }

    settings_heading(ui, "Identity");
    let fingerprint = state
        .sync
        .as_ref()
        .map(|sync| sync.identity_key()[..16].to_string())
        .unwrap_or_else(|| "not created until you connect".to_string());
    info_row(ui, pal, "Identity", &fingerprint);
    recovery_phrase_controls(ui, state, &theme);
    #[cfg(not(target_arch = "wasm32"))]
    info_row(
        ui,
        pal,
        "Notes",
        &default_database_path().display().to_string(),
    );
    info_row(
        ui,
        pal,
        "Version",
        &format!("{} ({})", env!("CARGO_PKG_VERSION"), env!("ENKR_GIT_HASH")),
    );

    if enkr_button(
        ui,
        "Show the welcome screen###enkr_show_welcome",
        Some("Revisit the first-run choices"),
        BtnVariant::Secondary,
    )
    .clicked()
    {
        state.show_welcome();
    }
    let _ = theme;
}

/// A label/value line for read-only facts.
fn info_row(ui: &mut IMUI, pal: &Colors, label: &str, value: &str) {
    let theme = *ui.theme();
    let row = ui.named_row(&format!("###enkr_info_{label}"), |ui| {
        ui.label(label)
            .width(ui, UISize::Pixels(96.0))
            .text_color(ui, pal.text_muted)
            .font_size(ui, theme.size_text - 1.0);
        // Selectable so an identity key or path can actually be copied out.
        ui.label(value)
            .width(ui, UISize::Fill)
            .text_color(ui, pal.text)
            .font_size(ui, theme.size_text - 1.0);
    });
    row.width(ui, UISize::ParentPct(1.0))
        .height(ui, UISize::Pixels(24.0))
        .align(ui, MainAxisAlign::Start, CrossAxisAlign::Center)
        .gap(ui, 8.0);
}

fn section_editor(ui: &mut IMUI, state: &mut EnkrState) {
    settings_heading(ui, "Editor");
    if settings_toggle(ui, "enkr_set_wrap", "Wrap long lines", state.wrap_x) {
        state.wrap_x = !state.wrap_x;
    }
}

/// Everything about talking to a server: which one, as whom, this identity's
/// invite key, and the spaces the server is holding for you. Previously split
/// between the Settings window and a separate Synchronization window, which is
/// why neither ever told the whole story.
fn section_sync(ui: &mut IMUI, state: &mut EnkrState) {
    let theme = *ui.theme();
    // One status line for the whole section. It used to appear twice, once
    // saying "Connected" and once "Not connected - configure the server in
    // Settings" — the latter written when Sync was its own window and could
    // sensibly point elsewhere.
    let (status, status_color) = match state.sync.as_ref() {
        Some(sync) if sync.connected() => ("Connected", Color::new("#34a853")),
        Some(sync) if sync.incompatible().is_some() => {
            ("Incompatible server version", Color::new("#d93025"))
        }
        Some(sync) if sync.rejected() => ("Account token refused", Color::new("#d93025")),
        Some(_) => ("Connecting\u{2026}", Color::new("#f0a030")),
        None => ("Not connected", theme.text_muted),
    };
    ui.label(status)
        .width(ui, UISize::ParentPct(1.0))
        .height(ui, UISize::Pixels(24.0))
        .text_color(ui, status_color);

    settings_heading(ui, "Sync servers");
    let servers = state.server_list();
    let active = state.active_server.clone();
    // The engine thread being up (`is_some`) is *not* the same as having
    // reached the server: only `connected()` means the WebSocket
    // handshake succeeded.
    let engine_running = state.sync.is_some();
    let net_connected = state.sync.as_ref().is_some_and(|s| s.connected());
    // Plan/usage is only known for the server we actually authenticated to,
    // and only when it issued us an account — it arrives on the handshake.
    let account = state.sync.as_ref().and_then(|s| s.account());
    // Precomputed per server, before the render loop: the detail line reads
    // `state` while the rows below need it mutably.
    let details: Vec<(bool, Vec<String>)> = servers
        .iter()
        .map(|server| {
            let has_token = state.account_token(server).is_some();
            let names = state
                .notes
                .spaces()
                .iter()
                .filter(|space| space.server.as_deref() == Some(server.as_str()))
                .map(|space| space.name.clone())
                .collect();
            (has_token, names)
        })
        .collect();
    // What the *connected* server says it holds — including spaces this installation
    // has never mirrored. Only obtainable for the one server we have a live
    // connection to, which is why every other row can show local spaces only.
    let remote_rows = if net_connected {
        state.remote_space_rows()
    } else {
        Vec::new()
    };
    let mut refresh_spaces = false;
    let mut fetch_space: Option<Uuid> = None;
    let mut delete_space: Option<Uuid> = None;
    let mut activate: Option<String> = None;
    let mut remove: Option<String> = None;
    for (server, (has_token, space_names)) in servers.iter().zip(&details) {
        let is_active = *server == active;
        let is_default = server == DEFAULT_SERVER;
        let row = ui.named_row(&format!("###enkr_srv_row_{server}"), |ui| {
            let status = if is_active && net_connected {
                "\u{25cf}" // filled dot: active + connected
            } else if is_active {
                "\u{25cb}" // hollow dot: active but not (yet) connected
            } else {
                " "
            };
            let dot_color = if is_active && net_connected {
                Color::new("#34a853")
            } else {
                theme.text_muted
            };
            ui.label(status)
                .width(ui, UISize::Pixels(14.0))
                .text_color(ui, dot_color);
            ui.label(server)
                .width(ui, UISize::Fill)
                .font_size(ui, theme.size_text - 1.0)
                .text_color(ui, theme.text);
            if is_active {
                let label = if net_connected {
                    "connected"
                } else if engine_running {
                    "connecting\u{2026}"
                } else {
                    "active"
                };
                ui.label(label)
                    .text_color(ui, theme.text_muted)
                    .font_size(ui, theme.size_text - 2.0);
            } else if enkr_button(
                ui,
                &format!("Use###enkr_srv_use_{server}"),
                Some("Make this the active server"),
                BtnVariant::Secondary,
            )
            .height(ui, UISize::Pixels(26.0))
            .clicked()
            {
                activate = Some(server.clone());
            }
            if !is_default
                && enkr_button(
                    ui,
                    &format!("Remove###enkr_srv_remove_{server}"),
                    Some("Remove this server"),
                    BtnVariant::Secondary,
                )
                .height(ui, UISize::Pixels(26.0))
                .clicked()
            {
                remove = Some(server.clone());
            }
        });
        row.width(ui, UISize::ParentPct(1.0))
            .height(ui, UISize::Pixels(30.0))
            .align(ui, MainAxisAlign::Start, CrossAxisAlign::Center)
            .gap(ui, 6.0);

        // Second line: what this server actually is to you. Without it the
        // list was three near-identical URLs with no way to tell which one
        // holds your spaces or which one you have paid for.
        let mut detail = String::new();
        let space_count = if is_active && net_connected {
            remote_rows.len()
        } else {
            space_names.len()
        };
        detail.push_str(
            match space_count {
                0 => "No spaces".to_string(),
                1 => "1 space".to_string(),
                n => format!("{n} spaces"),
            }
            .as_str(),
        );
        detail.push_str(if *has_token {
            " \u{00b7} Token saved"
        } else {
            " \u{00b7} No token"
        });
        // Usage is only meaningful for the connection that reported it.
        if is_active
            && net_connected
            && let Some(info) = account
        {
            detail.push_str(&format!(
                " \u{00b7} {} of {} used",
                format_bytes(info.used_bytes),
                format_bytes(info.quota_bytes)
            ));
            if let Some(at) = info.expires_at {
                let days = (at - now_ms()) / 86_400_000;
                detail.push_str(&if days < 0 {
                    " \u{00b7} EXPIRED".to_string()
                } else {
                    format!(" \u{00b7} {days}d left")
                });
            }
        }
        ui.label(&detail)
            .width(ui, UISize::ParentPct(1.0))
            .height(ui, UISize::Pixels(18.0))
            .padding(ui, 0.0, 0.0, 0.0, 20.0)
            .text_color(ui, theme.text_muted)
            .font_size(ui, theme.size_text - 2.0);

        // The spaces this server holds, under the server that holds them. They
        // used to be one flat list at the bottom of the page, which silently
        // meant "spaces on whichever server happens to be connected" — with
        // several servers configured there was nothing saying which one a row
        // belonged to.
        if is_active && net_connected {
            for remote in &remote_rows {
                let id_full = remote.space_id.to_string();
                // First 8 hex chars of the uuid, to tell two unnamed spaces
                // apart until a peek yields their names.
                let label = match &remote.name {
                    Some(name) => format!("{name}  ({})", &id_full[..8]),
                    None => format!("Unnamed space  ({})", &id_full[..8]),
                };
                let space_row = ui.named_row(&format!("###enkr_remote_row_{id_full}"), |ui| {
                    ui.label(if remote.local.is_some() {
                        "\u{2713}" // here as well as on the server
                    } else {
                        "\u{2601}" // server only
                    })
                    .width(ui, UISize::Pixels(16.0))
                    .font_size(ui, theme.size_text - 2.0)
                    .text_color(ui, theme.text_muted);
                    ui.label(&label)
                        .width(ui, UISize::Fill)
                        .font_size(ui, theme.size_text - 2.0)
                        .text_color(ui, theme.text_muted);
                    if remote.local.is_none()
                        && enkr_button(
                            ui,
                            &format!("Sync###enkr_fetch_{id_full}"),
                            Some("Copy this space onto this installation"),
                            BtnVariant::Secondary,
                        )
                        .height(ui, UISize::Pixels(24.0))
                        .clicked()
                    {
                        fetch_space = Some(remote.space_id);
                    }
                    // Delete is about the space on the *server*, so it belongs
                    // to whoever owns it — whether or not this installation holds a
                    // local copy. Hiding it unless mirrored left an owner
                    // unable to delete a space they had unsynced.
                    if remote.is_owner
                        && enkr_button(
                            ui,
                            &format!("Delete###enkr_delete_{id_full}"),
                            Some("Delete this space for everyone"),
                            BtnVariant::Danger,
                        )
                        .height(ui, UISize::Pixels(24.0))
                        .clicked()
                    {
                        delete_space = Some(remote.space_id);
                    }
                });
                space_row
                    .width(ui, UISize::ParentPct(1.0))
                    .height(ui, UISize::Pixels(26.0))
                    .padding(ui, 0.0, 0.0, 0.0, 20.0)
                    .align(ui, MainAxisAlign::Start, CrossAxisAlign::Center)
                    .gap(ui, 6.0);
            }
            let refresh_row = ui.named_row("###enkr_remote_refresh_row", |ui| {
                if enkr_button(
                    ui,
                    "Refresh###enkr_sync_refresh",
                    Some("Ask the server what it holds"),
                    BtnVariant::Secondary,
                )
                .height(ui, UISize::Pixels(24.0))
                .clicked()
                {
                    refresh_spaces = true;
                }
            });
            refresh_row
                .width(ui, UISize::ParentPct(1.0))
                .height(ui, UISize::Pixels(28.0))
                .padding(ui, 0.0, 0.0, 0.0, 20.0)
                .align(ui, MainAxisAlign::Start, CrossAxisAlign::Center);
        } else {
            // Only one connection runs at a time, so for every other server all
            // that can honestly be shown is what this installation already holds.
            for name in space_names {
                ui.label(&format!("\u{2713}  {name}"))
                    .width(ui, UISize::ParentPct(1.0))
                    .height(ui, UISize::Pixels(22.0))
                    .padding(ui, 0.0, 0.0, 0.0, 36.0)
                    .text_color(ui, theme.text_muted)
                    .font_size(ui, theme.size_text - 2.0);
            }
            if !space_names.is_empty() || is_active {
                ui.label("Connect to this server to see everything it holds.")
                    .width(ui, UISize::ParentPct(1.0))
                    .height(ui, UISize::Pixels(20.0))
                    .padding(ui, 0.0, 0.0, 0.0, 20.0)
                    .text_color(ui, theme.text_muted)
                    .font_size(ui, theme.size_text - 2.0);
            }
        }
    }
    if refresh_spaces && let Some(sync) = state.sync.as_mut() {
        sync.refresh_remote_spaces();
    }
    if let Some(space) = fetch_space
        && let Some(sync) = state.sync.as_mut()
    {
        sync.fetch_space(space);
    }
    if let Some(space) = delete_space {
        state.delete_space_confirm = Some(space);
    }
    if let Some(server) = activate {
        state.select_server(server);
    }
    if let Some(server) = remove {
        if state.active_server == server {
            state.active_server = DEFAULT_SERVER.to_string();
            state.disconnect_sync();
        }
        state.remove_server(&server);
    }

    // Account token for the *active* server. Per server because a token is
    // minted by one relay and means nothing to another.
    spacer(ui, "###enkr_sync_gap_token", 10.0);
    settings_heading(ui, "Account token");
    state.sync_token_field();
    let has_token = state.account_token(&active).is_some();
    ui.wrapping_label(&format!(
        "Only if {} requires one. Your host gives you this; collaborators \
         invited to someone else's space need none.",
        server_host(&active)
    ))
    .width(ui, UISize::ParentPct(1.0))
    .text_color(ui, theme.text_muted)
    .font_size(ui, theme.size_text - 2.0);
    let token_row = ui.row(|ui| {
        ui.line_edit_with_placeholder(
            "###enkr_account_token",
            &mut state.token_input,
            true,
            "paste the token your host gave you",
        )
        .width(ui, UISize::Fill);
        if enkr_button(
            ui,
            "Save###enkr_account_token_btn",
            Some("Store this token and reconnect with it"),
            BtnVariant::Secondary,
        )
        .clicked()
        {
            let (server, token) = (active.clone(), state.token_input.clone());
            state.set_account_token(&server, &token);
            // Entering a token is how a rejected connection is meant to
            // recover, so act on it immediately instead of waiting for the
            // user to find the Connect button.
            state.connect_sync();
        }
    });
    token_row
        .width(ui, UISize::ParentPct(1.0))
        .height(ui, UISize::Pixels(34.0))
        .align(ui, MainAxisAlign::Start, CrossAxisAlign::Center)
        .gap(ui, 6.0);
    if has_token {
        ui.label("Clear the field and save to remove it.")
            .width(ui, UISize::ParentPct(1.0))
            .text_color(ui, theme.text_muted)
            .font_size(ui, theme.size_text - 2.0);
    }

    // Web build: no custom-server input at all — see `EnkrState::add_server`'s doc
    // comment for why only the hardcoded default is ever offered there.
    #[cfg(not(target_arch = "wasm32"))]
    {
        spacer(ui, "###enkr_sync_gap_add", 10.0);
        settings_heading(ui, "Add a server");
        ui.wrapping_label(
            "Self-hosted or another provider. Adding one does not move any \
             existing space: each space stays on the server it was first synced to.",
        )
        .width(ui, UISize::ParentPct(1.0))
        .text_color(ui, theme.text_muted)
        .font_size(ui, theme.size_text - 2.0);
        let add_row = ui.row(|ui| {
            ui.line_edit_with_placeholder(
                "###enkr_add_server",
                &mut state.add_server_input,
                false,
                "host:port or wss:// URL",
            )
            .width(ui, UISize::Fill);
            if enkr_button(
                ui,
                "Add\u{2026}###enkr_add_server_btn",
                Some("Add a custom server (host:port or ws:// URL)"),
                BtnVariant::Secondary,
            )
            .clicked()
            {
                let url = state.add_server_input.trim().to_string();
                state.add_server(&url);
                state.add_server_input.clear();
            }
        });
        add_row
            .width(ui, UISize::ParentPct(1.0))
            .height(ui, UISize::Pixels(34.0))
            .align(ui, MainAxisAlign::Start, CrossAxisAlign::Center)
            .gap(ui, 6.0);
    }

    spacer(ui, "###enkr_sync_gap_nick", 10.0);
    ui.label("Nickname (shown to collaborators)")
        .width(ui, UISize::ParentPct(1.0))
        .text_color(ui, theme.text_muted)
        .font_size(ui, theme.size_text - 1.0);
    ui.line_edit("###enkr_set_nick", &mut state.nickname_input, false)
        .width(ui, UISize::ParentPct(1.0));
    // Global connect/disconnect for the active server.
    let (sync_label, sync_tooltip, sync_variant): (&str, &str, BtnVariant) = if engine_running {
        (
            "Disconnect###enkr_sync_toggle",
            "Stop synchronizing",
            BtnVariant::Secondary,
        )
    } else {
        (
            "Connect###enkr_sync_toggle",
            "Connect to the active server",
            BtnVariant::Primary,
        )
    };
    if enkr_button(ui, sync_label, Some(sync_tooltip), sync_variant)
        .width(ui, UISize::ParentPct(1.0))
        .clicked()
    {
        if engine_running {
            state.disconnect_sync();
        } else {
            state.connect_sync();
        }
    }
    // Surface a connection / sync error so a failed (e.g. wss) attempt
    // isn't silent.
    if let Some(err) = state.sync.as_ref().and_then(|s| s.last_error()) {
        let err = err.to_string();
        ui.label(&err)
            .width(ui, UISize::ParentPct(1.0))
            .text_color(ui, Color::new("#e05252"))
            .font_size(ui, theme.size_text - 1.0);
    }

    let Some(sync) = state.sync.as_mut() else {
        return;
    };

    settings_heading(ui, "This identity's key");
    ui.label("Share this with someone to be invited to their space.")
        .width(ui, UISize::ParentPct(1.0))
        .text_color(ui, theme.text_muted)
        .font_size(ui, theme.size_text - 1.0);
    let mut key = sync.identity_key().to_string();
    let key_field = ui.textarea_with_options(
        "###enkr_identity_key",
        &mut key,
        TextAreaOptions::new()
            .wrap_x(true)
            .scroll_x(false)
            .scroll_y(false)
            .read_only(true)
            .font_size(9.0)
            .padding(Padding::all(8.0)),
    );
    key_field
        .width(ui, UISize::ParentPct(1.0))
        .height(ui, UISize::Pixels(84.0))
        .text_color(ui, theme.text_muted);
}

/// Import and export. Native-only in substance: `import_folder_into` is a wasm
/// stub and `export_folder` is not compiled there at all.
fn section_data(ui: &mut IMUI, state: &mut EnkrState) {
    let theme = *ui.theme();
    #[cfg(not(target_arch = "wasm32"))]
    {
        ui.label(
            "Read a folder of markdown files in as a new space, or write these notes back out.",
        )
        .width(ui, UISize::ParentPct(1.0))
        .text_color(ui, theme.text_muted)
        .font_size(ui, theme.size_text - 1.0);
        if enkr_button(
            ui,
            "Import a folder\u{2026}###enkr_settings_import",
            Some("Import markdown files as a new space"),
            BtnVariant::Secondary,
        )
        .clicked()
        {
            state.open_import_picker();
        }
        if enkr_button(
            ui,
            "Export to a folder\u{2026}###enkr_settings_export",
            Some("Write these notes out as markdown files"),
            BtnVariant::Secondary,
        )
        .clicked()
        {
            state.open_export_picker();
        }
    }
    #[cfg(target_arch = "wasm32")]
    {
        let _ = state;
        ui.label("Import and export are desktop-only for now.")
            .width(ui, UISize::ParentPct(1.0))
            .text_color(ui, theme.text_muted)
            .font_size(ui, theme.size_text - 1.0);
    }
}

/// Renderer diagnostics. Last, because they are the least likely thing a person
/// came here to change — and they used to be in the middle of the window.
fn section_advanced(ui: &mut IMUI) {
    settings_heading(ui, "Rendering");
    let vsync = ui.vsync_enabled();
    if settings_toggle(ui, "enkr_set_vsync", "VSync", vsync) {
        ui.set_vsync_enabled(!vsync);
    }
    let cap_fps = ui.cap_fps_to_refresh_rate();
    if settings_toggle(ui, "enkr_set_capfps", "Cap FPS to refresh rate", cap_fps) {
        ui.set_cap_fps_to_refresh_rate(!cap_fps);
    }
    let continuous = ui.render_continuously();
    if settings_toggle(
        ui,
        "enkr_set_continuous",
        "Continuous rendering",
        continuous,
    ) {
        ui.set_render_continuously(!continuous);
    }
}

/// Backup and restore for the cryptographic identity.
///
/// Lives under "This device" rather than with the sync server settings: the
/// phrase is not per-server, it is the identity. Only meaningful once a key
/// exists, which is on first connect — before that there is nothing to back up
/// and saying so is clearer than a button that errors.
fn recovery_phrase_controls(ui: &mut IMUI, state: &mut EnkrState, theme: &UITheme) {
    let has_identity = state.identity_store.is_some();
    ui.label(if has_identity {
        "Twelve words that restore this identity on another installation. Every \
         installation using it shares the same permissions and authorship."
    } else {
        "Once you connect to a sync server, a recovery phrase is created here."
    })
    .width(ui, UISize::ParentPct(1.0))
    .text_color(ui, theme.text_muted)
    .font_size(ui, theme.size_text - 1.0);
    if !has_identity {
        return;
    }
    let buttons = ui.row(|ui| {
        if enkr_button(
            ui,
            "Show recovery phrase\u{2026}###enkr_settings_show_phrase",
            Some("Reveal the twelve words for this identity"),
            BtnVariant::Secondary,
        )
        .width(ui, UISize::Fill)
        .clicked()
        {
            state.open_recovery_phrase(false);
        }
        if enkr_button(
            ui,
            "Restore from a phrase\u{2026}###enkr_settings_restore_phrase",
            Some("Restore this identity from twelve words"),
            BtnVariant::Secondary,
        )
        .width(ui, UISize::Fill)
        .clicked()
        {
            state.open_recovery_restore();
        }
    });
    buttons
        .width(ui, UISize::ParentPct(1.0))
        .height(ui, UISize::Pixels(32.0))
        .gap(ui, theme.gap_sm);
}
