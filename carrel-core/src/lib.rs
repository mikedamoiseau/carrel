//! Carrel core library — UI- and IPC-free shared functionality.
//!
//! This crate hosts data models, the error taxonomy, and shared utilities
//! consumed by both the Tauri desktop app (`src-tauri/`) and any future
//! headless/server binaries. It deliberately has zero Tauri or axum
//! dependencies so every module is reusable from any Rust context.
//!
//! Extracted from `src-tauri/src/` so the server can share it; see CHANGELOG
//! 2.0.0, "carrel-core crate extraction".

pub mod activity;
pub mod backup;
pub mod cache;
pub mod cbr;
pub mod cbz;
pub mod db;
pub mod dictionary;
pub mod enrichment;
pub mod epub;
pub mod error;
pub mod events;
pub mod http_retry;
pub mod image_util;
pub mod isbn;
#[cfg(feature = "mobi")]
pub mod mobi;
pub mod models;
pub mod opds;
pub mod opds_feed;
pub mod openlibrary;
pub mod page_cache;
pub mod paths;
pub mod pdf;
pub mod plugins;
pub mod profile_lock;
pub mod providers;
pub mod reader;
pub mod search;
pub mod source_cache;
pub mod storage;
pub mod sync;
pub mod vocabulary;

// Flat re-exports for common types so consumers can write
// `use carrel_core::{CarrelError, CarrelResult, Book}` without the extra
// module prefix.
pub use error::{CarrelError, CarrelResult};
