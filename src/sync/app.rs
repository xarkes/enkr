//! GUI-side sync controller: bridges the immediate-mode UI (synchronous,
//! frame-driven) and the async [`SyncClient`] without ever blocking a frame.
//!
//! Single-replica architecture:
//! - The note's own Yrs doc is the **only** content replica. A per-note
//!   observer forwards locally-originated update bytes to the engine
//!   ([`SyncClient::queue_update`]); decrypted remote updates come back as
//!   [`SyncEvent::DocBytes`] and are applied under a remote origin (no echo).
//! - There is nothing to diverge and nothing to diff: no state vectors, no
//!   acknowledgements bookkeeping. Durability is the note database's job —
//!   the per-note `needs_push` flag reships full state (idempotent) after
//!   crashes/offline restarts; [`SyncEvent::DocIdle`] clears it.
//! - Space **index docs** (title/name metadata) have no note counterpart, so
//!   the bridge owns tiny in-memory replicas for them, wired into the same
//!   byte pipeline.
//! - A small **bridge thread** runs async operations against the engine
//!   (`create_space`, `join_space`, …); completions and engine events arrive
//!   through one polled channel, each waking the event loop via
//!   [`RepaintWaker`] — the UI reacts per event, never by polling at 60fps.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;
// `std::time::Instant` unconditionally panics on wasm32-unknown-unknown;
// `web_time::Instant` is API-identical, real `std::time::Instant` on
// native, `performance.now()`-backed on wasm32 — see Cargo.toml.
use web_time::Instant;

use mae::imui::RepaintWaker;
use uuid::Uuid;
use yrs::updates::decoder::Decode;
use yrs::{Any, Map, Out, ReadTxn, StateVector, StickyIndex, Transact, Update};

use crate::note::{Note, NoteDatabase, REMOTE_ORIGIN};
use enkr_proto::crypto::BlobKey;
use enkr_proto::wire::{AccountInfo, ErrorCode, ImageMime};

use super::{
    DevicePk, KexPk, MemberEntry, MemberRole, SyncClient, SyncConfig, SyncError, SyncEvent,
    index_doc_id,
};

/// How many times a blob upload may fail (as a connection drop rather than a
/// clean error) before the blob is quarantined out of the reship loop. Small
/// enough that a poison blob stops wedging reconnects quickly, but above 1 so a
/// genuine transient network blip still gets retried.
const MAX_BLOB_UPLOAD_ATTEMPTS: u32 = 3;

/// Throttle floor for movement pings — rapid non-edit caret moves (drag-select,
/// key-repeat) coalesce to at most one ping per this interval.
const PRESENCE_MIN_INTERVAL: Duration = Duration::from_millis(80);
/// Heartbeat: re-send the caret at least this often even when it hasn't moved,
/// so an idle collaborator's caret stays alive on peers (well under the TTL).
const PRESENCE_HEARTBEAT: Duration = Duration::from_secs(10);
/// Presence entries disappear when not refreshed for this long.
const PRESENCE_TTL: Duration = Duration::from_secs(30);

/// Yrs map in the index doc: doc-uuid string → note title.
const INDEX_NOTES_MAP: &str = "notes";
/// Yrs map in the index doc: space metadata ("name").
const INDEX_META_MAP: &str = "meta";
/// Yrs map in the index doc: folder-uuid string → folder name.
const INDEX_FOLDERS_MAP: &str = "folders";
/// Yrs map in the index doc: folder-uuid string → parent folder uuid string.
/// Missing entry means top-level folder.
const INDEX_FOLDER_PARENTS_MAP: &str = "folder_parents";
/// Yrs map in the index doc: doc-uuid string → folder-uuid string. A doc
/// without an entry sits at the space root.
const INDEX_NOTE_FOLDERS_MAP: &str = "note_folders";
/// Yrs map in the index doc: blob-uuid string → blob name (the `./blob/<name>`
/// link target). Lets peers discover a space's images.
const INDEX_BLOBS_MAP: &str = "blobs";
/// Yrs map in the index doc: blob-uuid string → folder-uuid string.
const INDEX_BLOB_FOLDERS_MAP: &str = "blob_folders";
/// Yrs map in the index doc: blob-uuid string → `"<mime_u8>:<hex content hash>"`.
/// Carries the format + an integrity check for the separately-fetched content.
const INDEX_BLOB_META_MAP: &str = "blob_meta";

// `Send` on native: `task_rx` is drained on the bridge thread, a different
// thread than whichever queued the task, so the future has to be safely
// movable across that boundary. wasm32 has no threads at all — nothing
// needs `Send` there, which matters because `RepaintWaker` (captured by
// most queued tasks, to wake the UI when they finish) legitimately isn't
// `Send` on wasm32 (see its doc comment in `imui.rs`).
#[cfg(not(target_arch = "wasm32"))]
type Task = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;
#[cfg(target_arch = "wasm32")]
type Task = Pin<Box<dyn Future<Output = ()> + 'static>>;

/// Sync state of a space or note, for the sidebar indicators.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SyncIndicator {
    /// Not mapped to any remote space/doc.
    LocalOnly,
    /// Mapped, but the server is unreachable.
    Offline,
    /// Mapped and connected, with catch-up or unacknowledged edits in flight.
    Synchronizing,
    /// Live and fully acknowledged.
    Synchronized,
    /// The last sync operation failed (see [`AppSync::last_error`]).
    Errored,
}

/// Another member's live presence on a doc. Caret/selection positions are
/// CRDT anchors ([`yrs::StickyIndex`]) created on the *sender's* replica and
/// resolved against the local one — they stay glued to the right logical
/// place even while either side keeps editing between pings.
#[derive(Clone, Debug)]
pub struct Presence {
    pub device: DevicePk,
    pub nickname: String,
    /// The peer's caret position.
    pub caret: Option<yrs::StickyIndex>,
    /// The peer's selection anchor (other end of the selection).
    pub selection_anchor: Option<yrs::StickyIndex>,
    last_seen: Instant,
}

impl Presence {
    /// Stable per-device palette slot for presence colors.
    pub fn color_slot(&self) -> usize {
        self.device.iter().fold(0usize, |acc, b| acc + *b as usize) % 6
    }
}

/// One member of a shared space, as shown in the share dialog.
#[derive(Clone, Debug)]
pub struct MemberView {
    pub device_pk: DevicePk,
    pub role: MemberRole,
    /// True for this device — its row carries no management controls.
    pub is_self: bool,
}

impl MemberView {
    /// Short, human-readable device fingerprint (first 4 bytes, hex).
    pub fn short_id(&self) -> String {
        short_device_id(&self.device_pk)
    }
}

/// Short, human-readable device fingerprint (first 4 bytes, hex). Used as a
/// presence label fallback when a member hasn't set a nickname.
fn short_device_id(device_pk: &DevicePk) -> String {
    device_pk[..4].iter().map(|b| format!("{b:02x}")).collect()
}

/// Lowercase hex of a 32-byte content hash, for the index `blob_meta` value.
fn hex32(bytes: &[u8; 32]) -> String {
    let mut s = String::with_capacity(64);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// Parse a `blob_meta` value `"<mime_u8>:<hex hash>"` into its parts.
/// `mime : content-hash : content-key`. An entry without a key is unusable —
/// its ciphertext can't be opened — so it is rejected rather than adopted.
fn parse_blob_meta(meta: &str) -> Option<(ImageMime, [u8; 32], [u8; 32])> {
    let (mime_str, rest) = meta.split_once(':')?;
    let (hash_str, key_str) = rest.split_once(':')?;
    let mime = match mime_str.parse::<u8>().ok()? {
        2 => ImageMime::Jpeg,
        _ => ImageMime::Png,
    };
    Some((mime, unhex32(hash_str)?, unhex32(key_str)?))
}

fn unhex32(hex: &str) -> Option<[u8; 32]> {
    if hex.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(out)
}

/// One remote space as shown in the sync window.
#[derive(Clone, Debug)]
pub struct RemoteSpace {
    pub space_id: Uuid,
    /// Local space id when already synced locally.
    pub local: Option<i64>,
}

struct DocSync {
    note_id: String,
    space: Uuid,
    live: bool,
    /// Mirror of the engine's per-doc busy/idle edge events. Starts false:
    /// only a real `DocIdle` (everything queued was pushed and acked) may
    /// clear the note's `needs_push` flag.
    engine_idle: bool,
    /// The note's `local_edit_clock` as of the last `DocIdle` — i.e. the latest
    /// local edit confirmed pushed+acked (so peers have it). While the note's
    /// live clock runs ahead of this, our caret points into content peers don't
    /// have yet, so presence pings are held back (the caret rides the update
    /// instead). See [`AppSync::presence_ping`].
    delivered_edit_clock: u64,
    /// Title last written into the space index doc.
    index_title: Option<String>,
}

/// Bridge-owned replica of a space's index doc (no note counterpart).
struct IndexReplica {
    space: Uuid,
    doc: yrs::Doc,
    /// Subscription caught up (`DocSynced` seen). Only a live replica is
    /// authoritative for folder *existence*: before catch-up its maps are
    /// partial, so absence means nothing.
    live: bool,
    /// Forwards locally-originated index writes to the engine.
    _observer: yrs::Subscription,
    /// True once this device may author the space's `name` in the index —
    /// because it pushed the space, or because it has read a name from the
    /// index at least once.
    ///
    /// The index name is one last-writer-wins key with no owner. A device that
    /// pulled a space holds a placeholder name until adoption, and writing that
    /// before the real name arrives renames the space for *everyone*: the
    /// owner's own diff pass is revision-gated, so it never corrects it. Notes
    /// don't have this problem — `needs_push` reships them — but index docs have
    /// no such net.
    may_author_name: bool,
}

enum AppEvent {
    Sync(SyncEvent),
    SpacePushed {
        local_space: i64,
        result: Result<Uuid, SyncError>,
    },
    DocCreated {
        note_id: String,
        space: Uuid,
        result: Result<Uuid, SyncError>,
    },
    SpaceJoined {
        space: Uuid,
        result: Result<(), SyncError>,
    },
    /// Like [`AppEvent::SpaceJoined`] but for a peek: keys and index only, no
    /// local mirror.
    SpacePeeked {
        space: Uuid,
        result: Result<(), SyncError>,
    },
    DocOpened {
        result: Result<(), SyncError>,
    },
    SpacesListed {
        result: Result<Vec<Uuid>, SyncError>,
    },
    MemberAdded {
        space: Uuid,
        result: Result<(), SyncError>,
    },
    MemberRemoved {
        space: Uuid,
        result: Result<(), SyncError>,
    },
    MemberRoleChanged {
        space: Uuid,
        result: Result<(), SyncError>,
    },
    MembersListed {
        space: Uuid,
        result: Result<Vec<MemberEntry>, SyncError>,
    },
    /// A blob upload finished. `Ok` clears the blob's `needs_push` flag; a
    /// permanent error (e.g. [`SyncError::BlobTooLarge`]) parks the blob so the
    /// reship loop stops hammering the server, while a transient error just
    /// leaves `needs_push` set for the next reconnect.
    BlobUploaded {
        blob_id: Uuid,
        result: Result<(), SyncError>,
    },
    /// A blob fetch finished; `bytes` is `None` if it wasn't available.
    BlobFetched {
        blob_id: Uuid,
        bytes: Option<Vec<u8>>,
    },
    /// The engine event stream overflowed and events were dropped — recover
    /// by re-pulling everything from the server.
    EventsLagged,
}

/// Severity of a user-facing [`Notice`], mapped to a toast level by the UI.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NoticeLevel {
    Info,
    Warning,
    Danger,
}

/// A transient message for the user. The sync layer is UI-agnostic, so it queues
/// these and the render layer drains them (see `AppSync::take_notices`) into
/// toasts rather than reaching into the widget toolkit itself.
#[derive(Clone, Debug)]
pub struct Notice {
    pub level: NoticeLevel,
    pub message: String,
}

pub struct AppSync {
    client: std::sync::Arc<SyncClient>,
    task_tx: tokio::sync::mpsc::UnboundedSender<Task>,
    events_tx: std::sync::mpsc::Sender<AppEvent>,
    events_rx: std::sync::mpsc::Receiver<AppEvent>,
    waker: RepaintWaker,
    /// Native only — kept only to own the bridge thread's lifetime (never
    /// joined). wasm32 has nothing to hold here: its bridge tasks are
    /// `spawn_local`'d directly, with no thread/handle of their own.
    #[cfg(not(target_arch = "wasm32"))]
    _bridge: std::thread::JoinHandle<()>,

    connected: bool,
    /// `(server, client)` wire versions when the relay turned us away over a
    /// mismatch. While set, the engine is not attempting to connect.
    incompatible: Option<(u16, u16)>,
    /// The relay refused this device's account token, or requires one we did
    /// not present. Terminal until the user supplies a different token.
    rejected: bool,
    /// At least one connection attempt has failed since the last success.
    ///
    /// Distinct from `rejected`: the engine *is* still retrying (a 502 from a
    /// reverse proxy, a server that is down, DNS that does not resolve), so
    /// this is not terminal. But to the user an attempt that keeps failing is
    /// not progress, and calling it "Connecting…" hides both the problem and
    /// the fact that they are free to try somewhere else.
    connect_failed: bool,
    /// The account this connection authenticated as, as of the last handshake.
    /// `None` means the relay gave us none — either it wants no account, or no
    /// token was presented. Read by Settings → Sync to show plan and usage.
    account: Option<AccountInfo>,
    /// Remote spaces the relay refused for want of an account. Consulted by the
    /// `SpacePushed` handler because the refusal can arrive *before* the
    /// optimistic push result it invalidates — the two travel by different
    /// paths — and binding a refused space would leave it looking synced.
    rejected_spaces: HashSet<Uuid>,
    device_key: String,
    nickname: String,
    /// The sync server URL this engine is bound to; only spaces bound to it
    /// participate in sync.
    active_server: String,
    last_error: Option<String>,
    /// Whether `last_error` describes a *connection* problem rather than a
    /// durable one.
    ///
    /// `last_error` serves two audiences: transient failures ("not connected to
    /// sync server", which every in-flight request reports the moment the
    /// socket drops) and lasting conditions (a quarantined image, a membership
    /// warning) that stay true across reconnects. Only the first kind is retired
    /// by reconnecting — clearing both would erase state the user still needs,
    /// and clearing neither leaves "not connected" on screen underneath a green
    /// "Connected".
    last_error_transient: bool,
    remote_space_ids: Vec<Uuid>,
    /// Names of remote spaces with no local mirror yet.
    ///
    /// A space's name lives in its (encrypted) index doc, so the server cannot
    /// supply it and the listing is bare uuids until something decrypts one.
    /// Peeking fills this in so the user can tell which space is which *before*
    /// deciding to sync it.
    remote_names: HashMap<Uuid, String>,
    /// Spaces a peek has been *attempted* for — joined for their keys and
    /// index, with no local mirror created (that is what "Sync" does).
    ///
    /// Attempted, not in-flight: the remote list asks for a peek on every
    /// frame it is on screen, so anything that clears on completion would
    /// re-issue a join per frame for any space whose name never resolves. One
    /// attempt per space per session is enough — the listing still shows the
    /// id, and syncing works regardless.
    peeked: HashSet<Uuid>,
    /// remote doc id → UI-side sync state (note docs only).
    docs: HashMap<Uuid, DocSync>,
    /// index doc id → bridge-owned replica.
    index_docs: HashMap<Uuid, IndexReplica>,
    /// Local spaces with a push in flight.
    pushing: HashSet<i64>,
    /// Note ids with a CreateDoc in flight (guards the per-pump rescan).
    creating: HashSet<String>,
    /// Remote spaces with a join/fetch in flight.
    joining: HashSet<Uuid>,
    /// `NoteDatabase::spaces_rev` last reconciled into the index docs.
    spaces_rev_seen: u64,
    /// `NoteDatabase::folders_rev` last reconciled into the index docs.
    folders_rev_seen: u64,
    /// `NoteDatabase::blobs_rev` last reconciled into the index docs.
    blobs_rev_seen: u64,
    /// Blob ids with a content upload in flight (dedups the reship).
    pushing_blobs: HashSet<Uuid>,
    /// Blob ids that will never sync: either the relay rejected them with a
    /// graceful `BlobTooLarge`, or an upload repeatedly killed the connection
    /// (a proxy/frame-limit below our ceiling closes the socket instead of
    /// erroring). Kept out of the reship loop so one bad blob can't wedge the
    /// connection; surfaced to the user via `last_error` instead.
    failed_blobs: HashSet<Uuid>,
    /// Consecutive failed upload attempts per blob. A blob whose upload keeps
    /// dropping the connection (rather than returning a clean error) is
    /// quarantined into `failed_blobs` once this crosses
    /// [`MAX_BLOB_UPLOAD_ATTEMPTS`], so the reship loop can't hammer the server
    /// forever. Reset on a successful upload.
    blob_upload_attempts: HashMap<Uuid, u32>,
    /// User-facing messages queued for the UI to raise as toasts.
    notices: Vec<Notice>,
    /// Force a folder diff pass on the next pump (replica turned live,
    /// deletion queued, …) regardless of `folders_rev_seen`.
    index_dirty: bool,
    /// Folder deletions awaiting their index replica: index doc id → folder
    /// ids to remove from the "folders" map once the replica is live.
    pending_folder_removals: HashMap<Uuid, Vec<Uuid>>,
    /// Blob deletions awaiting their index replica: index doc id → blob ids to
    /// remove from the "blobs"/"blob_meta"/"blob_folders" maps once the replica
    /// is live. Without this a locally-deleted image stays advertised in the
    /// index doc and a restart re-adopts + re-downloads it.
    pending_blob_removals: HashMap<Uuid, Vec<Uuid>>,
    presence: HashMap<Uuid, HashMap<DevicePk, Presence>>,
    presence_sent: HashMap<Uuid, (Instant, (Option<usize>, Option<usize>))>,
    /// The doc this device is currently announcing presence on (the focused
    /// note's doc, when synced). Changing it sends a leave on the old doc and
    /// an immediate ping on the new one. `None` = focused on a note that isn't
    /// synced here, so we're present nowhere.
    presence_doc: Option<Uuid>,
    /// Cached active-member lists per space, for the share dialog.
    members: HashMap<Uuid, Vec<MemberView>>,
    /// Spaces with a `list_members` request in flight (dedups refreshes).
    members_refreshing: HashSet<Uuid>,
}

/// Forwards the engine's broadcast events into the UI-side channel, waking
/// the UI after each — shared verbatim between native's bridge thread and
/// wasm32's `spawn_local`'d task (see `AppSync::start`); only *how* this
/// runs differs by platform, not what it does.
async fn forward_engine_events(
    mut engine_events: tokio::sync::broadcast::Receiver<SyncEvent>,
    forward_tx: std::sync::mpsc::Sender<AppEvent>,
    forward_waker: RepaintWaker,
) {
    loop {
        let event = match engine_events.recv().await {
            Ok(event) => AppEvent::Sync(event),
            // Overflow skips ahead — translate the gap into a recovery
            // marker, never stop forwarding (that silently kills sync).
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => AppEvent::EventsLagged,
            Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
        };
        if forward_tx.send(event).is_err() {
            return;
        }
        forward_waker.wake();
    }
}

impl Drop for AppSync {
    /// Disconnecting drops the `AppSync`. Stop the engine *now* so it closes
    /// the connection and stops processing inbound server frames (rooms info,
    /// remote updates, presence) on the spot, instead of lingering until the
    /// bridge thread and the last `Arc<SyncClient>` wind down — otherwise we
    /// keep receiving connected-rooms info after a "Disconnect".
    fn drop(&mut self) {
        self.client.request_shutdown();
    }
}

impl AppSync {
    /// Boot the sync engine + bridge thread. Blocks only for the device
    /// identity load (fast); never call from inside a frame.
    pub fn start(
        config: SyncConfig,
        nickname: String,
        waker: RepaintWaker,
    ) -> Result<Self, SyncError> {
        // The server this engine is bound to. Spaces bound to a *different*
        // server are filtered out of every adopt/reconcile pass so switching
        // the active server can't push a server-A space onto server B.
        let active_server = config.server_url.clone();
        let client = std::sync::Arc::new(SyncClient::spawn(config)?);
        let device_key = encode_device_key(&client.device_pk(), &client.kex_pk());
        let (task_tx, mut task_rx) = tokio::sync::mpsc::unbounded_channel::<Task>();
        let (events_tx, events_rx) = std::sync::mpsc::channel::<AppEvent>();

        let forward_client = client.clone();
        let forward_tx = events_tx.clone();
        let forward_waker = waker.clone();

        // Native: a dedicated thread running its own single-threaded tokio
        // runtime + `LocalSet` (so the spawned tasks below don't need to be
        // `Send` — they capture `Arc<SyncClient>`/`RepaintWaker`, cheap to
        // share but not worth requiring cross-thread-safe for). wasm32: no
        // threads exist at all, so both tasks are `spawn_local`'d directly
        // on the main thread instead — see `sync/thread.rs`'s doc comment
        // for the same `block_on`-panics-on-idle reasoning that rules out
        // trying to keep the tokio runtime/`LocalSet` here too.
        #[cfg(not(target_arch = "wasm32"))]
        let bridge = std::thread::Builder::new()
            .name("enkr-sync-bridge".into())
            .spawn(move || {
                let Ok(rt) = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                else {
                    return;
                };
                let local = tokio::task::LocalSet::new();
                local.block_on(&rt, async move {
                    let engine_events = forward_client.events();
                    tokio::task::spawn_local(forward_engine_events(
                        engine_events,
                        forward_tx,
                        forward_waker,
                    ));
                    while let Some(task) = task_rx.recv().await {
                        tokio::task::spawn_local(task);
                    }
                });
            })
            .map_err(|e| SyncError::Other(e.to_string()))?;

        #[cfg(target_arch = "wasm32")]
        {
            let engine_events = forward_client.events();
            wasm_bindgen_futures::spawn_local(forward_engine_events(
                engine_events,
                forward_tx,
                forward_waker,
            ));
            wasm_bindgen_futures::spawn_local(async move {
                while let Some(task) = task_rx.recv().await {
                    wasm_bindgen_futures::spawn_local(task);
                }
            });
        }

        Ok(Self {
            client,
            task_tx,
            events_tx,
            events_rx,
            waker,
            #[cfg(not(target_arch = "wasm32"))]
            _bridge: bridge,
            connected: false,
            incompatible: None,
            rejected: false,
            connect_failed: false,
            account: None,
            rejected_spaces: HashSet::new(),
            device_key,
            nickname,
            active_server,
            last_error: None,
            last_error_transient: false,
            remote_space_ids: Vec::new(),
            remote_names: HashMap::new(),
            peeked: HashSet::new(),
            docs: HashMap::new(),
            index_docs: HashMap::new(),
            pushing: HashSet::new(),
            creating: HashSet::new(),
            joining: HashSet::new(),
            // MAX forces one initial diff pass once replicas come up.
            spaces_rev_seen: u64::MAX,
            folders_rev_seen: u64::MAX,
            blobs_rev_seen: u64::MAX,
            pushing_blobs: HashSet::new(),
            failed_blobs: HashSet::new(),
            blob_upload_attempts: HashMap::new(),
            notices: Vec::new(),
            index_dirty: false,
            pending_folder_removals: HashMap::new(),
            pending_blob_removals: HashMap::new(),
            presence: HashMap::new(),
            presence_sent: HashMap::new(),
            presence_doc: None,
            members: HashMap::new(),
            members_refreshing: HashSet::new(),
        })
    }

    pub fn connected(&self) -> bool {
        self.connected
    }

    /// `(server, client)` wire versions when the two cannot talk to each other.
    /// Distinct from "not connected yet": no further attempt will be made until
    /// the user reconnects against a compatible relay.
    pub fn incompatible(&self) -> Option<(u16, u16)> {
        self.incompatible
    }

    /// True when the relay refused this device's credentials. The engine has
    /// stopped retrying: the fix is a valid account token, not patience.
    pub fn rejected(&self) -> bool {
        self.rejected
    }

    /// Whether the last connection attempt failed. The engine keeps retrying —
    /// unlike [`AppSync::rejected`] this is not terminal — but the UI should
    /// say so rather than showing indefinite progress.
    pub fn connect_failed(&self) -> bool {
        self.connect_failed && !self.connected
    }

    /// Plan and usage for the account this connection authenticated as, as
    /// reported at the last handshake. A point-in-time reading, not live.
    pub fn account(&self) -> Option<AccountInfo> {
        self.account
    }

    /// hex(`device_pk` ‖ `kex_pk`) — what another user types to invite this
    /// device (TOFU PoC; exchanged out-of-band).
    pub fn device_key(&self) -> &str {
        &self.device_key
    }

    pub fn set_nickname(&mut self, nickname: String) {
        self.nickname = nickname;
    }

    pub fn last_error(&self) -> Option<&str> {
        self.last_error.as_deref()
    }

    /// Remote spaces from the last refresh, flagged with their local mirror.
    pub fn remote_spaces(&self, notes: &NoteDatabase) -> Vec<RemoteSpace> {
        self.remote_space_ids
            .iter()
            .map(|space_id| RemoteSpace {
                space_id: *space_id,
                local: notes.space_by_remote(space_id),
            })
            .collect()
    }

    /// True while bridge operations are in flight (joins, pushes, doc
    /// creations). Edits don't count: their lifecycle is fully event-driven.
    pub fn has_pending(&self) -> bool {
        !self.pushing.is_empty() || !self.joining.is_empty() || !self.creating.is_empty()
    }

    /// Upload an image blob's bytes to the server under `blob_id`, sealed by the
    /// engine. On success the blob's `needs_push` flag is cleared.
    pub fn upload_blob(&self, space: Uuid, blob_id: Uuid, key: [u8; 32], bytes: Vec<u8>) {
        let client = self.client.clone();
        self.submit(async move {
            let result = client
                .put_blob(space, blob_id, BlobKey::from_bytes(key), bytes)
                .await;
            if let Err(err) = &result {
                log::warn!("blob {blob_id} upload failed: {err}");
            }
            AppEvent::BlobUploaded { blob_id, result }
        });
    }

    /// Fetch + decrypt an image blob's bytes from the server by id.
    pub fn request_blob(&self, space: Uuid, blob_id: Uuid, key: [u8; 32]) {
        let client = self.client.clone();
        self.submit(async move {
            let bytes = match client
                .get_blob(space, blob_id, BlobKey::from_bytes(key))
                .await
            {
                Ok(bytes) => bytes,
                Err(err) => {
                    log::warn!("blob {blob_id} fetch failed: {err}");
                    None
                }
            };
            AppEvent::BlobFetched { blob_id, bytes }
        });
    }

    fn submit(&self, fut: impl Future<Output = AppEvent> + Send + 'static) {
        let tx = self.events_tx.clone();
        let waker = self.waker.clone();
        let _ = self.task_tx.send(Box::pin(async move {
            let _ = tx.send(fut.await);
            waker.wake();
        }));
    }

    // -- mappings ---------------------------------------------------------------

    /// Rebuild doc↔note mappings from the note database at startup, and
    /// (re)gain keys + subscriptions. Safe to call repeatedly (runs on every
    /// connect).
    pub fn adopt(&mut self, notes: &NoteDatabase) {
        let remotes: Vec<Uuid> = notes
            .spaces()
            .iter()
            .filter(|space| space.server.as_deref() == Some(self.active_server.as_str()))
            .filter_map(|space| space.remote)
            .collect();
        for remote in remotes {
            self.fetch_space(remote);
        }
    }

    /// Wire a note's doc into the byte pipeline and remember the mapping.
    fn register_doc(&mut self, notes: &mut NoteDatabase, doc: Uuid, space: Uuid, note_id: String) {
        if self.docs.contains_key(&doc) {
            return;
        }
        let mut delivered_edit_clock = 0;
        if let Some(note) = notes.note_mut(&note_id) {
            let client = self.client.clone();
            note.attach_sync_observer(move |update| {
                let _ = client.queue_update(doc, update);
            });
            // Anything already in the note ships as full state on catch-up
            // (resolvable everywhere), so treat the current clock as delivered.
            delivered_edit_clock = note.local_edit_clock();
        }
        self.docs.insert(
            doc,
            DocSync {
                note_id,
                space,
                live: false,
                engine_idle: false,
                delivered_edit_clock,
                index_title: None,
            },
        );
    }

    /// Create (or keep) the bridge-owned replica of a space's index doc.
    fn ensure_index_replica(&mut self, space: Uuid) {
        let index_id = index_doc_id(&space);
        if self.index_docs.contains_key(&index_id) {
            return;
        }
        let doc = yrs::Doc::new();
        let client = self.client.clone();
        let observer = doc
            .observe_update_v1(move |txn, event| {
                let is_remote = txn
                    .origin()
                    .is_some_and(|origin| origin == &yrs::Origin::from(REMOTE_ORIGIN));
                if !is_remote {
                    let _ = client.queue_update(index_id, event.update.clone());
                }
            })
            .expect("index doc observer");
        self.index_docs.insert(
            index_id,
            IndexReplica {
                space,
                doc,
                live: false,
                _observer: observer,
                may_author_name: false,
            },
        );
    }

    /// Batched `open_doc_async`: one `Subscribe` per `SUBSCRIBE_BATCH` docs
    /// instead of one per doc. Used by the join paths, where a space with many
    /// notes would otherwise cost a frame and a server subscribe pass each.
    fn open_docs_async(&mut self, space: Uuid, docs: Vec<Uuid>) {
        if docs.is_empty() {
            return;
        }
        let client = self.client.clone();
        self.submit(async move {
            AppEvent::DocOpened {
                result: client.open_docs(space, docs).await,
            }
        });
    }

    fn open_doc_async(&mut self, space: Uuid, doc: Uuid) {
        let client = self.client.clone();
        self.submit(async move {
            AppEvent::DocOpened {
                result: client.open_doc(space, doc).await,
            }
        });
    }

    // -- operations (UI entry points) --------------------------------------------

    /// Push a local space to the server: create the remote space, then one
    /// doc per note (content + index entries follow through the pipeline).
    pub fn push_space(&mut self, notes: &NoteDatabase, local_space: i64) {
        if notes.space_remote(local_space).is_some() || !self.pushing.insert(local_space) {
            return;
        }
        let client = self.client.clone();
        self.submit(async move {
            AppEvent::SpacePushed {
                local_space,
                result: client.create_space().await,
            }
        });
    }

    /// Ask the server which spaces this device can read.
    pub fn refresh_remote_spaces(&mut self) {
        let client = self.client.clone();
        self.submit(async move {
            AppEvent::SpacesListed {
                result: client.list_spaces().await,
            }
        });
    }

    /// Join + mirror a remote space locally (or catch up an already-mapped
    /// one): fetch keys, subscribe the index doc, then open every note doc.
    pub fn fetch_space(&mut self, space: Uuid) {
        if !self.joining.insert(space) {
            return;
        }
        let client = self.client.clone();
        self.submit(async move {
            AppEvent::SpaceJoined {
                space,
                result: client.join_space(space).await,
            }
        });
    }

    /// Drop every bit of runtime sync state for a remote space whose local
    /// mirror was just deleted: the index replica, all note-doc mappings, and
    /// the engine's keys/subscriptions. Without this, a later `fetch_space`
    /// short-circuits (the engine still has keys + live subscriptions, the
    /// index replica still holds the old note list) and recreates the space
    /// shell but never re-pulls its notes. After forgetting, a re-fetch is a
    /// fresh join: keys are re-fetched and every doc re-subscribed from seq 0.
    pub fn space_deleted(&mut self, remote: Uuid) {
        self.drop_space_runtime(remote);
        let _ = self.client.forget_space(remote);
    }

    /// Owner action: ask the server to destroy the space's content. Each member
    /// (this device included) keeps its local copy but unsyncs it when the
    /// server's `SpaceDeleted` broadcast comes back through the pump, so this
    /// only fires the request.
    pub fn delete_remote_space(&mut self, remote: Uuid) {
        let _ = self.client.delete_space(remote);
    }

    /// Tear down all UI-side runtime state for a remote space: its index
    /// replica, note-doc mappings, presence, and member/join bookkeeping.
    /// Shared by the local-delete path ([`AppSync::space_deleted`]) and the
    /// server-driven `SpaceDeleted` handler.
    fn drop_space_runtime(&mut self, remote: Uuid) {
        let index_id = index_doc_id(&remote);
        let doc_ids: Vec<Uuid> = self
            .docs
            .iter()
            .filter(|(_, state)| state.space == remote)
            .map(|(doc, _)| *doc)
            .collect();
        for doc in &doc_ids {
            self.docs.remove(doc);
            self.presence.remove(doc);
            if self.presence_doc == Some(*doc) {
                self.presence_doc = None;
            }
        }
        self.index_docs.remove(&index_id);
        self.presence.remove(&index_id);
        self.pending_folder_removals.remove(&index_id);
        self.pending_blob_removals.remove(&index_id);
        self.joining.remove(&remote);
        self.members.remove(&remote);
        self.members_refreshing.remove(&remote);
    }

    /// Sever a space's sync link while keeping every local note: detach each
    /// note from its remote doc, unbind the space from its server, and drop the
    /// sync runtime. The space stays in the sidebar as a plain local-only
    /// space. Used when the server reports the space was deleted (the content
    /// is gone remotely, but a member's own copy is theirs to keep).
    fn unsync_space(&mut self, remote: Uuid, notes: &mut NoteDatabase) {
        if let Some(local) = notes.space_by_remote(&remote) {
            for note_id in notes.note_ids_in_space(local) {
                if let Some(note) = notes.note_mut(&note_id) {
                    note.detach_sync_observer();
                    note.set_remote_doc(None);
                    note.set_needs_push(false);
                }
            }
            notes.set_space_remote(local, None);
            notes.set_space_server(local, None);
        }
        self.drop_space_runtime(remote);
    }

    /// Invite another device (key string shown in its sync window).
    pub fn share_space(
        &mut self,
        space: Uuid,
        device_key: &str,
        role: MemberRole,
    ) -> Result<(), String> {
        let (device_pk, kex_pk) = decode_device_key(device_key)?;
        // Membership is keyed on `device_pk` alone, so inviting our own device
        // (even with a tweaked `kex_pk`) would re-add ourselves and could drop
        // our own permissions, leaving us stuck.
        if device_pk == self.client.device_pk() {
            return Err("that's this device's own key — you can't invite yourself".into());
        }
        let client = self.client.clone();
        self.submit(async move {
            AppEvent::MemberAdded {
                space,
                result: client.add_member(space, device_pk, kex_pk, role).await,
            }
        });
        Ok(())
    }

    /// Active members of a space, for the share dialog. Kicks off a background
    /// refresh the first time a space is queried; later calls read the cache.
    pub fn members(&mut self, space: Uuid) -> Vec<MemberView> {
        if !self.members.contains_key(&space) {
            self.refresh_members(space);
        }
        self.members.get(&space).cloned().unwrap_or_default()
    }

    /// True when this device may manage members (add/remove/role) in `space`.
    pub fn can_admin(&self, space: Uuid) -> bool {
        self.members
            .get(&space)
            .is_some_and(|list| list.iter().any(|m| m.is_self && m.role.can_admin()))
    }

    /// True when this device may edit docs in `space` (Owner or Writer).
    /// A space whose member list hasn't been loaded yet is treated as writable
    /// so the editor doesn't flash read-only before the background membership
    /// refresh lands; readers only ever resolve to `false` once their role is
    /// known. The member list is refreshed on join (see `SpaceJoined`).
    pub fn can_write(&self, space: Uuid) -> bool {
        match self.members.get(&space) {
            Some(list) => list
                .iter()
                .find(|m| m.is_self)
                .is_none_or(|m| m.role.can_write()),
            None => true,
        }
    }

    /// Record a condition that stays true until something is done about it —
    /// a quarantined blob, a failed merge, a membership warning. Reconnecting
    /// does not retire these, so the transient flag is cleared alongside.
    fn record_durable_error(&mut self, message: String) {
        self.last_error_transient = false;
        self.last_error = Some(message);
    }

    /// Record a failed operation for display.
    ///
    /// `Disconnected` is flagged transient: it means "the socket was down when
    /// this request ran", which stops being true the moment we reconnect.
    fn record_error(&mut self, err: &SyncError) {
        self.last_error_transient = matches!(err, SyncError::Disconnected);
        self.last_error = Some(err.to_string());
    }

    /// (Re)load the member list for a space. At most one request in flight.
    fn refresh_members(&mut self, space: Uuid) {
        if !self.members_refreshing.insert(space) {
            return;
        }
        let client = self.client.clone();
        self.submit(async move {
            AppEvent::MembersListed {
                space,
                result: client.list_members(space).await,
            }
        });
    }

    /// Revoke a device's access: rotates the space key and drops it from the
    /// member set.
    pub fn uninvite(&mut self, space: Uuid, device_pk: DevicePk) {
        let client = self.client.clone();
        self.submit(async move {
            AppEvent::MemberRemoved {
                space,
                result: client.remove_member(space, device_pk).await,
            }
        });
    }

    /// Change an existing member's permission level.
    pub fn change_member_role(&mut self, space: Uuid, device_pk: DevicePk, role: MemberRole) {
        let client = self.client.clone();
        self.submit(async move {
            AppEvent::MemberRoleChanged {
                space,
                result: client.set_member_role(space, device_pk, role).await,
            }
        });
    }

    /// Propagate a local folder deletion into the space's index doc. Call
    /// alongside [`NoteDatabase::delete_folder`]; no-op for local-only
    /// spaces. The removal queues until the index replica is live, so a
    /// just-joined space can't resurrect the folder mid-catch-up.
    pub fn folder_deleted(&mut self, notes: &NoteDatabase, space_id: i64, folder: Uuid) {
        let Some(remote) = notes.space_remote(space_id) else {
            return;
        };
        self.pending_folder_removals
            .entry(index_doc_id(&remote))
            .or_default()
            .extend(notes.folder_subtree(folder));
        self.index_dirty = true;
    }

    /// Propagate a local image deletion. Two parts: retract the blob from the
    /// space index doc so peers (and this device after a restart) stop resolving
    /// its `./blob/<name>` link and re-downloading it, and tell the relay to
    /// drop the sealed content so it isn't orphaned in server storage.
    pub fn blob_deleted(&mut self, notes: &NoteDatabase, space_id: i64, blob: Uuid) {
        let Some(remote) = notes.space_remote(space_id) else {
            return;
        };
        self.pending_blob_removals
            .entry(index_doc_id(&remote))
            .or_default()
            .push(blob);
        self.index_dirty = true;
        // Best-effort server-side content delete (fire-and-forget); the index
        // retraction above is the durable half that guarantees convergence.
        let _ = self.client.delete_blob(remote, blob);
    }

    /// Create the remote doc for a note that was added to a synced space.
    pub fn push_note(&mut self, notes: &NoteDatabase, note_id: &str) {
        let Some(note) = notes.note(note_id) else {
            return;
        };
        if note.remote_doc().is_some() {
            return;
        }
        let Some(space) = notes.space_remote(note.space_id()) else {
            return;
        };
        if !self.creating.insert(note_id.to_string()) {
            return;
        }
        let client = self.client.clone();
        let note_id = note_id.to_string();
        self.submit(async move {
            AppEvent::DocCreated {
                note_id,
                space,
                result: client.create_doc(space).await,
            }
        });
    }

    /// Note which doc the user is focused on (the active note's synced doc, or
    /// `None` for a local-only / different-server note). On a change it tells
    /// peers we left the previous doc — so our caret clears there immediately
    /// instead of lingering until TTL — and arms an instant ping on the new doc
    /// (the "first heartbeat on focus"). Same-space switches keep the space
    /// indicator (we reappear on the new doc); cross-space switches drop it.
    /// Idempotent per frame: a no-op while the focused doc is unchanged.
    pub fn focus_doc(&mut self, doc: Option<Uuid>) {
        // Only a doc we actually sync can carry our presence.
        let doc = doc.filter(|d| self.docs.contains_key(d));
        if self.presence_doc == doc {
            return;
        }
        if let Some(old) = self.presence_doc.take() {
            if self.connected {
                let _ = self.client.send_ephemeral(old, encode_presence_leave());
            }
            self.presence_sent.remove(&old);
        }
        self.presence_doc = doc;
        // Drop the throttle entry so the next `presence_ping` on the new doc
        // fires on this very frame.
        if let Some(new) = doc {
            self.presence_sent.remove(&new);
        }
    }

    /// Share the local caret + selection as CRDT anchors created on `note`'s
    /// replica (peers resolve them on theirs). Sent on *movement* — throttled
    /// so a drag-select doesn't flood — plus a low-rate heartbeat that keeps an
    /// idle caret alive on peers.
    ///
    /// Held back entirely while there are local edits not yet pushed+acked: the
    /// caret then sits in content peers don't have yet, so an ephemeral anchor
    /// would either dangle (vanish) on their side or race ahead of the
    /// debounced update (caret jumps before the text changes). During that
    /// window the caret rides the update instead (`caret_author`); pings resume
    /// once the doc goes idle.
    pub fn presence_ping(
        &mut self,
        doc: Uuid,
        note: &Note,
        caret: Option<usize>,
        selection_anchor: Option<usize>,
    ) {
        // Presence is broadcast even without a nickname: peers fall back to a
        // short device id, so an unnamed device's caret still shows up.
        if !self.connected {
            return;
        }
        let Some(delivered) = self.docs.get(&doc).map(|d| d.delivered_edit_clock) else {
            return;
        };
        // Undelivered local edits: let the caret ride the update, not a ping.
        if note.local_edit_clock() != delivered {
            return;
        }
        let now = Instant::now();
        let position = (caret, selection_anchor);
        let last = self.presence_sent.get(&doc).copied();
        // A genuine move, far enough from the last send to clear the throttle.
        let moved = last.is_none_or(|(_, prev)| prev != position);
        let due_for_move =
            moved && last.is_none_or(|(at, _)| now.duration_since(at) >= PRESENCE_MIN_INTERVAL);
        let due_for_heartbeat =
            last.is_none_or(|(at, _)| now.duration_since(at) >= PRESENCE_HEARTBEAT);
        if !due_for_move && !due_for_heartbeat {
            return;
        }
        self.presence_sent.insert(doc, (now, position));
        let caret_anchor = caret.and_then(|idx| note.caret_anchor(idx));
        let sel_anchor = selection_anchor.and_then(|idx| note.caret_anchor(idx));
        let _ = self.client.send_ephemeral(
            doc,
            encode_presence(&self.nickname, caret_anchor.as_ref(), sel_anchor.as_ref()),
        );
    }

    /// Everyone present on any doc of a space (deduped by device).
    pub fn space_presence(&mut self, notes: &NoteDatabase, local_space: i64) -> Vec<Presence> {
        let Some(remote) = notes.space_remote(local_space) else {
            return Vec::new();
        };
        let docs: Vec<Uuid> = self
            .docs
            .iter()
            .filter(|(_, d)| d.space == remote)
            .map(|(id, _)| *id)
            .collect();
        let mut by_device: HashMap<DevicePk, Presence> = HashMap::new();
        for doc in docs {
            for presence in self.presence(&doc) {
                by_device.entry(presence.device).or_insert(presence);
            }
        }
        let mut list: Vec<Presence> = by_device.into_values().collect();
        list.sort_by(|a, b| a.nickname.cmp(&b.nickname));
        list
    }

    /// Live presence on a doc (expired entries pruned).
    pub fn presence(&mut self, doc: &Uuid) -> Vec<Presence> {
        let Some(entries) = self.presence.get_mut(doc) else {
            return Vec::new();
        };
        entries.retain(|_, p| p.last_seen.elapsed() < PRESENCE_TTL);
        let mut list: Vec<Presence> = entries.values().cloned().collect();
        list.sort_by(|a, b| a.nickname.cmp(&b.nickname));
        list
    }

    // -- indicators ----------------------------------------------------------------

    pub fn note_indicator(&self, note: &Note) -> SyncIndicator {
        let Some(doc) = note.remote_doc() else {
            return SyncIndicator::LocalOnly;
        };
        if !self.connected {
            return SyncIndicator::Offline;
        }
        match self.docs.get(&doc) {
            // Green only when subscribed and nothing local is unacknowledged.
            // `needs_push` is set synchronously by every local edit and only
            // cleared after a real DocIdle ack, so it alone covers in-flight
            // edits — green must never lie about convergence. Don't require
            // `engine_idle` here: receive-only docs (a joined space that was
            // never edited locally) get no busy→idle edge, so they'd stay
            // "Synchronizing" forever.
            Some(state) if state.live && !note.needs_push() => SyncIndicator::Synchronized,
            Some(_) => SyncIndicator::Synchronizing,
            None => SyncIndicator::Synchronizing,
        }
    }

    pub fn space_indicator(&self, notes: &NoteDatabase, local_space: i64) -> SyncIndicator {
        let Some(remote) = notes.space_remote(local_space) else {
            return if self.pushing.contains(&local_space) {
                SyncIndicator::Synchronizing
            } else {
                SyncIndicator::LocalOnly
            };
        };
        if !self.connected {
            return SyncIndicator::Offline;
        }
        if self.joining.contains(&remote) {
            return SyncIndicator::Synchronizing;
        }
        // Same green rule as `note_indicator`: live + acknowledged, without
        // requiring an `engine_idle` edge that receive-only docs never get.
        let busy = self.docs.values().any(|d| {
            d.space == remote
                && (!d.live || notes.note(&d.note_id).is_some_and(|note| note.needs_push()))
        });
        if busy {
            SyncIndicator::Synchronizing
        } else {
            SyncIndicator::Synchronized
        }
    }

    // -- the per-event pump --------------------------------------------------------

    /// Drain completions/events and run the per-frame reconciliation. Called
    /// once per frame; does nothing (and allocates nothing) when nothing
    /// happened. Returns true when state changed in a way that affects
    /// rendering.
    pub fn pump(&mut self, notes: &mut NoteDatabase) -> bool {
        let mut changed = false;
        while let Ok(event) = self.events_rx.try_recv() {
            self.handle_event(event, notes);
            changed = true;
        }
        changed |= self.reconcile(notes);
        changed
    }

    /// Per-pump reconciliation: adopt brand-new notes in synced spaces, keep
    /// index titles fresh, and clear `needs_push` once the engine confirmed
    /// everything.
    ///
    /// "Confirmed" is `delivered_edit_clock == local_edit_clock`: the engine
    /// went idle having delivered every local edit the note has made. This used
    /// to be approximated by `!note.is_dirty()`, using autosave as a proxy for
    /// "no newer edit" — correct, but it meant the dot only turned green once
    /// the note had been *persisted*, up to `AUTOSAVE_DELAY` (750 ms) after the
    /// last keystroke and long after the relay's ack. The clock answers the
    /// actual question (has the engine delivered everything?) instead of a
    /// slower one that merely implies it.
    ///
    /// The clock is captured when `DocIdle` is *handled*, so an edit made
    /// between the engine emitting idle and this draining it would be recorded
    /// as delivered — but that same edit also emits `DocBusy`, and `pump`
    /// drains the whole queue before calling this, so `engine_idle` is already
    /// false again by the time it is read.
    fn reconcile(&mut self, notes: &mut NoteDatabase) -> bool {
        let mut changed = false;

        // New notes created locally in a synced space get a remote doc.
        let unmapped: Vec<String> = notes
            .spaces()
            .iter()
            .filter(|space| {
                space.remote.is_some()
                    && space.server.as_deref() == Some(self.active_server.as_str())
            })
            .flat_map(|space| notes.note_ids_in_space(space.id))
            .filter(|id| {
                notes
                    .note(id)
                    .is_some_and(|note| note.remote_doc().is_none())
            })
            .collect();
        for note_id in unmapped {
            self.push_note(notes, &note_id);
        }

        let doc_ids: Vec<Uuid> = self.docs.keys().copied().collect();
        for doc in doc_ids {
            let state = self.docs.get_mut(&doc).unwrap();
            let Some(note) = notes.note(&state.note_id) else {
                continue;
            };
            if state.engine_idle
                && note.local_edit_clock() == state.delivered_edit_clock
                && note.needs_push()
            {
                let note_id = state.note_id.clone();
                if let Some(note) = notes.note_mut(&note_id) {
                    note.set_needs_push(false);
                    changed = true;
                }
                continue;
            }
            // Denormalized index entry: keep the title copy fresh.
            let title = note.title();
            if state.index_title.as_deref() != Some(title) {
                state.index_title = Some(title.to_string());
                // Copy the space id out so the `state` borrow ends before the
                // `&mut self` call below.
                let space = state.space;
                self.write_index_entry(space, doc, title);
                changed = true;
            }
        }

        // Space-name diff pass — one u64 compare per pump when nothing moved.
        let spaces_rev = notes.spaces_rev();
        if spaces_rev != self.spaces_rev_seen || self.index_dirty {
            self.spaces_rev_seen = spaces_rev;
            changed |= self.sync_space_names_to_index(notes);
        }

        // Folder diff pass — one u64 compare per pump when nothing moved.
        let dirty = self.index_dirty;
        let rev = notes.folders_rev();
        if rev != self.folders_rev_seen || self.index_dirty {
            self.folders_rev_seen = rev;
            self.index_dirty = false;
            changed |= self.sync_folders_to_index(notes);
        }

        // Blob content reship (durability) + metadata diff pass.
        self.reship_blobs(notes);
        let brev = notes.blobs_rev();
        if brev != self.blobs_rev_seen || dirty {
            self.blobs_rev_seen = brev;
            changed |= self.sync_blobs_to_index(notes);
        }
        changed
    }

    fn handle_event(&mut self, event: AppEvent, notes: &mut NoteDatabase) {
        match event {
            AppEvent::Sync(event) => self.handle_sync_event(event, notes),
            AppEvent::SpacePushed {
                local_space,
                result,
            } => {
                self.pushing.remove(&local_space);
                match result {
                    // Refused while this result was in flight: binding it now
                    // would undo the un-marking that already happened.
                    Ok(space) if self.rejected_spaces.contains(&space) => {
                        log::warn!("not binding space {space}: the relay refused it");
                    }
                    Ok(space) => {
                        notes.set_space_remote(local_space, Some(space));
                        notes.set_space_server(local_space, Some(self.active_server.clone()));
                        self.ensure_index_replica(space);
                        // We created this space, so its name is ours to author.
                        if let Some(replica) = self.index_docs.get_mut(&index_doc_id(&space)) {
                            replica.may_author_name = true;
                        }
                        if let Some(name) = notes.space_name(local_space) {
                            let name = name.to_string();
                            self.write_index_name(space, &name);
                        }
                        for note_id in notes.note_ids_in_space(local_space) {
                            self.push_note(notes, &note_id);
                        }
                    }
                    Err(err) => self.record_error(&err),
                }
            }
            AppEvent::DocCreated {
                note_id,
                space,
                result,
            } => {
                self.creating.remove(&note_id);
                // Doc work that was already in flight when the relay refused the
                // space. It fails with "unknown space" (we dropped the space's
                // state on the refusal), which would replace the one message
                // that tells the user what to actually do about it.
                if self.rejected_spaces.contains(&space) {
                    log::debug!("ignoring doc result for refused space {space}");
                    return;
                }
                match result {
                    Ok(doc) => {
                        if let Some(note) = notes.note_mut(&note_id) {
                            note.set_remote_doc(Some(doc));
                            // Brand-new remote doc: everything local is
                            // unacknowledged by definition.
                            note.set_needs_push(true);
                        }
                        self.register_doc(notes, doc, space, note_id);
                    }
                    Err(err) => self.record_error(&err),
                }
            }
            AppEvent::SpacePeeked { space, result } => {
                if result.is_ok() {
                    // Keys are in hand; opening the index doc is what actually
                    // yields the name, via `sync_index_to_notes`.
                    self.ensure_index_replica(space);
                    self.open_doc_async(space, index_doc_id(&space));
                    // ...but the index only *arrives* once per subscription. If
                    // this doc was already open — a previous mirror that has
                    // since been deleted, or an earlier peek — no further update
                    // lands to trigger the read, and the space would list
                    // without its name. Same reasoning as `SpaceJoined` below.
                    self.sync_index_to_notes(space, notes);
                }
                // A failed peek is not worth reporting: the listing still shows
                // the space by id, and the user can still sync it.
            }
            AppEvent::SpaceJoined { space, result } => {
                self.joining.remove(&space);
                match result {
                    Ok(()) => {
                        // Local mirror space (kept if it already exists).
                        // Name the mirror from the index if we already peeked
                        // it (the sync window does, to list the space by name).
                        // Creating it as a placeholder instead would let the
                        // outbound name pass write that placeholder into the
                        // shared index before the real name is adopted — the map
                        // is last-writer-wins per key and the owner's own pass is
                        // revision-gated, so the space would be renamed for
                        // everyone. Falls back to the placeholder when the peek
                        // never happened; index adoption then renames it.
                        let peeked = self.remote_names.get(&space).cloned();
                        let local = notes.space_by_remote(&space).unwrap_or_else(|| {
                            let local = notes
                                .create_space_named(peeked.as_deref().unwrap_or("Synced space"));
                            notes.set_space_remote(local, Some(space));
                            local
                        });
                        // Bind the mirror to the server it was fetched from.
                        notes.set_space_server(local, Some(self.active_server.clone()));
                        self.ensure_index_replica(space);
                        self.open_doc_async(space, index_doc_id(&space));
                        // Materialize whatever the replica already holds. The
                        // index only *arrives* once per subscription, so if
                        // anything opened this doc earlier — a peek that read
                        // the space's name, or a previous mirror that was
                        // deleted — no further update lands to trigger this,
                        // and the space would appear with none of its notes.
                        self.sync_index_to_notes(space, notes);
                        // (Re-)attach already-mapped notes (boot adopt path).
                        let mapped: Vec<(Uuid, String)> = notes
                            .note_ids_in_space(local)
                            .into_iter()
                            .filter_map(|id| {
                                notes
                                    .note(&id)
                                    .and_then(|note| note.remote_doc())
                                    .map(|doc| (doc, id))
                            })
                            .collect();
                        let docs: Vec<Uuid> = mapped.iter().map(|(doc, _)| *doc).collect();
                        self.open_docs_async(space, docs);
                        for (doc, note_id) in mapped {
                            self.register_doc(notes, doc, space, note_id);
                        }
                        // Learn our own role now so the editor can go
                        // read-only for spaces where we're only a Reader.
                        self.refresh_members(space);
                    }
                    Err(err) => self.record_error(&err),
                }
            }
            AppEvent::DocOpened { result } => {
                if let Err(err) = result {
                    self.record_error(&err);
                }
            }
            AppEvent::SpacesListed { result } => match result {
                Ok(spaces) => self.remote_space_ids = spaces,
                Err(err) => self.record_error(&err),
            },
            AppEvent::MemberAdded { space, result }
            | AppEvent::MemberRemoved { space, result }
            | AppEvent::MemberRoleChanged { space, result } => match result {
                // The engine applied the op to its local log before replying,
                // so a refresh now reflects the change.
                Ok(()) => self.refresh_members(space),
                Err(err) => self.record_error(&err),
            },
            AppEvent::MembersListed { space, result } => {
                self.members_refreshing.remove(&space);
                match result {
                    Ok(entries) => {
                        let self_pk = self.client.device_pk();
                        let mut list: Vec<MemberView> = entries
                            .into_iter()
                            .map(|entry| MemberView {
                                device_pk: entry.device_pk,
                                role: entry.role,
                                is_self: entry.device_pk == self_pk,
                            })
                            .collect();
                        // This device first, then owners → writers → readers.
                        list.sort_by_key(|m| (!m.is_self, m.role as u8, m.device_pk));
                        self.members.insert(space, list);
                    }
                    Err(err) => self.record_error(&err),
                }
            }
            AppEvent::BlobUploaded { blob_id, result } => {
                self.pushing_blobs.remove(&blob_id);
                match result {
                    Ok(()) => {
                        self.blob_upload_attempts.remove(&blob_id);
                        self.failed_blobs.remove(&blob_id);
                        notes.clear_blob_needs_push(&blob_id.to_string());
                    }
                    // Explicitly rejected by the relay: quarantine immediately.
                    Err(SyncError::BlobTooLarge) => {
                        self.quarantine_blob(blob_id, "Upload failed: image is too large to sync");
                    }
                    // A drop (Disconnected) carries no verdict: it might be an
                    // unrelated network blip, or a blob whose frame a proxy keeps
                    // refusing (closing the socket instead of erroring). Retry a
                    // few times, then quarantine so it can't wedge reconnects
                    // forever - the exact failure behind an nginx frame limit.
                    Err(_) => {
                        let attempts = self.blob_upload_attempts.entry(blob_id).or_insert(0);
                        *attempts += 1;
                        if *attempts >= MAX_BLOB_UPLOAD_ATTEMPTS {
                            self.quarantine_blob(
                                blob_id,
                                "Upload failed: couldn't sync image (the server keeps rejecting it)",
                            );
                        }
                    }
                }
            }
            AppEvent::BlobFetched { blob_id, bytes } => {
                if let Some(bytes) = bytes
                    && !notes.set_blob_bytes_from_remote(&blob_id.to_string(), bytes)
                {
                    self.record_durable_error(format!("blob {blob_id}: content hash mismatch"));
                }
            }
            AppEvent::EventsLagged => {
                log::warn!("sync event stream lagged; re-pulling every doc");
                // Dropped DocBytes are unrecoverable locally (the engine is
                // doc-less) — but the server can replay everything.
                let _ = self.client.resync();
            }
        }
    }

    fn handle_sync_event(&mut self, event: SyncEvent, notes: &mut NoteDatabase) {
        match event {
            SyncEvent::Connected => {
                self.connected = true;
                self.connect_failed = false;
                // Reconnecting retires "not connected" and nothing else. Left
                // in place, it sat in Settings → Sync directly beneath a green
                // "Connected", contradicting it.
                if self.last_error_transient {
                    self.last_error = None;
                    self.last_error_transient = false;
                }
                // Re-announce presence from scratch: clear the throttle and
                // forget the focused doc so the next frame's `focus_doc` +
                // `presence_ping` emit a fresh heartbeat right after connecting.
                self.presence_sent.clear();
                self.presence_doc = None;
                // Re-fetch keys/subscriptions for every mapped space.
                self.adopt(notes);
            }
            SyncEvent::Disconnected => {
                self.connected = false;
                for state in self.docs.values_mut() {
                    state.live = false;
                    state.engine_idle = false;
                }
                for replica in self.index_docs.values_mut() {
                    replica.live = false;
                }
            }
            SyncEvent::ConnectError { context } => {
                self.connected = false;
                self.connect_failed = true;
                self.record_durable_error(context);
            }
            SyncEvent::Incompatible {
                server_version,
                client_version,
            } => {
                self.connected = false;
                self.incompatible = Some((server_version, client_version));
                self.record_durable_error(format!(
                    "server speaks protocol v{server_version}, this app speaks \
                     v{client_version} — update whichever is older"
                ));
            }
            SyncEvent::DocBytes {
                doc_id,
                update,
                caret_author,
            } => {
                self.apply_doc_bytes(doc_id, &update, caret_author, notes);
            }
            SyncEvent::DocSynced { doc_id, .. } => {
                if let Some(replica) = self.index_docs.get_mut(&doc_id) {
                    replica.live = true;
                    let space = replica.space;
                    // Caught up: the maps are now authoritative — materialize
                    // them (incl. folder deletions) and schedule the outbound
                    // diff that writes anything locally flagged.
                    self.sync_index_to_notes(space, notes);
                    self.index_dirty = true;
                }
                if let Some(state) = self.docs.get_mut(&doc_id) {
                    state.live = true;
                    // Reship anything the server may not have (offline edits,
                    // crash recovery, fresh docs). Idempotent at Yrs level.
                    let note_id = state.note_id.clone();
                    if let Some(note) = notes.note(&note_id)
                        && note.needs_push()
                    {
                        let update = note.encode_update_since(&StateVector::default());
                        let _ = self.client.queue_update(doc_id, update);
                    }
                }
            }
            SyncEvent::SnapshotNeeded { doc_id, covers_seq } => {
                let state = if let Some(replica) = self.index_docs.get(&doc_id) {
                    Some(
                        replica
                            .doc
                            .transact()
                            .encode_state_as_update_v1(&StateVector::default()),
                    )
                } else if let Some(doc_state) = self.docs.get(&doc_id) {
                    notes
                        .note(&doc_state.note_id)
                        .map(|note| note.encode_update_since(&StateVector::default()))
                } else {
                    None
                };
                if let Some(state) = state {
                    let _ = self.client.provide_snapshot(doc_id, covers_seq, state);
                }
            }
            SyncEvent::DocIdle { doc_id } => {
                if let Some(state) = self.docs.get_mut(&doc_id) {
                    state.engine_idle = true;
                    // All local edits are now delivered: presence anchors are
                    // safe to send again (they reference content peers have).
                    let clock = notes.note(&state.note_id).map(|n| n.local_edit_clock());
                    if let Some(clock) = clock {
                        state.delivered_edit_clock = clock;
                    }
                }
                if self.index_docs.contains_key(&doc_id) {
                    self.clear_acked_folder_flags(doc_id, notes);
                }
            }
            SyncEvent::DocBusy { doc_id } => {
                if let Some(state) = self.docs.get_mut(&doc_id) {
                    state.engine_idle = false;
                }
            }
            SyncEvent::EpochBumped { .. } => {}
            SyncEvent::SpaceDeleted { space_id } => {
                // The space was destroyed server-side (by its owner). We keep
                // the local copy and its notes — only the sync link is severed,
                // turning it back into a local-only space. Also forget it in the
                // cached remote list so it vanishes from the sync window without
                // waiting for a manual Refresh.
                self.unsync_space(space_id, notes);
                self.remote_space_ids.retain(|id| *id != space_id);
            }
            SyncEvent::Ephemeral {
                doc_id,
                author_device,
                payload,
            } => {
                match decode_presence(&payload) {
                    Some(PresenceUpdate::Here {
                        nickname,
                        caret,
                        selection_anchor,
                    }) => {
                        // Unnamed peers still get a stable label so their caret
                        // tag isn't blank.
                        let nickname = if nickname.is_empty() {
                            short_device_id(&author_device)
                        } else {
                            nickname
                        };
                        self.presence.entry(doc_id).or_default().insert(
                            author_device,
                            Presence {
                                device: author_device,
                                nickname,
                                caret,
                                selection_anchor,
                                last_seen: Instant::now(),
                            },
                        );
                    }
                    Some(PresenceUpdate::Gone) => {
                        if let Some(entries) = self.presence.get_mut(&doc_id) {
                            entries.remove(&author_device);
                        }
                    }
                    None => {}
                }
            }
            SyncEvent::SecurityWarning { context } => {
                log::warn!("security warning: {context}");
                // Surface it: dropped frames are how silent divergence starts.
                self.record_durable_error(format!("security warning: {context}"));
            }
            SyncEvent::Account { info } => {
                self.account = info;
            }
            SyncEvent::SpaceRejected { space_id, context } => {
                log::warn!("relay refused space {space_id}: {context}");
                // The push was fire-and-forget, so the space is already carrying
                // a remote id the relay never stored. Undo that binding or it
                // reads as synced forever while nothing reaches the server.
                self.rejected_spaces.insert(space_id);
                if let Some(local) = notes.space_by_remote(&space_id) {
                    notes.set_space_remote(local, None);
                    notes.set_space_server(local, None);
                    self.pushing.remove(&local);
                    // Un-map the notes too. `push_space` optimistically gives
                    // each one a remote doc id, and unbinding only the *space*
                    // leaves those pointing at docs on a space the relay never
                    // stored — which `note_indicator` reads as "Synchronizing"
                    // (a doc it has no engine state for), so every note in the
                    // space shows a pending-change dot for a push that can
                    // never happen. Clearing `needs_push` with it: there is
                    // nothing to push to, and re-syncing the space later sets
                    // both again.
                    for note_id in notes.note_ids_in_space(local) {
                        if let Some(note) = notes.note_mut(&note_id) {
                            note.set_remote_doc(None);
                            note.set_needs_push(false);
                        }
                    }
                }
                // Engine-side doc state for the space is already gone
                // (`forget_space_state`); drop the bridge's view of it too.
                self.docs.retain(|_, doc| doc.space != space_id);
                self.index_docs.remove(&index_doc_id(&space_id));
                self.remote_names.remove(&space_id);
                self.peeked.remove(&space_id);
                self.record_durable_error(
                    "this server needs an account to sync a space".to_string(),
                );
                self.notices.push(Notice {
                    level: NoticeLevel::Danger,
                    message: "This server needs an account to sync a space - add your \
                              token in Settings > Sync. Your notes are safe, still \
                              stored on this device."
                        .to_string(),
                });
            }
            SyncEvent::Rejected { context } => {
                log::warn!("relay refused this device: {context}");
                self.connected = false;
                self.rejected = true;
                self.record_durable_error(
                    "this server refused your account token - check it in Settings > Sync"
                        .to_string(),
                );
            }
            SyncEvent::ServerError { code, context } => {
                // The relay's own wording is for an operator reading logs. These
                // two codes are the ones a paying customer will actually hit, so
                // they get an answer that says what to do about it. The
                // connection is fine — only this operation was refused.
                match code {
                    ErrorCode::AccountRequired => self.record_durable_error(
                        "this server needs an account token to create a space - \
                         add one in Settings > Sync"
                            .to_string(),
                    ),
                    ErrorCode::QuotaExceeded => self.record_durable_error(
                        "storage full - delete some notes or images, or upgrade your plan"
                            .to_string(),
                    ),
                    _ => self.record_durable_error(context),
                }
            }
            SyncEvent::UpdateTooLarge { doc_id, bytes } => {
                log::warn!("doc {doc_id}: update of {bytes} bytes too large to sync");
                self.record_durable_error(format!(
                    "an edit is too large to sync ({} MB) - try removing large pasted content",
                    bytes / (1024 * 1024)
                ));
            }
        }
    }

    /// Apply decrypted remote update bytes to the right replica.
    fn apply_doc_bytes(
        &mut self,
        doc_id: Uuid,
        update: &[u8],
        caret_author: Option<DevicePk>,
        notes: &mut NoteDatabase,
    ) {
        let decoded = match Update::decode_v1(update) {
            Ok(decoded) => decoded,
            Err(err) => {
                self.record_durable_error(format!("undecodable remote update: {err}"));
                return;
            }
        };
        if let Some(replica) = self.index_docs.get(&doc_id) {
            let space = replica.space;
            // Take the failure out of the borrow before reporting it.
            let merge_failed = {
                let mut txn = replica.doc.transact_mut_with(REMOTE_ORIGIN);
                txn.apply_update(decoded).err()
            };
            if let Some(err) = merge_failed {
                self.record_durable_error(format!("merge of index update failed: {err}"));
                return;
            }
            self.sync_index_to_notes(space, notes);
            return;
        }
        let Some(state) = self.docs.get(&doc_id) else {
            return;
        };
        let note_id = state.note_id.clone();
        let Some(note) = notes.note_mut(&note_id) else {
            return;
        };
        // For a live edit, track where the author's caret landed and move their
        // remote caret to it at apply time — no trailing presence ping needed.
        match caret_author {
            Some(author) => match note.apply_remote_update_tracking_caret(decoded) {
                Ok(caret) => self.set_edit_caret(doc_id, author, caret),
                Err(err) => {
                    self.record_durable_error(format!("merge of remote update failed: {err}"));
                }
            },
            None => {
                if let Err(err) = note.apply_remote_update_decoded(decoded) {
                    self.record_durable_error(format!("merge of remote update failed: {err}"));
                }
            }
        }
    }

    /// Place `author`'s remote caret at an edit-derived position. Selection
    /// collapses on type, so any prior selection anchor is cleared. Preserves
    /// the nickname learned from earlier presence pings (falls back to a short
    /// device id until one arrives).
    fn set_edit_caret(&mut self, doc_id: Uuid, author: DevicePk, caret: Option<StickyIndex>) {
        let entry = self.presence.entry(doc_id).or_default();
        let nickname = entry
            .get(&author)
            .map(|p| p.nickname.clone())
            .unwrap_or_else(|| short_device_id(&author));
        entry.insert(
            author,
            Presence {
                device: author,
                nickname,
                caret,
                selection_anchor: None,
                last_seen: Instant::now(),
            },
        );
    }

    /// Read the index's space name, and record that we have seen one — from
    /// then on a local rename may be written back.
    fn adopt_index_space_name(&mut self, space: Uuid) -> Option<String> {
        let name = self.index_space_name(space)?;
        if let Some(replica) = self.index_docs.get_mut(&index_doc_id(&space)) {
            replica.may_author_name = true;
        }
        Some(name)
    }

    /// The space name held in an index replica, if that replica exists and
    /// carries one.
    fn index_space_name(&self, space: Uuid) -> Option<String> {
        let replica = self.index_docs.get(&index_doc_id(&space))?;
        let meta = replica.doc.get_or_insert_map(INDEX_META_MAP);
        let txn = replica.doc.transact();
        match meta.get(&txn, "name") {
            Some(Out::Any(Any::String(name))) => Some(name.to_string()),
            _ => None,
        }
    }

    /// Fetch a remote space's keys and index doc *without* mirroring it
    /// locally, so its name can be shown before the user commits to syncing.
    ///
    /// Joining is what grants the keys, and the name is only readable once
    /// something can decrypt the index — the server holds ciphertext and could
    /// not tell us even if asked.
    pub fn peek_space(&mut self, space: Uuid) {
        if self.joining.contains(&space)
            || self.remote_names.contains_key(&space)
            || !self.peeked.insert(space)
        {
            return;
        }
        let client = self.client.clone();
        self.submit(async move {
            AppEvent::SpacePeeked {
                space,
                result: client.join_space(space).await,
            }
        });
    }

    /// A remote space's name, once a peek (or a sync) has decrypted it.
    pub fn remote_space_name(&self, space: Uuid) -> Option<&str> {
        self.remote_names.get(&space).map(String::as_str)
    }

    /// Materialize index doc contents: adopt newly-listed note docs and the
    /// space name.
    fn sync_index_to_notes(&mut self, space: Uuid, notes: &mut NoteDatabase) {
        // No local mirror: this is a peek, so remember the name for the
        // remote-spaces list and stop there — adopting notes/folders would
        // amount to syncing a space the user has not asked for.
        if notes.space_by_remote(&space).is_none() {
            if let Some(name) = self.adopt_index_space_name(space) {
                self.remote_names.insert(space, name);
            }
            return;
        }
        let Some(local) = notes.space_by_remote(&space) else {
            return;
        };
        let index_id = index_doc_id(&space);
        let (name, entries, folder_entries, parent_entries, assignments, live) = {
            let Some(replica) = self.index_docs.get(&index_id) else {
                return;
            };
            let notes_map = replica.doc.get_or_insert_map(INDEX_NOTES_MAP);
            let meta_map = replica.doc.get_or_insert_map(INDEX_META_MAP);
            let folders_map = replica.doc.get_or_insert_map(INDEX_FOLDERS_MAP);
            let parents_map = replica.doc.get_or_insert_map(INDEX_FOLDER_PARENTS_MAP);
            let assign_map = replica.doc.get_or_insert_map(INDEX_NOTE_FOLDERS_MAP);
            let txn = replica.doc.transact();
            // The index map *is* `doc-uuid -> title`; the title was being read
            // and dropped, so a fetched space arrived as a list of untitled
            // notes. Folder names came through this same map and were applied,
            // which is why only note titles were missing.
            let mut entries: Vec<(Uuid, String)> = Vec::new();
            for (key, value) in notes_map.iter(&txn) {
                if let (Ok(doc_id), Out::Any(Any::String(title))) = (Uuid::parse_str(key), value) {
                    entries.push((doc_id, title.to_string()));
                }
            }
            let mut folder_entries = Vec::new();
            for (key, value) in folders_map.iter(&txn) {
                if let (Ok(folder_id), Out::Any(Any::String(name))) = (Uuid::parse_str(key), value)
                {
                    folder_entries.push((folder_id, name.to_string()));
                }
            }
            let mut parent_entries: Vec<(Uuid, Uuid)> = Vec::new();
            for (key, value) in parents_map.iter(&txn) {
                if let (Ok(folder_id), Out::Any(Any::String(parent))) =
                    (Uuid::parse_str(key), value)
                    && let Ok(parent_id) = Uuid::parse_str(&parent)
                {
                    parent_entries.push((folder_id, parent_id));
                }
            }
            let mut assignments: Vec<(Uuid, Uuid)> = Vec::new();
            for (key, value) in assign_map.iter(&txn) {
                if let (Ok(doc_id), Out::Any(Any::String(folder))) = (Uuid::parse_str(key), value)
                    && let Ok(folder_id) = Uuid::parse_str(&folder)
                {
                    assignments.push((doc_id, folder_id));
                }
            }
            let name = match meta_map.get(&txn, "name") {
                Some(Out::Any(Any::String(name))) => Some(name.to_string()),
                _ => None,
            };
            (
                name,
                entries,
                folder_entries,
                parent_entries,
                assignments,
                replica.live,
            )
        };
        if let Some(name) = name {
            // Seeing a name in the index is what earns the right to write one
            // back, so record it even when it already matches locally.
            if let Some(replica) = self.index_docs.get_mut(&index_id) {
                replica.may_author_name = true;
            }
            if notes.space_name(local) != Some(name.as_str()) {
                notes.rename_space(local, &name);
            }
        }
        let mut adopted: Vec<Uuid> = Vec::new();
        for (doc, title) in entries {
            if let Some(existing) = notes.note_id_by_remote_doc(&doc).map(str::to_string) {
                // The note is already here. Adopt the index's title only when
                // it differs from what *we* last wrote there — that is exactly
                // "someone else renamed it", and it leaves a local rename that
                // has not been pushed yet alone.
                let ours = self
                    .docs
                    .get(&doc)
                    .and_then(|state| state.index_title.clone());
                if !title.is_empty()
                    && ours.as_deref() != Some(title.as_str())
                    && notes.note_title(&existing) != Some(title.as_str())
                {
                    notes.set_note_title(&existing, &title);
                    if let Some(state) = self.docs.get_mut(&doc) {
                        state.index_title = Some(title.clone());
                    }
                }
                continue;
            }
            if self.docs.contains_key(&doc) {
                continue;
            }
            let note_id = notes.create_note_from_remote(local, doc);
            if !title.is_empty() {
                notes.set_note_title(&note_id, &title);
            }
            // Subscribed in one batch below: a space with 10k notes would
            // otherwise send 10k single-entry `Subscribe` frames here.
            adopted.push(doc);
            self.register_doc(notes, doc, space, note_id);
            // Record what the index already says, so the writer pass does not
            // immediately rewrite the same value back.
            if let Some(state) = self.docs.get_mut(&doc) {
                state.index_title = Some(title);
            }
        }
        self.open_docs_async(space, adopted);

        // Folders: upsert from the map. Skip ids queued for removal here
        // (the deletion hasn't reached the map yet) — without this, a peer's
        // index update would resurrect a folder we just deleted.
        let pending_removals = self.pending_folder_removals.get(&index_id);
        for (folder_id, folder_name) in &folder_entries {
            if pending_removals.is_some_and(|ids| ids.contains(folder_id)) {
                continue;
            }
            let parent = parent_entries
                .iter()
                .find(|(id, _)| id == folder_id)
                .map(|(_, parent)| *parent)
                .filter(|parent| parent != folder_id)
                .filter(|parent| {
                    pending_removals.is_none_or(|ids| !ids.contains(parent))
                        && folder_entries
                            .iter()
                            .any(|(candidate, _)| candidate == parent)
                });
            notes.adopt_remote_folder(local, *folder_id, parent, folder_name);
        }
        // A live map is authoritative for existence: unflagged local folders
        // absent from it were deleted remotely.
        if live {
            let stale: Vec<Uuid> = notes
                .folders_in_space(local)
                .filter(|folder| {
                    !folder.needs_push && !folder_entries.iter().any(|(id, _)| *id == folder.id)
                })
                .map(|folder| folder.id)
                .collect();
            for folder in stale {
                notes.delete_folder(&folder);
            }
        }
        // Note → folder assignments for every locally-mapped doc. Flagged
        // notes keep their local assignment until it's written + acked.
        let moves: Vec<(String, Option<Uuid>)> = self
            .docs
            .iter()
            .filter(|(_, state)| state.space == space)
            .filter_map(|(doc, state)| {
                let note = notes.note(&state.note_id)?;
                if note.folder_needs_push() {
                    return None;
                }
                let desired = assignments
                    .iter()
                    .find(|(d, _)| d == doc)
                    .map(|(_, folder)| *folder)
                    .filter(|folder| notes.folder(folder).is_some());
                (note.folder() != desired).then(|| (state.note_id.clone(), desired))
            })
            .collect();
        for (note_id, folder) in moves {
            notes.set_note_folder_from_remote(&note_id, folder);
        }

        self.adopt_remote_blobs(space, local, live, notes);
    }

    /// Inbound blob pass: read the index `blobs`/`blob_meta`/`blob_folders` maps
    /// and adopt any blob not present locally (metadata only), then fetch its
    /// content by id. Renames/moves of already-known blobs are left to the
    /// owner (a peer never rewrites another device's blob metadata here).
    fn adopt_remote_blobs(
        &mut self,
        space: Uuid,
        local: i64,
        live: bool,
        notes: &mut NoteDatabase,
    ) {
        // Only adopt + fetch from a caught-up index. During backlog replay the
        // map passes through historical intermediate states - a blob inserted
        // early in history and deleted later - and fetching off those transients
        // fires a GetBlob for every image the final state no longer has (an
        // expensive round trip per deleted blob on each reconnect). The
        // `DocSynced` handler re-runs this pass once the replica is live and its
        // map is authoritative.
        if !live {
            return;
        }
        let index_id = index_doc_id(&space);
        let entries: Vec<(Uuid, String, Option<Uuid>, ImageMime, [u8; 32], [u8; 32])> = {
            let Some(replica) = self.index_docs.get(&index_id) else {
                return;
            };
            let blobs_map = replica.doc.get_or_insert_map(INDEX_BLOBS_MAP);
            let folders_map = replica.doc.get_or_insert_map(INDEX_BLOB_FOLDERS_MAP);
            let meta_map = replica.doc.get_or_insert_map(INDEX_BLOB_META_MAP);
            let txn = replica.doc.transact();
            let mut out = Vec::new();
            for (key, value) in blobs_map.iter(&txn) {
                let (Ok(blob_id), Out::Any(Any::String(name))) = (Uuid::parse_str(key), value)
                else {
                    continue;
                };
                let Some((mime, hash, blob_key)) = (match meta_map.get(&txn, key) {
                    Some(Out::Any(Any::String(meta))) => parse_blob_meta(&meta),
                    _ => None,
                }) else {
                    continue;
                };
                let folder = match folders_map.get(&txn, key) {
                    Some(Out::Any(Any::String(id))) => Uuid::parse_str(&id).ok(),
                    _ => None,
                };
                out.push((blob_id, name.to_string(), folder, mime, hash, blob_key));
            }
            out
        };
        // Skip ids queued for removal here: the deletion hasn't reached the map
        // yet, so without this an inbound index update would resurrect a blob we
        // just deleted locally.
        let pending_removals = self.pending_blob_removals.get(&index_id);
        for (blob_id, name, folder, mime, hash, blob_key) in &entries {
            if pending_removals.is_some_and(|ids| ids.contains(blob_id)) {
                continue;
            }
            let folder = folder.filter(|f| notes.folder(f).is_some());
            let needs_fetch = notes.upsert_blob_meta_from_remote(
                &blob_id.to_string(),
                local,
                name,
                *mime,
                *hash,
                *blob_key,
                folder,
            );
            if needs_fetch {
                self.request_blob(space, *blob_id, *blob_key);
            }
        }
        // A live map is authoritative for existence: an unflagged local blob
        // absent from it was deleted on another device, so drop it here too.
        let stale: Vec<String> = notes
            .blobs_in_space(local)
            .filter(|blob| {
                !blob.needs_push && !entries.iter().any(|(id, ..)| id.to_string() == blob.id)
            })
            .map(|blob| blob.id.clone())
            .collect();
        for blob in stale {
            notes.delete_blob(&blob);
        }
    }

    /// Outbound folder pass: write local folder names + note assignments
    /// into every live index replica wherever they differ from the map.
    /// Local state always wins here — inbound applies skip anything flagged
    /// `needs_push`, so by the time this runs, unflagged state already
    /// matches the map and the diff is a no-op.
    fn sync_folders_to_index(&mut self, notes: &NoteDatabase) -> bool {
        let mut changed = false;
        for (index_id, replica) in &self.index_docs {
            if !replica.live {
                continue;
            }
            let Some(local_space) = notes.space_by_remote(&replica.space) else {
                continue;
            };
            let folders_map = replica.doc.get_or_insert_map(INDEX_FOLDERS_MAP);
            let parents_map = replica.doc.get_or_insert_map(INDEX_FOLDER_PARENTS_MAP);
            let assign_map = replica.doc.get_or_insert_map(INDEX_NOTE_FOLDERS_MAP);
            let mut txn = replica.doc.transact_mut();

            for folder in self
                .pending_folder_removals
                .remove(index_id)
                .unwrap_or_default()
            {
                let key = folder.to_string();
                folders_map.remove(&mut txn, &key);
                parents_map.remove(&mut txn, &key);
                changed = true;
            }

            for folder in notes.folders_in_space(local_space) {
                let key = folder.id.to_string();
                let current = match folders_map.get(&txn, &key) {
                    Some(Out::Any(Any::String(name))) => Some(name.to_string()),
                    _ => None,
                };
                if current.as_deref() != Some(folder.name.as_str()) {
                    folders_map.insert(&mut txn, key, folder.name.clone());
                    changed = true;
                }
                let parent_key = folder.id.to_string();
                let current_parent = match parents_map.get(&txn, &parent_key) {
                    Some(Out::Any(Any::String(id))) => Uuid::parse_str(&id).ok(),
                    _ => None,
                };
                if current_parent != folder.parent {
                    match folder.parent {
                        Some(parent) => {
                            parents_map.insert(&mut txn, parent_key, parent.to_string());
                        }
                        None => {
                            parents_map.remove(&mut txn, &parent_key);
                        }
                    }
                    changed = true;
                }
            }

            for (doc, state) in &self.docs {
                if state.space != replica.space {
                    continue;
                }
                let local = notes.note(&state.note_id).and_then(|note| note.folder());
                let key = doc.to_string();
                let current = match assign_map.get(&txn, &key) {
                    Some(Out::Any(Any::String(id))) => Uuid::parse_str(&id).ok(),
                    _ => None,
                };
                if current != local {
                    match local {
                        Some(folder) => {
                            assign_map.insert(&mut txn, key, folder.to_string());
                        }
                        None => {
                            assign_map.remove(&mut txn, &key);
                        }
                    }
                    changed = true;
                }
            }
        }
        changed
    }

    /// Park a blob out of the reship loop so a single unshippable blob can't
    /// keep tearing the connection down, and tell the user it won't sync. The
    /// blob stays on disk and visible locally (with a "not uploaded" badge);
    /// only its server upload is given up. `message` is shown to the user.
    fn quarantine_blob(&mut self, blob_id: Uuid, message: &str) {
        self.blob_upload_attempts.remove(&blob_id);
        self.failed_blobs.insert(blob_id);
        log::warn!("blob {blob_id} quarantined: {message}");
        self.record_durable_error(message.to_string());
        self.notices.push(Notice {
            level: NoticeLevel::Danger,
            message: message.to_string(),
        });
    }

    /// Drain queued user-facing messages (the UI raises them as toasts).
    pub fn take_notices(&mut self) -> Vec<Notice> {
        std::mem::take(&mut self.notices)
    }

    /// Whether a blob's upload was permanently given up on (too large, or the
    /// relay kept refusing it). Drives the "not uploaded" badge in the UI.
    pub fn blob_failed(&self, blob_id: &Uuid) -> bool {
        self.failed_blobs.contains(blob_id)
    }

    /// Re-upload the content of any flagged blob in a synced space (covers the
    /// crash/offline case). Deduped by `pushing_blobs`; cleared on `BlobUploaded`.
    fn reship_blobs(&mut self, notes: &NoteDatabase) {
        let pending: Vec<(Uuid, Uuid, [u8; 32], Vec<u8>)> = notes
            .spaces()
            .iter()
            .filter(|space| {
                space.server.as_deref() == Some(self.active_server.as_str())
                    && space.remote.is_some()
            })
            .flat_map(|space| {
                let remote = space.remote.unwrap();
                notes
                    .blobs_in_space(space.id)
                    .filter(|blob| blob.needs_push && !blob.bytes.is_empty())
                    .filter_map(|blob| {
                        let id = Uuid::parse_str(&blob.id).ok()?;
                        (!self.pushing_blobs.contains(&id) && !self.failed_blobs.contains(&id))
                            .then(|| (remote, id, blob.key, blob.bytes.clone()))
                    })
                    .collect::<Vec<_>>()
            })
            .collect();
        for (remote, id, key, bytes) in pending {
            self.pushing_blobs.insert(id);
            self.upload_blob(remote, id, key, bytes);
        }
    }

    /// Outbound blob metadata pass: write each local blob's name, folder and
    /// `mime:hash` into every live index replica wherever the map differs.
    /// Idempotent — a CRDT map `insert` only changes state on a real diff.
    fn sync_blobs_to_index(&mut self, notes: &NoteDatabase) -> bool {
        let mut changed = false;
        for (index_id, replica) in &self.index_docs {
            if !replica.live {
                continue;
            }
            let Some(local_space) = notes.space_by_remote(&replica.space) else {
                continue;
            };
            let blobs_map = replica.doc.get_or_insert_map(INDEX_BLOBS_MAP);
            let folders_map = replica.doc.get_or_insert_map(INDEX_BLOB_FOLDERS_MAP);
            let meta_map = replica.doc.get_or_insert_map(INDEX_BLOB_META_MAP);
            let mut txn = replica.doc.transact_mut();
            // Prune blobs deleted locally: drop their advertisement from every
            // index map so peers stop resolving the link and a restart won't
            // re-adopt + re-download the image.
            for blob in self
                .pending_blob_removals
                .remove(index_id)
                .unwrap_or_default()
            {
                let key = blob.to_string();
                blobs_map.remove(&mut txn, &key);
                meta_map.remove(&mut txn, &key);
                folders_map.remove(&mut txn, &key);
                changed = true;
            }
            for blob in notes.blobs_in_space(local_space) {
                // Durability gate: never advertise a blob to peers until the
                // relay has confirmed its content is stored (`needs_push`
                // cleared). Otherwise a peer resolves the `./blob/<name>` link
                // to a blob the relay can't serve - a permanent broken image.
                // Clearing `needs_push` bumps `blobs_rev`, which re-runs this
                // pass to publish the entry once the upload lands. An already
                // published blob keeps its existing entry (we just skip updating
                // it) while a re-upload is in flight.
                if blob.needs_push {
                    continue;
                }
                let key = blob.id.clone();
                let current_name = match blobs_map.get(&txn, &key) {
                    Some(Out::Any(Any::String(name))) => Some(name.to_string()),
                    _ => None,
                };
                if current_name.as_deref() != Some(blob.name.as_str()) {
                    blobs_map.insert(&mut txn, key.clone(), blob.name.clone());
                    changed = true;
                }
                // mime : content-hash : content-key. The key rides in the index
                // doc precisely so it is re-sealed under the current epoch by
                // the index doc's own snapshot compaction — that is what lets a
                // rotation roll blob keys forward without re-encrypting blobs.
                let want_meta = format!(
                    "{}:{}:{}",
                    blob.mime as u8,
                    hex32(&blob.content_hash),
                    hex32(&blob.key)
                );
                let current_meta = match meta_map.get(&txn, &key) {
                    Some(Out::Any(Any::String(meta))) => Some(meta.to_string()),
                    _ => None,
                };
                if current_meta.as_deref() != Some(want_meta.as_str()) {
                    meta_map.insert(&mut txn, key.clone(), want_meta);
                    changed = true;
                }
                let current_folder = match folders_map.get(&txn, &key) {
                    Some(Out::Any(Any::String(id))) => Uuid::parse_str(&id).ok(),
                    _ => None,
                };
                if current_folder != blob.folder {
                    match blob.folder {
                        Some(folder) => {
                            folders_map.insert(&mut txn, key, folder.to_string());
                        }
                        None => {
                            folders_map.remove(&mut txn, &key);
                        }
                    }
                    changed = true;
                }
            }
        }
        changed
    }

    /// Outbound space metadata pass: keep the remote index doc's name copy
    /// aligned with local renames after the initial push.
    fn sync_space_names_to_index(&mut self, notes: &NoteDatabase) -> bool {
        let mut changed = false;
        for replica in self.index_docs.values() {
            if !replica.live || !replica.may_author_name {
                continue;
            }
            let Some(local_space) = notes.space_by_remote(&replica.space) else {
                continue;
            };
            let Some(name) = notes.space_name(local_space) else {
                continue;
            };
            let meta_map = replica.doc.get_or_insert_map(INDEX_META_MAP);
            let mut txn = replica.doc.transact_mut();
            let current = match meta_map.get(&txn, "name") {
                Some(Out::Any(Any::String(existing))) => Some(existing.to_string()),
                _ => None,
            };
            if current.as_deref() != Some(name) {
                meta_map.insert(&mut txn, "name", name.to_string());
                changed = true;
            }
        }
        changed
    }

    /// The engine acknowledged everything queued for this index doc: clear
    /// `needs_push` on folders/assignments whose map entry now matches the
    /// local state (i.e. our write landed, not someone else's).
    fn clear_acked_folder_flags(&mut self, index_id: Uuid, notes: &mut NoteDatabase) {
        let Some(replica) = self.index_docs.get(&index_id) else {
            return;
        };
        let Some(local_space) = notes.space_by_remote(&replica.space) else {
            return;
        };
        let space = replica.space;
        let (acked_folders, acked_notes) = {
            let folders_map = replica.doc.get_or_insert_map(INDEX_FOLDERS_MAP);
            let parents_map = replica.doc.get_or_insert_map(INDEX_FOLDER_PARENTS_MAP);
            let assign_map = replica.doc.get_or_insert_map(INDEX_NOTE_FOLDERS_MAP);
            let txn = replica.doc.transact();
            let acked_folders: Vec<Uuid> = notes
                .folders_in_space(local_space)
                .filter(|folder| {
                    if !folder.needs_push
                        || !matches!(
                            folders_map.get(&txn, &folder.id.to_string()),
                            Some(Out::Any(Any::String(name))) if *name == *folder.name
                        )
                    {
                        return false;
                    }
                    let parent = match parents_map.get(&txn, &folder.id.to_string()) {
                        Some(Out::Any(Any::String(id))) => Uuid::parse_str(&id).ok(),
                        _ => None,
                    };
                    parent == folder.parent
                })
                .map(|folder| folder.id)
                .collect();
            let acked_notes: Vec<String> = self
                .docs
                .iter()
                .filter(|(_, state)| state.space == space)
                .filter_map(|(doc, state)| {
                    let note = notes.note(&state.note_id)?;
                    if !note.folder_needs_push() {
                        return None;
                    }
                    let map_value = match assign_map.get(&txn, &doc.to_string()) {
                        Some(Out::Any(Any::String(id))) => Uuid::parse_str(&id).ok(),
                        _ => None,
                    };
                    (map_value == note.folder()).then(|| state.note_id.clone())
                })
                .collect();
            (acked_folders, acked_notes)
        };
        for folder in acked_folders {
            notes.clear_folder_needs_push(&folder);
        }
        for note_id in acked_notes {
            notes.clear_note_folder_needs_push(&note_id);
        }
    }

    fn write_index_entry(&mut self, space: Uuid, doc: Uuid, title: &str) {
        let index_id = index_doc_id(&space);
        let Some(replica) = self.index_docs.get(&index_id) else {
            return;
        };
        let map = replica.doc.get_or_insert_map(INDEX_NOTES_MAP);
        let mut txn = replica.doc.transact_mut();
        let key = doc.to_string();
        let current = match map.get(&txn, &key) {
            Some(Out::Any(Any::String(existing))) => existing.to_string(),
            _ => String::new(),
        };
        if current != title {
            map.insert(&mut txn, key, title.to_string());
        }
    }

    fn write_index_name(&mut self, space: Uuid, name: &str) {
        let index_id = index_doc_id(&space);
        let Some(replica) = self.index_docs.get(&index_id) else {
            return;
        };
        let map = replica.doc.get_or_insert_map(INDEX_META_MAP);
        let mut txn = replica.doc.transact_mut();
        map.insert(&mut txn, "name", name.to_string());
    }
}

// -- device key + presence codecs ------------------------------------------------

fn encode_device_key(device_pk: &DevicePk, kex_pk: &KexPk) -> String {
    let mut out = String::with_capacity(128);
    for b in device_pk.iter().chain(kex_pk.iter()) {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

fn decode_device_key(text: &str) -> Result<(DevicePk, KexPk), String> {
    let text: String = text.chars().filter(|c| !c.is_whitespace()).collect();
    if text.len() != 128 {
        return Err("device key must be 128 hex characters".into());
    }
    let mut bytes = [0u8; 64];
    for (i, chunk) in text.as_bytes().chunks(2).enumerate() {
        let hex = std::str::from_utf8(chunk).map_err(|_| "invalid device key")?;
        bytes[i] = u8::from_str_radix(hex, 16).map_err(|_| "invalid device key")?;
    }
    let mut device_pk = [0u8; 32];
    let mut kex_pk = [0u8; 32];
    device_pk.copy_from_slice(&bytes[..32]);
    kex_pk.copy_from_slice(&bytes[32..]);
    Ok((device_pk, kex_pk))
}

/// A decoded presence ephemeral.
enum PresenceUpdate {
    /// The author is on this doc, with their caret/selection anchors.
    Here {
        nickname: String,
        caret: Option<yrs::StickyIndex>,
        selection_anchor: Option<yrs::StickyIndex>,
    },
    /// The author left this doc (focused another note / disconnected). Peers
    /// drop their caret here so the indicator clears without waiting for TTL.
    Gone,
}

/// Postcard tuple `(nickname, caret_sticky_v1, selection_anchor_sticky_v1, leaving)`;
/// the sticky indexes travel in Yrs' own v1 binary encoding. `leaving = true`
/// is a tombstone (the other fields are unused).
fn encode_presence(
    nickname: &str,
    caret: Option<&yrs::StickyIndex>,
    selection_anchor: Option<&yrs::StickyIndex>,
) -> Vec<u8> {
    use yrs::updates::encoder::Encode;
    let tuple = (
        nickname.to_string(),
        caret.map(|sticky| sticky.encode_v1()),
        selection_anchor.map(|sticky| sticky.encode_v1()),
        false,
    );
    enkr_proto::wire::encode(&tuple).unwrap_or_default()
}

/// A "left this doc" tombstone — see [`PresenceUpdate::Gone`].
fn encode_presence_leave() -> Vec<u8> {
    let tuple = (String::new(), None::<Vec<u8>>, None::<Vec<u8>>, true);
    enkr_proto::wire::encode(&tuple).unwrap_or_default()
}

fn decode_presence(payload: &[u8]) -> Option<PresenceUpdate> {
    let (nickname, caret, selection_anchor, leaving): (
        String,
        Option<Vec<u8>>,
        Option<Vec<u8>>,
        bool,
    ) = enkr_proto::wire::decode(payload).ok()?;
    if leaving {
        return Some(PresenceUpdate::Gone);
    }
    // An empty nickname is allowed; the caller substitutes a device-id label.
    let caret = caret.and_then(|bytes| yrs::StickyIndex::decode_v1(&bytes).ok());
    let selection_anchor =
        selection_anchor.and_then(|bytes| yrs::StickyIndex::decode_v1(&bytes).ok());
    Some(PresenceUpdate::Here {
        nickname,
        caret,
        selection_anchor,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_key_roundtrip() {
        let device_pk = [7u8; 32];
        let kex_pk = [9u8; 32];
        let text = encode_device_key(&device_pk, &kex_pk);
        assert_eq!(text.len(), 128);
        assert_eq!(decode_device_key(&text).unwrap(), (device_pk, kex_pk));
        // Whitespace-tolerant (pasted keys often wrap).
        let spaced = format!("{} {}", &text[..64], &text[64..]);
        assert_eq!(decode_device_key(&spaced).unwrap(), (device_pk, kex_pk));
        assert!(decode_device_key("abc").is_err());
    }

    #[test]
    fn presence_roundtrip() {
        use yrs::{Doc, GetString, IndexedSequence, Text, Transact};
        let doc = Doc::new();
        let text = doc.get_or_insert_text("body");
        text.insert(&mut doc.transact_mut(), 0, "hello world");
        let caret = text
            .sticky_index(&doc.transact(), 5, yrs::Assoc::Before)
            .unwrap();
        let anchor = text
            .sticky_index(&doc.transact(), 2, yrs::Assoc::Before)
            .unwrap();

        let payload = encode_presence("ana", Some(&caret), Some(&anchor));
        let Some(PresenceUpdate::Here {
            nickname,
            caret: dec_caret,
            selection_anchor: dec_anchor,
        }) = decode_presence(&payload)
        else {
            panic!("expected a Here presence");
        };
        assert_eq!(nickname, "ana");
        // The decoded anchors resolve to the original logical positions —
        // even after a concurrent edit shifts the text.
        text.insert(&mut doc.transact_mut(), 0, ">> ");
        let txn = doc.transact();
        assert_eq!(dec_caret.unwrap().get_offset(&txn).unwrap().index, 8);
        assert_eq!(dec_anchor.unwrap().get_offset(&txn).unwrap().index, 5);
        assert_eq!(text.get_string(&txn).len(), 14);
        drop(txn);

        // Empty nicknames now decode (the caller substitutes a device label);
        // only genuinely malformed payloads are rejected.
        assert!(matches!(
            decode_presence(&encode_presence("bob", None, None)),
            Some(PresenceUpdate::Here { nickname, caret: None, selection_anchor: None }) if nickname == "bob"
        ));
        assert!(matches!(
            decode_presence(&encode_presence("", None, None)),
            Some(PresenceUpdate::Here { nickname, .. }) if nickname.is_empty()
        ));
        // The leave tombstone round-trips to Gone.
        assert!(matches!(
            decode_presence(&encode_presence_leave()),
            Some(PresenceUpdate::Gone)
        ));
        assert!(decode_presence(b"xx").is_none());
    }
}
