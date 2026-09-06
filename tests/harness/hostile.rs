//! A `Store` that can be told to fail, and to lie.
//!
//! Two failure classes the relay has to survive and that no other harness can
//! produce: a storage backend that errors transiently (disk pressure, lock
//! contention on the single SQLite connection), and a *relay operator* that is
//! actively hostile — the untrusted-server threat model this protocol is built
//! around. Both are injected under the server rather than over the wire, which
//! keeps the tests free of a hand-rolled malicious relay.
//!
//! Delegation-only apart from the injected behaviour: if the `Store` trait grows
//! a method this stops compiling, which is the intended fail-loud signal.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use enkr_proto::membership::MemberRole;
use enkr_proto::wire::{self, IdentityPk, UpdateFrame};
use enkr_syncd::storage::{
    Account, EnvelopeRow, NewAccount, Result, SnapshotRow, SqliteStore, Store, StoreError,
};
use uuid::Uuid;

/// Switches the test flips to shape the relay's misbehaviour.
#[derive(Debug, Default)]
pub struct Hostility {
    /// Fail every `updates_since` — the read a cold subscribe depends on.
    pub fail_backlog_reads: AtomicBool,
    /// Serve the membership log with the last `suppress_membership_ops` entries
    /// withheld. This is the rollback attack `TODO.md:82` describes: a relay
    /// that hides the newest `Remove` keeps a revoked member looking current.
    pub suppress_membership_ops: AtomicU64,
    /// Replace only the unsigned snapshot metadata value returned by the store.
    /// The encoded `SnapshotFrame` remains untouched, simulating a lying relay.
    pub lie_snapshot_covers_seq: AtomicU64,
    /// Corrupt one stored update before it is sent in a backlog response.
    /// The corruption is one-shot so a later resync can recover the frame.
    pub corrupt_update_seq: AtomicU64,
    /// Replay one stored update as a later sequence number in a backlog.
    /// This simulates a relay presenting old signed content after revocation.
    pub replay_update_as_seq: AtomicU64,
    /// Append a forged key envelope for this epoch to every envelope fetch,
    /// sealed to the *requesting* device under a space key the relay chose.
    ///
    /// A sealed box is anonymous — wrapping one needs only the recipient's
    /// public kex key, which the relay has — so this needs no stolen secret
    /// and no cooperating member. Stored as `epoch + 1` so that `0` can mean
    /// "off" while still allowing epoch **0** to be forged — which is the case
    /// that matters, since a space that has never rotated is entirely at
    /// epoch 0 and an attack there needs no invented epoch at all.
    pub forge_envelope_epoch: AtomicU64,
    /// Who the forged envelope is sealed to (the victim's public kex key).
    pub forge_envelope_recipient: std::sync::Mutex<Option<[u8; 32]>>,
    /// Serve the frame from the *first* backlog entry in place of the one
    /// really stored at this seq, stored as `seq + 1` so `0` can mean "off".
    ///
    /// Distinct from `replay_update_as_seq`, which appends a duplicate
    /// *alongside* the real frame: there the genuine content still arrives and
    /// retires the seq. This substitutes, which is the shape that matters —
    /// the relay hides an update and covers the gap with one it already served,
    /// so a client counting sequences advances over content it never received.
    pub substitute_update_seq: AtomicU64,
    /// Withhold every key envelope for this epoch, stored as `epoch + 1` so
    /// that `0` can mean "off". Models a relay that simply does not hand a
    /// member the rotated key — the state in which a client must refuse to fall
    /// back to the epoch the removed member still holds.
    pub withhold_envelope_epoch: AtomicU64,
    /// Serve this membership log instead of the real one. `Create` is
    /// self-signed, so an attacker can mint a wholly valid log for a space they
    /// have nothing to do with — the substitution a client must refuse.
    pub substitute_membership_log: std::sync::Mutex<Option<Vec<Vec<u8>>>>,
}

impl Hostility {
    pub fn fail_backlog_reads(&self, on: bool) {
        self.fail_backlog_reads.store(on, Ordering::Relaxed);
    }

    pub fn suppress_membership_ops(&self, n: u64) {
        self.suppress_membership_ops.store(n, Ordering::Relaxed);
    }

    pub fn lie_snapshot_covers_seq(&self, n: u64) {
        self.lie_snapshot_covers_seq.store(n, Ordering::Relaxed);
    }

    pub fn corrupt_update_once(&self, seq: u64) {
        self.corrupt_update_seq.store(seq, Ordering::Relaxed);
    }

    pub fn replay_update_once_as(&self, seq: u64) {
        self.replay_update_as_seq.store(seq, Ordering::Relaxed);
    }

    pub fn substitute_update_seq(&self, seq: u64) {
        self.substitute_update_seq
            .store(seq + 1, Ordering::Relaxed);
    }

    pub fn withhold_envelope_epoch(&self, epoch: u32) {
        self.withhold_envelope_epoch
            .store(epoch as u64 + 1, Ordering::Relaxed);
    }

    pub fn substitute_membership_log(&self, ops: Vec<Vec<u8>>) {
        *self.substitute_membership_log.lock().unwrap() = Some(ops);
    }

    pub fn forge_envelope_for_epoch(&self, epoch: u32, recipient_kex: [u8; 32]) {
        *self.forge_envelope_recipient.lock().unwrap() = Some(recipient_kex);
        self.forge_envelope_epoch
            .store(epoch as u64 + 1, Ordering::Relaxed);
    }
}

/// The space key the hostile relay tries to make clients adopt. Fixed so a test
/// can check whether content ended up encrypted under it.
pub const FORGED_SPACE_KEY: [u8; 32] = [0xAB; 32];

pub struct HostileStore {
    pub inner: SqliteStore,
    pub hostility: Arc<Hostility>,
}

#[async_trait::async_trait]
impl Store for HostileStore {
    async fn create_account(&self, new: NewAccount<'_>) -> Result<Account> {
        self.inner.create_account(new).await
    }

    async fn account_by_token(&self, token_hash: &[u8; 32]) -> Result<Option<Account>> {
        self.inner.account_by_token(token_hash).await
    }

    async fn account(&self, account_id: &Uuid) -> Result<Option<Account>> {
        self.inner.account(account_id).await
    }

    async fn accounts(&self) -> Result<Vec<Account>> {
        self.inner.accounts().await
    }

    async fn delete_account(&self, account_id: &Uuid) -> Result<bool> {
        self.inner.delete_account(account_id).await
    }

    async fn set_account_expiry(&self, account_id: &Uuid, expires_at: Option<i64>) -> Result<bool> {
        self.inner.set_account_expiry(account_id, expires_at).await
    }

    async fn bind_identity_account(
        &self,
        identity_pk: &IdentityPk,
        account_id: Option<&Uuid>,
    ) -> Result<()> {
        self.inner.bind_identity_account(identity_pk, account_id).await
    }

    async fn space_owner_account(&self, space_id: &Uuid) -> Result<Option<Uuid>> {
        self.inner.space_owner_account(space_id).await
    }

    async fn recompute_usage(&self) -> Result<Vec<(Uuid, i64, i64)>> {
        self.inner.recompute_usage().await
    }

    async fn upsert_identity(&self, identity_pk: &IdentityPk, kex_pk: &[u8; 32], now: i64) -> Result<()> {
        self.inner.upsert_identity(identity_pk, kex_pk, now).await
    }

    async fn space_epoch(&self, space_id: &Uuid) -> Result<Option<u32>> {
        self.inner.space_epoch(space_id).await
    }

    async fn create_space(
        &self,
        space_id: &Uuid,
        creator: &IdentityPk,
        signed_op: &[u8],
        envelopes: &[EnvelopeRow],
        now: i64,
    ) -> Result<()> {
        self.inner
            .create_space(space_id, creator, signed_op, envelopes, now)
            .await
    }

    async fn add_member(
        &self,
        space_id: &Uuid,
        identity_pk: &IdentityPk,
        role: MemberRole,
        epoch_added: u32,
        op_seq: u64,
        signed_op: &[u8],
        envelopes: &[EnvelopeRow],
        now: i64,
    ) -> Result<()> {
        self.inner
            .add_member(
                space_id,
                identity_pk,
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
        identity_pk: &IdentityPk,
        new_epoch: u32,
        op_seq: u64,
        signed_op: &[u8],
        envelopes: &[EnvelopeRow],
        now: i64,
    ) -> Result<()> {
        self.inner
            .remove_member(
                space_id, identity_pk, new_epoch, op_seq, signed_op, envelopes, now,
            )
            .await
    }

    async fn is_active_member(&self, space_id: &Uuid, identity_pk: &IdentityPk) -> Result<bool> {
        self.inner.is_active_member(space_id, identity_pk).await
    }

    async fn member_role(
        &self,
        space_id: &Uuid,
        identity_pk: &IdentityPk,
    ) -> Result<Option<MemberRole>> {
        self.inner.member_role(space_id, identity_pk).await
    }

    async fn spaces_for_identity(&self, identity_pk: &IdentityPk) -> Result<Vec<Uuid>> {
        self.inner.spaces_for_identity(identity_pk).await
    }

    async fn delete_space(&self, space_id: &Uuid) -> Result<()> {
        self.inner.delete_space(space_id).await
    }

    async fn next_membership_seq(&self, space_id: &Uuid) -> Result<u64> {
        self.inner.next_membership_seq(space_id).await
    }

    async fn membership_log(&self, space_id: &Uuid) -> Result<Vec<Vec<u8>>> {
        if let Some(ops) = self.hostility.substitute_membership_log.lock().unwrap().clone() {
            return Ok(ops);
        }
        let mut ops = self.inner.membership_log(space_id).await?;
        let suppress = self
            .hostility
            .suppress_membership_ops
            .load(Ordering::Relaxed) as usize;
        ops.truncate(ops.len().saturating_sub(suppress));
        Ok(ops)
    }

    async fn envelopes_for_identity(
        &self,
        space_id: &Uuid,
        identity_pk: &IdentityPk,
    ) -> Result<Vec<(u32, Vec<u8>)>> {
        let mut envelopes = self.inner.envelopes_for_identity(space_id, identity_pk).await?;
        if let withheld @ 1.. = self.hostility.withhold_envelope_epoch.load(Ordering::Relaxed) {
            envelopes.retain(|(epoch, _)| *epoch != (withheld - 1) as u32);
        }
        let forged = match self.hostility.forge_envelope_epoch.load(Ordering::Relaxed) {
            0 => return Ok(envelopes),
            stored => (stored - 1) as u32,
        };
        {
            // Replace, not append: a relay that wants a device to adopt its key
            // simply withholds the genuine envelope for that epoch. Appending
            // would only ever test the tie-break.
            envelopes.retain(|(epoch, _)| *epoch != forged);
            // The relay knows every member's public kex key (it stores them and
            // they travel in the membership log), and sealing needs nothing
            // else. So it can offer any device a key of its choosing for any
            // epoch it likes.
            let Some(kex_pk) = *self.hostility.forge_envelope_recipient.lock().unwrap() else {
                return Ok(envelopes);
            };
            let sealed = enkr_proto::crypto::seal_space_key(
                &kex_pk,
                &enkr_proto::crypto::SpaceKey(FORGED_SPACE_KEY),
            );
            envelopes.push((forged, sealed));
        }
        Ok(envelopes)
    }

    async fn create_doc(&self, doc_id: &Uuid, space_id: &Uuid, now: i64) -> Result<()> {
        self.inner.create_doc(doc_id, space_id, now).await
    }

    async fn doc_space(&self, doc_id: &Uuid) -> Result<Option<Uuid>> {
        self.inner.doc_space(doc_id).await
    }

    async fn doc_spaces(&self, doc_ids: &[Uuid]) -> Result<Vec<(Uuid, Uuid)>> {
        self.inner.doc_spaces(doc_ids).await
    }

    async fn append_update(
        &self,
        doc_id: &Uuid,
        frame: &[u8],
        epoch: u32,
        now: i64,
    ) -> Result<u64> {
        self.inner.append_update(doc_id, frame, epoch, now).await
    }

    async fn updates_since(
        &self,
        doc_id: &Uuid,
        after_seq: u64,
        limit: u32,
    ) -> Result<Vec<(u64, Vec<u8>)>> {
        if self.hostility.fail_backlog_reads.load(Ordering::Relaxed) {
            return Err(StoreError::NotFound("injected backlog read failure"));
        }
        let mut updates = self.inner.updates_since(doc_id, after_seq, limit).await?;
        let target = self.hostility.corrupt_update_seq.load(Ordering::Relaxed);
        if target != 0 {
            for (seq, bytes) in &mut updates {
                if *seq != target {
                    continue;
                }
                let Ok(mut frame) = wire::decode::<UpdateFrame>(bytes) else {
                    continue;
                };
                let Some(last) = frame.ciphertext.last_mut() else {
                    continue;
                };
                *last ^= 0x01;
                *bytes = wire::encode(&frame).expect("update frame encodes");
                let _ = self.hostility.corrupt_update_seq.compare_exchange(
                    target,
                    0,
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                );
                break;
            }
        }
        if let stored @ 1.. = self.hostility.substitute_update_seq.load(Ordering::Relaxed) {
            let target = stored - 1;
            if let Some((_, first)) = updates.first().cloned() {
                for (seq, bytes) in &mut updates {
                    if *seq == target {
                        *bytes = first;
                        break;
                    }
                }
            }
        }
        let replay_seq = self.hostility.replay_update_as_seq.load(Ordering::Relaxed);
        if replay_seq != 0
            && let Some((_, bytes)) = updates.first().cloned()
        {
            updates.push((replay_seq, bytes));
            let _ = self.hostility.replay_update_as_seq.compare_exchange(
                replay_seq,
                0,
                Ordering::Relaxed,
                Ordering::Relaxed,
            );
        }
        Ok(updates)
    }

    async fn head_seq(&self, doc_id: &Uuid) -> Result<u64> {
        self.inner.head_seq(doc_id).await
    }

    async fn put_snapshot(&self, snapshot: &SnapshotRow) -> Result<bool> {
        self.inner.put_snapshot(snapshot).await
    }

    async fn latest_snapshot(&self, doc_id: &Uuid) -> Result<Option<SnapshotRow>> {
        let snapshot = self.inner.latest_snapshot(doc_id).await?;
        let lie = self
            .hostility
            .lie_snapshot_covers_seq
            .load(Ordering::Relaxed);
        Ok(snapshot.map(|mut snapshot| {
            if lie != 0 {
                snapshot.covers_seq = lie;
            }
            snapshot
        }))
    }

    async fn ack_snapshot(&self, doc_id: &Uuid, covers_seq: u64) -> Result<()> {
        self.inner.ack_snapshot(doc_id, covers_seq).await
    }

    async fn gc_eligible(
        &self,
        settled_before: i64,
        unacked_before: i64,
    ) -> Result<Vec<(Uuid, u64)>> {
        self.inner.gc_eligible(settled_before, unacked_before).await
    }

    async fn gc_updates_through(&self, doc_id: &Uuid, seq: u64) -> Result<u64> {
        self.inner.gc_updates_through(doc_id, seq).await
    }

    async fn gc_envelopes(&self) -> Result<u64> {
        self.inner.gc_envelopes().await
    }

    async fn put_blob(
        &self,
        blob_id: &Uuid,
        space_id: &Uuid,
        bytes: &[u8],
        now: i64,
    ) -> Result<()> {
        self.inner.put_blob(blob_id, space_id, bytes, now).await
    }

    async fn get_blob(&self, blob_id: &Uuid) -> Result<Option<Vec<u8>>> {
        self.inner.get_blob(blob_id).await
    }

    async fn delete_blob(&self, blob_id: &Uuid, space_id: &Uuid) -> Result<()> {
        self.inner.delete_blob(blob_id, space_id).await
    }
}
