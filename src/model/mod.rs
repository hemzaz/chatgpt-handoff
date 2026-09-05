//! The domain model: conversations, graph nodes, and messages.
//!
//! These types are deliberately tolerant of schema drift. Unknown fields are
//! ignored, unknown enum variants are preserved, and every optional concept is
//! an `Option` — a ChatGPT export format change should degrade output quality,
//! never break the load.

pub mod conversation;
pub mod message;
pub mod node;

pub use conversation::{Conversation, ConversationSet};
pub use message::{Author, ContentPart, Message, MessageContent, MessageMetadata, Role};
pub use node::ConversationNode;

pub(crate) use conversation::RawConversation;
