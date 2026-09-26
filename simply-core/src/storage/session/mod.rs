//! Session storage - DB-agnostic session abstraction
//!
//! This module provides:
//!
//! - `Session<T: TurnStore, C: TextStore>` - DB-agnostic session with lazy resolution
//! - `ResolvedContent` / `ResolvedMessage` - Cached resolved content
//! - `AssetResolver` - Resolution trait for assets and documents
//!
//! Session implements `ConversationContext` directly.
//!
//! For SQLite storage, use `storage::SqliteStore`.

mod resolver;
#[allow(clippy::module_inception)] // storage::session::session holds the Session type itself
mod session;
mod types;

#[cfg(test)]
mod tests;

// Re-export session types
pub use resolver::AssetResolver;
pub use session::Session;
pub use types::{ResolvedContent, ResolvedMessage};
