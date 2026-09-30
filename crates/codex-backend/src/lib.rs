pub mod anthropic;
pub mod auth;
pub mod config;
pub mod embedded;
pub mod logging;
pub mod monitor;
pub mod openai_compat;
pub mod paths;
pub mod project;
pub mod provider;
pub mod providers;
pub mod registry;
pub mod request_identity;
pub mod retry;
pub mod server;
pub mod session;
pub mod traffic;

pub use crate::anthropic::error::{ErrorDetail, ErrorEnvelope, json_error};
pub use crate::anthropic::schema::MessagesRequest;
pub use crate::provider::{AuthCommand, CliHandlers, Provider, RequestContext};
pub use crate::registry::Registry;

/// The claude-code-proxy release this backend was vendored from.
pub const VENDORED_VERSION: &str = env!("CARGO_PKG_VERSION");
