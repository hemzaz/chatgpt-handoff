//! Continuation-context generation.
pub mod deterministic;
pub mod prompt;
pub mod recent;
pub mod template;

use crate::error::Result;
use crate::graph::ConversationBranch;
use crate::model::Conversation;
use crate::timefmt::TimeZoneMode;
use crate::transcript::TranscriptOptions;

pub use deterministic::DeterministicContextGenerator;
pub use prompt::{PromptContextGenerator, summarization_prompt};
pub use recent::{RecentSelection, select_recent};
pub use template::{ContextDocument, SECTION_ORDER, Section};

pub const DEFAULT_RECENT_MESSAGES: usize = 30;

#[derive(Debug, Clone)]
pub struct ContextOptions {
    pub recent_messages: usize,
    pub recent_chars: Option<usize>,
    pub transcript: TranscriptOptions,
    pub timezone: TimeZoneMode,
}

impl Default for ContextOptions {
    fn default() -> Self {
        Self {
            recent_messages: DEFAULT_RECENT_MESSAGES,
            recent_chars: None,
            transcript: TranscriptOptions::default(),
            timezone: TimeZoneMode::default(),
        }
    }
}

/// Strategy for turning a conversation branch into a handoff context document.
pub trait ContextGenerator {
    fn name(&self) -> &'static str;
    fn generate(
        &self,
        conversation: &Conversation,
        branch: &ConversationBranch,
        options: &ContextOptions,
    ) -> Result<ContextDocument>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ContextMode {
    #[default]
    Deterministic,
    Prompt,
}

impl ContextMode {
    pub fn generator(self) -> Box<dyn ContextGenerator> {
        match self {
            ContextMode::Deterministic => Box::new(DeterministicContextGenerator),
            ContextMode::Prompt => Box::new(PromptContextGenerator),
        }
    }
}
