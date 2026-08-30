//! Managed-relay adapters: a third party submits the user's meta-transaction and pays the gas,
//! and we [`poll`](crate::core::deps::Relay::poll) the returned task to inclusion. Currently one
//! family, [`GelatoRelay`]; self-relay is not here — it submits an outer tx through the standard
//! pipeline and is orchestrated by the facade, not through the [`Relay`](crate::core::deps::Relay)
//! port.

pub mod gelato;

pub use gelato::GelatoRelay;
