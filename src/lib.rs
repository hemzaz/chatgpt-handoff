//! `chatgpt-handoff` — turn a ChatGPT data export into a clean transcript and a
//! compact continuation context package.
//!
//! # Architecture
//!
//! ```text
//! export::   untrusted bytes (json / zip)  ->  model::ConversationSet
//! model::    tolerant domain types
//! graph::    Conversation -> ConversationBranch   (active branch reconstruction)
//! search::   fuzzy matching over titles/ids/content
//! select::   deterministic selector resolution
//! transcript:: branch -> archival Markdown
//! context::  branch -> compact continuation context
//! output::   safe, atomic, non-clobbering file writes
//! ```
//!
//! The library never panics on malformed input and never uses `anyhow`;
//! `main.rs` is the only place that formats errors for humans.

#![forbid(unsafe_code)]

pub mod cli;
pub mod context;
pub mod error;
pub mod export;
pub mod graph;
pub mod model;
pub mod output;
pub mod search;
pub mod select;
pub mod text;
pub mod timefmt;
pub mod transcript;

pub use error::{Error, GraphError, Result, SelectError};
pub use model::{Conversation, ConversationSet};
