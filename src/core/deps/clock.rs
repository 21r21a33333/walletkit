//! [`Clock`] — the wall-clock time port.

/// Wall-clock time source in unix seconds. A port so the executor's timeouts and
/// approval-expiry checks are deterministic under test. Infallible — a clock read
/// cannot fail — so it has no `{TraitName}Error` and returns a plain value.
pub trait Clock: Send + Sync {
    /// Current time as whole unix seconds.
    fn now_unix(&self) -> u64;
}
