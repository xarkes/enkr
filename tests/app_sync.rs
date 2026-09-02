//! GUI-level sync scenario (PLAN.md §6 "Client testkit"): one in-process
//! `enkr-syncd` and two real Enkr apps driven exclusively through simulated
//! clicks and keystrokes on their widgets.
//!
//! Base scenario:
//! - Client A pushes a local Space to remote
//! - Client A shares the Space with Client B (typed device key)
//! - Client B fetches remote Spaces and syncs the shared one
//! - Client B modifies that Space's first note; A sees it
//! - Client A modifies the note as well; B receives it
//! - Client B disconnects; A keeps editing; B reconnects and catches up

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use enkr::app::{EnkrState, render};
use enkr::note::NoteDatabase;
use enkr::sync::{IdentityStore, MemberRole};
use enkr_syncd::storage::{
    Account, DevicePk, EnvelopeRow, NewAccount, Result as StoreResult, SnapshotRow, SqliteStore,
    Store,
};
use enkr_syncd::{ServerConfig, ServerHandle, serve};

/// Shared with the engine-level suites: a loopback proxy that adds a realistic
/// round trip. Needed here because localhost hides every ordering that depends
/// on the server's reply being slower than the client's next few sends.
#[path = "harness/net.rs"]
mod net;
use mae::imui::UIBoxFlags;
use mae::testkit::{UiDriver, UiHarness, UiSnapshot};
use uuid::Uuid;

const PHASE_TIMEOUT: Duration = Duration::from_secs(20);
/// Real time must pass between frames for the debounce/network pipeline.
const FRAME_PACE: Duration = Duration::from_millis(15);

const SETTINGS_ICON: &str = "\u{e8b8}";

/// Serializes the scenarios in this file.
///
/// Each one runs a real server, real WebSockets and real timers, and asserts
/// against wall-clock budgets (`pump_until` deadlines, and in places a fixed
/// sleep before asserting something did *not* arrive). Twenty-two of those in
/// parallel starve each other's budgets and fail at random — which is what was
/// happening. Serializing costs about three seconds across the whole file,
/// because these tests are dominated by waiting rather than by CPU.
/// Reentrant per *thread*, which is per test: libtest gives each test its own
/// thread, and several scenarios start two servers (a space bound to one server
/// must not be pushable to another). A plain mutex deadlocks the moment a test
/// takes it twice.
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

thread_local! {
    static SERIAL_HELD: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

struct SerialGuard(Option<std::sync::MutexGuard<'static, ()>>);

impl Drop for SerialGuard {
    fn drop(&mut self) {
        if self.0.is_some() {
            SERIAL_HELD.with(|held| held.set(false));
        }
    }
}

fn serialize() -> SerialGuard {
    if SERIAL_HELD.with(|held| held.get()) {
        return SerialGuard(None);
    }
    // Poison is ignored on purpose: one panicking test must not cascade into
    // every later test failing to acquire.
    let guard = SERIAL.lock().unwrap_or_else(|err| err.into_inner());
    SERIAL_HELD.with(|held| held.set(true));
    SerialGuard(Some(guard))
}

struct TestServer {
    rt: tokio::runtime::Runtime,
    handle: Option<ServerHandle>,
    addr: std::net::SocketAddr,
    db_path: PathBuf,
    /// Held for the test's lifetime; declared last so it is released only
    /// after the runtime and server have shut down.
    _serial: SerialGuard,
}

impl TestServer {
    fn start() -> Self {
        Self::start_with(ServerConfig::default())
    }

    fn start_with(config: ServerConfig) -> Self {
        // Acquire before building anything: a queued test must not sit on a
        // tokio runtime (and its worker threads) while it waits its turn.
        let serial = serialize();
        let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
        let db_path =
            std::env::temp_dir().join(format!("enkr_app_sync_srv_{}.sqlite3", Uuid::new_v4()));
        let handle = rt.block_on(async {
            let store = SqliteStore::open(&db_path).await.expect("server store");
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind");
            serve(Arc::new(store), listener, config).await
        });
        Self {
            addr: handle.addr,
            handle: Some(handle),
            rt,
            db_path,
            _serial: serial,
        }
    }

    fn url(&self) -> String {
        format!("ws://{}/ws", self.addr)
    }
}

impl TestServer {
    /// A server whose blob reads are counted, so a test can assert a reconnect
    /// issues no wasteful `GetBlob` for images the index no longer advertises.
    fn start_counting_blob_gets() -> (Self, Arc<AtomicU64>) {
        let serial = serialize();
        let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
        let db_path =
            std::env::temp_dir().join(format!("enkr_app_sync_srv_{}.sqlite3", Uuid::new_v4()));
        let blob_gets = Arc::new(AtomicU64::new(0));
        let counter = blob_gets.clone();
        let handle = rt.block_on(async {
            let store = SqliteStore::open(&db_path).await.expect("server store");
            let store = CountingStore {
                inner: store,
                blob_gets: counter,
            };
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind");
            serve(Arc::new(store), listener, ServerConfig::default()).await
        });
        (
            Self {
                addr: handle.addr,
                handle: Some(handle),
                rt,
                db_path,
                _serial: serial,
            },
            blob_gets,
        )
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            self.rt.block_on(handle.shutdown());
        }
        let _ = std::fs::remove_file(&self.db_path);
        let _ = std::fs::remove_file(self.db_path.with_extension("sqlite3-wal"));
        let _ = std::fs::remove_file(self.db_path.with_extension("sqlite3-shm"));
    }
}

/// A `Store` decorator that forwards every call to an inner `SqliteStore` and
/// tallies `get_blob` reads. Delegation-only: if the trait grows a method this
/// stops compiling, which is the intended fail-loud signal.
struct CountingStore {
    inner: SqliteStore,
    blob_gets: Arc<AtomicU64>,
}

#[async_trait::async_trait]
impl Store for CountingStore {
    async fn create_account(&self, new: NewAccount<'_>) -> StoreResult<Account> {
        self.inner.create_account(new).await
    }

    async fn account_by_token(&self, token_hash: &[u8; 32]) -> StoreResult<Option<Account>> {
        self.inner.account_by_token(token_hash).await
    }

    async fn account(&self, account_id: &Uuid) -> StoreResult<Option<Account>> {
        self.inner.account(account_id).await
    }

    async fn accounts(&self) -> StoreResult<Vec<Account>> {
        self.inner.accounts().await
    }

    async fn delete_account(&self, account_id: &Uuid) -> StoreResult<bool> {
        self.inner.delete_account(account_id).await
    }

    async fn set_account_expiry(
        &self,
        account_id: &Uuid,
        expires_at: Option<i64>,
    ) -> StoreResult<bool> {
        self.inner.set_account_expiry(account_id, expires_at).await
    }

    async fn bind_device_account(
        &self,
        device_pk: &DevicePk,
        account_id: Option<&Uuid>,
    ) -> StoreResult<()> {
        self.inner.bind_device_account(device_pk, account_id).await
    }

    async fn space_owner_account(&self, space_id: &Uuid) -> StoreResult<Option<Uuid>> {
        self.inner.space_owner_account(space_id).await
    }

    async fn recompute_usage(&self) -> StoreResult<Vec<(Uuid, i64, i64)>> {
        self.inner.recompute_usage().await
    }

    async fn upsert_device(
        &self,
        device_pk: &DevicePk,
        kex_pk: &[u8; 32],
        now: i64,
    ) -> StoreResult<()> {
        self.inner.upsert_device(device_pk, kex_pk, now).await
    }
    async fn space_epoch(&self, space_id: &Uuid) -> StoreResult<Option<u32>> {
        self.inner.space_epoch(space_id).await
    }
    async fn create_space(
        &self,
        space_id: &Uuid,
        creator: &DevicePk,
        signed_op: &[u8],
        envelopes: &[EnvelopeRow],
        now: i64,
    ) -> StoreResult<()> {
        self.inner
            .create_space(space_id, creator, signed_op, envelopes, now)
            .await
    }
    async fn add_member(
        &self,
        space_id: &Uuid,
        device_pk: &DevicePk,
        role: MemberRole,
        epoch_added: u32,
        op_seq: u64,
        signed_op: &[u8],
        envelopes: &[EnvelopeRow],
        now: i64,
    ) -> StoreResult<()> {
        self.inner
            .add_member(
                space_id,
                device_pk,
                role,
                epoch_added,
                op_seq,
                signed_op,
                envelopes,
                now,
            )
            .await
    }
    async fn remove_member(
        &self,
        space_id: &Uuid,
        device_pk: &DevicePk,
        new_epoch: u32,
        op_seq: u64,
        signed_op: &[u8],
        envelopes: &[EnvelopeRow],
        now: i64,
    ) -> StoreResult<()> {
        self.inner
            .remove_member(
                space_id, device_pk, new_epoch, op_seq, signed_op, envelopes, now,
            )
            .await
    }
    async fn is_active_member(&self, space_id: &Uuid, device_pk: &DevicePk) -> StoreResult<bool> {
        self.inner.is_active_member(space_id, device_pk).await
    }
    async fn member_role(
        &self,
        space_id: &Uuid,
        device_pk: &DevicePk,
    ) -> StoreResult<Option<MemberRole>> {
        self.inner.member_role(space_id, device_pk).await
    }
    async fn spaces_for_device(&self, device_pk: &DevicePk) -> StoreResult<Vec<Uuid>> {
        self.inner.spaces_for_device(device_pk).await
    }
    async fn delete_space(&self, space_id: &Uuid) -> StoreResult<()> {
        self.inner.delete_space(space_id).await
    }
    async fn next_membership_seq(&self, space_id: &Uuid) -> StoreResult<u64> {
        self.inner.next_membership_seq(space_id).await
    }
    async fn membership_log(&self, space_id: &Uuid) -> StoreResult<Vec<Vec<u8>>> {
        self.inner.membership_log(space_id).await
    }
    async fn envelopes_for_device(
        &self,
        space_id: &Uuid,
        device_pk: &DevicePk,
    ) -> StoreResult<Vec<(u32, Vec<u8>)>> {
        self.inner.envelopes_for_device(space_id, device_pk).await
    }
    async fn create_doc(&self, doc_id: &Uuid, space_id: &Uuid, now: i64) -> StoreResult<()> {
        self.inner.create_doc(doc_id, space_id, now).await
    }
    async fn doc_space(&self, doc_id: &Uuid) -> StoreResult<Option<Uuid>> {
        self.inner.doc_space(doc_id).await
    }

    async fn doc_spaces(&self, doc_ids: &[Uuid]) -> StoreResult<Vec<(Uuid, Uuid)>> {
        self.inner.doc_spaces(doc_ids).await
    }
    async fn append_update(
        &self,
        doc_id: &Uuid,
        frame: &[u8],
        epoch: u32,
        now: i64,
    ) -> StoreResult<u64> {
        self.inner.append_update(doc_id, frame, epoch, now).await
    }
    async fn updates_since(
        &self,
        doc_id: &Uuid,
        after_seq: u64,
        limit: u32,
    ) -> StoreResult<Vec<(u64, Vec<u8>)>> {
        self.inner.updates_since(doc_id, after_seq, limit).await
    }
    async fn head_seq(&self, doc_id: &Uuid) -> StoreResult<u64> {
        self.inner.head_seq(doc_id).await
    }
    async fn put_snapshot(&self, snapshot: &SnapshotRow) -> StoreResult<bool> {
        self.inner.put_snapshot(snapshot).await
    }
    async fn latest_snapshot(&self, doc_id: &Uuid) -> StoreResult<Option<SnapshotRow>> {
        self.inner.latest_snapshot(doc_id).await
    }
    async fn ack_snapshot(&self, doc_id: &Uuid, covers_seq: u64) -> StoreResult<()> {
        self.inner.ack_snapshot(doc_id, covers_seq).await
    }
    async fn gc_eligible(&self, created_before: i64) -> StoreResult<Vec<(Uuid, u64)>> {
        self.inner.gc_eligible(created_before).await
    }
    async fn gc_updates_through(&self, doc_id: &Uuid, seq: u64) -> StoreResult<u64> {
        self.inner.gc_updates_through(doc_id, seq).await
    }

    async fn gc_envelopes(&self) -> StoreResult<u64> {
        self.inner.gc_envelopes().await
    }
    async fn put_blob(
        &self,
        blob_id: &Uuid,
        space_id: &Uuid,
        bytes: &[u8],
        now: i64,
    ) -> StoreResult<()> {
        self.inner.put_blob(blob_id, space_id, bytes, now).await
    }
    async fn get_blob(&self, blob_id: &Uuid) -> StoreResult<Option<Vec<u8>>> {
        self.blob_gets.fetch_add(1, Ordering::Relaxed);
        self.inner.get_blob(blob_id).await
    }
    async fn delete_blob(&self, blob_id: &Uuid, space_id: &Uuid) -> StoreResult<()> {
        self.inner.delete_blob(blob_id, space_id).await
    }
}

/// One Enkr app instance: a real `EnkrState` rendered by the UI harness.
struct App {
    harness: UiHarness,
    state: EnkrState,
    sync_db: PathBuf,
    notes_db: Option<PathBuf>,
    /// Remove the backing files on drop (disabled for crash-recovery tests
    /// that resurrect the same files in a fresh App).
    cleanup: bool,
}

impl App {
    fn new() -> Self {
        let sync_db =
            std::env::temp_dir().join(format!("enkr_app_sync_cli_{}.key", Uuid::new_v4()));
        Self::with_files(None, sync_db)
    }

    /// An app that has never been through onboarding, so the first-connect
    /// recovery-phrase prompt still fires. Only the test that covers that
    /// prompt wants it; every other scenario would just have a modal in the way.
    fn first_run() -> Self {
        let sync_db =
            std::env::temp_dir().join(format!("enkr_app_sync_cli_{}.key", Uuid::new_v4()));
        Self::build(None, sync_db, false)
    }

    /// `notes_db = Some(path)` makes the note store file-backed (durability /
    /// restart scenarios). The device key always lives at `sync_db` so the
    /// identity survives disconnects and "crashes".
    fn with_files(notes_db: Option<PathBuf>, sync_db: PathBuf) -> Self {
        Self::build(notes_db, sync_db, true)
    }

    /// A genuinely fresh install: no `onboarded` flag, so it opens on the
    /// welcome screen. The recovery prompt is pre-acked — it fires on connect
    /// and would otherwise sit over the screen under test.
    fn fresh_install() -> Self {
        let sync_db =
            std::env::temp_dir().join(format!("enkr_app_sync_cli_{}.key", Uuid::new_v4()));
        Self::build_inner(None, sync_db, true, false)
    }

    fn build(notes_db: Option<PathBuf>, sync_db: PathBuf, onboarded: bool) -> Self {
        Self::build_inner(notes_db, sync_db, onboarded, true)
    }

    fn build_inner(
        notes_db: Option<PathBuf>,
        sync_db: PathBuf,
        onboarded: bool,
        skip_welcome: bool,
    ) -> Self {
        let mut harness = UiHarness::new(1100.0, 720.0);
        let mut notes = match &notes_db {
            Some(path) => NoteDatabase::open(path).expect("open file-backed note db"),
            None => NoteDatabase::new_in_memory(),
        };
        // A file-backed database really is a fresh install, so it would open on
        // the welcome screen. These scenarios are testing sync, not onboarding
        // — mark it done so they start in the editor. (In-memory databases mark
        // themselves; see `NoteDatabase::new_in_memory`.)
        if skip_welcome {
            notes.meta_set("onboarded", "1");
        } else {
            // `new_in_memory` pre-marks itself onboarded (it is normally a
            // fixture, not a first run); an empty value unsets the key.
            notes.meta_set("onboarded", "");
        }
        // Same reasoning for the recovery-phrase prompt: it is a modal on first
        // connect, so every scenario that is not testing it would have to
        // dismiss it first.
        if onboarded {
            notes.meta_set(enkr::app::META_RECOVERY_ACKED, "1");
        }
        let mut state = EnkrState::with_notes(notes);
        state.sync_identity = Some(IdentityStore::Path(sync_db.clone()));
        state.set_repaint_waker(harness.ui().repaint_waker());
        harness.frame(|ui| render(ui, &mut state));
        Self {
            harness,
            state,
            sync_db,
            notes_db,
            cleanup: true,
        }
    }

    fn frame(&mut self) -> UiSnapshot {
        let state = &mut self.state;
        self.harness.frame(|ui| render(ui, state))
    }

    fn click(&mut self, id: &str) {
        self.harness.click(id);
        self.frame();
    }

    /// Click a note item in the sidebar by its title. The same text can also
    /// appear in the top bar and the editor, so restrict to the sidebar area
    /// (left column, below the top bar).
    /// Make `name` the active space through the sidebar's switcher dropdown.
    ///
    /// Spaces other than the active one are no longer permanently listed in the
    /// sidebar, so switching is a two-step gesture: open the switcher, pick the
    /// space. Once active, the space's own name labels the switcher button, so
    /// right-clicking it reaches the space menu.
    fn switch_to_space(&mut self, name: &str) {
        self.frame();
        self.click("###enkr_space_switcher");
        self.frame();
        self.click(name);
        self.frame();
    }

    fn click_sidebar_note(&mut self, title: &str) {
        let snap = self.frame();
        let center = snap
            .nodes
            .iter()
            .filter(|node| {
                node.text.as_deref() == Some(title)
                    && node.bounds.x1 < 280.0
                    && node.bounds.y0 > 56.0
            })
            .last()
            .unwrap_or_else(|| panic!("no sidebar note titled {title:?}"))
            .center();
        self.harness.click_at(center.x(), center.y());
        self.frame();
    }

    /// Click a single-line input by its stable `###` id and type into it.
    ///
    /// Preferred over `type_into_line_edit`: ordinals shift whenever a field is
    /// added, removed or moved to another settings category, and the resulting
    /// failure ("line edit #1 not found") says nothing about which field was
    /// meant.
    fn type_into_field(&mut self, id: &str, text: &str) {
        let snap = self.frame();
        let center = snap.node(id).center();
        self.harness.click_at(center.x(), center.y());
        self.frame();
        self.harness.type_text(text);
        self.frame();
    }

    /// Click the `index`-th single-line input currently on screen and type.
    fn type_into_line_edit(&mut self, index: usize, text: &str) {
        let snap = self.frame();
        let inputs: Vec<_> = snap
            .nodes
            .iter()
            .filter(|node| node.flags.contains(UIBoxFlags::LINE_EDIT))
            // The editable note title sits in the 56px top bar and is app chrome,
            // not a settings input — skip it so indices address the dialog fields.
            .filter(|node| node.bounds.y0 >= 56.0)
            .map(|node| node.center())
            .collect();
        let center = inputs.get(index).copied().unwrap_or_else(|| {
            panic!(
                "line edit #{index} not found ({} present)\n{}",
                inputs.len(),
                snap.debug_dump()
            )
        });
        self.harness.click_at(center.x(), center.y());
        self.frame();
        self.harness.type_text(text);
        self.frame();
    }

    /// Click into the markdown editor (placing the caret) without typing.
    fn focus_editor(&mut self) {
        let snap = self.frame();
        let editor = snap
            .nodes
            .iter()
            .find(|node| node.flags.contains(UIBoxFlags::MULTILINE))
            .expect("markdown editor on screen");
        let center = editor.center();
        self.harness.click_at(center.x(), center.y());
        self.frame();
    }

    /// Click into the markdown editor and type at the caret.
    fn type_into_editor(&mut self, text: &str) {
        let snap = self.frame();
        let editor = snap
            .nodes
            .iter()
            .find(|node| node.flags.contains(UIBoxFlags::MULTILINE))
            .expect("markdown editor on screen");
        let center = editor.center();
        self.harness.click_at(center.x(), center.y());
        self.frame();
        self.harness.type_text(text);
        self.frame();
    }

    /// Connect to the server through the settings window: add the server to the
    /// list, make it the active server ("Use"), then connect. The default
    /// server (xark.es) is active initially, so the freshly-added server is the
    /// only one offering "Use".
    /// Open Settings → Sync from the sidebar's status pill.
    ///
    /// The extra frame is load-bearing: a view swap is decided at the *top* of
    /// `render`, while the pill's click handler runs during the build, so the
    /// new view first appears one frame later. The old sync *window* rendered
    /// in the same frame because it was painted after the sidebar — a window
    /// and a view genuinely differ here.
    fn open_sync_from_pill(&mut self) {
        self.click("###enkr_status_pill");
        self.frame();
    }

    /// Settings is a view with categories now, so reaching the server controls
    /// is: open Settings, then pick "Sync & Devices".
    fn open_sync_settings(&mut self) {
        self.frame();
        self.click(SETTINGS_ICON);
        self.frame();
        self.click("Sync & Devices");
        self.frame();
    }

    fn connect(&mut self, url: &str, nickname: &str) {
        self.open_sync_settings();
        // Only on the first connect. Reconnect scenarios come back through here
        // after a `disconnect`, by which point the server is already listed and
        // already active — so it has no "Use" button, and the first "Use" on
        // screen would belong to the *default* server row and switch us to the
        // wrong server.
        if !self.state.server_list().iter().any(|s| s == url) {
            self.type_into_field("###enkr_add_server", url);
            self.click("Add\u{2026}");
            self.frame(); // let the new server row (with its "Use" button) build
            self.click("Use"); // make the added server active
        }
        self.type_into_field("###enkr_set_nick", nickname);
        self.click("Connect");
        self.harness.key_press(mae::os::OSKeyCode::KeyEscape);
        self.frame();
        assert!(self.state.sync.is_some(), "sync engine should have started");
    }

    /// Reconnect to the already-configured server: straight to Sync, Connect,
    /// then back to the editor. The server and nickname are already set, so
    /// this is not the full `connect` flow.
    fn reconnect(&mut self) {
        self.open_sync_settings();
        self.click("Connect");
        self.harness.key_press(mae::os::OSKeyCode::KeyEscape);
        self.frame();
    }

    fn disconnect(&mut self) {
        self.open_sync_settings();
        self.click("Disconnect");
        self.harness.key_press(mae::os::OSKeyCode::KeyEscape);
        self.frame();
        assert!(self.state.sync.is_none(), "sync engine should have stopped");
    }

    fn connected(&self) -> bool {
        self.state
            .sync
            .as_ref()
            .is_some_and(|sync| sync.connected())
    }

    /// Text of the first note (by build order) whose body contains `marker`.
    fn note_text_containing(&self, marker: &str) -> Option<String> {
        self.state
            .notes
            .summaries()
            .iter()
            .map(|summary| {
                self.state
                    .notes
                    .note(&summary.id)
                    .map(|note| note.text())
                    .unwrap_or_default()
            })
            .find(|text| text.contains(marker))
    }
}

impl Drop for App {
    fn drop(&mut self) {
        if !self.cleanup {
            return;
        }
        let _ = std::fs::remove_file(&self.sync_db);
        if let Some(notes_db) = &self.notes_db {
            remove_db_files(notes_db);
        }
    }
}

fn remove_db_files(path: &PathBuf) {
    let _ = std::fs::remove_file(path);
    for suffix in ["-wal", "-shm"] {
        let side = path.with_file_name(format!(
            "{}{suffix}",
            path.file_name().unwrap().to_string_lossy()
        ));
        let _ = std::fs::remove_file(side);
    }
}

/// Keep both apps frame-pumping until `cond` holds (or panic on timeout).
fn pump_until(apps: &mut [&mut App], what: &str, mut cond: impl FnMut(&mut [&mut App]) -> bool) {
    let deadline = Instant::now() + PHASE_TIMEOUT;
    loop {
        for app in apps.iter_mut() {
            app.frame();
        }
        if cond(apps) {
            return;
        }
        assert!(Instant::now() < deadline, "timed out waiting for: {what}");
        std::thread::sleep(FRAME_PACE);
    }
}

/// B's own database also holds a builtin "Welcome" note, so conditions must
/// target the note actually mapped to a remote doc.
fn synced_note_with(state: &EnkrState, marker: &str) -> Option<String> {
    state
        .notes
        .summaries()
        .iter()
        .find(|summary| {
            state.notes.note(&summary.id).is_some_and(|note| {
                note.remote_doc().is_some()
                    && summary.id != "Welcome"
                    && note.text().contains(marker)
            })
        })
        .map(|summary| summary.id.clone())
}

/// Full UI-driven setup shared by the scenarios: both clients connect, A
/// pushes its "Shared" space, invites B by typed device key, and B fetches it
/// until the Welcome note is mirrored. Returns `(a, b, b_synced_note_id)`;
/// A's copy of the note keeps its local id `"Welcome"`.
fn establish_shared_pair(server: &TestServer) -> (App, App, String) {
    establish_shared_pair_apps(server, App::new(), App::new(), "bob", MemberRole::Writer)
}

/// Same flow with caller-provided apps (e.g. file-backed for crash tests), a
/// nickname for the invitee B (empty = unnamed), and a chosen role for B.
fn establish_shared_pair_apps(
    server: &TestServer,
    mut a: App,
    mut b: App,
    b_nick: &str,
    b_role: MemberRole,
) -> (App, App, String) {
    // A unique space name so B's sidebar can be driven unambiguously.
    a.state
        .notes
        .rename_space(a.state.notes.default_space_id(), "Shared");

    // -- Both clients connect through the settings UI -------------------------
    a.connect(&server.url(), "ana");
    b.connect(&server.url(), b_nick);
    pump_until(&mut [&mut a, &mut b], "both clients connected", |apps| {
        apps.iter().all(|app| app.connected())
    });

    // -- A syncs its local Space to the server (right-click → submenu) ---------
    a.frame();
    a.harness.right_click("###enkr_space_switcher");
    a.frame();
    a.click("Sync this space\u{2026} >"); // opens the server picker submenu
    a.click(&format!("{}  (active)", server.url())); // active server entry
    pump_until(
        &mut [&mut a, &mut b],
        "space pushed and note mapped",
        |apps| {
            let state = &apps[0].state;
            state
                .notes
                .space_remote(state.notes.default_space_id())
                .is_some()
                && state
                    .notes
                    .note("Welcome")
                    .is_some_and(|note| note.remote_doc().is_some())
        },
    );

    // -- A invites B using B's device key (read from B's sync window) ---------
    b.open_sync_from_pill();
    let b_key = {
        let snap = b.frame();
        snap.nodes
            .iter()
            .filter_map(|node| node.text.clone())
            .find(|text| text.len() == 128 && text.chars().all(|c| c.is_ascii_hexdigit()))
            .expect("device key visible in B's sync window")
    };
    b.harness.key_press(mae::os::OSKeyCode::KeyEscape);
    b.frame();

    a.harness.right_click("###enkr_space_switcher");
    a.frame();
    a.click("Share\u{2026}");
    a.type_into_line_edit(0, &b_key);
    let role_button = match b_role {
        MemberRole::Owner => "Owner",
        MemberRole::Writer => "Write",
        MemberRole::Reader => "Read only",
    };
    a.click(role_button);
    a.click("Invite");
    // Give the membership op a moment to land server-side.
    pump_until(&mut [&mut a, &mut b], "invite processed", |apps| {
        apps[0]
            .state
            .sync
            .as_ref()
            .is_some_and(|s| s.last_error().is_none())
    });

    // -- B fetches the remote space from the sync window ----------------------
    b.open_sync_from_pill();
    let deadline = Instant::now() + PHASE_TIMEOUT;
    loop {
        b.click("Refresh");
        for _ in 0..6 {
            a.frame();
            b.frame();
            std::thread::sleep(FRAME_PACE);
        }
        let snap = b.frame();
        if snap.try_node("Sync").is_some() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "B never saw the shared space in its remote list"
        );
    }
    b.click("Sync");
    pump_until(
        &mut [&mut a, &mut b],
        "B mirrors the space + note content",
        |apps| {
            let b = &apps[1].state;
            b.notes.spaces().iter().any(|space| space.name == "Shared")
                && synced_note_with(b, "# Welcome").is_some()
        },
    );
    b.harness.key_press(mae::os::OSKeyCode::KeyEscape);
    b.frame();

    b.switch_to_space("Shared");
    let synced_note_id = synced_note_with(&b.state, "# Welcome").expect("synced welcome note in B");
    (a, b, synced_note_id)
}

// --- UiDriver port of the old (app.rs unit test) `clicking_a_note_selects_it` ---
//
// One scenario function, written once against `mae::testkit::UiDriver`, run
// both natively (`NativeDriver`, always on) and against a real page in a
// real browser (`CdpDriver`, feature = "cdp") — see `mae/src/testkit.rs`
// and `mae/src/testkit/cdp.rs` for the mechanism.
//
// The original unit test asserted `state.active_note_id == "Product
// roadmap"` directly — internal Rust state a browser-driven `CdpDriver`
// can't see. This instead asserts the effect actually observable through
// either driver: the editor's content switches to the clicked note's body.
// That's a strictly black-box check, so if anything it's a stronger
// regression test than the original, not a weaker stand-in for it.

/// Exact literal bodies from `NoteDatabase::demo()`'s seed data
/// (`enkr/src/note.rs`). Used as `id`s: the editor's `data-mae-id`/testkit
/// match is its full current content (a hosted `<textarea>`'s whole value —
/// see `mae/src/imui/paint_dom.rs`), so checking which one is present is how
/// both drivers observe "which note is currently shown", with no risk of
/// colliding with the note *titles* shown elsewhere (the sidebar, the
/// top-bar title field) the way asserting on a title string alone would.
const WELCOME_BODY: &str = "# Welcome\n\n## First note\n\nThis is your first note! Let's try to play with it :)\n\n## First note\n\nThis is your first note! Let's try to play with it :)\n\n## First note\n\nThis is your first note! Let's try to play with it :)\n\n## First note\n\nThis is your first note! Let's try to play with it :)\n\n## First note\n\nThis is your first note! Let's try to play with it :)\n\n## First note\n\nThis is your first note! Let's try to play with it :)\n\n## First note\n\nThis is your first note! Let's try to play with it :)\n\n## First note\n\nThis is your first note! Let's try to play with it :)";
const PRODUCT_ROADMAP_BODY: &str =
    "# Product roadmap\n\nQ3 milestones and deliverables for the team.";

fn clicking_a_note_selects_it<D: UiDriver>(driver: &mut D) {
    assert!(
        driver.exists(WELCOME_BODY),
        "the initially active note's (Welcome) content should be in the editor"
    );
    driver.click("Product roadmap");
    assert!(
        driver.exists(PRODUCT_ROADMAP_BODY),
        "the editor should switch to the clicked note's content"
    );
}

enkr::driver_test!(
    clicking_a_note_selects_it,
    900.0,
    600.0,
    EnkrState::with_notes(NoteDatabase::demo())
);

/// A space pulled with the Reader role makes the editor read-only: B cannot
/// type into the synced note, so its content stays exactly as A authored it.
#[test]
fn reader_pulled_space_makes_editor_read_only() {
    let server = TestServer::start();
    let (mut a, mut b, synced_note_id) =
        establish_shared_pair_apps(&server, App::new(), App::new(), "bob", MemberRole::Reader);

    // B opens the synced note and waits until it has learned (from the
    // membership log fetched on join) that it is only a Reader in this space.
    b.click_sidebar_note("Welcome");
    assert_eq!(b.state.active_note_id, synced_note_id);
    pump_until(
        &mut [&mut a, &mut b],
        "B learns the shared space is read-only for it",
        |apps| {
            let state = &apps[1].state;
            state
                .notes
                .spaces()
                .iter()
                .find(|space| space.name == "Shared")
                .and_then(|space| space.remote)
                .zip(state.sync.as_ref())
                .is_some_and(|(remote, sync)| !sync.can_write(remote))
        },
    );

    // Typing into the read-only editor must be a no-op.
    let before = b
        .state
        .notes
        .note(&synced_note_id)
        .map(|note| note.text())
        .unwrap_or_default();
    assert!(
        before.contains("# Welcome"),
        "B should hold A's synced welcome content"
    );
    b.type_into_editor("READER_EDIT ");
    b.frame();
    let after = b
        .state
        .notes
        .note(&synced_note_id)
        .map(|note| note.text())
        .unwrap_or_default();
    assert_eq!(before, after, "reader edits must be ignored");
    assert!(!after.contains("READER_EDIT"));
}

/// A peer that never set a nickname still broadcasts presence. The original bug
/// dropped unnamed peers on both the send and decode paths, so a Mac↔Windows
/// pair where one side had no nickname could see only the named side's caret.
/// The unnamed peer now appears with a short device-id label.
#[test]
fn unnamed_peer_caret_is_visible_to_others() {
    let server = TestServer::start();
    let (mut a, mut b, b_note_id) =
        establish_shared_pair_apps(&server, App::new(), App::new(), "", MemberRole::Writer);

    // B set no nickname, so peers label it by the first 4 bytes of its device
    // key (hex(device_pk ‖ kex_pk) — the first 8 chars are device_pk[..4]).
    let b_label = b.state.sync.as_ref().unwrap().device_key()[..8].to_string();

    // B plants a caret by editing the shared note.
    b.click_sidebar_note("Welcome");
    assert_eq!(b.state.active_note_id, b_note_id);
    b.type_into_editor("HELLO_FROM_B ");

    // A receives B's edit (data path) and sees B's caret labeled with the
    // device id (presence path no longer drops the unnamed peer).
    pump_until(
        &mut [&mut a, &mut b],
        "A sees the unnamed peer's edit and caret",
        |apps| {
            if apps[0].note_text_containing("HELLO_FROM_B").is_none() {
                return false;
            }
            let a = &mut apps[0];
            let Some(doc) = a.state.notes.note("Welcome").and_then(|n| n.remote_doc()) else {
                return false;
            };
            let Some(sync) = a.state.sync.as_mut() else {
                return false;
            };
            let presences = sync.presence(&doc);
            let Some(note) = a.state.notes.note("Welcome") else {
                return false;
            };
            presences.iter().any(|p| {
                p.nickname == b_label
                    && p.caret
                        .as_ref()
                        .and_then(|sticky| note.caret_from_anchor(sticky))
                        .is_some()
            })
        },
    );
}

/// Switching focus to another note sends a leave on the doc you left, so a
/// peer's caret clears there immediately instead of lingering until the TTL.
#[test]
fn focusing_another_note_clears_presence_on_the_previous_doc() {
    let server = TestServer::start();
    let (mut a, mut b, _welcome) = establish_shared_pair(&server);

    // B plants a caret on Welcome; A sees bob there.
    b.click_sidebar_note("Welcome");
    b.type_into_editor("HELLO_FROM_B ");
    pump_until(&mut [&mut a, &mut b], "A sees bob on Welcome", |apps| {
        let a = &mut apps[0];
        let Some(doc) = a.state.notes.note("Welcome").and_then(|n| n.remote_doc()) else {
            return false;
        };
        let Some(sync) = a.state.sync.as_mut() else {
            return false;
        };
        sync.presence(&doc).iter().any(|p| p.nickname == "bob")
    });

    // B focuses a different note (a fresh note in the same space) → leaves
    // Welcome's doc.
    b.click("+  New note");
    b.frame();

    // A stops seeing bob on the Welcome doc well within the 30s TTL — proving
    // the explicit leave, not mere expiry (PHASE_TIMEOUT is 20s).
    pump_until(
        &mut [&mut a, &mut b],
        "bob's caret clears on Welcome",
        |apps| {
            let a = &mut apps[0];
            let Some(doc) = a.state.notes.note("Welcome").and_then(|n| n.remote_doc()) else {
                return false;
            };
            let Some(sync) = a.state.sync.as_mut() else {
                return false;
            };
            !sync.presence(&doc).iter().any(|p| p.nickname == "bob")
        },
    );
}

/// The durability gate's happy path: a blob that uploads cleanly is published
/// to the space index and reaches the peer with its content intact.
#[test]
fn durable_image_blob_reaches_the_peer() {
    let server = TestServer::start();
    let (mut a, mut b, _note) = establish_shared_pair(&server);

    let space = a.state.notes.default_space_id();
    let bytes = vec![3u8; 4096];
    let id = a.state.notes.create_blob_in(
        space,
        "pic.png",
        enkr_proto::wire::ImageMime::Png,
        bytes.clone(),
    );

    pump_until(
        &mut [&mut a, &mut b],
        "peer adopts the uploaded blob",
        |apps| {
            apps[1]
                .state
                .notes
                .blob(&id)
                .is_some_and(|blob| blob.bytes == bytes)
        },
    );
}

/// Regression: deleting an image locally must retract its advertisement from the
/// space index doc. Otherwise the index keeps resolving the `./blob/<name>` link
/// and a restart (or a peer) re-adopts and re-downloads the deleted image.
#[test]
fn deleted_image_blob_is_retracted_from_peers() {
    let server = TestServer::start();
    let (mut a, mut b, _note) = establish_shared_pair(&server);

    let space = a.state.notes.default_space_id();
    let bytes = vec![7u8; 4096];
    let id = a.state.notes.create_blob_in(
        space,
        "gone.png",
        enkr_proto::wire::ImageMime::Png,
        bytes.clone(),
    );

    // Both devices must first hold the uploaded, advertised blob.
    pump_until(&mut [&mut a, &mut b], "peer adopts the blob", |apps| {
        apps[1]
            .state
            .notes
            .blob(&id)
            .is_some_and(|blob| blob.bytes == bytes)
    });

    // Delete it locally the way the UI does (propagate into the index doc).
    let blob_uuid = uuid::Uuid::parse_str(&id).unwrap();
    if let Some(sync) = a.state.sync.as_mut() {
        sync.blob_deleted(&a.state.notes, space, blob_uuid);
    }
    a.state.notes.delete_blob(&id);

    pump_until(
        &mut [&mut a, &mut b],
        "blob deletion reaches the peer",
        |apps| apps[0].state.notes.blob(&id).is_none() && apps[1].state.notes.blob(&id).is_none(),
    );
}

/// Deleting an image locally must delete its sealed content from the relay too,
/// not just retract the index advertisement - otherwise the ciphertext is
/// orphaned in server storage forever.
#[test]
fn local_delete_removes_blob_content_from_server() {
    let server = TestServer::start();
    let (mut a, mut b, _note) = establish_shared_pair(&server);

    let space = a.state.notes.default_space_id();
    let bytes = vec![5u8; 4096];
    let id = a.state.notes.create_blob_in(
        space,
        "doomed.png",
        enkr_proto::wire::ImageMime::Png,
        bytes.clone(),
    );

    let blob_rows = || -> i64 {
        rusqlite::Connection::open(&server.db_path)
            .unwrap()
            .query_row("SELECT COUNT(*) FROM blobs", [], |r| r.get(0))
            .unwrap()
    };

    // B adopting the content proves the upload durably reached the server.
    pump_until(&mut [&mut a, &mut b], "blob stored on server", |apps| {
        apps[1]
            .state
            .notes
            .blob(&id)
            .is_some_and(|blob| blob.bytes == bytes)
    });
    assert_eq!(blob_rows(), 1, "server should hold the uploaded blob");

    // Delete locally the way the UI does.
    let blob_uuid = uuid::Uuid::parse_str(&id).unwrap();
    if let Some(sync) = a.state.sync.as_mut() {
        sync.blob_deleted(&a.state.notes, space, blob_uuid);
    }
    a.state.notes.delete_blob(&id);

    // The fire-and-forget DeleteBlob reaches the relay and drops the row.
    let deadline = Instant::now() + PHASE_TIMEOUT;
    while blob_rows() != 0 {
        assert!(
            Instant::now() < deadline,
            "server never dropped the deleted image's content"
        );
        a.frame();
        b.frame();
        std::thread::sleep(FRAME_PACE);
    }
}

/// Regression: a client that replays a space's full index backlog on reconnect
/// must NOT re-fetch images that were added and then deleted in that history.
/// The backlog delivers each historical frame separately, so adopting off the
/// intermediate (pre-deletion) map state would fire a wasteful `GetBlob` for
/// every deleted image on every reconnect. Only a caught-up (live) index is
/// authoritative enough to fetch from.
#[test]
fn reconnect_does_not_refetch_deleted_images() {
    let (server, blob_gets) = TestServer::start_counting_blob_gets();
    let (mut a, mut b, _note) = establish_shared_pair(&server);

    let space = a.state.notes.default_space_id();
    let bytes = vec![9u8; 4096];
    let id = a.state.notes.create_blob_in(
        space,
        "temp.png",
        enkr_proto::wire::ImageMime::Png,
        bytes.clone(),
    );

    // B fetches the freshly uploaded blob (its one and only legitimate GetBlob).
    pump_until(&mut [&mut a, &mut b], "B adopts the blob", |apps| {
        apps[1]
            .state
            .notes
            .blob(&id)
            .is_some_and(|blob| blob.bytes == bytes)
    });

    // A deletes it; both converge to no blob.
    let blob_uuid = uuid::Uuid::parse_str(&id).unwrap();
    if let Some(sync) = a.state.sync.as_mut() {
        sync.blob_deleted(&a.state.notes, space, blob_uuid);
    }
    a.state.notes.delete_blob(&id);
    pump_until(&mut [&mut a, &mut b], "blob deletion converges", |apps| {
        apps[0].state.notes.blob(&id).is_none() && apps[1].state.notes.blob(&id).is_none()
    });

    let baseline = blob_gets.load(Ordering::Relaxed);

    // B reconnects: a fresh sync engine (it persists no seq state) replays the
    // entire index history from seq 0 - the add-blob frame followed by the
    // remove-blob frame.
    b.disconnect();
    b.reconnect();
    pump_until(&mut [&mut a, &mut b], "B reconnects", |apps| {
        apps[1].connected()
    });

    // Settle so any erroneous backlog-time GetBlob would have been issued and
    // its BlobData response counted.
    let settle = Instant::now() + Duration::from_millis(1000);
    while Instant::now() < settle {
        a.frame();
        b.frame();
        std::thread::sleep(FRAME_PACE);
    }

    assert!(
        b.state.notes.blob(&id).is_none(),
        "a deleted image must not resurrect after reconnect"
    );
    assert_eq!(
        blob_gets.load(Ordering::Relaxed),
        baseline,
        "reconnect re-fetched an image the index no longer advertises"
    );
}

/// The durability gate's whole point: a blob the relay never stored (quarantined
/// after repeated refusals) must NOT be advertised in the space index, so a peer
/// never resolves the link to a phantom the relay can't serve.
#[test]
fn quarantined_blob_is_not_advertised_to_peers() {
    let mut config = ServerConfig::default();
    config.max_frame_bytes = 64 * 1024;
    let server = TestServer::start_with(config);
    let (mut a, mut b, _note) = establish_shared_pair(&server);

    let space = a.state.notes.default_space_id();
    let big = vec![9u8; 256 * 1024];
    let id = a
        .state
        .notes
        .create_blob_in(space, "big.png", enkr_proto::wire::ImageMime::Png, big);

    // A gives up on the unshippable blob (bounded retries).
    pump_until(&mut [&mut a, &mut b], "A quarantines the blob", |apps| {
        apps[0]
            .state
            .sync
            .as_ref()
            .and_then(|s| s.last_error())
            .is_some_and(|e| e.contains("couldn't sync image"))
    });

    // Give B ample time to (not) receive it, then assert no phantom entry.
    for _ in 0..80 {
        a.frame();
        b.frame();
    }
    assert!(
        b.state.notes.blob(&id).is_none(),
        "a blob the relay never stored must not appear in a peer's index"
    );
}

/// Regression: a blob whose frame the relay keeps refusing (an nginx proxy
/// capping frames below the client's 16 MiB ceiling closes the socket instead
/// of returning `BlobTooLarge`) must not wedge the app in a connect/disconnect
/// loop. After a few failed attempts the blob is quarantined and the connection
/// stays usable for everything else.
#[test]
fn blob_the_relay_keeps_refusing_is_quarantined_not_retried_forever() {
    // Frame limit far below the blob, but above handshake/doc traffic, so only
    // the blob upload trips it - exactly what a proxy frame cap does.
    let mut config = ServerConfig::default();
    config.max_frame_bytes = 64 * 1024;
    let server = TestServer::start_with(config);
    let (mut a, mut b, synced_note_id) = establish_shared_pair(&server);

    // Inject an image blob into A's (synced) space: too big for the relay's
    // frame, well under the client's own pre-check, so the client ships it and
    // the relay drops the connection with no graceful error.
    let space = a.state.notes.default_space_id();
    let bytes = vec![7u8; 256 * 1024];
    a.state
        .notes
        .create_blob_in(space, "big.png", enkr_proto::wire::ImageMime::Png, bytes);

    // The reship loop retries a bounded number of times, then quarantines it.
    pump_until(
        &mut [&mut a],
        "blob quarantined after bounded retries",
        |apps| {
            apps[0]
                .state
                .sync
                .as_ref()
                .and_then(|s| s.last_error())
                .is_some_and(|e| e.contains("couldn't sync image"))
        },
    );

    // The failure surfaces to the user as a toast (drained from the sync layer
    // into ui.toast during render).
    let snap = a.frame();
    assert!(
        snap.nodes.iter().any(|n| n
            .text
            .as_deref()
            .is_some_and(|t| t.contains("Upload failed"))),
        "a failure toast should be shown:\n{}",
        snap.debug_dump()
    );

    // The connection is not wedged: a normal edit on A still reaches B.
    a.click_sidebar_note("Welcome");
    assert_eq!(a.state.active_note_id, "Welcome");
    a.type_into_editor("AFTER_BLOB ");
    pump_until(
        &mut [&mut a, &mut b],
        "A's edit still syncs to B after the blob failure",
        |apps| apps[1].note_text_containing("AFTER_BLOB").is_some(),
    );
    let _ = synced_note_id;
    assert!(a.connected(), "connection wedged after quarantine");
}

/// PLAN §6 base scenario, end to end through the real widgets.
#[test]
fn two_clients_share_a_space_through_the_ui() {
    let server = TestServer::start();
    let (mut a, mut b, synced_note_id) = establish_shared_pair(&server);

    // -- B edits the note; A sees the modification ----------------------------
    b.click_sidebar_note("Welcome");
    assert_eq!(b.state.active_note_id, synced_note_id);
    b.type_into_editor("EDIT_FROM_B ");
    pump_until(&mut [&mut a, &mut b], "A receives B's edit", |apps| {
        apps[0].note_text_containing("EDIT_FROM_B").is_some()
    });

    // Presence: B's caret sits in the shared doc and pinged moments ago, so
    // A should see "bob" (checked before it can expire).
    {
        let doc = a
            .state
            .notes
            .note("Welcome")
            .and_then(|note| note.remote_doc())
            .expect("mapped note");
        let a_sync = a.state.sync.as_mut().expect("A sync");
        let presence = a_sync.presence(&doc);
        assert!(
            presence.iter().any(|p| p.nickname == "bob"),
            "A should see bob's presence on the shared note"
        );
    }

    // B selects three chars (Shift+Left ×3); A resolves bob's selection on
    // its own replica as a 3-char range ending at bob's caret.
    for _ in 0..3 {
        b.harness.key_press_with_flags(
            mae::os::OSKeyCode::KeyLeftArrow,
            mae::os::OSEventFlag::Shift,
        );
    }
    b.frame();
    pump_until(&mut [&mut a, &mut b], "A sees bob's selection", |apps| {
        let a_app = &mut apps[0];
        let Some(doc) = a_app
            .state
            .notes
            .note("Welcome")
            .and_then(|note| note.remote_doc())
        else {
            return false;
        };
        let Some(sync) = a_app.state.sync.as_mut() else {
            return false;
        };
        let presences = sync.presence(&doc);
        let Some(note) = a_app.state.notes.note("Welcome") else {
            return false;
        };
        presences.iter().any(|p| {
            p.nickname == "bob"
                && p.caret
                    .as_ref()
                    .and_then(|sticky| note.caret_from_anchor(sticky))
                    .zip(
                        p.selection_anchor
                            .as_ref()
                            .and_then(|sticky| note.caret_from_anchor(sticky)),
                    )
                    .is_some_and(|(caret, anchor)| anchor.abs_diff(caret) == 3)
        })
    });

    // -- A edits as well; B receives it ----------------------------------------
    a.click_sidebar_note("Welcome");
    a.type_into_editor("EDIT_FROM_A ");
    pump_until(&mut [&mut a, &mut b], "B receives A's edit", |apps| {
        apps[1].note_text_containing("EDIT_FROM_A").is_some()
    });

    // -- B disconnects; A keeps editing; B reconnects and catches up ----------
    b.disconnect();
    a.type_into_editor("WHILE_B_OFFLINE ");
    // Let A flush + push while B is down.
    let until = Instant::now() + Duration::from_secs(2);
    while Instant::now() < until {
        a.frame();
        std::thread::sleep(FRAME_PACE);
    }
    assert!(b.note_text_containing("WHILE_B_OFFLINE").is_none());

    b.reconnect();
    pump_until(
        &mut [&mut a, &mut b],
        "B catches up on offline edits after reconnect",
        |apps| apps[1].note_text_containing("WHILE_B_OFFLINE").is_some(),
    );

    // Both replicas hold all three markers.
    for app in [&a, &b] {
        let text = app
            .note_text_containing("WHILE_B_OFFLINE")
            .expect("converged note");
        assert!(text.contains("EDIT_FROM_A"));
        assert!(text.contains("EDIT_FROM_B"));
    }
}

/// A device that joins a shared space and never edits anything must still
/// settle to green: its docs are receive-only, so the engine produces no
/// busy→idle edge for them, and the indicator must not wait for one.
#[test]
fn joined_space_settles_to_synchronized_without_local_edits() {
    let server = TestServer::start();
    let (mut a, mut b, b_note_id) = establish_shared_pair(&server);

    pump_until(
        &mut [&mut a, &mut b],
        "B's mirrored space + note turn Synchronized without B ever editing",
        |apps| {
            let state = &apps[1].state;
            let sync = state.sync.as_ref().unwrap();
            let note_green = state.notes.note(&b_note_id).is_some_and(|note| {
                sync.note_indicator(note) == enkr::sync::app::SyncIndicator::Synchronized
            });
            let space_green = state
                .notes
                .spaces()
                .iter()
                .find(|space| space.name == "Shared")
                .is_some_and(|space| {
                    sync.space_indicator(&state.notes, space.id)
                        == enkr::sync::app::SyncIndicator::Synchronized
                });
            note_green && space_green
        },
    );
}

/// Deleting a synced space locally and then re-fetching it must re-pull the
/// notes — not just recreate an empty shell. Regression: the sync runtime kept
/// the old keys, index replica, and doc subscriptions after a local delete, so
/// the re-join short-circuited and no content came back.
#[test]
fn refetch_after_local_delete_repulls_notes() {
    let server = TestServer::start();
    let (mut a, mut b, _b_note_id) = establish_shared_pair(&server);

    // B deletes its local mirror of the shared space (right-click → Delete).
    b.switch_to_space("Shared");
    b.harness.right_click("###enkr_space_switcher");
    b.frame();
    b.click("Delete");
    assert!(
        b.state.notes.spaces().iter().all(|s| s.name != "Shared"),
        "local mirror should be gone after delete"
    );
    assert!(
        synced_note_with(&b.state, "# Welcome").is_none(),
        "synced note should be gone after delete"
    );

    // B re-fetches the same remote space from the sync window. B is still a
    // member server-side, so the space reappears in the remote list with a
    // fresh "Sync" button.
    b.open_sync_from_pill();
    let deadline = Instant::now() + PHASE_TIMEOUT;
    loop {
        b.click("Refresh");
        for _ in 0..6 {
            a.frame();
            b.frame();
            std::thread::sleep(FRAME_PACE);
        }
        if b.frame().try_node("Sync").is_some() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "B never saw the shared space in its remote list again"
        );
    }
    b.click("Sync");
    pump_until(
        &mut [&mut a, &mut b],
        "B re-mirrors the space + note content after re-fetch",
        |apps| {
            let b = &apps[1].state;
            b.notes.spaces().iter().any(|space| space.name == "Shared")
                && synced_note_with(b, "# Welcome").is_some()
        },
    );
}

/// The owner deleting a synced space destroys it server-side, but every member
/// (the owner included) *keeps* its local copy and notes — the space is merely
/// unsynced (remote link severed) rather than removed. The space also drops out
/// of the remote list immediately, without a manual Refresh.
#[test]
fn owner_delete_space_unsyncs_it_for_all_members() {
    let server = TestServer::start();
    let (mut a, mut b, b_note_id) = establish_shared_pair(&server);
    let a_space = a.state.notes.default_space_id();
    let remote = a
        .state
        .notes
        .space_remote(a_space)
        .expect("A's space is synced");

    // Populate A's remote list so we can watch the space drop out of it.
    a.state.sync.as_mut().unwrap().refresh_remote_spaces();
    pump_until(&mut [&mut a, &mut b], "A lists the shared space", |apps| {
        apps[0]
            .state
            .sync
            .as_ref()
            .unwrap()
            .remote_spaces(&apps[0].state.notes)
            .iter()
            .any(|r| r.space_id == remote)
    });

    // A is the space owner: request a delete-for-everyone.
    a.state.sync.as_mut().unwrap().delete_remote_space(remote);

    // B keeps the "Shared" space and its synced note, but the space is now a
    // plain local-only space (no remote binding).
    pump_until(
        &mut [&mut a, &mut b],
        "B's mirror is unsynced but its content stays",
        |apps| {
            let b = &apps[1].state;
            let shared = b.notes.spaces().iter().find(|s| s.name == "Shared");
            shared.is_some_and(|s| s.remote.is_none()) && b.notes.note(&b_note_id).is_some()
        },
    );

    // The owner keeps its own copy too, now local-only...
    let a_shared = a.state.notes.spaces().iter().find(|s| s.name == "Shared");
    assert!(
        a_shared.is_some_and(|s| s.remote.is_none()),
        "owner keeps the space as local-only"
    );
    // ...and the deleted space is gone from the remote list right away — no
    // Refresh needed.
    assert!(
        a.state
            .sync
            .as_ref()
            .unwrap()
            .remote_spaces(&a.state.notes)
            .iter()
            .all(|r| r.space_id != remote),
        "deleted space drops out of the remote list immediately"
    );
}

/// A non-owner cannot destroy a shared space: the server rejects the delete,
/// so every member keeps it.
#[test]
fn non_owner_cannot_delete_space() {
    let server = TestServer::start();
    let (mut a, mut b, _b_note_id) = establish_shared_pair(&server);
    let b_space = b
        .state
        .notes
        .spaces()
        .iter()
        .find(|s| s.name == "Shared")
        .map(|s| s.id)
        .expect("B mirrors the shared space");
    let remote = b.state.notes.space_remote(b_space).expect("synced");

    // B joined as a Writer (see `establish_shared_pair`): not an owner.
    b.state.sync.as_mut().unwrap().delete_remote_space(remote);

    // Give the rejected request time to round-trip; nothing should be removed.
    for _ in 0..20 {
        a.frame();
        b.frame();
        std::thread::sleep(FRAME_PACE);
    }
    assert!(
        a.state.notes.spaces().iter().any(|s| s.name == "Shared"),
        "owner must still have the space"
    );
    assert!(
        b.state.notes.spaces().iter().any(|s| s.name == "Shared"),
        "writer must still have the space"
    );
}

#[test]
fn space_rename_syncs_between_clients() {
    let server = TestServer::start();
    let (mut a, mut b, _b_note_id) = establish_shared_pair(&server);

    a.state
        .notes
        .rename_space(a.state.notes.default_space_id(), "Renamed");

    pump_until(&mut [&mut a, &mut b], "space rename reaches B", |apps| {
        apps[1]
            .state
            .notes
            .spaces()
            .iter()
            .any(|space| space.name == "Renamed")
    });
}

#[test]
fn folders_sync_between_clients() {
    let server = TestServer::start();
    let (mut a, mut b, b_note_id) = establish_shared_pair(&server);
    let a_space = a.state.notes.default_space_id();
    let folder = a
        .state
        .notes
        .create_folder(a_space, "Projects")
        .expect("create folder");
    let child = a
        .state
        .notes
        .create_folder_in(a_space, Some(folder), "Specs")
        .expect("create child folder");
    a.state.notes.set_note_folder("Welcome", Some(child));

    pump_until(
        &mut [&mut a, &mut b],
        "folder and note assignment reach B",
        |apps| {
            let b_state = &apps[1].state;
            let Some(b_space) = b_state
                .notes
                .spaces()
                .iter()
                .find(|space| space.name == "Shared")
                .map(|space| space.id)
            else {
                return false;
            };
            let Some(folder) = b_state
                .notes
                .folders_in_space(b_space)
                .find(|folder| folder.name == "Projects")
                .map(|folder| folder.id)
            else {
                return false;
            };
            let Some(child) = b_state
                .notes
                .folders_in_space(b_space)
                .find(|candidate| candidate.name == "Specs" && candidate.parent == Some(folder))
                .map(|candidate| candidate.id)
            else {
                return false;
            };
            b_state
                .notes
                .note(&b_note_id)
                .is_some_and(|note| note.folder() == Some(child))
        },
    );

    a.state.notes.rename_folder(&folder, "Archive");
    pump_until(&mut [&mut a, &mut b], "folder rename reaches B", |apps| {
        let b_state = &apps[1].state;
        let Some(b_space) = b_state
            .notes
            .spaces()
            .iter()
            .find(|space| space.name == "Shared")
            .map(|space| space.id)
        else {
            return false;
        };
        b_state
            .notes
            .folders_in_space(b_space)
            .any(|folder| folder.name == "Archive")
    });

    if let Some(sync) = a.state.sync.as_mut() {
        sync.folder_deleted(&a.state.notes, a_space, folder);
    }
    a.state.notes.delete_folder(&folder);
    pump_until(&mut [&mut a, &mut b], "folder deletion reaches B", |apps| {
        let b_state = &apps[1].state;
        let Some(b_space) = b_state
            .notes
            .spaces()
            .iter()
            .find(|space| space.name == "Shared")
            .map(|space| space.id)
        else {
            return false;
        };
        b_state.notes.folders_in_space(b_space).next().is_none()
            && b_state
                .notes
                .note(&b_note_id)
                .is_some_and(|note| note.folder().is_none())
    });
}

/// The failproof requirement: two clients hammering the same note at the same
/// time, without ever waiting for each other, must converge to identical
/// content — and a green "Synchronized" indicator must not lie about it.
#[test]
fn simultaneous_typing_converges() {
    let server = TestServer::start();
    let (mut a, mut b, b_note_id) = establish_shared_pair(&server);

    a.click_sidebar_note("Welcome");
    b.click_sidebar_note("Welcome");
    a.focus_editor();
    b.focus_editor();

    // Interleaved bursts with no convergence waits in between. The pacing
    // varies so flush debounces, engine debounces and diff round-trips
    // interleave differently every few iterations.
    const ROUNDS: usize = 60;
    for i in 0..ROUNDS {
        a.harness.type_text(&format!("a{i} "));
        a.frame();
        b.harness.type_text(&format!("b{i} "));
        b.frame();
        std::thread::sleep(Duration::from_millis((i % 4) as u64 * 3));
    }

    let texts = |apps: &[&mut App]| -> (String, String) {
        let a_text = apps[0]
            .state
            .notes
            .note("Welcome")
            .map(|n| n.text())
            .unwrap_or_default();
        let b_text = apps[1]
            .state
            .notes
            .note(&b_note_id)
            .map(|n| n.text())
            .unwrap_or_default();
        (a_text, b_text)
    };

    let mut tick = 0u32;
    pump_until(
        &mut [&mut a, &mut b],
        "simultaneous edits converge to identical text",
        |apps| {
            let (a_text, b_text) = texts(apps);
            tick += 1;
            if tick % 120 == 0 {
                let missing_a: Vec<usize> = (0..ROUNDS)
                    .filter(|i| {
                        !a_text.contains(&format!("a{i} ")) || !a_text.contains(&format!("b{i} "))
                    })
                    .collect();
                let missing_b: Vec<usize> = (0..ROUNDS)
                    .filter(|i| {
                        !b_text.contains(&format!("a{i} ")) || !b_text.contains(&format!("b{i} "))
                    })
                    .collect();
                eprintln!(
                    "tick {tick}: eq={} a_len={} b_len={} a_missing={:?} b_missing={:?} a_pend={} b_pend={} a_err={:?} b_err={:?}",
                    a_text == b_text,
                    a_text.len(),
                    b_text.len(),
                    missing_a,
                    missing_b,
                    apps[0].state.sync.as_ref().is_some_and(|s| s.has_pending()),
                    apps[1].state.sync.as_ref().is_some_and(|s| s.has_pending()),
                    apps[0].state.sync.as_ref().and_then(|s| s.last_error()),
                    apps[1].state.sync.as_ref().and_then(|s| s.last_error()),
                );
            }
            a_text == b_text
                && (0..ROUNDS).all(|i| {
                    a_text.contains(&format!("a{i} ")) && a_text.contains(&format!("b{i} "))
                })
                && apps.iter().all(|app| {
                    // Fully settled: live, acknowledged, nothing flagged.
                    let state = &app.state;
                    let sync = state.sync.as_ref().unwrap();
                    !sync.has_pending()
                        && state.notes.summaries().iter().all(|summary| {
                            state.notes.note(&summary.id).is_none_or(|note| {
                                note.remote_doc().is_none()
                                    || sync.note_indicator(note)
                                        == enkr::sync::app::SyncIndicator::Synchronized
                            })
                        })
                })
        },
    );

    // Green must mean green: with everything idle, both indicators are
    // Synchronized and the contents are byte-identical.
    let (a_text, b_text) = {
        let a_text = a.state.notes.note("Welcome").map(|n| n.text()).unwrap();
        let b_text = b.state.notes.note(&b_note_id).map(|n| n.text()).unwrap();
        (a_text, b_text)
    };
    assert_eq!(a_text, b_text);
    for (app, id) in [(&a, "Welcome"), (&b, b_note_id.as_str())] {
        let note = app.state.notes.note(id).unwrap();
        let indicator = app.state.sync.as_ref().unwrap().note_indicator(note);
        assert_eq!(
            indicator,
            enkr::sync::app::SyncIndicator::Synchronized,
            "note indicator must be Synchronized after convergence"
        );
    }

    // Quiescence: nobody is editing, so the server log must stop growing
    // (no echo loops between the UI and engine replicas).
    let count_updates = || -> i64 {
        rusqlite::Connection::open(&server.db_path)
            .unwrap()
            .query_row("SELECT COUNT(*) FROM updates", [], |r| r.get(0))
            .unwrap()
    };
    // Drain any in-flight tail first.
    let settle = Instant::now() + Duration::from_millis(800);
    while Instant::now() < settle {
        a.frame();
        b.frame();
        std::thread::sleep(FRAME_PACE);
    }
    let before = count_updates();
    let idle = Instant::now() + Duration::from_secs(1);
    while Instant::now() < idle {
        a.frame();
        b.frame();
        std::thread::sleep(FRAME_PACE);
    }
    assert_eq!(
        count_updates(),
        before,
        "server log keeps growing while both clients are idle (echo loop)"
    );
}

/// Reconnect under fire: B drops mid-stream, keeps typing offline, and comes
/// back while A is still typing. Nothing may be lost in the adopt/reopen
/// window — this was the historical silent-drop path.
#[test]
fn typing_through_reconnect_converges() {
    let server = TestServer::start();
    let (mut a, mut b, b_note_id) = establish_shared_pair(&server);

    a.click_sidebar_note("Welcome");
    b.click_sidebar_note("Welcome");
    a.focus_editor();
    b.focus_editor();

    const ROUNDS: usize = 30;
    for i in 0..ROUNDS {
        if i == 8 {
            b.disconnect();
        }
        if i == 20 {
            // Reconnect through settings while A keeps typing.
            b.reconnect();
            // The settings round-trip steals editor focus; restore it.
            b.click_sidebar_note("Welcome");
            b.focus_editor();
        }
        a.harness.type_text(&format!("a{i} "));
        a.frame();
        b.harness.type_text(&format!("b{i} "));
        b.frame();
        std::thread::sleep(Duration::from_millis(5));
    }

    let mut tick = 0u32;
    pump_until(
        &mut [&mut a, &mut b],
        "edits across a reconnect converge to identical text",
        |apps| {
            let a_text = apps[0]
                .state
                .notes
                .note("Welcome")
                .map(|n| n.text())
                .unwrap_or_default();
            let b_text = apps[1]
                .state
                .notes
                .note(&b_note_id)
                .map(|n| n.text())
                .unwrap_or_default();
            tick += 1;
            if tick % 120 == 0 {
                let missing_a: Vec<usize> = (0..ROUNDS)
                    .filter(|i| {
                        !a_text.contains(&format!("a{i} ")) || !a_text.contains(&format!("b{i} "))
                    })
                    .collect();
                let missing_b: Vec<usize> = (0..ROUNDS)
                    .filter(|i| {
                        !b_text.contains(&format!("a{i} ")) || !b_text.contains(&format!("b{i} "))
                    })
                    .collect();
                eprintln!(
                    "tick {tick}: eq={} a_missing={missing_a:?} b_missing={missing_b:?} a_err={:?} b_err={:?} b_conn={}",
                    a_text == b_text,
                    apps[0].state.sync.as_ref().and_then(|s| s.last_error()),
                    apps[1].state.sync.as_ref().and_then(|s| s.last_error()),
                    apps[1].connected(),
                );
            }
            a_text == b_text
                && (0..ROUNDS).all(|i| {
                    a_text.contains(&format!("a{i} ")) && a_text.contains(&format!("b{i} "))
                })
        },
    );
}

/// Crash recovery (the `needs_push` durability contract): B edits offline,
/// the app dies without a clean shutdown, and a fresh "process" booting from
/// the same note database + device key must reship the flagged content on
/// its first connect — nothing depends on engine-side persistence (there is
/// none anymore).
#[test]
fn crashed_client_reships_offline_edits_on_next_boot() {
    let server = TestServer::start();
    let notes_db =
        std::env::temp_dir().join(format!("enkr_app_sync_crash_{}.sqlite3", Uuid::new_v4()));
    let key_path = std::env::temp_dir().join(format!("enkr_app_sync_crash_{}.key", Uuid::new_v4()));
    let b = App::with_files(Some(notes_db.clone()), key_path.clone());
    let (mut a, mut b, _b_note_id) =
        establish_shared_pair_apps(&server, App::new(), b, "bob", MemberRole::Writer);

    // B goes offline and edits; the autosave persists content + needs_push.
    b.disconnect();
    b.click_sidebar_note("Welcome");
    b.type_into_editor("CRASH_EDIT ");
    let settle = Instant::now() + Duration::from_millis(1200);
    while Instant::now() < settle {
        b.frame();
        std::thread::sleep(FRAME_PACE);
    }

    // "Crash": tear B down without EnkrState::shutdown(), keeping its files.
    b.cleanup = false;
    drop(b);
    assert!(a.note_text_containing("CRASH_EDIT").is_none());

    // Fresh boot from the same files: the persisted server URL auto-connects,
    // adopt re-joins the space, and the flagged note reships.
    let mut b2 = App::with_files(Some(notes_db.clone()), key_path.clone());
    pump_until(
        &mut [&mut a, &mut b2],
        "crashed client's offline edit reaches A after reboot",
        |apps| apps[0].note_text_containing("CRASH_EDIT").is_some(),
    );

    drop(b2); // cleanup=true removes the shared files
}

/// Regression (PLAN-account.md §2): a space is bound to exactly one server.
/// Promote a space to server A, then switch the active server to a *different*
/// server B and reconnect — the space must stay bound to A and must never be
/// re-pushed onto B (the old bug switched the global server and re-shipped
/// every synced space to the new one).
#[test]
fn space_bound_to_one_server_is_not_pushed_to_another() {
    let server_a = TestServer::start();
    let server_b = TestServer::start();
    let mut app = App::new();
    app.state
        .notes
        .rename_space(app.state.notes.default_space_id(), "Shared");
    let space_id = app.state.notes.default_space_id();

    // -- Connect to A and sync the space there via the right-click submenu ----
    app.connect(&server_a.url(), "ana");
    pump_until(&mut [&mut app], "connected to A", |apps| {
        apps[0].connected()
    });
    app.frame();
    app.harness.right_click("###enkr_space_switcher");
    app.frame();
    app.click("Sync this space\u{2026} >");
    app.click(&format!("{}  (active)", server_a.url()));
    pump_until(&mut [&mut app], "space pushed to A", |apps| {
        apps[0].state.notes.space_remote(space_id).is_some()
    });
    let remote_on_a = app.state.notes.space_remote(space_id);
    assert_eq!(
        app.state.notes.space_server(space_id),
        Some(server_a.url().as_str()),
        "space should be bound to server A"
    );

    // -- Switch the active server to B and reconnect --------------------------
    app.disconnect();
    app.state.active_server = server_b.url();
    app.state.connect_sync();
    pump_until(&mut [&mut app], "connected to B", |apps| {
        apps[0].connected()
    });
    // Give any (erroneous) adopt/re-push pass ample time to run.
    let until = Instant::now() + Duration::from_secs(2);
    while Instant::now() < until {
        app.frame();
        std::thread::sleep(FRAME_PACE);
    }

    // The binding is untouched: still server A, same remote id.
    assert_eq!(
        app.state.notes.space_remote(space_id),
        remote_on_a,
        "remote id must not change when the active server switches"
    );
    assert_eq!(
        app.state.notes.space_server(space_id),
        Some(server_a.url().as_str()),
        "space must stay bound to server A"
    );

    // Server B must hold no spaces for this device — nothing was pushed to it.
    app.state.sync.as_mut().unwrap().refresh_remote_spaces();
    let until = Instant::now() + Duration::from_secs(1);
    while Instant::now() < until {
        app.frame();
        std::thread::sleep(FRAME_PACE);
    }
    let remote_spaces = app
        .state
        .sync
        .as_ref()
        .unwrap()
        .remote_spaces(&app.state.notes);
    assert!(
        remote_spaces.is_empty(),
        "server B must hold no spaces for this device, found {remote_spaces:?}"
    );
}

/// A fetched space arrives with its notes *named*.
///
/// The space index doc is a `doc-uuid -> title` map, and the fetch path read
/// the title and threw it away — so a pulled space showed a list of untitled
/// notes. Nothing caught it because every existing assertion checked note
/// *content* (`# Welcome` in the body) rather than the title, and content
/// travels by a different route: the note's own Yrs doc.
#[test]
fn fetching_a_space_pulls_note_titles_and_later_renames() {
    let server = TestServer::start();
    let (mut a, mut b, synced_note_id) = establish_shared_pair(&server);

    let a_title = a
        .state
        .notes
        .note_title("Welcome")
        .expect("A's note")
        .to_string();
    assert_eq!(a_title, "Welcome", "precondition: A's note is titled");

    assert_eq!(
        b.state.notes.note_title(&synced_note_id),
        Some(a_title.as_str()),
        "B's fetched copy should carry the same title, not an untitled placeholder"
    );

    // A rename on one side reaches the other through the same map.
    a.state.notes.set_note_title("Welcome", "Renamed by A");
    pump_until(&mut [&mut a, &mut b], "B sees the rename", |apps| {
        apps[1]
            .state
            .notes
            .note_title(&synced_note_id)
            .is_some_and(|title| title == "Renamed by A")
    });
}

/// A member can see a remote space's *name* before choosing to sync it, and
/// can delete a space they own without holding a local copy.
///
/// The name lives in the space's encrypted index doc, so the server cannot
/// supply it — the listing was bare uuids until something decrypted one, which
/// only happened after syncing. Peeking fetches the keys and index without
/// creating a local mirror.
#[test]
fn a_remote_space_shows_its_name_before_being_synced() {
    let server = TestServer::start();
    let (mut a, mut b, _note) = establish_shared_pair(&server);

    // B drops its local mirror but stays a member server-side.
    let shared = b
        .state
        .notes
        .spaces()
        .iter()
        .find(|s| s.name == "Shared")
        .map(|s| s.id)
        .expect("B mirrors the shared space");
    // Through the UI, as a user would: right-click the space, Delete.
    b.switch_to_space("Shared");
    b.harness.right_click("###enkr_space_switcher");
    b.frame();
    b.click("Delete");
    b.frame();
    let _ = shared;
    assert!(
        b.state.notes.spaces().iter().all(|s| s.name != "Shared"),
        "precondition: no local mirror"
    );

    // The remote listing must still be able to name it.
    b.open_sync_from_pill();
    let remote_id = a
        .state
        .notes
        .spaces()
        .iter()
        .find(|s| s.name == "Shared")
        .and_then(|s| s.remote)
        .expect("A's space is synced");
    pump_until(&mut [&mut a, &mut b], "B learns the space's name", |apps| {
        apps[1]
            .state
            .sync
            .as_ref()
            .and_then(|sync| sync.remote_space_name(remote_id))
            .is_some_and(|name| name == "Shared")
    });
}

/// Reconnecting retires "not connected".
///
/// `last_error` is one channel for two kinds of thing: a request that failed
/// because the socket was down, and a condition that stays true until someone
/// acts on it. Showing the first after reconnecting put "not connected to sync
/// server" in Settings directly beneath a green "Connected".
///
/// The other half — that a *durable* error survives — is covered by
/// `blob_the_relay_keeps_refusing_is_quarantined_not_retried_forever`, which
/// fails outright if reconnecting clears everything. It cannot be asserted here
/// because disconnecting through the UI drops the engine entirely, so a
/// reconnect starts from a clean one.
#[test]
fn reconnecting_clears_a_disconnect_error() {
    let server = TestServer::start();
    let (mut a, mut b, _note) = establish_shared_pair(&server);

    // Going offline makes in-flight work fail with `Disconnected`.
    b.disconnect();
    b.state.notes.create_note();
    b.frame();
    b.connect(&server.url(), "bob");
    pump_until(&mut [&mut a, &mut b], "B reconnects", |apps| {
        apps[1].connected()
    });
    assert_eq!(
        b.state.sync.as_ref().and_then(|s| s.last_error()),
        None,
        "a disconnect error must not outlive the disconnect"
    );
}

/// The recovery phrase is offered once, on first connect, and can be shown
/// again afterwards.
///
/// The prompt fires on connect rather than at first launch because that is when
/// the key is created — a local-only install has no identity and nothing at
/// stake. And it must fire exactly once: a warning that reappears every launch
/// is one people learn to dismiss without reading.
#[test]
fn the_recovery_phrase_is_offered_on_first_connect_and_only_once() {
    let server = TestServer::start();
    let mut app = App::first_run();

    // Nothing to back up before there is a key.
    assert!(
        app.state.identity_store.is_none(),
        "an identity should not exist before connecting"
    );

    app.connect(&server.url(), "ana");
    pump_until(&mut [&mut app], "connected", |apps| apps[0].connected());

    let snap = app.frame();
    assert!(
        snap.try_node("###enkr_recovery_phrase").is_some(),
        "first connect should offer the recovery phrase"
    );
    // Twelve words, and the same ones the identity actually derives from.
    let phrase =
        enkr::sync::recovery_phrase(app.state.identity_store.as_ref().unwrap()).expect("phrase");
    assert_eq!(phrase.split_whitespace().count(), 12, "{phrase}");

    app.click("###enkr_recovery_ack");
    let snap = app.frame();
    assert!(
        snap.try_node("###enkr_recovery_phrase").is_none(),
        "acknowledging should dismiss it"
    );

    // Reconnecting must not ask again.
    app.disconnect();
    app.reconnect();
    pump_until(&mut [&mut app], "reconnected", |apps| apps[0].connected());
    assert!(
        app.frame().try_node("###enkr_recovery_phrase").is_none(),
        "the prompt reappeared after it had been acknowledged"
    );

    // ...but it stays reachable from Settings.
    app.click(SETTINGS_ICON);
    app.frame();
    app.click("###enkr_settings_show_phrase");
    assert!(
        app.frame().try_node("###enkr_recovery_phrase").is_some(),
        "Settings should be able to show the phrase again"
    );
}

/// A relay that requires an account refuses to create a space for a device that
/// has no token — and the client must *un-mark* the space rather than leave it
/// looking synced.
///
/// The client's `create_space` is fire-and-forget: it binds the space to a
/// remote id and a server immediately, before the relay has said yes. So a
/// refusal that is only logged leaves a space that claims to sync, shows a sync
/// status in the sidebar, and silently never reaches the server — the worst
/// possible outcome for something a user chose deliberately.
#[test]
fn a_space_refused_for_want_of_an_account_is_unmarked_and_reported() {
    let server = TestServer::start_with(ServerConfig {
        require_account: true,
        ..ServerConfig::default()
    });
    let mut app = App::new();
    // The handshake still succeeds: the account gate is at CreateSpace, so an
    // invited collaborator (who never has a token) can still sync.
    app.connect(&server.url(), "ana");
    pump_until(&mut [&mut app], "connected", |apps| apps[0].connected());

    let space_id = app.state.notes.default_space_id();
    app.frame();
    app.harness.right_click("###enkr_space_switcher");
    app.frame();
    app.click("Sync this space\u{2026} >");
    app.click(&format!("{}  (active)", server.url()));

    // The refusal is fast enough that the optimistic binding can come and go
    // between two pumps, so this waits on the *end* state and proves it is not
    // vacuous with the contrast case below rather than by catching a flicker.
    pump_until(&mut [&mut app], "refused space is un-marked", |apps| {
        apps[0].state.notes.space_remote(space_id).is_none()
            && apps[0]
                .state
                .sync
                .as_ref()
                .is_some_and(|s| s.last_error().is_some())
    });
    assert!(
        app.state.notes.space_server(space_id).is_none(),
        "the space still points at a server that refused to store it"
    );

    // ...and the user is told why, rather than left with a space that quietly
    // stopped syncing.
    let error = app
        .state
        .sync
        .as_ref()
        .and_then(|sync| sync.last_error())
        .unwrap_or_default()
        .to_string();
    assert!(
        error.contains("account"),
        "no user-facing explanation for the refusal (last_error: {error:?})"
    );

    // The notes themselves are untouched: this is a sync failure, not data loss.
    let note_ids = app.state.notes.note_ids_in_space(space_id);
    assert!(!note_ids.is_empty(), "the refused space lost its notes");

    // ...and they must read as local, not as forever-pending. Unbinding only
    // the space leaves each note pointing at a doc the relay never stored,
    // which shows as a pending-change dot for a push that can never happen.
    let sync = app.state.sync.as_ref().expect("engine");
    for id in &note_ids {
        let note = app.state.notes.note(id).expect("note");
        assert_eq!(
            sync.note_indicator(note),
            enkr::sync::app::SyncIndicator::LocalOnly,
            "note {id} still advertises pending sync work after its space was refused"
        );
        assert!(
            note.remote_doc().is_none(),
            "note {id} still points at a doc on a space the relay refused"
        );
    }

    // The contrast that makes the assertions above mean something: the exact
    // same clicks against a relay that does *not* require an account must leave
    // the space bound. Without this, "remote is none" would also pass if the
    // sync click had silently done nothing at all.
    let open = TestServer::start();
    let mut ok_app = App::new();
    ok_app.connect(&open.url(), "ana");
    pump_until(&mut [&mut ok_app], "connected", |apps| apps[0].connected());
    let ok_space = ok_app.state.notes.default_space_id();
    ok_app.frame();
    ok_app.harness.right_click("###enkr_space_switcher");
    ok_app.frame();
    ok_app.click("Sync this space\u{2026} >");
    ok_app.click(&format!("{}  (active)", open.url()));
    pump_until(
        &mut [&mut ok_app],
        "space stays bound on an open relay",
        |apps| apps[0].state.notes.space_remote(ok_space).is_some(),
    );
    assert!(
        ok_app.state.notes.space_server(ok_space).is_some(),
        "a relay that asks for no account still failed to bind the space"
    );
}

/// After connecting, the welcome screen stops asking how to connect and starts
/// answering "so where do my notes go?".
///
/// Connecting is a means, not an end. The old pane left the user sitting on the
/// form they had just submitted, holding a device key, with no route to
/// actually having a space — the two ways to get one (make a new one, or copy
/// one the server already holds) both lived somewhere else entirely.
#[test]
fn the_welcome_screen_offers_spaces_once_connected() {
    let server = TestServer::start();
    let mut app = App::fresh_install();

    // Through the welcome screen itself, the way a first-run user arrives here.
    app.frame(); // build the tree before aiming at anything in it
    app.click("###enkr_welcome_tab_online");
    app.type_into_field("###enkr_welcome_server", &server.url());
    app.click("###enkr_welcome_connect");
    pump_until(&mut [&mut app], "connected", |apps| apps[0].connected());
    // The first connect mints the device key and offers the recovery phrase;
    // acknowledge it so it is not sitting over the screen under test.
    if app.frame().try_node("###enkr_recovery_ack").is_some() {
        app.click("###enkr_recovery_ack");
    }

    let snap = app.frame();
    let all: String = snap
        .nodes
        .iter()
        .filter_map(|n| n.text.clone())
        .collect::<Vec<_>>()
        .join(" | ");

    // The step replaced the connect form: its heading and a route to a space.
    assert!(
        all.contains("You're connected"),
        "the connected step should replace the welcome question:\n{all}"
    );
    assert!(
        snap.try_node("###enkr_welcome_create_space").is_some(),
        "no way to create a space from the connected step:\n{all}"
    );
    // The picker, whose question has now been answered, is gone...
    assert!(
        snap.try_node("###enkr_welcome_tab_offline").is_none(),
        "the offline/online picker should not survive into the connected step"
    );
    // ...but not at the cost of stranding someone who picked the wrong server.
    assert!(
        snap.try_node("###enkr_welcome_back").is_some(),
        "the connected step must offer a way back to another server"
    );

    // Creating a space from here binds it to the server just connected to —
    // otherwise the button promises sync and silently delivers a local space.
    let before: Vec<i64> = app.state.notes.spaces().iter().map(|s| s.id).collect();
    app.click("###enkr_welcome_create_space");
    pump_until(&mut [&mut app], "the new space syncs", |apps| {
        apps[0]
            .state
            .notes
            .spaces()
            .iter()
            .any(|s| !before.contains(&s.id) && s.remote.is_some())
    });
    let new_space = app
        .state
        .notes
        .spaces()
        .iter()
        .find(|s| !before.contains(&s.id))
        .expect("a space was created")
        .clone();
    assert_eq!(
        new_space.server.as_deref(),
        Some(server.url().as_str()),
        "the new space was not bound to the connected server"
    );
}

/// A refused account token must not strand the welcome screen on "Connecting…".
///
/// The engine deliberately stops retrying a refused credential — asking again
/// with the same token cannot start working. But `connect_sync` bailed out
/// whenever an engine existed, so that dead engine made Connect a no-op while
/// the screen still read "Connecting…": no error, no progress, and no way to
/// try a different server or token. The way out has to be reachable from the
/// screen the user is stuck on.
#[test]
fn a_refused_token_leaves_the_welcome_screen_usable() {
    let server = TestServer::start();
    let mut app = App::fresh_install();
    app.frame();
    app.click("###enkr_welcome_tab_online");
    app.type_into_field("###enkr_welcome_server", &server.url());
    // No such account exists on this relay, so the handshake is refused.
    app.type_into_field("###enkr_welcome_token", "not-a-real-token");
    app.click("###enkr_welcome_connect");

    pump_until(&mut [&mut app], "the relay refuses the token", |apps| {
        apps[0].state.sync_is_dead()
    });

    // The screen must say so, not pretend to still be working.
    let snap = app.frame();
    let all: String = snap
        .nodes
        .iter()
        .filter_map(|n| n.text.clone())
        .collect::<Vec<_>>()
        .join(" | ");
    assert!(
        !all.contains("Connecting"),
        "a refused connection still reads as in progress:\n{all}"
    );
    assert!(
        !app.connected(),
        "the client should not consider itself connected"
    );

    // And the way out has to work: point it at a different server and retry.
    // Deliberately *not* by changing the token — clearing the token happens to
    // drop the engine on its own, which would hide the bug this covers. Only
    // switching servers leaves the dead engine in place for `connect_sync` to
    // deal with, which is the reported case: "no chance to try another one".
    let other = TestServer::start();
    // Cleared directly: `type_into_field` appends, and the harness has no
    // select-all gesture, so typing alone would dial the two URLs concatenated.
    app.state.token_input.clear();
    app.state.add_server_input.clear();
    app.type_into_field("###enkr_welcome_server", &other.url());
    app.click("###enkr_welcome_connect");
    pump_until(&mut [&mut app], "a different server connects", |apps| {
        apps[0].connected()
    });
}

/// A server that cannot be reached at all — a proxy answering 502, a relay that
/// is down, a host that does not resolve — must not strand the welcome screen
/// either.
///
/// This is *not* the refused-token case: the engine is still retrying, and
/// rightly so, because an outage does end. But it is not progress, and the
/// guard in `connect_sync` that refuses to duplicate a live engine swallowed
/// the button press, so the screen showed "Connecting…" with no way to point
/// the app at a server that does work.
#[test]
fn an_unreachable_server_leaves_the_welcome_screen_usable() {
    let mut app = App::fresh_install();
    app.frame();
    app.click("###enkr_welcome_tab_online");
    // Nothing is listening here, which is the same shape as a 502 to the
    // client: the WebSocket upgrade never completes.
    app.type_into_field("###enkr_welcome_server", "ws://127.0.0.1:1/ws");
    app.click("###enkr_welcome_connect");

    pump_until(&mut [&mut app], "the attempt fails", |apps| {
        apps[0]
            .state
            .sync
            .as_ref()
            .is_some_and(|sync| sync.connect_failed())
    });

    let snap = app.frame();
    let all: String = snap
        .nodes
        .iter()
        .filter_map(|n| n.text.clone())
        .collect::<Vec<_>>()
        .join(" | ");
    assert!(
        !all.contains("Connecting"),
        "an unreachable server still reads as a connection in progress:\n{all}"
    );

    // The way out: a server that is actually there.
    let server = TestServer::start();
    app.state.add_server_input.clear();
    app.type_into_field("###enkr_welcome_server", &server.url());
    app.click("###enkr_welcome_connect");
    pump_until(&mut [&mut app], "a reachable server connects", |apps| {
        apps[0].connected()
    });
}

/// The refusal arriving *after* the notes were optimistically mapped.
///
/// `create_doc`, like `create_space`, returns as soon as it has sent — so on a
/// real link every note in the space is given a remote doc id and a
/// `needs_push` flag a full round trip before the relay's "AccountRequired"
/// comes back. Un-binding only the space then leaves each note pointing at a
/// doc on a space the relay never stored, which `note_indicator` reports as
/// "Synchronizing": a pending-change dot for a push that can never happen.
///
/// Localhost hides this entirely — the refusal beats the pushes — so this test
/// puts a real round trip in front of the relay.
#[test]
fn notes_stop_advertising_sync_when_their_space_is_refused() {
    let server = TestServer::start_with(ServerConfig {
        require_account: true,
        ..ServerConfig::default()
    });
    // 40 ms each way: comfortably longer than the client takes to fire off the
    // whole CreateSpace/CreateDoc/Subscribe burst.
    let proxy = server
        .rt
        .block_on(async { net::NetProxy::start(server.addr, Duration::from_millis(40)).await });

    let mut app = App::new();
    app.connect(&proxy.url(), "ana");
    pump_until(&mut [&mut app], "connected", |apps| apps[0].connected());

    let space_id = app.state.notes.default_space_id();
    app.frame();
    app.harness.right_click("###enkr_space_switcher");
    app.frame();
    app.click("Sync this space\u{2026} >");
    app.click(&format!("{}  (active)", proxy.url()));

    // The optimistic mapping really does land first — without this the test
    // could pass by never having mapped anything, which is what localhost does.
    pump_until(&mut [&mut app], "notes are optimistically mapped", |apps| {
        let state = &apps[0].state;
        state.notes.note_ids_in_space(space_id).iter().any(|id| {
            state
                .notes
                .note(id)
                .is_some_and(|n| n.remote_doc().is_some())
        })
    });

    // Then the refusal lands and must clean them up again.
    pump_until(&mut [&mut app], "the refusal un-maps the notes", |apps| {
        let state = &apps[0].state;
        state.notes.space_remote(space_id).is_none()
            && state.notes.note_ids_in_space(space_id).iter().all(|id| {
                state
                    .notes
                    .note(id)
                    .is_some_and(|n| n.remote_doc().is_none())
            })
    });

    let sync = app.state.sync.as_ref().expect("engine");
    for id in app.state.notes.note_ids_in_space(space_id) {
        let note = app.state.notes.note(&id).expect("note");
        assert_eq!(
            sync.note_indicator(note),
            enkr::sync::app::SyncIndicator::LocalOnly,
            "note {id} still shows pending sync work after its space was refused"
        );
        assert!(
            !note.needs_push(),
            "note {id} is still flagged for a push that has nowhere to go"
        );
    }
}

/// The last edit of a burst must settle to "synced" on its own, without any
/// further input.
///
/// The app only draws when something asks it to. `reconcile` — which clears
/// `needs_push` and turns the indicator green — runs in `sync::pump`, *before*
/// `autosave_due` in the same frame, and it refuses to clear the flag while the
/// note is still dirty. So the frame that finally flushes the note is the frame
/// after which nobody asks for another: the engine's idle event has already
/// been consumed, so no wake is coming either. The note then sits
/// "Synchronizing" until unrelated input (moving the mouse) happens to draw.
///
/// This test therefore drives frames the way the real loop does — only when one
/// has been requested — because a test that just draws in a row cannot see work
/// deferred to a frame that never comes.
#[test]
fn the_last_edit_settles_to_synced_without_further_input() {
    let server = TestServer::start();
    // A's own copy: the helper returns *B*'s note id, and B is not needed here.
    let (mut a, b, _b_note) = establish_shared_pair(&server);
    drop(b);
    let note_id = "Welcome".to_string();

    // Settle everything the setup left in flight.
    pump_until(&mut [&mut a], "initial sync settles", |apps| {
        apps[0]
            .state
            .notes
            .note(&note_id)
            .is_some_and(|note| !note.needs_push())
    });

    // One edit, then hands off the keyboard — no mouse, no further input.
    a.type_into_editor("x");

    // Drive frames only while something has asked for one, exactly as
    // `IMUI::eventloop` does. Background sync events set the flag through the
    // repaint waker, so this also covers "a wake arrives while we are idle".
    let deadline = Instant::now() + Duration::from_secs(20);
    // Whether the note was still unsaved on the previous look, so the moment it
    // goes green can be compared against the autosave rather than after it.
    let mut still_dirty = true;
    loop {
        if a.harness.ui_mut().take_repaint_request() {
            a.frame();
            continue;
        }
        // Nothing wants a frame: the app is idle and what is on screen is
        // final. That is the moment the indicator has to already be green.
        let note = a.state.notes.note(&note_id);
        let settled = note.is_some_and(|note| !note.needs_push());
        if settled {
            // It went green *before* the note was persisted. The clear used to
            // be gated on `!is_dirty()`, using autosave as a proxy for "no
            // newer edit" — which made the dot wait out AUTOSAVE_DELAY (750 ms)
            // after the last keystroke, long after the relay had acked. 750 ms
            // is a wide enough margin for this not to be a timing race.
            assert!(
                still_dirty,
                "the note only went green after being autosaved - the indicator \
                 is waiting on persistence rather than on the relay's ack"
            );
            break;
        }
        still_dirty = note.is_some_and(|note| note.is_dirty());
        assert!(
            Instant::now() < deadline,
            "the app went idle with the note still flagged as unsynced - it will \
             stay yellow until unrelated input draws another frame"
        );
        std::thread::sleep(Duration::from_millis(5));
    }

    let sync = a.state.sync.as_ref().expect("engine");
    let note = a.state.notes.note(&note_id).expect("note");
    assert_eq!(
        sync.note_indicator(note),
        enkr::sync::app::SyncIndicator::Synchronized,
        "an idle app should show the note as synced"
    );
}

// --- Collaboration with a real browser client ------------------------------
//
// Every scenario above pairs two *native* apps. The web build is a different
// backend end to end — a DOM reconciler instead of a GPU paint pass, hosted
// `<textarea>`s instead of a synthetic caret, `web_sys::WebSocket` instead of
// tokio-tungstenite — and none of that had ever been exercised against a real
// relay. What follows drives client B in a headless Chromium while A stays a
// native app, so the two halves of the protocol meet across the two backends.

/// A shared space edited from a native app and a real browser at once: both
/// sides converge, and the browser logs no JavaScript error while doing it.
///
/// The error half is the point as much as the convergence half. A wasm panic
/// in a browser is *silent* from the outside — the frame dies, the page stops
/// updating, and every subsequent assertion just reads as "the click did
/// nothing", pointing at the wrong thing entirely. `console_errors` surfaces
/// the panic itself (`console_error_panic_hook` turns it into a real message),
/// which is the difference between a diagnosis and a mystery.
#[cfg(feature = "cdp")]
#[test]
fn a_browser_client_collaborates_with_a_native_one() {
    let server = TestServer::start();
    let mut a = App::new();
    let (mut b, b_key) = browser_client(&server);

    // -- A pushes a space, then invites B by its device key -------------------
    a.state
        .notes
        .rename_space(a.state.notes.default_space_id(), "Shared");
    a.connect(&server.url(), "ana");
    pump_until(&mut [&mut a], "A connected", |apps| apps[0].connected());

    a.frame();
    a.harness.right_click("###enkr_space_switcher");
    a.frame();
    a.click("Sync this space\u{2026} >");
    a.click(&format!("{}  (active)", server.url()));
    pump_until(&mut [&mut a], "space pushed and note mapped", |apps| {
        let state = &apps[0].state;
        state
            .notes
            .space_remote(state.notes.default_space_id())
            .is_some()
            && state
                .notes
                .note("Welcome")
                .is_some_and(|note| note.remote_doc().is_some())
    });

    a.harness.right_click("###enkr_space_switcher");
    a.frame();
    a.click("Share\u{2026}");
    a.type_into_line_edit(0, &b_key);
    a.click("Write");
    a.click("Invite");
    pump_until(&mut [&mut a], "invite processed", |apps| {
        apps[0]
            .state
            .sync
            .as_ref()
            .is_some_and(|s| s.last_error().is_none())
    });

    // -- B pulls the shared space through its own (browser) UI ---------------
    b.click(SETTINGS_ICON);
    b.click("Sync & Devices");
    let deadline = Instant::now() + PHASE_TIMEOUT;
    loop {
        b.click("Refresh");
        pump_both(&mut a, &mut b, 6);
        if b.exists("Sync") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the browser client never saw the shared space in its remote list"
        );
    }
    b.click("Sync");
    b.key_press(mae::os::OSKeyCode::KeyEscape);

    // The pulled space is not the active one; open it, and its copy of the
    // note both sides are about to edit.
    pump_both(&mut a, &mut b, 10);
    b.click("###enkr_space_switcher");
    b.click("Shared");
    b.click("Welcome");

    // -- A edits; the browser shows it ---------------------------------------
    a.click_sidebar_note("Welcome");
    a.type_into_editor("from-ana");
    await_in_browser(&mut a, &mut b, "from-ana");

    // -- The browser edits; A sees it ----------------------------------------
    let editor = browser_editor_key(&mut b);
    b.click(&editor);
    b.type_text("from-bob");
    pump_until(&mut [&mut a], "A receives the browser's edit", |apps| {
        apps[0]
            .note_text_containing("from-bob")
            .is_some_and(|text| text.contains("from-ana"))
    });

    // -- Both edit the same note, turn by turn, while updates are in flight --
    //
    // The interesting half for the browser: every remote update lands in a
    // hosted `<textarea>` whose value is rewritten under a live caret, and
    // redraws the collaborator-caret overlay over it.
    for round in 0..4 {
        let marker = format!("a{round}");
        a.type_into_editor(&marker);
        let editor = browser_editor_key(&mut b);
        b.click(&editor);
        b.type_text(&format!("b{round}"));
        await_in_browser(&mut a, &mut b, &marker);
        pump_until(&mut [&mut a], "A receives the browser's edit", |apps| {
            apps[0].note_text_containing(&format!("b{round}")).is_some()
        });
    }

    // A collaborator's caret is shown *in* the browser's editor, not just as
    // a badge in the sidebar — see `paint_remote_carets`.
    let deadline = Instant::now() + PHASE_TIMEOUT;
    loop {
        pump_both(&mut a, &mut b, 4);
        if b.debug_eval("!!document.querySelector('.mae-remote-carets')")
            .as_bool()
            .unwrap_or(false)
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the browser never showed the other client's caret in the editor"
        );
    }

    assert_eq!(
        b.console_errors(),
        Vec::<String>::new(),
        "collaborating should not throw anything in the browser"
    );
}

/// Launch a headless-browser Enkr client already connected to `server`, and
/// read its device key out of its own Sync settings — the same string A has to
/// be given to invite it, and the only way in without a second machine.
#[cfg(feature = "cdp")]
fn browser_client(server: &TestServer) -> (mae::testkit::cdp::CdpDriver, String) {
    let mut b = enkr::testkit_support::launch_test_harness_with_query(&format!(
        "?server={}&nick=bob",
        server.url()
    ));
    b.click(SETTINGS_ICON);
    b.click("Sync & Devices");
    // The key is 64 bytes as hex. Matched by shape rather than by a widget id
    // because it is rendered as a plain selectable label, whose `data-mae-id`
    // *is* its text.
    let key = b.debug_eval(
        "(() => { \
           const hex = /^[0-9a-fA-F]{128}$/; \
           for (const el of document.querySelectorAll('[data-mae-id]')) { \
             const v = el.getAttribute('data-mae-id'); \
             if (hex.test(v)) return v; \
           } \
           return null; \
         })()",
    );
    let key = key
        .as_str()
        .expect("the browser client's device key should be on its Sync settings page")
        .to_string();
    b.key_press(mae::os::OSKeyCode::KeyEscape);
    (b, key)
}

/// Advance both clients for `rounds` interleaved turns. A's frames are pumped
/// by hand (it is a `UiHarness`); the browser drives itself off its own render
/// loop, so all `settle` does there is wait for it to go idle — which is also
/// what gives the network time to move between them.
#[cfg(feature = "cdp")]
fn pump_both(a: &mut App, b: &mut mae::testkit::cdp::CdpDriver, rounds: usize) {
    for _ in 0..rounds {
        a.frame();
        b.settle();
        std::thread::sleep(FRAME_PACE);
    }
}

/// Pump until `marker` shows up in the browser's rendered text.
///
/// A *substring* search over the page, not `UiDriver::exists` (which matches a
/// whole element's text exactly): a marker typed into the middle of a note
/// body is one fragment of the editor's whole value.
#[cfg(feature = "cdp")]
fn await_in_browser(a: &mut App, b: &mut mae::testkit::cdp::CdpDriver, marker: &str) {
    let deadline = Instant::now() + PHASE_TIMEOUT;
    loop {
        pump_both(a, b, 4);
        if browser_shows(b, marker) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "the browser client never showed {marker:?}"
        );
    }
}

/// The `data-mae-key` of the browser's note editor, whose id carries the note
/// id (`###enkr_editor_<id>`) and so cannot be written down in advance.
#[cfg(feature = "cdp")]
fn browser_editor_key(b: &mut mae::testkit::cdp::CdpDriver) -> String {
    b.debug_eval(
        "(() => { \
           const el = document.querySelector('[data-mae-key^=\"###enkr_editor_\"]'); \
           return el ? el.getAttribute('data-mae-key') : null; \
         })()",
    )
    .as_str()
    .expect("the browser client should have a note editor on screen")
    .to_string()
}

/// Is `marker` anywhere in the browser page's text — including inside a
/// hosted `<textarea>`'s value, which `innerText` alone does not cover?
#[cfg(feature = "cdp")]
fn browser_shows(b: &mut mae::testkit::cdp::CdpDriver, marker: &str) -> bool {
    let lit = serde_json::to_string(marker).expect("marker is a JSON string");
    b.debug_eval(&format!(
        "(() => {{ \
           const needle = {lit}; \
           if (document.body.innerText.includes(needle)) return true; \
           for (const el of document.querySelectorAll('textarea, input')) {{ \
             if ((el.value || '').includes(needle)) return true; \
           }} \
           return false; \
         }})()"
    ))
    .as_bool()
    .unwrap_or(false)
}
