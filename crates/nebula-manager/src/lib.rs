//! The NebulaDesk manager: the control plane.
//!
//! It owns identity, the device inventory, published resources and the
//! entitlements that connect them. It never carries session media; its role
//! in a launch ends when it hands the client a signed ticket.

#![warn(missing_docs)]

pub mod audit;
pub mod auth;
pub mod config;
pub mod error;
pub mod routes;
pub mod signing;
pub mod state;

pub use config::Config;
pub use error::{ApiError, ApiResult};
pub use routes::router;
pub use state::AppState;
