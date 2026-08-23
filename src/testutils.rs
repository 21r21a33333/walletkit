//! Shared test doubles and fixtures — one configurable mock per port plus a
//! [`Harness`] builder, so a port signature change is edited **here once** rather
//! than across every `#[cfg(test)]` module. Mocks expose `pub` fields with a
//! `Default`, so a test overrides only what it asserts on
//! (`MockRpc { gas_reverts: true, ..Default::default() }`) and shares one
//! [`CallLog`] across mocks to assert pipeline ordering.

use crate::core::deps::{
    Clock, GasOracle, GasOracleError, NonceManager, NonceManagerError, PolicyEngine,
    PolicyEngineError, Rpc, RpcError, Signer, SignerError, StateStore, StateStoreError,
    SubmissionError, SubmissionStrategy, Versioned,
};
use crate::core::wallet::{
    AccountExecutor, Decision, GasEnvelope, HandleId, IntentHash, NonceScope, NonceState,
    PolicyApproval, PolicyRejection, TransactionManager, TxHandle, TxIntent, TxStatus,
};
use alloy_consensus::{Receipt, ReceiptEnvelope, ReceiptWithBloom, SignableTransaction, TxEip1559};
use alloy_eips::Encodable2718;
use alloy_eips::eip1559::Eip1559Estimation;
use alloy_primitives::{Address, B256, Bytes, Signature, TxHash, TxKind, U256};
use alloy_rpc_types_eth::{TransactionReceipt, TransactionRequest};
use async_trait::async_trait;
use parking_lot::Mutex;
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
#[derive(Default)]
pub(crate) struct MockRpc {
    pub pending_nonce: u64,
    pub tx_count: u64,
    pub block_number: u64,
    pub finalized: Option<u64>,
    pub base_fee: u128,
    pub receipt: Option<TransactionReceipt>,
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
        Ok(self.block_number)
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
    async fn send_raw(&self, _: Bytes) -> Result<TxHash, RpcError> {
        Ok(TxHash::ZERO)
    }
    async fn receipt(&self, _: TxHash) -> Result<Option<TransactionReceipt>, RpcError> {
        Ok(self.receipt.clone())
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
    async fn evaluate(&self, intent: &TxIntent) -> Result<Decision, PolicyEngineError> {
        note(&self.log, "policy");
        *self.calls.lock() += 1;
        Ok(if self.allow {
            Decision::Allow(PolicyApproval::mint(
                intent.hash(),
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
}

// ---- SubmissionStrategy ----

#[derive(Clone, Copy, Default)]
pub(crate) enum Submit {
    #[default]
    Ok,
    /// Indeterminate (retryable) failure — the tx may already be in the mempool.
    Transient,
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
    async fn submit(&self, rlp: Bytes) -> Result<TxHash, SubmissionError> {
        note(&self.log, "submit");
        self.seen.lock().push(rlp);
        match self.outcome {
            Submit::Ok => Ok(TxHash::ZERO),
            Submit::Transient => Err(SubmissionError::Rpc(RpcError::Call {
                message: "timeout".into(),
                transient: true,
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
