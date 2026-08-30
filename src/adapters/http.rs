//! Shared HTTP response triage for the adapters that post directly to a relay endpoint
//! (private-submission Flashbots, gasless Gelato). The auth/transient split is universal — only
//! the success body is endpoint-specific — so both callers reuse [`classify_status`] and map the
//! one [`HttpClass::Body`] case with their own parser, mapping the result into their own error
//! type at the edge.

use reqwest::StatusCode;

/// The universal categories of a relay HTTP response, by status code. A `Body` response still
/// needs endpoint-specific parsing (the payload shape differs), but the credential-rejection and
/// retry decisions are the same everywhere.
pub(crate) enum HttpClass {
    /// `401`/`403` — the endpoint rejected our credentials. Terminal; retrying will not help.
    Unauthorized,
    /// `5xx` or `429` — the endpoint is overloaded or erroring; the request may retry.
    Transient,
    /// Any other status — the body carries the endpoint-specific result or error to parse.
    Body,
}

/// Triage a relay response by status code alone (the safety-relevant split every caller shares).
pub(crate) fn classify_status(status: StatusCode) -> HttpClass {
    if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
        HttpClass::Unauthorized
    } else if status.is_server_error() || status == StatusCode::TOO_MANY_REQUESTS {
        HttpClass::Transient
    } else {
        HttpClass::Body
    }
}

/// Bound a relay's message so a large body can't bloat an error or log line.
pub(crate) fn clip(text: &str) -> String {
    text.chars().take(200).collect()
}
