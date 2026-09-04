//! Accounts and per-account storage quota — the gate between a paying customer
//! and the relay's disk.
//!
//! ```text
//! cargo test -p enkr --test accounts -- --ignored --nocapture --test-threads=1
//! ```
//!
//! `#[ignore]`d like the scale and resilience suites: real sockets, real
//! SQLite files, real clocks.
//!
//! The property under test is not "writes are refused" — it is that a customer
//! who hits their limit is **read-only, not locked out**: they can still read,
//! export, and delete their way back under it. A relay that refuses everything
//! turns a billing problem into a support ticket, and a relay whose `used_bytes`
//! drifts upward locks out someone who never filled their quota at all.

mod harness;

use std::time::Duration;

use uuid::Uuid;

use harness::{TestServer, converge, invite_and_join, now_ms, wait_connected};

/// Big enough that the handful of bytes a doc's metadata costs never dominates,
/// small enough to fill in a few writes.
const SMALL_QUOTA: i64 = 64 * 1024;

/// How long to wait for a state change the client can't be asked about
/// directly (a refusal has no ack to await).
const SETTLE: Duration = Duration::from_millis(600);

/// Bytes that don't compress, so a quota test can't be defeated by the
/// encryption layer or SQLite doing something clever.
fn filler(len: usize, seed: u8) -> Vec<u8> {
    (0..len)
        .map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed))
        .collect()
}

/// Push blobs until the account is full, returning how many landed.
async fn fill_quota(client: &harness::TestClient, space: Uuid, chunk: usize) -> usize {
    let mut stored = 0;
    for i in 0..64 {
        let bytes = filler(chunk, i as u8);
        if client.put_blob(space, Uuid::new_v4(), bytes).await.is_err() {
            break;
        }
        stored += 1;
    }
    stored
}

/// A relay in hosted mode lets an account-less device *connect* but not create
/// a space of its own.
///
/// The gate is deliberately not at the handshake: an invited collaborator has
/// no token, and refusing them at auth would make sharing impossible on exactly
/// the relays people pay for (see `a_guest_syncs_and_bills_the_space_owner`).
/// Creating a space is the act that starts consuming somebody's storage, so
/// that is what needs an account.
#[tokio::test]
#[ignore = "accounts"]
async fn creating_a_space_needs_an_account() {
    let server = TestServer::start_requiring_accounts().await;
    let anon = server.client();
    wait_connected(&anon).await;

    // `create_space` is fire-and-forget on the client (the space exists locally
    // whether or not the relay keeps it), so asking the *client* whether it
    // succeeded proves nothing. The relay's own tables are the answer.
    anon.create_space().await.expect("local create");
    tokio::time::sleep(SETTLE).await;
    assert_eq!(
        server.space_count(),
        0,
        "an account-less device created a space on a relay that requires \
         accounts — storage nobody is paying for"
    );

    // ...and the same relay lets an account holder through.
    let (_id, token) = server.create_account("alice", SMALL_QUOTA, None).await;
    let alice = server.client_with_token(&token);
    wait_connected(&alice).await;
    alice.create_space().await.expect("account holder create");
    tokio::time::sleep(SETTLE).await;
    assert_eq!(
        server.space_count(),
        1,
        "an account holder could not create a space"
    );
}

/// An unknown or revoked token is refused outright, not silently downgraded to
/// "no account" — a customer whose payment lapsed must be told, not quietly
/// given a free relay.
#[tokio::test]
#[ignore = "accounts"]
async fn a_revoked_token_stops_working() {
    let server = TestServer::start_requiring_accounts().await;
    let (id, token) = server.create_account("bob", SMALL_QUOTA, None).await;

    let bob = server.client_with_token(&token);
    wait_connected(&bob).await;
    let space = bob.create_space().await.expect("create space");
    let doc = bob.create_doc(space).await.expect("create doc");
    bob.insert_text(doc, 0, "paid for").await.expect("insert");
    converge(&[&bob], doc).await;
    drop(bob);

    server.delete_account(id).await;

    let after = server.client_with_token(&token);
    tokio::time::sleep(SETTLE).await;
    let status = after.status().await.expect("status");
    assert!(
        !status.connected && status.rejected,
        "a revoked token still authenticates"
    );

    // A garbage token is refused the same way — no silent anonymous fallback.
    let bogus = server.client_with_token(&Uuid::new_v4().to_string());
    tokio::time::sleep(SETTLE).await;
    assert!(
        !bogus.status().await.expect("status").connected,
        "an unknown token authenticated"
    );
}

/// Writes are billed to the account and refused once it is full — but reads
/// keep working, and **deleting frees the quota back up**, which is the whole
/// reason over-quota is read-only rather than a hard stop.
#[tokio::test]
#[ignore = "accounts"]
async fn quota_refuses_writes_then_deleting_restores_them() {
    let server = TestServer::start_requiring_accounts().await;
    let (id, token) = server.create_account("carol", SMALL_QUOTA, None).await;
    let carol = server.client_with_token(&token);
    wait_connected(&carol).await;

    let space = carol.create_space().await.expect("create space");
    let doc = carol.create_doc(space).await.expect("create doc");
    carol
        .insert_text(doc, 0, "under quota")
        .await
        .expect("insert");
    converge(&[&carol], doc).await;

    let baseline = server.used_bytes(id);
    assert!(
        baseline > 0,
        "the account was billed nothing for a space, a doc and an edit — \
         accounting is not wired to the write path"
    );
    assert!(baseline < SMALL_QUOTA, "the setup already filled the quota");

    // Fill it with blobs, which are the only thing big enough to matter, then
    // top up with smaller ones. Shrinking chunks matter: the check is
    // `used + additional <= quota`, so an account with 7 KB free still legally
    // accepts a 200-byte edit. Leaving that headroom would make the "an edit is
    // refused" assertion below fail against perfectly correct behaviour.
    let chunk = 8 * 1024;
    let stored = fill_quota(&carol, space, chunk).await;
    assert!(
        stored > 0 && stored < 64,
        "expected the quota to stop the uploads somewhere in the middle, stored {stored}"
    );
    for smaller in [1024, 128, 16] {
        fill_quota(&carol, space, smaller).await;
    }
    let full = server.used_bytes(id);
    assert!(
        full > SMALL_QUOTA - 256,
        "stopped at {full} bytes, well under the {SMALL_QUOTA}-byte quota — \
         something other than the quota refused the writes"
    );

    // Read still works while over quota: this is the point.
    let text = carol.doc_text(doc).await.expect("read doc");
    assert_eq!(
        text, "under quota",
        "reads broke once the account filled up"
    );

    // A text edit is refused too, but non-destructively: the local replica keeps
    // it and the outbox holds it for when there is room again.
    carol
        .insert_text(doc, 0, "over!")
        .await
        .expect("local insert");
    tokio::time::sleep(SETTLE).await;
    let after_edit = server.used_bytes(id);
    assert!(
        after_edit <= full + 64,
        "an edit was billed ({full} -> {after_edit}) despite the account being full"
    );

    // Now delete our way back under, which must actually credit the account.
    let blobs: Vec<Uuid> = server
        .raw_db()
        .prepare("SELECT blob_id FROM blobs")
        .expect("prepare")
        .query_map([], |row| {
            let raw: Vec<u8> = row.get(0)?;
            Ok(Uuid::from_slice(&raw).expect("blob uuid"))
        })
        .expect("query")
        .collect::<Result<_, _>>()
        .expect("rows");
    assert!(!blobs.is_empty(), "no blobs were stored at all");
    for blob in &blobs {
        carol.delete_blob(space, *blob).expect("delete blob");
    }
    tokio::time::sleep(SETTLE).await;
    let freed = server.used_bytes(id);
    assert!(
        freed < full / 2,
        "deleting every blob freed almost nothing ({full} -> {freed}): \
         the customer is locked out of storage they no longer use"
    );

    // ...and writes work again, without reconnecting.
    let probe = filler(chunk, 200);
    carol
        .put_blob(space, Uuid::new_v4(), probe)
        .await
        .expect("a write after freeing space was still refused");
}

/// Quota admission must include the serialized BlobFrame envelope. A quota
/// smaller than one encoded tiny blob is still larger than its ciphertext;
/// checking ciphertext alone would incorrectly store it and overrun quota.
#[tokio::test]
#[ignore = "accounts"]
async fn quota_counts_encoded_blob_bytes() {
    let server = TestServer::start_requiring_accounts().await;
    let (id, token) = server.create_account("encoded", 40, None).await;
    let client = server.client_with_token(&token);
    wait_connected(&client).await;

    let space = client.create_space().await.expect("create space");
    let result = client.put_blob(space, Uuid::new_v4(), vec![7]).await;
    assert!(
        result.is_err(),
        "ciphertext-only quota check accepted a blob"
    );
    assert_eq!(
        server.used_bytes(id),
        0,
        "a rejected blob must not consume storage"
    );
}

/// A lapsed subscription is read-only, not a lockout: the customer can still
/// open their notes, export them, and delete things — they just cannot grow.
#[tokio::test]
#[ignore = "accounts"]
async fn an_expired_account_can_still_read_and_delete() {
    let server = TestServer::start_requiring_accounts().await;
    let (id, token) = server.create_account("dave", SMALL_QUOTA, None).await;
    // A persistent key: the reconnect below has to come back as the *same*
    // device, or it is not a member of the space it just created.
    let key_path = std::env::temp_dir().join(format!("enkr_dave_{}.key", Uuid::new_v4()));
    let dave = server.client_with_token_at(&token, key_path.clone());
    wait_connected(&dave).await;

    let space = dave.create_space().await.expect("create space");
    let doc = dave.create_doc(space).await.expect("create doc");
    dave.insert_text(doc, 0, "written while paid")
        .await
        .expect("insert");
    let blob = Uuid::new_v4();
    let blob_key = dave
        .put_blob(space, blob, filler(4096, 7))
        .await
        .expect("put blob");
    converge(&[&dave], doc).await;
    let while_paid = server.used_bytes(id);

    // Lapse it, then reconnect so the connection picks up the new expiry.
    // `allow_write` re-reads the account per write, so this reconnect is about
    // the client, not the server's cache.
    server.set_account_expiry(id, Some(now_ms() - 1)).await;
    drop(dave);
    let dave = server.client_with_token_at(&token, key_path);
    wait_connected(&dave).await;
    dave.join_space(space).await.expect("rejoin space");
    dave.open_doc(space, doc).await.expect("open doc");

    // Reads: fine. Polled, not read once — `open_doc` subscribes, it does not
    // wait for the backlog to arrive, so reading immediately measures the
    // round trip rather than the account's permissions.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        let text = dave.doc_text(doc).await.expect("read");
        if text == "written while paid" {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "an expired account could not read its own notes (got {text:?})"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    dave.get_blob(space, blob, blob_key)
        .await
        .expect("an expired account could not download its own image");

    // Writes: refused.
    let _ = dave.put_blob(space, Uuid::new_v4(), filler(4096, 9)).await;
    tokio::time::sleep(SETTLE).await;
    let after = server.used_bytes(id);
    assert!(
        after <= while_paid + 64,
        "an expired account grew storage ({while_paid} -> {after})"
    );

    // Deletes: fine, and credited.
    dave.delete_blob(space, blob).expect("delete blob");
    tokio::time::sleep(SETTLE).await;
    assert!(
        server.used_bytes(id) < while_paid,
        "an expired account could not free its own storage"
    );
}

/// A collaborator brings no account of their own and still syncs — their bytes
/// are billed to whoever owns the space. With no free tier, the alternative
/// would be making every invited guest pay, which would kill sharing.
#[tokio::test]
#[ignore = "accounts"]
async fn a_guest_syncs_and_bills_the_space_owner() {
    let server = TestServer::start_requiring_accounts().await;
    let (owner_id, token) = server.create_account("erin", 1024 * 1024, None).await;
    let owner = server.client_with_token(&token);
    wait_connected(&owner).await;

    let space = owner.create_space().await.expect("create space");
    let doc = owner.create_doc(space).await.expect("create doc");
    owner.insert_text(doc, 0, "hello ").await.expect("insert");
    converge(&[&owner], doc).await;

    // No token at all — on a relay that requires one for its own devices.
    let guest = server.client();
    invite_and_join(&owner, &guest, space).await;
    guest.open_doc(space, doc).await.expect("open doc");
    // Insert at index 6 below, so the guest has to actually have the six
    // characters first — otherwise it edits an empty replica and the merge is
    // correct but the text is not what the test claims.
    converge(&[&owner, &guest], doc).await;

    let before = server.used_bytes(owner_id);
    guest
        .insert_text(doc, 6, "from the guest")
        .await
        .expect("guest insert");
    converge(&[&owner, &guest], doc).await;

    assert_eq!(
        owner.doc_text(doc).await.expect("read"),
        "hello from the guest",
        "a guest with no account could not contribute"
    );
    let after = server.used_bytes(owner_id);
    assert!(
        after > before,
        "the guest's write ({before} -> {after}) was billed to nobody — \
         an unaccounted write path is a free relay for anyone with an invite"
    );
}

/// `used_bytes` is a running total maintained by hand at every write and every
/// delete, including GC, which removes rows from two tables and reports only a
/// count. Any missed decrement drifts the total upward forever and eventually
/// locks out a customer who never filled their quota.
///
/// So: churn hard, then check the running total against a from-scratch
/// recomputation of the same number.
#[tokio::test]
#[ignore = "accounts"]
async fn usage_does_not_drift_under_churn() {
    let server = TestServer::start_requiring_accounts().await;
    let (id, token) = server.create_account("frank", 64 * 1024 * 1024, None).await;
    let client = server.client_with_token(&token);
    wait_connected(&client).await;

    // Two spaces so a whole-space delete is exercised without ending the test.
    let keep = client.create_space().await.expect("create space");
    let scratch = client.create_space().await.expect("create space");

    // Enough edits to cross the snapshot threshold, so GC runs for real: it is
    // the compaction path that deletes updates *and* superseded snapshots.
    let mut docs = Vec::new();
    for _ in 0..3 {
        let doc = client.create_doc(keep).await.expect("create doc");
        for i in 0..250 {
            client
                .insert_text(doc, 0, &format!("{i} "))
                .await
                .expect("insert");
        }
        docs.push(doc);
    }
    for doc in &docs {
        converge(&[&client], *doc).await;
    }

    // Blobs added and removed, so the blob credits are exercised both ways.
    let mut blobs = Vec::new();
    for i in 0..6 {
        let blob = Uuid::new_v4();
        client
            .put_blob(keep, blob, filler(16 * 1024, i))
            .await
            .expect("put blob");
        blobs.push(blob);
    }
    for blob in blobs.iter().take(3) {
        client.delete_blob(keep, *blob).expect("delete blob");
    }

    // A doomed space: written to, then destroyed wholesale.
    let doomed = client.create_doc(scratch).await.expect("create doc");
    for i in 0..100 {
        client
            .insert_text(doomed, 0, &format!("{i} "))
            .await
            .expect("insert");
    }
    client
        .put_blob(scratch, Uuid::new_v4(), filler(32 * 1024, 3))
        .await
        .expect("put blob");
    converge(&[&client], doomed).await;
    client.delete_space(scratch).expect("delete space");

    // Let every deferred credit land before comparing.
    client.flush().await.expect("flush");
    tokio::time::sleep(Duration::from_secs(2)).await;

    let stored = server.used_bytes(id);
    let drift = server.recompute_usage().await;
    assert!(
        drift.is_empty(),
        "used_bytes drifted from what the rows actually hold: {drift:?} \
         (running total {stored}); a positive drift eventually locks out a \
         paying customer who never filled their quota"
    );
    assert!(
        stored > 0,
        "a churn workload billed the account nothing at all"
    );
}
