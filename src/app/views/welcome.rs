//! First launch.
//!
//! Enkr has always dropped straight into an editor with one seeded note, which
//! leaves the central question unanswered: are these notes staying on this
//! machine, or are they going somewhere? Sync was configurable in four
//! unconnected places, and a fresh install never dials anything — true, but
//! nothing said so.
//!
//! So this screen asks that question once, plainly, and offers the settings
//! facts a first-run user actually wants (where the data lives, which build
//! this is, who this device is). Working offline is a first-class answer here,
//! not a decline.

use crate::app::*;

/// Widest the content column gets; centred in whatever space is available.
const COLUMN_WIDTH: f32 = 460.0;
/// Breathing room kept either side of the welcome card on a window too narrow
/// to give it its full [`COLUMN_WIDTH`].
const WELCOME_MARGIN: f32 = 12.0;

pub(crate) fn welcome_view(ui: &mut IMUI, state: &mut EnkrState, pal: &Colors) -> UIBoxHandle {
    let pal = *pal;
    let theme = *ui.theme();
    let connected = state.sync.as_ref().is_some_and(|sync| sync.connected());
    // An engine that has given up is not a connection in progress. Counting it
    // as one is what left this screen on "Connecting…" for ever after a refused
    // token, with no error shown and no way to try a different server.
    let refused = state.sync_is_dead();
    // A retry loop that keeps failing is not progress. The engine will go on
    // trying (right, for an outage), but the screen must say what is happening
    // and stay usable so another server can be tried.
    let failing = state
        .sync
        .as_ref()
        .is_some_and(|sync| sync.connect_failed());
    let connecting = state.sync.is_some() && !connected && !refused && !failing;

    let mut start_offline = false;
    let mut connect = false;
    let mut import = false;
    let mut copy_key: Option<String> = None;
    let mut fetch: Option<Uuid> = None;
    let mut create_space = false;
    let mut back = false;

    // Connecting is a means, not an end: the question it leaves unanswered is
    // "so where do my notes go?". Once the handshake lands, the card becomes a
    // different screen that answers it — what is already on this server, and
    // the two ways to end up with something to write in.
    // Seed the token field from what is stored for the active server, so an
    // empty field genuinely means "no token" rather than "not filled in yet".
    // Without this a token that the relay rejects cannot be cleared from here,
    // which is the state a refused connection leaves you in.
    if state.welcome_tab == WelcomeTab::Online {
        state.sync_token_field();
    }
    let spaces_step = connected && state.welcome_tab == WelcomeTab::Online;
    let remote_rows = if spaces_step {
        state.remote_space_rows()
    } else {
        Vec::new()
    };

    let root = ui.named_column("###enkr_welcome", |ui| {
        let column = ui.named_column("###enkr_welcome_column", |ui| {
            ui.label(if spaces_step {
                "You're connected"
            } else {
                "Welcome to Enkr"
            })
            .width(ui, UISize::ParentPct(1.0))
            .height(ui, UISize::Pixels(40.0))
            .text_color(ui, pal.text)
            .font_size(ui, 26.0);
            ui.wrapping_label(if spaces_step {
                "Start a new space, or bring one that is already on this server \
                 onto this device."
            } else {
                "Markdown notes, organised into spaces. Everything you sync is \
                 end-to-end encrypted — the server never sees your text."
            })
            .width(ui, UISize::ParentPct(1.0))
            .text_color(ui, pal.text_muted)
            .font_size(ui, 14.0);

            spacer(ui, "###enkr_welcome_gap1", 12.0);

            // ---- Pick one ------------------------------------------------
            // Hidden on the spaces step: the question it asks has been
            // answered, and leaving it up invites a mis-click back into a
            // choice the user already made.
            if !spaces_step {
                let picker = ui.named_row("###enkr_welcome_picker", |ui| {
                    for (tab, label) in welcome_tabs() {
                        let selected = state.welcome_tab == tab;
                        let variant = if selected {
                            BtnVariant::Primary
                        } else {
                            BtnVariant::Secondary
                        };
                        if enkr_button(ui, label, None, variant)
                            .width(ui, UISize::Fill)
                            .height(ui, UISize::Pixels(32.0))
                            .clicked()
                            && !selected
                        {
                            state.welcome_tab = tab;
                            // Restart the fade from nothing, so the new panel
                            // arrives rather than swapping in mid-frame.
                            state.welcome_fade = 0.0;
                        }
                    }
                });
                picker
                    .width(ui, UISize::ParentPct(1.0))
                    .height(ui, UISize::Pixels(36.0))
                    .gap(ui, theme.gap_sm);
            }

            // Advance the cross-fade, asking for the next frame while it runs —
            // the app only repaints on demand, so without this it would stop
            // part-way through.
            if state.welcome_fade < 1.0 {
                state.welcome_fade = (state.welcome_fade + ui.dt() * 6.0).min(1.0);
                ui.request_repaint();
            }
            let fade = state.welcome_fade;

            spacer(ui, "###enkr_welcome_gap2", 8.0);

            let panel = ui.named_column("###enkr_welcome_panel", |ui| {
                if spaces_step {
                    // -- Already on this server ------------------------------
                    if remote_rows.is_empty() {
                        ui.wrapping_label(
                            "This server is holding nothing for you yet. Create a space \
                         and it will sync here.",
                        )
                        .width(ui, UISize::ParentPct(1.0))
                        .text_color(ui, pal.text_muted)
                        .font_size(ui, 13.0);
                    } else {
                        settings_heading(ui, "Already on this server");
                        for remote in &remote_rows {
                            let id_full = remote.space_id.to_string();
                            let label = match &remote.name {
                                Some(name) => name.clone(),
                                // A peek is in flight; the id is all there is to go
                                // on until its index decrypts.
                                None => format!("Unnamed space ({})", &id_full[..8]),
                            };
                            let row =
                                ui.named_row(&format!("###enkr_welcome_remote_{id_full}"), |ui| {
                                    ui.label(&label)
                                        .width(ui, UISize::Fill)
                                        .text_color(ui, pal.text)
                                        .font_size(ui, 13.0);
                                    if remote.local.is_some() {
                                        ui.label("on this device")
                                            .text_color(ui, pal.text_muted)
                                            .font_size(ui, 12.0);
                                    } else if enkr_button(
                                        ui,
                                        &format!("Copy here###enkr_welcome_fetch_{id_full}"),
                                        Some("Bring this space onto this device"),
                                        BtnVariant::Secondary,
                                    )
                                    .height(ui, UISize::Pixels(26.0))
                                    .clicked()
                                    {
                                        fetch = Some(remote.space_id);
                                    }
                                });
                            row.width(ui, UISize::ParentPct(1.0))
                                .height(ui, UISize::Pixels(30.0))
                                .align(ui, MainAxisAlign::Start, CrossAxisAlign::Center)
                                .gap(ui, 8.0);
                        }
                    }

                    spacer(ui, "###enkr_welcome_gap_new", 8.0);
                    settings_heading(ui, "Start something new");
                    if enkr_button(
                        ui,
                        "Create a space###enkr_welcome_create_space",
                        Some("Make a new space that syncs to this server"),
                        BtnVariant::Primary,
                    )
                    .width(ui, UISize::ParentPct(1.0))
                    .height(ui, UISize::Pixels(34.0))
                    .clicked()
                    {
                        create_space = true;
                    }

                    // Still needed here: being invited is the third way to get a
                    // space, and it is the only one that requires handing something
                    // to another person.
                    spacer(ui, "###enkr_welcome_gap3", 8.0);
                    settings_heading(ui, "Your device key");
                    ui.wrapping_label(
                        "Send this to someone so they can invite you to a shared space.",
                    )
                    .width(ui, UISize::ParentPct(1.0))
                    .text_color(ui, pal.text_muted)
                    .font_size(ui, 12.0);
                    let key = state
                        .sync
                        .as_ref()
                        .map(|sync| sync.device_key().to_string())
                        .unwrap_or_default();
                    let row = ui.named_row("###enkr_welcome_key_row", |ui| {
                        ui.label(&short_key(&key))
                            .width(ui, UISize::Fill)
                            .text_color(ui, pal.text)
                            .font_size(ui, 12.0);
                        if enkr_button(
                            ui,
                            "Copy###enkr_welcome_copy_key",
                            Some("Copy the full key to the clipboard"),
                            BtnVariant::Secondary,
                        )
                        .height(ui, UISize::Pixels(26.0))
                        .clicked()
                        {
                            copy_key = Some(key.clone());
                        }
                    });
                    row.width(ui, UISize::ParentPct(1.0))
                        .height(ui, UISize::Pixels(30.0))
                        .align(ui, MainAxisAlign::Start, CrossAxisAlign::Center)
                        .gap(ui, 8.0);

                    spacer(ui, "###enkr_welcome_gap_done", 8.0);
                    let actions = ui.named_row("###enkr_welcome_step_actions", |ui| {
                        // An escape hatch, because hiding the picker would otherwise
                        // strand someone who connected to the wrong server.
                        if enkr_button(
                            ui,
                            "Use a different server###enkr_welcome_back",
                            Some("Disconnect and choose again"),
                            BtnVariant::Secondary,
                        )
                        .height(ui, UISize::Pixels(34.0))
                        .clicked()
                        {
                            back = true;
                        }
                        if enkr_button(
                            ui,
                            "Go to my notes###enkr_welcome_done",
                            Some("Finish setting up"),
                            BtnVariant::Primary,
                        )
                        .width(ui, UISize::Fill)
                        .height(ui, UISize::Pixels(34.0))
                        .clicked()
                        {
                            start_offline = true;
                        }
                    });
                    actions
                        .width(ui, UISize::ParentPct(1.0))
                        .height(ui, UISize::Pixels(38.0))
                        .align(ui, MainAxisAlign::Start, CrossAxisAlign::Center)
                        .gap(ui, 8.0);
                    return;
                }
                match state.welcome_tab {
                    WelcomeTab::Offline => {
                        ui.wrapping_label(
                            "Your notes stay on this device. Nothing is sent anywhere. You can \
                         connect later from Settings.",
                        )
                        .width(ui, UISize::ParentPct(1.0))
                        .text_color(ui, pal.text_muted)
                        .font_size(ui, 13.0);
                        if enkr_button(
                            ui,
                            "Start offline###enkr_welcome_offline",
                            Some("Keep everything on this device"),
                            BtnVariant::Primary,
                        )
                        .width(ui, UISize::ParentPct(1.0))
                        .height(ui, UISize::Pixels(34.0))
                        .clicked()
                        {
                            start_offline = true;
                        }
                    }
                    WelcomeTab::Online => {
                        ui.wrapping_label(
                            "Share spaces across your devices, and with other people. Pick a \
                         name they'll see beside your cursor.",
                        )
                        .width(ui, UISize::ParentPct(1.0))
                        .text_color(ui, pal.text_muted)
                        .font_size(ui, 13.0);

                        ui.label("Your name")
                            .width(ui, UISize::ParentPct(1.0))
                            .text_color(ui, pal.text_muted)
                            .font_size(ui, 12.0);
                        ui.line_edit("###enkr_welcome_nick", &mut state.nickname_input, false)
                            .width(ui, UISize::ParentPct(1.0));

                        // The web build only ever offers the default server
                        // (`add_server` is a no-op there), so there is nothing to
                        // choose.
                        #[cfg(not(target_arch = "wasm32"))]
                        {
                            // Name the server that will actually be used, rather
                            // than leaving an empty box and no indication that
                            // connecting does anything sensible. `active_server` is
                            // DEFAULT_SERVER on a fresh install, so this reads as a
                            // prefilled default.
                            let in_use = state.active_server.clone();
                            ui.label("Server")
                                .width(ui, UISize::ParentPct(1.0))
                                .text_color(ui, pal.text_muted)
                                .font_size(ui, 12.0);
                            ui.wrapping_label(&in_use)
                                .width(ui, UISize::ParentPct(1.0))
                                .text_color(ui, pal.text)
                                .font_size(ui, 12.0);
                            ui.line_edit_with_placeholder(
                                "###enkr_welcome_server",
                                &mut state.add_server_input,
                                false,
                                "Or paste another server's URL",
                            )
                            .width(ui, UISize::ParentPct(1.0));
                            ui.wrapping_label("Leave this empty to use the server above.")
                                .width(ui, UISize::ParentPct(1.0))
                                .text_color(ui, pal.text_muted)
                                .font_size(ui, 11.0);

                            // Paid relays need one; self-hosted and invited-guest
                            // setups need nothing, so it stays optional and last.
                            ui.label("Account token (optional)")
                                .width(ui, UISize::ParentPct(1.0))
                                .text_color(ui, pal.text_muted)
                                .font_size(ui, 12.0);
                            ui.line_edit_with_placeholder(
                                "###enkr_welcome_token",
                                &mut state.token_input,
                                true,
                                "Only if your server requires one",
                            )
                            .width(ui, UISize::ParentPct(1.0));
                        }

                        let (label, variant) = if connected {
                            ("Connected###enkr_welcome_connect", BtnVariant::Secondary)
                        } else if connecting {
                            (
                                "Connecting\u{2026}###enkr_welcome_connect",
                                BtnVariant::Secondary,
                            )
                        } else if refused || failing {
                            // Says what the button will actually do. The server
                            // and token fields above are live, so changing
                            // either and pressing this is the way out.
                            ("Try again###enkr_welcome_connect", BtnVariant::Primary)
                        } else {
                            ("Connect###enkr_welcome_connect", BtnVariant::Primary)
                        };
                        if enkr_button(ui, label, Some("Connect to the sync server"), variant)
                            .width(ui, UISize::ParentPct(1.0))
                            .height(ui, UISize::Pixels(34.0))
                            .clicked()
                            && !connected
                        {
                            connect = true;
                        }
                        if let Some(err) = state.sync.as_ref().and_then(|sync| sync.last_error()) {
                            let err = err.to_string();
                            ui.wrapping_label(&err)
                                .width(ui, UISize::ParentPct(1.0))
                                .text_color(ui, Color::new("#e05252"))
                                .font_size(ui, 12.0);
                        }
                    }
                    WelcomeTab::Import => {
                        ui.wrapping_label(
                            "Read a folder of markdown files in as a space. You can still choose \
                         to sync afterwards.",
                        )
                        .width(ui, UISize::ParentPct(1.0))
                        .text_color(ui, pal.text_muted)
                        .font_size(ui, 13.0);
                        #[cfg(not(target_arch = "wasm32"))]
                        if enkr_button(
                            ui,
                            "Import a folder of markdown\u{2026}###enkr_welcome_import",
                            Some("Read an existing folder in as a space"),
                            BtnVariant::Primary,
                        )
                        .width(ui, UISize::ParentPct(1.0))
                        .height(ui, UISize::Pixels(34.0))
                        .clicked()
                        {
                            import = true;
                        }
                    }
                }
            });
            panel
                .width(ui, UISize::ParentPct(1.0))
                .height(ui, UISize::ChildrenSum)
                .gap(ui, 8.0)
                .opacity(ui, fade);

            spacer(ui, "###enkr_welcome_gap5", 20.0);

            // ---- The facts a first-run user asks for ---------------------
            let footer = ui.named_column("###enkr_welcome_footer", |ui| {
                #[cfg(not(target_arch = "wasm32"))]
                footer_line(
                    ui,
                    &pal,
                    &format!("Notes are stored at {}", default_database_path().display()),
                );
                footer_line(
                    ui,
                    &pal,
                    &format!(
                        "Enkr {} ({})",
                        env!("CARGO_PKG_VERSION"),
                        env!("ENKR_GIT_HASH")
                    ),
                );
            });
            footer
                .width(ui, UISize::ParentPct(1.0))
                .gap(ui, 2.0)
                .padding(ui, 8.0, 0.0, 0.0, 0.0);
            let _ = theme;
        });
        // Hugs its content rather than filling, so the scroller can centre it
        // — but never wider than the window. The root here is a *column*, so
        // the horizontal overflow pass never looks at this card
        // (`imui/layout.rs` only reconciles along the container's own axis)
        // and `CrossAxisAlign::Center` clamps at zero: an oversized card was
        // simply clipped on the right, with no horizontal scroll to recover
        // it. On a 390px phone that lost 70px of every onboarding screen.
        let width = COLUMN_WIDTH.min(ui.window_size().0 - WELCOME_MARGIN * 2.0);
        column
            .width(ui, UISize::Pixels(width))
            .height(ui, UISize::ChildrenSum)
            .padding(ui, 40.0, 24.0, 32.0, 24.0)
            .gap(ui, 8.0);
    });
    // Horizontally centred, but anchored to the top rather than centred
    // vertically. Centring made the card's position a function of its height,
    // so switching tabs — Offline's body is short, Online's is tall — slid the
    // title and the picker up and down under the pointer. The picker is the one
    // thing that must hold still: it is what the user is aiming at.
    // Content taller than the window scrolls, as before.
    let root = root
        .width(ui, UISize::ParentPct(1.0))
        .height(ui, UISize::Fill)
        .padding(ui, 0.0, SCROLLBAR_GUTTER, 0.0, 0.0)
        .align(ui, MainAxisAlign::Start, CrossAxisAlign::Center)
        .scroll_y(ui, true)
        .clip(ui, true)
        .background(ui, pal.app_bg);

    if start_offline {
        state.finish_onboarding();
    }
    if connect {
        state.connect_from_welcome();
    }
    if import {
        state.open_import_picker();
    }
    if create_space {
        state.create_synced_space();
    }
    if let Some(space) = fetch
        && let Some(sync) = state.sync.as_mut()
    {
        sync.fetch_space(space);
    }
    if back {
        state.disconnect_sync();
    }
    if let Some(key) = copy_key {
        mae::os::clipboard_set(&key);
        ui.toast(ToastLevel::Info, "Device key copied");
    }
    root
}

/// The picker's tabs, in the order they are offered.
///
/// Offline first, because it is the safe answer and the one that needs no
/// decisions. Import is native-only: there is no folder to read on the web
/// build, so offering the tab would only lead to a dead end.
fn welcome_tabs() -> Vec<(WelcomeTab, &'static str)> {
    let mut tabs = vec![
        (WelcomeTab::Offline, "Offline###enkr_welcome_tab_offline"),
        (WelcomeTab::Online, "Online###enkr_welcome_tab_online"),
    ];
    #[cfg(not(target_arch = "wasm32"))]
    tabs.push((WelcomeTab::Import, "Import###enkr_welcome_tab_import"));
    tabs
}

fn footer_line(ui: &mut IMUI, pal: &Colors, text: &str) {
    ui.wrapping_label(text)
        .width(ui, UISize::ParentPct(1.0))
        .text_color(ui, pal.text_faint)
        .font_size(ui, 11.0);
}

/// The key is 128 hex characters — unreadable in full and pointless to show.
/// The head and tail are enough to check you copied the right one.
fn short_key(key: &str) -> String {
    if key.len() <= 24 {
        return key.to_string();
    }
    format!("{}\u{2026}{}", &key[..12], &key[key.len() - 8..])
}
