//! Local in-process embedding provider — pure-Rust BERT inference via candle.
//!
//! Downloads the model from Hugging Face on first use and caches it locally.
//! No API keys, no external services, no native (C/C++) dependencies.

#[cfg(feature = "local-embedding")]
mod embedding;

#[cfg(feature = "local-embedding")]
pub use embedding::LocalEmbeddingProvider;
