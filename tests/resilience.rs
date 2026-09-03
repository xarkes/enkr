//! Protocol and relay behaviour under failure (`sync-recap-0.md` §8).
//!
//! `tests/sync.rs` already covers clean failures — a server restart, tampered
//! frames, oversized frames, crash recovery. This file covers the messier ones:
//! links that stall instead of closing, herds of clients reconnecting at once,
//! background GC racing a client that is still catching up, and a relay whose
//! store is failing.
//!
//! ```text
//! cargo test -p enkr --test resilience -- --ignored --nocapture --test-threads=1
//! ```
//!
//! `#[ignore]`d like the scale suite: these drive real sockets and real clocks,
//! so they are slow and do not belong in the default run.

mod harness;

use std::time::{Duration, Instant};

use enkr_syncd::ServerConfig;
use uuid::Uuid;

use enkr::sync::MemberRole;
use enkr_proto::PROTOCOL_VERSION;
use harness::net::NetProxy;
use harness::{TestServer, converge, invite_and_join, invite_and_join_as, wait_connected};

/// How long a test will wait for a client to notice something is wrong.
const NOTICE_BUDGET: Duration = Duration::from_secs(30);

/// A link that stalls rather than closing must still be noticed.
///
/// This is the failure a real user actually hits — a laptop sleeping, a NAT
/// dropping the mapping, a network black hole. Nothing errors: the socket stays
/// open and bytes are accepted, they just never arrive. With no ping and no read
/// deadline anywhere (the only timeout in the client is the 5 s *connect*
/// timeout), the client goes on believing it is connected and silently stops
/// syncing — no error, no reconnect, just stale content.
#[tokio::test]
#[ignore = "resilience"]
async fn client_notices_a_stalled_link() {
    let server = TestServer::start_default().await;
    let proxy = NetProxy::start(server.addr, Duration::from_millis(5)).await;

    let a = server.client_at(proxy.url());
    wait_connected(&a).await;
    let space = a.create_space().await.expect("create space");
    let doc = a.create_doc(space).await.expect("create doc");
    a.insert_text(doc, 0, "before;").await.expect("insert");
    converge(&[&a], doc).await;

    // Black-hole the link. Nothing is closed, so nothing errors.
    proxy.stall();
    a.insert_text(doc, 0, "after;").await.expect("insert");

    let started = Instant::now();
    loop {
        let status = a.status().await.expect("status");
        if !status.connected {
            println!(
                "[stall] client noticed the dead link after {:?}",
                started.elapsed()
            );
            break;
        }
        assert!(
            started.elapsed() < NOTICE_BUDGET,
            "client still reports connected {:?} after the link went silent — \
             it has no liveness check, so it will never reconnect and never sync",
            started.elapsed()
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // ...and recovers once the link comes back.
    proxy.resume();
    let deadline = Instant::now() + NOTICE_BUDGET;
    loop {
        if a.status().await.expect("status").connected {
            break;
        }
        assert!(Instant::now() < deadline, "client never reconnected");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    converge(&[&a], doc).await;
}

/// The relay must reclaim connections whose client has gone silent, or a stalled
/// peer holds a session, a send queue and its room membership indefinitely.
#[tokio::test]
#[ignore = "resilience"]
async fn server_reclaims_a_silent_connection() {
    let config = ServerConfig {
        client_timeout: Duration::from_secs(2),
        ..ServerConfig::default()
    };
    let server = TestServer::start(config).await;
    let proxy = NetProxy::start(server.addr, Duration::from_millis(5)).await;

    let a = server.client_at(proxy.url());
    wait_connected(&a).await;
    let space = a.create_space().await.expect("create space");
    let doc = a.create_doc(space).await.expect("create doc");
    converge(&[&a], doc).await;

    proxy.stall();
    let started = Instant::now();
    let deadline = started + NOTICE_BUDGET;
    while server.live_connections() > 0 {
        assert!(
            Instant::now() < deadline,
            "relay still holds {} connection(s) {:?} after the client went silent",
            server.live_connections(),
            started.elapsed()
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    println!(
        "[stall] relay reclaimed the dead connection after {:?}",
        started.elapsed()
    );
}

/// An outage must not be followed by a synchronised reconnect herd.
///
/// The backoff doubled from `reconnect_min` to `reconnect_max` with no jitter,
/// so every client dropped by one outage retried in lockstep — the load spike
/// lands exactly when the relay is least able to absorb it.
///
/// The partition is driven through the proxy rather than by restarting the
/// relay: `TestServer::restart` brings the listener up inside the call, so
/// clients can reconnect before the measurement even starts, which makes every
/// arrival look simultaneous regardless of the backoff.
#[tokio::test]
#[ignore = "resilience"]
async fn reconnect_storm_is_spread_out() {
    const CLIENTS: usize = 30;
    let server = TestServer::start_default().await;
    let proxy = NetProxy::start(server.addr, Duration::from_millis(1)).await;

    let mut clients = Vec::with_capacity(CLIENTS);
    for _ in 0..CLIENTS {
        let client = server.client_at(proxy.url());
        wait_connected(&client).await;
        clients.push(client);
    }

    // Cut everyone off and keep them off long enough for the backoff to grow.
    proxy.partition();
    proxy.cut().await;
    tokio::time::sleep(Duration::from_secs(3)).await;
    let still_up = {
        let mut n = 0;
        for client in &clients {
            if client.status().await.expect("status").connected {
                n += 1;
            }
        }
        n
    };
    // Premise check: if the partition were not actually severing connections,
    // every client would still be online and the spread below would measure
    // nothing. (An earlier proxy bug did exactly that.)
    assert_eq!(
        still_up, 0,
        "the partition did not sever connections, so the measurement is meaningless"
    );

    let started = Instant::now();
    proxy.heal();

    let mut seen = vec![false; CLIENTS];
    let mut arrivals = Vec::with_capacity(CLIENTS);
    let deadline = started + NOTICE_BUDGET;
    while arrivals.len() < CLIENTS {
        for (i, client) in clients.iter().enumerate() {
            if seen[i] {
                continue;
            }
            if client.status().await.expect("status").connected {
                seen[i] = true;
                arrivals.push(started.elapsed());
            }
        }
        assert!(
            Instant::now() < deadline,
            "only {}/{CLIENTS} clients reconnected",
            arrivals.len()
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    arrivals.sort_unstable();
    let spread = arrivals[arrivals.len() - 1] - arrivals[0];
    println!(
        "[herd] {CLIENTS} clients reconnected over a {spread:?} window (first {:?}, last {:?})",
        arrivals[0],
        arrivals[arrivals.len() - 1]
    );
    assert!(
        spread > Duration::from_millis(200),
        "all {CLIENTS} clients reconnected within {spread:?} of each other — the backoff \
         has no jitter, so an outage is followed by a synchronised retry herd"
    );
}

/// Background GC must not strand a client that is still paging backlog.
///
/// `handle_subscribe` reads `latest_snapshot` once and then pages `updates_since`
/// in a loop. If `gc_updates_through` deletes rows between pages, the client
/// receives a gap in the seq sequence, parks the later frames in `ahead` waiting
/// for seqs that no longer exist, and the doc never goes live — a permanently
/// stalled document rather than a slow one.
#[tokio::test]
#[ignore = "resilience"]
async fn gc_during_catch_up_does_not_strand_a_subscriber() {
    // Compact and collect as aggressively as the config allows, so the sweep
    // lands in the middle of a cold subscribe rather than minutes later.
    let config = ServerConfig {
        gc_interval: Duration::from_millis(20),
        snapshot_retention: Duration::from_millis(0),
        snapshot_request_threshold: 8,
        backlog_page: 4,
        ..ServerConfig::default()
    };
    let server = TestServer::start(config).await;
    let a = server.client();
    wait_connected(&a).await;
    let space = a.create_space().await.expect("create space");
    let doc = a.create_doc(space).await.expect("create doc");

    // A long *frame* history: flush per edit so the backlog spans many pages.
    for i in 0..200 {
        a.insert_text(doc, 0, format!("{i};"))
            .await
            .expect("insert");
        a.flush().await.expect("flush");
    }
    converge(&[&a], doc).await;
    let expected = a.doc_text(doc).await.expect("text");

    // Repeatedly cold-subscribe while compaction and GC churn underneath.
    for round in 0..5 {
        let b = server.client();
        wait_connected(&b).await;
        invite_and_join(&a, &b, space).await;
        b.open_doc(space, doc).await.expect("open doc");
        let text = converge(&[&a, &b], doc).await;
        assert_eq!(
            text, expected,
            "round {round}: a subscriber that caught up while GC was running \
             ended with different content"
        );
    }
}

/// A failing store must not take the connection down with it.
///
/// Every store call in `handle_msg` used to propagate straight out of the
/// connection loop, so one transient database error — lock contention on the
/// single SQLite connection is the obvious source — dropped the client instead
/// of reporting the failure. With a whole fleet behind one store that turns a
/// blip into a reconnect storm on an already-struggling relay.
#[tokio::test]
#[ignore = "resilience"]
async fn a_failing_store_does_not_drop_the_connection() {
    let (server, hostility) = TestServer::start_hostile(ServerConfig::default()).await;
    let a = server.client();
    wait_connected(&a).await;
    let space = a.create_space().await.expect("create space");
    let doc = a.create_doc(space).await.expect("create doc");
    a.insert_text(doc, 0, "content;").await.expect("insert");
    converge(&[&a], doc).await;

    let b = server.client();
    wait_connected(&b).await;
    invite_and_join(&a, &b, space).await;

    // Every backlog read now fails.
    hostility.fail_backlog_reads(true);
    b.open_doc(space, doc).await.expect("open doc");
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(
        b.status().await.expect("status").connected,
        "a storage failure disconnected the client instead of being reported"
    );

    // Recovery: once the store is healthy the client catches up without a
    // reconnect being needed.
    hostility.fail_backlog_reads(false);
    b.open_doc(space, doc).await.expect("re-open doc");
    let text = converge(&[&a, &b], doc).await;
    assert!(text.contains("content;"), "did not recover: {text:?}");
}

/// Content is refused when the log the client holds says its author never had
/// write rights — even though the frames are perfectly signed and decryptable.
///
/// A reader holds the space key, so it can seal and sign a valid update or
/// snapshot; only the role says it may not, and the relay's ACL is a mirror the
/// relay maintains itself. So peers re-derive the role from the signed log and
/// refuse content on their own account. This drives that gate the only way an
/// honest client can be made to: the relay suppresses a *promotion*, so a
/// legitimate writer's frames reach a joiner whose log still shows the author
/// as read-only.
///
/// The refusal has to be a park, not a drop. Neither an `Add` nor a promotion
/// bumps the epoch, so a connected client has no signal to refetch the log and
/// a stale one is an ordinary race — discarding on it would diverge the replica
/// permanently over a member who is entirely legitimate.
#[tokio::test]
#[ignore = "resilience"]
async fn content_from_an_author_the_log_says_cannot_write_is_refused() {
    let (server, hostility) = TestServer::start_hostile(ServerConfig::default()).await;
    let owner = server.client();
    wait_connected(&owner).await;
    let space = owner.create_space().await.expect("create space");
    let doc = owner.create_doc(space).await.expect("create doc");
    owner.insert_text(doc, 0, "owner;").await.expect("insert");
    owner.flush().await.expect("flush");

    // Alice keeps her identity across the restart below, so she stays the same
    // device — and therefore the same author — throughout.
    let alice_key = std::env::temp_dir().join(format!("enkr-acl-alice-{}.key", Uuid::new_v4()));
    let alice = server.client_with_identity(alice_key.clone());
    wait_connected(&alice).await;
    invite_and_join_as(&owner, &alice, space, MemberRole::Reader).await; // op 1

    // The joiner learns the log while alice is still read-only.
    let joiner = server.client();
    wait_connected(&joiner).await;
    invite_and_join(&owner, &joiner, space).await; // op 2

    // Alice is promoted (op 3, the newest). Her own client only picks that up
    // on a fresh join — nothing pushes a role change to a connected member.
    owner
        .set_member_role(space, alice.device_pk(), MemberRole::Writer)
        .await
        .expect("promote");
    alice.shutdown().await;
    let alice = server.client_with_identity(alice_key.clone());
    wait_connected(&alice).await;
    alice.join_space(space).await.expect("rejoin");
    alice.open_doc(space, doc).await.expect("open doc");
    alice.insert_text(doc, 0, "alice;").await.expect("insert");
    alice.flush().await.expect("flush");
    converge(&[&owner, &alice], doc).await;

    // The relay now withholds the promotion, so the joiner's log — and every
    // refetch it makes — still says alice may only read.
    hostility.suppress_membership_ops(1);
    let mut events = joiner.events();
    joiner.open_doc(space, doc).await.expect("open doc");
    tokio::time::sleep(Duration::from_millis(1000)).await;

    let text = joiner.doc_text(doc).await.expect("doc text");
    assert!(
        !text.contains("alice;"),
        "applied content from an author the log shows as read-only: {text:?}"
    );
    let mut flagged = false;
    while let Ok(event) = events.try_recv() {
        if let enkr::sync::SyncEvent::SecurityWarning { context } = event
            && context.contains("no write rights")
        {
            flagged = true;
        }
    }
    assert!(flagged, "the refusal was silent; it must be surfaced");

    // Parked, not discarded: the honest log lets the same frames through.
    hostility.suppress_membership_ops(0);
    alice.insert_text(doc, 0, "alice2;").await.expect("insert");
    alice.flush().await.expect("flush");
    let text = converge(&[&owner, &alice, &joiner], doc).await;
    assert!(
        text.contains("alice;") && text.contains("alice2;"),
        "parked frames were not replayed once the log authorised them: {text:?}"
    );
    let _ = std::fs::remove_file(&alice_key);
}

/// A relay that hides the newest membership ops must not roll a client back.
///
/// `TODO.md:82` describes the attack: an actively malicious server suppresses
/// the latest `Remove`/`EpochBump` and serves a stale log, so a revoked member
/// still looks current to everyone else. There is no signed monotonic head over
/// the log, so a client cannot detect this in general — but it can refuse to go
/// *backwards* from what it has already seen, which is what the B2 fix
/// (`sync-recap-0.md` §0b) put in place.
///
/// The boundary this test draws: a connected client is protected; a client that
/// has never seen the newer log — a fresh join, or any restart, since membership
/// is not persisted — still has nothing to compare against and remains exposed.
#[tokio::test]
#[ignore = "resilience"]
async fn a_stale_membership_log_cannot_roll_a_client_back() {
    let (server, hostility) = TestServer::start_hostile(ServerConfig::default()).await;
    let owner = server.client();
    wait_connected(&owner).await;
    let space = owner.create_space().await.expect("create space");
    let doc = owner.create_doc(space).await.expect("create doc");

    let member = server.client();
    wait_connected(&member).await;
    invite_and_join_as(&owner, &member, space, MemberRole::Writer).await;
    member.open_doc(space, doc).await.expect("open doc");
    converge(&[&owner, &member], doc).await;

    // The owner revokes the member; the relay then hides that Remove and the
    // epoch bump that came with it.
    owner
        .remove_member(space, member.device_pk())
        .await
        .expect("remove member");
    tokio::time::sleep(Duration::from_millis(300)).await;
    hostility.suppress_membership_ops(1);

    // Force the owner to refetch: the truncated log it gets back must not undo
    // what it already knows.
    owner.join_space(space).await.expect("refetch membership");
    tokio::time::sleep(Duration::from_millis(300)).await;

    // `list_members` reports active members only, so the revoked device
    // reappearing here *is* the rollback.
    let members = owner.list_members(space).await.expect("list members");
    let still_active = members
        .iter()
        .any(|entry| entry.device_pk == member.device_pk());
    assert!(
        !still_active,
        "a suppressed Remove op resurrected a revoked member — the client accepted a \
         membership log shorter than the one it had already applied"
    );
}

/// A large backlog must not assemble an outbound message the transport rejects.
///
/// `backlog_page` bounds a page by *count* (256), and `MAX_MESSAGE_BYTES` is
/// enforced on the **read** path only, so a page of large frames could build a
/// `Backlog` message far bigger than anything either side would accept — the
/// relay writes it, the client's reader refuses it, and the connection dies on
/// every attempt to catch up. A doc that can never be read again.
///
/// Configured here so an uncapped page would exceed the frame limit several
/// times over: 20 frames of ~50 KiB against a 256 KiB ceiling.
#[tokio::test]
#[ignore = "resilience"]
async fn a_large_backlog_is_chunked_to_fit_the_frame_limit() {
    // Sized to overflow the *client's* read ceiling (`MAX_MESSAGE_BYTES`, 16 MiB,
    // fixed in the transport) when 256 frames land in one page: 250 x 70 KiB is
    // ~17 MiB. Nothing smaller reproduces it, because that ceiling — not the
    // server's configurable inbound guard — is what a Backlog message has to fit.
    const FRAMES: usize = 250;
    const FRAME_BYTES: usize = 70 * 1024;
    let config = ServerConfig {
        backlog_max_bytes: 1024 * 1024,
        ..ServerConfig::default()
    };
    let server = TestServer::start(config).await;
    let a = server.client();
    wait_connected(&a).await;
    let space = a.create_space().await.expect("create space");
    let doc = a.create_doc(space).await.expect("create doc");

    // Each flush becomes one sizeable frame in the log.
    for i in 0..FRAMES {
        let chunk = "x".repeat(FRAME_BYTES);
        a.insert_text(doc, 0, format!("<{i}>{chunk}"))
            .await
            .expect("insert");
        a.flush().await.expect("flush");
    }
    converge(&[&a], doc).await;
    let expected = a.doc_text(doc).await.expect("text");
    let stored = row_count(&server, "SELECT COUNT(*) FROM updates");
    let bytes = row_count(&server, "SELECT COALESCE(SUM(size), 0) FROM updates");
    println!("[backlog] {stored} frames, {bytes} bytes in the log");
    assert!(
        bytes > enkr_proto::wire::MAX_MESSAGE_BYTES as i64,
        "log is only {bytes} bytes — too small to overflow a single Backlog message"
    );

    // A cold subscriber must catch up over several bounded messages.
    let b = server.client();
    wait_connected(&b).await;
    invite_and_join(&a, &b, space).await;
    b.open_doc(space, doc).await.expect("open doc");
    let text = converge(&[&a, &b], doc).await;
    assert_eq!(
        text, expected,
        "a cold subscriber did not catch up over a large backlog"
    );
    assert!(
        b.status().await.expect("status").connected,
        "the connection did not survive serving a large backlog"
    );
}

fn row_count(server: &TestServer, sql: &str) -> i64 {
    server
        .raw_db()
        .query_row(sql, [], |row| row.get(0))
        .expect("count query")
}

/// A relay speaking a different wire version must fail the connection, loudly
/// and once — not retry forever behind "Connecting…".
///
/// The mismatch is detected server-side and answered in place of the
/// `Challenge`. The client used to read that only as "expected Challenge", which
/// is indistinguishable from a flaky relay, so it backed off and tried again
/// forever while the UI sat on "Connecting…" with nothing to act on.
#[tokio::test]
#[ignore = "resilience"]
async fn an_incompatible_relay_fails_the_connection() {
    let config = ServerConfig {
        protocol_version: PROTOCOL_VERSION.wrapping_add(1),
        ..ServerConfig::default()
    };
    let server = TestServer::start(config).await;
    let client = server.client();

    // The verdict must arrive, and must arrive as a *verdict* rather than as an
    // endless series of connection attempts.
    let deadline = Instant::now() + NOTICE_BUDGET;
    let versions = loop {
        let status = client.status().await.expect("status");
        assert!(!status.connected, "connected to an incompatible relay");
        if let Some(versions) = status.incompatible {
            break versions;
        }
        assert!(
            Instant::now() < deadline,
            "client never reported the version mismatch — it is still retrying, \
             which is what leaves the UI stuck on \"Connecting…\""
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    let (server_version, client_version) = versions;
    println!("[version] relay v{server_version}, client v{client_version}");
    assert_eq!(server_version, PROTOCOL_VERSION.wrapping_add(1));
    assert_eq!(client_version, PROTOCOL_VERSION);

    // And it stays failed rather than quietly resuming the retry loop.
    tokio::time::sleep(Duration::from_secs(1)).await;
    let status = client.status().await.expect("status");
    assert!(!status.connected);
    assert!(status.incompatible.is_some(), "the verdict was forgotten");
}

/// The same relay serves a client that *does* match, so the check rejects only
/// genuine mismatches rather than everything.
#[tokio::test]
#[ignore = "resilience"]
async fn a_matching_relay_still_connects() {
    let server = TestServer::start_default().await;
    let client = server.client();
    wait_connected(&client).await;
    assert!(
        client
            .status()
            .await
            .expect("status")
            .incompatible
            .is_none(),
        "a compatible relay was reported as incompatible"
    );
}

/// `/health` must reflect live state, not just return 200.
///
/// A relay you cannot ask "are you up, how many are connected, what is failing"
/// cannot be operated — and an endpoint that answers `ok` regardless is worse
/// than none, because it reports healthy while everything burns.
#[tokio::test]
#[ignore = "resilience"]
async fn health_reports_live_state() {
    let server = TestServer::start_default().await;
    let url = format!("http://{}/health", server.addr);

    let idle = http_get(&url).await;
    assert!(
        idle.contains("\"status\":\"ok\""),
        "unexpected body: {idle}"
    );
    assert!(
        idle.contains("\"connections\":0"),
        "should report no connections before anyone connects: {idle}"
    );

    let client = server.client();
    wait_connected(&client).await;
    let busy = http_get(&url).await;
    assert!(
        busy.contains("\"connections\":1"),
        "a connected client is not reflected: {busy}"
    );

    // Nothing sensitive: no space ids, device keys or user data.
    for leak in ["space", "device", "note"] {
        assert!(
            !busy.contains(leak),
            "health response mentions {leak:?}: {busy}"
        );
    }
    println!("[health] {busy}");
}

/// Minimal HTTP/1.1 GET — the test crate has no HTTP client, and pulling one in
/// for a single request is not worth the dependency.
async fn http_get(url: &str) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let rest = url.strip_prefix("http://").expect("http url");
    let (host, path) = rest.split_once('/').expect("path");
    let mut stream = tokio::net::TcpStream::connect(host).await.expect("connect");
    stream
        .write_all(
            format!("GET /{path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n").as_bytes(),
        )
        .await
        .expect("write request");
    let mut response = String::new();
    stream
        .read_to_string(&mut response)
        .await
        .expect("read response");
    response
}

/// A backup taken from a live relay restores, and brings back exactly what
/// losing the database would have cost.
///
/// Clients clear `needs_push` once acked, so they will **not** re-upload after
/// a relay is restored from nothing: existing devices keep working from their
/// local copies, but a *new* device gets an empty space and any content whose
/// only holder is offline is gone. That is the loss this test reproduces and
/// then repairs — the restore is verified by a device that has never seen the
/// content before, because a device that already has it locally proves nothing.
#[tokio::test]
#[ignore = "resilience"]
async fn a_backup_taken_live_restores_the_history() {
    let mut server = TestServer::start_default().await;
    // A persistent key, so the author can be closed and reopened as the same
    // device — which is what a real install is, and what makes it still the
    // space's owner after the outage.
    let key = std::env::temp_dir().join(format!("enkr_restore_key_{}.key", Uuid::new_v4()));
    let author = server.client_with_identity(key.clone());
    wait_connected(&author).await;
    let space = author.create_space().await.expect("create space");
    let doc = author.create_doc(space).await.expect("create doc");
    author
        .insert_text(doc, 0, "content-worth-keeping;")
        .await
        .expect("insert");
    converge(&[&author], doc).await;

    // Snapshot while the relay is still serving — the whole point is that this
    // does not require downtime.
    let backup = std::env::temp_dir().join(format!("enkr_backup_{}.sqlite3", Uuid::new_v4()));
    enkr_syncd::storage::backup_database(&server.db_path, &backup)
        .await
        .expect("backup a live database");
    assert!(
        std::fs::metadata(&backup).expect("backup exists").len() > 0,
        "backup file is empty"
    );
    // It refuses to clobber an existing snapshot rather than overwrite the only
    // copy of something.
    assert!(
        enkr_syncd::storage::backup_database(&server.db_path, &backup)
            .await
            .is_err(),
        "backup silently overwrote an existing file"
    );

    // Confirm the loss is real before claiming the restore fixed it: an empty
    // relay knows nothing about the space.
    //
    // On a throwaway relay, and started *before* this one is stopped. Two
    // reasons. Taking `author` through a relay that has forgotten it would
    // leave it holding membership state the server lacks, which cannot
    // currently be repaired by refetching (the reconciliation gap left open in
    // `sync-recap-0.md` §0b/D2) — that would be testing the gap, not the
    // backup. And binding port 0 only after `server` released its port lets the
    // OS hand out that very port, at which point `author` reconnects to the
    // empty relay and the rest of the test is measuring the wrong server.
    {
        let mut empty = TestServer::start_default().await;
        let stranded = empty.client();
        wait_connected(&stranded).await;
        assert!(
            stranded.join_space(space).await.is_err(),
            "an empty relay should not know this space — the test would prove nothing"
        );
        drop(stranded);
        empty.stop().await;
    }

    // Close the author before the outage. `TestServer::stop` shuts the listener
    // down gracefully, which leaves *established* WebSockets running — a client
    // left open would keep talking to the old server instance, still reading the
    // deleted database through its open file handle, and its writes would land
    // somewhere the restored relay never sees.
    drop(author);
    server.stop().await;
    std::fs::remove_file(&server.db_path).expect("destroy the database");
    let _ = std::fs::remove_file(server.db_path.with_extension("sqlite3-wal"));
    let _ = std::fs::remove_file(server.db_path.with_extension("sqlite3-shm"));

    // Restore: put the snapshot back where the database was.
    std::fs::copy(&backup, &server.db_path).expect("restore the backup");
    server.restart().await;

    // The owner comes back as the same device, and picks its space up from the
    // restored relay — membership log and key envelopes included, or it could
    // not decrypt anything below.
    let author = server.client_with_identity(key.clone());
    wait_connected(&author).await;
    author.join_space(space).await.expect("owner rejoins");

    // A device that has never seen this space reads the history back. It has to
    // be a newcomer: a client that already holds the content locally would
    // "pass" without the relay serving anything at all.
    let newcomer = server.client();
    wait_connected(&newcomer).await;
    invite_and_join(&author, &newcomer, space).await;
    newcomer.open_doc(space, doc).await.expect("open doc");
    // Polled rather than `converge`d: with a single client `converge` is
    // trivially satisfied — every replica agrees when there is only one — so it
    // returns before the backlog has arrived and would pass on an empty doc.
    let deadline = Instant::now() + NOTICE_BUDGET;
    loop {
        let text = newcomer.doc_text(doc).await.expect("doc text");
        if text.contains("content-worth-keeping;") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "restored relay did not serve the history: {text:?}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    let _ = std::fs::remove_file(&backup);
    let _ = std::fs::remove_file(&key);
}

/// One client cannot saturate the relay everyone else shares.
///
/// There were no quotas or rate limits of any kind, so a buggy loop or a
/// hostile client could push as fast as the socket allowed — an availability
/// incident for every paying customer on the box, not a nuisance.
#[tokio::test]
#[ignore = "resilience"]
async fn a_flooding_client_is_cut_off_and_others_keep_working() {
    let config = ServerConfig {
        messages_per_second: 20.0,
        message_burst: 40.0,
        ..ServerConfig::default()
    };
    let server = TestServer::start_strict(config).await;

    // A well-behaved neighbour, established before the flood.
    let neighbour = server.client();
    wait_connected(&neighbour).await;
    let space = neighbour.create_space().await.expect("create space");
    let doc = neighbour.create_doc(space).await.expect("create doc");
    neighbour
        .insert_text(doc, 0, "before;")
        .await
        .expect("insert");
    converge(&[&neighbour], doc).await;

    // A flood: far past the burst allowance, as fast as the socket takes it.
    let flooder = server.client();
    wait_connected(&flooder).await;
    let flood_space = flooder.create_space().await.expect("create space");
    let flood_doc = flooder.create_doc(flood_space).await.expect("create doc");
    for i in 0..400 {
        let _ = flooder.insert_text(flood_doc, 0, "x").await;
        let _ = flooder.flush().await;
        if i % 20 == 0 && !flooder.status().await.expect("status").connected {
            break;
        }
    }

    let deadline = Instant::now() + NOTICE_BUDGET;
    while flooder.status().await.expect("status").connected {
        assert!(
            Instant::now() < deadline,
            "a client flooding well past its allowance was never cut off"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    println!("[rate] flooding client was disconnected");

    // The neighbour is unaffected — that is the whole point of the cap.
    neighbour
        .insert_text(doc, 0, "after;")
        .await
        .expect("insert");
    let text = converge(&[&neighbour], doc).await;
    assert!(
        text.contains("after;") && text.contains("before;"),
        "the neighbour was collateral damage: {text:?}"
    );
    assert!(
        neighbour.status().await.expect("status").connected,
        "the neighbour was disconnected by someone else's flood"
    );
}

/// One device cannot hold unbounded connections.
#[tokio::test]
#[ignore = "resilience"]
async fn a_device_cannot_open_unbounded_connections() {
    const CAP: usize = 3;
    let config = ServerConfig {
        max_connections_per_device: CAP,
        ..ServerConfig::default()
    };
    let server = TestServer::start_strict(config).await;

    // The same device key each time, so these all count against one slot pool.
    let key = std::env::temp_dir().join(format!("enkr_cap_key_{}.key", Uuid::new_v4()));
    let mut clients = Vec::new();
    for _ in 0..CAP {
        let client = server.client_with_identity(key.clone());
        wait_connected(&client).await;
        clients.push(client);
    }
    assert_eq!(server.live_connections() as usize, CAP);

    // One more from the same device is refused, and stays refused.
    let extra = server.client_with_identity(key.clone());
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(
        server.live_connections() as usize,
        CAP,
        "the relay accepted more connections than the per-device cap"
    );

    // A *different* device is unaffected: the cap is per device, not global.
    let other = server.client();
    wait_connected(&other).await;
    assert_eq!(server.live_connections() as usize, CAP + 1);

    // Releasing a slot lets the device back in, so the count is not a leak.
    drop(extra);
    clients.pop();
    let deadline = Instant::now() + NOTICE_BUDGET;
    let reconnected = server.client_with_identity(key.clone());
    while server.live_connections() as usize != CAP + 1 {
        assert!(
            Instant::now() < deadline,
            "a released slot was never reusable ({} live)",
            server.live_connections()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    drop(reconnected);
    let _ = std::fs::remove_file(&key);
}

/// Quitting must end the connection with a WebSocket closing handshake.
///
/// A client that simply vanishes is indistinguishable from one that crashed:
/// the relay logs `connection closed with error: WebSocket protocol error:
/// Connection reset without closing handshake` and counts a failed connection
/// — so the one metric that is supposed to mean "something is wrong" fires
/// every single time a user closes the app, and stops meaning anything.
#[tokio::test]
#[ignore = "resilience"]
async fn quitting_closes_the_connection_with_a_handshake() {
    let server = TestServer::start_default().await;
    let health = format!("http://{}/health", server.addr);

    let client = server.client();
    wait_connected(&client).await;
    client.shutdown().await;

    // The relay tears the connection down asynchronously, so wait for it to be
    // gone before reading the failure counter reported alongside it.
    let deadline = Instant::now() + NOTICE_BUDGET;
    loop {
        let body = http_get(&health).await;
        if body.contains("\"connections\":0") {
            assert!(
                body.contains("\"failed_connections\":0"),
                "a graceful quit was counted as a failed connection: {body}"
            );
            return;
        }
        assert!(
            Instant::now() < deadline,
            "the connection never closed: {body}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}
