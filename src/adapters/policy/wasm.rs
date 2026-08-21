//! A hardened, in-process WASM plugin runner (feature `policy-moonpay`). Runs a
//! sandboxed `wasip1` module over a stdin-bytes → stdout-bytes contract — the
//! substrate the MoonPay/OWS engine uses to run a policy `executable`.
//!
//! Hardening: fresh `Store` per run (no state leaks); the linker grants only the
//! in-memory stdin/stdout pipes (no fs/net/env); **fuel-bounded** (a runaway plugin
//! traps instead of hanging the signing path) and **memory-capped**. The module is
//! compiled once into an [`InstancePre`] so per-run instantiation is cheap.

use crate::core::deps::PolicyEngineError;
use std::fmt::Display;
use wasmtime::{
    Config, Engine, InstancePre, Linker, Module, Store, StoreLimits, StoreLimitsBuilder,
};
use wasmtime_wasi::WasiCtxBuilder;
use wasmtime_wasi::p2::pipe::{MemoryInputPipe, MemoryOutputPipe};
use wasmtime_wasi::preview1::{self, WasiP1Ctx};

/// Instruction budget per run — bounds a runaway plugin.
const RUN_FUEL: u64 = 1_000_000_000;
/// Linear-memory cap per run.
const MAX_MEMORY: usize = 64 << 20;
/// Upper bound on plugin output.
const MAX_OUTPUT: usize = 64 << 10;

/// Per-`Store` host state: the WASI context (pipes only) plus resource limits.
struct HostState {
    wasi: WasiP1Ctx,
    limits: StoreLimits,
}

/// A compiled `wasip1` plugin runnable in a hardened sandbox.
pub(crate) struct WasmPlugin {
    engine: Engine,
    instance_pre: InstancePre<HostState>,
}

impl WasmPlugin {
    /// Compile the module once and pre-resolve its WASI imports.
    pub(crate) fn compile(wasm: &[u8]) -> Result<Self, PolicyEngineError> {
        let mut config = Config::new();
        config.async_support(true); // callers are async; drive WASI on the async path
        config.consume_fuel(true);
        let engine = Engine::new(&config).map_err(load)?;
        let module = Module::new(&engine, wasm).map_err(load)?;

        let mut linker: Linker<HostState> = Linker::new(&engine);
        preview1::add_to_linker_async(&mut linker, |s| &mut s.wasi).map_err(load)?;
        let instance_pre = linker.instantiate_pre(&module).map_err(load)?;
        Ok(Self {
            engine,
            instance_pre,
        })
    }

    /// Feed `input` on stdin, run to completion, return stdout bytes. Any trap,
    /// non-zero exit, or fuel exhaustion is a fail-closed [`PolicyEngineError::Eval`].
    pub(crate) async fn run(&self, input: Vec<u8>) -> Result<Vec<u8>, PolicyEngineError> {
        let stdout = MemoryOutputPipe::new(MAX_OUTPUT);
        let wasi = WasiCtxBuilder::new()
            .stdin(MemoryInputPipe::new(input))
            .stdout(stdout.clone())
            .build_p1(); // no fs/net/env/args granted

        let mut store = Store::new(
            &self.engine,
            HostState {
                wasi,
                limits: StoreLimitsBuilder::new().memory_size(MAX_MEMORY).build(),
            },
        );
        store.limiter(|s| &mut s.limits);
        store.set_fuel(RUN_FUEL).map_err(eval)?;

        let instance = self
            .instance_pre
            .instantiate_async(&mut store)
            .await
            .map_err(eval)?;
        let start = instance
            .get_typed_func::<(), ()>(&mut store, "_start")
            .map_err(eval)?;

        // A wasip1 command exits via proc_exit; wasmtime surfaces exit 0 (normal
        // return) as I32Exit(0). Any trap, non-zero exit, or fuel exhaustion fails closed.
        if let Err(err) = start.call_async(&mut store, ()).await {
            match err.downcast_ref::<wasmtime_wasi::I32Exit>() {
                Some(e) if e.0 == 0 => {}
                Some(e) => {
                    return Err(PolicyEngineError::Eval(format!(
                        "plugin exited with code {}",
                        e.0
                    )));
                }
                None => return Err(PolicyEngineError::Eval(format!("plugin trapped: {err}"))),
            }
        }
        Ok(stdout.contents().to_vec())
    }
}

fn load(e: impl Display) -> PolicyEngineError {
    PolicyEngineError::Load(e.to_string())
}
fn eval(e: impl Display) -> PolicyEngineError {
    PolicyEngineError::Eval(e.to_string())
}
