//! Sync protocol scenarios (PLAN.md §9).
//!
//! The in-process server + client harness these scenarios drive lives in
//! `tests/harness/`, shared with `tests/scale.rs`.

mod harness;

use std::time::Duration;

use enkr::sync::{MemberEntry, MemberRole, SyncError, SyncEvent, index_doc_id};
use enkr_syncd::ServerConfig;
use uuid::Uuid;

use harness::{
    CONVERGE_TIMEOUT, TestClient, TestServer, converge, invite_and_join, invite_and_join_as,
    space_with_two_clients, wait_connected,
};

// ===========================================================================
// M1: plaintext-equivalent sync core (now with crypto always on)
// ===========================================================================

#[tokio::test]
async fn two_clients_live_edit_converges() {
    let server = TestServer::start_default().await;
    let (a, b, _space, doc) = space_with_two_clients(&server).await;

    a.insert_text(doc, 0, "hello").await.unwrap();
    let text = converge(&[&a, &b], doc).await;
    assert_eq!(text, "hello");

    b.insert_text(doc, 5, " world").await.unwrap();
    let text = converge(&[&a, &b], doc).await;
    assert_eq!(text, "hello world");
}

/// A blob id is a global storage key. Reusing it for different sealed content
/// must fail instead of acknowledging the second upload while retaining the
/// first bytes.
#[tokio::test]
async fn blob_id_collision_is_rejected_end_to_end() {
    let server = TestServer::start_default().await;
    let owner = server.client();
    wait_connected(&owner).await;
    let space = owner.create_space().await.unwrap();
    let blob = Uuid::new_v4();

    owner
        .put_blob(space, blob, b"first-content".to_vec())
        .await
        .unwrap();
    let second = owner
        .put_blob(space, blob, b"different-content".to_vec())
        .await;
    assert!(second.is_err(), "colliding blob id was acknowledged");
}

/// A corrupt backlog frame must not retire its sequence number. Once the
/// relay serves the intact frame on resync, the client must still apply it.
#[tokio::test]
async fn corrupt_backlog_frame_is_recovered_by_resync() {
    let (server, hostility) = TestServer::start_hostile(ServerConfig::default()).await;
    let owner = server.client();
    wait_connected(&owner).await;
    let space = owner.create_space().await.unwrap();
    let doc = owner.create_doc(space).await.unwrap();

    owner.insert_text(doc, 0, "recoverable").await.unwrap();
    owner.flush().await.unwrap();
    assert_eq!(converge(&[&owner], doc).await, "recoverable");

    // Make the first cold-subscribe response contain a wire-valid but
    // cryptographically invalid copy of sequence 1.
    hostility.corrupt_update_once(1);
    let joiner = server.client();
    wait_connected(&joiner).await;
    invite_and_join(&owner, &joiner, space).await;
    joiner.open_doc(space, doc).await.unwrap();

    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(joiner.doc_text(doc).await.unwrap(), "");

    // The one-shot hostile response is now disabled. Resync must ask for seq 1
    // again; the receive path must not have retired the rejected sequence.
    joiner.resync().unwrap();
    assert_eq!(converge(&[&owner, &joiner], doc).await, "recoverable");
}

/// A removed writer's valid old-epoch frame must not be accepted as new
/// content when a hostile relay replays it after revocation.
#[tokio::test]
async fn removed_writer_frame_is_rejected_by_peers() {
    let (server, hostility) = TestServer::start_hostile(ServerConfig::default()).await;
    let owner = server.client();
    let removed = server.client();
    let peer = server.client();
    wait_connected(&owner).await;
    wait_connected(&removed).await;
    wait_connected(&peer).await;

    let space = owner.create_space().await.unwrap();
    let doc = owner.create_doc(space).await.unwrap();
    invite_and_join(&owner, &removed, space).await;
    invite_and_join(&owner, &peer, space).await;
    removed.open_doc(space, doc).await.unwrap();
    peer.open_doc(space, doc).await.unwrap();

    removed.insert_text(doc, 0, "before-removal").await.unwrap();
    converge(&[&owner, &removed, &peer], doc).await;

    owner
        .remove_member(space, removed.identity_pk())
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Re-offer the already-stored frame under a fresh sequence number. The
    // signature and old epoch are valid, but its author is now revoked.
    hostility.replay_update_once_as(2);
    let mut events = peer.events();
    peer.resync().unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;

    let mut content_events = 0;
    while let Ok(event) = events.try_recv() {
        if matches!(event, SyncEvent::DocBytes { .. }) {
            content_events += 1;
        }
    }
    assert_eq!(
        content_events, 0,
        "peer accepted replayed content from a removed writer"
    );
    assert_eq!(peer.doc_text(doc).await.unwrap(), "before-removal");
}

/// An accountless authenticated key is only an invited collaborator identity;
/// it must not create durable relay state merely by connecting. Otherwise a
/// client can exhaust the database by reconnecting with fresh keypairs.
#[tokio::test]
async fn accountless_connections_do_not_register_devices() {
    let server = TestServer::start_default().await;
    let client = server.client();
    wait_connected(&client).await;

    let identities: i64 = server
        .raw_db()
        .query_row("SELECT COUNT(*) FROM identities", [], |row| row.get(0))
        .unwrap();
    assert_eq!(
        identities, 0,
        "an accountless handshake grew the identities table"
    );
}

#[tokio::test]
async fn blob_uploads_and_downloads_across_clients() {
    let server = TestServer::start_default().await;
    let (a, b, space, _doc) = space_with_two_clients(&server).await;

    let blob = Uuid::new_v4();
    let bytes = b"\x89PNG\r\n\x1a\n fake image payload".to_vec();
    let key = a.put_blob(space, blob, bytes.clone()).await.unwrap();

    // The other member fetches + decrypts the same bytes, using the content key
    // it would have read from the space index doc.
    let fetched = b.get_blob(space, blob, key.clone()).await.unwrap();
    assert_eq!(fetched.as_deref(), Some(bytes.as_slice()));

    // An unknown id resolves to None rather than erroring.
    assert_eq!(b.get_blob(space, Uuid::new_v4(), key).await.unwrap(), None);
}

/// A blob stays readable across an epoch rotation, using only its own content
/// key — no space epoch key is consulted to open it.
///
/// This is the property that lets old key envelopes be collected at all
/// (`sync-recap-0.md` §0b/D1). Blob bytes are never re-sealed, so while the blob
/// key was derived from the space key every blob permanently pinned the epoch it
/// was uploaded under, and GC'ing that epoch's envelopes would have silently
/// destroyed the attachment. Now the key travels in the space index doc, which
/// *is* re-sealed under the current epoch by ordinary snapshot compaction.
#[tokio::test]
async fn blob_survives_epoch_rotation() {
    let server = TestServer::start_default().await;
    let (a, b, space, _doc) = space_with_two_clients(&server).await;

    let blob = Uuid::new_v4();
    let bytes = b"\x89PNG\r\n\x1a\n pre-rotation image".to_vec();
    let key = a.put_blob(space, blob, bytes.clone()).await.unwrap();

    // Rotate: adding then removing a device bumps the space epoch and reseals
    // the space key, leaving the blob sealed under a superseded epoch.
    let evictee = server.client();
    wait_connected(&evictee).await;
    invite_and_join(&a, &evictee, space).await;
    a.remove_member(space, evictee.identity_pk()).await.unwrap();

    // `remove_member` returns once the op is sent, not once it is persisted.
    let deadline = tokio::time::Instant::now() + CONVERGE_TIMEOUT;
    loop {
        let epoch: i64 = server
            .raw_db()
            .query_row("SELECT current_epoch FROM spaces", [], |row| row.get(0))
            .expect("space epoch");
        if epoch == 1 {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "removal never bumped the epoch (still {epoch})"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // A remaining member still opens the blob with the key from the index doc.
    let fetched = b.get_blob(space, blob, key).await.unwrap();
    assert_eq!(fetched.as_deref(), Some(bytes.as_slice()));
}

/// A frame limit *below* the client's blob pre-check (what an nginx proxy in
/// front of the relay effectively imposes) makes the relay close the connection
/// on "frame too large" rather than return a graceful `BlobTooLarge`. The client
/// therefore sees a plain `Disconnected`, not a permanent error - so the app
/// layer must not treat it as endlessly retriable (see the reship quarantine).
#[tokio::test]
async fn blob_over_transport_frame_limit_reports_disconnect_not_too_large() {
    let mut config = ServerConfig::default();
    config.max_frame_bytes = 64 * 1024;
    let server = TestServer::start(config).await;
    let a = server.client();
    wait_connected(&a).await;
    let space = a.create_space().await.unwrap();

    // Over the server/proxy frame limit, but well under the client's 16 MiB
    // pre-check, so the client ships it and the relay closes the connection.
    let bytes = vec![0u8; 256 * 1024];
    let result = a.put_blob(space, Uuid::new_v4(), bytes).await;
    assert_eq!(result.err(), Some(SyncError::Disconnected));
}

/// Regression: a multi-megabyte image (well under `MAX_BLOB_BYTES` but far over
/// the old 1 MiB frame default) must upload and round-trip. Previously the
/// server rejected the frame as "too large" and tore the connection down.
#[tokio::test]
async fn large_blob_uploads_and_downloads() {
    let server = TestServer::start_default().await;
    let (a, b, space, _doc) = space_with_two_clients(&server).await;

    let blob = Uuid::new_v4();
    let bytes: Vec<u8> = (0..4 * 1024 * 1024).map(|i| (i % 251) as u8).collect();
    let key = a.put_blob(space, blob, bytes.clone()).await.unwrap();

    let fetched = b.get_blob(space, blob, key).await.unwrap();
    assert_eq!(fetched.as_deref(), Some(bytes.as_slice()));
}

/// A blob whose sealed size exceeds `MAX_BLOB_BYTES` must fail permanently with
/// `BlobTooLarge` *without* wedging the connection — the client stays usable and
/// a normal blob still uploads afterwards (no poison-message reconnect loop).
#[tokio::test]
async fn oversized_blob_fails_without_wedging_connection() {
    let server = TestServer::start_default().await;
    let (a, b, space, _doc) = space_with_two_clients(&server).await;

    let huge = Uuid::new_v4();
    let bytes = vec![0u8; enkr_proto::wire::MAX_BLOB_BYTES + 1];
    assert_eq!(
        a.put_blob(space, huge, bytes).await.err(),
        Some(SyncError::BlobTooLarge),
    );

    // The connection survived: a normal blob still round-trips.
    let ok = Uuid::new_v4();
    let payload = b"still-working".to_vec();
    let key = a.put_blob(space, ok, payload.clone()).await.unwrap();
    assert_eq!(
        b.get_blob(space, ok, key).await.unwrap().as_deref(),
        Some(payload.as_slice()),
    );
}

/// Regression: a single doc update larger than the transport frame ceiling
/// (a multi-megabyte paste) must not wedge sync. The old behaviour sealed it
/// into a `PushUpdate`, the server closed the connection as "frame too large",
/// and the outbox re-sent it on every reconnect - a permanent connect/disconnect
/// loop. The engine must now drop it and keep the link usable for everything else.
#[tokio::test]
async fn oversized_doc_update_does_not_wedge_connection() {
    let server = TestServer::start_default().await;
    let (a, b, space, doc_ok) = space_with_two_clients(&server).await;

    // A second doc, established on both sides before the stress (seed + flush so
    // the server knows it before b subscribes).
    let doc_big = a.create_doc(space).await.unwrap();
    a.insert_text(doc_big, 0, "seed").await.unwrap();
    a.flush().await.unwrap();
    b.open_doc(space, doc_big).await.unwrap();
    converge(&[&a, &b], doc_big).await;

    // A pathological paste: one edit larger than the frame ceiling. The engine
    // must drop it (it stays applied locally) instead of shipping a frame the
    // server rejects - which would tear the link down and re-send from the
    // outbox on every reconnect.
    let huge = "x".repeat(enkr_proto::wire::MAX_MESSAGE_BYTES + 1);
    a.insert_text(doc_big, 4, huge).await.unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        a.status().await.unwrap().connected,
        "oversized update wedged the connection",
    );

    // The link stays usable: a normal edit on the other doc still syncs a -> b.
    a.insert_text(doc_ok, 0, "still works").await.unwrap();
    assert_eq!(converge(&[&a, &b], doc_ok).await, "still works");
}

#[tokio::test]
async fn concurrent_edits_converge() {
    let server = TestServer::start_default().await;
    let (a, b, _space, doc) = space_with_two_clients(&server).await;

    for i in 0..20 {
        a.insert_text(doc, 0, format!("a{i};")).await.unwrap();
        b.insert_text(doc, 0, format!("b{i};")).await.unwrap();
    }
    let text = converge(&[&a, &b], doc).await;
    for i in 0..20 {
        assert!(text.contains(&format!("a{i};")), "missing a{i} in {text:?}");
        assert!(text.contains(&format!("b{i};")), "missing b{i} in {text:?}");
    }
}

/// Property-style test: random op interleavings (seeded LCG) must converge.
#[tokio::test]
async fn randomized_interleavings_converge() {
    let server = TestServer::start_default().await;
    for seed in [0xdecafbad_u64, 0x5eed, 42] {
        let (a, b, _space, doc) = space_with_two_clients(&server).await;
        let mut rng = seed;
        let mut next = move || {
            rng = rng
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            rng >> 33
        };
        for i in 0..40 {
            let client = if next() % 2 == 0 { &a } else { &b };
            let len = client.doc_text(doc).await.unwrap().chars().count() as u64;
            let pos = if len == 0 { 0 } else { next() % (len + 1) } as u32;
            client
                .insert_text(doc, pos, format!("[{i}]"))
                .await
                .unwrap();
            if next() % 4 == 0 {
                tokio::time::sleep(Duration::from_millis((next() % 30) as u64)).await;
            }
        }
        // Inserts at random positions may split earlier markers, so presence
        // of "[i]" isn't guaranteed — but convergence (checked above) plus
        // zero content loss is: every inserted character must survive.
        let text = converge(&[&a, &b], doc).await;
        let expected_len: usize = (0..40).map(|i| format!("[{i}]").len()).sum();
        assert_eq!(
            text.len(),
            expected_len,
            "seed {seed}: content lost or duplicated: {text:?}"
        );
        a.shutdown().await;
        b.shutdown().await;
    }
}

// ===========================================================================
// Chaos: disconnects, server restart, offline catch-up (M1 accept criteria)
// ===========================================================================

#[tokio::test]
async fn offline_edits_survive_server_restart_and_converge() {
    let mut server = TestServer::start_default().await;
    let (a, b, _space, doc) = space_with_two_clients(&server).await;

    a.insert_text(doc, 0, "before;").await.unwrap();
    converge(&[&a, &b], doc).await;

    // Server goes away mid-session; both clients keep editing offline.
    server.stop().await;
    a.insert_text(doc, 0, "a-offline;").await.unwrap();
    b.insert_text(doc, 0, "b-offline;").await.unwrap();
    a.flush().await.unwrap();
    b.flush().await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;

    server.restart().await;
    let text = converge(&[&a, &b], doc).await;
    assert!(text.contains("before;"));
    assert!(text.contains("a-offline;"));
    assert!(text.contains("b-offline;"));
}

// ===========================================================================
// M2: encryption — server dumbness, zero plaintext, tamper rejection
// ===========================================================================

/// Dumb-server invariant: random bytes as ciphertext must be stored,
/// sequenced and relayed without error or interpretation.
#[tokio::test]
async fn dumb_server_stores_and_relays_random_bytes() {
    use enkr_proto::crypto::Identity;
    use enkr_proto::membership::{MembershipOp, MembershipOpKind, sign_op};
    use enkr_proto::wire::*;
    use enkr_proto::{PROTOCOL_VERSION, crypto, wire};
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;

    let server = TestServer::start_default().await;

    async fn raw_conn(
        url: &str,
        dev: &Identity,
    ) -> impl futures_util::Sink<Message, Error = tokio_tungstenite::tungstenite::Error>
    + futures_util::Stream<Item = Result<Message, tokio_tungstenite::tungstenite::Error>>
    + Unpin {
        let (mut ws, _) = tokio_tungstenite::connect_async(url)
            .await
            .expect("connect");
        let hello = ClientMsg::Hello {
            identity_pk: dev.identity_pk(),
            kex_pk: dev.kex_pk(),
            protocol_version: PROTOCOL_VERSION,
        };
        ws.send(Message::Binary(wire::encode(&hello).unwrap().into()))
            .await
            .unwrap();
        let challenge = recv_msg(&mut ws).await;
        let ServerMsg::Challenge { nonce, server_id } = challenge else {
            panic!("expected challenge, got {challenge:?}");
        };
        let sig = dev.sign(&crypto::auth_signing_bytes(&nonce, &server_id));
        ws.send(Message::Binary(
            wire::encode(&ClientMsg::Auth {
                sig: sig.to_vec(),
                account_token: None,
            })
            .unwrap()
            .into(),
        ))
        .await
        .unwrap();
        let ok = recv_msg(&mut ws).await;
        assert!(matches!(ok, ServerMsg::AuthOk { .. }), "got {ok:?}");
        ws
    }

    async fn recv_msg<S>(ws: &mut S) -> ServerMsg
    where
        S: futures_util::Stream<Item = Result<Message, tokio_tungstenite::tungstenite::Error>>
            + Unpin,
    {
        loop {
            let msg = tokio::time::timeout(Duration::from_secs(5), ws.next())
                .await
                .expect("server reply timeout")
                .expect("stream open")
                .expect("ws ok");
            if let Message::Binary(bytes) = msg {
                return wire::decode::<ServerMsg>(&bytes).expect("decodable");
            }
        }
    }

    let dev = Identity::generate();
    let url = server.url();
    let mut ws = raw_conn(&url, &dev).await;

    // Create a space + doc with a *valid* membership op (ACL is real), then
    // push frames whose ciphertext/sig are pure noise.
    let space = Uuid::new_v4();
    let doc = Uuid::new_v4();
    let op = MembershipOp {
        space_id: space,
        op_seq: 0,
        kind: MembershipOpKind::Create {
            creator_kex: dev.kex_pk(),
            // This space is never opened by a real client — the point is that
            // the relay stores and relays opaque noise — so the commitment
            // binds nothing anybody here will check.
            key_commitment: [0u8; 32],
        },
    };
    let signed = sign_op(&dev, &op).unwrap();
    for msg in [
        ClientMsg::CreateSpace {
            space_id: space,
            signed_op: signed,
            envelopes: vec![],
        },
        ClientMsg::CreateDoc {
            space_id: space,
            doc_id: doc,
        },
    ] {
        ws.send(Message::Binary(wire::encode(&msg).unwrap().into()))
            .await
            .unwrap();
    }

    let noise: Vec<u8> = (0..512u32)
        .map(|i| (i.wrapping_mul(2654435761) >> 13) as u8)
        .collect();
    let frame = UpdateFrame {
        doc_id: doc,
        epoch: 7,
        author_identity: dev.identity_pk(),
        nonce: [0xAB; 24],
        ciphertext: noise.clone(),
        sig: vec![0xCD; 64], // garbage signature: the server must not care
    };
    ws.send(Message::Binary(
        wire::encode(&ClientMsg::PushUpdate {
            doc_id: doc,
            client_tag: 1,
            frame: frame.clone(),
        })
        .unwrap()
        .into(),
    ))
    .await
    .unwrap();
    let ack = recv_msg(&mut ws).await;
    assert!(
        matches!(
            ack,
            ServerMsg::Ack {
                seq: 1,
                client_tag: 1,
                ..
            }
        ),
        "expected Ack for noise frame, got {ack:?}"
    );

    // A second identity subscribing gets the identical noise back.
    let dev2 = Identity::generate();
    let url2 = server.url();
    let op2 = MembershipOp {
        space_id: space,
        op_seq: 1,
        kind: MembershipOpKind::Add {
            identity_pk: dev2.identity_pk(),
            kex_pk: dev2.kex_pk(),
            role: enkr_proto::membership::MemberRole::Writer,
        },
    };
    let signed2 = sign_op(&dev, &op2).unwrap();
    ws.send(Message::Binary(
        wire::encode(&ClientMsg::AddMember {
            space_id: space,
            signed_op: signed2,
            envelopes: vec![],
        })
        .unwrap()
        .into(),
    ))
    .await
    .unwrap();

    let mut ws2 = raw_conn(&url2, &dev2).await;
    // The AddMember above races with this second connection: retry the
    // subscribe until the membership write is visible.
    let frames = loop {
        ws2.send(Message::Binary(
            wire::encode(&ClientMsg::Subscribe {
                entries: vec![SubscribeEntry {
                    doc_id: doc,
                    have_seq: 0,
                }],
            })
            .unwrap()
            .into(),
        ))
        .await
        .unwrap();
        match recv_msg(&mut ws2).await {
            ServerMsg::Backlog { frames, .. } => break frames,
            ServerMsg::Error {
                code: ErrorCode::NotMember,
                ..
            } => {
                tokio::time::sleep(Duration::from_millis(30)).await;
            }
            other => panic!("expected Backlog, got {other:?}"),
        }
    };
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0].0, 1);
    assert_eq!(frames[0].1, frame, "noise must round-trip bit-identical");
}

/// The server's entire database must contain zero plaintext.
#[tokio::test]
async fn server_storage_contains_no_plaintext() {
    let mut server = TestServer::start_default().await;
    let (a, b, _space, doc) = space_with_two_clients(&server).await;

    const MARKER: &str = "TOP-SECRET-PLAINTEXT-MARKER-0xDEADBEEF";
    a.insert_text(doc, 0, MARKER).await.unwrap();
    assert_eq!(converge(&[&a, &b], doc).await, MARKER);

    // Snapshot path too: it must be ciphertext like everything else.
    a.flush().await.unwrap();
    a.shutdown().await;
    b.shutdown().await;
    server.stop().await; // close WAL so the file is complete on disk

    let mut blob = std::fs::read(&server.db_path).expect("read server db");
    for suffix in ["-wal", "-shm"] {
        let side = server.db_path.with_file_name(format!(
            "{}{suffix}",
            server.db_path.file_name().unwrap().to_string_lossy()
        ));
        if let Ok(mut extra) = std::fs::read(side) {
            blob.append(&mut extra);
        }
    }
    let marker = MARKER.as_bytes();
    let found = blob.windows(marker.len()).any(|w| w == marker);
    assert!(!found, "plaintext marker leaked into server storage");
}

/// A malicious member pushes tampered ciphertext: honest clients drop it,
/// surface a security warning, and stay converged.
#[tokio::test]
async fn tampered_frames_rejected_by_clients() {
    use enkr_proto::crypto::Identity;
    use enkr_proto::wire::*;
    use enkr_proto::{PROTOCOL_VERSION, crypto, wire};
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;

    let server = TestServer::start_default().await;
    let (a, b, space, doc) = space_with_two_clients(&server).await;
    a.insert_text(doc, 0, "legit").await.unwrap();
    converge(&[&a, &b], doc).await;

    // Mallory is a *member* (worst case): she pushes a well-formed frame with
    // garbage ciphertext under the current epoch.
    let mallory = Identity::generate();
    a.add_member(
        space,
        mallory.identity_pk(),
        mallory.kex_pk(),
        MemberRole::Writer,
    )
    .await
    .unwrap();

    let (mut ws, _) = tokio_tungstenite::connect_async(server.url())
        .await
        .unwrap();
    ws.send(Message::Binary(
        wire::encode(&ClientMsg::Hello {
            identity_pk: mallory.identity_pk(),
            kex_pk: mallory.kex_pk(),
            protocol_version: PROTOCOL_VERSION,
        })
        .unwrap()
        .into(),
    ))
    .await
    .unwrap();
    let challenge = loop {
        if let Message::Binary(bytes) = ws.next().await.unwrap().unwrap() {
            break wire::decode::<ServerMsg>(&bytes).unwrap();
        }
    };
    let ServerMsg::Challenge { nonce, server_id } = challenge else {
        panic!()
    };
    let sig = mallory.sign(&crypto::auth_signing_bytes(&nonce, &server_id));
    ws.send(Message::Binary(
        wire::encode(&ClientMsg::Auth {
            sig: sig.to_vec(),
            account_token: None,
        })
        .unwrap()
        .into(),
    ))
    .await
    .unwrap();
    let _authok = ws.next().await;

    // Signed by Mallory (so it passes authorship checks) but the ciphertext
    // is garbage — AEAD must reject it on every honest client.
    let garbage = {
        let mut sig_bytes = Vec::new();
        sig_bytes.extend_from_slice(doc.as_bytes());
        sig_bytes.extend_from_slice(&0u32.to_le_bytes());
        sig_bytes.extend_from_slice(&[0x11; 24]);
        sig_bytes.extend_from_slice(&[0x42; 64]);
        UpdateFrame {
            doc_id: doc,
            epoch: 0,
            author_identity: mallory.identity_pk(),
            nonce: [0x11; 24],
            ciphertext: vec![0x42; 64],
            sig: mallory.sign(&sig_bytes).to_vec(),
        }
    };
    let mut warnings = b.events();
    ws.send(Message::Binary(
        wire::encode(&ClientMsg::PushUpdate {
            doc_id: doc,
            client_tag: 9,
            frame: garbage,
        })
        .unwrap()
        .into(),
    ))
    .await
    .unwrap();

    // b must reject the frame (decrypt failure) and keep working.
    let deadline = tokio::time::Instant::now() + CONVERGE_TIMEOUT;
    loop {
        match tokio::time::timeout_at(deadline, warnings.recv()).await {
            Ok(Ok(SyncEvent::SecurityWarning { .. })) => break,
            Ok(Ok(_)) => continue,
            Ok(Err(err)) => panic!("event stream closed: {err}"),
            Err(_) => panic!("no SecurityWarning for tampered frame"),
        }
    }
    a.insert_text(doc, 0, "still-works;").await.unwrap();
    let text = converge(&[&a, &b], doc).await;
    assert!(text.contains("legit"));
    assert!(text.contains("still-works;"));
    assert!(!text.contains('\u{42}'), "garbage must not enter the doc");
}

// ===========================================================================
// M3: membership, epochs, key rotation
// ===========================================================================

#[tokio::test]
async fn new_member_reads_full_history() {
    let server = TestServer::start_default().await;
    let a = server.client();
    wait_connected(&a).await;
    let space = a.create_space().await.unwrap();
    let doc = a.create_doc(space).await.unwrap();
    a.insert_text(doc, 0, "history-1;").await.unwrap();
    a.flush().await.unwrap();
    a.insert_text(doc, 0, "history-2;").await.unwrap();
    converge(&[&a], doc).await;

    let b = server.client();
    wait_connected(&b).await;
    invite_and_join(&a, &b, space).await;
    b.open_doc(space, doc).await.unwrap();
    let text = converge(&[&a, &b], doc).await;
    assert!(text.contains("history-1;"));
    assert!(text.contains("history-2;"));
}

/// Envelope GC must never strand content: a member joining *after* superseded
/// epochs have been collected still reads everything, including text written
/// before the rotation.
///
/// This is the safety guard for `Store::gc_envelopes`. The GC keeps every epoch
/// at or above the oldest surviving update/snapshot, so pre-rotation text is
/// either still in the log (and its epoch retained) or already folded into a
/// snapshot sealed under a later epoch.
#[tokio::test]
async fn history_survives_envelope_collection() {
    // Aggressive compaction + GC so the sweep actually happens inside the test.
    let config = ServerConfig {
        gc_interval: Duration::from_millis(100),
        snapshot_retention: Duration::from_millis(0),
        snapshot_settle: Duration::from_millis(0),
        snapshot_request_threshold: 2,
        ..ServerConfig::default()
    };
    let server = TestServer::start(config).await;
    let a = server.client();
    wait_connected(&a).await;
    let space = a.create_space().await.unwrap();
    let doc = a.create_doc(space).await.unwrap();

    // Written under epoch 0.
    a.insert_text(doc, 0, "before-rotation;").await.unwrap();
    a.flush().await.unwrap();
    converge(&[&a], doc).await;

    // Rotate to epoch 1, then write more so compaction has a reason to run.
    let evictee = server.client();
    wait_connected(&evictee).await;
    invite_and_join(&a, &evictee, space).await;
    a.remove_member(space, evictee.identity_pk()).await.unwrap();
    for i in 0..10 {
        a.insert_text(doc, 0, format!("after-{i};")).await.unwrap();
        a.flush().await.unwrap();
    }
    converge(&[&a], doc).await;

    // Wait for the sweep to actually collect epoch 0, so this test can't pass
    // vacuously by simply never GC'ing anything.
    let deadline = tokio::time::Instant::now() + CONVERGE_TIMEOUT;
    loop {
        let epoch0: i64 = server
            .raw_db()
            .query_row(
                "SELECT COUNT(*) FROM key_envelopes WHERE epoch = 0",
                [],
                |row| row.get(0),
            )
            .expect("envelope count");
        if epoch0 == 0 {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "epoch 0 envelopes were never collected ({epoch0} left)"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // A member invited now gets only the envelopes that survived.
    let b = server.client();
    wait_connected(&b).await;
    invite_and_join(&a, &b, space).await;
    b.open_doc(space, doc).await.unwrap();
    let text = converge(&[&a, &b], doc).await;
    assert!(
        text.contains("before-rotation;"),
        "pre-rotation text was lost after envelope GC: {text:?}"
    );
    assert!(
        text.contains("after-9;"),
        "post-rotation text missing: {text:?}"
    );
}

#[tokio::test]
async fn reader_members_can_read_but_not_write() {
    let server = TestServer::start_default().await;
    let owner = server.client();
    let reader = server.client();
    wait_connected(&owner).await;
    wait_connected(&reader).await;

    let space = owner.create_space().await.unwrap();
    let doc = owner.create_doc(space).await.unwrap();
    owner.insert_text(doc, 0, "owner-history;").await.unwrap();
    converge(&[&owner], doc).await;

    invite_and_join_as(&owner, &reader, space, MemberRole::Reader).await;
    reader.open_doc(space, doc).await.unwrap();
    assert_eq!(converge(&[&owner, &reader], doc).await, "owner-history;");

    let err = reader
        .create_doc(space)
        .await
        .expect_err("reader created a doc");
    assert!(
        err.to_string().contains("permission denied"),
        "unexpected create_doc error: {err}"
    );

    reader.insert_text(doc, 0, "reader-write;").await.unwrap();
    reader.flush().await.unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(owner.doc_text(doc).await.unwrap(), "owner-history;");
}

#[tokio::test]
async fn cannot_invite_own_device() {
    let server = TestServer::start_default().await;
    let owner = server.client();
    wait_connected(&owner).await;

    let space = owner.create_space().await.unwrap();
    // Re-adding our own device (membership is keyed on identity_pk) must be
    // rejected, even with a tweaked kex_pk — otherwise we'd overwrite our own
    // role and could lock ourselves out.
    let mut kex = owner.kex_pk();
    kex[31] ^= 0x01;
    let err = owner
        .add_member(space, owner.identity_pk(), kex, MemberRole::Writer)
        .await
        .expect_err("owner invited itself");
    assert!(
        err.to_string().contains("itself"),
        "unexpected self-invite error: {err}"
    );

    // Membership is unchanged: still just the owner.
    let members = owner.list_members(space).await.unwrap();
    assert_eq!(members.len(), 1);
    assert_eq!(members[0].identity_pk, owner.identity_pk());
}

#[tokio::test]
async fn removed_member_rejected_and_cannot_decrypt_post_rotation() {
    let server = TestServer::start_default().await;
    let (a, b, space, doc) = space_with_two_clients(&server).await;
    let c = server.client();
    wait_connected(&c).await;
    invite_and_join(&a, &c, space).await;
    c.open_doc(space, doc).await.unwrap();

    a.insert_text(doc, 0, "shared;").await.unwrap();
    converge(&[&a, &b, &c], doc).await;

    // Remove c → epoch rotation. Wait until the EpochBump actually reaches b
    // (a fixed sleep flakes under heavy parallel test load).
    let mut b_events = b.events();
    a.remove_member(space, c.identity_pk()).await.unwrap();
    let bump_deadline = tokio::time::Instant::now() + CONVERGE_TIMEOUT;
    loop {
        match tokio::time::timeout_at(bump_deadline, b_events.recv()).await {
            Ok(Ok(SyncEvent::EpochBumped { .. })) => break,
            Ok(Ok(_)) => continue,
            _ => panic!("b never saw the EpochBump"),
        }
    }
    a.insert_text(doc, 0, "post-rotation-secret;")
        .await
        .unwrap();
    let text = converge(&[&a, &b], doc).await;
    assert!(text.contains("post-rotation-secret;"));

    // c keeps its pre-removal history but never sees post-rotation content,
    // even though the ciphertext was fanned out to its open subscription.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let c_text = c.doc_text(doc).await.unwrap();
    assert!(c_text.contains("shared;"));
    assert!(
        !c_text.contains("post-rotation-secret;"),
        "removed member decrypted post-rotation traffic"
    );

    // And the server refuses c's writes.
    let mut events = c.events();
    c.insert_text(doc, 0, "evil;").await.unwrap();
    c.flush().await.unwrap();
    let deadline = tokio::time::Instant::now() + CONVERGE_TIMEOUT;
    loop {
        match tokio::time::timeout_at(deadline, events.recv()).await {
            Ok(Ok(SyncEvent::ServerError { .. })) => break,
            Ok(Ok(_)) => continue,
            _ => panic!("server accepted a removed member's write"),
        }
    }
    let text = converge(&[&a, &b], doc).await;
    assert!(
        !text.contains("evil;"),
        "removed member's write leaked into the doc"
    );
}

#[tokio::test]
async fn owner_lists_members_and_manages_roles() {
    let server = TestServer::start_default().await;
    let owner = server.client();
    let member = server.client();
    wait_connected(&owner).await;
    wait_connected(&member).await;

    let space = owner.create_space().await.unwrap();
    let doc = owner.create_doc(space).await.unwrap();
    invite_and_join_as(&owner, &member, space, MemberRole::Writer).await;
    member.open_doc(space, doc).await.unwrap();

    // The owner sees both identities with their roles.
    let role_of =
        |members: &[MemberEntry], pk| members.iter().find(|m| m.identity_pk == pk).map(|m| m.role);
    let members = owner.list_members(space).await.unwrap();
    assert_eq!(members.len(), 2);
    assert_eq!(
        role_of(&members, owner.identity_pk()),
        Some(MemberRole::Owner)
    );
    assert_eq!(
        role_of(&members, member.identity_pk()),
        Some(MemberRole::Writer)
    );

    // Demote the writer to reader: it loses write access.
    owner
        .set_member_role(space, member.identity_pk(), MemberRole::Reader)
        .await
        .unwrap();
    let members = owner.list_members(space).await.unwrap();
    assert_eq!(
        role_of(&members, member.identity_pk()),
        Some(MemberRole::Reader)
    );
    // Wait for the server's rejection rather than sleeping a fixed 200ms and
    // asserting a negative. The old form was racy in two directions: the
    // demotion is confirmed through the *owner's* membership view, which does
    // not prove the server has applied it yet, and under load 200ms was not
    // always enough for the write to be rejected and the rejection observed.
    // Waiting for `ServerError` — the same shape the removed-member case above
    // uses — is deterministic: it is the server telling us it refused.
    let mut events = member.events();
    member.insert_text(doc, 0, "reader-write;").await.unwrap();
    member.flush().await.unwrap();
    let deadline = tokio::time::Instant::now() + CONVERGE_TIMEOUT;
    loop {
        match tokio::time::timeout_at(deadline, events.recv()).await {
            Ok(Ok(SyncEvent::ServerError { .. })) => break,
            Ok(Ok(_)) => continue,
            // A timeout here is ambiguous — the rejection may simply not have
            // arrived yet — so say which of the two happened rather than
            // blaming the server for something that may just be a slow run.
            Ok(Err(err)) => panic!("event stream ended before the rejection: {err}"),
            Err(_) => panic!(
                "timed out after {CONVERGE_TIMEOUT:?} waiting for the server to reject \
                 a demoted reader's write"
            ),
        }
    }
    assert!(
        !owner.doc_text(doc).await.unwrap().contains("reader-write;"),
        "demoted reader's write leaked into the owner's replica"
    );

    // Uninvite the member: it drops out of the listing.
    owner
        .remove_member(space, member.identity_pk())
        .await
        .unwrap();
    let members = owner.list_members(space).await.unwrap();
    assert_eq!(members.len(), 1);
    assert_eq!(role_of(&members, member.identity_pk()), None);
}

#[tokio::test]
async fn index_doc_syncs_between_members() {
    let server = TestServer::start_default().await;
    let (a, b, space, _doc) = space_with_two_clients(&server).await;
    let index = index_doc_id(&space);
    a.open_doc(space, index).await.unwrap();
    b.open_doc(space, index).await.unwrap();
    a.insert_text(index, 0, "note-listing").await.unwrap();
    assert_eq!(converge(&[&a, &b], index).await, "note-listing");
}

// ===========================================================================
// M4: snapshots, server-requested compaction, GC, cold start
// ===========================================================================

#[tokio::test]
async fn single_client_compaction_via_request_snapshot_and_gc() {
    let mut config = ServerConfig::default();
    config.snapshot_request_threshold = 10;
    config.snapshot_retention = Duration::from_millis(0); // self-ack instantly
    config.snapshot_settle = Duration::from_millis(0);
    config.gc_interval = Duration::from_millis(100);
    let server = TestServer::start(config).await;

    let a = server.client();
    wait_connected(&a).await;
    let space = a.create_space().await.unwrap();
    let doc = a.create_doc(space).await.unwrap();

    // Each flush = one stored update; cross the threshold several times over.
    for i in 0..30 {
        a.insert_text(doc, 0, format!("{i};")).await.unwrap();
        a.flush().await.unwrap();
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    converge(&[&a], doc).await;
    // Allow RequestSnapshot → PutSnapshot → GC to complete.
    tokio::time::sleep(Duration::from_millis(800)).await;

    let conn = server.raw_db();
    let updates: i64 = conn
        .query_row("SELECT COUNT(*) FROM updates", [], |r| r.get(0))
        .unwrap();
    let snapshots: i64 = conn
        .query_row("SELECT COUNT(*) FROM snapshots", [], |r| r.get(0))
        .unwrap();
    assert!(snapshots >= 1, "server never received a snapshot");
    assert!(
        updates < 30,
        "log was never compacted: {updates} updates still stored"
    );

    // Cold start: a fresh member must reconstruct the doc from snapshot + tail.
    let b = server.client();
    wait_connected(&b).await;
    invite_and_join(&a, &b, space).await;
    b.open_doc(space, doc).await.unwrap();
    let text = converge(&[&a, &b], doc).await;
    for i in 0..30 {
        assert!(
            text.contains(&format!("{i};")),
            "missing edit {i} after cold sync"
        );
    }
}

/// Editing must keep working after the GC has emptied a compacted doc's log.
///
/// The sweep is what makes this interesting: once a snapshot covers head and
/// gets acked, `gc_updates_through` deletes every update row the doc has. A
/// `seq` counter derived from that table then restarts at 1 and re-issues seqs
/// the subscribers have already retired — which they drop as duplicates
/// (`seq <= have_seq`), diverging both replicas with no error on either side
/// and no way back, since `updates_since(covers_seq)` never returns them
/// either. The other compaction tests above only assert the log *shrank*, and
/// a partial sweep leaves a tail that hides this entirely.
#[tokio::test]
async fn edits_still_propagate_after_the_gc_drains_a_compacted_doc() {
    let mut config = ServerConfig::default();
    config.snapshot_request_threshold = 2;
    config.snapshot_retention = Duration::from_millis(0); // self-ack instantly
    config.snapshot_settle = Duration::from_millis(0);
    config.gc_interval = Duration::from_millis(100);
    let server = TestServer::start(config).await;

    let (a, b, space, doc) = space_with_two_clients(&server).await;
    for i in 0..4 {
        a.insert_text(doc, 0, format!("pre{i};")).await.unwrap();
        a.flush().await.unwrap();
        tokio::time::sleep(Duration::from_millis(120)).await;
    }
    let before = converge(&[&a, &b], doc).await;

    // Wait for a sweep that covers head, so the log really is empty — that is
    // the state a quiet doc settles into, not an edge case.
    let deadline = tokio::time::Instant::now() + CONVERGE_TIMEOUT;
    loop {
        let updates: i64 = server
            .raw_db()
            .query_row("SELECT COUNT(*) FROM updates", [], |r| r.get(0))
            .unwrap();
        if updates == 0 {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "log never fully compacted: {updates} update rows left"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    a.insert_text(doc, 0, "post-gc;").await.unwrap();
    a.flush().await.unwrap();
    let after = converge(&[&a, &b], doc).await;
    assert_eq!(after, format!("post-gc;{before}"));

    // And a member arriving after the sweep rebuilds the whole doc from
    // snapshot + post-sweep tail.
    let c = server.client();
    wait_connected(&c).await;
    invite_and_join(&a, &c, space).await;
    c.open_doc(space, doc).await.unwrap();
    assert_eq!(converge(&[&a, &b, &c], doc).await, after);
}

/// A reader must not be able to rewrite a doc by *snapshotting* it.
///
/// `reader_members_can_read_but_not_write` covers the push path. The snapshot
/// path is the wider hole: a snapshot is a full-state replace, and a reader
/// holds the space key — that is what lets it read — so it can seal and sign
/// one that is cryptographically perfect. Only the role says it may not, and
/// the role was checked on `PushUpdate` but not on `PutSnapshot`.
#[tokio::test]
async fn readers_cannot_rewrite_a_doc_through_a_snapshot() {
    let mut config = ServerConfig::default();
    config.snapshot_request_threshold = 2;
    let server = TestServer::start(config).await;

    let owner = server.client();
    // A reader that volunteers a snapshot at every opportunity.
    let mut cfg = enkr::sync::SyncConfig::new(server.url(), enkr::sync::IdentityStore::InMemory);
    cfg.debounce = Duration::from_millis(20);
    cfg.heartbeat = Duration::from_millis(300);
    cfg.liveness_timeout = Duration::from_secs(2);
    cfg.reconnect_max = Duration::from_secs(1);
    cfg.snapshot_threshold = 1;
    let reader = TestClient::spawn(cfg);
    wait_connected(&owner).await;
    wait_connected(&reader).await;

    let space = owner.create_space().await.unwrap();
    let doc = owner.create_doc(space).await.unwrap();
    owner.insert_text(doc, 0, "trusted;").await.unwrap();
    owner.flush().await.unwrap();
    invite_and_join_as(&owner, &reader, space, MemberRole::Reader).await;
    reader.open_doc(space, doc).await.unwrap();
    assert_eq!(converge(&[&owner, &reader], doc).await, "trusted;");

    // The reader's push is refused, but the text is in its local replica — so
    // any snapshot it authors carries it.
    reader.insert_text(doc, 0, "reader-write;").await.unwrap();
    reader.flush().await.unwrap();
    owner.insert_text(doc, 0, "more;").await.unwrap();
    owner.flush().await.unwrap();
    tokio::time::sleep(Duration::from_millis(600)).await;

    let reader_pk = reader.identity_pk().to_vec();
    let authored: i64 = server
        .raw_db()
        .query_row(
            "SELECT COUNT(*) FROM snapshots WHERE author_identity = ?1",
            [&reader_pk],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(authored, 0, "the relay stored a reader-authored snapshot");

    // A member arriving afterwards rebuilds the doc from what the relay has.
    let joiner = server.client();
    wait_connected(&joiner).await;
    invite_and_join(&owner, &joiner, space).await;
    joiner.open_doc(space, doc).await.unwrap();
    let text = converge(&[&owner, &joiner], doc).await;
    assert!(
        !text.contains("reader-write;"),
        "reader-authored content reached another member: {text:?}"
    );
}

/// The relay's outer SnapshotInfo.covers_seq is not authenticated. A forged
/// high value must not make the client retire future update sequence numbers.
#[tokio::test]
async fn forged_snapshot_metadata_cannot_wedge_a_doc() {
    let mut config = ServerConfig::default();
    config.snapshot_request_threshold = 1;
    config.snapshot_retention = Duration::from_secs(60);
    let (server, hostility) = TestServer::start_hostile(config).await;

    let owner = server.client();
    wait_connected(&owner).await;
    let space = owner.create_space().await.unwrap();
    let doc = owner.create_doc(space).await.unwrap();
    owner.insert_text(doc, 0, "before;").await.unwrap();
    owner.flush().await.unwrap();
    let snapshot_deadline = tokio::time::Instant::now() + CONVERGE_TIMEOUT;
    loop {
        let count: i64 = server
            .raw_db()
            .query_row("SELECT COUNT(*) FROM snapshots", [], |row| row.get(0))
            .unwrap();
        if count > 0 {
            break;
        }
        assert!(tokio::time::Instant::now() < snapshot_deadline);
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let joiner = server.client();
    wait_connected(&joiner).await;
    invite_and_join(&owner, &joiner, space).await;
    hostility.lie_snapshot_covers_seq(1 << 63);
    joiner.open_doc(space, doc).await.unwrap();

    // The signed snapshot still covers only the real pre-edit sequence. The
    // next live update must therefore not be mistaken for an already-seen seq.
    owner.insert_text(doc, 0, "after;").await.unwrap();
    owner.flush().await.unwrap();
    let text = converge(&[&owner, &joiner], doc).await;
    assert!(
        text.contains("after;"),
        "forged metadata wedged the document: {text:?}"
    );
}

/// The flip side of the gate above: it keys on whether a device could *ever*
/// write, not on its role right now.
///
/// A frame is not bound to the membership state it was written under, so gating
/// on the live role would make a demotion retroactively invalidate honest
/// history — the snapshot a writer authored before being demoted is the whole
/// doc, and dropping it strands every member who has not already applied it.
#[tokio::test]
async fn a_demoted_writers_existing_snapshot_is_still_accepted() {
    let mut config = ServerConfig::default();
    config.snapshot_request_threshold = 2;
    let server = TestServer::start(config).await;

    let (owner, member, space, doc) = space_with_two_clients(&server).await;
    // The member (a writer) authors the doc *and* its snapshot.
    for i in 0..4 {
        member.insert_text(doc, 0, format!("w{i};")).await.unwrap();
        member.flush().await.unwrap();
        tokio::time::sleep(Duration::from_millis(80)).await;
    }
    let text = converge(&[&owner, &member], doc).await;
    tokio::time::sleep(Duration::from_millis(400)).await;
    let member_pk = member.identity_pk().to_vec();
    let authored: i64 = server
        .raw_db()
        .query_row(
            "SELECT COUNT(*) FROM snapshots WHERE author_identity = ?1",
            [&member_pk],
            |r| r.get(0),
        )
        .unwrap();
    assert!(
        authored >= 1,
        "precondition: the writer authored a snapshot"
    );

    // Now demote it. The snapshot it already wrote must stay readable.
    owner
        .set_member_role(space, member.identity_pk(), MemberRole::Reader)
        .await
        .unwrap();
    let joiner = server.client();
    wait_connected(&joiner).await;
    invite_and_join(&owner, &joiner, space).await;
    joiner.open_doc(space, doc).await.unwrap();
    assert_eq!(
        converge(&[&owner, &joiner], doc).await,
        text,
        "a demoted writer's snapshot was rejected, stranding a new member"
    );
}

#[tokio::test]
async fn storage_stays_bounded_under_continuous_editing() {
    let mut config = ServerConfig::default();
    config.snapshot_request_threshold = 10;
    config.snapshot_retention = Duration::from_millis(0);
    config.snapshot_settle = Duration::from_millis(0);
    config.gc_interval = Duration::from_millis(50);
    let server = TestServer::start(config).await;

    let (a, b, _space, doc) = space_with_two_clients(&server).await;
    for i in 0..60 {
        let client = if i % 2 == 0 { &a } else { &b };
        client.insert_text(doc, 0, format!("{i};")).await.unwrap();
        client.flush().await.unwrap();
    }
    converge(&[&a, &b], doc).await;
    tokio::time::sleep(Duration::from_millis(600)).await;

    let conn = server.raw_db();
    let updates: i64 = conn
        .query_row("SELECT COUNT(*) FROM updates", [], |r| r.get(0))
        .unwrap();
    assert!(
        updates < 40,
        "storage not bounded: {updates} update rows for 60+ edits"
    );
}

/// Adversarial churn: 1 insert + 1 delete (net-zero visible text) repeated
/// thousands of times. The update log must stay compacted, but this also probes
/// whether the *snapshot blob* (full doc state) grows with tombstone history.
#[tokio::test]
async fn zero_net_delta_churn_storage() {
    let mut config = ServerConfig::default();
    config.snapshot_request_threshold = 20;
    config.snapshot_retention = Duration::from_millis(0); // self-ack instantly
    config.snapshot_settle = Duration::from_millis(0);
    config.gc_interval = Duration::from_millis(50);
    let server = TestServer::start(config).await;

    let a = server.client();
    wait_connected(&a).await;
    let space = a.create_space().await.unwrap();
    let doc = a.create_doc(space).await.unwrap();

    // One insert + one delete per cycle: the visible document is always empty.
    async fn churn(a: &TestClient, doc: Uuid, cycles: usize) {
        for _ in 0..cycles {
            a.insert_text(doc, 0, "x").await.unwrap();
            a.delete_text(doc, 0, 1).await.unwrap();
            a.flush().await.unwrap();
        }
    }

    // Measure server storage after GC has had time to run.
    fn measure(server: &TestServer) -> (i64, i64, i64) {
        let conn = server.raw_db();
        let updates: i64 = conn
            .query_row("SELECT COUNT(*) FROM updates", [], |r| r.get(0))
            .unwrap();
        let snapshot_bytes: i64 = conn
            .query_row(
                "SELECT COALESCE(SUM(LENGTH(blob)), 0) FROM snapshots",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let db_bytes: i64 = conn
            .query_row(
                "SELECT page_count * page_size FROM pragma_page_count(), pragma_page_size()",
                [],
                |r| r.get(0),
            )
            .unwrap();
        (updates, snapshot_bytes, db_bytes)
    }

    churn(&a, doc, 1250).await;
    converge(&[&a], doc).await;
    tokio::time::sleep(Duration::from_millis(400)).await;
    let (u1, snap1, db1) = measure(&server);

    churn(&a, doc, 1250).await;
    converge(&[&a], doc).await;
    tokio::time::sleep(Duration::from_millis(400)).await;
    let (u2, snap2, db2) = measure(&server);

    assert_eq!(
        a.doc_text(doc).await.unwrap(),
        "",
        "visible doc must be empty"
    );
    eprintln!(
        "after 1250 cycles: updates={u1} snapshot_bytes={snap1} db_bytes={db1}\n\
         after 2500 cycles: updates={u2} snapshot_bytes={snap2} db_bytes={db2}"
    );

    // Doubling the churn must not roughly double storage. Both quantities are
    // sublinear in the op count (2500 cycles = 5000 ops): the log compacts to a
    // bounded steady state, and the snapshot of an always-empty doc stays tiny
    // because Yrs garbage-collects tombstones.
    assert!(
        u2 < 800,
        "update log grew with history: {u2} rows after 5000 ops"
    );
    assert!(
        snap2 <= snap1 * 2,
        "snapshot storage grew with net-zero churn: {snap1} -> {snap2} bytes"
    );
}

/// M4 accept criterion: a 10k-update doc cold-syncs quickly from
/// snapshot + tail.
/// run with `cargo test -p enkr --test sync -- --ignored`.
#[tokio::test]
async fn cold_sync_10k_updates_under_a_second() {
    let mut config = ServerConfig::default();
    config.snapshot_request_threshold = 1000;
    config.snapshot_retention = Duration::from_millis(0);
    config.snapshot_settle = Duration::from_millis(0);
    config.gc_interval = Duration::from_millis(200);
    let server = TestServer::start(config).await;

    let a = server.client();
    wait_connected(&a).await;
    let space = a.create_space().await.unwrap();
    let doc = a.create_doc(space).await.unwrap();
    for i in 0..10_000 {
        a.insert_text(doc, 0, format!("{i};")).await.unwrap();
        if i % 50 == 0 {
            a.flush().await.unwrap();
        }
    }
    a.flush().await.unwrap();
    converge(&[&a], doc).await;
    tokio::time::sleep(Duration::from_secs(1)).await;

    let b = server.client();
    wait_connected(&b).await;
    invite_and_join(&a, &b, space).await;
    let started = std::time::Instant::now();
    b.open_doc(space, doc).await.unwrap();
    converge(&[&a, &b], doc).await;
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_secs(1),
        "cold sync took {elapsed:?}"
    );
}

// ===========================================================================
// Ephemeral (awareness) relay
// ===========================================================================

#[tokio::test]
async fn ephemeral_frames_relayed_not_stored() {
    let server = TestServer::start_default().await;
    let (a, b, _space, doc) = space_with_two_clients(&server).await;
    // Make sure b's subscription is live before firing the (unretried,
    // fire-and-forget) ephemeral frame.
    a.insert_text(doc, 0, "x").await.unwrap();
    converge(&[&a, &b], doc).await;
    let stored_before: i64 = server
        .raw_db()
        .query_row("SELECT COUNT(*) FROM updates", [], |r| r.get(0))
        .unwrap();

    let mut events = b.events();
    a.send_ephemeral(doc, b"cursor@42".to_vec()).unwrap();

    let deadline = tokio::time::Instant::now() + CONVERGE_TIMEOUT;
    loop {
        match tokio::time::timeout_at(deadline, events.recv()).await {
            Ok(Ok(SyncEvent::Ephemeral { payload, .. })) => {
                assert_eq!(payload, b"cursor@42");
                break;
            }
            Ok(Ok(_)) => continue,
            _ => panic!("ephemeral frame never relayed"),
        }
    }
    let updates: i64 = server
        .raw_db()
        .query_row("SELECT COUNT(*) FROM updates", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        updates, stored_before,
        "ephemeral frames must never be persisted"
    );
}

// ===========================================================================
// Untrusted-relay hardening (audit follow-ups)
// ===========================================================================

/// A relay cannot make a client encrypt under a key the relay chose.
///
/// Key envelopes are anonymous X25519 sealed boxes: wrapping one needs only the
/// recipient's *public* kex key, which the relay stores and which travels in the
/// membership log. So "it unsealed cleanly" proves nothing about who wrapped it.
/// If the client took the numerically-highest epoch it had a key for, a relay
/// could offer an envelope for an epoch that never happened and have every
/// subsequent edit — and every snapshot, which is the whole document — sealed
/// under a key it knows. That is a total break of the property this protocol
/// exists to provide.
///
/// The signed membership log is the only authenticated statement about which
/// epochs exist, so it is the ceiling.
#[tokio::test]
async fn a_relay_forged_key_envelope_is_never_used_to_seal() {
    let (server, hostility) = TestServer::start_hostile(ServerConfig::default()).await;

    let owner = server.client();
    wait_connected(&owner).await;
    let space = owner.create_space().await.unwrap();
    let doc = owner.create_doc(space).await.unwrap();
    owner.insert_text(doc, 0, "before;").await.unwrap();
    owner.flush().await.unwrap();
    converge(&[&owner], doc).await;

    // The relay arms itself before the victim's join, which is what triggers
    // the envelope fetch. The space has only ever been at epoch 0 — nobody has
    // been removed — so epoch 7 is an epoch the signed log does not justify,
    // and the relay is inventing it out of nothing but the victim's *public*
    // kex key.
    let victim = server.client();
    wait_connected(&victim).await;
    hostility.forge_envelope_for_epoch(7, victim.kex_pk());
    invite_and_join(&owner, &victim, space).await;
    victim.open_doc(space, doc).await.unwrap();
    converge(&[&owner, &victim], doc).await;

    // If the victim adopted the forged key it would seal under epoch 7, and the
    // owner — which holds only the real epoch-0 key — could never read this.
    victim.insert_text(doc, 0, "victim;").await.unwrap();
    victim.flush().await.unwrap();
    let text = converge(&[&owner, &victim], doc).await;
    assert!(
        text.contains("victim;") && text.contains("before;"),
        "an honest member could not read content the relay's forged key sealed: {text:?}"
    );

    // Belt and braces: nothing on the relay is sealed under the invented epoch.
    let forged: i64 = server
        .raw_db()
        .query_row("SELECT COUNT(*) FROM updates WHERE epoch = 7", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(
        forged, 0,
        "a frame was sealed under the epoch the relay invented"
    );
}

/// The attack the epoch ceiling alone does not stop: a forged envelope for the
/// space's **current** epoch.
///
/// A key envelope is an anonymous X25519 sealed box, so wrapping one needs
/// nothing but the recipient's public kex key — which the relay stores, and
/// which travels in the membership log. Bounding the *epoch* by the signed log
/// does nothing here: a space that has never rotated is entirely at epoch 0, so
/// the relay withholds the genuine epoch-0 envelope, substitutes one sealing a
/// key of its own, and the victim encrypts everything under it. The signed
/// key commitment is what makes the substitution detectable.
#[tokio::test]
async fn a_forged_current_epoch_envelope_is_refused_not_adopted() {
    use enkr_proto::crypto::{self, SpaceKey};
    use enkr_proto::wire::{self, UpdateFrame};
    use harness::hostile::FORGED_SPACE_KEY;

    let (server, hostility) = TestServer::start_hostile(ServerConfig::default()).await;

    let owner = server.client();
    wait_connected(&owner).await;
    let space = owner.create_space().await.unwrap();
    let doc = owner.create_doc(space).await.unwrap();
    owner.insert_text(doc, 0, "before;").await.unwrap();
    owner.flush().await.unwrap();
    converge(&[&owner], doc).await;

    // The space has never rotated, so epoch 0 *is* the current epoch — nothing
    // about this envelope is out of range.
    let victim = server.client();
    wait_connected(&victim).await;
    let warnings = victim.events();
    hostility.forge_envelope_for_epoch(0, victim.kex_pk());
    let _ = invite_and_join(&owner, &victim, space).await;
    let _ = victim.open_doc(space, doc).await;

    // Give the victim every chance to seal something under the forged key.
    let _ = victim.insert_text(doc, 0, "victim;").await;
    let _ = victim.flush().await;
    tokio::time::sleep(Duration::from_millis(500)).await;

    // The relay must hold nothing that key can open. Checked against the stored
    // frames rather than the victim's outbox, because what matters is whether
    // the attacker's key ever governs bytes that left the device.
    let forged_key = SpaceKey(FORGED_SPACE_KEY);
    let conn = server.raw_db();
    let frames: Vec<Vec<u8>> = {
        let mut stmt = conn.prepare("SELECT frame FROM updates").unwrap();
        let rows = stmt
            .query_map([], |row| row.get::<_, Vec<u8>>(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        rows
    };
    assert!(!frames.is_empty(), "no updates were stored at all");
    for bytes in &frames {
        let frame: UpdateFrame = wire::decode(bytes).unwrap();
        assert!(
            crypto::open_update(&frame, &forged_key, &space, &frame.doc_id).is_err(),
            "an update was sealed under the key the relay chose"
        );
    }

    // Refusing must also be *loud*. A device that cannot obtain a real key has
    // to look different from one that simply has nothing to say, or the user
    // sees a space that quietly never syncs.
    let mut warnings = warnings;
    let mut warned = false;
    while let Ok(event) = warnings.try_recv() {
        warned |= matches!(event, SyncEvent::SecurityWarning { .. });
    }
    assert!(warned, "the forged envelope was refused without telling anyone");

    // The owner's own history is untouched by any of this.
    let text = owner.doc_text(doc).await.unwrap();
    assert!(text.contains("before;"), "owner lost its own content: {text:?}");
}

/// A relay cannot replace a verified membership log with a different one.
///
/// The log is the client-side trust root, and its first op is *self-signed*
/// (TOFU) — so any identity can mint a syntactically perfect log for any space
/// id. A guard that compares lengths therefore proves nothing: a forged log of
/// equal length passes it and is adopted wholesale, after which the attacker is
/// the owner and the real member is not a member at all. The client has to
/// check that what it is served *extends* what it already verified.
#[tokio::test]
async fn a_substituted_membership_log_is_refused() {
    use enkr_proto::crypto::Identity;
    use enkr_proto::membership::{self, MembershipOp, MembershipOpKind};
    use enkr_proto::wire;

    let (server, hostility) = TestServer::start_hostile(ServerConfig::default()).await;

    let owner = server.client();
    wait_connected(&owner).await;
    let space = owner.create_space().await.unwrap();
    let doc = owner.create_doc(space).await.unwrap();
    owner.insert_text(doc, 0, "before;").await.unwrap();
    owner.flush().await.unwrap();

    let victim = server.client();
    wait_connected(&victim).await;
    invite_and_join(&owner, &victim, space).await;
    victim.open_doc(space, doc).await.unwrap();
    converge(&[&owner, &victim], doc).await;

    // A third member, so that something can arrive from a device the victim's
    // log does not know about — an `Add` bumps no epoch, so the victim has had
    // no reason to refetch since it joined. A frame it cannot attribute is what
    // makes it ask for the log again, and a lying relay's moment is the answer.
    let extra = server.client();
    wait_connected(&extra).await;
    invite_and_join(&owner, &extra, space).await;
    extra.open_doc(space, doc).await.unwrap();

    // The forged log: the attacker's own space, with the victim added as a
    // reader so nothing about it looks broken. Two ops, so it is no shorter
    // than the prefix the victim already confirmed — length is exactly what a
    // guard must not rely on.
    let attacker = Identity::generate();
    let sign = |seq, kind| {
        let op = MembershipOp {
            space_id: space,
            op_seq: seq,
            kind,
        };
        wire::encode(&membership::sign_op(&attacker, &op).unwrap()).unwrap()
    };
    let forged = vec![
        sign(
            0,
            MembershipOpKind::Create {
                creator_kex: attacker.kex_pk(),
                key_commitment: [0u8; 32],
            },
        ),
        sign(
            1,
            MembershipOpKind::Add {
                identity_pk: victim.identity_pk(),
                kex_pk: victim.kex_pk(),
                role: MemberRole::Reader,
            },
        ),
    ];

    // Armed only now: the relay's own `RemoveMember` validation reads the log
    // through the same store method, so substituting earlier would break the
    // ordinary paths this test depends on rather than the one it is probing.
    hostility.substitute_membership_log(forged);
    let mut warnings = victim.events();
    extra.insert_text(doc, 0, "extra;").await.unwrap();
    extra.flush().await.unwrap();
    tokio::time::sleep(Duration::from_millis(600)).await;

    // The victim's view of who owns this space must be untouched.
    let members = victim.list_members(space).await.unwrap();
    assert!(
        members
            .iter()
            .any(|m| m.identity_pk == owner.identity_pk() && m.role == MemberRole::Owner),
        "the real owner was displaced by the forged log"
    );
    assert!(
        !members
            .iter()
            .any(|m| m.identity_pk == attacker.identity_pk()),
        "the attacker was adopted into the space"
    );
    assert!(
        members
            .iter()
            .any(|m| m.identity_pk == victim.identity_pk()),
        "the victim was written out of its own space"
    );

    let mut warned = false;
    while let Ok(event) = warnings.try_recv() {
        warned |= matches!(event, SyncEvent::SecurityWarning { .. });
    }
    assert!(warned, "a substituted log was refused without telling anyone");
}

/// A client missing the rotated key must write nothing, not fall back.
///
/// The point of bumping the epoch on a removal is that the removed member keeps
/// the old key and must not be able to read anything written afterwards. A
/// member whose rotated envelope has not arrived — because the relay is slow, or
/// because it is withholding it on purpose — and which quietly seals under the
/// previous epoch hands the removed member exactly what the rotation was for.
/// Failing closed costs nothing: the edits stay queued and the client chases the
/// missing envelope.
#[tokio::test]
async fn a_client_without_the_rotated_key_seals_nothing() {
    let (server, hostility) = TestServer::start_hostile(ServerConfig::default()).await;

    let owner = server.client();
    wait_connected(&owner).await;
    let space = owner.create_space().await.unwrap();
    let doc = owner.create_doc(space).await.unwrap();
    owner.insert_text(doc, 0, "before;").await.unwrap();
    owner.flush().await.unwrap();

    let victim = server.client();
    wait_connected(&victim).await;
    invite_and_join(&owner, &victim, space).await;
    victim.open_doc(space, doc).await.unwrap();

    let evictee = server.client();
    wait_connected(&evictee).await;
    invite_and_join(&owner, &evictee, space).await;
    converge(&[&owner, &victim], doc).await;

    let epoch_zero_rows = |server: &TestServer| -> i64 {
        server
            .raw_db()
            .query_row("SELECT COUNT(*) FROM updates WHERE epoch = 0", [], |r| {
                r.get(0)
            })
            .unwrap()
    };
    let before_rotation = epoch_zero_rows(&server);

    // The rotation happens, but the victim never receives the epoch-1 envelope.
    hostility.withhold_envelope_epoch(1);
    owner
        .remove_member(space, evictee.identity_pk())
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(400)).await;

    // Now the victim edits. Its own log says the space is at epoch 1; it holds
    // only the epoch-0 key, which the evictee also still holds.
    victim.insert_text(doc, 0, "after-rotation;").await.unwrap();
    let _ = victim.flush().await;
    tokio::time::sleep(Duration::from_millis(400)).await;

    assert_eq!(
        epoch_zero_rows(&server),
        before_rotation,
        "an edit was sealed under the revoked epoch the removed member can read"
    );

    // Failing closed must not mean losing the edit: it is still buffered,
    // waiting for a key that a healthy relay would have sent.
    let status = victim.status().await.unwrap();
    assert!(
        status.pending_docs > 0,
        "the held edit was dropped rather than queued"
    );
}

/// A frame parked for a later retry must still retire its sequence.
///
/// Deferral is routine — neither adding a member nor promoting one bumps the
/// epoch, so a connected peer legitimately sees a frame from a device its copy
/// of the log does not know yet. If the retry never advances `have_seq`, the
/// contiguous frontier pins below that frame for the life of the doc: every
/// reconnect re-downloads the same backlog and the compaction threshold never
/// fires again.
#[tokio::test]
async fn a_deferred_frame_retires_its_sequence_once_it_applies() {
    let server = TestServer::start_default().await;

    let owner = server.client();
    wait_connected(&owner).await;
    let space = owner.create_space().await.unwrap();
    let doc = owner.create_doc(space).await.unwrap();

    let peer = server.client();
    wait_connected(&peer).await;
    invite_and_join(&owner, &peer, space).await;
    peer.open_doc(space, doc).await.unwrap();
    converge(&[&owner, &peer], doc).await;

    // A late joiner: the peer above is already connected and has no reason to
    // refetch the log, so the newcomer's first frames arrive from a device the
    // peer's replica cannot yet authorise, and get deferred.
    let late = server.client();
    wait_connected(&late).await;
    invite_and_join(&owner, &late, space).await;
    late.open_doc(space, doc).await.unwrap();
    late.insert_text(doc, 0, "late;").await.unwrap();
    late.flush().await.unwrap();

    let text = converge(&[&owner, &peer, &late], doc).await;
    assert!(text.contains("late;"), "deferred frame never applied");

    // Applying it is not enough: the frontier has to move with it. Parking
    // happens before the sequence is retired, so if the retry does not retire
    // it the contiguous frontier pins below that frame for the life of the
    // doc — every reconnect re-downloads the same backlog, and the compaction
    // threshold never fires again. Nothing else in the replica looks wrong.
    let head: u64 = server
        .raw_db()
        .query_row(
            "SELECT head_seq FROM docs WHERE doc_id = ?",
            [&doc.as_bytes()[..]],
            |r| r.get::<_, i64>(0),
        )
        .unwrap() as u64;
    assert!(head > 0, "no updates were stored");

    let status = peer.status().await.unwrap();
    let peer_have = status.have_seq.get(&doc).copied().unwrap_or(0);
    assert_eq!(
        peer_have, head,
        "the peer applied the deferred frame but left its frontier at {peer_have} \
         of {head}: it will re-download this backlog on every reconnect"
    );
}

/// The relay will not compact the update log behind a snapshot that has not
/// settled, however acked it looks.
///
/// A snapshot is a full-state replace the relay cannot read, let alone verify
/// covers what it claims, and the "ack" is a heuristic — some other device
/// subscribed past it — not a confirmation that anyone decrypted or applied it.
/// Compacting the instant that heuristic fires means one bad snapshot destroys
/// the only other copy of the history, permanently and silently. The settling
/// window is what leaves that recoverable.
#[tokio::test]
async fn gc_holds_the_log_until_a_snapshot_has_settled() {
    let mut config = ServerConfig::default();
    config.snapshot_request_threshold = 4;
    config.snapshot_retention = Duration::from_millis(0); // ack heuristic fires
    config.snapshot_settle = Duration::from_secs(600); // ...but nothing has settled
    config.gc_interval = Duration::from_millis(100);
    let server = TestServer::start(config).await;

    let a = server.client();
    wait_connected(&a).await;
    let space = a.create_space().await.unwrap();
    let doc = a.create_doc(space).await.unwrap();

    for i in 0..16 {
        a.insert_text(doc, 0, format!("{i};")).await.unwrap();
        a.flush().await.unwrap();
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    converge(&[&a], doc).await;

    // A second device subscribing is exactly what trips the ack heuristic.
    let b = server.client();
    wait_connected(&b).await;
    invite_and_join(&a, &b, space).await;
    b.open_doc(space, doc).await.unwrap();
    converge(&[&a, &b], doc).await;
    tokio::time::sleep(Duration::from_millis(800)).await;

    let conn = server.raw_db();
    let snapshots: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM snapshots WHERE doc_id = ?",
            [&doc.as_bytes()[..]],
            |r| r.get(0),
        )
        .unwrap();
    assert!(snapshots >= 1, "server never received a snapshot");

    let acked: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM snapshots WHERE doc_id = ? AND acked = 1",
            [&doc.as_bytes()[..]],
            |r| r.get(0),
        )
        .unwrap();
    assert!(acked >= 1, "the ack heuristic never fired, so this proves nothing");

    let head: i64 = conn
        .query_row(
            "SELECT head_seq FROM docs WHERE doc_id = ?",
            [&doc.as_bytes()[..]],
            |r| r.get(0),
        )
        .unwrap();
    let updates: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM updates WHERE doc_id = ?",
            [&doc.as_bytes()[..]],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        updates, head,
        "an acked-but-unsettled snapshot let the GC delete the history behind it"
    );
}
