//! Shared sync test harness (PLAN.md §9).
//!
//! Boots an in-process `enkr-syncd` backed by a temp-file SQLite store and
//! spins up N `enkr` sync clients in the same process over real WebSockets.
//!
//! The engine is doc-less (single-replica design): [`TestClient`] plays the
//! UI side — it owns the local Yrs docs, forwards locally-originated update
//! bytes to the engine, applies decrypted [`SyncEvent::DocBytes`], and
//! answers [`SyncEvent::SnapshotNeeded`] — exactly what `AppSync` + `Note`
//! do in the real app.
//!
//! Shared by `tests/sync.rs` (protocol scenarios) and `tests/scale.rs`
//! (`#[ignore]`d scale budgets).

// Each test binary compiles this module separately and uses a different
// slice of it, so unused-item warnings here are structural, not real.
#![allow(dead_code)]

pub mod hostile;
pub mod metered;
pub mod net;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use enkr::sync::{
    BlobKey, IdentityStore, MemberEntry, MemberRole, SyncClient, SyncConfig, SyncError, SyncEvent,
};
use enkr_syncd::storage::{SqliteStore, Store};
use enkr_syncd::{ServerConfig, ServerHandle, serve};
use uuid::Uuid;
use yrs::updates::decoder::Decode;
use yrs::{GetString, ReadTxn, StateVector, Text, Transact, Update};

/// Budget for a change to travel between clients through a real server.
///
/// Generous on purpose: `cargo test` runs test *binaries* in parallel, so this
/// suite can be competing with the GUI suite and with the CDP tests' headless
/// browser and wasm rebuild. The per-file serialization guard only orders
/// scenarios *within* a binary; it cannot slow the machine down less.
pub const CONVERGE_TIMEOUT: Duration = Duration::from_secs(30);

/// Origin tag for remote applies so the local observer doesn't echo them.
const TEST_REMOTE_ORIGIN: &str = "test-remote";

/// One sync client plus its UI-replica stand-in.
pub struct TestClient {
    client: Arc<SyncClient>,
    docs: Arc<Mutex<HashMap<Uuid, yrs::Doc>>>,
    /// Observers live outside the shared map: `yrs::Subscription` is !Send,
    /// and the pump task only needs the docs.
    observers: Mutex<HashMap<Uuid, yrs::Subscription>>,
    pump: tokio::task::JoinHandle<()>,
}

impl TestClient {
    pub fn spawn(config: SyncConfig) -> Self {
        let client = Arc::new(SyncClient::spawn(config).expect("spawn sync client"));
        let docs: Arc<Mutex<HashMap<Uuid, yrs::Doc>>> = Arc::default();

        // The pump applies decrypted remote updates to the local replicas and
        // answers snapshot requests with their full state (AppSync's job in
        // the real app).
        let pump_docs = docs.clone();
        let pump_client = client.clone();
        let mut events = client.events();
        let pump = tokio::spawn(async move {
            loop {
                match events.recv().await {
                    Ok(SyncEvent::DocBytes { doc_id, update, .. }) => {
                        let docs = pump_docs.lock().unwrap();
                        if let Some(doc) = docs.get(&doc_id)
                            && let Ok(update) = Update::decode_v1(&update)
                        {
                            let mut txn = doc.transact_mut_with(TEST_REMOTE_ORIGIN);
                            let _ = txn.apply_update(update);
                        }
                    }
                    Ok(SyncEvent::SnapshotNeeded { doc_id, covers_seq }) => {
                        let state = {
                            let docs = pump_docs.lock().unwrap();
                            docs.get(&doc_id).map(|doc| {
                                doc.transact()
                                    .encode_state_as_update_v1(&StateVector::default())
                            })
                        };
                        if let Some(state) = state {
                            let _ = pump_client.provide_snapshot(doc_id, covers_seq, state);
                        }
                    }
                    Ok(_) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        let _ = pump_client.resync();
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                }
            }
        });

        Self {
            client,
            docs,
            observers: Mutex::new(HashMap::new()),
            pump,
        }
    }

    /// Create the local replica for a doc and wire its update forwarder.
    fn register_local(&self, doc_id: Uuid) {
        let mut docs = self.docs.lock().unwrap();
        if docs.contains_key(&doc_id) {
            return;
        }
        let doc = yrs::Doc::new();
        let client = self.client.clone();
        let observer = doc
            .observe_update_v1(move |txn, event| {
                let is_remote = txn
                    .origin()
                    .is_some_and(|origin| origin == &yrs::Origin::from(TEST_REMOTE_ORIGIN));
                if !is_remote {
                    let _ = client.queue_update(doc_id, event.update.clone());
                }
            })
            .expect("doc observer");
        docs.insert(doc_id, doc);
        self.observers.lock().unwrap().insert(doc_id, observer);
    }

    pub fn device_pk(&self) -> enkr::sync::DevicePk {
        self.client.device_pk()
    }

    pub fn kex_pk(&self) -> enkr::sync::KexPk {
        self.client.kex_pk()
    }

    pub fn events(&self) -> tokio::sync::broadcast::Receiver<SyncEvent> {
        self.client.events()
    }

    pub async fn status(&self) -> Result<enkr::sync::SyncStatus, SyncError> {
        self.client.status().await
    }

    pub async fn flush(&self) -> Result<(), SyncError> {
        self.client.flush().await
    }

    pub fn resync(&self) -> Result<(), SyncError> {
        self.client.resync()
    }

    pub async fn create_space(&self) -> Result<Uuid, SyncError> {
        self.client.create_space().await
    }

    pub async fn join_space(&self, space: Uuid) -> Result<(), SyncError> {
        self.client.join_space(space).await
    }

    /// Upload a blob under a freshly minted content key, and hand that key
    /// back. In the real app the key rides in the space index doc; here it
    /// travels through the test, which keeps the out-of-band hop visible.
    pub async fn put_blob(
        &self,
        space: Uuid,
        blob: Uuid,
        bytes: Vec<u8>,
    ) -> Result<BlobKey, SyncError> {
        let key = BlobKey::generate();
        self.client
            .put_blob(space, blob, key.clone(), bytes)
            .await
            .map(|()| key)
    }

    pub async fn get_blob(
        &self,
        space: Uuid,
        blob: Uuid,
        key: BlobKey,
    ) -> Result<Option<Vec<u8>>, SyncError> {
        self.client.get_blob(space, blob, key).await
    }

    pub fn delete_blob(&self, space: Uuid, blob: Uuid) -> Result<(), SyncError> {
        self.client.delete_blob(space, blob)
    }

    pub fn delete_space(&self, space: Uuid) -> Result<(), SyncError> {
        self.client.delete_space(space)
    }

    pub async fn add_member(
        &self,
        space: Uuid,
        device_pk: enkr::sync::DevicePk,
        kex_pk: enkr::sync::KexPk,
        role: MemberRole,
    ) -> Result<(), SyncError> {
        self.client.add_member(space, device_pk, kex_pk, role).await
    }

    pub async fn remove_member(
        &self,
        space: Uuid,
        device_pk: enkr::sync::DevicePk,
    ) -> Result<(), SyncError> {
        self.client.remove_member(space, device_pk).await
    }

    pub async fn set_member_role(
        &self,
        space: Uuid,
        device_pk: enkr::sync::DevicePk,
        role: MemberRole,
    ) -> Result<(), SyncError> {
        self.client.set_member_role(space, device_pk, role).await
    }

    pub async fn list_members(&self, space: Uuid) -> Result<Vec<MemberEntry>, SyncError> {
        self.client.list_members(space).await
    }

    pub async fn create_doc(&self, space: Uuid) -> Result<Uuid, SyncError> {
        let doc = self.client.create_doc(space).await?;
        self.register_local(doc);
        Ok(doc)
    }

    pub async fn open_doc(&self, space: Uuid, doc: Uuid) -> Result<(), SyncError> {
        self.client.open_doc(space, doc).await?;
        self.register_local(doc);
        Ok(())
    }

    /// Batched `open_doc` — the shape `AppSync` uses when joining a space.
    pub async fn open_docs(&self, space: Uuid, docs: &[Uuid]) -> Result<(), SyncError> {
        self.client.open_docs(space, docs.to_vec()).await?;
        for doc in docs {
            self.register_local(*doc);
        }
        Ok(())
    }

    pub fn send_ephemeral(&self, doc: Uuid, payload: Vec<u8>) -> Result<(), SyncError> {
        self.client.send_ephemeral(doc, payload)
    }

    /// Insert into the local replica's "body" text; the observer forwards it.
    pub async fn insert_text(
        &self,
        doc: Uuid,
        index: u32,
        text: impl Into<String>,
    ) -> Result<(), SyncError> {
        let text = text.into();
        let docs = self.docs.lock().unwrap();
        let local = docs.get(&doc).ok_or(SyncError::UnknownDoc)?;
        let body = local.get_or_insert_text("body");
        let mut txn = local.transact_mut();
        let len = body.len(&txn);
        body.insert(&mut txn, index.min(len), &text);
        Ok(())
    }

    pub async fn delete_text(&self, doc: Uuid, index: u32, len: u32) -> Result<(), SyncError> {
        let docs = self.docs.lock().unwrap();
        let local = docs.get(&doc).ok_or(SyncError::UnknownDoc)?;
        let body = local.get_or_insert_text("body");
        let mut txn = local.transact_mut();
        body.remove_range(&mut txn, index, len);
        Ok(())
    }

    /// Any doc this client holds a replica of.
    pub fn any_doc(&self) -> Option<Uuid> {
        self.docs.lock().unwrap().keys().next().copied()
    }

    pub async fn doc_text(&self, doc: Uuid) -> Result<String, SyncError> {
        let docs = self.docs.lock().unwrap();
        let local = docs.get(&doc).ok_or(SyncError::UnknownDoc)?;
        let body = local.get_or_insert_text("body");
        Ok(body.get_string(&local.transact()))
    }

    /// Stop the engine and wait for it to finish - flush, then the WebSocket
    /// closing handshake, which is what makes the relay log a clean goodbye
    /// instead of `Connection reset without closing handshake`.
    ///
    /// On a blocking pool thread because the relay under test shares this
    /// test's runtime: blocking the runtime here is exactly what would stop
    /// the server answering the Close we are waiting for.
    pub async fn shutdown(self) {
        let client = self.client.clone();
        // Stops the pump task; the engine is stopped by the join below.
        drop(self);
        let _ = tokio::task::spawn_blocking(move || client.shutdown_blocking()).await;
    }
}

impl Drop for TestClient {
    fn drop(&mut self) {
        self.pump.abort();
    }
}

/// Serializes the scenarios in this file — same reasoning as
/// `tests/app_sync.rs`: real servers, real sockets and wall-clock assertions
/// (including negative ones after a fixed sleep) starve each other when run
/// twenty-three at a time, and fail at random. Reentrant per thread, which is
/// per test, because a scenario may start more than one server.
///
/// `#[tokio::test]` defaults to the current-thread flavour, so the guard stays
/// on the test's own thread across every await.
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

thread_local! {
    static SERIAL_HELD: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

pub struct SerialGuard(Option<std::sync::MutexGuard<'static, ()>>);

impl Drop for SerialGuard {
    fn drop(&mut self) {
        if self.0.is_some() {
            SERIAL_HELD.with(|held| held.set(false));
        }
    }
}

pub fn serialize() -> SerialGuard {
    if SERIAL_HELD.with(|held| held.get()) {
        return SerialGuard(None);
    }
    // Poison ignored: one panicking test must not cascade into every later one.
    let guard = SERIAL.lock().unwrap_or_else(|err| err.into_inner());
    SERIAL_HELD.with(|held| held.set(true));
    SerialGuard(Some(guard))
}

pub struct TestServer {
    handle: Option<ServerHandle>,
    pub addr: std::net::SocketAddr,
    pub db_path: PathBuf,
    config: ServerConfig,
    /// Present only for `start_metered` servers.
    metrics: Option<Arc<metered::StoreMetrics>>,
    /// Declared last so it releases only after the server has shut down.
    _serial: SerialGuard,
}

impl TestServer {
    /// Test-friendly limits: several scale scenarios deliberately hammer the
    /// relay (thousands of flushes, a thousand membership ops), which is exactly
    /// what the production caps exist to stop. Raised here so those tests
    /// measure what they are about; `rate_limit_cuts_off_a_flooding_client`
    /// covers the caps themselves with a strict config.
    fn permissive(mut config: ServerConfig) -> ServerConfig {
        config.messages_per_second = 1_000_000.0;
        config.message_burst = 1_000_000.0;
        config.max_connections_per_device = 1024;
        config
    }

    /// A relay with the caller's limits applied verbatim — for the tests that
    /// are *about* the limits.
    pub async fn start_strict(config: ServerConfig) -> Self {
        Self::start_inner(config).await
    }

    pub async fn start(config: ServerConfig) -> Self {
        Self::start_inner(Self::permissive(config)).await
    }

    async fn start_inner(config: ServerConfig) -> Self {
        // Before any I/O, so a queued test holds no sockets or files.
        let serial = serialize();
        let db_path =
            std::env::temp_dir().join(format!("enkr_syncd_test_{}.sqlite3", Uuid::new_v4()));
        let store = SqliteStore::open(&db_path)
            .await
            .expect("open server store");
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let handle = serve(Arc::new(store), listener, config.clone()).await;
        Self {
            addr: handle.addr,
            handle: Some(handle),
            db_path,
            config,
            metrics: None,
            _serial: serial,
        }
    }

    pub async fn start_default() -> Self {
        Self::start(ServerConfig::default()).await
    }

    /// A relay that refuses any device not presenting a valid account token —
    /// the hosted deployment's configuration.
    pub async fn start_requiring_accounts() -> Self {
        let mut config = ServerConfig::default();
        config.require_account = true;
        Self::start(config).await
    }

    /// Mint an account the way `enkr-syncd account create` does, returning its
    /// id and the plaintext token to hand a client. Opens its own connection to
    /// the same file: the relay is a dumb store, so tests inspect and seed it
    /// directly.
    pub async fn create_account(
        &self,
        label: &str,
        quota_bytes: i64,
        expires_at: Option<i64>,
    ) -> (Uuid, String) {
        let token = Uuid::new_v4().to_string();
        let hash = enkr_proto::crypto::content_hash(token.as_bytes());
        let store = SqliteStore::open(&self.db_path)
            .await
            .expect("open server store");
        let account = store
            .create_account(enkr_syncd::storage::NewAccount {
                label,
                token_hash: &hash,
                quota_bytes,
                expires_at,
                created_at: now_ms(),
            })
            .await
            .expect("create account");
        (account.account_id, token)
    }

    /// `(used_bytes, quota_bytes, expires_at)` straight from the row.
    pub fn account_row(&self, account_id: Uuid) -> (i64, i64, Option<i64>) {
        self.raw_db()
            .query_row(
                "SELECT used_bytes, quota_bytes, expires_at FROM accounts WHERE account_id = ?1",
                [account_id.as_bytes().as_slice()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .expect("account row")
    }

    pub fn used_bytes(&self, account_id: Uuid) -> i64 {
        self.account_row(account_id).0
    }

    /// Revoke as the CLI does, so a test can watch a live token stop working.
    pub async fn delete_account(&self, account_id: Uuid) {
        let store = SqliteStore::open(&self.db_path)
            .await
            .expect("open server store");
        store.delete_account(&account_id).await.expect("delete");
    }

    pub async fn set_account_expiry(&self, account_id: Uuid, expires_at: Option<i64>) {
        let store = SqliteStore::open(&self.db_path)
            .await
            .expect("open server store");
        store
            .set_account_expiry(&account_id, expires_at)
            .await
            .expect("set expiry");
    }

    /// What `used_bytes` *should* be, recomputed from the rows themselves.
    /// The running total is maintained by hand at five call sites; this is the
    /// independent answer to check it against.
    pub async fn recompute_usage(&self) -> Vec<(Uuid, i64, i64)> {
        let store = SqliteStore::open(&self.db_path)
            .await
            .expect("open server store");
        store.recompute_usage().await.expect("recompute")
    }

    /// How many spaces the relay is actually holding. The client keeps a space
    /// locally whether or not the relay accepted it, so this is the only way to
    /// tell a refused `CreateSpace` from an accepted one.
    pub fn space_count(&self) -> i64 {
        self.raw_db()
            .query_row("SELECT COUNT(*) FROM spaces", [], |row| row.get(0))
            .expect("count spaces")
    }

    /// A client presenting `token` — the harness equivalent of pasting one into
    /// Settings → Sync.
    pub fn client_with_token(&self, token: &str) -> TestClient {
        self.client_with_token_as(token, IdentityStore::InMemory)
    }

    /// Same, but with a persistent identity, so the client can be dropped and
    /// recreated as the *same device* — which is what a reconnect actually is.
    /// `InMemory` mints a new device key each time, and a new device is not a
    /// member of anything.
    pub fn client_with_token_at(&self, token: &str, key_path: PathBuf) -> TestClient {
        self.client_with_token_as(token, IdentityStore::Path(key_path))
    }

    fn client_with_token_as(&self, token: &str, identity: IdentityStore) -> TestClient {
        let mut config = SyncConfig::new(self.url(), identity);
        config.debounce = Duration::from_millis(20);
        config.heartbeat = Duration::from_millis(300);
        config.liveness_timeout = Duration::from_secs(2);
        config.reconnect_max = Duration::from_secs(1);
        config.account_token = Some(token.to_string());
        TestClient::spawn(config)
    }

    /// Same as [`TestServer::start`], but every store call the scale budgets
    /// care about is tallied — see [`metered::StoreMetrics`].
    pub async fn start_metered(config: ServerConfig) -> Self {
        let config = Self::permissive(config);
        let serial = serialize();
        let db_path =
            std::env::temp_dir().join(format!("enkr_syncd_test_{}.sqlite3", Uuid::new_v4()));
        let inner = SqliteStore::open(&db_path)
            .await
            .expect("open server store");
        let metrics = Arc::new(metered::StoreMetrics::default());
        let store = metered::MeteredStore {
            inner,
            metrics: metrics.clone(),
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let handle = serve(Arc::new(store), listener, config.clone()).await;
        Self {
            addr: handle.addr,
            handle: Some(handle),
            db_path,
            config,
            metrics: Some(metrics),
            _serial: serial,
        }
    }

    /// A server whose store can be told to fail, and to lie — see
    /// [`hostile::Hostility`].
    pub async fn start_hostile(config: ServerConfig) -> (Self, Arc<hostile::Hostility>) {
        let config = Self::permissive(config);
        let serial = serialize();
        let db_path =
            std::env::temp_dir().join(format!("enkr_syncd_test_{}.sqlite3", Uuid::new_v4()));
        let inner = SqliteStore::open(&db_path)
            .await
            .expect("open server store");
        let hostility = Arc::new(hostile::Hostility::default());
        let store = hostile::HostileStore {
            inner,
            hostility: hostility.clone(),
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let handle = serve(Arc::new(store), listener, config.clone()).await;
        (
            Self {
                addr: handle.addr,
                handle: Some(handle),
                db_path,
                config,
                metrics: None,
                _serial: serial,
            },
            hostility,
        )
    }

    /// Store-call tallies. Panics on a server that wasn't started metered.
    pub fn metrics(&self) -> &Arc<metered::StoreMetrics> {
        self.metrics
            .as_ref()
            .expect("server was not started with start_metered")
    }

    pub fn url(&self) -> String {
        format!("ws://{}/ws", self.addr)
    }

    pub async fn stop(&mut self) {
        if let Some(handle) = self.handle.take() {
            handle.shutdown().await;
        }
    }

    /// Same store, same address: simulates a server crash/restart.
    pub async fn restart(&mut self) {
        self.stop().await;
        let store = SqliteStore::open(&self.db_path)
            .await
            .expect("reopen server store");
        let listener = loop {
            // The OS may briefly hold the port in TIME_WAIT.
            match tokio::net::TcpListener::bind(self.addr).await {
                Ok(listener) => break listener,
                Err(_) => tokio::time::sleep(Duration::from_millis(50)).await,
            }
        };
        // A metered server stays metered across a restart, against the same
        // counters, so a test can span a crash.
        let store: Arc<dyn enkr_syncd::storage::Store> = match &self.metrics {
            Some(metrics) => Arc::new(metered::MeteredStore {
                inner: store,
                metrics: metrics.clone(),
            }),
            None => Arc::new(store),
        };
        self.handle = Some(serve(store, listener, self.config.clone()).await);
    }

    /// Spin up a sync client with test-friendly (fast) timings.
    pub fn client(&self) -> TestClient {
        self.client_at(self.url())
    }

    /// A client whose identity persists in `key_path`, so it can be dropped and
    /// recreated as the *same device* — which is what a real install is.
    /// `IdentityStore::InMemory` mints a new device key per client, so a
    /// recreated one would have lost every membership it had.
    pub fn client_with_identity(&self, key_path: PathBuf) -> TestClient {
        let mut config = SyncConfig::new(self.url(), IdentityStore::Path(key_path));
        config.debounce = Duration::from_millis(20);
        config.heartbeat = Duration::from_millis(300);
        config.liveness_timeout = Duration::from_secs(2);
        config.reconnect_max = Duration::from_secs(1);
        TestClient::spawn(config)
    }

    /// Same, pointed at an arbitrary URL — e.g. a
    /// [`net::NetProxy`] sitting in front of this server.
    pub fn client_at(&self, url: String) -> TestClient {
        let mut config = SyncConfig::new(url, IdentityStore::InMemory);
        config.debounce = Duration::from_millis(20);
        // Liveness on a test clock: production waits 15 s/45 s, which no test
        // wants to sit through. Kept well above the debounce and the RTT the
        // latency proxy injects so a busy link is never mistaken for a dead one.
        config.heartbeat = Duration::from_millis(300);
        config.liveness_timeout = Duration::from_secs(2);
        // Production caps backoff at 30 s; no test wants to wait that out.
        config.reconnect_max = Duration::from_secs(1);
        TestClient::spawn(config)
    }

    /// A client with **production** timings (notably the 120 ms edit debounce)
    /// rather than the fast test ones. Use it when the number being measured is
    /// user-visible latency, where the debounce is a real term in the total.
    pub fn realistic_client_at(&self, url: String) -> TestClient {
        TestClient::spawn(SyncConfig::new(url, IdentityStore::InMemory))
    }

    /// Production timings but with the edit debounce overridden — for measuring
    /// what that knob actually costs and buys.
    pub fn client_with_debounce(&self, url: String, debounce: Duration) -> TestClient {
        let mut config = SyncConfig::new(url, IdentityStore::InMemory);
        config.debounce = debounce;
        TestClient::spawn(config)
    }

    /// Connections the relay currently holds open.
    pub fn live_connections(&self) -> u64 {
        self.handle
            .as_ref()
            .map(ServerHandle::live_connections)
            .unwrap_or(0)
    }

    /// Open the server's SQLite file directly (server is "dumb" — its store
    /// is plain data we can inspect from tests).
    pub fn raw_db(&self) -> rusqlite::Connection {
        rusqlite::Connection::open(&self.db_path).expect("open server db read-only")
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.db_path);
        let _ = std::fs::remove_file(self.db_path.with_extension("sqlite3-wal"));
        let _ = std::fs::remove_file(self.db_path.with_extension("sqlite3-shm"));
    }
}

pub async fn wait_connected(client: &TestClient) {
    let deadline = tokio::time::Instant::now() + CONVERGE_TIMEOUT;
    loop {
        if client.status().await.expect("status").connected {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "client never connected"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Invite `invitee` into `space` and have it join — retrying the join until
/// the server has processed the membership op (different connections race).
pub async fn invite_and_join(inviter: &TestClient, invitee: &TestClient, space: Uuid) {
    invite_and_join_as(inviter, invitee, space, MemberRole::Writer).await;
}

pub async fn invite_and_join_as(
    inviter: &TestClient,
    invitee: &TestClient,
    space: Uuid,
    role: MemberRole,
) {
    inviter
        .add_member(space, invitee.device_pk(), invitee.kex_pk(), role)
        .await
        .expect("add member");
    let deadline = tokio::time::Instant::now() + CONVERGE_TIMEOUT;
    loop {
        match invitee.join_space(space).await {
            Ok(()) => return,
            Err(_) if tokio::time::Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(30)).await;
            }
            Err(err) => panic!("join_space never succeeded: {err}"),
        }
    }
}

/// Wait until every client reports identical doc text *and* fully drained
/// pipelines (debounce empty, outbox acked). Returns the converged text.
pub async fn converge(clients: &[&TestClient], doc: Uuid) -> String {
    let deadline = tokio::time::Instant::now() + CONVERGE_TIMEOUT;
    loop {
        let mut texts = Vec::with_capacity(clients.len());
        let mut idle = true;
        for client in clients {
            let status = client.status().await.expect("status");
            idle &= status.connected && status.outbox_len == 0 && status.pending_docs == 0;
            texts.push(client.doc_text(doc).await.expect("doc text"));
        }
        if idle && texts.windows(2).all(|w| w[0] == w[1]) {
            return texts.pop().unwrap();
        }
        // Truncated: a doc under test can be megabytes, and dumping every
        // replica's full text buries the failure it is meant to explain.
        let summary: Vec<String> = texts
            .iter()
            .map(|text| {
                let head: String = text.chars().take(80).collect();
                format!("{}b {head:?}", text.len())
            })
            .collect();
        assert!(
            tokio::time::Instant::now() < deadline,
            "clients failed to converge; texts: {summary:?}"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// Two-client space + doc, the standard fixture.
pub async fn space_with_two_clients(server: &TestServer) -> (TestClient, TestClient, Uuid, Uuid) {
    let a = server.client();
    let b = server.client();
    wait_connected(&a).await;
    wait_connected(&b).await;
    let space = a.create_space().await.expect("create space");
    let doc = a.create_doc(space).await.expect("create doc");
    invite_and_join(&a, &b, space).await;
    b.open_doc(space, doc).await.expect("open doc");
    (a, b, space, doc)
}

/// Wall clock in milliseconds, matching what the relay stamps rows with.
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
