//! Shared test doubles and fixtures — one configurable mock per port plus a
//! [`Harness`] builder, so a port signature change is edited **here once** rather
//! than across every `#[cfg(test)]` module. Mocks expose `pub` fields with a
//! `Default`, so a test overrides only what it asserts on
//! (`MockRpc { gas_reverts: true, ..Default::default() }`) and shares one
//! [`CallLog`] across mocks to assert pipeline ordering.

use crate::core::deps::{
    AccountActivity, Clock, GasOracle, GasOracleError, NonceManager, NonceManagerError,
    PolicyEngine, PolicyEngineError, Rpc, RpcError, Signer, SignerError, Simulated, StateStore,
    StateStoreError, SubmissionError, SubmissionOpts, SubmissionStrategy, Versioned,
};
use crate::core::wallet::{
    AccountExecutor, Decision, FenceToken, GasEnvelope, HandleId, IntentHash, NonceScope,
    NonceState, PolicyApproval, PolicyRejection, SignatureEnvelope, SigningRequest,
    TransactionManager, TxHandle, TxIntent, TxStatus,
};
use alloy_consensus::{
    Receipt, ReceiptEnvelope, ReceiptWithBloom, SignableTransaction, TxEip1559, TxLegacy,
};
use alloy_dyn_abi::TypedData;
use alloy_eips::Encodable2718;
use alloy_eips::eip1559::Eip1559Estimation;
use alloy_primitives::{Address, B256, Bytes, Signature, TxHash, TxKind, U256};
use alloy_rpc_types_eth::{AccessListResult, TransactionReceipt, TransactionRequest};
use async_trait::async_trait;
use parking_lot::Mutex;
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

/// An ordered log of the port calls a pipeline made, shared across mocks to assert
/// stage order (e.g. estimate → policy → sign → submit).
pub(crate) type CallLog = Arc<Mutex<Vec<&'static str>>>;

pub(crate) fn shared_log() -> CallLog {
    Arc::new(Mutex::new(Vec::new()))
}

fn note(log: &CallLog, event: &'static str) {
    log.lock().push(event);
}

pub(crate) fn estimation(max_fee: u128, max_priority: u128) -> Eip1559Estimation {
    Eip1559Estimation {
        max_fee_per_gas: max_fee,
        max_priority_fee_per_gas: max_priority,
    }
}

/// A minimal value-transfer intent from the zero account; tests tweak fields as needed.
pub(crate) fn intent() -> TxIntent {
    TxIntent {
        chain_id: 1,
        account: Address::ZERO,
        to: TxKind::Call(Address::from([0xaa; 20])),
        value: U256::ZERO,
        input: Bytes::new(),
        purpose: None,
    }
}

/// An in-flight handle for the zero account, carrying a real (decodable) signed tx so
/// bump logic can read its fees/gas; the default envelope admits typical bumps.
pub(crate) fn handle(nonce: u64, status: TxStatus) -> TxHandle {
    let intent = intent();
    let intent_hash = intent.hash();
    TxHandle {
        id: HandleId::new(intent_hash, nonce),
        account: Address::ZERO,
        intent,
        intent_hash,
        nonce,
        status,
        envelope: GasEnvelope::DEFAULT,
        signed: signed_tx(nonce),
        broadcasts: vec![TxHash::ZERO],
        last_broadcast_at: 0,
        cancelled: false,
    }
}

/// A handle for a specific `account` (the base [`handle`] uses the zero account) — the
/// store conformance suite uses a non-zero account so it doesn't collide with other tests
/// that share a process-global backend.
pub(crate) fn handle_for(account: Address, nonce: u64, status: TxStatus) -> TxHandle {
    let mut h = handle(nonce, status);
    h.account = account;
    h
}

/// The contract every [`StateStore`] backend must satisfy — run from each adapter's tests
/// so all backends behave identically: nonce CAS (commit + version-conflict), the handle
/// WAL (upsert/get), `pending_handles` excluding terminal, and upsert-by-id.
pub(crate) async fn state_store_conformance(store: Arc<dyn StateStore>) {
    let account = Address::from([0x11; 20]);
    let scope = NonceScope::eoa(account);

    // nonce CAS: the first commit succeeds; a stale expected_version is a conflict (not err).
    assert_eq!(store.load_nonce_state(scope).await.unwrap().version, 0);
    let s = NonceState {
        next: 5,
        ..Default::default()
    };
    assert!(
        store
            .cas_nonce_state(scope, 0, &s, FenceToken::SINGLE_WRITER)
            .await
            .unwrap(),
        "first CAS commits"
    );
    assert!(
        !store
            .cas_nonce_state(scope, 0, &s, FenceToken::SINGLE_WRITER)
            .await
            .unwrap(),
        "a stale version is a conflict, not an error"
    );
    let v1 = store.load_nonce_state(scope).await.unwrap();
    assert_eq!(v1.version, 1);
    assert_eq!(v1.value.next, 5);

    // handle WAL: upsert / get / pending excludes terminal.
    let sent = handle_for(account, 5, TxStatus::Sent);
    let done = handle_for(account, 6, TxStatus::Confirmed { block: 9 });
    store.put_handle(&sent).await.unwrap();
    store.put_handle(&done).await.unwrap();
    assert_eq!(store.handle(sent.id).await.unwrap().unwrap().nonce, 5);
    assert_eq!(
        store.handle(done.id).await.unwrap().unwrap().status,
        TxStatus::Confirmed { block: 9 }
    );
    // an unknown id reads as None.
    assert!(
        store
            .handle(handle_for(account, 99, TxStatus::Sent).id)
            .await
            .unwrap()
            .is_none(),
        "unknown id reads as None"
    );
    let pending = store.pending_handles(account).await.unwrap();
    assert_eq!(pending.len(), 1, "terminal handle excluded from pending");
    assert_eq!(pending[0].nonce, 5);

    // upsert-by-id: the live handle reaching terminal empties pending.
    let mut sent2 = sent.clone();
    sent2.status = TxStatus::Confirmed { block: 12 };
    store.put_handle(&sent2).await.unwrap();
    assert!(
        store.pending_handles(account).await.unwrap().is_empty(),
        "all handles now terminal"
    );

    // fence: a token below the high-water is rejected (a superseded owner must stop, not
    // retry); a higher one commits and raises the high-water. On a fresh account so it
    // doesn't disturb the CAS section above.
    let facct = Address::from([0x33; 20]);
    let fscope = NonceScope::eoa(facct);
    let base = NonceState::default();
    assert!(
        store
            .cas_nonce_state(fscope, 0, &base, FenceToken::SINGLE_WRITER)
            .await
            .unwrap()
    );
    assert!(
        store
            .cas_nonce_state(fscope, 1, &base, FenceToken::for_test(1))
            .await
            .unwrap(),
        "a higher fence commits and raises the high-water"
    );
    assert!(
        matches!(
            store
                .cas_nonce_state(fscope, 2, &base, FenceToken::SINGLE_WRITER)
                .await,
            Err(StateStoreError::Fenced)
        ),
        "a fence below the high-water is rejected even at the right version"
    );
}

/// The behavioral contract every backend must satisfy under the real [`LocalNonceManager`]
/// — run from each store's tests so allocate/release/reset and concurrent CAS behave
/// identically on in-memory, redb, and Postgres. Each scenario uses a distinct account, so
/// the suite is safe to run against a shared backend and alongside
/// [`state_store_conformance`]. Call it from a `multi_thread` test so the concurrency
/// section exercises real parallelism.
pub(crate) async fn nonce_manager_conformance(store: Arc<dyn StateStore>) {
    use crate::adapters::LocalNonceManager;

    // A manager over the shared store whose chain view reports `pending` as the next nonce.
    let mgr = |pending: u64| {
        LocalNonceManager::new(
            store.clone(),
            Arc::new(MockRpc {
                pending_nonce: pending,
                ..Default::default()
            }),
        )
    };

    // allocate: gapless, reconciling from the chain on first use.
    {
        let a = Address::from([0x40; 20]);
        let m = mgr(5);
        assert_eq!(m.allocate(a).await.unwrap(), 5);
        assert_eq!(m.allocate(a).await.unwrap(), 6);
        assert_eq!(m.allocate(a).await.unwrap(), 7);
    }

    // release the top: shrink the high-water and absorb now-contiguous freed nonces.
    {
        let a = Address::from([0x41; 20]);
        let m = mgr(5);
        for _ in 0..3 {
            m.allocate(a).await.unwrap(); // 5,6,7 -> next=8
        }
        m.release(a, 6).await.unwrap(); // middle gap -> free={6}
        m.release(a, 7).await.unwrap(); // top -> next 8->7, absorbs 6 -> next=6
        assert_eq!(m.allocate(a).await.unwrap(), 6);
    }

    // release a middle gap: recycle the lowest freed nonce first, then fresh.
    {
        let a = Address::from([0x42; 20]);
        let m = mgr(5);
        for _ in 0..3 {
            m.allocate(a).await.unwrap();
        }
        m.release(a, 6).await.unwrap();
        assert_eq!(m.allocate(a).await.unwrap(), 6); // recycle freed first
        assert_eq!(m.allocate(a).await.unwrap(), 8); // then fresh
    }

    // reset moves forward only — a stale reset never rewinds the high-water.
    {
        let a = Address::from([0x43; 20]);
        let m = mgr(5);
        for _ in 0..3 {
            m.allocate(a).await.unwrap();
        }
        m.reset(a, 10).await.unwrap();
        assert_eq!(m.allocate(a).await.unwrap(), 10);
        m.reset(a, 3).await.unwrap();
        assert_eq!(m.allocate(a).await.unwrap(), 11);
    }

    // reset drops freed nonces the chain already consumed but keeps those at or above
    // `chain_next` — the `>=` boundary (75 kept, 74 dropped).
    {
        use std::collections::BTreeSet;
        let a = Address::from([0x44; 20]);
        let scope = NonceScope::eoa(a);
        let seeded = NonceState {
            next: 100,
            free: BTreeSet::from([74, 75, 150]),
        };
        assert!(
            store
                .cas_nonce_state(scope, 0, &seeded, FenceToken::SINGLE_WRITER)
                .await
                .unwrap()
        );
        let m = mgr(0);
        m.reset(a, 75).await.unwrap();
        let after = store.load_nonce_state(scope).await.unwrap().value;
        assert_eq!(after.next, 100); // max(100, 75) — forward only
        assert_eq!(after.free, BTreeSet::from([75, 150])); // 74 dropped, 75 kept (>=)
        assert_eq!(m.allocate(a).await.unwrap(), 75);
        assert_eq!(m.allocate(a).await.unwrap(), 150);
        assert_eq!(m.allocate(a).await.unwrap(), 100);
        assert_eq!(m.allocate(a).await.unwrap(), 101);
    }

    // concurrent allocations never duplicate a nonce — the CAS-retry loop under contention.
    {
        let a = Address::from([0x45; 20]);
        let m = Arc::new(mgr(5));
        let tasks: Vec<_> = (0..50)
            .map(|_| {
                let m = m.clone();
                tokio::spawn(async move { m.allocate(a).await.unwrap() })
            })
            .collect();
        let mut nonces = Vec::new();
        for t in tasks {
            nonces.push(t.await.unwrap());
        }
        nonces.sort_unstable();
        assert_eq!(nonces, (5..55).collect::<Vec<_>>()); // 50 unique & contiguous
    }
}

/// A real EIP-1559 signed-tx encoding (fees 100/1, gas 21_000) — a decodable body for
/// the bump path, which recovers fees/gas from the persisted `signed` bytes.
fn signed_tx(nonce: u64) -> Bytes {
    let tx = TxEip1559 {
        chain_id: 1,
        nonce,
        gas_limit: 21_000,
        max_fee_per_gas: 100,
        max_priority_fee_per_gas: 1,
        to: TxKind::Create,
        value: U256::ZERO,
        input: Bytes::new(),
        access_list: Default::default(),
    };
    let signature = Signature::new(U256::from(1), U256::from(1), false);
    Bytes::from(tx.into_signed(signature).encoded_2718())
}

/// A real *legacy* signed-tx encoding — decodes cleanly via EIP-2718 but is not
/// EIP-1559, so `decode_fees` must reject it (the bump path only reconstructs 1559).
pub(crate) fn signed_legacy(nonce: u64) -> Bytes {
    let tx = TxLegacy {
        chain_id: Some(1),
        nonce,
        gas_price: 100,
        gas_limit: 21_000,
        to: TxKind::Create,
        value: U256::ZERO,
        input: Bytes::new(),
    };
    let signature = Signature::new(U256::from(1), U256::from(1), false);
    Bytes::from(tx.into_signed(signature).encoded_2718())
}

pub(crate) fn receipt(success: bool, block: u64, block_hash: B256) -> TransactionReceipt {
    TransactionReceipt {
        inner: ReceiptEnvelope::Eip1559(ReceiptWithBloom {
            receipt: Receipt {
                status: success.into(),
                cumulative_gas_used: 0,
                logs: vec![],
            },
            logs_bloom: Default::default(),
        }),
        transaction_hash: TxHash::ZERO,
        transaction_index: None,
        block_hash: Some(block_hash),
        block_number: Some(block),
        gas_used: 0,
        effective_gas_price: 0,
        blob_gas_used: None,
        blob_gas_price: None,
        from: Address::ZERO,
        to: None,
        contract_address: None,
    }
}

/// A receipt with no block anchor (block number/hash = `None`) — the pending/partial
/// shape that `anchor()` must treat as `Unknown` rather than trust.
pub(crate) fn receipt_unanchored() -> TransactionReceipt {
    let mut r = receipt(true, 0, B256::ZERO);
    r.block_number = None;
    r.block_hash = None;
    r
}

// ---- Clock ----

pub(crate) struct MockClock(pub u64);
impl Clock for MockClock {
    fn now_unix(&self) -> u64 {
        self.0
    }
}

// ---- Rpc ----

/// Every read returns its field; `estimate_gas` logs and can simulate a revert.
/// `finalized` is the `finalized`-tag head (`None` => depth mode); `canonical` is the
/// hash `block_hash(_)` returns for receipt anchoring (`None` => no such block).
/// `receipts` maps a specific broadcast hash to its receipt (for RBF newest-first tests);
/// a hash not in the map falls back to the scalar `receipt`.
#[derive(Default)]
pub(crate) struct MockRpc {
    pub pending_nonce: u64,
    pub tx_count: u64,
    pub block_number: u64,
    /// FIFO of heads consumed one per `block_number()` call — for the head-regression
    /// guard, where one executor must see a lower head the next cycle. Empty falls back
    /// to `block_number`, so existing fixed-head tests are unaffected.
    pub block_numbers: Mutex<VecDeque<u64>>,
    pub finalized: Option<u64>,
    pub base_fee: u128,
    pub receipt: Option<TransactionReceipt>,
    pub receipts: HashMap<TxHash, TransactionReceipt>,
    /// When set, `receipt()` returns a transient error — for the read-robustness path
    /// where an ambiguous receipt read must become `ChainEvent::Unknown`.
    pub receipt_err: bool,
    pub canonical: Option<B256>,
    pub gas_reverts: bool,
    pub log: CallLog,
}

#[async_trait]
impl Rpc for MockRpc {
    async fn pending_nonce(&self, _: Address) -> Result<u64, RpcError> {
        Ok(self.pending_nonce)
    }
    async fn tx_count(&self, _: Address) -> Result<u64, RpcError> {
        Ok(self.tx_count)
    }
    async fn block_number(&self) -> Result<u64, RpcError> {
        Ok(self
            .block_numbers
            .lock()
            .pop_front()
            .unwrap_or(self.block_number))
    }
    async fn finalized_block(&self) -> Result<Option<u64>, RpcError> {
        Ok(self.finalized)
    }
    async fn block_hash(&self, _: u64) -> Result<Option<B256>, RpcError> {
        Ok(self.canonical)
    }
    async fn estimate_fees(&self) -> Result<Eip1559Estimation, RpcError> {
        Ok(estimation(0, 0))
    }
    async fn base_fee(&self) -> Result<u128, RpcError> {
        Ok(self.base_fee)
    }
    async fn estimate_gas(&self, _: &TransactionRequest) -> Result<u64, RpcError> {
        note(&self.log, "estimate_gas");
        if self.gas_reverts {
            // A revert surfaces from estimate_gas as a deterministic (non-transient) error.
            Err(RpcError::Call {
                message: "execution reverted".into(),
                transient: false,
            })
        } else {
            Ok(21_000)
        }
    }
    async fn call(&self, _: &TransactionRequest) -> Result<Simulated, RpcError> {
        Ok(Simulated::Returned(Bytes::new()))
    }
    async fn create_access_list(
        &self,
        _: &TransactionRequest,
    ) -> Result<AccessListResult, RpcError> {
        Ok(AccessListResult::default())
    }
    async fn send_raw(&self, _: Bytes) -> Result<TxHash, RpcError> {
        Ok(TxHash::ZERO)
    }
    async fn receipt(&self, hash: TxHash) -> Result<Option<TransactionReceipt>, RpcError> {
        if self.receipt_err {
            return Err(RpcError::Call {
                message: "receipt read failed".into(),
                transient: true,
            });
        }
        match self.receipts.get(&hash) {
            Some(receipt) => Ok(Some(receipt.clone())),
            None => Ok(self.receipt.clone()),
        }
    }
    async fn account_activity(
        &self,
        accounts: &[Address],
    ) -> Result<Vec<AccountActivity>, RpcError> {
        Ok(accounts
            .iter()
            .map(|_| AccountActivity {
                nonce: self.tx_count,
                balance: U256::ZERO,
            })
            .collect())
    }
}

// ---- GasOracle ----

pub(crate) struct MockGas {
    pub estimate: Eip1559Estimation,
    /// `None` models the oracle at its ceiling (bump refuses).
    pub bump: Option<Eip1559Estimation>,
    pub log: CallLog,
}

impl Default for MockGas {
    fn default() -> Self {
        Self {
            estimate: estimation(100, 1),
            bump: None,
            log: shared_log(),
        }
    }
}

#[async_trait]
impl GasOracle for MockGas {
    async fn estimate(&self) -> Result<Eip1559Estimation, GasOracleError> {
        note(&self.log, "fees");
        Ok(self.estimate)
    }
    async fn bump(&self, _: Eip1559Estimation) -> Result<Eip1559Estimation, GasOracleError> {
        self.bump.ok_or(GasOracleError::CeilingExceeded {
            ceiling: 0,
            needed: 1,
        })
    }
}

// ---- PolicyEngine ----

pub(crate) struct MockPolicy {
    pub allow: bool,
    pub calls: Arc<Mutex<u32>>,
    pub envelope: GasEnvelope,
    pub valid_until: u64,
    pub log: CallLog,
}

impl Default for MockPolicy {
    fn default() -> Self {
        Self {
            allow: true,
            calls: Arc::new(Mutex::new(0)),
            envelope: GasEnvelope::DEFAULT,
            valid_until: u64::MAX,
            log: shared_log(),
        }
    }
}

#[async_trait]
impl PolicyEngine for MockPolicy {
    async fn evaluate(&self, request: &SigningRequest) -> Result<Decision, PolicyEngineError> {
        note(&self.log, "policy");
        *self.calls.lock() += 1;
        Ok(if self.allow {
            let payload_hash = request.signing_hash().unwrap_or_default();
            Decision::Allow(PolicyApproval::mint(
                payload_hash,
                self.envelope,
                self.valid_until,
            ))
        } else {
            Decision::Deny(PolicyRejection {
                rule: "test".into(),
                field: None,
                reason: "blocked".into(),
            })
        })
    }
}

// ---- NonceManager ----

#[derive(Default)]
pub(crate) struct MockNonce {
    /// The nonce `allocate` hands out.
    pub next: u64,
    pub log: CallLog,
}

#[async_trait]
impl NonceManager for MockNonce {
    async fn allocate(&self, _: Address) -> Result<u64, NonceManagerError> {
        note(&self.log, "allocate");
        Ok(self.next)
    }
    async fn release(&self, _: Address, _: u64) -> Result<(), NonceManagerError> {
        note(&self.log, "release");
        Ok(())
    }
    async fn reset(&self, _: Address, _: u64) -> Result<(), NonceManagerError> {
        note(&self.log, "reset");
        Ok(())
    }
}

// ---- Signer ----

pub(crate) struct MockSigner {
    pub address: Address,
    pub ok: bool,
    pub log: CallLog,
}

impl Default for MockSigner {
    fn default() -> Self {
        Self {
            address: Address::ZERO,
            ok: true,
            log: shared_log(),
        }
    }
}

#[async_trait]
impl Signer for MockSigner {
    fn address(&self) -> Address {
        self.address
    }
    async fn sign_transaction(
        &self,
        _: &TxEip1559,
        _: IntentHash,
        _: &PolicyApproval,
        _: u64,
    ) -> Result<Signature, SignerError> {
        note(&self.log, "sign");
        if self.ok {
            Ok(Signature::new(U256::from(1), U256::from(1), false))
        } else {
            Err(SignerError::Backend("boom".into()))
        }
    }

    async fn sign_message(
        &self,
        _: &[u8],
        _: &PolicyApproval,
        _: u64,
    ) -> Result<SignatureEnvelope, SignerError> {
        note(&self.log, "sign");
        self.envelope()
    }

    async fn sign_typed_data(
        &self,
        _: &TypedData,
        _: &PolicyApproval,
        _: u64,
    ) -> Result<SignatureEnvelope, SignerError> {
        note(&self.log, "sign");
        self.envelope()
    }
}

impl MockSigner {
    fn envelope(&self) -> Result<SignatureEnvelope, SignerError> {
        if self.ok {
            Ok(SignatureEnvelope::secp256k1(
                self.address,
                Signature::new(U256::from(1), U256::from(1), false),
            ))
        } else {
            Err(SignerError::Backend("boom".into()))
        }
    }
}

// ---- SubmissionStrategy ----

#[derive(Clone, Copy, Default)]
pub(crate) enum Submit {
    #[default]
    Ok,
    /// Indeterminate (retryable) failure — the tx may already be in the mempool.
    Transient,
    /// A non-transient "already known" / "nonce too low" — the node already has it.
    AlreadyKnown,
    /// Deterministic reject — definitely not broadcast.
    Deterministic,
}

#[derive(Default)]
pub(crate) struct MockSubmit {
    pub outcome: Submit,
    /// Every submitted RLP body, in order — for asserting rebroadcast/bump.
    pub seen: Arc<Mutex<Vec<Bytes>>>,
    pub log: CallLog,
}

#[async_trait]
impl SubmissionStrategy for MockSubmit {
    async fn submit(&self, rlp: Bytes, _opts: &SubmissionOpts) -> Result<TxHash, SubmissionError> {
        note(&self.log, "submit");
        self.seen.lock().push(rlp);
        match self.outcome {
            Submit::Ok => Ok(TxHash::ZERO),
            Submit::Transient => Err(SubmissionError::Rpc(RpcError::Call {
                message: "timeout".into(),
                transient: true,
            })),
            Submit::AlreadyKnown => Err(SubmissionError::Rpc(RpcError::Call {
                message: "already known".into(),
                transient: false,
            })),
            Submit::Deterministic => Err(SubmissionError::Rpc(RpcError::Call {
                message: "invalid".into(),
                transient: false,
            })),
        }
    }
}

// ---- StateStore ----

/// A functional handle store (upsert by id, `pending` excludes terminal) that also
/// logs each `put_handle` as `"persist"`. Nonce-state ops are unused by the
/// executor/manager tests (the nonce tests use the real `InMemoryStateStore`).
#[derive(Default)]
pub(crate) struct MockStore {
    handles: Mutex<Vec<TxHandle>>,
    pub log: CallLog,
}

impl MockStore {
    /// A store that records each persist into `log` (for pipeline-order assertions).
    pub fn logged(log: CallLog) -> Self {
        Self {
            handles: Mutex::default(),
            log,
        }
    }

    /// Snapshot of every stored handle (including terminal ones, unlike `pending`).
    pub fn all(&self) -> Vec<TxHandle> {
        self.handles.lock().clone()
    }
}

#[async_trait]
impl StateStore for MockStore {
    async fn put_handle(&self, handle: &TxHandle) -> Result<(), StateStoreError> {
        note(&self.log, "persist");
        let mut handles = self.handles.lock();
        match handles.iter_mut().find(|h| h.id == handle.id) {
            Some(slot) => *slot = handle.clone(),
            None => handles.push(handle.clone()),
        }
        Ok(())
    }
    async fn pending_handles(&self, account: Address) -> Result<Vec<TxHandle>, StateStoreError> {
        Ok(self
            .handles
            .lock()
            .iter()
            .filter(|h| h.account == account && !h.status.is_terminal())
            .cloned()
            .collect())
    }
    async fn handle(&self, id: HandleId) -> Result<Option<TxHandle>, StateStoreError> {
        Ok(self.handles.lock().iter().find(|h| h.id == id).cloned())
    }
    async fn load_nonce_state(
        &self,
        _: NonceScope,
    ) -> Result<Versioned<NonceState>, StateStoreError> {
        unreachable!("nonce state is exercised via the real InMemoryStateStore")
    }
    async fn cas_nonce_state(
        &self,
        _: NonceScope,
        _: u64,
        _: &NonceState,
        _: FenceToken,
    ) -> Result<bool, StateStoreError> {
        unreachable!("nonce state is exercised via the real InMemoryStateStore")
    }
}

// ---- Harness ----

/// Wires an [`AccountExecutor`] or [`TransactionManager`] from all-default mocks;
/// override only the ports a test asserts on. Setters take `Arc<dyn _>` so a test
/// keeps a typed `Arc<MockStore>`/`Arc<MockSubmit>` for post-run assertions and
/// passes a clone in.
pub(crate) struct Harness {
    rpc: Arc<dyn Rpc>,
    gas: Arc<dyn GasOracle>,
    policy: Arc<dyn PolicyEngine>,
    nonce: Arc<dyn NonceManager>,
    signer: Arc<dyn Signer>,
    submit: Arc<dyn SubmissionStrategy>,
    store: Arc<dyn StateStore>,
    clock: Arc<dyn Clock>,
    confirmations: u64,
    bump_timeout: u64,
}

impl Default for Harness {
    fn default() -> Self {
        Self {
            rpc: Arc::new(MockRpc::default()),
            gas: Arc::new(MockGas::default()),
            policy: Arc::new(MockPolicy::default()),
            nonce: Arc::new(MockNonce::default()),
            signer: Arc::new(MockSigner::default()),
            submit: Arc::new(MockSubmit::default()),
            store: Arc::new(MockStore::default()),
            clock: Arc::new(MockClock(0)),
            confirmations: 12,
            bump_timeout: 30,
        }
    }
}

impl Harness {
    pub fn rpc(mut self, v: Arc<dyn Rpc>) -> Self {
        self.rpc = v;
        self
    }
    pub fn gas(mut self, v: Arc<dyn GasOracle>) -> Self {
        self.gas = v;
        self
    }
    pub fn policy(mut self, v: Arc<dyn PolicyEngine>) -> Self {
        self.policy = v;
        self
    }
    pub fn nonce(mut self, v: Arc<dyn NonceManager>) -> Self {
        self.nonce = v;
        self
    }
    pub fn signer(mut self, v: Arc<dyn Signer>) -> Self {
        self.signer = v;
        self
    }
    pub fn submit(mut self, v: Arc<dyn SubmissionStrategy>) -> Self {
        self.submit = v;
        self
    }
    pub fn store(mut self, v: Arc<dyn StateStore>) -> Self {
        self.store = v;
        self
    }
    pub fn clock(mut self, v: Arc<dyn Clock>) -> Self {
        self.clock = v;
        self
    }
    pub fn confirmations(mut self, n: u64) -> Self {
        self.confirmations = n;
        self
    }
    pub fn bump_timeout(mut self, n: u64) -> Self {
        self.bump_timeout = n;
        self
    }

    pub fn executor(self) -> AccountExecutor {
        AccountExecutor::new(
            self.rpc,
            self.nonce,
            self.submit,
            self.store,
            self.gas,
            self.policy,
            self.signer,
            self.clock,
            Address::ZERO,
        )
        .with_confirmations(self.confirmations)
        .with_bump_timeout(self.bump_timeout)
    }

    pub fn manager(self) -> TransactionManager {
        TransactionManager::new(
            self.rpc,
            self.gas,
            self.policy,
            self.nonce,
            self.signer,
            self.submit,
            self.store,
            self.clock,
        )
    }
}
