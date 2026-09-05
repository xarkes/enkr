//! Layer 3 — modal surfaces: the shared window shell plus Share, the folder
//! picker, the rename dialogs and the destructive confirm.
//!
//! These are the surfaces that still own a draggable frame; the redesign
//! replaces `dialog_window` with a scrim-backed, non-draggable shell and
//! demotes the rename dialogs to inline row editing.

use crate::app::*;

pub(crate) const WINDOW_MARGIN: f32 = 12.0;

/// What the shared folder picker does when the user confirms a directory.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum FilePickMode {
    Import,
    Export,
}

pub(crate) struct ShareDialog {
    pub(crate) remote_space: Uuid,
    pub(crate) input: String,
    pub(crate) role: MemberRole,
    pub(crate) error: Option<String>,
}

/// Chrome for a modal surface: a full-window scrim, then a centred,
/// non-draggable pane with a title, a close cross and a scrolling body.
/// Returns whether the user asked to close it.
///
/// Replaces `dialog_window`, which let every dialog be dragged and remembered
/// its position per-window. That model — an in-app window manager — is what
/// produced the z-order bug: two draggable panes with no arbitration between
/// them. A modal is one thing at a time, so it does not need a position.
///
/// The scrim is an overlay pane with a background, which is enough to stop the
/// pointer reaching *ordinary* content beneath it. It does **not** stop other
/// overlays — mae overlays never block each other — so the layer state machine
/// closes the menu and palette when a modal opens rather than relying on paint
/// order (see `EnkrState::open_modal`).
pub(crate) fn modal_frame(
    ui: &mut IMUI,
    id: &str,
    title: &str,
    width: f32,
    height: f32,
    body: impl FnOnce(&mut IMUI),
) -> bool {
    let theme = *ui.theme();
    let (screen_w, screen_h) = ui.window_size();
    let width = width.min(screen_w - WINDOW_MARGIN * 2.0);
    let height = height.min(screen_h - WINDOW_MARGIN * 2.0);

    let mut scrim_color = Color::new("#000000");
    scrim_color.a = 0.32;
    let scrim = ui.floating_pane_at(Point::new(0.0, 0.0), Some(&format!("{id}_scrim")), |_| {});
    scrim
        .width(ui, UISize::Pixels(screen_w))
        .height(ui, UISize::Pixels(screen_h))
        .padding_all(ui, 0.0)
        .corner_radius(ui, 0.0)
        .background(ui, scrim_color)
        .border_color(ui, scrim_color);

    let pos = Point::new(
        ((screen_w - width) * 0.5).max(WINDOW_MARGIN),
        ((screen_h - height) * 0.5).max(WINDOW_MARGIN),
    );

    let mut close = false;
    let pane = ui.floating_pane_at(pos, Some(id), |ui| {
        let title_row = ui.named_row(&format!("{id}_title"), |ui| {
            ui.label(title)
                .width(ui, UISize::Fill)
                .height(ui, UISize::Pixels(28.0))
                .text_color(ui, theme.text)
                .font_size(ui, theme.size_text + 3.0);
            if ui
                .button_icon_plain(&format!("{CLOSE_ICON}{id}_close"), Some("Close"))
                .clicked()
            {
                close = true;
            }
        });
        title_row
            .width(ui, UISize::ParentPct(1.0))
            .height(ui, UISize::Pixels(32.0))
            .align(ui, MainAxisAlign::Start, CrossAxisAlign::Center);

        let body_box = ui.named_column(&format!("{id}_body"), body);
        body_box
            .width(ui, UISize::ParentPct(1.0))
            .height(ui, UISize::Fill)
            .gap(ui, theme.gap_sm)
            .padding(ui, 0.0, SCROLLBAR_GUTTER, 0.0, 0.0)
            .scroll_y(ui, true)
            .clip(ui, true);
    });
    pane.width(ui, UISize::Pixels(width))
        .height(ui, UISize::Pixels(height))
        .padding_all(ui, theme.pad_lg)
        .gap(ui, theme.gap_sm)
        .background(ui, theme.popover_bg)
        .border_color(ui, theme.border)
        .corner_radius(ui, theme.radius);

    // Clicking the scrim dismisses, the way every modern modal behaves.
    close || ui.press_outside(&[pane])
}

/// Owner confirmation before destroying a space on the server. Opened from the
/// sync window's per-space "Delete" button; on accept it fires the server-side
/// delete. Members (the owner included) keep their local copy — the space is
/// unsynced, not erased — when the resulting `SpaceDeleted` broadcast arrives
/// through the sync pump.
pub(crate) fn render_delete_space_confirm(ui: &mut IMUI, state: &mut EnkrState) {
    let Some(remote) = state.delete_space_confirm else {
        return;
    };
    let theme = *ui.theme();
    let width = 400.0;
    let height = 210.0;
    let chrome_close = modal_frame(
        ui,
        "###enkr_delete_space_window",
        "Delete space",
        width,
        height,
        |ui| {
            ui.label("Delete this space from the server?")
                .width(ui, UISize::ParentPct(1.0))
                .height(ui, UISize::Pixels(24.0))
                .text_color(ui, theme.text)
                .font_size(ui, theme.size_text + 1.0);
            ui.label("It will stop syncing for everyone and be removed from the")
                .width(ui, UISize::ParentPct(1.0))
                .text_color(ui, theme.text_muted)
                .font_size(ui, theme.size_text - 1.0);
            ui.label("server. Each member keeps their own local copy. This cannot be undone.")
                .width(ui, UISize::ParentPct(1.0))
                .text_color(ui, theme.text_muted)
                .font_size(ui, theme.size_text - 1.0);
            let buttons = ui.row(|ui| {
                if enkr_button(
                    ui,
                    "Delete###enkr_delete_space_confirm",
                    Some("Destroy this space for all members"),
                    BtnVariant::Danger,
                )
                .clicked()
                {
                    if let Some(sync) = state.sync.as_mut() {
                        sync.delete_remote_space(remote);
                    }
                    state.delete_space_confirm = None;
                }
                if enkr_button(
                    ui,
                    "Cancel###enkr_delete_space_cancel",
                    None,
                    BtnVariant::Secondary,
                )
                .clicked()
                {
                    state.delete_space_confirm = None;
                }
            });
            buttons
                .width(ui, UISize::ParentPct(1.0))
                .height(ui, UISize::Pixels(34.0))
                .align(ui, MainAxisAlign::Start, CrossAxisAlign::Center)
                .gap(ui, theme.gap_md);
        },
    );
    if chrome_close {
        state.delete_space_confirm = None;
    }
}

/// Drive the folder picker when it's open: import/export the chosen folder per
/// the active [`FilePickMode`], or close it on cancel.
pub(crate) fn render_file_explorer(ui: &mut IMUI, state: &mut EnkrState) {
    let Some(explorer) = state.file_explorer.as_mut() else {
        return;
    };
    let outcome = explorer.show(ui);
    // Read the "import as a new space" checkbox before dropping the explorer.
    let as_new_space = explorer.toggle_value();
    match outcome {
        FileExplorerOutcome::Browsing => {}
        FileExplorerOutcome::Cancelled => state.file_explorer = None,
        FileExplorerOutcome::Picked(path) => {
            state.file_explorer = None;
            match state.file_pick_mode {
                FilePickMode::Import => state.import_notes_from(path, as_new_space),
                FilePickMode::Export => state.export_notes_to(path),
            }
        }
    }
}

pub(crate) fn role_label(role: MemberRole) -> &'static str {
    match role {
        MemberRole::Owner => "Owner",
        MemberRole::Writer => "Writer",
        MemberRole::Reader => "Reader",
    }
}

/// Invite dialog: paste the other identity's key (shown in *its* sync window),
/// plus the list of identities already invited (manageable by owners).
pub(crate) fn render_share_dialog(ui: &mut IMUI, state: &mut EnkrState) {
    // Taken out for the frame to keep `state.sync` independently borrowable.
    let Some(mut dialog) = state.share_dialog.take() else {
        return;
    };
    let theme = *ui.theme();
    let (_, screen_h) = ui.window_size();
    let width = 420.0;
    let height = (screen_h - WINDOW_MARGIN * 2.0).min(460.0).max(240.0);

    // Member list + admin capability resolve from the cached, locally-verified
    // membership log (the first read kicks off a background refresh).
    let members = state
        .sync
        .as_mut()
        .map(|sync| sync.members(dialog.remote_space))
        .unwrap_or_default();
    let can_admin = state
        .sync
        .as_ref()
        .is_some_and(|sync| sync.can_admin(dialog.remote_space));

    // Set from inside the body by Invite (on success) and Cancel; combined with
    // the chrome's close button below.
    let mut close = false;
    let chrome_close = modal_frame(
        ui,
        "###enkr_share_window",
        "Share space",
        width,
        height,
        |ui| {
            ui.label("Paste the invitee's identity key:")
                .width(ui, UISize::ParentPct(1.0))
                .text_color(ui, theme.text_muted);
            ui.line_edit("###enkr_share_key", &mut dialog.input, false)
                .width(ui, UISize::ParentPct(1.0));
            ui.label("Permission")
                .width(ui, UISize::ParentPct(1.0))
                .text_color(ui, theme.text_muted);
            let permissions = ui.row(|ui| {
                for (role, tooltip, id) in [
                    (
                        MemberRole::Owner,
                        "Can write and manage members",
                        "Owner###enkr_share_role_owner",
                    ),
                    (
                        MemberRole::Writer,
                        "Can read and write notes",
                        "Write###enkr_share_role_writer",
                    ),
                    (
                        MemberRole::Reader,
                        "Can read notes only",
                        "Read only###enkr_share_role_reader",
                    ),
                ] {
                    let selected = dialog.role == role;
                    let variant = if selected {
                        BtnVariant::Primary
                    } else {
                        BtnVariant::Secondary
                    };
                    let button = enkr_button(ui, id, Some(tooltip), variant)
                        .width(ui, UISize::Fill)
                        .height(ui, UISize::Pixels(28.0));
                    if button.clicked() {
                        dialog.role = role;
                    }
                }
            });
            permissions
                .width(ui, UISize::ParentPct(1.0))
                .height(ui, UISize::Pixels(34.0))
                .align(ui, MainAxisAlign::Start, CrossAxisAlign::Center)
                .gap(ui, theme.gap_sm);
            if let Some(error) = dialog.error.as_deref() {
                let error = error.to_string();
                ui.label(&error)
                    .width(ui, UISize::ParentPct(1.0))
                    .text_color(ui, Color::new("#e05252"))
                    .font_size(ui, theme.size_text - 1.0);
            }
            let buttons = ui.row(|ui| {
                if enkr_button(
                    ui,
                    "Invite###enkr_share_invite",
                    Some("Add this identity"),
                    BtnVariant::Primary,
                )
                .clicked()
                {
                    match state.sync.as_mut() {
                        Some(sync) => {
                            match sync.share_space(dialog.remote_space, &dialog.input, dialog.role)
                            {
                                Ok(()) => close = true,
                                Err(err) => dialog.error = Some(err),
                            }
                        }
                        None => dialog.error = Some("not connected".into()),
                    }
                }
                if enkr_button(
                    ui,
                    "Cancel###enkr_share_cancel",
                    None,
                    BtnVariant::Secondary,
                )
                .clicked()
                {
                    close = true;
                }
            });
            buttons
                .width(ui, UISize::ParentPct(1.0))
                .height(ui, UISize::Pixels(34.0))
                .align(ui, MainAxisAlign::Start, CrossAxisAlign::Center)
                .gap(ui, theme.gap_md);

            ui.label("People with access")
                .width(ui, UISize::ParentPct(1.0))
                .height(ui, UISize::Pixels(26.0))
                .text_color(ui, theme.text)
                .font_size(ui, theme.size_text + 1.0);
            if members.is_empty() {
                ui.label("No invited identities yet.")
                    .width(ui, UISize::ParentPct(1.0))
                    .text_color(ui, theme.text_muted)
                    .font_size(ui, theme.size_text - 1.0);
            }
            for member in &members {
                let short = member.short_identity_id();
                let row = ui.row(|ui| {
                    let mut name = short.clone();
                    if member.is_self {
                        name.push_str("  (this identity)");
                    }
                    ui.label(&name)
                        .width(ui, UISize::Fill)
                        .text_color(ui, theme.text);
                    if can_admin && !member.is_self {
                        for (role, glyph) in [
                            (MemberRole::Owner, "O"),
                            (MemberRole::Writer, "W"),
                            (MemberRole::Reader, "R"),
                        ] {
                            let selected = member.role == role;
                            let variant = if selected {
                                BtnVariant::Primary
                            } else {
                                BtnVariant::Secondary
                            };
                            let clicked = enkr_button(
                                ui,
                                &format!("{glyph}###enkr_member_role_{short}_{glyph}"),
                                Some(role_label(role)),
                                variant,
                            )
                            .height(ui, UISize::Pixels(24.0))
                            .clicked();
                            if clicked
                                && !selected
                                && let Some(sync) = state.sync.as_mut()
                            {
                                sync.change_member_role(
                                    dialog.remote_space,
                                    member.identity_pk,
                                    role,
                                );
                            }
                        }
                        if enkr_button(
                            ui,
                            &format!("Remove###enkr_member_remove_{short}"),
                            Some("Revoke this identity's access"),
                            BtnVariant::Danger,
                        )
                        .height(ui, UISize::Pixels(24.0))
                        .clicked()
                            && let Some(sync) = state.sync.as_mut()
                        {
                            sync.uninvite(dialog.remote_space, member.identity_pk);
                        }
                    } else {
                        ui.label(role_label(member.role))
                            .text_color(ui, theme.text_muted)
                            .font_size(ui, theme.size_text - 1.0);
                    }
                });
                row.width(ui, UISize::ParentPct(1.0))
                    .height(ui, UISize::Pixels(30.0))
                    .align(ui, MainAxisAlign::Start, CrossAxisAlign::Center)
                    .gap(ui, theme.gap_sm);
            }
        },
    );
    if !(close || chrome_close) {
        state.share_dialog = Some(dialog);
    }
}

/// The recovery-phrase surfaces: reveal, and restore.
///
/// The phrase is the only thing that can ever read this identity's synced notes —
/// the relay holds ciphertext and nothing else — so the copy here is blunt on
/// purpose. There is no reset link and no support route; if it is lost, the
/// content is gone.
pub(crate) fn render_recovery_dialog(ui: &mut IMUI, state: &mut EnkrState) {
    let Some(dialog) = state.recovery.as_mut() else {
        return;
    };
    let theme = *ui.theme();
    let width = 460.0;

    match dialog {
        RecoveryDialog::Reveal { phrase, first_run } => {
            let first_run = *first_run;
            let mut phrase_text = phrase.clone();
            let mut acknowledged = false;
            // No close cross on the first run: the point of the prompt is that
            // it is read, and a cross is the fastest way past it.
            let chrome_close = modal_frame(
                ui,
                "###enkr_recovery_window",
                "Your recovery phrase",
                width,
                320.0,
                |ui| {
                    ui.label(
                        "These twelve words are the only thing that can read your synced \
                         notes. Write them down and keep them somewhere safe.",
                    )
                    .width(ui, UISize::ParentPct(1.0))
                    .text_color(ui, theme.text_muted)
                    .font_size(ui, theme.size_text - 1.0);
                    let words = ui.textarea_with_options(
                        "###enkr_recovery_phrase",
                        &mut phrase_text,
                        TextAreaOptions::new()
                            .wrap_x(true)
                            .scroll_x(false)
                            .scroll_y(false)
                            .read_only(true)
                            .padding(Padding::all(10.0)),
                    );
                    words
                        .width(ui, UISize::ParentPct(1.0))
                        .height(ui, UISize::Pixels(72.0));
                    ui.label(
                        "Anyone with these words can read your notes. We cannot recover \
                         them for you, and losing them means losing the notes.",
                    )
                    .width(ui, UISize::ParentPct(1.0))
                    .text_color(ui, Color::new("#e0a052"))
                    .font_size(ui, theme.size_text - 1.0);
                    if enkr_button(
                        ui,
                        if first_run {
                            "I have written it down###enkr_recovery_ack"
                        } else {
                            "Done###enkr_recovery_ack"
                        },
                        None,
                        BtnVariant::Primary,
                    )
                    .clicked()
                    {
                        acknowledged = true;
                    }
                },
            );
            if acknowledged || (chrome_close && !first_run) {
                state.acknowledge_recovery_phrase();
                state.recovery = None;
            }
        }
        RecoveryDialog::Restore {
            input,
            error,
            confirmed_overwrite,
        } => {
            let mut typed = input.clone();
            let mut submit = false;
            let mut cancel = false;
            let mut confirm_overwrite = false;
            let already_confirmed = *confirmed_overwrite;
            let shown_error = error.clone();
            let chrome_close = modal_frame(
                ui,
                "###enkr_recovery_restore_window",
                "Restore from a recovery phrase",
                width,
                300.0,
                |ui| {
                    ui.label(
                        "Type the twelve words from another installation of this app. This \
                         installation will use that identity; installations using it share \
                         permissions and authorship. The change takes effect after a restart.",
                    )
                    .width(ui, UISize::ParentPct(1.0))
                    .text_color(ui, theme.text_muted)
                    .font_size(ui, theme.size_text - 1.0);
                    ui.line_edit("###enkr_recovery_input", &mut typed, false)
                        .width(ui, UISize::ParentPct(1.0));
                    ui.label(
                        "Replacing this installation's identity removes it from every space \
                         its old identity was invited to. Those invitations cannot be recovered.",
                    )
                    .width(ui, UISize::ParentPct(1.0))
                    .text_color(ui, Color::new("#e0a052"))
                    .font_size(ui, theme.size_text - 1.0);
                    if let Some(error) = shown_error.as_deref() {
                        ui.label(error)
                            .width(ui, UISize::ParentPct(1.0))
                            .text_color(ui, Color::new("#e05252"))
                            .font_size(ui, theme.size_text - 1.0);
                    }
                    let buttons = ui.row(|ui| {
                        let label = if already_confirmed {
                            "Replace my identity###enkr_recovery_restore"
                        } else {
                            "Restore###enkr_recovery_restore"
                        };
                        if enkr_button(ui, label, None, BtnVariant::Primary)
                            .width(ui, UISize::Fill)
                            .clicked()
                        {
                            if already_confirmed {
                                submit = true;
                            } else {
                                confirm_overwrite = true;
                            }
                        }
                        if enkr_button(
                            ui,
                            "Cancel###enkr_recovery_cancel",
                            None,
                            BtnVariant::Secondary,
                        )
                        .width(ui, UISize::Fill)
                        .clicked()
                        {
                            cancel = true;
                        }
                    });
                    buttons
                        .width(ui, UISize::ParentPct(1.0))
                        .height(ui, UISize::Pixels(32.0))
                        .gap(ui, theme.gap_sm);
                },
            );

            if cancel || chrome_close {
                state.recovery = None;
                return;
            }
            // Two presses, not a second dialog: the first turns the button into
            // what it will actually do, so the destructive action is named
            // before it happens.
            if confirm_overwrite {
                if let Some(RecoveryDialog::Restore {
                    input,
                    confirmed_overwrite,
                    error,
                }) = state.recovery.as_mut()
                {
                    *input = typed;
                    *error = None;
                    *confirmed_overwrite = true;
                }
                return;
            }
            if submit {
                match state.restore_recovery_phrase(&typed, true) {
                    Ok(()) => state.recovery = None,
                    Err(err) => {
                        if let Some(RecoveryDialog::Restore { input, error, .. }) =
                            state.recovery.as_mut()
                        {
                            *input = typed;
                            *error = Some(err);
                        }
                    }
                }
                return;
            }
            if let Some(RecoveryDialog::Restore { input, .. }) = state.recovery.as_mut() {
                *input = typed;
            }
        }
    }
}
