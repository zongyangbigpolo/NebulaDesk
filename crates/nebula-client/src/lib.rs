//! The NebulaDesk workspace app.
//!
//! A user signs in, sees what they may reach, and picks a resource. Desktop
//! details may identify its machine, but connection uses a manager-issued
//! resource ticket, not a machine address. The gateway reaches the agent over
//! its outbound tunnel, and the relay carries end-to-end encrypted bytes.
//!
//! The layering here follows that: [`manager`] is the only part that speaks
//! HTTP, [`connect`] turns a ticket into an encrypted session, and everything
//! after that is media.

pub mod application;
mod application_native;
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
pub mod shortcuts;
pub mod video;

pub use connect::{connect_to_agent, Connected};
pub use direct::ConnectedReceiver;
pub use manager::{ManagerClient, Resource, SessionTicket};
