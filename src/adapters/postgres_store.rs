//! Networked/shared `StateStore` over PostgreSQL via sqlx (pure-Rust driver). Suitable for
//! multiple replicas sharing per-account state: the version-CAS gives cross-replica gapless
//! nonces, and the fence rejects a superseded owner (the Phase-3 lease issuer mints real
//! tokens). Values are stored as `JSONB` (same `serde_json` shape as the redb backend);
//! version/fence/nonce are `BIGINT` columns so they can be inspected and indexed in SQL.

use crate::core::deps::{StateStore, StateStoreError, Versioned};
use crate::core::wallet::{FenceToken, HandleId, NonceScope, NonceState, TxHandle};
use crate::obs::debug;
use alloy_primitives::Address;
use async_trait::async_trait;
use sqlx::{PgPool, Row};

/// Idempotent DDL run at connect — a partial index over the non-terminal rows makes
/// `pending_handles` a covered lookup rather than a full-table scan.
const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS nonce_state (
    account TEXT PRIMARY KEY,
    version BIGINT NOT NULL,
    fence   BIGINT NOT NULL,
    state   JSONB  NOT NULL
);
CREATE TABLE IF NOT EXISTS tx_handles (
    id       BYTEA   PRIMARY KEY,
    account  TEXT    NOT NULL,
    terminal BOOLEAN NOT NULL,
    handle   JSONB   NOT NULL
);
CREATE INDEX IF NOT EXISTS tx_handles_pending ON tx_handles (account) WHERE NOT terminal;
";

pub struct PostgresStateStore {
    pool: PgPool,
}

impl PostgresStateStore {
    /// Connect to `url` and ensure the schema exists.
    pub async fn connect(url: &str) -> Result<Self, StateStoreError> {
        let pool = PgPool::connect(url).await.map_err(backend)?;
        sqlx::raw_sql(SCHEMA)
            .execute(&pool)
            .await
            .map_err(backend)?;
        Ok(Self { pool })
    }

    /// Delete all persisted state for `account` — decommissioning, and the clean-slate reset
    /// the integration harness needs before reusing an account against a shared database.
    pub async fn clear_account(&self, account: Address) -> Result<(), StateStoreError> {
        let account = account_key(&account);
        for table in ["nonce_state", "tx_handles"] {
            sqlx::query(&format!("DELETE FROM {table} WHERE account = $1"))
                .bind(&account)
                .execute(&self.pool)
                .await
                .map_err(backend)?;
        }
        Ok(())
    }
}

fn backend<E: std::error::Error + Send + Sync + 'static>(e: E) -> StateStoreError {
    StateStoreError::Backend {
        source: Box::new(e),
    }
}

fn ser(e: serde_json::Error) -> StateStoreError {
    StateStoreError::Serialization {
        source: Box::new(e),
    }
}

fn account_key(account: &Address) -> String {
    format!("{account:x}")
}

fn fence_to_i64(fence: FenceToken) -> i64 {
    fence.as_u64() as i64
}

#[async_trait]
impl StateStore for PostgresStateStore {
    async fn load_nonce_state(
        &self,
        scope: NonceScope,
    ) -> Result<Versioned<NonceState>, StateStoreError> {
        let account = account_key(&scope.account);
        let row = sqlx::query("SELECT version, state FROM nonce_state WHERE account = $1")
            .bind(&account)
            .fetch_optional(&self.pool)
            .await
            .map_err(backend)?;
        match row {
            Some(r) => {
                let version = r.get::<i64, _>("version") as u64;
                let value = serde_json::from_value(r.get("state")).map_err(ser)?;
                Ok(Versioned { value, version })
            }
            None => Ok(Versioned::default()),
        }
    }

    async fn cas_nonce_state(
        &self,
        scope: NonceScope,
        expected_version: u64,
        state: &NonceState,
        fence: FenceToken,
    ) -> Result<bool, StateStoreError> {
        let account = account_key(&scope.account);
        let fence_i = fence_to_i64(fence);
        let mut tx = self.pool.begin().await.map_err(backend)?;
        // Serialize all CAS for this account so the read-check-write is atomic — even the
        // first insert. `SELECT … FOR UPDATE` can't lock a not-yet-existent row, so two
        // concurrent first-inserts would both pass the version check and duplicate a nonce;
        // an advisory xact lock (auto-released at commit/rollback) closes that race.
        sqlx::query("SELECT pg_advisory_xact_lock(hashtext($1)::bigint)")
            .bind(&account)
            .execute(&mut *tx)
            .await
            .map_err(backend)?;
        let row = sqlx::query("SELECT version, fence FROM nonce_state WHERE account = $1")
            .bind(&account)
            .fetch_optional(&mut *tx)
            .await
            .map_err(backend)?;
        let (cur_version, cur_fence) = match &row {
            Some(r) => (r.get::<i64, _>("version"), r.get::<i64, _>("fence")),
            None => (0, fence_to_i64(FenceToken::SINGLE_WRITER)),
        };
        if fence_i < cur_fence {
            return Err(StateStoreError::Fenced);
        }
        if cur_version != expected_version as i64 {
            return Ok(false);
        }
        let json = serde_json::to_value(state).map_err(ser)?;
        // Verified `version == expected` and `fence >= cur_fence` under the lock, so set both
        // explicitly to the new values (no `GREATEST`/`+1` in SQL — mirrors redb + in-memory).
        sqlx::query(
            "INSERT INTO nonce_state (account, version, fence, state) VALUES ($1, $2, $3, $4)
             ON CONFLICT (account) DO UPDATE SET version = $2, fence = $3, state = $4",
        )
        .bind(&account)
        .bind(expected_version as i64 + 1)
        .bind(fence_i)
        .bind(json)
        .execute(&mut *tx)
        .await
        .map_err(backend)?;
        tx.commit().await.map_err(backend)?;
        debug!(account = %scope.account, "postgres nonce committed");
        Ok(true)
    }

    async fn put_handle(&self, handle: &TxHandle) -> Result<(), StateStoreError> {
        let id = handle.id.as_bytes();
        let account = account_key(&handle.account);
        let json = serde_json::to_value(handle).map_err(ser)?;
        sqlx::query(
            "INSERT INTO tx_handles (id, account, terminal, handle) VALUES ($1, $2, $3, $4)
             ON CONFLICT (id) DO UPDATE SET account = $2, terminal = $3, handle = $4",
        )
        .bind(id.as_slice())
        .bind(&account)
        .bind(handle.status.is_terminal())
        .bind(json)
        .execute(&self.pool)
        .await
        .map_err(backend)?;
        debug!("postgres handle persisted");
        Ok(())
    }

    async fn pending_handles(&self, account: Address) -> Result<Vec<TxHandle>, StateStoreError> {
        let account = account_key(&account);
        let rows = sqlx::query("SELECT handle FROM tx_handles WHERE account = $1 AND NOT terminal")
            .bind(&account)
            .fetch_all(&self.pool)
            .await
            .map_err(backend)?;
        rows.into_iter()
            .map(|r| serde_json::from_value(r.get("handle")).map_err(ser))
            .collect()
    }

    async fn handle(&self, id: HandleId) -> Result<Option<TxHandle>, StateStoreError> {
        let id = id.as_bytes();
        let row = sqlx::query("SELECT handle FROM tx_handles WHERE id = $1")
            .bind(id.as_slice())
            .fetch_optional(&self.pool)
            .await
            .map_err(backend)?;
        match row {
            Some(r) => Ok(Some(serde_json::from_value(r.get("handle")).map_err(ser)?)),
            None => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    // Holds Postgres to the exact same bar as the in-memory and redb backends: the store
    // contract (incl. fence rejection) plus the full nonce-manager behavior, run against a
    // real DB. Skips when no `DATABASE_URL` is set (local dev / no-DB CI). This is the only
    // test that touches Postgres, so it truncates first for a clean, deterministic slate.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn postgres_passes_store_and_manager_conformance() {
        let Ok(url) = std::env::var("DATABASE_URL") else {
            eprintln!("skipping postgres conformance: DATABASE_URL not set");
            return;
        };
        let pg = PostgresStateStore::connect(&url).await.expect("connect");
        sqlx::query("TRUNCATE nonce_state, tx_handles")
            .execute(&pg.pool)
            .await
            .unwrap();
        let store: Arc<dyn StateStore> = Arc::new(pg);
        crate::testutils::state_store_conformance(store.clone()).await;
        crate::testutils::nonce_manager_conformance(store).await;
    }
}
