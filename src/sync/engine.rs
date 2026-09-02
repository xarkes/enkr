//! The sync state machine. One instance per device, running on the dedicated
//! sync thread inside a single-threaded tokio runtime.
//!
//! The engine is **doc-less**: it never holds a Yrs document. It is a
//! crypto/transport layer over raw update bytes:
//!
//! Send path:    Cmd::QueueUpdate(bytes) → debounce buffer
//!               → merge buffered deltas (yrs::merge_updates_v1, byte-level)
//!               → encrypt(doc_key[epoch], AAD) + sign → outbox (in-memory)
//!               → PushUpdate → Ack → DocIdle
//!
//! Receive path: Broadcast/Backlog/Snapshot → verify sig (known member?)
//!               → decrypt → emit SyncEvent::DocBytes (the UI replica applies).
//!
//! Snapshots:    the engine knows *when* (server request / tail threshold)
//!               but not *what* — it emits SnapshotNeeded and seals whatever
//!               full state the UI provides back.
//!
//! Nothing is persisted here except the device identity (see `identity.rs`):
//! keys/membership re-fetch from the server on join, sequence state rebuilds
//! from snapshot+backlog, and unacknowledged local content is re-shipped by
//! the UI's `needs_push` flag. Unknown epoch → FetchEnvelopes, retry. Bad
//! sig/AAD → drop + warn.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::time::Duration;

use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use tokio::sync::{broadcast, mpsc, oneshot};
use uuid::Uuid;
// `std::time::Instant` unconditionally panics on wasm32-unknown-unknown;
// `web_time::Instant` is API-identical, real `std::time::Instant` on
// native, `performance.now()`-backed on wasm32 — see Cargo.toml.
use web_time::Instant;

use enkr_proto::PROTOCOL_VERSION;
use enkr_proto::crypto::{self, DeviceIdentity, SpaceKey};
use enkr_proto::membership::{self, MemberRole, MembershipOp, MembershipOpKind, MembershipState};
use enkr_proto::wire::{
    self, ClientMsg, DevicePk, EnvelopeUpload, KexPk, ServerMsg, SnapshotFrame, SubscribeEntry,
    UpdateFrame,
};

use super::transport::{self, Ws, WsError};
use super::{Cmd, MemberEntry, SyncConfig, SyncError, SyncEvent, SyncStatus};

/// Upper bound on the non-ciphertext part of an encoded `PushUpdate` frame:
/// doc id, epoch, author key, nonce, Ed25519 signature, client tag and postcard
/// headers. Used to bound a doc update's ciphertext against `MAX_MESSAGE_BYTES`
/// without re-encoding the whole frame on the debounce-flush hot path.
const UPDATE_FRAME_OVERHEAD: usize = 1024;

/// Cap on frames parked per doc awaiting keys or a membership refresh. Deep
/// enough for an honest backlog burst from a member we haven't learned about
/// yet; bounded so a hostile relay can't use it as a memory amplifier.
const MAX_DEFERRED_FRAMES: usize = 1024;

/// Entries per `Subscribe` message. A `SubscribeEntry` is a uuid plus a varint
/// seq — about 25 bytes — so this is far below `MAX_MESSAGE_BYTES` and doubles
/// as the frame-size guard. Chunking also keeps the server's per-message
/// subscribe loop short enough that other traffic on the connection interleaves
/// instead of queueing behind one huge batch.
const SUBSCRIBE_BATCH: usize = 512;

/// How long the shutdown path waits for the WebSocket closing handshake (our
/// Close frame out, the relay's Close back). Short on purpose: this runs while
/// the user is closing the window, so a relay that never answers must cost a
/// blink, not a hang - the socket dies with the process either way, the
/// handshake only decides whether the relay logs that as an error.
const CLOSE_TIMEOUT: Duration = Duration::from_millis(500);

/// Why a connection attempt failed, and whether trying again could help.
enum HandshakeFailure {
    /// Transient — the engine backs off and retries.
    Retry(String),
    /// The relay speaks a different wire version. Permanent until software
    /// changes, so the engine stops attempting rather than looping.
    Incompatible { server_version: u16 },
    /// The relay refused this identity — a wrong, revoked or missing account
    /// token. Also permanent: retrying with the same credential cannot start
    /// working, and the loop is exactly what made the version mismatch look
    /// like a hang. Cleared by an explicit reconnect (a fresh engine), which
    /// is what entering a new token does.
    Rejected {
        code: wire::ErrorCode,
        context: String,
    },
}

/// A deterministic point in `[0, window]`, mixed from this device's key and a
/// per-attempt salt. Keeping it deterministic avoids pulling a random source
/// into the engine; mixing the device key is what makes two clients pick
/// *different* delays, which is the whole point of jitter.
fn jitter(window: Duration, device_pk: &DevicePk, salt: u64) -> Duration {
    use std::hash::{BuildHasher, Hasher};
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    hasher.write(device_pk);
    hasher.write_u64(salt);
    let micros = window.as_micros() as u64;
    if micros == 0 {
        return window;
    }
    Duration::from_micros(hasher.finish() % micros)
}

/// Local update bytes awaiting the debounce flush.
#[derive(Default)]
struct Pending {
    updates: Vec<Vec<u8>>,
    bytes: usize,
    /// When the *oldest* currently-buffered edit was queued (set only on the
    /// empty→non-empty transition). The flush fires `debounce` after this, so
    /// latency is capped at `debounce` even while the user keeps typing —
    /// continuous typing ships a batch every `debounce` rather than stalling
    /// until a pause.
    first_edit: Option<Instant>,
}

struct DocState {
    space_id: Uuid,
    /// Highest *contiguous* server seq incorporated; safe to resubscribe from.
    have_seq: u64,
    /// Seqs seen ahead of the contiguous frontier (out-of-order fan-out).
    ahead: BTreeSet<u64>,
    /// Local updates queued by the UI, awaiting debounce flush.
    pending: Pending,
    /// covers_seq of the newest snapshot we know of (drives the client-side
    /// compaction trigger).
    snapshot_covers: u64,
    /// Outstanding server `RequestSnapshot` we couldn't satisfy yet.
    requested_snapshot: Option<u64>,
    /// covers_seq of the `SnapshotNeeded` we already asked the UI for —
    /// avoids re-emitting every seq advance while the answer is in flight.
    snapshot_asked: Option<u64>,
    /// Frames we couldn't process yet — an unknown epoch (retried once the
    /// envelopes arrive) or an author missing from our copy of the membership
    /// log (retried once the log is refreshed). Bounded by `MAX_DEFERRED_FRAMES`
    /// so a hostile relay can't grow it without limit.
    deferred: Vec<UpdateFrame>,
    live: bool,
    /// Local updates queued or unacked (drives DocBusy/DocIdle events).
    busy: bool,
}

struct OutboxItem {
    doc_id: Uuid,
    frame: UpdateFrame,
}

struct SpaceState {
    keys: BTreeMap<u32, SpaceKey>,
    current_epoch: u32,
    membership: MembershipState,
}

impl SpaceState {
    fn latest_key(&self) -> Option<(u32, &SpaceKey)> {
        self.keys.iter().next_back().map(|(e, k)| (*e, k))
    }
}

#[derive(Default)]
struct JoinProgress {
    have_membership: bool,
    have_envelopes: bool,
    waiters: Vec<oneshot::Sender<Result<(), SyncError>>>,
}

pub(super) struct Engine {
    config: SyncConfig,
    identity: DeviceIdentity,
    events: broadcast::Sender<SyncEvent>,
    docs: HashMap<Uuid, DocState>,
    spaces: HashMap<Uuid, SpaceState>,
    /// Frames pushed but not yet acked, by client tag. In-memory by design:
    /// loss across restarts is recovered by the UI's `needs_push` reship.
    outbox: BTreeMap<u64, OutboxItem>,
    sink: Option<SplitSink<Ws, Vec<u8>>>,
    stream: Option<SplitStream<Ws>>,
    next_reconnect: Instant,
    backoff: Duration,
    /// When the relay last said anything. A stalled link produces no error, so
    /// silence is the only signal there is; `on_tick` turns it into one.
    last_inbound: Instant,
    /// When the outstanding `Ping` was sent, if any.
    ///
    /// Liveness is judged from an *unanswered probe*, not from raw silence.
    /// Silence alone also describes a client too busy to read — applying a large
    /// backlog blocks this loop, and a plain silence timer then kills a perfectly
    /// healthy connection at exactly the moment it is doing the most work.
    /// Starting the clock when we actually ask makes the test "we asked and got
    /// nothing back", which a busy client cannot trip.
    ping_sent_at: Option<Instant>,
    /// Set once the relay has rejected us over a wire-version mismatch, which
    /// stops all further connection attempts: retrying cannot succeed until one
    /// side is upgraded, and doing it silently is what left the UI stuck on
    /// "Connecting…".
    incompatible: Option<(u16, u16)>,
    /// Set once the relay has refused this device's credentials. Stops further
    /// attempts for the same reason `incompatible` does: nothing about a
    /// rejected token changes by asking again.
    rejected: bool,
    /// Spaces the relay refused to create for want of an account.
    ///
    /// Kept because a refusal arrives mid-flight: `CreateSpace`, `CreateDoc`
    /// and `Subscribe` are all sent before the first reply lands, so the one
    /// useful error is followed by an `UnknownSpace` and an `UnknownDoc` for
    /// work that was already doomed. Those clobbered the real explanation in
    /// the UI with "unknown space", which tells the user nothing about what to
    /// do. Swallowed here instead.
    rejected_spaces: HashSet<Uuid>,
    /// When to re-drive subscriptions the relay failed to serve.
    ///
    /// A storage error is now reported instead of dropping the connection,
    /// which is right — but the old behaviour had a side effect worth keeping:
    /// the disconnect re-subscribed everything on reconnect. Without a retry
    /// here a doc whose backlog read failed stays registered, never live, and
    /// never catches up, and the connection staying healthy means nothing else
    /// will notice.
    resubscribe_at: Option<Instant>,
    next_tag: u64,
    joins: HashMap<Uuid, JoinProgress>,
    /// Spaces with a queued-but-unsent FetchEnvelopes (filled from sync code
    /// paths that can't await; drained on the next async opportunity).
    envelope_queue: HashSet<Uuid>,
    /// Spaces with a FetchEnvelopes in flight — don't re-request until the
    /// Envelopes response (or an EpochBump) clears the flag.
    envelopes_inflight: HashSet<Uuid>,
    /// Spaces with a queued-but-unsent FetchMembership, and one in flight.
    /// Adding a member does not bump the epoch, so an already-connected member
    /// is never told about a later joiner; a frame from a device we don't know
    /// yet is the signal to refresh the log. Same queue/inflight shape as the
    /// envelope fetch above.
    membership_queue: HashSet<Uuid>,
    membership_inflight: HashSet<Uuid>,
    /// Callers awaiting a `SpaceList` answer (responses carry no correlation
    /// id; any SpaceList resolves all of them — PoC).
    pending_space_lists: Vec<oneshot::Sender<Result<Vec<Uuid>, SyncError>>>,
    /// Blob uploads awaiting `BlobStored`, keyed by blob_id (the correlation id).
    pending_blob_puts: HashMap<Uuid, oneshot::Sender<Result<(), SyncError>>>,
    /// Blob fetches awaiting `BlobData`, keyed by blob_id. Multiple callers may
    /// await the same id; each carries the space to decrypt under.
    #[allow(clippy::type_complexity)]
    #[allow(clippy::type_complexity)]
    pending_blob_gets: HashMap<
        Uuid,
        Vec<(
            Uuid,
            crypto::BlobKey,
            oneshot::Sender<Result<Option<Vec<u8>>, SyncError>>,
        )>,
    >,
}

enum Wake {
    Cmd(Option<Cmd>),
    Ws(Option<Result<Vec<u8>, WsError>>),
    Tick,
}

impl Engine {
    pub(super) fn new(
        config: SyncConfig,
        identity: DeviceIdentity,
        events: broadcast::Sender<SyncEvent>,
    ) -> Self {
        let backoff = config.reconnect_min;
        Self {
            config,
            identity,
            events,
            docs: HashMap::new(),
            spaces: HashMap::new(),
            outbox: BTreeMap::new(),
            sink: None,
            stream: None,
            next_reconnect: Instant::now(),
            backoff,
            last_inbound: Instant::now(),
            ping_sent_at: None,
            incompatible: None,
            rejected: false,
            rejected_spaces: HashSet::new(),
            resubscribe_at: None,
            // The server dedups (device, client_tag) across connections for
            // outbox-retry idempotency. Tags are no longer persisted, so each
            // engine session must claim a fresh tag range — a wall-clock
            // start guarantees no overlap with previous sessions' tags.
            next_tag: web_time::SystemTime::now()
                .duration_since(web_time::UNIX_EPOCH)
                .map(|d| d.as_nanos() as u64)
                .unwrap_or(1)
                .max(1),
            joins: HashMap::new(),
            envelope_queue: HashSet::new(),
            envelopes_inflight: HashSet::new(),
            membership_queue: HashSet::new(),
            membership_inflight: HashSet::new(),
            pending_space_lists: Vec::new(),
            pending_blob_puts: HashMap::new(),
            pending_blob_gets: HashMap::new(),
        }
    }

    pub(super) async fn run(mut self, mut cmd_rx: mpsc::UnboundedReceiver<Cmd>) {
        let mut tick = super::clock::Interval::new(Duration::from_millis(25));

        loop {
            let wake = tokio::select! {
                cmd = cmd_rx.recv() => Wake::Cmd(cmd),
                msg = recv_ws(&mut self.stream) => Wake::Ws(msg),
                _ = tick.tick() => Wake::Tick,
            };
            match wake {
                Wake::Cmd(None) | Wake::Cmd(Some(Cmd::Shutdown)) => break,
                Wake::Cmd(Some(cmd)) => self.handle_cmd(cmd).await,
                Wake::Ws(Some(Ok(msg))) => self.handle_ws(msg).await,
                Wake::Ws(Some(Err(_))) | Wake::Ws(None) => self.disconnect(),
                Wake::Tick => self.on_tick().await,
            }
        }

        // Drain: best-effort push of whatever is still buffered. Anything
        // that doesn't make it is covered by the notes DB + needs_push.
        let doc_ids: Vec<Uuid> = self.docs.keys().copied().collect();
        for doc_id in doc_ids {
            self.flush_doc(doc_id).await;
        }
        self.close_ws().await;
    }

    /// End the connection with a WebSocket closing handshake instead of just
    /// dropping the socket. Both sides read a bare drop as a fault - the relay
    /// logs it as `connection closed with error: WebSocket protocol error:
    /// Connection reset without closing handshake` and counts it a failed
    /// connection - when a user quitting the app is the most ordinary ending
    /// there is. A Close frame is what says "this was deliberate".
    ///
    /// Best-effort and bounded by [`CLOSE_TIMEOUT`]: nothing here is worth
    /// delaying app exit for, and the frames already pushed are covered by the
    /// notes DB + `needs_push` regardless.
    async fn close_ws(&mut self) {
        let (Some(mut sink), Some(mut stream)) = (self.sink.take(), self.stream.take()) else {
            return;
        };
        let handshake = async {
            if let Err(err) = sink.close().await {
                log::debug!("sync: close frame not sent: {err}");
                return;
            }
            // The handshake isn't finished until the relay's own Close comes
            // back (or the stream ends): returning at `sink.close()` and
            // letting the process exit can still cut the connection before the
            // relay has read our Close, which is the very error we're avoiding.
            // Anything else arriving in the meantime is deliberately dropped -
            // we are leaving, and the UI is already gone.
            while let Some(Ok(_)) = stream.next().await {}
        };
        if super::clock::timeout(CLOSE_TIMEOUT, handshake).await.is_err() {
            log::debug!("sync: server did not complete the close handshake in time");
        }
    }

    fn emit(&self, event: SyncEvent) {
        let _ = self.events.send(event);
    }

    fn warn_security(&self, context: impl Into<String>) {
        let context = context.into();
        log::warn!("security: {context}");
        self.emit(SyncEvent::SecurityWarning { context });
    }

    // -- connection management -------------------------------------------------

    fn disconnect(&mut self) {
        if self.sink.is_some() || self.stream.is_some() {
            self.emit(SyncEvent::Disconnected);
        }
        self.sink = None;
        self.stream = None;
        self.schedule_reconnect();
        for doc in self.docs.values_mut() {
            doc.live = false;
        }
        for waiter in self.pending_space_lists.drain(..) {
            let _ = waiter.send(Err(SyncError::Disconnected));
        }
        for (_, reply) in self.pending_blob_puts.drain() {
            let _ = reply.send(Err(SyncError::Disconnected));
        }
        for (_, waiters) in self.pending_blob_gets.drain() {
            for (_, _, reply) in waiters {
                let _ = reply.send(Err(SyncError::Disconnected));
            }
        }
    }

    fn connected(&self) -> bool {
        self.sink.is_some()
    }

    async fn try_connect(&mut self) {
        match self.handshake().await {
            Ok(ws) => {
                let (sink, stream) = ws.split();
                self.sink = Some(sink);
                self.stream = Some(stream);
                self.backoff = self.config.reconnect_min;
                self.last_inbound = Instant::now();
                self.ping_sent_at = None;
                self.emit(SyncEvent::Connected);
                self.on_connected().await;
            }
            Err(HandshakeFailure::Retry(err)) => {
                log::warn!("connect to {} failed: {err}", self.config.server_url);
                self.emit(SyncEvent::ConnectError { context: err });
                self.schedule_reconnect();
            }
            Err(HandshakeFailure::Rejected { code, context }) => {
                log::error!(
                    "{} refused this device ({code:?}): {context}",
                    self.config.server_url
                );
                self.rejected = true;
                self.emit(SyncEvent::Rejected { context });
            }
            Err(HandshakeFailure::Incompatible { server_version }) => {
                // Deliberately no `schedule_reconnect`: retrying cannot work
                // until one side is upgraded, and the silent retry loop is
                // exactly what left the UI on "Connecting…" forever.
                log::error!(
                    "{} speaks protocol v{server_version}, this client speaks v{PROTOCOL_VERSION}",
                    self.config.server_url
                );
                self.incompatible = Some((server_version, PROTOCOL_VERSION));
                self.emit(SyncEvent::Incompatible {
                    server_version,
                    client_version: PROTOCOL_VERSION,
                });
            }
        }
    }

    async fn handshake(&mut self) -> Result<Ws, HandshakeFailure> {
        let mut ws = transport::connect(&self.config.server_url)
            .await
            .map_err(HandshakeFailure::Retry)?;

        send_ws(
            &mut ws,
            &ClientMsg::Hello {
                device_pk: self.identity.device_pk(),
                kex_pk: self.identity.kex_pk(),
                protocol_version: PROTOCOL_VERSION,
            },
        )
        .await
        .map_err(HandshakeFailure::Retry)?;
        // The relay answers a version mismatch here, in place of the Challenge.
        // Reading it as just "expected Challenge" is what made an incompatible
        // relay look like a flaky one and retry forever.
        match read_ws(&mut ws).await.map_err(HandshakeFailure::Retry)? {
            ServerMsg::Challenge { nonce, server_id } => {
                self.finish_handshake(ws, nonce, server_id).await
            }
            ServerMsg::Error {
                code: wire::ErrorCode::BadProtocolVersion,
                context,
            } => Err(HandshakeFailure::Incompatible {
                server_version: context.parse().unwrap_or(0),
            }),
            other => Err(HandshakeFailure::Retry(format!(
                "expected Challenge, got {other:?}"
            ))),
        }
    }

    async fn finish_handshake(
        &mut self,
        mut ws: Ws,
        nonce: [u8; 32],
        server_id: [u8; 16],
    ) -> Result<Ws, HandshakeFailure> {
        let sig = self
            .identity
            .sign(&crypto::auth_signing_bytes(&nonce, &server_id));
        send_ws(
            &mut ws,
            &ClientMsg::Auth {
                sig: sig.to_vec(),
                account_token: self.config.account_token.clone(),
            },
        )
        .await
        .map_err(HandshakeFailure::Retry)?;
        match read_ws(&mut ws).await.map_err(HandshakeFailure::Retry)? {
            ServerMsg::AuthOk { account, .. } => {
                // Report it even when None: "this relay gave us no account" is
                // exactly what settings needs to distinguish a free relay from
                // one where the token has not been entered yet.
                self.emit(SyncEvent::Account { info: account });
                Ok(ws)
            }
            // An auth refusal is about *this* credential, so retrying with it
            // is pointless — the relay will keep saying no until the user
            // changes something.
            ServerMsg::Error { code, context } => Err(HandshakeFailure::Rejected { code, context }),
            _ => Err(HandshakeFailure::Retry("expected AuthOk".into())),
        }
    }

    /// Reconnect = re-Subscribe with current have_seq + outbox retry. No
    /// server-side session state to restore.
    async fn on_connected(&mut self) {
        let entries: Vec<SubscribeEntry> = self
            .docs
            .iter()
            .map(|(doc_id, doc)| SubscribeEntry {
                doc_id: *doc_id,
                have_seq: doc.have_seq,
            })
            .collect();
        self.send_subscribe(entries).await;
        let resend: Vec<(u64, Uuid, UpdateFrame)> = self
            .outbox
            .iter()
            .map(|(tag, item)| (*tag, item.doc_id, item.frame.clone()))
            .collect();
        for (tag, doc_id, frame) in resend {
            self.send(ClientMsg::PushUpdate {
                doc_id,
                client_tag: tag,
                frame,
            })
            .await;
        }
    }

    async fn send(&mut self, msg: ClientMsg) -> bool {
        let Some(sink) = self.sink.as_mut() else {
            return false;
        };
        let Ok(bytes) = wire::encode(&msg) else {
            return false;
        };
        // Defense in depth: never transmit a frame the server would reject as
        // too large. Dropping it keeps the connection alive (the alternative -
        // a server-side close - would loop on every reconnect); callers that
        // can produce oversized frames (doc updates, blobs) pre-check and
        // surface a precise error before reaching here.
        if bytes.len() > wire::MAX_MESSAGE_BYTES {
            log::error!(
                "refusing to send oversized frame ({} bytes > {} limit)",
                bytes.len(),
                wire::MAX_MESSAGE_BYTES,
            );
            return false;
        }
        if sink.send(bytes).await.is_err() {
            self.disconnect();
            return false;
        }
        #[cfg(debug_assertions)]
        debug_log_client_msg("sent", &msg);
        true
    }

    /// Re-subscribe any doc still waiting to go live, after the relay reported
    /// it could not serve a request.
    async fn retry_failed_subscriptions(&mut self) {
        if !self.connected() || self.resubscribe_at.is_none_or(|at| Instant::now() < at) {
            return;
        }
        self.resubscribe_at = None;
        let entries: Vec<SubscribeEntry> = self
            .docs
            .iter()
            .filter(|(_, doc)| !doc.live)
            .map(|(doc_id, doc)| SubscribeEntry {
                doc_id: *doc_id,
                have_seq: doc.have_seq,
            })
            .collect();
        if !entries.is_empty() {
            log::debug!(
                "retrying {} subscription(s) after a relay error",
                entries.len()
            );
            self.send_subscribe(entries).await;
        }
    }

    /// Turn relay silence into a disconnect.
    ///
    /// Nothing errors when a link black-holes: the socket stays open and writes
    /// are accepted, so without this the client keeps believing it is connected
    /// and silently stops syncing. After `heartbeat` of quiet we probe, and
    /// after `liveness_timeout` we give up and let the reconnect path run.
    async fn check_liveness(&mut self) {
        if !self.connected() {
            return;
        }
        let now = Instant::now();
        let quiet = now.duration_since(self.last_inbound);
        // Any inbound message clears `ping_sent_at`, so an outstanding probe
        // means genuine one-way silence, not a slow local apply.
        if self
            .ping_sent_at
            .is_some_and(|at| now.duration_since(at) >= self.config.liveness_timeout)
        {
            log::warn!(
                "relay did not answer a ping within {:?}; reconnecting",
                self.config.liveness_timeout
            );
            self.disconnect();
            self.schedule_reconnect();
            return;
        }
        if quiet >= self.config.heartbeat && self.ping_sent_at.is_none() {
            self.ping_sent_at = Some(now);
            self.send(ClientMsg::Ping).await;
        }
    }

    /// Back off with **full jitter**: a random point in `[0, backoff]` rather
    /// than the backoff itself. Every client dropped by one relay restart
    /// otherwise retries in lockstep, so the load spike lands exactly when the
    /// relay is least able to absorb it.
    fn schedule_reconnect(&mut self) {
        let jittered = jitter(self.backoff, &self.identity.device_pk(), self.next_tag);
        self.next_reconnect = Instant::now() + jittered;
        self.backoff = (self.backoff * 2).min(self.config.reconnect_max);
    }

    // -- ticks: reconnect, debounce flush ---------------------------------------

    async fn on_tick(&mut self) {
        if !self.connected()
            && self.incompatible.is_none()
            && !self.rejected
            && Instant::now() >= self.next_reconnect
        {
            self.try_connect().await;
        }
        self.check_liveness().await;
        self.retry_failed_subscriptions().await;
        let now = Instant::now();
        let due: Vec<Uuid> = self
            .docs
            .iter()
            .filter(|(_, doc)| {
                !doc.pending.updates.is_empty()
                    && (doc.pending.bytes >= self.config.debounce_max_bytes
                        || doc
                            .pending
                            .first_edit
                            .is_some_and(|at| now.duration_since(at) >= self.config.debounce))
            })
            .map(|(id, _)| *id)
            .collect();
        for doc_id in due {
            self.flush_doc(doc_id).await;
        }
    }

    /// Debounce flush: merge buffered update bytes into one frame,
    /// encrypt + sign, remember in the outbox, push.
    async fn flush_doc(&mut self, doc_id: Uuid) {
        // Look the keys up *before* draining the buffer: when they aren't
        // available yet (join/epoch fetch in flight) the edits must stay
        // queued for the next tick, never be dropped.
        let Some(space_id) = self.docs.get(&doc_id).map(|doc| doc.space_id) else {
            return;
        };
        let Some(space) = self.spaces.get(&space_id) else {
            return;
        };
        if !space.membership.can_write(&self.identity.device_pk()) {
            if let Some(doc) = self.docs.get_mut(&doc_id) {
                doc.pending.updates.clear();
                doc.pending.bytes = 0;
                doc.pending.first_edit = None;
            }
            return;
        }
        let Some((epoch, key)) = space.latest_key() else {
            return;
        };
        let (epoch, key) = (epoch, key.clone());
        let doc = self.docs.get_mut(&doc_id).unwrap();
        let updates = std::mem::take(&mut doc.pending.updates);
        doc.pending.bytes = 0;
        doc.pending.first_edit = None;
        if updates.is_empty() {
            return;
        }
        let merged = match yrs::merge_updates_v1(&updates) {
            Ok(merged) => merged,
            Err(err) => {
                // Corrupt buffered update: re-queueing would loop forever.
                // Surface it loudly instead of silently diverging.
                self.warn_security(format!(
                    "doc {doc_id}: merge of local updates failed: {err}"
                ));
                return;
            }
        };
        let frame = crypto::seal_update(&self.identity, &key, &space_id, &doc_id, epoch, &merged);
        // Refuse a frame the server would reject as "too large": the ciphertext
        // dominates the encoded PushUpdate (ids, nonce, signature and postcard
        // headers are small and bounded), so guard on it with headroom. Sending
        // it anyway would tear the connection down, and because it lives in the
        // outbox it would be re-sent on every reconnect - wedging sync forever.
        // Drop it instead (it stays applied locally) and surface the failure.
        if frame.ciphertext.len() + UPDATE_FRAME_OVERHEAD > wire::MAX_MESSAGE_BYTES {
            self.emit(SyncEvent::UpdateTooLarge {
                doc_id,
                bytes: frame.ciphertext.len(),
            });
            return;
        }
        let tag = self.next_tag;
        self.next_tag += 1;
        self.outbox.insert(
            tag,
            OutboxItem {
                doc_id,
                frame: frame.clone(),
            },
        );
        self.send(ClientMsg::PushUpdate {
            doc_id,
            client_tag: tag,
            frame,
        })
        .await;
    }

    // -- busy/idle tracking --------------------------------------------------------

    fn doc_has_unacked(&self, doc_id: &Uuid) -> bool {
        self.outbox.values().any(|item| item.doc_id == *doc_id)
    }

    fn mark_busy(&mut self, doc_id: Uuid) {
        if let Some(doc) = self.docs.get_mut(&doc_id)
            && !doc.busy
        {
            doc.busy = true;
            self.emit(SyncEvent::DocBusy { doc_id });
        }
    }

    /// Emit DocIdle when everything queued for this doc has been pushed and
    /// acknowledged (the UI clears `needs_push` on it).
    fn check_idle(&mut self, doc_id: Uuid) {
        let unacked = self.doc_has_unacked(&doc_id);
        if let Some(doc) = self.docs.get_mut(&doc_id)
            && doc.busy
            && doc.pending.updates.is_empty()
            && !unacked
        {
            doc.busy = false;
            self.emit(SyncEvent::DocIdle { doc_id });
        }
    }

    // -- doc registry ------------------------------------------------------------

    fn register_doc(&mut self, doc_id: Uuid, space_id: Uuid) {
        self.docs.entry(doc_id).or_insert(DocState {
            space_id,
            have_seq: 0,
            ahead: BTreeSet::new(),
            pending: Pending::default(),
            snapshot_covers: 0,
            requested_snapshot: None,
            snapshot_asked: None,
            deferred: Vec::new(),
            live: false,
            busy: false,
        });
    }

    // -- commands ------------------------------------------------------------------

    async fn handle_cmd(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::Shutdown => unreachable!("handled by run()"),
            Cmd::CreateSpace { reply } => {
                let _ = reply.send(self.create_space().await);
            }
            Cmd::JoinSpace { space_id, reply } => {
                // A space counts as joined only once *both* the key envelopes and
                // the membership log have loaded. Envelopes can arrive on their
                // own (e.g. when the membership fetch races ahead of the inviter's
                // Add op committing and comes back NotMember), which leaves the
                // space with keys but an empty membership; treating that as joined
                // would make every historical frame look like it came from an
                // unknown author and get dropped.
                if self
                    .spaces
                    .get(&space_id)
                    .is_some_and(|s| !s.keys.is_empty() && !s.membership.members.is_empty())
                {
                    let _ = reply.send(Ok(()));
                    return;
                }
                if !self.connected() {
                    let _ = reply.send(Err(SyncError::Disconnected));
                    return;
                }
                self.joins.entry(space_id).or_default().waiters.push(reply);
                self.send(ClientMsg::FetchMembership { space_id }).await;
                self.send(ClientMsg::FetchEnvelopes { space_id }).await;
            }
            Cmd::CreateDoc {
                space_id,
                doc_id,
                reply,
            } => {
                let _ = reply.send(self.create_doc(space_id, doc_id).await.map(|_| doc_id));
            }
            Cmd::OpenDoc {
                space_id,
                doc_id,
                reply,
            } => {
                let _ = reply.send(self.open_doc(space_id, doc_id).await);
            }
            Cmd::OpenDocs {
                space_id,
                doc_ids,
                reply,
            } => {
                let _ = reply.send(self.open_docs(space_id, doc_ids).await);
            }
            Cmd::AddMember {
                space_id,
                device_pk,
                kex_pk,
                role,
                reply,
            } => {
                let _ = reply.send(self.add_member(space_id, device_pk, kex_pk, role).await);
            }
            Cmd::RemoveMember {
                space_id,
                device_pk,
                reply,
            } => {
                let _ = reply.send(self.remove_member(space_id, device_pk).await);
            }
            Cmd::SetMemberRole {
                space_id,
                device_pk,
                role,
                reply,
            } => {
                let _ = reply.send(self.set_member_role(space_id, device_pk, role).await);
            }
            Cmd::ListMembers { space_id, reply } => {
                let _ = reply.send(self.list_members(space_id));
            }
            Cmd::QueueUpdate { doc_id, update } => {
                // Unknown docs are dropped: the UI's needs_push flag reships
                // the full note state once the doc is opened/registered.
                if let Some(doc) = self.docs.get_mut(&doc_id) {
                    doc.pending.bytes += update.len();
                    doc.pending.updates.push(update);
                    // Stamp only the first edit of a batch: the flush timer runs
                    // from the oldest pending edit, capping latency at `debounce`
                    // instead of resetting on every keystroke.
                    doc.pending.first_edit.get_or_insert_with(Instant::now);
                    self.mark_busy(doc_id);
                }
            }
            Cmd::ProvideSnapshot {
                doc_id,
                covers_seq,
                state,
            } => {
                self.seal_and_put_snapshot(doc_id, covers_seq, &state).await;
            }
            Cmd::SendEphemeral { doc_id, payload } => {
                self.send_ephemeral(doc_id, &payload).await;
            }
            Cmd::PutBlob {
                space_id,
                blob_id,
                blob_key,
                plaintext,
                reply,
            } => {
                self.put_blob(space_id, blob_id, blob_key, plaintext, reply)
                    .await;
            }
            Cmd::GetBlob {
                space_id,
                blob_id,
                blob_key,
                reply,
            } => {
                if self.send(ClientMsg::GetBlob { space_id, blob_id }).await {
                    self.pending_blob_gets
                        .entry(blob_id)
                        .or_default()
                        .push((space_id, blob_key, reply));
                } else {
                    let _ = reply.send(Err(SyncError::Disconnected));
                }
            }
            Cmd::DeleteBlob { space_id, blob_id } => {
                // Best-effort storage reclaim; if we're offline the send drops
                // and the blob stays orphaned (but already unadvertised).
                self.send(ClientMsg::DeleteBlob { space_id, blob_id }).await;
            }
            Cmd::Resync => {
                let entries: Vec<SubscribeEntry> = self
                    .docs
                    .iter_mut()
                    .map(|(doc_id, doc)| {
                        doc.have_seq = 0;
                        doc.ahead.clear();
                        SubscribeEntry {
                            doc_id: *doc_id,
                            have_seq: 0,
                        }
                    })
                    .collect();
                if !entries.is_empty() && self.connected() {
                    self.send(ClientMsg::Subscribe { entries }).await;
                }
            }
            Cmd::ForgetSpace { space_id } => {
                // Local-only teardown: the device stays a member server-side.
                // Dropping the keys + doc subscriptions makes a later JoinSpace
                // re-fetch envelopes/membership and re-subscribe every doc from
                // seq 0, so content is pulled fresh into the new local mirror.
                self.forget_space_state(space_id);
            }
            Cmd::ListSpaces { reply } => {
                if !self.connected() {
                    let _ = reply.send(Err(SyncError::Disconnected));
                    return;
                }
                self.pending_space_lists.push(reply);
                self.send(ClientMsg::ListSpaces).await;
            }
            Cmd::DeleteSpace { space_id } => {
                // Fire-and-forget: the server validates ownership and replies
                // with a SpaceDeleted broadcast that drives the local teardown
                // (see the ServerMsg handler), for us and every other member.
                if self.connected() {
                    self.send(ClientMsg::DeleteSpace { space_id }).await;
                }
            }
            Cmd::Flush { reply } => {
                let doc_ids: Vec<Uuid> = self.docs.keys().copied().collect();
                for doc_id in doc_ids {
                    self.flush_doc(doc_id).await;
                }
                let _ = reply.send(());
            }
            Cmd::Status { reply } => {
                let pending_docs = self
                    .docs
                    .values()
                    .filter(|d| !d.pending.updates.is_empty())
                    .count();
                let _ = reply.send(SyncStatus {
                    connected: self.connected(),
                    incompatible: self.incompatible,
                    rejected: self.rejected,
                    outbox_len: self.outbox.len(),
                    pending_docs,
                });
            }
        }
    }

    async fn create_space(&mut self) -> Result<Uuid, SyncError> {
        if !self.connected() {
            return Err(SyncError::Disconnected);
        }
        let space_id = Uuid::new_v4();
        let key = SpaceKey::generate();
        let op = MembershipOp {
            space_id,
            op_seq: 0,
            kind: MembershipOpKind::Create {
                creator_kex: self.identity.kex_pk(),
            },
        };
        let signed = membership::sign_op(&self.identity, &op)
            .map_err(|e| SyncError::Other(e.to_string()))?;
        let envelope = EnvelopeUpload {
            device_pk: self.identity.device_pk(),
            epoch: 0,
            sealed_key: crypto::seal_space_key(&self.identity.kex_pk(), &key),
        };

        let mut membership_state = MembershipState::default();
        membership_state
            .apply(&space_id, &signed)
            .map_err(|e| SyncError::Other(e.to_string()))?;
        self.spaces.insert(
            space_id,
            SpaceState {
                keys: BTreeMap::from([(0, key)]),
                current_epoch: 0,
                membership: membership_state,
            },
        );

        if !self
            .send(ClientMsg::CreateSpace {
                space_id,
                signed_op: signed,
                envelopes: vec![envelope],
            })
            .await
        {
            return Err(SyncError::Disconnected);
        }
        // Index doc: fixed derivable id, encrypted like any doc (§6).
        let index_id = crypto::index_doc_id(&space_id);
        self.create_doc(space_id, index_id).await?;
        Ok(space_id)
    }

    async fn create_doc(&mut self, space_id: Uuid, doc_id: Uuid) -> Result<(), SyncError> {
        let Some(space) = self.spaces.get(&space_id) else {
            return Err(SyncError::UnknownSpace);
        };
        if !space.membership.can_write(&self.identity.device_pk()) {
            return Err(SyncError::Other("permission denied".into()));
        }
        if !self.connected() {
            return Err(SyncError::Disconnected);
        }
        if !self.send(ClientMsg::CreateDoc { space_id, doc_id }).await {
            return Err(SyncError::Disconnected);
        }
        self.register_doc(doc_id, space_id);
        self.send(ClientMsg::Subscribe {
            entries: vec![SubscribeEntry {
                doc_id,
                have_seq: 0,
            }],
        })
        .await;
        Ok(())
    }

    async fn open_doc(&mut self, space_id: Uuid, doc_id: Uuid) -> Result<(), SyncError> {
        self.open_docs(space_id, vec![doc_id]).await
    }

    /// Subscribe to many docs at once. Joining a space with N notes used to cost
    /// N single-entry `Subscribe` frames — N encodes on the wire and N separate
    /// `handle_subscribe` passes on the server, each with its own membership
    /// check and backlog loop. Already-open docs are skipped, so this is
    /// idempotent and safe to re-drive.
    async fn open_docs(&mut self, space_id: Uuid, doc_ids: Vec<Uuid>) -> Result<(), SyncError> {
        if !self.spaces.contains_key(&space_id) {
            return Err(SyncError::UnknownSpace);
        }
        let mut entries = Vec::with_capacity(doc_ids.len());
        for doc_id in doc_ids {
            if self.docs.contains_key(&doc_id) {
                continue;
            }
            self.register_doc(doc_id, space_id);
            entries.push(SubscribeEntry {
                doc_id,
                have_seq: 0,
            });
        }
        if self.connected() {
            self.send_subscribe(entries).await;
        }
        Ok(())
    }

    /// Ship `entries` as `Subscribe` messages of at most [`SUBSCRIBE_BATCH`].
    async fn send_subscribe(&mut self, entries: Vec<SubscribeEntry>) {
        for chunk in entries.chunks(SUBSCRIBE_BATCH) {
            self.send(ClientMsg::Subscribe {
                entries: chunk.to_vec(),
            })
            .await;
        }
    }

    async fn add_member(
        &mut self,
        space_id: Uuid,
        device_pk: DevicePk,
        kex_pk: KexPk,
        role: MemberRole,
    ) -> Result<(), SyncError> {
        if !self.connected() {
            return Err(SyncError::Disconnected);
        }
        // Re-adding ourselves (membership is keyed on `device_pk`) would
        // overwrite our own role and could leave us stuck without admin rights.
        if device_pk == self.identity.device_pk() {
            return Err(SyncError::Other("cannot invite this device itself".into()));
        }
        let Some(space) = self.spaces.get_mut(&space_id) else {
            return Err(SyncError::UnknownSpace);
        };
        // History stays readable: seal *every* epoch key to the invitee.
        let envelopes: Vec<EnvelopeUpload> = space
            .keys
            .iter()
            .map(|(epoch, key)| EnvelopeUpload {
                device_pk,
                epoch: *epoch,
                sealed_key: crypto::seal_space_key(&kex_pk, key),
            })
            .collect();
        let op = MembershipOp {
            space_id,
            op_seq: space.membership.next_op_seq,
            kind: MembershipOpKind::Add {
                device_pk,
                kex_pk,
                role,
            },
        };
        let signed = membership::sign_op(&self.identity, &op)
            .map_err(|e| SyncError::Other(e.to_string()))?;
        space
            .membership
            .apply(&space_id, &signed)
            .map_err(|e| SyncError::Other(e.to_string()))?;
        if !self
            .send(ClientMsg::AddMember {
                space_id,
                signed_op: signed,
                envelopes,
            })
            .await
        {
            return Err(SyncError::Disconnected);
        }
        Ok(())
    }

    async fn remove_member(
        &mut self,
        space_id: Uuid,
        device_pk: DevicePk,
    ) -> Result<(), SyncError> {
        if !self.connected() {
            return Err(SyncError::Disconnected);
        }
        let Some(space) = self.spaces.get_mut(&space_id) else {
            return Err(SyncError::UnknownSpace);
        };
        // Bump from the *signed log's* epoch, not `space.current_epoch`.
        // The latter is max-merged from the server's unauthenticated
        // `current_epoch` hints, while `MembershipState::apply` validates a
        // Remove against the log's own epoch (`membership.rs:167`). The two
        // drift — a refetch that lands before the server has persisted our
        // latest op replaces `space.membership` with a staler replay while the
        // max-merged hint keeps our higher value — and once they do, every
        // later removal in that space is rejected with "removal must bump
        // epoch by one".
        let new_epoch = space.membership.current_epoch + 1;
        let new_key = SpaceKey::generate();
        let op = MembershipOp {
            space_id,
            op_seq: space.membership.next_op_seq,
            kind: MembershipOpKind::Remove {
                device_pk,
                new_epoch,
            },
        };
        let signed = membership::sign_op(&self.identity, &op)
            .map_err(|e| SyncError::Other(e.to_string()))?;
        space
            .membership
            .apply(&space_id, &signed)
            .map_err(|e| SyncError::Other(e.to_string()))?;
        // Wrap the rotated key to every remaining member (including us).
        let envelopes: Vec<EnvelopeUpload> = space
            .membership
            .active_members()
            .map(|(member_pk, info)| EnvelopeUpload {
                device_pk: *member_pk,
                epoch: new_epoch,
                sealed_key: crypto::seal_space_key(&info.kex_pk, &new_key),
            })
            .collect();
        space.keys.insert(new_epoch, new_key);
        space.current_epoch = new_epoch;
        if !self
            .send(ClientMsg::RemoveMember {
                space_id,
                signed_op: signed,
                new_epoch,
                envelopes,
            })
            .await
        {
            return Err(SyncError::Disconnected);
        }
        Ok(())
    }

    /// Change an existing member's role by re-issuing an `Add` op. The device
    /// already holds every epoch key, so this only re-records its role in the
    /// signed log (admin-gated by `MembershipState::apply`).
    async fn set_member_role(
        &mut self,
        space_id: Uuid,
        device_pk: DevicePk,
        role: MemberRole,
    ) -> Result<(), SyncError> {
        let kex_pk = {
            let space = self.spaces.get(&space_id).ok_or(SyncError::UnknownSpace)?;
            space
                .membership
                .members
                .get(&device_pk)
                .filter(|m| !m.removed)
                .map(|m| m.kex_pk)
                .ok_or_else(|| SyncError::Other("device is not an active member".into()))?
        };
        self.add_member(space_id, device_pk, kex_pk, role).await
    }

    /// Active members of a space, read from the locally-verified membership log.
    fn list_members(&self, space_id: Uuid) -> Result<Vec<MemberEntry>, SyncError> {
        let space = self.spaces.get(&space_id).ok_or(SyncError::UnknownSpace)?;
        Ok(space
            .membership
            .active_members()
            .map(|(device_pk, info)| MemberEntry {
                device_pk: *device_pk,
                kex_pk: info.kex_pk,
                role: info.role,
            })
            .collect())
    }

    // -- server messages -------------------------------------------------------------

    async fn handle_ws(&mut self, bytes: Vec<u8>) {
        let Ok(msg) = wire::decode::<ServerMsg>(&bytes) else {
            self.warn_security("undecodable server message");
            return;
        };
        #[cfg(debug_assertions)]
        debug_log_server_msg("received", &msg);
        self.last_inbound = Instant::now();
        self.ping_sent_at = None;
        match msg {
            // Proof of life and nothing else — recorded just above.
            ServerMsg::Pong => {}
            ServerMsg::Challenge { .. } | ServerMsg::AuthOk { .. } => {}
            ServerMsg::Ack {
                doc_id,
                seq,
                client_tag,
            } => {
                self.outbox.remove(&client_tag);
                self.note_seq(doc_id, seq);
                self.check_idle(doc_id);
                self.after_seq_advance(doc_id).await;
            }
            ServerMsg::Broadcast { doc_id, seq, frame } => {
                self.apply_frame(doc_id, seq, frame, true);
                self.after_seq_advance(doc_id).await;
            }
            ServerMsg::Backlog {
                doc_id,
                frames,
                done: _,
            } => {
                for (seq, frame) in frames {
                    self.apply_frame(doc_id, seq, frame, false);
                }
                self.after_seq_advance(doc_id).await;
            }
            ServerMsg::SnapshotInfo {
                doc_id,
                covers_seq,
                epoch: _,
                snapshot,
            } => {
                self.apply_snapshot_info(doc_id, covers_seq, snapshot).await;
            }
            ServerMsg::SubscribedOk { doc_id, head_seq } => {
                if let Some(doc) = self.docs.get_mut(&doc_id) {
                    doc.live = true;
                }
                self.emit(SyncEvent::DocSynced { doc_id, head_seq });
            }
            ServerMsg::RequestSnapshot { doc_id, covers_seq } => {
                if let Some(doc) = self.docs.get_mut(&doc_id) {
                    doc.requested_snapshot = Some(covers_seq);
                }
                self.after_seq_advance(doc_id).await;
            }
            ServerMsg::Ephemeral { doc_id, payload } => {
                self.receive_ephemeral(doc_id, &payload);
            }
            ServerMsg::Envelopes {
                space_id,
                current_epoch,
                envelopes,
            } => {
                self.handle_envelopes(space_id, current_epoch, envelopes)
                    .await;
            }
            ServerMsg::Membership {
                space_id,
                current_epoch,
                ops,
            } => {
                self.handle_membership(space_id, current_epoch, ops).await;
            }
            ServerMsg::EpochBump {
                space_id,
                new_epoch,
            } => {
                self.emit(SyncEvent::EpochBumped {
                    space_id,
                    epoch: new_epoch,
                });
                // Fetch the rotated key + refreshed membership; if we were the
                // removed device the server answers NotMember and decryption of
                // new traffic stays impossible — exactly the design.
                self.envelopes_inflight.remove(&space_id);
                self.membership_inflight.remove(&space_id);
                self.request_envelopes(space_id).await;
                self.send(ClientMsg::FetchMembership { space_id }).await;
            }
            ServerMsg::SpaceDeleted { space_id } => {
                // The space is gone server-side. Drop our keys/subscriptions and
                // let the UI tear down the local mirror.
                self.forget_space_state(space_id);
                self.emit(SyncEvent::SpaceDeleted { space_id });
            }
            ServerMsg::SpaceList { spaces } => {
                for waiter in self.pending_space_lists.drain(..) {
                    let _ = waiter.send(Ok(spaces.clone()));
                }
            }
            ServerMsg::BlobStored { blob_id } => {
                if let Some(reply) = self.pending_blob_puts.remove(&blob_id) {
                    let _ = reply.send(Ok(()));
                }
            }
            ServerMsg::BlobData { blob_id, frame } => {
                if let Some(waiters) = self.pending_blob_gets.remove(&blob_id) {
                    for (space_id, blob_key, reply) in waiters {
                        let _ = reply.send(self.open_blob_for(
                            space_id,
                            blob_id,
                            &blob_key,
                            frame.as_ref(),
                        ));
                    }
                }
            }
            ServerMsg::Error { code, context } => {
                log::warn!("server error {code:?}: {context}");
                // Transient by definition: whatever the relay could not serve,
                // ask for it again shortly.
                if code == wire::ErrorCode::Internal {
                    self.resubscribe_at
                        .get_or_insert(Instant::now() + self.config.reconnect_min);
                }
                // A blob rejection carries the blob id as its context. Resolve
                // the matching pending put permanently so the caller stops
                // reshipping, rather than leaving it to hang until disconnect
                // (which would re-drive the poison upload on reconnect).
                if code == wire::ErrorCode::BlobTooLarge
                    && let Ok(blob_id) = Uuid::parse_str(&context)
                    && let Some(reply) = self.pending_blob_puts.remove(&blob_id)
                {
                    let _ = reply.send(Err(SyncError::BlobTooLarge));
                    return;
                }
                // Quota is account-wide, not per blob, so every upload in
                // flight is equally refused — and errors carry no correlation
                // id to pick out just one. Failing them all is what stops an
                // over-quota user's image upload from spinning forever with no
                // message, which is how this first showed up.
                // A refused CreateSpace names the space in its context. Report
                // it as its own event so the app can un-mark that space rather
                // than only showing a message about a space it cannot identify.
                if code == wire::ErrorCode::AccountRequired
                    && let Ok(space_id) = Uuid::parse_str(&context)
                {
                    self.forget_space_state(space_id);
                    self.rejected_spaces.insert(space_id);
                    self.emit(SyncEvent::SpaceRejected { space_id, context });
                    return;
                }
                // Fallout from a space the relay already refused: the docs were
                // sent before the refusal arrived. Reporting these would
                // overwrite the explanation the user can act on.
                if matches!(
                    code,
                    wire::ErrorCode::UnknownSpace | wire::ErrorCode::UnknownDoc
                ) && let Ok(id) = Uuid::parse_str(&context)
                    && (self.rejected_spaces.contains(&id)
                        || self
                            .rejected_spaces
                            .iter()
                            .any(|space| crypto::index_doc_id(space) == id))
                {
                    log::debug!("ignoring {code:?} for already-refused space content");
                    return;
                }
                if matches!(
                    code,
                    wire::ErrorCode::QuotaExceeded | wire::ErrorCode::AccountRequired
                ) {
                    for (_, reply) in self.pending_blob_puts.drain() {
                        let _ = reply.send(Err(SyncError::QuotaExceeded));
                    }
                }
                // Errors carry no correlation id (PoC); a membership/space
                // error while joins are pending fails them all so callers can
                // retry instead of hanging.
                if matches!(
                    code,
                    wire::ErrorCode::NotMember | wire::ErrorCode::UnknownSpace
                ) && !self.joins.is_empty()
                {
                    for (_, progress) in self.joins.drain() {
                        for waiter in progress.waiters {
                            let _ = waiter.send(Err(SyncError::Other(context.clone())));
                        }
                    }
                    self.envelopes_inflight.clear();
                    self.membership_inflight.clear();
                }
                self.emit(SyncEvent::ServerError { code, context });
            }
        }
    }

    /// Drop all in-memory state for a space: keys/membership, doc
    /// subscriptions, and any pending join/envelope bookkeeping. Shared by the
    /// local `ForgetSpace` (mirror deleted) and `SpaceDeleted` (destroyed
    /// server-side) paths.
    fn forget_space_state(&mut self, space_id: Uuid) {
        self.spaces.remove(&space_id);
        self.docs.retain(|_, doc| doc.space_id != space_id);
        self.joins.remove(&space_id);
        self.envelope_queue.remove(&space_id);
        self.envelopes_inflight.remove(&space_id);
        self.membership_queue.remove(&space_id);
        self.membership_inflight.remove(&space_id);
    }

    fn note_seq(&mut self, doc_id: Uuid, seq: u64) {
        let Some(doc) = self.docs.get_mut(&doc_id) else {
            return;
        };
        if seq <= doc.have_seq {
            return;
        }
        doc.ahead.insert(seq);
        while doc.ahead.remove(&(doc.have_seq + 1)) {
            doc.have_seq += 1;
        }
    }

    fn seen(&self, doc_id: &Uuid, seq: u64) -> bool {
        self.docs
            .get(doc_id)
            .is_some_and(|d| seq <= d.have_seq || d.ahead.contains(&seq))
    }

    /// Verify author membership + signature, decrypt, hand the plaintext
    /// update bytes to the UI replica. CRDT updates are order-independent, so
    /// out-of-order fan-out is forwarded immediately and only `have_seq`
    /// tracking cares about contiguity.
    /// `live` marks a real-time broadcast (vs. backlog catch-up): only then is
    /// the author attached as `caret_author` so receivers move the remote
    /// caret instantly.
    fn apply_frame(&mut self, doc_id: Uuid, seq: u64, frame: UpdateFrame, live: bool) {
        if self.seen(&doc_id, seq) {
            return; // duplicate: skip decrypt cost
        }
        let Some(doc) = self.docs.get(&doc_id) else {
            return;
        };
        let space_id = doc.space_id;
        let own = frame.author_device == self.identity.device_pk();
        let Some(space) = self.spaces.get(&space_id) else {
            return;
        };
        if !own && !space.membership.was_ever_member(&frame.author_device) {
            // Not necessarily hostile, and usually not: adding a member never
            // bumps the epoch, so an already-connected member has no reason to
            // have refetched the log since this device joined. Defer the frame
            // and refresh the log rather than dropping an honest edit — that
            // silently diverges the replica forever.
            log::debug!("doc {doc_id}: frame from device missing from the membership log");
            self.note_seq(doc_id, seq); // frontier advances; the frame is kept
            self.defer_frame(doc_id, frame);
            self.deferred_membership_request(space_id);
            return;
        }
        let Some(key) = space.keys.get(&frame.epoch) else {
            // Unknown epoch: queue + fetch envelopes, retry on arrival.
            let missing_epoch = frame.epoch;
            log::debug!("doc {doc_id}: missing epoch {missing_epoch}, fetching envelopes");
            let space_id_copy = space_id;
            // note_seq so the frontier advances; the frame itself is retained.
            self.note_seq(doc_id, seq);
            self.defer_frame(doc_id, frame);
            self.deferred_envelope_request(space_id_copy);
            return;
        };
        match crypto::open_update(&frame, key, &space_id, &doc_id) {
            Ok(plaintext) => {
                self.note_seq(doc_id, seq);
                // A device never moves its own remote caret from an echo.
                let caret_author = (live && !own).then_some(frame.author_device);
                self.emit(SyncEvent::DocBytes {
                    doc_id,
                    update: plaintext,
                    caret_author,
                });
            }
            Err(err) => {
                self.warn_security(format!("doc {doc_id} seq {seq}: {err}"));
                self.note_seq(doc_id, seq);
            }
        }
    }

    /// Encrypt + sign an awareness payload under the doc's latest epoch key
    /// and relay it. Best-effort: dropped silently when keys/connection are
    /// missing (presence is non-durable by design).
    async fn send_ephemeral(&mut self, doc_id: Uuid, payload: &[u8]) {
        let Some(doc) = self.docs.get(&doc_id) else {
            return;
        };
        let space_id = doc.space_id;
        let Some(space) = self.spaces.get(&space_id) else {
            return;
        };
        if !space.membership.can_write(&self.identity.device_pk()) {
            return;
        }
        let Some((epoch, key)) = space.latest_key() else {
            return;
        };
        let frame = crypto::seal_ephemeral(&self.identity, key, &space_id, &doc_id, epoch, payload);
        let Ok(bytes) = wire::encode(&frame) else {
            return;
        };
        self.send(ClientMsg::Ephemeral {
            doc_id,
            payload: bytes,
        })
        .await;
    }

    /// Verify + decrypt a relayed awareness payload. Same gate as updates:
    /// known member, valid signature, valid AEAD — otherwise drop + warn.
    fn receive_ephemeral(&mut self, doc_id: Uuid, payload: &[u8]) {
        let Some(doc) = self.docs.get(&doc_id) else {
            return;
        };
        let space_id = doc.space_id;
        let Some(space) = self.spaces.get(&space_id) else {
            return;
        };
        let Ok(frame) = wire::decode::<wire::EphemeralFrame>(payload) else {
            self.warn_security(format!("doc {doc_id}: undecodable ephemeral"));
            return;
        };
        if !space.membership.was_ever_member(&frame.author_device) {
            self.warn_security(format!("doc {doc_id}: ephemeral from unknown device"));
            return;
        }
        let Some(key) = space.keys.get(&frame.epoch) else {
            return; // unknown epoch: presence is transient, just drop it
        };
        match crypto::open_ephemeral(&frame, key, &space_id, &doc_id) {
            Ok(plaintext) => self.emit(SyncEvent::Ephemeral {
                doc_id,
                author_device: frame.author_device,
                payload: plaintext,
            }),
            Err(err) => self.warn_security(format!("doc {doc_id}: ephemeral: {err}")),
        }
    }

    /// Queue an envelope fetch from a sync (non-async) code path; sent on the
    /// next async opportunity, at most one in flight per space.
    /// Park a frame we can't process yet. Dropping the oldest on overflow
    /// keeps a hostile relay from growing this without bound; the frames lost
    /// that way are ones we were never able to decrypt or attribute anyway.
    fn defer_frame(&mut self, doc_id: Uuid, frame: UpdateFrame) {
        let Some(doc) = self.docs.get_mut(&doc_id) else {
            return;
        };
        if doc.deferred.len() >= MAX_DEFERRED_FRAMES {
            doc.deferred.remove(0);
        }
        doc.deferred.push(frame);
    }

    fn deferred_envelope_request(&mut self, space_id: Uuid) {
        if !self.envelopes_inflight.contains(&space_id) {
            self.envelope_queue.insert(space_id);
        }
    }

    fn deferred_membership_request(&mut self, space_id: Uuid) {
        if !self.membership_inflight.contains(&space_id) {
            self.membership_queue.insert(space_id);
        }
    }

    async fn request_membership(&mut self, space_id: Uuid) {
        if self.membership_inflight.insert(space_id) {
            self.membership_queue.remove(&space_id);
            self.send(ClientMsg::FetchMembership { space_id }).await;
        }
    }

    async fn drain_membership_queue(&mut self) {
        while let Some(space_id) = self.membership_queue.iter().next().copied() {
            self.membership_queue.remove(&space_id);
            self.request_membership(space_id).await;
        }
    }

    async fn request_envelopes(&mut self, space_id: Uuid) {
        if self.envelopes_inflight.insert(space_id) {
            self.envelope_queue.remove(&space_id);
            self.send(ClientMsg::FetchEnvelopes { space_id }).await;
        }
    }

    async fn drain_envelope_queue(&mut self) {
        while let Some(space_id) = self.envelope_queue.iter().next().copied() {
            self.envelope_queue.remove(&space_id);
            self.request_envelopes(space_id).await;
        }
    }

    /// Housekeeping after seq movement: flush deferred envelope fetches and
    /// ask the UI for a snapshot when one is due (server-requested or
    /// threshold-crossed). The engine can't produce snapshots itself — it has
    /// no document — so it emits `SnapshotNeeded` and waits for
    /// `ProvideSnapshot`.
    async fn after_seq_advance(&mut self, doc_id: Uuid) {
        self.drain_envelope_queue().await;
        self.drain_membership_queue().await;

        let Some(doc) = self.docs.get_mut(&doc_id) else {
            return;
        };
        let have = doc.have_seq;
        let server_asked = doc.requested_snapshot.is_some_and(|covers| have >= covers);
        let threshold_crossed =
            have.saturating_sub(doc.snapshot_covers) >= self.config.snapshot_threshold && doc.live;
        if (server_asked || threshold_crossed)
            && have > 0
            && doc.snapshot_asked.is_none_or(|asked| have > asked)
        {
            doc.snapshot_asked = Some(have);
            self.emit(SyncEvent::SnapshotNeeded {
                doc_id,
                covers_seq: have,
            });
        }
    }

    /// Snapshot = the full doc state provided by the UI, encrypted under the
    /// **latest** epoch — this is how the server's at-rest copy eventually
    /// becomes unreadable with revoked keys.
    async fn put_blob(
        &mut self,
        space_id: Uuid,
        blob_id: Uuid,
        blob_key: crypto::BlobKey,
        plaintext: Vec<u8>,
        reply: oneshot::Sender<Result<(), SyncError>>,
    ) {
        // No space-key lookup: a blob is sealed under its own random content
        // key, which rides in the space index doc (see `crypto::BlobKey`).
        if !self.spaces.contains_key(&space_id) {
            let _ = reply.send(Err(SyncError::UnknownSpace));
            return;
        }
        // Reject over-ceiling blobs before they touch the wire: the sealed frame
        // would exceed MAX_BLOB_BYTES, and sending it would trip the server's
        // frame guard and tear down the connection (poisoning every reconnect).
        // Fail permanently here so the caller stops reshipping instead.
        if plaintext.len() + crypto::AEAD_TAG_BYTES > wire::MAX_BLOB_BYTES {
            let _ = reply.send(Err(SyncError::BlobTooLarge));
            return;
        }
        let frame = crypto::seal_blob(&blob_key, &space_id, &blob_id, &plaintext);
        if self.send(ClientMsg::PutBlob { frame }).await {
            self.pending_blob_puts.insert(blob_id, reply);
        } else {
            let _ = reply.send(Err(SyncError::Disconnected));
        }
    }

    /// Decrypt a fetched blob frame under the blob's own content key, which the
    /// caller read from the space index doc. `Ok(None)` for a soft miss (the
    /// server doesn't have it yet) so the caller can retry; the content-hash
    /// check against the index doc happens app-side.
    fn open_blob_for(
        &self,
        space_id: Uuid,
        blob_id: Uuid,
        blob_key: &crypto::BlobKey,
        frame: Option<&wire::BlobFrame>,
    ) -> Result<Option<Vec<u8>>, SyncError> {
        let Some(frame) = frame else {
            return Ok(None);
        };
        match crypto::open_blob(frame, blob_key, &space_id, &blob_id) {
            Ok(plain) => Ok(Some(plain)),
            Err(_) => {
                self.warn_security(format!("blob {blob_id}: decrypt failed"));
                Ok(None)
            }
        }
    }

    async fn seal_and_put_snapshot(&mut self, doc_id: Uuid, covers_seq: u64, state: &[u8]) {
        let Some(doc) = self.docs.get_mut(&doc_id) else {
            return;
        };
        // The event channel is FIFO: by the time the UI answered, it had
        // applied every DocBytes ≤ covers_seq, so `state` covers them.
        doc.requested_snapshot = None;
        doc.snapshot_asked = None;
        doc.snapshot_covers = doc.snapshot_covers.max(covers_seq);
        let space_id = doc.space_id;
        let Some(space) = self.spaces.get(&space_id) else {
            return;
        };
        let Some((epoch, key)) = space.latest_key() else {
            return;
        };
        let snapshot = crypto::seal_snapshot(
            &self.identity,
            key,
            &space_id,
            &doc_id,
            covers_seq,
            epoch,
            state,
        );
        self.send(ClientMsg::PutSnapshot { snapshot }).await;
    }

    async fn apply_snapshot_info(
        &mut self,
        doc_id: Uuid,
        covers_seq: u64,
        snapshot: Option<SnapshotFrame>,
    ) {
        let Some(doc) = self.docs.get_mut(&doc_id) else {
            return;
        };
        doc.snapshot_covers = doc.snapshot_covers.max(covers_seq);
        let Some(frame) = snapshot else {
            return;
        };
        let space_id = doc.space_id;
        let Some(space) = self.spaces.get(&space_id) else {
            return;
        };
        let own = frame.author_device == self.identity.device_pk();
        if !own && !space.membership.was_ever_member(&frame.author_device) {
            self.warn_security(format!("doc {doc_id}: snapshot from unknown device"));
            return;
        }
        let Some(key) = space.keys.get(&frame.epoch) else {
            self.warn_security(format!(
                "doc {doc_id}: snapshot under unknown epoch {}",
                frame.epoch
            ));
            self.request_envelopes(space_id).await;
            return;
        };
        match crypto::open_snapshot(&frame, key, &space_id, &doc_id) {
            Ok(plaintext) => {
                let doc = self.docs.get_mut(&doc_id).unwrap();
                // The snapshot subsumes everything ≤ covers_seq.
                if covers_seq > doc.have_seq {
                    doc.have_seq = covers_seq;
                    doc.ahead.retain(|s| *s > covers_seq);
                }
                self.emit(SyncEvent::DocBytes {
                    doc_id,
                    update: plaintext,
                    // A snapshot is a full-state replace, not a live edit.
                    caret_author: None,
                });
            }
            Err(err) => self.warn_security(format!("doc {doc_id}: snapshot: {err}")),
        }
    }

    async fn handle_envelopes(
        &mut self,
        space_id: Uuid,
        current_epoch: u32,
        envelopes: Vec<wire::Envelope>,
    ) {
        self.envelopes_inflight.remove(&space_id);
        let space = self.spaces.entry(space_id).or_insert_with(|| SpaceState {
            keys: BTreeMap::new(),
            current_epoch: 0,
            membership: MembershipState::default(),
        });
        for envelope in envelopes {
            if space.keys.contains_key(&envelope.epoch) {
                continue;
            }
            match crypto::unseal_space_key(&self.identity, &envelope.sealed_key) {
                Ok(key) => {
                    space.keys.insert(envelope.epoch, key);
                }
                Err(err) => {
                    log::warn!("envelope unseal failed (epoch {}): {err}", envelope.epoch)
                }
            }
        }
        space.current_epoch = space.current_epoch.max(current_epoch);

        self.retry_deferred_frames(space_id);
        self.finish_join(space_id, |progress| progress.have_envelopes = true);
    }

    /// Re-run every frame parked for a space, after the missing piece (space
    /// keys or membership log) has arrived. A frame that is still not usable
    /// goes back on the queue.
    fn retry_deferred_frames(&mut self, space_id: Uuid) {
        let retriable: Vec<(Uuid, Vec<UpdateFrame>)> = self
            .docs
            .iter_mut()
            .filter(|(_, d)| d.space_id == space_id && !d.deferred.is_empty())
            .map(|(id, d)| (*id, std::mem::take(&mut d.deferred)))
            .collect();
        for (doc_id, frames) in retriable {
            for frame in frames {
                self.retry_frame(doc_id, frame);
            }
        }
    }

    /// Like `apply_frame` but for already-sequenced frames (seq accounting
    /// was done when the frame was first seen and queued).
    fn retry_frame(&mut self, doc_id: Uuid, frame: UpdateFrame) {
        let Some(doc) = self.docs.get(&doc_id) else {
            return;
        };
        let space_id = doc.space_id;
        let Some(space) = self.spaces.get(&space_id) else {
            return;
        };
        let own = frame.author_device == self.identity.device_pk();
        if !own && !space.membership.was_ever_member(&frame.author_device) {
            // The refreshed log still doesn't know this device. Keep the frame
            // (a later Add may yet explain it) but say so — after a refetch
            // this is what a forged frame looks like.
            self.warn_security(format!("doc {doc_id}: frame from unknown device"));
            self.defer_frame(doc_id, frame);
            return;
        }
        let Some(key) = space.keys.get(&frame.epoch) else {
            self.defer_frame(doc_id, frame);
            return;
        };
        match crypto::open_update(&frame, key, &space_id, &doc_id) {
            // Deferred (was undecryptable): no longer "live", so no caret jump.
            Ok(plaintext) => self.emit(SyncEvent::DocBytes {
                doc_id,
                update: plaintext,
                caret_author: None,
            }),
            Err(err) => self.warn_security(format!("doc {doc_id}: retry decrypt: {err}")),
        }
    }

    async fn handle_membership(
        &mut self,
        space_id: Uuid,
        current_epoch: u32,
        ops: Vec<wire::SignedMembershipOp>,
    ) {
        // The signed log is the trust root — replay it fully; the server's
        // current_epoch is just a hint for key fetching.
        match MembershipState::replay(&space_id, &ops) {
            Ok(state) => {
                let space = self.spaces.entry(space_id).or_insert_with(|| SpaceState {
                    keys: BTreeMap::new(),
                    current_epoch: 0,
                    membership: MembershipState::default(),
                });
                space.current_epoch = space
                    .current_epoch
                    .max(current_epoch)
                    .max(state.current_epoch);
                // Never regress to a shorter prefix of the same append-only
                // log. A refetch races our own just-sent ops: `add_member` /
                // `remove_member` apply locally and send without waiting
                // (`engine.rs:828,869`), so a reply the server assembled before
                // persisting them would rewind `next_op_seq` and drop members
                // we already know about — after which every later op in the
                // space fails ("removed device was never a member", or a
                // rejected op_seq). Ours is a verified extension of theirs, so
                // keep the longer one and let the server catch up.
                if state.next_op_seq >= space.membership.next_op_seq {
                    space.membership = state;
                }
                self.membership_inflight.remove(&space_id);
                // The refreshed log may be exactly what a parked frame was
                // waiting for — a member added after we joined.
                self.retry_deferred_frames(space_id);
                self.finish_join(space_id, |progress| progress.have_membership = true);
            }
            Err(err) => {
                self.membership_inflight.remove(&space_id);
                self.warn_security(format!("space {space_id}: membership log invalid: {err}"));
                // A poisoned log fails the join.
                if let Some(progress) = self.joins.remove(&space_id) {
                    for waiter in progress.waiters {
                        let _ = waiter.send(Err(SyncError::Other(format!(
                            "membership verification failed: {err}"
                        ))));
                    }
                }
            }
        }
    }

    fn finish_join(&mut self, space_id: Uuid, mark: impl FnOnce(&mut JoinProgress)) {
        let Some(progress) = self.joins.get_mut(&space_id) else {
            return;
        };
        mark(progress);
        if progress.have_membership && progress.have_envelopes {
            let progress = self.joins.remove(&space_id).unwrap();
            for waiter in progress.waiters {
                let _ = waiter.send(Ok(()));
            }
        }
    }
}

async fn recv_ws(stream: &mut Option<SplitStream<Ws>>) -> Option<Result<Vec<u8>, WsError>> {
    match stream {
        Some(stream) => stream.next().await,
        None => std::future::pending().await,
    }
}

async fn send_ws(ws: &mut Ws, msg: &ClientMsg) -> Result<(), String> {
    let bytes = wire::encode(msg).map_err(|e| e.to_string())?;
    ws.send(bytes).await.map_err(|e| e.to_string())?;
    #[cfg(debug_assertions)]
    debug_log_client_msg("sent", msg);
    Ok(())
}

async fn read_ws(ws: &mut Ws) -> Result<ServerMsg, String> {
    let deadline = Duration::from_secs(5);
    let bytes = super::clock::timeout(deadline, ws.next())
        .await
        .map_err(|_| "handshake timeout".to_string())?
        .ok_or("connection closed during handshake")?
        .map_err(|e| e.to_string())?;
    let msg = wire::decode::<ServerMsg>(&bytes).map_err(|e| e.to_string())?;
    #[cfg(debug_assertions)]
    debug_log_server_msg("received", &msg);
    Ok(msg)
}

#[cfg(debug_assertions)]
fn debug_log_client_msg(direction: &str, msg: &ClientMsg) {
    println!(
        "enkr sync request {direction}: {}",
        describe_client_msg(msg)
    );
}

#[cfg(debug_assertions)]
fn debug_log_server_msg(direction: &str, msg: &ServerMsg) {
    println!(
        "enkr sync response {direction}: {}",
        describe_server_msg(msg)
    );
}

#[cfg(debug_assertions)]
fn describe_client_msg(msg: &ClientMsg) -> String {
    match msg {
        ClientMsg::Ping => "Ping".to_string(),
        ClientMsg::Hello {
            protocol_version, ..
        } => format!("Hello protocol_version={protocol_version}"),
        ClientMsg::Auth { sig, account_token } => format!(
            "Auth sig_len={} token={}",
            sig.len(),
            if account_token.is_some() { "yes" } else { "no" }
        ),
        ClientMsg::CreateSpace {
            space_id,
            envelopes,
            ..
        } => format!("CreateSpace space={space_id} envelopes={}", envelopes.len()),
        ClientMsg::AddMember {
            space_id,
            envelopes,
            ..
        } => format!("AddMember space={space_id} envelopes={}", envelopes.len()),
        ClientMsg::RemoveMember {
            space_id,
            new_epoch,
            envelopes,
            ..
        } => format!(
            "RemoveMember space={space_id} new_epoch={new_epoch} envelopes={}",
            envelopes.len()
        ),
        ClientMsg::CreateDoc { space_id, doc_id } => {
            format!("CreateDoc space={space_id} doc={doc_id}")
        }
        ClientMsg::Subscribe { entries } => {
            format!("Subscribe entries={}", entries.len())
        }
        ClientMsg::PushUpdate {
            doc_id,
            client_tag,
            frame,
        } => format!(
            "PushUpdate doc={doc_id} tag={client_tag} bytes={}",
            frame.ciphertext.len()
        ),
        ClientMsg::Ephemeral { doc_id, payload } => {
            format!("Ephemeral doc={doc_id} bytes={}", payload.len())
        }
        ClientMsg::PutSnapshot { snapshot } => format!(
            "PutSnapshot doc={} covers_seq={} bytes={}",
            snapshot.doc_id,
            snapshot.covers_seq,
            snapshot.ciphertext.len()
        ),
        ClientMsg::FetchEnvelopes { space_id } => format!("FetchEnvelopes space={space_id}"),
        ClientMsg::FetchMembership { space_id } => format!("FetchMembership space={space_id}"),
        ClientMsg::ListSpaces => "ListSpaces".to_string(),
        ClientMsg::DeleteSpace { space_id } => format!("DeleteSpace space={space_id}"),
        ClientMsg::PutBlob { frame } => format!(
            "PutBlob space={} blob={} bytes={}",
            frame.space_id,
            frame.blob_id,
            frame.ciphertext.len()
        ),
        ClientMsg::GetBlob { space_id, blob_id } => {
            format!("GetBlob space={space_id} blob={blob_id}")
        }
        ClientMsg::DeleteBlob { space_id, blob_id } => {
            format!("DeleteBlob space={space_id} blob={blob_id}")
        }
    }
}

#[cfg(debug_assertions)]
fn describe_server_msg(msg: &ServerMsg) -> String {
    match msg {
        ServerMsg::Pong => "Pong".to_string(),
        ServerMsg::Challenge { .. } => "Challenge".to_string(),
        ServerMsg::AuthOk { session_id, .. } => format!("AuthOk session={session_id}"),
        ServerMsg::SnapshotInfo {
            doc_id,
            covers_seq,
            snapshot,
            ..
        } => format!(
            "SnapshotInfo doc={doc_id} covers_seq={covers_seq} has_snapshot={}",
            snapshot.is_some()
        ),
        ServerMsg::Backlog {
            doc_id,
            frames,
            done,
        } => format!("Backlog doc={doc_id} frames={} done={done}", frames.len()),
        ServerMsg::SubscribedOk { doc_id, head_seq } => {
            format!("SubscribedOk doc={doc_id} head_seq={head_seq}")
        }
        ServerMsg::Ack {
            doc_id,
            seq,
            client_tag,
        } => format!("Ack doc={doc_id} seq={seq} tag={client_tag}"),
        ServerMsg::Broadcast { doc_id, seq, frame } => {
            format!(
                "Broadcast doc={doc_id} seq={seq} bytes={}",
                frame.ciphertext.len()
            )
        }
        ServerMsg::Ephemeral { doc_id, payload } => {
            format!("Ephemeral doc={doc_id} bytes={}", payload.len())
        }
        ServerMsg::RequestSnapshot { doc_id, covers_seq } => {
            format!("RequestSnapshot doc={doc_id} covers_seq={covers_seq}")
        }
        ServerMsg::Envelopes {
            space_id,
            current_epoch,
            envelopes,
        } => format!(
            "Envelopes space={space_id} current_epoch={current_epoch} envelopes={}",
            envelopes.len()
        ),
        ServerMsg::Membership {
            space_id,
            current_epoch,
            ops,
        } => format!(
            "Membership space={space_id} current_epoch={current_epoch} ops={}",
            ops.len()
        ),
        ServerMsg::EpochBump {
            space_id,
            new_epoch,
        } => format!("EpochBump space={space_id} new_epoch={new_epoch}"),
        ServerMsg::Error { code, context } => {
            format!("Error code={code:?} context={context}")
        }
        ServerMsg::SpaceList { spaces } => format!("SpaceList spaces={}", spaces.len()),
        ServerMsg::SpaceDeleted { space_id } => format!("SpaceDeleted space={space_id}"),
        ServerMsg::BlobStored { blob_id } => format!("BlobStored blob={blob_id}"),
        ServerMsg::BlobData { blob_id, frame } => {
            format!("BlobData blob={blob_id} has_frame={}", frame.is_some())
        }
    }
}
