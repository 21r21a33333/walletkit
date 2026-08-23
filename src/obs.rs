//! Observability shim. Instrumentation call sites use `crate::obs::{info,warn,error,
//! debug,trace}!` and `#[cfg_attr(feature = "tracing", tracing::instrument(...))]` so no
//! `cfg` leaks into logic. With the `tracing` feature on these forward to `tracing`; with
//! it off they compile to no-ops (arguments are not evaluated, matching `tracing`'s own
//! disabled-callsite behavior). Function spans use `cfg_attr(instrument)`, which simply
//! isn't applied when the feature is off — no span shim needed.

#[cfg(feature = "tracing")]
pub(crate) use tracing::{debug, error, info, warn};

// One no-op macro, path-aliased to the five event names. A bare `macro_rules! warn`
// collides with the built-in `#[warn]` attribute in the macro namespace, so the
// definition is named `noop` and re-exported by path (the same shape the enabled path
// uses for `tracing::warn`).
#[cfg(not(feature = "tracing"))]
mod noop {
    macro_rules! noop {
        ($($t:tt)*) => {{}};
    }
    pub(crate) use noop;
}

#[cfg(not(feature = "tracing"))]
pub(crate) use noop::noop as debug;
#[cfg(not(feature = "tracing"))]
pub(crate) use noop::noop as error;
#[cfg(not(feature = "tracing"))]
pub(crate) use noop::noop as info;
#[cfg(not(feature = "tracing"))]
pub(crate) use noop::noop as warn;
