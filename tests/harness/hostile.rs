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
use enkr_proto::wire::{self, DevicePk, UpdateFrame};
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
}

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

    async fn bind_device_account(
        &self,
        device_pk: &DevicePk,
        account_id: Option<&Uuid>,
    ) -> Result<()> {
        self.inner.bind_device_account(device_pk, account_id).await
    }

    async fn space_owner_account(&self, space_id: &Uuid) -> Result<Option<Uuid>> {
        self.inner.space_owner_account(space_id).await
    }

    async fn recompute_usage(&self) -> Result<Vec<(Uuid, i64, i64)>> {
        self.inner.recompute_usage().await
    }

    async fn upsert_device(&self, device_pk: &DevicePk, kex_pk: &[u8; 32], now: i64) -> Result<()> {
        self.inner.upsert_device(device_pk, kex_pk, now).await
    }

    async fn space_epoch(&self, space_id: &Uuid) -> Result<Option<u32>> {
        self.inner.space_epoch(space_id).await
    }

    async fn create_space(
        &self,
        space_id: &Uuid,
        creator: &DevicePk,
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
        device_pk: &DevicePk,
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
    ) -> Result<()> {
        self.inner
            .remove_member(
                space_id, device_pk, new_epoch, op_seq, signed_op, envelopes, now,
            )
            .await
    }

    async fn is_active_member(&self, space_id: &Uuid, device_pk: &DevicePk) -> Result<bool> {
        self.inner.is_active_member(space_id, device_pk).await
    }

    async fn member_role(
        &self,
        space_id: &Uuid,
        device_pk: &DevicePk,
    ) -> Result<Option<MemberRole>> {
        self.inner.member_role(space_id, device_pk).await
    }

    async fn spaces_for_device(&self, device_pk: &DevicePk) -> Result<Vec<Uuid>> {
        self.inner.spaces_for_device(device_pk).await
    }

    async fn delete_space(&self, space_id: &Uuid) -> Result<()> {
        self.inner.delete_space(space_id).await
    }

    async fn next_membership_seq(&self, space_id: &Uuid) -> Result<u64> {
        self.inner.next_membership_seq(space_id).await
    }

    async fn membership_log(&self, space_id: &Uuid) -> Result<Vec<Vec<u8>>> {
        let mut ops = self.inner.membership_log(space_id).await?;
        let suppress = self
            .hostility
            .suppress_membership_ops
            .load(Ordering::Relaxed) as usize;
        ops.truncate(ops.len().saturating_sub(suppress));
        Ok(ops)
    }

    async fn envelopes_for_device(
        &self,
        space_id: &Uuid,
        device_pk: &DevicePk,
    ) -> Result<Vec<(u32, Vec<u8>)>> {
        self.inner.envelopes_for_device(space_id, device_pk).await
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

    async fn gc_eligible(&self, created_before: i64) -> Result<Vec<(Uuid, u64)>> {
        self.inner.gc_eligible(created_before).await
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
