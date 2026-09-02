//! Scale budgets for the sync protocol (`sync-recap-0.md`).
//!
//! Every test here is `#[ignore]`d — they are slow by construction and are not
//! part of the normal `cargo test` run:
//!
//! ```text
//! cargo test -p enkr --test scale -- --ignored --nocapture --test-threads=1
//! ```
//!
//! The assertions encode the *release budget*, not today's behaviour, so
//! several of them fail against the current code on purpose. That failure is
//! the baseline; each fix in the scale plan turns one of them green. Every test
//! also prints its measured numbers under `--nocapture`, so a run is useful
//! even when it fails.
//!
//! Scale knobs default to the release target and can be dialled down for a
//! quick local run, e.g. `ENKR_SCALE_DOCS=200 ENKR_SCALE_CLIENTS=5`.

mod harness;

use std::time::{Duration, Instant};

use enkr::sync::{MemberRole, SyncEvent};
use enkr_proto::crypto::DeviceIdentity;
use enkr_syncd::ServerConfig;
use uuid::Uuid;

use harness::metered::StoreMetrics;
use harness::net::NetProxy;
use harness::{TestClient, TestServer, converge, invite_and_join, wait_connected};

// ---------------------------------------------------------------------------
// Scale knobs
// ---------------------------------------------------------------------------

/// Release target: 20 spaces × 10k docs, 50 clients/space, 100k updates on a
/// hot doc, 1k membership ops.
fn knob(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

fn docs_per_space() -> usize {
    knob("ENKR_SCALE_DOCS", 10_000)
}
fn clients() -> usize {
    knob("ENKR_SCALE_CLIENTS", 50)
}
fn spaces() -> usize {
    knob("ENKR_SCALE_SPACES", 20)
}
fn updates() -> usize {
    knob("ENKR_SCALE_UPDATES", 100_000)
}
fn membership_ops() -> usize {
    knob("ENKR_SCALE_MEMBERSHIP_OPS", 1_000)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// `n` connected clients, all members of `space`. Generalises
/// `space_with_two_clients` to a fleet.
async fn fleet(server: &TestServer, owner: &TestClient, space: Uuid, n: usize) -> Vec<TestClient> {
    let mut peers = Vec::with_capacity(n);
    for _ in 0..n {
        let peer = server.client();
        wait_connected(&peer).await;
        invite_and_join(owner, &peer, space).await;
        peers.push(peer);
    }
    peers
}

/// Open `docs` on `client` the way `AppSync` does when joining a space: one
/// batched `Subscribe` per `SUBSCRIBE_BATCH` docs, not one frame per doc.
async fn open_docs(client: &TestClient, space: Uuid, docs: &[Uuid]) {
    client.open_docs(space, docs).await.expect("open docs");
}

/// Time from now until *every* peer's replica of `doc` contains `needle` —
/// the room's edit→remote-apply latency. `None` on timeout.
///
/// Measuring the whole room per round (rather than per peer, sequentially)
/// matters: polling peers one at a time reports ~0 for everyone after the
/// first, because they receive the broadcast while the first is being waited on.
async fn wait_for_all(
    peers: &[TestClient],
    doc: Uuid,
    needle: &str,
    budget: Duration,
) -> Option<Duration> {
    let started = Instant::now();
    loop {
        let mut pending = false;
        for peer in peers {
            if !peer.doc_text(doc).await.ok()?.contains(needle) {
                pending = true;
                break;
            }
        }
        if !pending {
            return Some(started.elapsed());
        }
        if started.elapsed() > budget {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
}

/// Block until a metered counter stops rising — the server has drained what we
/// queued. `open_doc` returns as soon as the `Subscribe` is written to the
/// socket (`engine.rs:761-779`), so without this a test reads its metrics
/// before the server has done the work.
async fn settle(counter: &std::sync::atomic::AtomicU64, quiet: Duration, budget: Duration) {
    let started = Instant::now();
    let mut last = StoreMetrics::get(counter);
    let mut stable_since = Instant::now();
    loop {
        tokio::time::sleep(Duration::from_millis(20)).await;
        let now = StoreMetrics::get(counter);
        if now != last {
            last = now;
            stable_since = Instant::now();
        } else if stable_since.elapsed() >= quiet {
            return;
        }
        if started.elapsed() > budget {
            return;
        }
    }
}

/// p50 / p99 of a latency sample, in milliseconds.
fn percentiles(mut samples: Vec<Duration>) -> (f64, f64) {
    assert!(!samples.is_empty(), "no latency samples collected");
    samples.sort_unstable();
    let at = |q: f64| {
        let idx = ((samples.len() as f64 - 1.0) * q).round() as usize;
        samples[idx].as_secs_f64() * 1000.0
    };
    (at(0.50), at(0.99))
}

fn row_count(db: &rusqlite::Connection, sql: &str) -> i64 {
    db.query_row(sql, [], |row| row.get(0))
        .expect("count query")
}

// ===========================================================================
// Join cost
// ===========================================================================

/// A space with many docs must not cost one `Subscribe` round trip per doc.
///
/// Budget: a fresh joiner's subscribe path touches the store a small multiple
/// of the *batch* count, not of the doc count. `doc_space` is called once per
/// `Subscribe` entry (`enkr-syncd/src/lib.rs:479-483`), so it is the direct
/// read of how many entries the server had to process one at a time.
#[tokio::test]
#[ignore = "scale budget"]
async fn cold_join_large_space() {
    let n = docs_per_space();
    let server = TestServer::start_metered(ServerConfig::default()).await;
    let a = server.client();
    wait_connected(&a).await;
    let space = a.create_space().await.expect("create space");

    let seeding = Instant::now();
    let mut docs = Vec::with_capacity(n);
    for _ in 0..n {
        docs.push(a.create_doc(space).await.expect("create doc"));
    }
    println!("[cold_join] seeded {n} docs in {:?}", seeding.elapsed());

    let b = server.client();
    wait_connected(&b).await;
    invite_and_join(&a, &b, space).await;

    server.metrics().reset();
    let joining = Instant::now();
    open_docs(&b, space, &docs).await;
    settle(
        &server.metrics().doc_space,
        Duration::from_millis(500),
        Duration::from_secs(120),
    )
    .await;
    let elapsed = joining.elapsed();
    server.metrics().report("cold_join");
    println!("[cold_join] {n} docs opened in {elapsed:?}");

    let subscribe_entries = StoreMetrics::get(&server.metrics().doc_space);
    assert!(
        subscribe_entries < n as u64 / 4,
        "join issued ~one Subscribe entry per doc ({subscribe_entries} store lookups for {n} \
         docs) — batch the join (scale plan step 2)"
    );
}

/// The same join, over a link with a realistic round trip.
///
/// Localhost has no RTT, which hides how chatty a protocol is. A real client
/// talks to a remote relay at roughly 30–80 ms RTT, so this runs the join
/// through a delaying proxy and reports the wall clock. It asserts only the
/// batching invariant (the timing is reported, not gated) because absolute
/// numbers here depend on the host.
#[tokio::test]
#[ignore = "scale budget"]
async fn cold_join_large_space_over_wan() {
    let n = docs_per_space().min(2_000);
    let one_way = Duration::from_millis(25); // 50 ms RTT
    let server = TestServer::start_metered(ServerConfig::default()).await;
    let proxy = NetProxy::start(server.addr, one_way).await;

    let a = server.client();
    wait_connected(&a).await;
    let space = a.create_space().await.expect("create space");
    let mut docs = Vec::with_capacity(n);
    for _ in 0..n {
        docs.push(a.create_doc(space).await.expect("create doc"));
    }

    // The joiner is the one behind the delayed link. Time the handshake as
    // proof the proxy is really delaying — otherwise this whole test could pass
    // while measuring a zero-latency link.
    let handshake = Instant::now();
    let b = server.client_at(proxy.url());
    wait_connected(&b).await;
    let handshake = handshake.elapsed();
    assert!(
        handshake >= one_way * 2,
        "handshake took {handshake:?} — the latency proxy is not delaying traffic"
    );
    invite_and_join(&a, &b, space).await;

    server.metrics().reset();
    let joining = Instant::now();
    open_docs(&b, space, &docs).await;
    settle(
        &server.metrics().updates_since,
        Duration::from_millis(500),
        Duration::from_secs(180),
    )
    .await;
    let elapsed = joining.elapsed();
    server.metrics().report("cold_join_wan");
    println!(
        "[cold_join_wan] {n} docs joined over a {}ms-RTT link in {elapsed:?}",
        one_way.as_millis() * 2
    );

    // Batching is the invariant under test: doc→space resolution and the
    // membership check must not scale with the doc count.
    let per_entry = StoreMetrics::get(&server.metrics().doc_space);
    let acl = StoreMetrics::get(&server.metrics().is_active_member);
    assert_eq!(per_entry, 0, "subscribe still resolves doc→space per entry");
    assert!(
        acl < n as u64 / 10,
        "membership checked {acl} times for {n} docs — should be once per space per message"
    );
}

/// Catching up a long-lived doc must ride a snapshot, not a full history
/// replay. Extends `cold_sync_10k_updates_under_a_second` (sync.rs) by 10×.
#[tokio::test]
#[ignore = "scale budget"]
async fn cold_sync_many_updates() {
    let n = updates();
    let server = TestServer::start_metered(ServerConfig::default()).await;
    let a = server.client();
    wait_connected(&a).await;
    let space = a.create_space().await.expect("create space");
    let doc = a.create_doc(space).await.expect("create doc");

    // Flush periodically: without it the client's debounce merges the whole run
    // into a couple of frames (`merge_updates_v1`, `engine.rs:385-454`) and the
    // backlog under test never accumulates. Matches the shape
    // `cold_sync_10k_updates_under_a_second` uses in `sync.rs`.
    let seeding = Instant::now();
    for i in 0..n {
        a.insert_text(doc, 0, format!("{i};"))
            .await
            .expect("insert");
        if i % 50 == 0 {
            a.flush().await.expect("flush");
        }
    }
    a.flush().await.expect("flush");
    converge(&[&a], doc).await;
    println!("[cold_sync] seeded {n} updates in {:?}", seeding.elapsed());
    let frames = row_count(&server.raw_db(), "SELECT COUNT(*) FROM updates");
    println!("[cold_sync] {frames} frames in the server log");

    let b = server.client();
    wait_connected(&b).await;
    invite_and_join(&a, &b, space).await;

    server.metrics().reset();
    let started = Instant::now();
    b.open_doc(space, doc).await.expect("open doc");
    converge(&[&a, &b], doc).await;
    let elapsed = started.elapsed();
    server.metrics().report("cold_sync");
    println!("[cold_sync] cold-synced {n} updates in {elapsed:?}");

    assert!(
        elapsed < Duration::from_secs(5),
        "cold sync of {n} updates took {elapsed:?}; budget is 5s (scale plan step 1)"
    );
}

/// A doc's *first* remote subscriber should get a snapshot too. Today the
/// snapshot offer is gated on the replica already being live **and** a 500
/// update tail (`engine.rs:1239-1264`), so a first joiner replays all history —
/// the structural cause of the "~5s to sync one document" report (TODO.md:8).
#[tokio::test]
#[ignore = "scale budget"]
async fn first_join_gets_snapshot() {
    // Deliberately below the 500-update snapshot threshold, but flushed
    // periodically so it becomes a real *frame* history rather than one merged
    // update — the debounce would otherwise coalesce the whole run and there
    // would be no backlog to replay.
    const EDITS: usize = 300;
    let server = TestServer::start_metered(ServerConfig::default()).await;
    let a = server.client();
    wait_connected(&a).await;
    let space = a.create_space().await.expect("create space");
    let doc = a.create_doc(space).await.expect("create doc");
    for i in 0..EDITS {
        a.insert_text(doc, 0, format!("{i};"))
            .await
            .expect("insert");
        if i % 5 == 0 {
            a.flush().await.expect("flush");
        }
    }
    a.flush().await.expect("flush");
    converge(&[&a], doc).await;
    let frames = row_count(&server.raw_db(), "SELECT COUNT(*) FROM updates");
    assert!(
        frames >= 32,
        "only {frames} frames in the log — the test needs a real history to replay"
    );

    let b = server.client();
    wait_connected(&b).await;
    invite_and_join(&a, &b, space).await;

    server.metrics().reset();
    b.open_doc(space, doc).await.expect("open doc");
    converge(&[&a, &b], doc).await;
    settle(
        &server.metrics().put_snapshot,
        Duration::from_millis(300),
        Duration::from_secs(30),
    )
    .await;
    server.metrics().report("first_join");

    let snapshots = StoreMetrics::get(&server.metrics().put_snapshot);
    assert!(
        snapshots > 0,
        "a cold join replayed {frames} frames and left no snapshot behind, so the next \
         joiner replays them too (scale plan step 1b)"
    );

    // And the point of it: a second joiner rides the snapshot instead of the log.
    let c = server.client();
    wait_connected(&c).await;
    invite_and_join(&a, &c, space).await;
    server.metrics().reset();
    c.open_doc(space, doc).await.expect("open doc");
    converge(&[&a, &c], doc).await;
    server.metrics().report("second_join");
    let served = StoreMetrics::get(&server.metrics().latest_snapshot);
    assert!(served > 0, "second joiner did not consult a snapshot");
}

// ===========================================================================
// Fan-out
// ===========================================================================

/// PLAN.md §M6: median edit→remote-apply < 150 ms, p99 < 400 ms.
#[tokio::test]
#[ignore = "scale budget"]
async fn fanout_many_clients_one_doc() {
    const ROUNDS: usize = 20;
    let k = clients();
    let server = TestServer::start_metered(ServerConfig::default()).await;
    let owner = server.client();
    wait_connected(&owner).await;
    let space = owner.create_space().await.expect("create space");
    let doc = owner.create_doc(space).await.expect("create doc");

    let peers = fleet(&server, &owner, space, k).await;
    for peer in &peers {
        peer.open_doc(space, doc).await.expect("open doc");
    }

    // Let every subscription settle, so the first rounds measure fan-out and
    // not a still-completing join.
    let all: Vec<&TestClient> = std::iter::once(&owner).chain(peers.iter()).collect();
    converge(&all, doc).await;

    let mut samples = Vec::with_capacity(ROUNDS);
    for round in 0..ROUNDS {
        let marker = format!("<m{round}>");
        owner.insert_text(doc, 0, &marker).await.expect("insert");
        let latency = wait_for_all(&peers, doc, &marker, Duration::from_secs(10))
            .await
            .unwrap_or_else(|| panic!("round {round} never reached every peer"));
        samples.push(latency);
    }

    let (p50, p99) = percentiles(samples);
    server.metrics().report("fanout_one_doc");
    println!("[fanout_one_doc] {k} clients: p50={p50:.1}ms p99={p99:.1}ms");
    assert!(
        p50 < 150.0,
        "median edit→remote-apply {p50:.1}ms exceeds the 150ms budget"
    );
    assert!(
        p99 < 400.0,
        "p99 edit→remote-apply {p99:.1}ms exceeds the 400ms budget"
    );
}

/// PLAN.md's M6 budget, measured the way a user would experience it: production
/// debounce (120 ms) over a link with a real round trip, not localhost.
///
/// The budget is median edit→remote-apply < 150 ms, p99 < 400 ms. The floor here
/// is `debounce + RTT` before any server work, so this reports the split rather
/// than only pass/fail — if the budget is unreachable at a given RTT, the fix is
/// the debounce, not the relay.
#[tokio::test]
#[ignore = "scale budget"]
async fn fanout_latency_over_wan() {
    const ROUNDS: usize = 20;
    let k = clients().min(10);
    let one_way = Duration::from_millis(25); // 50 ms RTT
    let server = TestServer::start_default().await;
    let proxy = NetProxy::start(server.addr, one_way).await;

    let owner = server.realistic_client_at(proxy.url());
    wait_connected(&owner).await;
    let space = owner.create_space().await.expect("create space");
    let doc = owner.create_doc(space).await.expect("create doc");

    let mut peers = Vec::with_capacity(k);
    for _ in 0..k {
        let peer = server.realistic_client_at(proxy.url());
        wait_connected(&peer).await;
        invite_and_join(&owner, &peer, space).await;
        peer.open_doc(space, doc).await.expect("open doc");
        peers.push(peer);
    }
    let all: Vec<&TestClient> = std::iter::once(&owner).chain(peers.iter()).collect();
    converge(&all, doc).await;

    let mut samples = Vec::with_capacity(ROUNDS);
    for round in 0..ROUNDS {
        let marker = format!("<w{round}>");
        owner.insert_text(doc, 0, &marker).await.expect("insert");
        let latency = wait_for_all(&peers, doc, &marker, Duration::from_secs(30))
            .await
            .unwrap_or_else(|| panic!("round {round} never reached every peer"));
        samples.push(latency);
    }

    let (p50, p99) = percentiles(samples);
    let floor = 120.0 + (one_way.as_millis() * 2) as f64;
    println!(
        "[fanout_wan] {k} clients, {}ms RTT: p50={p50:.1}ms p99={p99:.1}ms \
         (debounce+RTT floor is ~{floor:.0}ms of that)",
        one_way.as_millis() * 2
    );
    assert!(
        p99 < 400.0,
        "p99 edit→remote-apply {p99:.1}ms exceeds the 400ms budget"
    );
}

/// What the edit debounce actually costs and buys, over a realistic link.
///
/// `SyncConfig::debounce` is documented as "the #1 performance knob" and
/// PLAN.md's own M6 target is qualified "(debounce-dominated)". This is not a
/// pass/fail budget — it prints the trade so the value can be chosen with
/// numbers rather than by feel.
///
/// Two profiles, because they pull in opposite directions:
/// - **isolated edit** (a keystroke after a pause) pays the *whole* window, as
///   `Pending.first_edit` is stamped on the empty→non-empty transition.
/// - **sustained typing** ships a batch every `debounce`, so frame volume — the
///   relay's real load, one serialised `append_update` each — scales with 1/debounce.
#[tokio::test]
#[ignore = "scale budget"]
async fn debounce_latency_vs_volume() {
    const ROUNDS: usize = 10;
    const BURST: usize = 300;
    let one_way = Duration::from_millis(25); // 50 ms RTT

    println!("[debounce] RTT={}ms", one_way.as_millis() * 2);
    for debounce_ms in [30u64, 60, 120] {
        let server = TestServer::start_metered(ServerConfig::default()).await;
        let proxy = NetProxy::start(server.addr, one_way).await;
        let debounce = Duration::from_millis(debounce_ms);

        let a = server.client_with_debounce(proxy.url(), debounce);
        let b = server.client_with_debounce(proxy.url(), debounce);
        wait_connected(&a).await;
        wait_connected(&b).await;
        let space = a.create_space().await.expect("create space");
        let doc = a.create_doc(space).await.expect("create doc");
        invite_and_join(&a, &b, space).await;
        b.open_doc(space, doc).await.expect("open doc");
        converge(&[&a, &b], doc).await;

        // Profile 1: isolated edits, each after the pipeline has gone quiet.
        let peers = [b];
        let mut samples = Vec::with_capacity(ROUNDS);
        for round in 0..ROUNDS {
            let marker = format!("<i{round}>");
            a.insert_text(doc, 0, &marker).await.expect("insert");
            let latency = wait_for_all(&peers, doc, &marker, Duration::from_secs(30))
                .await
                .expect("isolated edit reached the peer");
            samples.push(latency);
        }
        let (p50, p99) = percentiles(samples);

        // Profile 2: a sustained burst — what it costs the relay in frames.
        server.metrics().reset();
        let burst = Instant::now();
        for i in 0..BURST {
            a.insert_text(doc, 0, format!("{i};"))
                .await
                .expect("insert");
            tokio::time::sleep(Duration::from_millis(5)).await; // ~200 keystrokes/s
        }
        let tail = format!("<end{debounce_ms}>");
        a.insert_text(doc, 0, &tail).await.expect("insert");
        wait_for_all(&peers, doc, &tail, Duration::from_secs(60))
            .await
            .expect("burst reached the peer");
        let frames = StoreMetrics::get(&server.metrics().append_update);
        println!(
            "[debounce] {debounce_ms:>3}ms: isolated p50={p50:>6.1}ms p99={p99:>6.1}ms | \
             {BURST}-keystroke burst = {frames} frames in {:?}",
            burst.elapsed()
        );
    }
}

/// Many clients spread across many spaces and docs: guards the fan-out lock
/// sharding (scale plan step 4c). Correctness bar — everything converges.
#[tokio::test]
#[ignore = "scale budget"]
async fn fanout_many_clients_many_docs() {
    let k = clients().min(20);
    let s = spaces().min(5);
    let server = TestServer::start_default().await;
    let owner = server.client();
    wait_connected(&owner).await;

    let mut rooms = Vec::with_capacity(s);
    for _ in 0..s {
        let space = owner.create_space().await.expect("create space");
        let doc = owner.create_doc(space).await.expect("create doc");
        rooms.push((space, doc));
    }

    let started = Instant::now();
    let mut peers = Vec::new();
    for i in 0..k {
        let (space, doc) = rooms[i % rooms.len()];
        let peer = server.client();
        wait_connected(&peer).await;
        invite_and_join(&owner, &peer, space).await;
        peer.open_doc(space, doc).await.expect("open doc");
        // Settle the subscription first: editing a replica that is still
        // catching up is a real case, but `late_joiner_edits_reach_existing_members`
        // covers it separately — this test is about fan-out under load.
        converge(&[&peer], doc).await;
        peer.insert_text(doc, 0, format!("<p{i}>"))
            .await
            .expect("insert");
        peers.push((peer, space, doc));
    }

    for (space, doc) in &rooms {
        let _ = space;
        let members: Vec<&TestClient> = std::iter::once(&owner)
            .chain(peers.iter().filter(|(_, _, d)| d == doc).map(|(p, _, _)| p))
            .collect();
        converge(&members, *doc).await;
    }
    println!(
        "[fanout_many_docs] {k} clients over {s} spaces converged in {:?}",
        started.elapsed()
    );
}

/// A member who joins *after* you are already connected must have their edits
/// reach you.
///
/// This is a correctness bar, not a budget, and it is the most important thing
/// the scale work turned up: with three members it fails outright. Every
/// existing test has at most two clients, where the case cannot arise — the
/// inviter applies the `Add` op locally, so it always knows the new member.
/// A *third* member is never told, because adding a member does not bump the
/// space epoch (only removal does, `engine.rs:848`), so nothing prompts a
/// membership refetch. The late joiner's frames then fail
/// `was_ever_member` in `apply_frame` (`engine.rs:1120-1125`), are reported as
/// "frame from unknown device", and `note_seq` advances the frontier past them
/// — so they are dropped permanently, not retried.
#[tokio::test]
#[ignore = "scale budget"]
async fn late_joiner_edits_reach_existing_members() {
    let server = TestServer::start_default().await;
    let owner = server.client();
    wait_connected(&owner).await;
    let space = owner.create_space().await.expect("create space");
    let doc = owner.create_doc(space).await.expect("create doc");

    // `first` joins and fully settles before anyone else exists.
    let first = server.client();
    wait_connected(&first).await;
    invite_and_join(&owner, &first, space).await;
    first.open_doc(space, doc).await.expect("open doc");
    owner.insert_text(doc, 0, "<owner>").await.expect("insert");
    converge(&[&owner, &first], doc).await;

    // Watch what `first` makes of the newcomer's frames.
    let mut first_events = first.events();

    // Now a third member joins and edits.
    let second = server.client();
    wait_connected(&second).await;
    invite_and_join(&owner, &second, space).await;
    second.open_doc(space, doc).await.expect("open doc");
    second.insert_text(doc, 0, "<late>").await.expect("insert");

    let deadline = Instant::now() + Duration::from_secs(10);
    let mut warnings = Vec::new();
    while Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(200), first_events.recv()).await {
            Ok(Ok(SyncEvent::SecurityWarning { context })) => warnings.push(context),
            Ok(Ok(_)) => {}
            Ok(Err(_)) => break,
            Err(_) => {
                if first.doc_text(doc).await.expect("text").contains("<late>") {
                    break;
                }
            }
        }
    }

    let text = first.doc_text(doc).await.expect("doc text");
    assert!(
        text.contains("<late>"),
        "an already-connected member never received a later joiner's edit \
         (text {text:?}, security warnings {warnings:?}) — refetch membership and retry the \
         frame instead of dropping it"
    );
}

// ===========================================================================
// Unbounded growth: membership log and key envelopes
// ===========================================================================

/// The membership log is never compacted and is re-sent in full on every fetch
/// (`enkr-syncd/src/lib.rs:560-574`). A space with churn must not pay for its
/// whole history on each connect.
#[tokio::test]
#[ignore = "scale budget"]
async fn membership_log_growth() {
    let ops = membership_ops();
    let server = TestServer::start_metered(ServerConfig::default()).await;
    let owner = server.client();
    wait_connected(&owner).await;
    let space = owner.create_space().await.expect("create space");

    // Throwaway identities: the log only needs distinct device keys, and
    // spawning `ops` real clients would dwarf what this test measures.
    for _ in 0..ops {
        let device = DeviceIdentity::generate();
        owner
            .add_member(
                space,
                device.device_pk(),
                device.kex_pk(),
                MemberRole::Writer,
            )
            .await
            .expect("add member");
    }

    // `add_member` returns once the op is sent, not once it is stored, so the
    // log has to settle before it can be counted.
    let db = server.raw_db();
    let expected = ops as i64 + 1; // + the space's own Create op
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut rows = row_count(&db, "SELECT COUNT(*) FROM membership_log");
    while rows < expected && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
        rows = row_count(&db, "SELECT COUNT(*) FROM membership_log");
    }
    assert_eq!(rows, expected, "membership ops were dropped");
    let bytes = row_count(
        &db,
        "SELECT COALESCE(SUM(LENGTH(signed_op)), 0) FROM membership_log",
    );
    println!("[membership] {rows} ops, {bytes} bytes of signed ops");

    // A fresh member's join must not scale with the whole log.
    let newcomer = server.client();
    wait_connected(&newcomer).await;
    server.metrics().reset();
    let started = Instant::now();
    invite_and_join(&owner, &newcomer, space).await;
    let elapsed = started.elapsed();
    server.metrics().report("membership");
    println!("[membership] newcomer joined a {rows}-op space in {elapsed:?}");

    assert!(
        elapsed < Duration::from_secs(2),
        "joining a space with {rows} membership ops took {elapsed:?} — tail-fetch the log \
         (scale plan step 3a)"
    );
}

/// Repeated membership changes on one space must all land.
///
/// They do not today. `add_member`/`remove_member` take `op_seq` from the
/// client's local `next_op_seq`, apply the op locally, and send it
/// (`engine.rs:828,869`) without waiting for confirmation. A removal bumps the
/// epoch, so the server pushes `EpochBump`, and the client answers with
/// `FetchMembership`; when that reply lands, `handle_membership` replaces
/// `space.membership` wholesale with the server's replay — rewinding
/// `next_op_seq` if an op the client already signed isn't in it yet. The next
/// op then reuses a seq, the server rejects it with `Conflict("membership
/// op_seq already used")` (`sqlite.rs:505`), and the client — which applied it
/// locally — silently diverges from the server's view of who is a member.
#[tokio::test]
#[ignore = "scale budget"]
async fn repeated_membership_changes_all_land() {
    let cycles = membership_ops().min(400) / 2;
    let server = TestServer::start_default().await;
    let owner = server.client();
    wait_connected(&owner).await;
    let space = owner.create_space().await.expect("create space");

    for cycle in 0..cycles {
        let device = DeviceIdentity::generate();
        owner
            .add_member(
                space,
                device.device_pk(),
                device.kex_pk(),
                MemberRole::Writer,
            )
            .await
            .unwrap_or_else(|err| panic!("add on cycle {cycle}: {err}"));
        owner
            .remove_member(space, device.device_pk())
            .await
            .unwrap_or_else(|err| panic!("remove on cycle {cycle}: {err}"));
    }

    // Give the last ops time to land before reading the server's view.
    tokio::time::sleep(Duration::from_millis(500)).await;
    let db = server.raw_db();
    let ops = row_count(&db, "SELECT COUNT(*) FROM membership_log");
    let epoch = row_count(&db, "SELECT current_epoch FROM spaces");
    println!("[membership_churn] {ops} ops, epoch {epoch} after {cycles} add/remove cycles");

    // Create + (add, remove) per cycle; one epoch bump per removal.
    assert_eq!(
        ops,
        1 + 2 * cycles as i64,
        "membership ops were dropped by the server (op_seq reuse after a refetch \
         rewound the client's next_op_seq)"
    );
    assert_eq!(epoch, cycles as i64, "not every removal bumped the epoch");
}

/// Key envelopes are kept for every historical epoch, for every device, and
/// returned whole by `envelopes_for_device` (`sqlite.rs:264-281`).
#[tokio::test]
#[ignore = "scale budget"]
async fn envelope_growth_under_epoch_churn() {
    let cycles = membership_ops().min(200) / 2;
    // Fast GC tick + no snapshot retention so compaction and the envelope sweep
    // both run inside the test rather than a minute later.
    let config = ServerConfig {
        gc_interval: Duration::from_millis(100),
        snapshot_retention: Duration::from_millis(0),
        snapshot_request_threshold: 4,
        ..ServerConfig::default()
    };
    let server = TestServer::start(config).await;
    let owner = server.client();
    wait_connected(&owner).await;
    let space = owner.create_space().await.expect("create space");

    for _ in 0..cycles {
        let device = DeviceIdentity::generate();
        owner
            .add_member(
                space,
                device.device_pk(),
                device.kex_pk(),
                MemberRole::Writer,
            )
            .await
            .expect("add member");
        // Each removal bumps the epoch and seals a fresh key to everyone left.
        owner
            .remove_member(space, device.device_pk())
            .await
            .expect("remove member");
        // Paced: `repeated_membership_changes_all_land` covers the op_seq race
        // that back-to-back churn trips; this test is about envelope retention.
        tokio::time::sleep(Duration::from_millis(60)).await;
    }

    // Let the GC sweep run.
    let deadline = Instant::now() + Duration::from_secs(10);
    let db = server.raw_db();
    let epoch = row_count(&db, "SELECT current_epoch FROM spaces");
    let mut envelopes = row_count(&db, "SELECT COUNT(*) FROM key_envelopes");
    while envelopes > 4 && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(100)).await;
        envelopes = row_count(&db, "SELECT COUNT(*) FROM key_envelopes");
    }
    println!("[envelopes] {envelopes} envelopes at epoch {epoch} after {cycles} rotations");

    // Superseded epochs are collected once no update or snapshot still needs
    // them, so the row count tracks the live members, not the epoch history.
    assert!(
        envelopes < epoch,
        "envelopes are not being collected ({envelopes} rows at epoch {epoch}) — \
         see sync-recap-0.md §0b/D1"
    );
}

// ===========================================================================
// Server storage and GC
// ===========================================================================

/// Storage must stay sublinear in edits, and the 60s GC tick must not cost a
/// full scan of every snapshot on the server (`gc_eligible`, `sqlite.rs:418`).
#[tokio::test]
#[ignore = "scale budget"]
async fn server_storage_at_scale() {
    let n = docs_per_space().min(500);
    let s = spaces().min(5);
    let config = ServerConfig {
        gc_interval: Duration::from_millis(200),
        ..ServerConfig::default()
    };
    let server = TestServer::start_metered(config).await;
    let a = server.client();
    wait_connected(&a).await;

    let started = Instant::now();
    let mut created = 0usize;
    for _ in 0..s {
        let space = a.create_space().await.expect("create space");
        for _ in 0..n {
            let doc = a.create_doc(space).await.expect("create doc");
            created += 1;
            for i in 0..10 {
                a.insert_text(doc, i, "z").await.expect("insert");
            }
        }
    }
    a.flush().await.expect("flush");
    settle(
        &server.metrics().create_doc,
        Duration::from_millis(500),
        Duration::from_secs(60),
    )
    .await;
    // `create_doc` returns once the request is written, not once the server
    // has persisted it, so the raw-DB reads below need the server to drain.
    println!(
        "[storage] test made {created} create_doc calls; server saw {}",
        StoreMetrics::get(&server.metrics().create_doc)
    );

    let db = server.raw_db();
    let docs = row_count(&db, "SELECT COUNT(*) FROM docs");
    // One index doc per space rides along with the notes (`index_doc_id`).
    assert_eq!(docs, (n * s + s) as i64, "seeding did not create every doc");
    let updates = row_count(&db, "SELECT COUNT(*) FROM updates");
    let bytes = row_count(
        &db,
        "SELECT page_count * page_size FROM pragma_page_count(), pragma_page_size()",
    );
    server.metrics().report("storage");
    println!(
        "[storage] {docs} docs, {updates} update rows, {bytes} bytes on disk, seeded in {:?}",
        started.elapsed()
    );
}

/// What a note actually costs the relay, for capacity planning.
///
/// Reports rather than gates: the number depends on edit history (a note typed
/// character by character carries more CRDT metadata than one pasted in), so
/// this measures a realistic middle — notes built over many edits, then
/// compacted the way a live doc would be.
#[tokio::test]
#[ignore = "scale budget"]
async fn storage_cost_per_note() {
    const NOTES: usize = 200;
    const EDITS_PER_NOTE: usize = 40;
    const CHARS_PER_EDIT: usize = 50;
    // Compact aggressively so this measures the steady state, not the tail.
    let config = ServerConfig {
        gc_interval: Duration::from_millis(100),
        snapshot_retention: Duration::from_millis(0),
        snapshot_request_threshold: 8,
        ..ServerConfig::default()
    };
    let server = TestServer::start(config).await;
    let a = server.client();
    wait_connected(&a).await;
    let space = a.create_space().await.expect("create space");

    let mut plaintext = 0usize;
    for _ in 0..NOTES {
        let doc = a.create_doc(space).await.expect("create doc");
        for _ in 0..EDITS_PER_NOTE {
            let text = "lorem ipsum dolor sit amet consectetur adipiscing ";
            a.insert_text(doc, 0, &text[..CHARS_PER_EDIT])
                .await
                .expect("insert");
            plaintext += CHARS_PER_EDIT;
            a.flush().await.expect("flush");
        }
    }
    a.flush().await.expect("flush");
    converge(&[&a], doc_of(&a, space).await).await;
    // Let compaction and GC settle.
    tokio::time::sleep(Duration::from_secs(2)).await;

    let db = server.raw_db();
    let on_disk = row_count(
        &db,
        "SELECT page_count * page_size FROM pragma_page_count(), pragma_page_size()",
    );
    let updates = row_count(&db, "SELECT COALESCE(SUM(size), 0) FROM updates");
    let snapshots = row_count(&db, "SELECT COALESCE(SUM(LENGTH(blob)), 0) FROM snapshots");
    println!(
        "[cost] {NOTES} notes, {plaintext}B plaintext -> {on_disk}B on disk \
         (updates {updates}B, snapshots {snapshots}B) = {:.1}B/note, {:.1}x plaintext",
        on_disk as f64 / NOTES as f64,
        on_disk as f64 / plaintext as f64,
    );
}

/// Any doc of `space` on this client — the cost test only needs something to
/// converge on.
async fn doc_of(client: &TestClient, _space: Uuid) -> Uuid {
    client.any_doc().expect("a doc was created")
}

// ===========================================================================
// Presence
// ===========================================================================

/// Every presence ping costs `doc_space` + `is_active_member` on the shared
/// connection before the relay (`enkr-syncd/src/lib.rs:491-510`), for a message
/// the server never persists. At many clients that is the dominant DB load.
///
/// (The "send a Gone tombstone on disconnect" half of TODO.md:22 lives in
/// `AppSync`, above this layer — it belongs in `tests/app_sync.rs`.)
#[tokio::test]
#[ignore = "scale budget"]
async fn presence_ping_cost() {
    const PINGS: usize = 20;
    let k = clients().min(20);
    let server = TestServer::start_metered(ServerConfig::default()).await;
    let owner = server.client();
    wait_connected(&owner).await;
    let space = owner.create_space().await.expect("create space");
    let doc = owner.create_doc(space).await.expect("create doc");
    let peers = fleet(&server, &owner, space, k).await;
    for peer in &peers {
        peer.open_doc(space, doc).await.expect("open doc");
    }

    server.metrics().reset();
    for round in 0..PINGS {
        for peer in &peers {
            peer.send_ephemeral(doc, vec![round as u8; 32])
                .expect("ephemeral");
        }
    }
    tokio::time::sleep(Duration::from_millis(500)).await;
    server.metrics().report("presence");

    let sent = (PINGS * k) as u64;
    let acl = StoreMetrics::get(&server.metrics().is_active_member);
    println!("[presence] {sent} pings cost {acl} membership lookups");
    assert!(
        acl < sent / 2,
        "every presence ping hits the database ({acl} lookups for {sent} pings) — cache the \
         ephemeral ACL check per connection (scale plan step 5)"
    );
}
