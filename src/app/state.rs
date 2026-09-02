//! `EnkrState` — everything the app knows between frames, and the operations
//! that mutate it: note/space/folder CRUD, sync wiring, import/export, and
//! session restore. The render tree reads this; it never reads the tree.

use crate::app::*;

/// The active sync server (the one the single live connection dials). Written
/// only on an explicit connect, so it also gates startup autoconnect — a fresh
/// install never auto-dials the default server.
pub(crate) const META_SERVER_URL: &str = "sync_server_url";
/// Newline-joined list of user-added custom servers. The default server is
/// always implied (and non-deletable), so it isn't stored here.
pub(crate) const META_SERVERS: &str = "sync_servers";
pub(crate) const META_NICKNAME: &str = "sync_nickname";
/// Prefix for the per-server account token (`sync_token:<url>`). Per server
/// because a token is minted by one relay and meaningless to any other, and a
/// user may pay for one relay while collaborating for free on another.
pub(crate) const META_TOKEN_PREFIX: &str = "sync_token:";
/// Set once the user has been shown their recovery phrase and confirmed they
/// wrote it down, so the first-connect prompt appears exactly once.
pub const META_RECOVERY_ACKED: &str = "recovery_phrase_acknowledged";

/// Hardcoded default sync server, pre-filled and non-deletable in the server
/// list (PLAN-account.md §2 "Synced @ default server").
pub(crate) const DEFAULT_SERVER: &str = "wss://xark.es/enkr/ws";
/// Local-only session memory: the last opened note and its caret position, so
/// the app reopens where you left off.
pub(crate) const META_LAST_NOTE: &str = "last_note_id";
pub(crate) const META_LAST_CURSOR: &str = "last_note_cursor";
/// Set once the user has been through the welcome screen. Absent = first run.
pub(crate) const META_ONBOARDED: &str = "onboarded";

/// The active destination.
///
/// A View owns the body. `Editor` and `Image` are *document* views — they keep
/// the sidebar and breadcrumb, because you reach an image by clicking a sidebar
/// row and need that row to still be there afterwards. Settings and Welcome
/// (P5) replace the chrome entirely.
#[derive(Clone, PartialEq, Debug)]
pub(crate) enum View {
    Editor,
    /// Viewing an image blob by name, in place of the editor.
    Image(String),
    /// Settings, as a full-window destination with its own category rail.
    Settings(SettingsSection),
    /// First-run choices. Also reachable again from Settings → General.
    Welcome,
}

impl View {
    /// Whether this view keeps the sidebar and top bar. The document views do;
    /// Settings and Welcome own the whole window.
    pub(crate) fn has_chrome(&self) -> bool {
        matches!(self, View::Editor | View::Image(_))
    }

    /// The blob name being viewed, if any.
    pub(crate) fn image(&self) -> Option<&str> {
        match self {
            View::Image(name) => Some(name),
            _ => None,
        }
    }
}

/// A sidebar row being renamed in place.
pub(crate) struct InlineEdit {
    pub(crate) target: RenameTarget,
    pub(crate) buffer: String,
    /// Focus the field on the next frame (set when the rename starts).
    pub(crate) focus_pending: bool,
    /// Select the whole name, one frame *after* focusing.
    ///
    /// The field has no text-edit state on the frame it is created, so setting
    /// a selection then is silently dropped — it has to wait for the frame
    /// after.
    pub(crate) select_pending: bool,
}

/// Which of the three first-run answers the welcome screen is showing.
///
/// A picker rather than one long page: the three are alternatives, and stacking
/// them vertically read as a checklist — people scrolled past "start offline"
/// looking for the rest of the setup. Naming all three side by side makes the
/// choice visible while the body only ever carries one.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum WelcomeTab {
    Offline,
    Online,
    Import,
}

/// The recovery-phrase surfaces. Two modes rather than two dialogs: they share
/// a frame and only one can be open at a time.
pub(crate) enum RecoveryDialog {
    /// Showing the phrase. `first_run` is the one-time prompt after the first
    /// connect, which requires an acknowledgement rather than a close button —
    /// the whole point is that it is not dismissed without being read.
    Reveal { phrase: String, first_run: bool },
    /// Typing a phrase in to rebuild this device's identity.
    Restore {
        input: String,
        error: Option<String>,
        /// Set once the user has confirmed replacing an existing identity.
        confirmed_overwrite: bool,
    },
}

/// Where the caret should land after "New note".
///
/// A new note is nameless, and naming it is the first thing anyone does — but
/// the title lives in the top bar, so it had to be found and clicked before it
/// could be typed. Focus goes there first and hands off to the body once the
/// name is settled, so the whole "make a note called X and write in it" gesture
/// is keyboard-only.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum NewNoteFocus {
    /// Nothing pending — normal editing.
    Idle,
    /// Focus the title field on the next frame.
    Title,
    /// Title is focused; select what is there so typing replaces it. Deferred a
    /// frame because the field has no edit state to select within until it has
    /// been built once (same reason as [`InlineEdit::select_pending`]).
    SelectTitle,
    /// Title focused and selected; waiting for Enter or Escape to settle it.
    Naming,
    /// The name is settled (Enter or Escape) — move to the body.
    Body,
}

/// What an [`InlineEdit`] is renaming.
#[derive(Clone, PartialEq)]
pub(crate) enum RenameTarget {
    Space(i64),
    Folder(Uuid),
    /// Blobs are addressed by name, which is what `./blob/<name>` links use.
    Blob(String),
}

pub struct EnkrState {
    pub side_width: f32,
    pub notes: NoteDatabase,
    /// This frame's note summaries, refilled once at the top of [`render`] and
    /// read by the sidebar. Retained so the per-frame rebuild reuses its
    /// allocations instead of making a fresh `Vec` of `String`s every frame.
    pub(crate) summaries: Vec<NoteSummary>,
    pub active_note_id: String,
    pub(crate) active_space_id: i64,
    pub(crate) theme_kind: ThemeKind,
    /// Wrap long editor lines instead of scrolling them horizontally.
    pub(crate) wrap_x: bool,
    pub(crate) splitter_drag_offset: f32,
    pub(crate) last_persistence_error: Option<String>,

    // -- synchronization ----------------------------------------------------
    pub sync: Option<AppSync>,
    pub(crate) waker: Option<RepaintWaker>,
    /// Test hook: where the device identity lives. Defaults to a flat key
    /// file next to the note database (the only persistent sync state).
    pub sync_identity: Option<IdentityStore>,
    /// Omit the hardcoded default server from the picker. Set by demo runs
    /// (`--showcase`) that must not advertise the public endpoint.
    pub hide_default_server: bool,
    /// Is the sidebar showing as a drawer over the content? Only meaningful
    /// on a viewport too narrow to hold it beside the content (see
    /// `NARROW_WIDTH`); ignored, and reset, above that width. Not persisted —
    /// a drawer is a gesture, not a setting.
    pub(crate) drawer_open: bool,
    /// Skips the outside-press dismissal test on the frame the drawer opens,
    /// when the press that opened it is still in the queue. Same guard the
    /// context menu uses (`MenuState::armed`).
    pub(crate) drawer_armed: bool,
    /// Servers to offer on top of the persisted list. Set by demo and test
    /// runs that have to reach a relay the UI itself cannot add — the web
    /// build offers no "add a server" field at all (see [`Self::add_server`]),
    /// so `enkr/src/bin/test_harness.rs` has no other way to point a browser
    /// client at an in-process `enkr-syncd`. Not persisted: whatever set it
    /// sets it again next launch.
    pub extra_servers: Vec<String>,
    /// The server the live connection dials. Defaults to the persisted active
    /// server, else [`DEFAULT_SERVER`]. Each space is bound to its own server;
    /// only spaces bound to this one sync.
    pub active_server: String,
    /// "Add server…" text field in Settings.
    pub add_server_input: String,
    /// Account-token text field for the active server. Not persisted itself —
    /// committed to `META_TOKEN_PREFIX` when the user saves it.
    pub token_input: String,
    /// Which server `token_input` currently holds the token for, so switching
    /// servers reseeds the field instead of showing another relay's token.
    pub(crate) token_input_server: String,
    pub nickname_input: String,
    /// Connect on the first frame when a server URL was persisted.
    pub(crate) sync_autoconnect: bool,
    /// Layer 1: the open context menu, if any.
    pub(crate) menu: Option<MenuState>,
    /// When set, the content area shows this image (blob name, in the active
    /// space) as a full viewer instead of the note editor.
    /// The active destination. Views are navigation, not transient surfaces:
    /// no scrim, no click-away, and `Escape` reaches them only after every
    /// layer above has been dismissed.
    pub(crate) view: View,
    /// Where `Escape` / the back button returns to. Bounded: a back-stack this
    /// shallow never needs to grow, and an unbounded one would just leak.
    pub(crate) view_stack: Vec<View>,
    /// Fade-in progress of the current view, `0.0..=1.0`.
    ///
    /// Held here rather than retained by the toolkit: a keyed store touched
    /// every frame is exactly what the hot-path rule forbids, and one `f32` on
    /// the app is all this needs. Reset to 0 on every view change and eased
    /// toward 1 with `IMUI::dt`.
    pub(crate) view_fade: f32,
    /// In-progress drag-and-drop of a sidebar note/folder; `None` when idle.
    pub(crate) drag: Option<DragState>,
    /// A sidebar row being renamed in place.
    ///
    /// Replaces three near-identical modal dialogs. Renaming in the row you are
    /// looking at is how Finder, VS Code and every file tree does it — a modal
    /// for a single text field is ceremony, and it hides the thing you are
    /// renaming behind itself.
    pub(crate) inline_edit: Option<InlineEdit>,
    /// Focus hand-off for a freshly created note; see [`NewNoteFocus`].
    pub(crate) new_note_focus: NewNoteFocus,
    pub(crate) share_dialog: Option<ShareDialog>,
    /// Open folder picker (Mae's file explorer), used to choose a directory for
    /// import/export. `None` when no picker is showing.
    pub(crate) file_explorer: Option<FileExplorer>,
    /// What the open `file_explorer` will do on confirm. Only meaningful while
    /// `file_explorer` is `Some`.
    pub(crate) file_pick_mode: FilePickMode,
    /// Remote space id awaiting a "delete for everyone" confirmation, opened
    /// from the sync window for spaces this device owns.
    pub(crate) delete_space_confirm: Option<Uuid>,
    /// The open recovery-phrase dialog, if any.
    pub(crate) recovery: Option<RecoveryDialog>,
    pub(crate) welcome_tab: WelcomeTab,
    /// 0..1 cross-fade for the welcome body, restarted on every tab change so
    /// the switch reads as one panel replacing another rather than a jump.
    pub(crate) welcome_fade: f32,
    /// Where this device's seed lives, once a connection has resolved it.
    /// Needed to read the phrase back and to restore one.
    pub identity_store: Option<IdentityStore>,
    /// The active note's editor box from the previous frame, used to anchor
    /// the caret across remote edits (the key is stable frame-to-frame).
    pub(crate) editor_handle: Option<(String, UIBoxHandle)>,
    /// Local search palette; `None` when closed. Owns the background search
    /// worker, so closing the palette tears the worker down.
    pub(crate) search: Option<PaletteState>,
    /// A pending caret jump `(note_id, char_offset)` queued by selecting a
    /// search result; applied once that note's editor is built (possibly next
    /// frame, after switching notes).
    pub(crate) pending_jump: Option<(String, usize)>,
    /// Blob names awaiting a `./blob/<name>` link insertion at the editor caret
    /// (queued by the toolbar "Insert image" action; drained by `image_pump`).
    pub(crate) pending_image_inserts: Vec<String>,
    /// A file picked via the browser's native file-open dialog, awaiting
    /// `image_pump` to turn it into a blob + inserted link. Written from a
    /// detached async task (`insert_image_from_file`'s `change` listener),
    /// which can't reach `&mut EnkrState` directly — same `Rc<RefCell<...>>`
    /// handoff idiom as `note.rs`'s `NoteStoreHandle::error`.
    #[cfg(target_arch = "wasm32")]
    pub(crate) picked_image: std::rc::Rc<std::cell::RefCell<Option<(String, Vec<u8>)>>>,
    /// Live caret position of the active note's editor, captured each frame so
    /// it can be persisted (last-session memory) without the UI at shutdown.
    pub(crate) active_cursor: usize,
    /// In-progress edit of the active note's title (the editable top label),
    /// held across frames as `(note_id, buffer)` so erasing doesn't snap back to
    /// the stored title. Reseeded from the note when the active note changes.
    pub(crate) title_edit: Option<(String, String)>,
    /// In-progress edit of the viewed image's filename (the editable top label
    /// while an image is the content view), held across frames as
    /// `(blob_id, buffer)`. Reseeded when the viewed blob changes.
    pub(crate) blob_title_edit: Option<(String, String)>,
    /// `--demo` only: overlay fake collaborator carets on the active note so the
    /// remote-caret badge and hover name tag can be checked without a second
    /// client or a server. Debug builds only.
    #[cfg(debug_assertions)]
    pub(crate) demo_presence: bool,
}

impl EnkrState {
    #[cfg(not(target_arch = "wasm32"))]
    pub fn new() -> Self {
        let db_path = default_database_path();
        let notes = NoteDatabase::open(&db_path).unwrap_or_else(|err| {
            eprintln!(
                "Could not open note database at {}: {err}. Falling back to in-memory notes.",
                db_path.display()
            );
            NoteDatabase::new_in_memory()
        });
        Self::with_notes(notes)
    }

    /// wasm32 counterpart to `new` — necessarily async (see `NoteDatabase::
    /// open_wasm`'s doc comment). The wasm entry point awaits this once at
    /// startup before the first frame builds.
    #[cfg(target_arch = "wasm32")]
    pub async fn new_wasm() -> Self {
        let notes = NoteDatabase::open_wasm("enkr").await.unwrap_or_else(|err| {
            log::error!(
                "Could not open IndexedDB note database: {err}. Falling back to in-memory notes."
            );
            NoteDatabase::new_in_memory()
        });
        Self::with_notes(notes)
    }

    pub fn with_notes(notes: NoteDatabase) -> Self {
        // A database that has never been through onboarding opens on the
        // welcome screen. `NoteDatabase::demo()` marks itself onboarded, so
        // fixtures and the test harness land in the editor as before — only a
        // genuinely fresh install sees this.
        let view = if Self::is_onboarded(&notes) {
            View::Editor
        } else {
            View::Welcome
        };
        // Reopen the last note from the previous session if it still exists,
        // otherwise fall back to the first note.
        let restored = notes
            .meta_get(META_LAST_NOTE)
            .filter(|id| notes.contains(id))
            .map(str::to_string);
        let active_note_id = restored
            .clone()
            .or_else(|| notes.first_note_id().map(str::to_string))
            .unwrap_or_default();
        // Show the restored note's space, not the default one.
        let active_space_id = notes
            .note(&active_note_id)
            .map(|note| note.space_id())
            .unwrap_or_else(|| notes.default_space_id());
        // Restore the caret into that note on the first frame. Reuses the
        // search jump path, which clamps to the current text length so a
        // shorter (e.g. post-sync) document keeps a valid position.
        let restored_cursor = restored
            .as_ref()
            .and_then(|_| notes.meta_get(META_LAST_CURSOR))
            .and_then(|c| c.parse::<usize>().ok());
        let pending_jump = restored
            .clone()
            .zip(restored_cursor)
            .map(|(id, cursor)| (id, cursor));
        let persisted_active = notes.meta_get(META_SERVER_URL).unwrap_or("").to_string();
        let nickname_input = notes.meta_get(META_NICKNAME).unwrap_or("").to_string();
        // Autoconnect only when a server was explicitly connected before (the
        // persisted active server is set); a fresh install stays offline.
        let sync_autoconnect = !persisted_active.is_empty();
        let active_server = if persisted_active.is_empty() {
            DEFAULT_SERVER.to_string()
        } else {
            persisted_active
        };
        Self {
            side_width: 260.0,
            summaries: Vec::new(),
            notes,
            active_note_id,
            active_space_id,
            theme_kind: ThemeKind::Light,
            wrap_x: true,
            splitter_drag_offset: 0.0,
            last_persistence_error: None,
            sync: None,
            waker: None,
            sync_identity: None,
            drawer_open: false,
            drawer_armed: false,
            hide_default_server: false,
            extra_servers: Vec::new(),
            active_server,
            add_server_input: String::new(),
            token_input: String::new(),
            token_input_server: String::new(),
            nickname_input,
            sync_autoconnect,
            menu: None,
            view,
            view_stack: Vec::new(),
            view_fade: 1.0,
            drag: None,
            inline_edit: None,
            new_note_focus: NewNoteFocus::Idle,
            share_dialog: None,
            file_explorer: None,
            file_pick_mode: FilePickMode::Import,
            delete_space_confirm: None,
            recovery: None,
            welcome_tab: WelcomeTab::Offline,
            welcome_fade: 1.0,
            identity_store: None,
            editor_handle: None,
            search: None,
            pending_jump,
            pending_image_inserts: Vec::new(),
            #[cfg(target_arch = "wasm32")]
            picked_image: std::rc::Rc::new(std::cell::RefCell::new(None)),
            active_cursor: 0,
            title_edit: None,
            blob_title_edit: None,
            #[cfg(debug_assertions)]
            demo_presence: false,
        }
    }

    /// Enable the `--demo` fake-collaborator overlay (see [`Self::demo_presence`]).
    #[cfg(debug_assertions)]
    pub fn set_demo_presence(&mut self, on: bool) {
        self.demo_presence = on;
    }

    /// Delete a folder locally and propagate the deletion into the space's
    /// index doc when the space is synced.
    pub(crate) fn delete_folder(&mut self, space_id: i64, folder_id: Uuid) {
        if let Some(sync) = self.sync.as_mut() {
            sync.folder_deleted(&self.notes, space_id, folder_id);
        }
        self.notes.delete_folder(&folder_id);
    }

    /// Delete an image blob locally and propagate the deletion into the space's
    /// index doc when the space is synced, so peers and this device after a
    /// restart stop re-downloading it.
    pub(crate) fn delete_blob(&mut self, blob_id: &str) {
        if let Some(sync) = self.sync.as_mut()
            && let Some(blob) = self.notes.blob(blob_id)
            && let Ok(id) = Uuid::parse_str(blob_id)
        {
            sync.blob_deleted(&self.notes, blob.space_id, id);
        }
        self.notes.delete_blob(blob_id);
    }

    pub fn set_repaint_waker(&mut self, waker: RepaintWaker) {
        self.waker = Some(waker);
    }

    /// Boot the sync engine against the configured server. Persists the
    /// settings so the next launch reconnects automatically.
    pub fn connect_sync(&mut self) {
        if self.sync.is_some() {
            // An engine that has given up is not a connection in progress. It
            // never retries by design (a refused token or a wire-version
            // mismatch cannot start working on its own), so returning here left
            // Connect doing nothing at all while the UI still said
            // "Connecting…" — no error, no way out, no way to try another
            // server. Replace it instead.
            if !self.sync_is_dead() {
                return;
            }
            self.sync = None;
        }
        let Some(waker) = self.waker.clone() else {
            return;
        };
        let active = self.active_server.trim().to_string();
        let url = normalize_server_url(&active);
        let Some(url) = url else {
            return;
        };
        let nickname = self.nickname_input.trim().to_string();
        // Persist the active server (this also arms startup autoconnect).
        self.notes.meta_set(META_SERVER_URL, &active);
        self.notes.meta_set(META_NICKNAME, &nickname);
        // Persisted across launches on both targets: a real key file
        // natively, `localStorage` in the browser (there is no filesystem
        // for `IdentityStore::Path` to use there — `default_device_key_
        // path()` would degrade to a meaningless relative path via
        // `platform_config_dir`'s fallback, and `std::fs::read` on
        // wasm32-unknown-unknown always fails). The device key *is* this
        // device's membership in every space it has been admitted to, so
        // losing it on reload silently orphans the device — see
        // `sync/identity.rs`.
        let identity = self.sync_identity.clone().unwrap_or_else(|| {
            #[cfg(not(target_arch = "wasm32"))]
            {
                IdentityStore::Path(default_device_key_path())
            }
            #[cfg(target_arch = "wasm32")]
            {
                IdentityStore::LocalStorage(DEVICE_KEY_STORAGE_KEY.to_string())
            }
        });
        let mut config = SyncConfig::new(url, identity.clone());
        config.account_token = self.account_token(&active);
        match AppSync::start(config, nickname, waker) {
            Ok(mut sync) => {
                sync.adopt(&self.notes);
                self.sync = Some(sync);
                self.identity_store = Some(identity);
                // The device key is created by that call, and from here on it is
                // the only thing that can read anything synced. Prompt once,
                // now — not at first launch, when a local-only install has no
                // identity yet and nothing at stake.
                if self.notes.meta_get(META_RECOVERY_ACKED).is_none() {
                    self.open_recovery_phrase(true);
                }
            }
            Err(err) => self.record_persistence_error(format!("sync: {err}")),
        }
    }

    /// Start a connection because the user explicitly asked for one, replacing
    /// whatever engine is there.
    ///
    /// The distinction from [`Self::connect_sync`] matters: that one refuses to
    /// duplicate a live engine, which is right for autoconnect but wrong for a
    /// button press. An engine stuck in a retry loop — the relay is down, the
    /// proxy is answering 502, the host does not resolve — is not connected and
    /// not dead, so the guard silently swallowed the click and left the user
    /// watching "Connecting…" with no way to point the app somewhere else.
    pub(crate) fn reconnect_sync(&mut self) {
        self.sync = None;
        self.connect_sync();
    }

    /// The sync engine exists but has stopped for good: the relay refused this
    /// device's credentials, or speaks a protocol this build cannot talk.
    ///
    /// Both are terminal by design — the engine deliberately stops retrying,
    /// because asking again with the same token or the same build cannot
    /// succeed. What they need is a *new* engine pointed somewhere else, so
    /// callers must not mistake this for "still connecting".
    pub fn sync_is_dead(&self) -> bool {
        self.sync.as_ref().is_some_and(|sync| {
            !sync.connected() && (sync.rejected() || sync.incompatible().is_some())
        })
    }

    /// Stop synchronizing. Unacknowledged edits stay flagged `needs_push`
    /// in the note database and reship on the next connect.
    pub fn disconnect_sync(&mut self) {
        self.sync = None;
    }

    /// The selectable sync servers: the non-deletable default first, then any
    /// user-added custom servers (deduped).
    ///
    /// Web build only ever offers the hardcoded default — no custom relay
    /// picking from a browser tab (see `add_server`'s doc comment for why).
    pub fn server_list(&self) -> Vec<String> {
        let mut servers = Vec::new();
        if !self.hide_default_server {
            servers.push(DEFAULT_SERVER.to_string());
        }
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(custom) = self.notes.meta_get(META_SERVERS) {
            for url in custom.lines() {
                let url = url.trim();
                if !url.is_empty() && !servers.iter().any(|s| s == url) {
                    servers.push(url.to_string());
                }
            }
        }
        for url in &self.extra_servers {
            let url = url.trim();
            if !url.is_empty() && !servers.iter().any(|s| s == url) {
                servers.push(url.to_string());
            }
        }
        servers
    }

    /// The paying-account token for `server`, if one has been entered. `None`
    /// is the normal case: self-hosted relays ask for nothing, and a device
    /// invited into someone else's space is billed to that space's owner.
    pub(crate) fn account_token(&self, server: &str) -> Option<String> {
        self.notes
            .meta_get(&format!("{META_TOKEN_PREFIX}{}", server.trim()))
            .map(str::to_string)
            .filter(|token| !token.is_empty())
    }

    /// The connected server's spaces, resolved for display.
    ///
    /// Mutating because naming a space that is only on the server requires a
    /// *peek* (decrypting its index far enough to read the name — the server
    /// itself never sees it), and knowing whether Delete should be offered
    /// requires its membership. Both are requests, so this is not a pure read.
    ///
    /// Empty when nothing is connected: only one connection runs at a time, so
    /// there is no honest listing for any other server.
    pub(crate) fn remote_space_rows(&mut self) -> Vec<RemoteRow> {
        let Some(sync) = self.sync.as_mut() else {
            return Vec::new();
        };
        let spaces = sync.remote_spaces(&self.notes);
        spaces
            .iter()
            .map(|remote| {
                let name = remote
                    .local
                    .and_then(|local| self.notes.space_name(local))
                    .map(str::to_string)
                    .or_else(|| sync.remote_space_name(remote.space_id).map(str::to_string));
                if name.is_none() {
                    sync.peek_space(remote.space_id);
                }
                // Membership comes from the server, not the local mirror, so
                // this resolves for any space the device belongs to — an owner
                // can delete a space they never synced here.
                sync.members(remote.space_id);
                RemoteRow {
                    space_id: remote.space_id,
                    local: remote.local,
                    name,
                    is_owner: sync.can_admin(remote.space_id),
                }
            })
            .collect()
    }

    /// Reseed the account-token field when the active server changed. Called
    /// from the settings view, which is the only place the field is shown.
    pub(crate) fn sync_token_field(&mut self) {
        if self.token_input_server != self.active_server {
            self.token_input_server = self.active_server.clone();
            self.token_input = self.account_token(&self.active_server).unwrap_or_default();
        }
    }

    /// Store (or clear, when blank) the account token for `server`. Dropping a
    /// live connection so the next connect re-authenticates with it.
    pub(crate) fn set_account_token(&mut self, server: &str, token: &str) {
        let key = format!("{META_TOKEN_PREFIX}{}", server.trim());
        let token = token.trim();
        if self.account_token(server).as_deref().unwrap_or("") == token {
            return;
        }
        self.notes.meta_set(&key, token);
        if self.active_server == server {
            self.disconnect_sync();
        }
    }

    /// Add a custom server to the persisted list (no-op if blank, the default,
    /// or already present).
    ///
    /// A no-op entirely on wasm32: the web build only ever talks to the
    /// hardcoded `DEFAULT_SERVER` (`server_list` never surfaces anything
    /// else there either) — an untrusted page pointing a browser tab's sync
    /// engine at an arbitrary relay isn't a capability the web build offers.
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn add_server(&mut self, url: &str) {
        let url = url.trim();
        if url.is_empty() || self.server_list().iter().any(|s| s == url) {
            return;
        }
        let mut custom: Vec<String> = self
            .notes
            .meta_get(META_SERVERS)
            .map(|s| s.lines().map(str::to_string).collect())
            .unwrap_or_default();
        custom.push(url.to_string());
        self.notes.meta_set(META_SERVERS, &custom.join("\n"));
    }
    #[cfg(target_arch = "wasm32")]
    pub(crate) fn add_server(&mut self, _url: &str) {}

    /// Remove a custom server (the default server can't be removed).
    pub(crate) fn remove_server(&mut self, url: &str) {
        if url == DEFAULT_SERVER {
            return;
        }
        let custom: Vec<String> = self
            .notes
            .meta_get(META_SERVERS)
            .map(|s| {
                s.lines()
                    .map(str::trim)
                    .filter(|line| !line.is_empty() && *line != url)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        self.notes.meta_set(META_SERVERS, &custom.join("\n"));
    }

    /// Select `server` as the active one without connecting (the global
    /// Connect button does that). Drops any live connection to a different
    /// server since only one is active at a time.
    pub(crate) fn select_server(&mut self, server: String) {
        if self.active_server != server {
            self.active_server = server;
            self.disconnect_sync();
        }
    }

    /// Make `server` the active connection (reconnecting if it changed) and
    /// connect if needed. Used when promoting a space to sync.
    pub(crate) fn activate_server(&mut self, server: String) {
        if self.active_server != server {
            self.active_server = server;
            self.disconnect_sync();
        }
        if self.sync.is_none() {
            self.connect_sync();
        }
    }

    /// Promote a space to sync against `server` (PLAN-account.md §6 "Sync this
    /// space…"): bind the space, switch the active connection to that server if
    /// needed, then push. A space is bound to exactly one server and never
    /// re-pushed to another.
    pub(crate) fn sync_space_to_server(&mut self, space_id: i64, server: String) {
        self.notes.set_space_server(space_id, Some(server.clone()));
        self.activate_server(server);
        if let Some(sync) = self.sync.as_mut() {
            sync.push_space(&self.notes, space_id);
        }
    }

    /// Create a space and immediately bind it to the active server.
    ///
    /// The welcome screen's "create a space" is a promise that what you write
    /// next ends up on the server you just connected to, so the binding happens
    /// here rather than leaving the user to find "Sync this space…" afterwards.
    pub(crate) fn create_synced_space(&mut self) {
        self.create_space_and_select();
        let (space_id, server) = (self.active_space_id, self.active_server.clone());
        self.sync_space_to_server(space_id, server);
    }

    pub(crate) fn ensure_sync_started(&mut self) {
        if self.sync.is_none() && self.sync_autoconnect && self.waker.is_some() {
            self.sync_autoconnect = false;
            self.connect_sync();
        }
    }

    pub(crate) fn ensure_active_note(&mut self) {
        // The active space can vanish out from under us (e.g. a synced space
        // deleted by its owner arrives through the sync pump): fall back to the
        // first remaining space so the sidebar never points at a dead id.
        if !self
            .notes
            .spaces()
            .iter()
            .any(|space| space.id == self.active_space_id)
        {
            self.active_space_id = self.notes.default_space_id();
        }

        if self.notes.contains(&self.active_note_id) {
            return;
        }

        self.active_note_id = self
            .notes
            .first_note_id()
            .map(str::to_string)
            .unwrap_or_default();
    }

    pub(crate) fn create_note_and_select(&mut self) {
        self.flush_note(&self.active_note_id.clone());
        self.active_note_id = self.notes.create_note_in(self.active_space_id);
        self.new_note_focus = NewNoteFocus::Title;
    }

    pub(crate) fn create_note_in_folder_and_select(&mut self, space_id: i64, folder_id: Uuid) {
        self.flush_note(&self.active_note_id.clone());
        let note_id = self.notes.create_note_in(space_id);
        self.notes.set_note_folder(&note_id, Some(folder_id));
        self.active_space_id = space_id;
        self.active_note_id = note_id;
        self.new_note_focus = NewNoteFocus::Title;
    }

    pub(crate) fn create_space_and_select(&mut self) {
        self.flush_note(&self.active_note_id.clone());
        let space_id = self.notes.create_space();
        // Drop a first note in the new space so it opens ready to edit.
        self.active_note_id = self.notes.create_note_in(space_id);
        self.active_space_id = space_id;
    }

    pub(crate) fn select_note(&mut self, note_id: String) {
        // Picking a note is the drawer's whole purpose, so it has done its job
        // — and on a narrow viewport it is covering the note that was just
        // opened. Unconditional: `drawer_open` is already false everywhere
        // else.
        self.drawer_open = false;
        if note_id == self.active_note_id {
            return;
        }
        self.flush_note(&self.active_note_id.clone());
        self.active_note_id = note_id;
    }

    /// Commit a sidebar drag-and-drop: move the dragged note/folder onto the
    /// dropped-on space or folder. Dropping on a space header moves the item to
    /// that space (or, when it's the item's own space, back to that space's
    /// root / top level).
    pub(crate) fn apply_drop(&mut self, item: DragItem, target: DropTarget) {
        if !drop_allowed(&self.notes, &item, target) {
            return;
        }
        match (item, target) {
            (DragItem::Note(id), DropTarget::Folder(folder)) => {
                self.notes.set_note_folder(&id, Some(folder));
            }
            (DragItem::Note(id), DropTarget::Space(space)) => {
                if self.notes.note(&id).is_some_and(|n| n.space_id() == space) {
                    self.notes.set_note_folder(&id, None);
                } else {
                    self.notes.move_note_to_space(&id, space);
                }
            }
            (DragItem::Folder(folder), DropTarget::Folder(parent)) => {
                self.notes.set_folder_parent(&folder, Some(parent));
            }
            (DragItem::Folder(folder), DropTarget::Space(space)) => {
                if self
                    .notes
                    .folder(&folder)
                    .is_some_and(|f| f.space_id == space)
                {
                    self.notes.set_folder_parent(&folder, None);
                } else {
                    self.notes.move_folder_to_space(&folder, space);
                }
            }
        }
    }

    pub(crate) fn delete_note(&mut self, note_id: &str) {
        let was_active = self.active_note_id == note_id;
        self.notes.delete_note(note_id);
        if was_active {
            self.active_note_id = self
                .notes
                .summaries()
                .into_iter()
                .find(|summary| summary.space_id == self.active_space_id)
                .map(|summary| summary.id)
                .or_else(|| self.notes.first_note_id().map(str::to_string))
                .unwrap_or_default();
        }
    }

    pub(crate) fn delete_space(&mut self, space_id: i64) {
        // Capture the remote binding before the local row is gone: a synced
        // space must also be forgotten by the sync runtime, or re-fetching it
        // later recreates the shell without re-pulling notes.
        let remote = self.notes.space_remote(space_id);
        if !self.notes.delete_space(space_id) {
            return;
        }
        if let (Some(remote), Some(sync)) = (remote, self.sync.as_mut()) {
            sync.space_deleted(remote);
        }
        if self.active_space_id == space_id {
            self.active_space_id = self.notes.default_space_id();
        }
        self.active_note_id = self
            .notes
            .summaries()
            .into_iter()
            .find(|summary| summary.space_id == self.active_space_id)
            .map(|summary| summary.id)
            .or_else(|| self.notes.first_note_id().map(str::to_string))
            .unwrap_or_default();
    }

    pub(crate) fn autosave_due(&mut self, ui: &mut IMUI) {
        if let Err(err) = self.notes.flush_due() {
            self.record_persistence_error(err.to_string());
        }
        if self.notes.has_dirty_notes() {
            ui.request_repaint();
        }
    }

    pub fn shutdown(&mut self) {
        // Not `disconnect_sync`: on the way out we wait for the engine, so it
        // gets to close the connection rather than having the socket yanked
        // out from under it when the process exits.
        if let Some(sync) = self.sync.take() {
            sync.shutdown();
        }
        self.persist_last_session();
        if let Err(err) = self.notes.flush_dirty() {
            self.record_persistence_error(err.to_string());
        }
    }

    /// Persist the last opened note + caret position (local-only) so the next
    /// launch reopens here. Write-through to the note store's writer thread,
    /// which drains queued writes before exiting.
    pub(crate) fn persist_last_session(&mut self) {
        if self.active_note_id.is_empty() {
            return;
        }
        self.notes
            .meta_set(META_LAST_NOTE, &self.active_note_id.clone());
        self.notes
            .meta_set(META_LAST_CURSOR, &self.active_cursor.to_string());
    }

    pub(crate) fn flush_note(&mut self, note_id: &str) {
        if let Err(err) = self.notes.flush_note(note_id) {
            self.record_persistence_error(err.to_string());
        }
    }

    /// Open a file picker, store the chosen image as a blob in the active
    /// space, and queue a `./blob/<name>` link insertion at the editor caret.
    ///
    /// Native only — `mae::os::open_image_file_dialog` is already a no-op on
    /// wasm32, and image decode (`normalize_image_for_storage`) is a
    /// base-app scope cut there too (see the wasm32 stub below).
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn insert_image_from_file(&mut self) {
        let Some(path) = mae::os::open_image_file_dialog() else {
            return;
        };
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(err) => {
                self.record_persistence_error(format!("could not read image: {err}"));
                return;
            }
        };
        let Some((data, mime)) = normalize_image_for_storage(&bytes) else {
            self.record_persistence_error("unsupported image format".to_string());
            return;
        };
        let base = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("image")
            .to_string();
        let name = if mime == ImageMime::Png && !base.to_lowercase().ends_with(".png") {
            format!("{base}.png")
        } else {
            base
        };
        let space_id = self.active_space_id;
        let id = self.notes.create_blob_in(space_id, &name, mime, data);
        if let Some(final_name) = self.notes.blob(&id).map(|b| b.name.clone()) {
            self.pending_image_inserts.push(final_name);
        }
        self.upload_blob_if_synced(&id);
    }

    /// wasm32 counterpart to the native `insert_image_from_file` above. A
    /// native OS file dialog blocks and returns a path synchronously; a
    /// browser file picker does neither (no filesystem, and `<input
    /// type=file>` only ever resolves via a `change` event on its own
    /// microtask) — so this only *opens* the picker. The chosen bytes land in
    /// `picked_image` (read by `image_pump`, mirroring how a pasted image
    /// already flows through `IMUI::take_pasted_image`) once the user
    /// actually picks a file, which may be frames later or never.
    #[cfg(target_arch = "wasm32")]
    pub(crate) fn insert_image_from_file(&mut self) {
        use wasm_bindgen::JsCast;
        use wasm_bindgen::closure::Closure;

        let Some(document) = web_sys::window().and_then(|w| w.document()) else {
            return;
        };
        let Ok(el) = document.create_element("input") else {
            return;
        };
        let Ok(input): Result<web_sys::HtmlInputElement, _> = el.dyn_into() else {
            return;
        };
        input.set_type("file");
        input.set_accept("image/png,image/jpeg");
        let _ = input.style().set_property("display", "none");
        let Some(body) = document.body() else { return };
        if body.append_child(&input).is_err() {
            return;
        }

        let slot = self.picked_image.clone();
        let waker = self.waker.clone();
        let input_for_listener = input.clone();
        let document_for_cleanup = document.clone();
        // Leaked on purpose: a one-shot listener for a user-triggered dialog,
        // not per-frame work — the input element (and this closure with it)
        // is removed from the document once a file is chosen, or otherwise
        // lives harmlessly off-screen for the rest of the session if the
        // user cancels the dialog instead.
        let onchange = Closure::<dyn FnMut()>::new(move || {
            let Some(file) = input_for_listener.files().and_then(|files| files.get(0)) else {
                return;
            };
            let name = file.name();
            let slot = slot.clone();
            let waker = waker.clone();
            let input_to_remove = input_for_listener.clone();
            let document_for_cleanup = document_for_cleanup.clone();
            wasm_bindgen_futures::spawn_local(async move {
                if let Ok(buf) = wasm_bindgen_futures::JsFuture::from(file.array_buffer()).await {
                    let bytes = js_sys::Uint8Array::new(&buf).to_vec();
                    *slot.borrow_mut() = Some((name, bytes));
                    if let Some(waker) = waker {
                        waker.wake();
                    }
                }
                if let Some(body) = document_for_cleanup.body() {
                    let _ = body.remove_child(&input_to_remove);
                }
            });
        });
        input.set_onchange(Some(onchange.as_ref().unchecked_ref()));
        onchange.forget();
        input.click();
    }

    /// Upload a blob's bytes to the server if its space is synced. The engine
    /// seals it; the `BlobUploaded` event clears `needs_push` on success.
    /// Show the recovery phrase. `first_run` marks the one-time prompt, which
    /// asks for an acknowledgement rather than offering a plain close.
    pub(crate) fn open_recovery_phrase(&mut self, first_run: bool) {
        let store = match self.identity_store.clone() {
            Some(store) => store,
            None => {
                self.last_persistence_error = Some(
                    "Connect to a sync server first — that is when the key is created.".into(),
                );
                return;
            }
        };
        match crate::sync::recovery_phrase(&store) {
            Ok(phrase) => self.recovery = Some(RecoveryDialog::Reveal { phrase, first_run }),
            Err(err) => self.last_persistence_error = Some(err),
        }
    }

    pub(crate) fn open_recovery_restore(&mut self) {
        self.recovery = Some(RecoveryDialog::Restore {
            input: String::new(),
            error: None,
            confirmed_overwrite: false,
        });
    }

    /// Record that the phrase has been seen, so the first-connect prompt does
    /// not reappear.
    pub(crate) fn acknowledge_recovery_phrase(&mut self) {
        self.notes.meta_set(META_RECOVERY_ACKED, "1");
    }

    /// Rebuild this device's identity from `phrase`.
    ///
    /// Takes effect on the next launch: the sync engine resolved the old
    /// identity when it started and there is no way to swap it underneath a
    /// live connection, so telling the user to restart is more honest than
    /// appearing to switch and then behaving as the old device.
    pub(crate) fn restore_recovery_phrase(
        &mut self,
        phrase: &str,
        overwrite: bool,
    ) -> Result<(), String> {
        let store = self
            .identity_store
            .clone()
            .ok_or("Connect to a sync server first — that is when the key is created.")?;
        crate::sync::restore_from_phrase(&store, phrase, overwrite)?;
        self.acknowledge_recovery_phrase();
        Ok(())
    }

    pub(crate) fn upload_blob_if_synced(&mut self, blob_id: &str) {
        let upload = {
            let Some(blob) = self.notes.blob(blob_id) else {
                return;
            };
            // Already stored and acked — this is a deduplicated paste of an
            // image the relay has. Re-uploading would be a no-op there
            // (`ON CONFLICT DO NOTHING`) but still costs the bytes on the wire.
            if !blob.needs_push {
                return;
            }
            let Some(remote) = self.notes.space_remote(blob.space_id) else {
                return;
            };
            let Ok(id) = Uuid::parse_str(blob_id) else {
                return;
            };
            (remote, id, blob.key, blob.bytes.clone())
        };
        if let Some(sync) = self.sync.as_mut() {
            sync.upload_blob(upload.0, upload.1, upload.2, upload.3);
        }
    }

    /// Open Mae's folder picker to choose a directory to import notes from.
    /// The picker carries an "import as a new space" checkbox.
    pub(crate) fn open_import_picker(&mut self) {
        self.file_pick_mode = FilePickMode::Import;
        self.file_explorer = Some(
            FileExplorer::folder_picker(default_import_path())
                .title("Import notes from folder")
                .confirm_label("Import this folder")
                .with_toggle("Import as a new space", false),
        );
    }

    /// Open Mae's folder picker to choose a directory to export notes to.
    pub(crate) fn open_export_picker(&mut self) {
        self.file_pick_mode = FilePickMode::Export;
        self.file_explorer = Some(
            FileExplorer::folder_picker(default_export_path())
                .title("Export notes to folder")
                .confirm_label("Export here"),
        );
    }

    /// Import every markdown note found under `root`, either into the active
    /// space or into a freshly created space named after the folder.
    pub(crate) fn import_notes_from(&mut self, root: PathBuf, as_new_space: bool) {
        let space_id = if as_new_space {
            let name = folder_display_name(&root);
            let space = self.notes.create_space_named(&name);
            self.active_space_id = space;
            space
        } else {
            self.active_space_id
        };
        match self.notes.import_folder_into(&root, space_id) {
            Ok(imported_ids) => {
                if let Some(id) = imported_ids.into_iter().next() {
                    self.active_note_id = id;
                }
            }
            Err(err) => self.record_persistence_error(err.to_string()),
        }
    }

    pub(crate) fn export_notes_to(&mut self, root: PathBuf) {
        if let Err(err) = self.notes.export_folder(&root) {
            self.record_persistence_error(err.to_string());
        }
    }

    /// Open the search palette for `scope` (Global = all notes, Document = the
    /// active note), or just retarget/refocus it if already open. Snapshots the
    /// relevant note text into a corpus handed to a fresh background worker, so
    /// the scan itself never touches the UI thread.
    pub(crate) fn open_search(&mut self, scope: SearchScope) {
        // Retarget an open *search* palette rather than stacking a second one.
        // A switcher or move-to palette is a different job, so Cmd+F over one
        // of those replaces it outright.
        if self.search.as_ref().is_some_and(|p| p.scope().is_some()) {
            // Compute the corpus before the mutable borrow of `self.search`.
            let rebuild = self
                .search
                .as_ref()
                .is_some_and(|p| p.scope() != Some(scope));
            let corpus = rebuild.then(|| self.build_search_corpus(scope));
            let palette = self.search.as_mut().unwrap();
            palette.focus_pending = true;
            if let Some(corpus) = corpus {
                palette.kind = PaletteKind::Search(scope);
                palette.last_query.clear();
                palette.rows.clear();
                if let Some(runtime) = palette.search.as_mut() {
                    runtime.engine.set_corpus(corpus);
                }
            }
            return;
        }
        let Some(waker) = self.waker.clone() else {
            return;
        };
        let mut engine = SearchEngine::spawn(waker);
        engine.set_corpus(self.build_search_corpus(scope));
        self.search = Some(PaletteState {
            kind: PaletteKind::Search(scope),
            query: String::new(),
            last_query: String::new(),
            selected: 0,
            focus_pending: true,
            armed: false,
            rows: Vec::new(),
            search: Some(SearchRuntime {
                engine,
                generation: 0,
                searching: false,
                awaiting_first: false,
            }),
        });
    }

    /// Open the space switcher (Cmd+K, or the sidebar header).
    pub(crate) fn open_space_switcher(&mut self) {
        let rows = self.space_rows("");
        self.search = Some(PaletteState {
            kind: PaletteKind::SpaceSwitcher,
            query: String::new(),
            last_query: String::new(),
            // Start on the active space, so Enter is a no-op rather than a
            // surprise jump to whichever space happens to sort first.
            selected: rows
                .iter()
                .position(|row| row.action == PaletteAction::SwitchSpace(self.active_space_id))
                .unwrap_or(0),
            focus_pending: true,
            armed: false,
            rows,
            search: None,
        });
    }

    /// Open the move-to palette for `subject` (Cmd+Shift+M, or a context menu).
    pub(crate) fn open_move_to(&mut self, subject: MoveSubject) {
        let rows = self.move_destinations(&subject, "");
        self.search = Some(PaletteState {
            kind: PaletteKind::MoveTo(subject),
            query: String::new(),
            last_query: String::new(),
            selected: 0,
            focus_pending: true,
            armed: false,
            rows,
            search: None,
        });
    }

    /// Every space, filtered by `query`, as palette rows.
    ///
    /// Rebuilt only when the query changes — the candidate set is tens of
    /// entries, so filtering it inline beats a worker, but doing it per frame
    /// would still be waste.
    pub(crate) fn space_rows(&self, query: &str) -> Vec<PaletteRow> {
        let needle = query.trim().to_lowercase();
        let mut rows: Vec<PaletteRow> = self
            .notes
            .spaces()
            .iter()
            .filter(|space| needle.is_empty() || space.name.to_lowercase().contains(&needle))
            .map(|space| {
                let count = self.notes.note_count_in_space(space.id);
                let where_ = match space.server.as_deref() {
                    Some(server) => format!("{count} notes \u{00b7} synced @ {server}"),
                    None => format!("{count} notes \u{00b7} on this device"),
                };
                PaletteRow {
                    title: space.name.clone(),
                    subtitle: where_,
                    highlights: Vec::new(),
                    indicator: self
                        .sync
                        .as_ref()
                        .map(|sync| sync.space_indicator(&self.notes, space.id))
                        .unwrap_or(SyncIndicator::LocalOnly),
                    action: PaletteAction::SwitchSpace(space.id),
                }
            })
            .collect();

        // The space-level actions that used to hide behind unlabelled glyphs.
        rows.push(PaletteRow {
            title: "New space".to_string(),
            subtitle: String::new(),
            highlights: Vec::new(),
            indicator: SyncIndicator::LocalOnly,
            action: PaletteAction::NewSpace,
        });
        #[cfg(not(target_arch = "wasm32"))]
        rows.push(PaletteRow {
            title: "Import a folder\u{2026}".to_string(),
            subtitle: "Read markdown files in as a new space".to_string(),
            highlights: Vec::new(),
            indicator: SyncIndicator::LocalOnly,
            action: PaletteAction::ImportFolder,
        });
        rows
    }

    /// Every legal destination for `subject`, across every space, as a path.
    ///
    /// Destinations are shown as `Space / Folder / Subfolder`, which is what the
    /// old flat name lists could not express: two folders called "Notes" in
    /// different spaces were indistinguishable, and a folder in another space
    /// was unreachable in one step. Illegal destinations are filtered out here
    /// rather than rejected on selection.
    pub(crate) fn move_destinations(&self, subject: &MoveSubject, query: &str) -> Vec<PaletteRow> {
        let needle = query.trim().to_lowercase();
        let mut rows = Vec::new();
        for space in self.notes.spaces() {
            let indicator = self
                .sync
                .as_ref()
                .map(|sync| sync.space_indicator(&self.notes, space.id))
                .unwrap_or(SyncIndicator::LocalOnly);
            // Moving into a synced space changes who can read the note, because
            // a space is bound to exactly one server. Say so on the row rather
            // than after the fact.
            let audience = match space.server.as_deref() {
                Some(server) => format!("shared with everyone in this space \u{00b7} {server}"),
                None => "stays on this device".to_string(),
            };

            let mut candidates: Vec<(Option<Uuid>, String)> = vec![(None, space.name.clone())];
            for folder in self.notes.folders_in_space(space.id) {
                candidates.push((Some(folder.id), self.folder_path(space, folder.id)));
            }
            for (folder, path) in candidates {
                if !self.move_allowed(subject, space.id, folder) {
                    continue;
                }
                if !needle.is_empty() && !path.to_lowercase().contains(&needle) {
                    continue;
                }
                rows.push(PaletteRow {
                    title: path,
                    subtitle: audience.clone(),
                    highlights: Vec::new(),
                    indicator,
                    action: PaletteAction::MoveTo {
                        space: space.id,
                        folder,
                    },
                });
            }
        }

        // Typing something that matches nothing is usually a new folder.
        let typed = query.trim();
        if !typed.is_empty() {
            let space = self.active_space_id;
            let space_name = self.notes.space_name(space).unwrap_or("this space");
            rows.push(PaletteRow {
                title: format!("New folder \u{201c}{typed}\u{201d} in {space_name}"),
                subtitle: String::new(),
                highlights: Vec::new(),
                indicator: SyncIndicator::LocalOnly,
                action: PaletteAction::CreateFolderAndMove {
                    space,
                    name: typed.to_string(),
                },
            });
        }
        rows
    }

    /// `Space / Folder / Subfolder` for a folder, walking up to the space.
    fn folder_path(&self, space: &crate::note::Space, folder: Uuid) -> String {
        let mut chain = Vec::new();
        let mut current = Some(folder);
        while let Some(id) = current {
            let Some(f) = self.notes.folder(&id) else {
                break;
            };
            chain.push(f.name.as_str());
            current = f.parent;
        }
        chain.reverse();
        let mut path = space.name.clone();
        for name in chain {
            path.push_str(" / ");
            path.push_str(name);
        }
        path
    }

    /// Would moving `subject` to `(space, folder)` be a real move?
    ///
    /// Rules out no-ops (its current home) and cycles (a folder into its own
    /// subtree) — the same conditions `drop_allowed` enforces for drag & drop,
    /// applied up front so an illegal destination never appears in the list.
    fn move_allowed(&self, subject: &MoveSubject, space: i64, folder: Option<Uuid>) -> bool {
        match subject {
            MoveSubject::Note(id) => {
                let Some(note) = self.notes.note(id) else {
                    return false;
                };
                !(note.space_id() == space && note.folder() == folder)
            }
            MoveSubject::Folder(id) => {
                let Some(f) = self.notes.folder(id) else {
                    return false;
                };
                if f.space_id == space && f.parent == folder {
                    return false;
                }
                match folder {
                    // Never into itself or anything beneath it.
                    Some(target) => !self.notes.folder_subtree(*id).contains(&target),
                    None => true,
                }
            }
            MoveSubject::Blob(name) => {
                let Some(blob) = self.notes.blob_by_name(self.active_space_id, name) else {
                    return false;
                };
                !(blob.space_id == space && blob.folder == folder)
            }
        }
    }

    /// Carry out a chosen palette row.
    pub(crate) fn apply_palette_action(&mut self, action: PaletteAction, kind: &PaletteKind) {
        let subject = match kind {
            PaletteKind::MoveTo(subject) => Some(subject.clone()),
            _ => None,
        };
        match action {
            PaletteAction::OpenNote { id, offset, jump } => {
                self.select_note(id.clone());
                if jump {
                    self.pending_jump = Some((id, offset));
                }
            }
            PaletteAction::SwitchSpace(space) => {
                self.active_space_id = space;
                self.ensure_active_note();
            }
            PaletteAction::MoveTo { space, folder } => {
                if let Some(subject) = subject {
                    self.move_subject(&subject, space, folder);
                }
            }
            PaletteAction::CreateFolderAndMove { space, name } => {
                if let Some(subject) = subject
                    && let Some(folder) = self.notes.create_folder(space, &name)
                {
                    self.move_subject(&subject, space, Some(folder));
                }
            }
            PaletteAction::NewSpace => self.create_space_and_select(),
            PaletteAction::ImportFolder => self.open_import_picker(),
        }
    }

    /// Move `subject` into `(space, folder)`, following it there.
    fn move_subject(&mut self, subject: &MoveSubject, space: i64, folder: Option<Uuid>) {
        match subject {
            MoveSubject::Note(id) => {
                self.notes.move_note_to_space(id, space);
                self.notes.set_note_folder(id, folder);
                self.active_space_id = space;
                self.select_note(id.clone());
            }
            MoveSubject::Folder(id) => {
                self.notes.set_folder_parent(id, folder);
                self.active_space_id = space;
            }
            MoveSubject::Blob(name) => {
                if let Some(blob) = self.notes.blob_by_name(self.active_space_id, name) {
                    let blob_id = blob.id.clone();
                    self.notes.set_blob_folder(&blob_id, folder);
                }
            }
        }
    }

    /// Text snapshot fed to the search worker. Global searches every note's body,
    /// Document only the active note's body, and Title every note's title (the
    /// "go to note" palette). Built once per palette open (off the per-frame path).
    pub(crate) fn build_search_corpus(&self, scope: SearchScope) -> Vec<SearchDoc> {
        self.notes
            .summaries()
            .iter()
            .filter(|summary| scope != SearchScope::Document || summary.id == self.active_note_id)
            .filter_map(|summary| {
                // Title search matches the title and labels rows with the folder
                // path; body search matches decoded text and labels with the full
                // path (which already ends in the title).
                let (text, full_name) = if scope == SearchScope::Title {
                    (summary.title.clone(), self.note_location(summary))
                } else {
                    (
                        self.notes.note(&summary.id)?.text(),
                        self.note_full_name(summary),
                    )
                };
                Some(SearchDoc {
                    note_id: summary.id.clone(),
                    full_name,
                    text,
                })
            })
            .collect()
    }

    /// Folder path for a note, no title: `Space / Folder / Subfolder`.
    pub(crate) fn note_location(&self, summary: &NoteSummary) -> String {
        // Folder chain from the note's folder up to the space root, reversed.
        let mut chain: Vec<&str> = Vec::new();
        let mut current = summary.folder;
        while let Some(id) = current {
            let Some(folder) = self.notes.folder(&id) else {
                break;
            };
            chain.push(folder.name.as_str());
            current = folder.parent;
        }
        chain.reverse();

        let mut name = String::new();
        if let Some(space) = self.notes.space_name(summary.space_id) {
            name.push_str(space);
        }
        for folder in chain {
            name.push_str(" / ");
            name.push_str(folder);
        }
        name
    }

    /// Create a folder and immediately rename it in place.
    ///
    /// "New folder…" used to open a dialog for a name it did not yet have. The
    /// folder now exists straight away with a placeholder name and its row is
    /// live for editing — the same gesture as creating a folder in a file
    /// manager, and it works whether or not you bother to type anything.
    pub(crate) fn create_folder_and_rename(&mut self, space_id: i64, parent: Option<Uuid>) {
        let Some(id) = self.notes.create_folder_in(space_id, parent, "New folder") else {
            return;
        };
        if let Some(parent) = parent {
            // Make sure the new child is actually visible to be renamed.
            self.notes.set_folder_folded(&parent, false);
        }
        self.active_space_id = space_id;
        self.begin_rename(RenameTarget::Folder(id));
    }

    /// Begin renaming `target` in place, seeded with its current name.
    pub(crate) fn begin_rename(&mut self, target: RenameTarget) {
        let name = match &target {
            RenameTarget::Space(id) => self.notes.space_name(*id).unwrap_or_default().to_string(),
            RenameTarget::Folder(id) => self
                .notes
                .folder(id)
                .map(|folder| folder.name.clone())
                .unwrap_or_default(),
            RenameTarget::Blob(name) => name.clone(),
        };
        self.inline_edit = Some(InlineEdit {
            target,
            buffer: name,
            focus_pending: true,
            select_pending: true,
        });
    }

    /// Commit the open inline rename. A blank name is a no-op, so a row can
    /// never lose its name to an accidental Enter on an empty field.
    pub(crate) fn commit_rename(&mut self) {
        let Some(edit) = self.inline_edit.take() else {
            return;
        };
        let name = edit.buffer.trim();
        if name.is_empty() {
            return;
        }
        match edit.target {
            RenameTarget::Space(id) => self.notes.rename_space(id, name),
            RenameTarget::Folder(id) => self.notes.rename_folder(&id, name),
            RenameTarget::Blob(old) => {
                if let Some(blob) = self.notes.blob_by_name(self.active_space_id, &old) {
                    let id = blob.id.clone();
                    self.notes.rename_blob(&id, name);
                    // Keep the viewer pointed at the renamed file.
                    if self.view.image() == Some(old.as_str())
                        && let Some(new_name) = self.notes.blob(&id).map(|b| b.name.clone())
                    {
                        self.view = View::Image(new_name);
                    }
                }
            }
        }
    }

    /// Is `target` the row currently being renamed?
    pub(crate) fn renaming(&self, target: &RenameTarget) -> bool {
        self.inline_edit
            .as_ref()
            .is_some_and(|edit| &edit.target == target)
    }

    /// `Folder / Subfolder` for the active note — the breadcrumb.
    ///
    /// Deliberately *not* prefixed with the space: the sidebar header already
    /// names it, and repeating it here both wasted the space and made "Shared"
    /// ambiguous on screen. Empty for a note at the space root, so the caller
    /// can drop the separator rather than render a dangling slash.
    pub(crate) fn active_note_folder_path(&self) -> String {
        let Some(summary) = self
            .summaries
            .iter()
            .find(|summary| summary.id == self.active_note_id)
        else {
            return String::new();
        };
        let mut chain = Vec::new();
        let mut current = summary.folder;
        while let Some(id) = current {
            let Some(folder) = self.notes.folder(&id) else {
                break;
            };
            chain.push(folder.name.as_str());
            current = folder.parent;
        }
        chain.reverse();
        chain.join(" / ")
    }

    /// Human-readable location for a note: `Space / Folder / Subfolder / Title`.
    pub(crate) fn note_full_name(&self, summary: &NoteSummary) -> String {
        let mut name = self.note_location(summary);
        name.push_str(" / ");
        name.push_str(&summary.title);
        name
    }

    /// Dismiss the topmost transient surface, innermost first: menu submenu →
    /// menu → palette → modal-ish dialogs → the image view.
    ///
    /// One ordered rule replaces the hand-maintained `if/else` chain this grew
    /// out of; every new surface joins the order here rather than adding a
    /// branch at the call site. Returns whether anything was dismissed.
    pub(crate) fn dismiss_top(&mut self) -> bool {
        if let Some(menu) = self.menu.as_mut()
            && menu.submenu.is_some()
        {
            menu.submenu = None;
            return true;
        }
        if self.menu.take().is_some() {
            return true;
        }
        if self.search.take().is_some() {
            return true;
        }
        // Escape reverts an inline rename rather than committing it.
        if self.inline_edit.take().is_some() {
            return true;
        }
        if self.share_dialog.take().is_some()
            || self.file_explorer.take().is_some()
            || self.delete_space_confirm.take().is_some()
        {
            return true;
        }
        // Below every popover — a menu opened *from* the drawer closes first —
        // but above view navigation, so Escape puts the sidebar away rather
        // than leaving the view behind it.
        if self.drawer_open {
            self.drawer_open = false;
            return true;
        }
        // Views are navigation, so Escape leaves them only once every layer
        // above has been dismissed — and it pops one step, back to where the
        // user came from.
        if self.view != View::Editor {
            let back = self.view_stack.pop().unwrap_or(View::Editor);
            self.set_view(back);
            return true;
        }
        false
    }

    /// Navigate to `view`, remembering where we came from so Escape and the
    /// back button return there. Depth is capped — nothing in the app nests
    /// views more than a step or two, and an unbounded stack would only grow.
    pub(crate) fn push_view(&mut self, view: View) {
        if self.view == view {
            return;
        }
        if self.view_stack.len() < 8 {
            self.view_stack.push(self.view.clone());
        }
        self.set_view(view);
    }

    /// Switch to `view` without touching the back-stack, restarting its fade.
    pub(crate) fn set_view(&mut self, view: View) {
        if self.view == view {
            return;
        }
        // Only *arriving somewhere* animates. Moving between Settings
        // categories goes through here too, and re-running the entrance on
        // every category click is not what a tab does — you would see the whole
        // window pulse when you click "Editor".
        let same_destination =
            matches!((&self.view, &view), (View::Settings(_), View::Settings(_)));
        self.view = view;
        if !same_destination {
            self.view_fade = 0.0;
        }
    }

    /// Advance the view fade and report the opacity to draw this frame.
    ///
    /// Returns 1.0 once settled, so the steady state costs one comparison and
    /// no repaint — a fade that never finishes would pin the app at full frame
    /// rate forever.
    pub(crate) fn tick_view_fade(&mut self, ui: &mut IMUI) -> f32 {
        if self.view_fade >= 1.0 {
            return 1.0;
        }
        let rate = mae::imui::smooth_rate(ui.theme().motion.menu_rate, ui.dt());
        self.view_fade = mae::imui::animate_scalar(self.view_fade, 1.0, rate, 0.01);
        ui.request_repaint();
        self.view_fade
    }

    /// Open Settings at `section`.
    pub(crate) fn open_settings(&mut self, section: SettingsSection) {
        self.push_view(View::Settings(section));
    }

    /// Leave the welcome screen and remember that we have. Written to the note
    /// database's meta table, the same place the last-opened note lives, so it
    /// survives a restart without a second store.
    pub(crate) fn finish_onboarding(&mut self) {
        self.notes.meta_set(META_ONBOARDED, "1");
        self.set_view(View::Editor);
        self.view_stack.clear();
    }

    /// Has the user been through (or dismissed) the welcome screen?
    pub(crate) fn is_onboarded(notes: &NoteDatabase) -> bool {
        notes.meta_get(META_ONBOARDED).is_some()
    }

    /// Connect using the details typed on the welcome screen, then stay put:
    /// on success the screen grows a device-key section, which is the next
    /// thing the user needs.
    pub(crate) fn connect_from_welcome(&mut self) {
        #[cfg(not(target_arch = "wasm32"))]
        {
            let typed = self.add_server_input.trim().to_string();
            if !typed.is_empty() && typed != self.active_server {
                self.add_server(&typed);
                if let Some(url) = normalize_server_url(&typed) {
                    self.active_server = url;
                }
            }
            // After the server is settled, so the token lands on the server it
            // was typed for rather than the one that was active a moment ago.
            //
            // Applied even when empty: the field is seeded from storage before
            // it is shown, so blank means "this server needs no token" — which
            // is the only way to walk back a token the relay refused.
            let (server, token) = (self.active_server.clone(), self.token_input.clone());
            self.set_account_token(&server, &token);
            self.token_input_server = server;
        }
        // An explicit press: always start a fresh attempt, even if an engine is
        // already grinding away at a server that is not answering.
        self.reconnect_sync();
    }

    /// Show the first-run screen again, from Settings → General.
    pub(crate) fn show_welcome(&mut self) {
        self.push_view(View::Welcome);
    }

    /// Is the space switcher palette open? Drives the trigger's pressed look.
    pub(crate) fn space_switcher_open(&self) -> bool {
        self.search
            .as_ref()
            .is_some_and(|p| p.kind == PaletteKind::SpaceSwitcher)
    }

    /// One-line sync state **for the active space**, for the sidebar status
    /// pill: what to say, and what colour to say it in.
    ///
    /// Deliberately per-space rather than per-connection. A space is bound to
    /// exactly one server (or to none at all), so a single global "Synced"
    /// answered a question nobody was asking: it read the same while looking at
    /// a local-only space, which is the one case where the honest answer is
    /// "this never leaves your device". The pill now answers "is *this* space
    /// syncing, and where".
    ///
    /// Connection state only. The pill used to lead with `last_error`, which is
    /// a *sticky* record of the last thing that went wrong — never cleared, and
    /// used for durable conditions like a quarantined image as well as for
    /// connection failures. So one transient hiccup left the sidebar reading
    /// "Sync error" for the rest of the session while everything worked. What
    /// went wrong belongs in Settings → Sync, where it can be read and acted
    /// on.
    pub(crate) fn connection_status(&self, pal: &Colors) -> (String, Color) {
        let nickname = self.nickname_input.trim();
        let Some(bound) = self.notes.space_server(self.active_space_id) else {
            // Not a failure and not a degraded state — a local-only space is a
            // deliberate choice, so it reads neutrally and carries no nickname
            // (there is nobody to be known to).
            return ("Local only".to_string(), pal.text_faint);
        };
        let bound = bound.to_string();

        // Bound to a server that is not the one the live connection dials.
        // Only one connection runs at a time, so this space genuinely is not
        // syncing right now — saying "Synced" because some *other* space's
        // server is connected would be a lie.
        if bound != self.active_server {
            return (
                format!("Paused \u{00b7} {}", server_host(&bound)),
                Color::new("#f0a030"),
            );
        }

        let (state_label, color) = match self.sync.as_ref() {
            Some(sync) if sync.connected() && sync.has_pending() => {
                ("Syncing\u{2026}", Color::new("#f0a030"))
            }
            Some(sync) if sync.connected() => ("Synced", Color::new("#34a853")),
            // A version mismatch is terminal, not slow: saying "Connecting…"
            // for it is how this looked like a hang.
            Some(sync) if sync.incompatible().is_some() => {
                ("Incompatible server", Color::new("#d93025"))
            }
            // Also terminal: the engine has stopped retrying, so "Connecting…"
            // would be a lie here too.
            Some(sync) if sync.rejected() => ("Account needed", Color::new("#d93025")),
            Some(_) => ("Connecting\u{2026}", Color::new("#f0a030")),
            // The space wants a server but no engine is running yet.
            None => ("Not connected", pal.text_faint),
        };
        let label = if nickname.is_empty() {
            state_label.to_string()
        } else {
            format!("{state_label} \u{00b7} {nickname}")
        };
        (label, color)
    }

    /// Open a context menu at `pos`, replacing any menu already open.
    ///
    /// `armed: false` suppresses click-away dismissal for one frame: the press
    /// that opened this menu is still in the event queue, and the pane has no
    /// painted rect yet to test the pointer against.
    pub(crate) fn open_menu(&mut self, menu: Menu, pos: Point) {
        self.menu = Some(MenuState {
            menu,
            anchor: Anchor::At(pos),
            submenu: None,
            armed: false,
        });
    }

    pub(crate) fn record_persistence_error(&mut self, error: String) {
        if self.last_persistence_error.as_deref() != Some(error.as_str()) {
            eprintln!("Note database error: {error}");
        }
        self.last_persistence_error = Some(error);
    }
}

/// One space as the connected server reports it, resolved for display.
///
/// Shared by Settings → Sync and the welcome screen's post-connect step, which
/// answer the same question ("what is on this server, and do I have it?") and
/// would otherwise each repeat the peek-and-name dance.
pub(crate) struct RemoteRow {
    pub space_id: Uuid,
    /// Set when this device already mirrors the space.
    pub local: Option<i64>,
    /// `None` until a peek decrypts the space's index far enough to read it.
    pub name: Option<String>,
    pub is_owner: bool,
}

/// `wss://sync.example.com/enkr/ws` → `sync.example.com`.
///
/// The pill has room for a few words, and the host is the part that tells one
/// server from another; scheme and path are noise at that size.
pub(crate) fn server_host(url: &str) -> &str {
    let after_scheme = url.split_once("://").map(|(_, rest)| rest).unwrap_or(url);
    after_scheme
        .split(['/', '?'])
        .next()
        .unwrap_or(after_scheme)
}
