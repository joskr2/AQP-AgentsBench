//! Minimal kernel for the harn skeleton.
//!
//! DeepSeek Harness (DSH) builds on Cordis: a service container plus a typed
//! event system whose listeners run as a middleware "waterfall". This crate
//! keeps the same two seams but strips everything dynamic: no plugin loading,
//! no hot reload, no YAML patches.
//!
//! * [`EventBus`] - fan-out of typed [`HarnessEvent`]s via `tokio::sync::broadcast`.
//! * [`Waterfall`] - an ordered chain of middlewares that can inspect and
//!   rewrite the value flowing through it.
//! * [`ServiceRegistry`] - type-keyed storage of trait objects (`Arc<dyn Trait>`).
#![forbid(unsafe_code)]

mod event;
mod service;
mod waterfall;

pub use event::{EventBus, HarnessEvent};
pub use service::ServiceRegistry;
pub use waterfall::{Middleware, Next, Waterfall};

/// Errors raised by the kernel.
#[derive(Debug, thiserror::Error)]
pub enum KernelError {
    #[error("middleware chain failed: {0}")]
    Middleware(String),
    #[error("service already registered: {0}")]
    DuplicateService(&'static str),
    #[error("service registry lock poisoned")]
    Poisoned,
}

pub type Result<T> = std::result::Result<T, KernelError>;
