//! A `Store` decorator that tallies the server-side calls the scale budgets
//! care about, so a test can assert on *work done* rather than only on
//! wall-clock (which is noisy on a loaded CI box).
//!
//! Delegation-only: if the `Store` trait grows a method this stops compiling,
//! which is the intended fail-loud signal.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use enkr_proto::membership::MemberRole;
use enkr_proto::wire::IdentityPk;
use enkr_syncd::storage::{
    Account, EnvelopeRow, NewAccount, Result, SnapshotRow, SqliteStore, Store,
};
use uuid::Uuid;

/// One counter per metered call. `Relaxed` throughout: these are tallies read
/// after the fact, never used to order anything.
#[derive(Debug, Default)]
pub struct StoreMetrics {
    /// One per `Subscribe` entry, plus one per relayed ephemeral.
    pub doc_space: AtomicU64,
    /// Batched doc→space resolution: one call per `Subscribe` message.
    pub doc_spaces: AtomicU64,
    /// The ACL check on every push *and* every presence ping.
    pub is_active_member: AtomicU64,
    /// Backlog pages served.
    pub updates_since: AtomicU64,
    pub latest_snapshot: AtomicU64,
    pub put_snapshot: AtomicU64,
    /// Full membership-log fetches (no cursor today).
    pub membership_log: AtomicU64,
    pub envelopes_for_identity: AtomicU64,
    /// The unindexed server-wide scan.
    pub spaces_for_identity: AtomicU64,
    pub append_update: AtomicU64,
    pub create_doc: AtomicU64,
    pub gc_eligible: AtomicU64,
    pub gc_envelopes: AtomicU64,
}

impl StoreMetrics {
    fn bump(counter: &AtomicU64) {
        counter.fetch_add(1, Ordering::Relaxed);
    }

    pub fn get(counter: &AtomicU64) -> u64 {
        counter.load(Ordering::Relaxed)
    }

    pub fn reset(&self) {
        for counter in [
            &self.doc_space,
            &self.doc_spaces,
            &self.is_active_member,
            &self.updates_since,
            &self.latest_snapshot,
            &self.put_snapshot,
            &self.membership_log,
            &self.envelopes_for_identity,
            &self.spaces_for_identity,
            &self.append_update,
            &self.create_doc,
            &self.gc_eligible,
            &self.gc_envelopes,
        ] {
            counter.store(0, Ordering::Relaxed);
        }
    }

    /// One-line dump for `--nocapture` runs.
    pub fn report(&self, label: &str) {
        println!(
            "[{label}] doc_space={} doc_spaces={} is_active_member={} updates_since={} latest_snapshot={} \
             put_snapshot={} membership_log={} envelopes_for_identity={} spaces_for_identity={} \
             append_update={} create_doc={} gc_eligible={} gc_envelopes={}",
            Self::get(&self.doc_space),
            Self::get(&self.doc_spaces),
            Self::get(&self.is_active_member),
            Self::get(&self.updates_since),
            Self::get(&self.latest_snapshot),
            Self::get(&self.put_snapshot),
            Self::get(&self.membership_log),
            Self::get(&self.envelopes_for_identity),
            Self::get(&self.spaces_for_identity),
            Self::get(&self.append_update),
            Self::get(&self.create_doc),
            Self::get(&self.gc_eligible),
            Self::get(&self.gc_envelopes),
        );
    }
}

pub struct MeteredStore {
    pub inner: SqliteStore,
    pub metrics: Arc<StoreMetrics>,
}

#[async_trait::async_trait]
impl Store for MeteredStore {
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
        StoreMetrics::bump(&self.metrics.is_active_member);
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
        StoreMetrics::bump(&self.metrics.spaces_for_identity);
        self.inner.spaces_for_identity(identity_pk).await
    }

    async fn delete_space(&self, space_id: &Uuid) -> Result<()> {
        self.inner.delete_space(space_id).await
    }

    async fn next_membership_seq(&self, space_id: &Uuid) -> Result<u64> {
        self.inner.next_membership_seq(space_id).await
    }

    async fn membership_log(&self, space_id: &Uuid) -> Result<Vec<Vec<u8>>> {
        StoreMetrics::bump(&self.metrics.membership_log);
        self.inner.membership_log(space_id).await
    }

    async fn envelopes_for_identity(
        &self,
        space_id: &Uuid,
        identity_pk: &IdentityPk,
    ) -> Result<Vec<(u32, Vec<u8>)>> {
        StoreMetrics::bump(&self.metrics.envelopes_for_identity);
        self.inner.envelopes_for_identity(space_id, identity_pk).await
    }

    async fn create_doc(&self, doc_id: &Uuid, space_id: &Uuid, now: i64) -> Result<()> {
        StoreMetrics::bump(&self.metrics.create_doc);
        self.inner.create_doc(doc_id, space_id, now).await
    }

    async fn doc_space(&self, doc_id: &Uuid) -> Result<Option<Uuid>> {
        StoreMetrics::bump(&self.metrics.doc_space);
        self.inner.doc_space(doc_id).await
    }

    async fn doc_spaces(&self, doc_ids: &[Uuid]) -> Result<Vec<(Uuid, Uuid)>> {
        StoreMetrics::bump(&self.metrics.doc_spaces);
        self.inner.doc_spaces(doc_ids).await
    }

    async fn append_update(
        &self,
        doc_id: &Uuid,
        frame: &[u8],
        epoch: u32,
        now: i64,
    ) -> Result<u64> {
        StoreMetrics::bump(&self.metrics.append_update);
        self.inner.append_update(doc_id, frame, epoch, now).await
    }

    async fn updates_since(
        &self,
        doc_id: &Uuid,
        after_seq: u64,
        limit: u32,
    ) -> Result<Vec<(u64, Vec<u8>)>> {
        StoreMetrics::bump(&self.metrics.updates_since);
        self.inner.updates_since(doc_id, after_seq, limit).await
    }

    async fn head_seq(&self, doc_id: &Uuid) -> Result<u64> {
        self.inner.head_seq(doc_id).await
    }

    async fn put_snapshot(&self, snapshot: &SnapshotRow) -> Result<bool> {
        StoreMetrics::bump(&self.metrics.put_snapshot);
        self.inner.put_snapshot(snapshot).await
    }

    async fn latest_snapshot(&self, doc_id: &Uuid) -> Result<Option<SnapshotRow>> {
        StoreMetrics::bump(&self.metrics.latest_snapshot);
        self.inner.latest_snapshot(doc_id).await
    }

    async fn ack_snapshot(&self, doc_id: &Uuid, covers_seq: u64) -> Result<()> {
        self.inner.ack_snapshot(doc_id, covers_seq).await
    }

    async fn gc_eligible(&self, created_before: i64) -> Result<Vec<(Uuid, u64)>> {
        StoreMetrics::bump(&self.metrics.gc_eligible);
        self.inner.gc_eligible(created_before).await
    }

    async fn gc_updates_through(&self, doc_id: &Uuid, seq: u64) -> Result<u64> {
        self.inner.gc_updates_through(doc_id, seq).await
    }

    async fn gc_envelopes(&self) -> Result<u64> {
        StoreMetrics::bump(&self.metrics.gc_envelopes);
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
