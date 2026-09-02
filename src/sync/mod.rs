//! E2EE sync engine (PLAN.md §6), single-replica design.
//!
//! All sync/network/crypto work runs on a dedicated thread with its own
//! single-threaded tokio runtime. The engine is **doc-less**: it never holds
//! a Yrs document — it encrypts, ships, sequences and decrypts raw Yrs update
//! bytes. The UI-side documents (notes, index replicas) are the only content
//! replicas; durability is the note database's job (`needs_push` flag).
//!
//! The rest of the app talks to the engine through [`SyncClient`]: commands
//! go over a channel, results come back via oneshots, and unsolicited
//! happenings (decrypted remote updates, epoch bumps, snapshot requests)
//! arrive as [`SyncEvent`]s. Nothing here may be called from the GUI frame
//! hot path except `try_recv` on the event receiver and non-blocking sends.

pub mod app;
mod clock;
mod engine;
pub mod identity;
mod thread;
mod transport;

use std::time::Duration;

use tokio::sync::{broadcast, mpsc, oneshot};
use uuid::Uuid;

use enkr_proto::crypto;
pub use enkr_proto::crypto::{BlobKey, index_doc_id};
pub use enkr_proto::membership::MemberRole;
pub use enkr_proto::wire::{DevicePk, ErrorCode, KexPk};
pub use identity::{IdentityStore, recovery_phrase, restore_from_phrase};

#[derive(Clone, Debug)]
pub struct SyncConfig {
    /// e.g. `ws://127.0.0.1:9070/ws`
    pub server_url: String,
    /// Where the device identity lives (the only persistent sync state).
    pub identity: IdentityStore,
    /// The #1 performance knob: how long queued update bytes pool before
    /// encrypt+sign+push of one merged frame.
    pub debounce: Duration,
    /// Flush early once this much pending update data accumulates.
    pub debounce_max_bytes: usize,
    /// Client-side compaction trigger: tail length since the last snapshot.
    pub snapshot_threshold: u64,
    pub reconnect_min: Duration,
    /// Ceiling on the retry backoff. Retries are scheduled at a random point in
    /// `[0, backoff]` (full jitter), so this is also the window an outage's
    /// reconnect herd is spread across — and, at the same time, the worst case a
    /// user waits before their client comes back. Raise it to protect the relay
    /// from a large fleet; lower it for snappier recovery.
    pub reconnect_max: Duration,
    /// Bearer token for the paying account on this relay, if the user has one.
    /// Empty is normal — a device invited into someone else's space needs none.
    pub account_token: Option<String>,
    /// Send a `Ping` after this much silence from the relay.
    pub heartbeat: Duration,
    /// Give up on the connection after this much silence and reconnect. Must
    /// comfortably exceed `heartbeat`, or a slow reply looks like a dead link.
    pub liveness_timeout: Duration,
}

impl SyncConfig {
    pub fn new(server_url: impl Into<String>, identity: IdentityStore) -> Self {
        Self {
            server_url: server_url.into(),
            identity,
            debounce: Duration::from_millis(120),
            debounce_max_bytes: 8 * 1024,
            snapshot_threshold: 500,
            reconnect_min: Duration::from_millis(100),
            reconnect_max: Duration::from_secs(10),
            account_token: None,
            heartbeat: Duration::from_secs(15),
            liveness_timeout: Duration::from_secs(45),
        }
    }
}

#[derive(Clone, Debug)]
pub enum SyncEvent {
    Connected,
    Disconnected,
    /// A connection attempt (TCP/TLS/handshake) failed; carries a
    /// human-readable reason. The engine keeps retrying with backoff.
    ConnectError {
        context: String,
    },
    /// The relay speaks a different wire version, so the two cannot understand
    /// each other at all. Unlike [`SyncEvent::ConnectError`] this is **not**
    /// retried: nothing changes until one side is upgraded, and silently
    /// retrying leaves the UI saying "Connecting…" forever. Reconnecting
    /// explicitly starts a fresh engine and clears it.
    Incompatible {
        server_version: u16,
        client_version: u16,
    },
    /// The relay refused this device's credentials at the handshake — a wrong,
    /// revoked, or (on a relay that demands one) missing account token. Like
    /// [`SyncEvent::Incompatible`] this is **not** retried: the same credential
    /// will be refused every time. Entering a different token reconnects.
    ///
    /// Distinct from a mid-session `ServerError { AccountRequired }`, which
    /// refuses one *operation* on a connection that is otherwise fine.
    Rejected {
        context: String,
    },
    /// The relay refused to create this space because the connection has no
    /// account. Carries the *space* so the app can un-mark it: a refused
    /// `CreateSpace` is fire-and-forget client-side, so without this the space
    /// keeps a remote id the relay never stored and looks synced forever.
    SpaceRejected {
        space_id: Uuid,
        context: String,
    },
    /// The account this connection authenticated as, as of the handshake.
    /// Point-in-time, not a live meter — see `wire::AccountInfo`.
    Account {
        info: Option<enkr_proto::wire::AccountInfo>,
    },
    /// A decrypted, signature-verified remote update for this doc. Apply it
    /// to the UI replica (idempotent, order-independent).
    ///
    /// `caret_author` is set only for a *live* broadcast (an edit happening
    /// now): the receiver moves that device's remote caret to the edit point
    /// the instant it applies, so the caret never lags the text. It is `None`
    /// for backlog catch-up, snapshots and deferred retries, where there is no
    /// live cursor to attribute.
    DocBytes {
        doc_id: Uuid,
        update: Vec<u8>,
        caret_author: Option<DevicePk>,
    },
    /// Subscription caught up; the doc is live.
    DocSynced {
        doc_id: Uuid,
        head_seq: u64,
    },
    /// The engine needs the doc's full state to produce a snapshot
    /// (server-requested or threshold-crossed). Answer with
    /// [`SyncClient::provide_snapshot`].
    SnapshotNeeded {
        doc_id: Uuid,
        covers_seq: u64,
    },
    /// Everything queued for this doc has been merged, pushed and
    /// acknowledged by the server.
    DocIdle {
        doc_id: Uuid,
    },
    /// The doc has queued or unacknowledged local updates.
    DocBusy {
        doc_id: Uuid,
    },
    EpochBumped {
        space_id: Uuid,
        epoch: u32,
    },
    /// The space was destroyed on the server (by its owner). Drop the local
    /// mirror: its content is gone for everyone.
    SpaceDeleted {
        space_id: Uuid,
    },
    /// A decrypted, signature-verified awareness payload from another member.
    Ephemeral {
        doc_id: Uuid,
        author_device: DevicePk,
        payload: Vec<u8>,
    },
    /// Bad signature / failed decrypt / unknown author — dropped frame.
    SecurityWarning {
        context: String,
    },
    ServerError {
        code: ErrorCode,
        context: String,
    },
    /// A local doc update sealed to a frame too large for the transport to
    /// carry (e.g. a multi-megabyte paste). The engine refuses to ship it -
    /// sending it would trip the server's frame guard and, via the outbox
    /// resend, wedge every reconnect - so the doc's oversized content stays
    /// local until the user trims it. Surfaced so that failure isn't silent.
    UpdateTooLarge {
        doc_id: Uuid,
        bytes: usize,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncError {
    Disconnected,
    UnknownDoc,
    UnknownSpace,
    /// The blob's sealed size exceeds `wire::MAX_BLOB_BYTES`. Permanent -
    /// retrying is futile, so callers must stop reshipping rather than loop.
    BlobTooLarge,
    /// The account paying for this space is out of storage, or its
    /// subscription lapsed. Permanent until something is deleted or the plan is
    /// upgraded — so, like `BlobTooLarge`, callers must stop reshipping.
    QuotaExceeded,
    EngineGone,
    Other(String),
}

impl std::fmt::Display for SyncError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SyncError::Disconnected => f.write_str("not connected to sync server"),
            SyncError::UnknownDoc => f.write_str("unknown doc"),
            SyncError::UnknownSpace => f.write_str("unknown space"),
            SyncError::BlobTooLarge => f.write_str("image is too large to sync"),
            SyncError::QuotaExceeded => {
                f.write_str("storage full - delete some notes or images, or upgrade your plan")
            }
            SyncError::EngineGone => f.write_str("sync engine stopped"),
            SyncError::Other(msg) => f.write_str(msg),
        }
    }
}

impl std::error::Error for SyncError {}

/// One active member of a space, derived from the locally-replayed (and
/// signature-verified) membership log. Used to populate the share dialog.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MemberEntry {
    pub device_pk: DevicePk,
    pub kex_pk: KexPk,
    pub role: MemberRole,
}

/// Point-in-time engine status, mostly for tests/diagnostics.
#[derive(Clone, Debug)]
pub struct SyncStatus {
    pub connected: bool,
    /// Set when the relay rejected us over a wire-version mismatch; no further
    /// connection attempts will be made.
    pub incompatible: Option<(u16, u16)>,
    /// The relay refused this device's credentials (bad/revoked/missing account
    /// token). Terminal like `incompatible`; an explicit reconnect clears it.
    pub rejected: bool,
    /// Frames awaiting server Ack.
    pub outbox_len: usize,
    /// Docs with queued update bytes in the debounce buffer.
    pub pending_docs: usize,
}

pub(crate) enum Cmd {
    CreateSpace {
        reply: oneshot::Sender<Result<Uuid, SyncError>>,
    },
    JoinSpace {
        space_id: Uuid,
        reply: oneshot::Sender<Result<(), SyncError>>,
    },
    CreateDoc {
        space_id: Uuid,
        doc_id: Uuid,
        reply: oneshot::Sender<Result<Uuid, SyncError>>,
    },
    OpenDoc {
        space_id: Uuid,
        doc_id: Uuid,
        reply: oneshot::Sender<Result<(), SyncError>>,
    },
    /// Subscribe to many docs in one go — see [`SyncClient::open_docs`].
    OpenDocs {
        space_id: Uuid,
        doc_ids: Vec<Uuid>,
        reply: oneshot::Sender<Result<(), SyncError>>,
    },
    AddMember {
        space_id: Uuid,
        device_pk: DevicePk,
        kex_pk: KexPk,
        role: MemberRole,
        reply: oneshot::Sender<Result<(), SyncError>>,
    },
    RemoveMember {
        space_id: Uuid,
        device_pk: DevicePk,
        reply: oneshot::Sender<Result<(), SyncError>>,
    },
    /// Change an existing member's role (re-issues a signed `Add` op).
    SetMemberRole {
        space_id: Uuid,
        device_pk: DevicePk,
        role: MemberRole,
        reply: oneshot::Sender<Result<(), SyncError>>,
    },
    /// Active members of a space, read from the local membership replica (no
    /// round-trip).
    ListMembers {
        space_id: Uuid,
        reply: oneshot::Sender<Result<Vec<MemberEntry>, SyncError>>,
    },
    /// Local Yrs update bytes from the UI replica, headed for the debounce →
    /// encrypt → push pipeline. Unknown docs are dropped (recovered by the
    /// `needs_push` reship once the doc opens).
    QueueUpdate {
        doc_id: Uuid,
        update: Vec<u8>,
    },
    /// Answer to [`SyncEvent::SnapshotNeeded`]: the doc's full state.
    ProvideSnapshot {
        doc_id: Uuid,
        covers_seq: u64,
        state: Vec<u8>,
    },
    SendEphemeral {
        doc_id: Uuid,
        payload: Vec<u8>,
    },
    /// Seal `plaintext` under the space key and upload it as blob `blob_id`.
    PutBlob {
        space_id: Uuid,
        blob_id: Uuid,
        blob_key: crypto::BlobKey,
        plaintext: Vec<u8>,
        reply: oneshot::Sender<Result<(), SyncError>>,
    },
    /// Fetch + decrypt blob `blob_id` under its content key (from the index
    /// doc). `Ok(None)` = the server doesn't have it yet; the caller may retry.
    GetBlob {
        space_id: Uuid,
        blob_id: Uuid,
        blob_key: crypto::BlobKey,
        reply: oneshot::Sender<Result<Option<Vec<u8>>, SyncError>>,
    },
    /// Delete blob `blob_id`'s stored content from the relay. Fire-and-forget:
    /// the space index doc already retracted its advertisement, this reclaims
    /// server storage. Best-effort (lost if offline; the blob is then orphaned
    /// but unadvertised).
    DeleteBlob {
        space_id: Uuid,
        blob_id: Uuid,
    },
    /// Re-pull every doc from seq 0 (snapshot + backlog re-delivery). Used
    /// when the event stream lagged: the doc-less engine cannot replay
    /// dropped `DocBytes`, but the server can.
    Resync,
    /// Drop all local engine state for a space (keys, membership, doc
    /// subscriptions) without leaving it server-side. Used when the local
    /// mirror is deleted: a later `JoinSpace` then re-fetches keys and
    /// re-subscribes every doc from seq 0, re-pulling content from scratch.
    ForgetSpace {
        space_id: Uuid,
    },
    ListSpaces {
        reply: oneshot::Sender<Result<Vec<Uuid>, SyncError>>,
    },
    /// Owner-only: ask the server to destroy a space for everyone. The server
    /// answers with a `SpaceDeleted` broadcast (which we also receive), so this
    /// is fire-and-forget — the teardown happens when that arrives.
    DeleteSpace {
        space_id: Uuid,
    },
    Flush {
        reply: oneshot::Sender<()>,
    },
    Status {
        reply: oneshot::Sender<SyncStatus>,
    },
    Shutdown,
}

/// Install the rustls `ring` crypto provider as the process default (once).
/// Required for wss:// because tokio-tungstenite compiles rustls without a
/// provider feature, so rustls can't auto-select one and would otherwise panic
/// on the first TLS handshake.
///
/// Native only — wasm32 has no `rustls` dependency at all (a `wss://` URL
/// already gets TLS from the browser itself; see `transport/wasm.rs`).
#[cfg(not(target_arch = "wasm32"))]
fn install_crypto_provider() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        // Err means another thread already installed one — fine either way.
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

#[cfg(target_arch = "wasm32")]
fn install_crypto_provider() {}

/// Handle to the sync engine (a dedicated thread on native, cooperatively
/// scheduled on wasm32 — see `thread.rs`). The app owns exactly one.
pub struct SyncClient {
    cmd_tx: mpsc::UnboundedSender<Cmd>,
    events: broadcast::Sender<SyncEvent>,
    device_pk: DevicePk,
    kex_pk: KexPk,
    handle: Option<thread::EngineHandle>,
}

impl SyncClient {
    /// Boot the sync engine. Loading/creating the device identity is quick
    /// enough (a 64-byte file, or nothing at all for `IdentityStore::
    /// InMemory`) to do directly here rather than reporting back
    /// asynchronously — needed on wasm32 regardless, since there's no way to
    /// synchronously wait on a spawned task there the way the old
    /// thread-plus-boot-channel handshake did.
    pub fn spawn(config: SyncConfig) -> Result<Self, SyncError> {
        install_crypto_provider();
        let identity = identity::load_or_create(&config.identity).map_err(SyncError::Other)?;
        let device_pk = identity.device_pk();
        let kex_pk = identity.kex_pk();

        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let (events, _) = broadcast::channel(4096);
        let handle = thread::spawn_engine(config, identity, cmd_rx, events.clone())
            .map_err(SyncError::Other)?;

        Ok(Self {
            cmd_tx,
            events,
            device_pk,
            kex_pk,
            handle: Some(handle),
        })
    }

    pub fn device_pk(&self) -> DevicePk {
        self.device_pk
    }

    pub fn kex_pk(&self) -> KexPk {
        self.kex_pk
    }

    pub fn events(&self) -> broadcast::Receiver<SyncEvent> {
        self.events.subscribe()
    }

    fn send_cmd(&self, cmd: Cmd) -> Result<(), SyncError> {
        self.cmd_tx.send(cmd).map_err(|_| SyncError::EngineGone)
    }

    async fn request<T>(
        &self,
        make: impl FnOnce(oneshot::Sender<Result<T, SyncError>>) -> Cmd,
    ) -> Result<T, SyncError> {
        let (tx, rx) = oneshot::channel();
        self.send_cmd(make(tx))?;
        rx.await.map_err(|_| SyncError::EngineGone)?
    }

    /// Create a space (+ its index doc) on the server; returns the space id.
    pub async fn create_space(&self) -> Result<Uuid, SyncError> {
        self.request(|reply| Cmd::CreateSpace { reply }).await
    }

    /// Join a space this device was invited to: fetches the membership log and
    /// key envelopes, verifies them, and subscribes the index doc.
    pub async fn join_space(&self, space_id: Uuid) -> Result<(), SyncError> {
        self.request(|reply| Cmd::JoinSpace { space_id, reply })
            .await
    }

    /// Create a fresh doc in a joined space and subscribe it.
    pub async fn create_doc(&self, space_id: Uuid) -> Result<Uuid, SyncError> {
        self.request(|reply| Cmd::CreateDoc {
            space_id,
            doc_id: Uuid::new_v4(),
            reply,
        })
        .await
    }

    /// Open (subscribe) an existing doc of a joined space.
    pub async fn open_doc(&self, space_id: Uuid, doc_id: Uuid) -> Result<(), SyncError> {
        self.request(|reply| Cmd::OpenDoc {
            space_id,
            doc_id,
            reply,
        })
        .await
    }

    /// Open (subscribe) many docs of a joined space in batched `Subscribe`
    /// messages. Prefer this to a loop over [`SyncClient::open_doc`]: joining a
    /// space with N notes otherwise costs N frames and N server-side subscribe
    /// passes. Docs already open are skipped.
    pub async fn open_docs(&self, space_id: Uuid, doc_ids: Vec<Uuid>) -> Result<(), SyncError> {
        self.request(|reply| Cmd::OpenDocs {
            space_id,
            doc_ids,
            reply,
        })
        .await
    }

    /// Invite a device (key obtained out-of-band, TOFU): wraps every epoch key
    /// to it and appends a signed Add op.
    pub async fn add_member(
        &self,
        space_id: Uuid,
        device_pk: DevicePk,
        kex_pk: KexPk,
        role: MemberRole,
    ) -> Result<(), SyncError> {
        self.request(|reply| Cmd::AddMember {
            space_id,
            device_pk,
            kex_pk,
            role,
            reply,
        })
        .await
    }

    /// Remove a device: rotates the space key to a new epoch wrapped only to
    /// the remaining members, and appends a signed Remove op.
    pub async fn remove_member(
        &self,
        space_id: Uuid,
        device_pk: DevicePk,
    ) -> Result<(), SyncError> {
        self.request(|reply| Cmd::RemoveMember {
            space_id,
            device_pk,
            reply,
        })
        .await
    }

    /// Change an existing member's role: appends a fresh signed `Add` op with
    /// the new role (the device keeps the keys it already holds).
    pub async fn set_member_role(
        &self,
        space_id: Uuid,
        device_pk: DevicePk,
        role: MemberRole,
    ) -> Result<(), SyncError> {
        self.request(|reply| Cmd::SetMemberRole {
            space_id,
            device_pk,
            role,
            reply,
        })
        .await
    }

    /// List the active members of a space (id + role), read from the locally
    /// verified membership log.
    pub async fn list_members(&self, space_id: Uuid) -> Result<Vec<MemberEntry>, SyncError> {
        self.request(|reply| Cmd::ListMembers { space_id, reply })
            .await
    }

    /// Queue local Yrs update bytes for encrypt+push. Fire-and-forget; safe
    /// from the GUI thread (non-blocking channel send).
    pub fn queue_update(&self, doc_id: Uuid, update: Vec<u8>) -> Result<(), SyncError> {
        self.send_cmd(Cmd::QueueUpdate { doc_id, update })
    }

    /// Answer a [`SyncEvent::SnapshotNeeded`] with the doc's full state.
    /// Fire-and-forget; safe from the GUI thread.
    pub fn provide_snapshot(
        &self,
        doc_id: Uuid,
        covers_seq: u64,
        state: Vec<u8>,
    ) -> Result<(), SyncError> {
        self.send_cmd(Cmd::ProvideSnapshot {
            doc_id,
            covers_seq,
            state,
        })
    }

    /// Awareness payload; the engine encrypts + signs it under the doc key
    /// before relay (the server still never sees plaintext). Fire-and-forget.
    pub fn send_ephemeral(&self, doc_id: Uuid, payload: Vec<u8>) -> Result<(), SyncError> {
        self.send_cmd(Cmd::SendEphemeral { doc_id, payload })
    }

    /// Upload an encrypted image blob to a joined space. The engine seals
    /// `plaintext` under the space's current epoch key. Resolves once the
    /// server confirms durable storage.
    pub async fn put_blob(
        &self,
        space_id: Uuid,
        blob_id: Uuid,
        blob_key: crypto::BlobKey,
        plaintext: Vec<u8>,
    ) -> Result<(), SyncError> {
        self.request(|reply| Cmd::PutBlob {
            space_id,
            blob_id,
            blob_key,
            plaintext,
            reply,
        })
        .await
    }

    /// Fetch + decrypt an image blob. `Ok(None)` means not available yet (the
    /// server lacks it or the epoch key isn't loaded) — safe to retry.
    pub async fn get_blob(
        &self,
        space_id: Uuid,
        blob_id: Uuid,
        blob_key: crypto::BlobKey,
    ) -> Result<Option<Vec<u8>>, SyncError> {
        self.request(|reply| Cmd::GetBlob {
            space_id,
            blob_id,
            blob_key,
            reply,
        })
        .await
    }

    /// Delete an image blob's stored content from the relay. Fire-and-forget:
    /// paired with the index-doc retraction on a local image delete so the
    /// sealed bytes don't linger on the server.
    pub fn delete_blob(&self, space_id: Uuid, blob_id: Uuid) -> Result<(), SyncError> {
        self.send_cmd(Cmd::DeleteBlob { space_id, blob_id })
    }

    /// Re-pull every subscribed doc from scratch (lost-event recovery).
    /// Fire-and-forget.
    pub fn resync(&self) -> Result<(), SyncError> {
        self.send_cmd(Cmd::Resync)
    }

    /// Forget all local state for a space (keys, membership, doc
    /// subscriptions) without leaving it server-side, so a later `join_space`
    /// re-fetches and re-subscribes everything from scratch. Fire-and-forget.
    pub fn forget_space(&self, space_id: Uuid) -> Result<(), SyncError> {
        self.send_cmd(Cmd::ForgetSpace { space_id })
    }

    /// Owner-only: destroy a space server-side for every member. Fire-and-forget;
    /// the teardown runs when the server's `SpaceDeleted` broadcast arrives.
    pub fn delete_space(&self, space_id: Uuid) -> Result<(), SyncError> {
        self.send_cmd(Cmd::DeleteSpace { space_id })
    }

    /// Ask the server which spaces this device is a member of (ids only;
    /// names live encrypted in each space's index doc).
    pub async fn list_spaces(&self) -> Result<Vec<Uuid>, SyncError> {
        self.request(|reply| Cmd::ListSpaces { reply }).await
    }

    /// Force the debounce buffers through encrypt+push now.
    pub async fn flush(&self) -> Result<(), SyncError> {
        let (tx, rx) = oneshot::channel();
        self.send_cmd(Cmd::Flush { reply: tx })?;
        rx.await.map_err(|_| SyncError::EngineGone)
    }

    pub async fn status(&self) -> Result<SyncStatus, SyncError> {
        let (tx, rx) = oneshot::channel();
        self.send_cmd(Cmd::Status { reply: tx })?;
        rx.await.map_err(|_| SyncError::EngineGone)
    }

    /// Ask the engine to stop *now* without consuming the handle. The engine
    /// breaks out of its loop, so it stops reading the socket and emitting
    /// server frames immediately. Used when the handle lives behind a shared
    /// `Arc` (the GUI bridge) and so can't call the consuming [`Self::shutdown`]
    /// — without this, the engine would keep processing inbound frames (rooms
    /// info, remote updates, presence) until the last `Arc` finally drops.
    pub fn request_shutdown(&self) {
        let _ = self.cmd_tx.send(Cmd::Shutdown);
    }

    /// Flush and stop the sync engine. Blocks until it exits on native;
    /// best-effort on wasm32, where nothing can block waiting for it (see
    /// `thread::EngineHandle::join`).
    pub fn shutdown(mut self) {
        let _ = self.cmd_tx.send(Cmd::Shutdown);
        if let Some(handle) = self.handle.take() {
            handle.join();
        }
    }
}

impl Drop for SyncClient {
    fn drop(&mut self) {
        // Best-effort stop; explicit shutdown() joins.
        let _ = self.cmd_tx.send(Cmd::Shutdown);
    }
}
