//! The NebulaDesk workspace app.
//!
//! A user signs in, sees what they may reach, and picks one. What serves it
//! is not their concern and is never told to them: the manager resolves a
//! resource to a machine, the gateway reaches that machine over a tunnel it
//! opened outbound, and the relay carries bytes neither of them can read.
//!
//! The layering here follows that: [`manager`] is the only part that speaks
//! HTTP, [`connect`] turns a ticket into an encrypted session, and everything
//! after that is media.

pub mod audio;
pub mod chrome;
pub mod connect;
pub mod desktop;
mod direct;
pub mod input;
pub mod manager;
pub mod nal;
pub mod render;
pub mod session;
pub mod video;

pub use connect::{connect_to_agent, Connected};
pub use direct::ConnectedReceiver;
pub use manager::{ManagerClient, Resource, SessionTicket};
