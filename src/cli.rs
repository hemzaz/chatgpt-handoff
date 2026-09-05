//! Command-line interface: argument definitions and command handlers.
//!
//! This is the application boundary. It is the only module allowed to use
//! `anyhow`, to print to stdout, or to talk to the terminal.

use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use clap::{ArgAction, Args, Parser, Subcommand, ValueEnum};
use serde_json::json;

use crate::context::{self, ContextMode, ContextOptions};
use crate::error::{AmbiguousCandidate, SelectError};
use crate::export::{self, LoadOptions};
use crate::graph::{self, ConversationStats};
use crate::model::{Conversation, ConversationSet};
use crate::output::{self, Writer};
use crate::select::{self, Resolution, Selector};
use crate::text;
use crate::timefmt::{self, TimeZoneMode};
use crate::transcript::{self, TranscriptOptions};

const ABOUT: &str = "Turn a ChatGPT export into a transcript and a compact handoff context";

const LONG_ABOUT: &str = "\
Reconstructs the active branch of a ChatGPT conversation from a data export and
produces two things: an archival transcript, and a compact continuation context
you can paste into a fresh conversation when the original hit its length limit.

Everything runs locally. No network calls, no API keys, no data leaves the machine.";

#[derive(Debug, Parser)]
#[command(
    name = "chatgpt-handoff",
    version,
    about = ABOUT,
    long_about = LONG_ABOUT,
    propagate_version = true
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,

    /// Increase logging verbosity (-v for info, -vv for debug). Logs go to stderr.
    #[arg(short = 'v', long = "verbose", action = ArgAction::Count, global = true)]
    pub verbose: u8,

    /// Render timestamps in the local timezone instead of UTC.
    #[arg(long, global = true)]
    pub local_time: bool,

    /// Refuse ZIP entries that declare more than this many uncompressed bytes.
    #[arg(long, global = true, value_name = "BYTES", default_value_t = export::zip::DEFAULT_MAX_UNPACKED_BYTES)]
    pub max_unpacked_bytes: u64,
}

impl Cli {
    pub fn timezone(&self) -> TimeZoneMode {
        if self.local_time {
            TimeZoneMode::Local
        } else {
            TimeZoneMode::Utc
        }
    }

    pub fn load_options(&self) -> LoadOptions {
        LoadOptions {
            max_unpacked_bytes: self.max_unpacked_bytes,
        }
    }

    /// Log level implied by the `-v` count.
    pub fn log_filter(&self) -> &'static str {
        match self.verbose {
            0 => "warn",
            1 => "info",
            _ => "debug",
        }
    }
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// List every conversation in the export.
    List(ListArgs),
    /// Fuzzy-search conversations by title, id, or (optionally) content.
    Find(FindArgs),
    /// Show metadata and statistics for one conversation.
    Show(ShowArgs),
    /// Render one conversation's active branch as Markdown.
    Transcript(TranscriptArgs),
    /// Write a complete handoff package (context + transcript + metadata).
    Extract(ExtractArgs),
    /// Emit an LLM prompt that turns a transcript into a high-quality handoff.
    Prompt(PromptArgs),
    /// Inspect the raw conversation graph (debugging aid).
    Inspect(InspectArgs),
}

/// How a conversation is chosen. Shared by every command that needs one.
#[derive(Debug, Clone, Args)]
pub struct SelectorArgs {
    /// Conversation id, or a unique id prefix.
    #[arg(long, value_name = "ID")]
    pub conversation: Option<String>,

    /// Conversation title (exact, then case-insensitive, then fuzzy).
    #[arg(long, value_name = "TITLE")]
    pub title: Option<String>,

    /// Choose interactively when the selector is ambiguous. Requires a terminal.
    #[arg(long)]
    pub pick: bool,
}

impl SelectorArgs {
    fn selector(&self, query: Option<String>) -> Selector {
        Selector {
            id: self.conversation.clone(),
            title: self.title.clone(),
            query,
        }
    }
}

/// Which roles appear in rendered output.
#[derive(Debug, Clone, Copy, Args)]
pub struct RoleFilterArgs {
    /// Include `system` messages.
    #[arg(long)]
    pub include_system: bool,
    /// Include `tool` messages and tool output.
    #[arg(long)]
    pub include_tools: bool,
    /// Include `developer` messages.
    #[arg(long)]
    pub include_developer: bool,
    /// Include messages ChatGPT hides from its own UI.
    #[arg(long)]
    pub include_hidden: bool,
}

impl From<RoleFilterArgs> for TranscriptOptions {
    fn from(args: RoleFilterArgs) -> Self {
        TranscriptOptions {
            include_system: args.include_system,
            include_tools: args.include_tools,
            include_developer: args.include_developer,
            include_hidden: args.include_hidden,
        }
    }
}

#[derive(Debug, Clone, Copy, ValueEnum, Default, PartialEq, Eq)]
pub enum SortKey {
    #[default]
    Updated,
    Created,
    Title,
}

#[derive(Debug, Clone, Copy, ValueEnum, Default, PartialEq, Eq)]
pub enum ContextModeArg {
    /// Build the context locally with heuristics. No network, no API key.
    #[default]
    Deterministic,
    /// Same, plus a `summarization-prompt.md` to run through any LLM.
    Prompt,
}

impl From<ContextModeArg> for ContextMode {
    fn from(arg: ContextModeArg) -> Self {
        match arg {
            ContextModeArg::Deterministic => ContextMode::Deterministic,
            ContextModeArg::Prompt => ContextMode::Prompt,
        }
    }
}

#[derive(Debug, Args)]
pub struct ListArgs {
    /// `conversations.json`, or a ChatGPT export `.zip`.
    pub input: PathBuf,
    #[arg(long, value_enum, default_value_t = SortKey::Updated)]
    pub sort: SortKey,
    /// Reverse the sort order.
    #[arg(long)]
    pub reverse: bool,
    /// Show at most N conversations.
    #[arg(long, value_name = "N")]
    pub limit: Option<usize>,
    /// Emit machine-readable JSON on stdout.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct FindArgs {
    /// `conversations.json`, or a ChatGPT export `.zip`.
    pub input: PathBuf,
    /// What to search for.
    pub query: String,
    /// Also search message content (slower).
    #[arg(long)]
    pub content: bool,
    #[arg(long, value_name = "N", default_value_t = 20)]
    pub limit: usize,
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct ShowArgs {
    pub input: PathBuf,
    /// Conversation id or partial title.
    pub query: Option<String>,
    #[command(flatten)]
    pub selector: SelectorArgs,
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct TranscriptArgs {
    pub input: PathBuf,
    /// Conversation id or partial title.
    pub query: Option<String>,
    #[command(flatten)]
    pub selector: SelectorArgs,
    #[command(flatten)]
    pub roles: RoleFilterArgs,
    /// Write to a file instead of stdout.
    #[arg(long, value_name = "FILE")]
    pub output: Option<PathBuf>,
    /// Overwrite an existing output file.
    #[arg(long)]
    pub force: bool,
}

#[derive(Debug, Args)]
pub struct ExtractArgs {
    pub input: PathBuf,
    /// Conversation id or partial title.
    pub query: Option<String>,
    #[command(flatten)]
    pub selector: SelectorArgs,
    #[command(flatten)]
    pub roles: RoleFilterArgs,
    /// Directory to create the handoff package in.
    #[arg(long, short = 'o', value_name = "DIR", default_value = "./handoff")]
    pub output: PathBuf,
    #[arg(long, value_enum, default_value_t = ContextModeArg::Deterministic)]
    pub context_mode: ContextModeArg,
    /// Preserve this many trailing messages near-verbatim.
    #[arg(long, value_name = "N", default_value_t = context::DEFAULT_RECENT_MESSAGES)]
    pub recent_messages: usize,
    /// Also cap the preserved tail at this many characters (stricter limit wins).
    #[arg(long, value_name = "N")]
    pub recent_chars: Option<usize>,
    /// Also write `raw-conversation.json` with the untouched source JSON.
    #[arg(long)]
    pub raw: bool,
    /// Overwrite existing files in the output directory.
    #[arg(long)]
    pub force: bool,
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct PromptArgs {
    pub input: PathBuf,
    /// Conversation id or partial title.
    pub query: Option<String>,
    #[command(flatten)]
    pub selector: SelectorArgs,
    #[command(flatten)]
    pub roles: RoleFilterArgs,
    #[arg(long, value_name = "N", default_value_t = context::DEFAULT_RECENT_MESSAGES)]
    pub recent_messages: usize,
    /// Write to a file instead of stdout.
    #[arg(long, value_name = "FILE")]
    pub output: Option<PathBuf>,
    #[arg(long)]
    pub force: bool,
}

#[derive(Debug, Args)]
pub struct InspectArgs {
    pub input: PathBuf,
    /// Conversation id or partial title.
    pub query: Option<String>,
    #[command(flatten)]
    pub selector: SelectorArgs,
    /// List every node in the graph, not just the summary.
    #[arg(long)]
    pub nodes: bool,
    #[arg(long)]
    pub json: bool,
}

/// Entry point used by `main`.
pub fn run(cli: Cli) -> Result<()> {
    let timezone = cli.timezone();
    let options = cli.load_options();

    match &cli.command {
        Command::List(args) => cmd_list(args, &options, timezone),
        Command::Find(args) => cmd_find(args, &options),
        Command::Show(args) => cmd_show(args, &options, timezone),
        Command::Transcript(args) => cmd_transcript(args, &options, timezone),
        Command::Extract(args) => cmd_extract(args, &options, timezone),
        Command::Prompt(args) => cmd_prompt(args, &options, timezone),
        Command::Inspect(args) => cmd_inspect(args, &options),
    }
}

// ---------------------------------------------------------------- loading --

fn load(input: &Path, options: &LoadOptions) -> Result<ConversationSet> {
    let set = export::load(input, options)
        .with_context(|| format!("could not read export {}", input.display()))?;
    for warning in &set.warnings {
        tracing::warn!("{warning}");
    }
    tracing::info!(
        conversations = set.len(),
        source = %set.source,
        "loaded export"
    );
    Ok(set)
}

/// Resolve a selector to exactly one conversation, offering the interactive
/// picker when the user asked for it and stdin is a terminal.
fn choose<'a>(
    set: &'a ConversationSet,
    selector: &Selector,
    pick: bool,
) -> Result<&'a Conversation> {
    if selector.is_empty() {
        return Err(anyhow::Error::from(SelectError::NoSelector));
    }
    match select::resolve(&set.conversations, selector) {
        Ok(Resolution::Unique(conversation)) => Ok(conversation),
        Ok(Resolution::Ambiguous(candidates)) => {
            let listed: Vec<AmbiguousCandidate> = candidates
                .iter()
                .map(|candidate| AmbiguousCandidate {
                    id: candidate.conversation.display_id(),
                    title: candidate.conversation.display_title(),
                    score: candidate.score,
                })
                .collect();

            if pick && std::io::stdin().is_terminal() {
                let index = prompt_for_choice(&listed)?;
                return candidates
                    .get(index)
                    .map(|candidate| candidate.conversation)
                    .ok_or_else(|| anyhow::anyhow!("no conversation selected"));
            }

            Err(anyhow::Error::from(SelectError::Ambiguous {
                query: selector.describe(),
                candidates: listed,
            }))
        }
        Err(err) => Err(anyhow::Error::from(err)),
    }
}

/// Interactive disambiguation. Prompts on stderr so `--json` stdout stays clean.
fn prompt_for_choice(candidates: &[AmbiguousCandidate]) -> Result<usize> {
    let mut stderr = std::io::stderr();
    writeln!(stderr, "Multiple conversations match:\n")?;
    for (index, candidate) in candidates.iter().enumerate() {
        writeln!(
            stderr,
            "  {:>2}) {:>3}  {}  {}",
            index + 1,
            candidate.score,
            short_id(&candidate.id),
            text::sanitize_display(&candidate.title)
        )?;
    }
    write!(stderr, "\nSelect [1-{}]: ", candidates.len())?;
    stderr.flush()?;

    let mut line = String::new();
    std::io::stdin()
        .read_line(&mut line)
        .context("could not read a selection from stdin")?;
    let choice: usize = line
        .trim()
        .parse()
        .context("expected the number of one of the listed conversations")?;
    if choice == 0 || choice > candidates.len() {
        anyhow::bail!("{choice} is not one of the listed conversations");
    }
    Ok(choice - 1)
}

fn short_id(id: &str) -> String {
    text::truncate_graphemes(id, 8).into_owned()
}

/// Resolve the conversation *and* its active branch, logging any graph damage.
fn resolve_branch<'a>(
    set: &'a ConversationSet,
    selector: &Selector,
    pick: bool,
) -> Result<(&'a Conversation, graph::ConversationBranch)> {
    let conversation = choose(set, selector, pick)?;
    let branch = graph::active_branch(conversation)
        .with_context(|| format!("could not reconstruct conversation {}", conversation.id))?;
    for warning in &branch.warnings {
        tracing::warn!("conversation {}: {warning}", conversation.id);
    }
    Ok((conversation, branch))
}

// --------------------------------------------------------------- commands --

fn cmd_list(args: &ListArgs, options: &LoadOptions, tz: TimeZoneMode) -> Result<()> {
    let set = load(&args.input, options)?;
    let mut conversations: Vec<&Conversation> = set.conversations.iter().collect();

    // Secondary key by id keeps the order total, so output is reproducible even
    // when timestamps tie or are missing.
    conversations.sort_by(|a, b| {
        let ordering = match args.sort {
            SortKey::Updated => b
                .update_time
                .unwrap_or(f64::MIN)
                .total_cmp(&a.update_time.unwrap_or(f64::MIN)),
            SortKey::Created => b
                .create_time
                .unwrap_or(f64::MIN)
                .total_cmp(&a.create_time.unwrap_or(f64::MIN)),
            SortKey::Title => a.display_title().cmp(&b.display_title()),
        };
        ordering.then_with(|| a.id.cmp(&b.id))
    });
    if args.reverse {
        conversations.reverse();
    }
    if let Some(limit) = args.limit {
        conversations.truncate(limit);
    }

    if args.json {
        let payload: Vec<_> = conversations
            .iter()
            .map(|conversation| {
                json!({
                    "id": conversation.id,
                    "title": conversation.title,
                    "created_at": timefmt::format(conversation.create_time, tz),
                    "updated_at": timefmt::format(conversation.update_time, tz),
                    "total_nodes": conversation.mapping.len(),
                })
            })
            .collect();
        print_json(&json!({ "source": set.source, "conversations": payload }))?;
        return Ok(());
    }

    if conversations.is_empty() {
        println!("No conversations found in {}.", set.source);
        return Ok(());
    }

    const HEADERS: [&str; 3] = ["ID", "UPDATED", "TITLE"];
    println!("{:<36}  {:<16}  {}", HEADERS[0], HEADERS[1], HEADERS[2]);
    for conversation in &conversations {
        println!(
            "{:<36}  {:<16}  {}",
            conversation.display_id(),
            timefmt::format_short(conversation.update_time, tz),
            conversation.display_title()
        );
    }
    println!("\n{} conversation(s).", conversations.len());
    Ok(())
}

fn cmd_find(args: &FindArgs, options: &LoadOptions) -> Result<()> {
    let set = load(&args.input, options)?;
    let search_options = crate::search::SearchOptions {
        limit: args.limit,
        search_content: args.content,
        ..Default::default()
    };
    let matches = crate::search::search(&set.conversations, &args.query, &search_options);

    if args.json {
        let payload: Vec<_> = matches
            .iter()
            .map(|found| {
                json!({
                    "id": found.conversation.id,
                    "title": found.conversation.title,
                    "score": found.score,
                    "matched": found.field,
                    "excerpt": found.excerpt,
                })
            })
            .collect();
        print_json(&json!({ "query": args.query, "matches": payload }))?;
        return Ok(());
    }

    if matches.is_empty() {
        println!("No conversation matches {:?}.", args.query);
        return Ok(());
    }
    for found in &matches {
        println!(
            "{:>3}  {}  {}",
            found.score,
            short_id(&found.conversation.display_id()),
            found.conversation.display_title()
        );
        if let Some(excerpt) = &found.excerpt {
            println!("     {}", text::sanitize_display(excerpt));
        }
    }
    Ok(())
}

fn cmd_show(args: &ShowArgs, options: &LoadOptions, tz: TimeZoneMode) -> Result<()> {
    let set = load(&args.input, options)?;
    let selector = args.selector.selector(args.query.clone());
    let (conversation, branch) = resolve_branch(&set, &selector, args.selector.pick)?;
    let stats = ConversationStats::compute(conversation, &branch);

    if args.json {
        let mut payload = serde_json::to_value(&stats)?;
        if let Some(object) = payload.as_object_mut() {
            object.insert("conversation_id".into(), json!(conversation.id));
            object.insert("title".into(), json!(conversation.title));
            object.insert(
                "created_at".into(),
                json!(timefmt::format(conversation.create_time, tz)),
            );
            object.insert(
                "updated_at".into(),
                json!(timefmt::format(conversation.update_time, tz)),
            );
            object.insert("branch_strategy".into(), json!(branch.strategy));
            object.insert(
                "warnings".into(),
                json!(
                    branch
                        .warnings
                        .iter()
                        .map(ToString::to_string)
                        .collect::<Vec<_>>()
                ),
            );
        }
        print_json(&payload)?;
        return Ok(());
    }

    println!("Title:                  {}", conversation.display_title());
    println!("Conversation ID:        {}", conversation.display_id());
    println!(
        "Created:                {}",
        timefmt::format(conversation.create_time, tz)
    );
    println!(
        "Updated:                {}",
        timefmt::format(conversation.update_time, tz)
    );
    println!("Branch strategy:        {}", branch.strategy);
    println!();
    println!("Active-branch messages: {}", stats.active_branch_messages);
    println!("Total graph nodes:      {}", stats.total_nodes);
    println!("User messages:          {}", stats.user_messages);
    println!("Assistant messages:     {}", stats.assistant_messages);
    if stats.system_messages + stats.developer_messages + stats.tool_messages + stats.other_messages
        > 0
    {
        println!(
            "Other messages:         {} (system {}, developer {}, tool {}, other {})",
            stats.system_messages
                + stats.developer_messages
                + stats.tool_messages
                + stats.other_messages,
            stats.system_messages,
            stats.developer_messages,
            stats.tool_messages,
            stats.other_messages
        );
    }
    println!("Approx. characters:     {}", stats.characters);
    println!("Approx. words:          {}", stats.words);
    println!("Branch depth:           {}", stats.branch_depth);
    println!(
        "Alternative branches:   {} (at {} branch point(s))",
        stats.alternative_branches, stats.branch_points
    );
    if stats.broken_parents > 0 || stats.unreachable_nodes > 0 {
        println!(
            "Graph damage:           {} broken parent(s), {} unreachable node(s)",
            stats.broken_parents, stats.unreachable_nodes
        );
    }
    Ok(())
}

fn cmd_transcript(args: &TranscriptArgs, options: &LoadOptions, tz: TimeZoneMode) -> Result<()> {
    let set = load(&args.input, options)?;
    let selector = args.selector.selector(args.query.clone());
    let (conversation, branch) = resolve_branch(&set, &selector, args.selector.pick)?;
    let rendered = transcript::render(conversation, &branch, &args.roles.into(), tz);

    match &args.output {
        None => {
            print!("{rendered}");
            Ok(())
        }
        Some(path) => {
            output::write_atomic(path, &rendered, args.force)?;
            eprintln!("Wrote {}", path.display());
            Ok(())
        }
    }
}

fn cmd_extract(args: &ExtractArgs, options: &LoadOptions, tz: TimeZoneMode) -> Result<()> {
    let set = load(&args.input, options)?;
    let selector = args.selector.selector(args.query.clone());
    let (conversation, branch) = resolve_branch(&set, &selector, args.selector.pick)?;

    let transcript_options: TranscriptOptions = args.roles.into();
    let context_options = ContextOptions {
        recent_messages: args.recent_messages,
        recent_chars: args.recent_chars,
        transcript: transcript_options,
        timezone: tz,
    };

    let stats = ConversationStats::compute(conversation, &branch);
    // Deliberately not `select_recent` over `branch.messages(...)`: the context
    // generator applies the role/hidden filter first, so computing the tail
    // here over the unfiltered branch made metadata.json and stdout report a
    // number the document did not contain. `recent_selection` is the one
    // definition both sides share.
    let recent = context::recent_selection(conversation, &branch, &context_options);

    let mode: ContextMode = args.context_mode.into();
    let generator = mode.generator();
    let document = generator.generate(conversation, &branch, &context_options)?;

    let metadata = json!({
        "conversation_id": conversation.id,
        "title": conversation.title,
        "created_at": timefmt::format(conversation.create_time, tz),
        "updated_at": timefmt::format(conversation.update_time, tz),
        "active_branch_messages": stats.active_branch_messages,
        "total_nodes": stats.total_nodes,
        "user_messages": stats.user_messages,
        "assistant_messages": stats.assistant_messages,
        "approx_characters": stats.characters,
        "approx_words": stats.words,
        "alternative_branches": stats.alternative_branches,
        "branch_strategy": branch.strategy,
        "context_mode": generator.name(),
        "recent_messages_preserved": recent.message_count,
        "source": set.source,
        "generated_by": concat!(env!("CARGO_PKG_NAME"), " ", env!("CARGO_PKG_VERSION")),
        "warnings": branch.warnings.iter().map(ToString::to_string).collect::<Vec<_>>(),
    });

    let mut writer = Writer::new(&args.output, args.force);
    writer.stage("context.md", document.render_markdown());
    writer.stage(
        "transcript.md",
        transcript::render(conversation, &branch, &transcript_options, tz),
    );
    writer.stage("metadata.json", format!("{metadata:#}\n"));

    if mode == ContextMode::Prompt {
        writer.stage(
            "summarization-prompt.md",
            context::summarization_prompt(conversation, &branch, &context_options),
        );
    }

    if args.raw {
        match export::raw_conversation(&args.input, options, &conversation.id)? {
            Some(value) => writer.stage("raw-conversation.json", format!("{value:#}\n")),
            None => tracing::warn!(
                "could not recover the raw JSON for conversation {}; \
                 raw-conversation.json was not written",
                conversation.id
            ),
        }
    }

    let written = writer.commit()?;

    if args.json {
        print_json(&json!({
            "output_dir": args.output,
            "files": written,
            "metadata": metadata,
        }))?;
        return Ok(());
    }

    println!("Created handoff package:\n");
    for path in &written {
        println!("  {}", path.display());
    }
    println!("\nConversation:\n  {}", conversation.display_title());
    println!(
        "\nActive branch:\n  {} messages",
        stats.active_branch_messages
    );
    println!(
        "\nRecent context preserved:\n  {} messages",
        recent.message_count
    );
    Ok(())
}

fn cmd_prompt(args: &PromptArgs, options: &LoadOptions, tz: TimeZoneMode) -> Result<()> {
    let set = load(&args.input, options)?;
    let selector = args.selector.selector(args.query.clone());
    let (conversation, branch) = resolve_branch(&set, &selector, args.selector.pick)?;

    let context_options = ContextOptions {
        recent_messages: args.recent_messages,
        recent_chars: None,
        transcript: args.roles.into(),
        timezone: tz,
    };
    let rendered = context::summarization_prompt(conversation, &branch, &context_options);

    match &args.output {
        None => {
            print!("{rendered}");
            Ok(())
        }
        Some(path) => {
            output::write_atomic(path, &rendered, args.force)?;
            eprintln!("Wrote {}", path.display());
            Ok(())
        }
    }
}

fn cmd_inspect(args: &InspectArgs, options: &LoadOptions) -> Result<()> {
    let set = load(&args.input, options)?;
    let selector = args.selector.selector(args.query.clone());
    let (conversation, branch) = resolve_branch(&set, &selector, args.selector.pick)?;
    let stats = ConversationStats::compute(conversation, &branch);

    let on_branch: std::collections::HashSet<&str> =
        branch.node_ids.iter().map(String::as_str).collect();

    // Deterministic node order: active branch first in traversal order, then
    // every other node sorted by id.
    let mut off_branch: Vec<&str> = conversation
        .mapping
        .keys()
        .map(String::as_str)
        .filter(|id| !on_branch.contains(id))
        .collect();
    off_branch.sort_unstable();

    let describe = |id: &str| {
        let node = conversation.node(id);
        let (role, chars) = match node.and_then(|n| n.message.as_ref()) {
            Some(message) => (
                message.role().as_str().to_string(),
                text::grapheme_count(&message.content.plain_text()),
            ),
            None => ("(no message)".to_string(), 0),
        };
        json!({
            "id": id,
            "role": role,
            "parent": node.and_then(|n| n.parent.clone()),
            "children": node.map(|n| n.children.len()).unwrap_or(0),
            "characters": chars,
            "on_active_branch": on_branch.contains(id),
        })
    };

    if args.json {
        let mut payload = json!({
            "conversation_id": conversation.id,
            "title": conversation.title,
            "current_node": conversation.current_node,
            "roots": conversation.roots(),
            "branch_strategy": branch.strategy,
            "branch_node_ids": branch.node_ids,
            "warnings": branch.warnings.iter().map(ToString::to_string).collect::<Vec<_>>(),
            "stats": stats,
        });
        if args.nodes {
            if let Some(object) = payload.as_object_mut() {
                let nodes: Vec<_> = branch
                    .node_ids
                    .iter()
                    .map(|id| describe(id))
                    .chain(off_branch.iter().map(|id| describe(id)))
                    .collect();
                object.insert("nodes".into(), json!(nodes));
            }
        }
        print_json(&payload)?;
        return Ok(());
    }

    println!("Conversation ID: {}", conversation.display_id());
    println!("Title:           {}", conversation.display_title());
    println!(
        "current_node:    {}",
        conversation.current_node.as_deref().unwrap_or("(absent)")
    );
    println!("Roots:           {}", conversation.roots().join(", "));
    println!("Branch strategy: {}", branch.strategy);
    println!(
        "Branch nodes:    {} of {} total",
        branch.node_ids.len(),
        stats.total_nodes
    );
    println!(
        "Branch points:   {} ({} alternative branch(es))",
        stats.branch_points, stats.alternative_branches
    );
    println!(
        "Damage:          {} broken parent(s), {} unreachable node(s)",
        stats.broken_parents, stats.unreachable_nodes
    );
    if branch.warnings.is_empty() {
        println!("Warnings:        none");
    } else {
        println!("Warnings:");
        for warning in &branch.warnings {
            println!("  - {warning}");
        }
    }

    if args.nodes {
        const NODE_HEADERS: [&str; 5] = ["NODE", "ROLE", "CHILD", "CHARS", "ON BRANCH"];
        println!(
            "\n{:<38} {:<12} {:>6} {:>8}  {}",
            NODE_HEADERS[0], NODE_HEADERS[1], NODE_HEADERS[2], NODE_HEADERS[3], NODE_HEADERS[4]
        );
        for id in branch.node_ids.iter().map(String::as_str).chain(off_branch) {
            let node = conversation.node(id);
            let role = node
                .and_then(|n| n.message.as_ref())
                .map(|m| m.role().as_str().to_string())
                .unwrap_or_else(|| "-".to_string());
            let chars = node
                .and_then(|n| n.message.as_ref())
                .map(|m| text::grapheme_count(&m.content.plain_text()))
                .unwrap_or(0);
            println!(
                "{:<38} {:<12} {:>6} {:>8}  {}",
                id,
                role,
                node.map(|n| n.children.len()).unwrap_or(0),
                chars,
                if on_branch.contains(id) { "yes" } else { "" }
            );
        }
    }
    Ok(())
}

/// Print machine-readable JSON. Only ever called for `--json`, and it is the
/// only thing written to stdout in that mode.
fn print_json(value: &serde_json::Value) -> Result<()> {
    let mut stdout = std::io::stdout().lock();
    serde_json::to_writer_pretty(&mut stdout, value)?;
    writeln!(stdout)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn extract_defaults_match_the_documented_behaviour() {
        let cli = Cli::parse_from(["chatgpt-handoff", "extract", "conversations.json"]);
        let Command::Extract(args) = cli.command else {
            panic!("expected the extract subcommand");
        };
        assert_eq!(args.output, PathBuf::from("./handoff"));
        assert_eq!(args.context_mode, ContextModeArg::Deterministic);
        assert_eq!(args.recent_messages, context::DEFAULT_RECENT_MESSAGES);
        assert_eq!(args.recent_chars, None);
        assert!(!args.force);
        assert!(!args.raw);
    }

    #[test]
    fn a_bare_positional_query_selects_a_conversation() {
        let cli = Cli::parse_from(["chatgpt-handoff", "extract", "export.zip", "partial title"]);
        let Command::Extract(args) = cli.command else {
            panic!("expected the extract subcommand");
        };
        assert_eq!(args.query.as_deref(), Some("partial title"));
        assert_eq!(args.selector.conversation, None);
    }

    #[test]
    fn selector_flags_are_available_on_every_conversation_command() {
        for verb in ["show", "transcript", "extract", "prompt", "inspect"] {
            let cli = Cli::parse_from([
                "chatgpt-handoff",
                verb,
                "export.zip",
                "--conversation",
                "abc123",
            ]);
            let selector = match cli.command {
                Command::Show(a) => a.selector,
                Command::Transcript(a) => a.selector,
                Command::Extract(a) => a.selector,
                Command::Prompt(a) => a.selector,
                Command::Inspect(a) => a.selector,
                _ => panic!("unexpected subcommand for {verb}"),
            };
            assert_eq!(selector.conversation.as_deref(), Some("abc123"), "{verb}");
        }
    }

    #[test]
    fn verbosity_maps_to_log_levels() {
        let quiet = Cli::parse_from(["chatgpt-handoff", "list", "x.json"]);
        assert_eq!(quiet.log_filter(), "warn");
        let info = Cli::parse_from(["chatgpt-handoff", "-v", "list", "x.json"]);
        assert_eq!(info.log_filter(), "info");
        let debug = Cli::parse_from(["chatgpt-handoff", "-vv", "list", "x.json"]);
        assert_eq!(debug.log_filter(), "debug");
    }

    #[test]
    fn timezone_is_utc_unless_requested() {
        let default = Cli::parse_from(["chatgpt-handoff", "list", "x.json"]);
        assert_eq!(default.timezone(), TimeZoneMode::Utc);
        let local = Cli::parse_from(["chatgpt-handoff", "--local-time", "list", "x.json"]);
        assert_eq!(local.timezone(), TimeZoneMode::Local);
    }

    #[test]
    fn role_flags_translate_to_transcript_options() {
        let cli = Cli::parse_from([
            "chatgpt-handoff",
            "transcript",
            "x.json",
            "--include-system",
            "--include-tools",
        ]);
        let Command::Transcript(args) = cli.command else {
            panic!("expected the transcript subcommand");
        };
        let options: TranscriptOptions = args.roles.into();
        assert!(options.include_system);
        assert!(options.include_tools);
        assert!(!options.include_developer);
        assert!(!options.include_hidden);
    }

    #[test]
    fn short_ids_are_grapheme_safe() {
        assert_eq!(short_id("abcdefghijkl"), "abcdefgh…");
        assert_eq!(short_id("abc"), "abc");
    }
}
