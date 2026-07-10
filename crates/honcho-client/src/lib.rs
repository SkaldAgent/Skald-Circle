//! Honcho v3 REST API client.
//!
//! # Quick start
//!
//! ```rust,no_run
//! use honcho_client::{HonchoClient, models::*};
//!
//! #[tokio::main]
//! async fn main() -> anyhow::Result<()> {
//!     // Point at your local Docker instance
//!     let client = HonchoClient::with_base_url("http://localhost:8000", "your-api-key");
//!
//!     // Create or retrieve a workspace
//!     let ws = client.create_workspace(&WorkspaceCreate {
//!         id: "my-agent".into(),
//!         ..Default::default()
//!     }).await?;
//!
//!     // Create a peer (= one user / agent identity)
//!     let peer = client.create_peer(&ws.id, &PeerCreate {
//!         id: "daniele".into(),
//!         metadata: None,
//!         configuration: None,
//!     }).await?;
//!
//!     // Open a session
//!     let session = client.create_session(&ws.id, &SessionCreate::default()).await?;
//!
//!     // Add messages
//!     client.add_message(&ws.id, &session.id, MessageCreate {
//!         content: "Hello!".into(),
//!         peer_id: peer.id.clone(),
//!         metadata: None,
//!         configuration: None,
//!         created_at: None,
//!     }).await?;
//!
//!     // Query memory (Dialectic API)
//!     let answer = client.peer_chat(&ws.id, &peer.id, &DialecticOptions {
//!         query: "What did the user say?".into(),
//!         session_id: Some(session.id.clone()),
//!         target: None,
//!         stream: Some(false),
//!         reasoning_level: None,
//!     }).await?;
//!
//!     println!("{answer:#?}");
//!     Ok(())
//! }
//! ```

pub mod client;
pub mod conclusions;
pub mod error;
pub mod messages;
pub mod models;
pub mod peers;
pub mod sessions;
pub mod workspaces;

// Flat re-exports for convenience
pub use client::HonchoClient;
pub use error::{HonchoError, Result};
