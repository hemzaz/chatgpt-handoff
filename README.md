# chatgpt-handoff

[![CI](https://github.com/hemzaz/chatgpt-handoff/actions/workflows/ci.yml/badge.svg)](https://github.com/hemzaz/chatgpt-handoff/actions/workflows/ci.yml)
[![License](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)
[![crates.io](https://img.shields.io/badge/crates.io-not%20yet%20published-lightgrey.svg)](https://crates.io/crates/chatgpt-handoff)

When a ChatGPT conversation hits the length limit, ChatGPT can't continue it —
you have to start a new conversation with no memory of the old one.
`chatgpt-handoff` takes ChatGPT's own data export and turns one conversation
into two files: an archival `transcript.md` of exactly what you saw (not every
abandoned regeneration attempt), and a compact `context.md` you paste into the
first message of a *new* conversation so the model can pick up where the old
one left off instead of restarting from zero.

> **Project status:** pre-1.0 and not yet published to crates.io, but the CLI
> below is real and working — every command and flag in this README has been
> checked against the binary's own `--help` output.

## Install

```bash
# From a checkout of this repository:
cargo install --path .

# Once published to crates.io:
cargo install chatgpt-handoff
```

## Getting a ChatGPT export

In ChatGPT: **Settings → Data controls → Export data**. You'll get an email
with a link to a `.zip` archive containing `conversations.json` (plus
`chat.html`, images, and other assets you don't need). `chatgpt-handoff`
reads either the `.zip` directly or a `conversations.json` you've already
unzipped — it sniffs the file's magic bytes rather than trusting the
extension, so a renamed file still works.

## Quick start

```bash
# List every conversation in an export (accepts the .zip ChatGPT emails you,
# or a conversations.json you've already unzipped), newest-updated first
chatgpt-handoff list ~/Downloads/chatgpt-export.zip

# Fuzzy-search conversation titles and ids — add --content to also search
# message bodies (slower: it walks every node). Hebrew, Arabic, and other
# non-Latin scripts work the same as ASCII
chatgpt-handoff find conversations.json "iboga"

# Show one conversation, selected by an exact or fuzzy title — non-ASCII
# titles work the same as any other
chatgpt-handoff show conversations.json --title "איבוגה גמילה מאופיאטים"

# Render just the active branch as a standalone archival Markdown transcript
chatgpt-handoff transcript conversations.json --conversation conv-linear-0001

# The main workflow: reconstruct the active branch and write
# transcript.md + context.md + metadata.json for one conversation
chatgpt-handoff extract conversations.json --title "Rust CLI design notes"

# Get the same handoff-writing prompt extract's `--context-mode prompt`
# generates, printed to stdout instead of run through the deterministic
# generator — paste it into any LLM alongside transcript.md
chatgpt-handoff prompt conversations.json --conversation conv-linear-0001

# See the raw graph: which nodes are on the active branch and which were
# abandoned regenerations
chatgpt-handoff inspect conversations.json --conversation conv-branch-0002 --nodes
```

A successful `extract` writes its files atomically into an output directory
(`./handoff` by default; nothing is overwritten unless you pass `--force`)
and reports what it wrote:

```
$ chatgpt-handoff extract conversations.json --title "Rust CLI design notes"
Created handoff package:

  handoff/context.md
  handoff/transcript.md
  handoff/metadata.json

Conversation:
  Rust CLI design notes

Active branch:
  8 messages

Recent context preserved:
  8 messages
```

Every `extract` run writes three files — `context.md`, `transcript.md`, and a
machine-readable `metadata.json` sidecar (see
[Context generation](#context-generation) below). Two flags add more:
`--context-mode prompt` also writes `summarization-prompt.md`, and `--raw`
also writes `raw-conversation.json`.

## Commands and flags

Eight subcommands: `list`, `find`, `show`, `transcript`, `extract`, `prompt`,
`inspect`, and `help`. Every flag below is checked against the binary's own
`--help` output, not guessed.

**Selecting a conversation.** `show`, `transcript`, `extract`, `prompt`, and
`inspect` all take an optional positional query (a fuzzy id or title match)
or the explicit `--conversation <ID>` / `--title <TITLE>` flags. When more
than one conversation matches, the command lists the candidates and exits
rather than guessing — add `--pick` to resolve that ambiguity interactively
from the candidate list instead. `--pick` requires a real terminal and is
never on by default, so every command stays usable unattended in a script.

Options that matter enough to call out individually, beyond what's already
covered above and in [Context generation](#context-generation):

| Command | Flag | What it does | Default |
|---|---|---|---|
| `list` | `--sort <updated\|created\|title>` | Sort order | `updated` |
| `list` | `--reverse` | Reverse the sort order | off |
| `list` | `--limit <N>` | Show at most N conversations | unlimited |
| `find` | `--content` | Also fuzzy-search message bodies, not just titles/ids (slower — walks every node) | off |
| `find` | `--limit <N>` | Maximum matches returned | 20 |
| `extract` | `-o`, `--output <DIR>` | Directory to create the handoff package in | `./handoff` |
| `inspect` | `--nodes` | List every node in the graph, not just the summary — see below | off |

`transcript`, `extract`, and `prompt` additionally share a set of
message-inclusion flags. By default only `user` and `assistant` messages are
rendered — the conversational core — and each flag below opts in exactly one
more category: `--include-system`, `--include-developer`, `--include-tools`
(tool calls and their output), and `--include-hidden` (messages ChatGPT
itself hides from the UI).

Cross-cutting flags available on most or all commands:

| Flag | What it does | Default |
|---|---|---|
| `--json` | Emit machine-readable JSON instead of human-readable text (`list`, `find`, `show`, `extract`, `inspect`) | off |
| `--local-time` | Render timestamps in the local timezone instead of UTC | UTC |
| `--max-unpacked-bytes <N>` | Zip-bomb guard — see [Security notes](#security-notes) | 536870912 (512 MiB) |
| `-v`, `-vv` | Increase logging verbosity (info / debug) to stderr | off |

## Active-branch semantics

A ChatGPT conversation isn't a list of messages — it's a tree. Every time you
regenerate an answer or edit a previous message, the export's `mapping` gains
another branch; only one `current_node` pointer says which leaf you were
actually looking at. `chatgpt-handoff` reconstructs *the conversation you saw*
by starting at `current_node` and walking `parent` links up to the root, then
reversing — never by iterating `mapping` directly, which would splice
abandoned regenerations into the transcript as if they'd happened.

Branch reconstruction is pluggable (a `BranchStrategy` trait), and the default
strategy falls back automatically to a `longest-path` strategy — the deepest
reachable path through the graph — whenever `current_node` is missing,
dangling, or resolves to a branch with no messages on it. When that fallback
fires, it's recorded as a warning rather than silently swallowed, so you can
tell when the tool had to guess.

**See it for yourself** with `inspect --nodes`. This fixture's
`conv-branch-0002` has a regenerated answer: one assistant reply was kept,
the other abandoned. Real output:

```
$ chatgpt-handoff inspect tests/fixtures/sample-export.json --conversation conv-branch-0002 --nodes
Conversation ID: conv-branch-0002
Title:           Regenerated answer demo
current_node:    node-04-assistant-kept
Roots:           node-00-root
Branch strategy: current-node
Branch nodes:    5 of 6 total
Branch points:   1 (1 alternative branch(es))
Damage:          0 broken parent(s), 0 unreachable node(s)
Warnings:        none

NODE                                   ROLE          CHILD    CHARS  ON BRANCH
node-00-root                           -                 1        0  yes
node-01-user                           user              2       86  yes
node-03-assistant-kept                 assistant         1      170  yes
node-04-user-kept                      user              1       82  yes
node-04-assistant-kept                 assistant         0      125  yes
node-02-assistant-abandoned            assistant         0       99
```

`node-02-assistant-abandoned` — the first, discarded regeneration — is listed
so you can see the graph damage was found and accounted for, but its `ON
BRANCH` column is blank: it's counted in `Branch nodes: 5 of 6 total` and the
one `Branch point`, but it will never appear in `transcript.md` or `context.md`.

## Context generation

`context.md` is a fixed 14-section handoff document, generated from the
reconstructed active branch. Every section always appears — a section with
nothing found in it says so explicitly, so a model reading the document can
tell "nothing here" apart from "never considered". The 14 sections, in order:

1. Conversation
2. Purpose
3. Important Background
4. Established Facts
5. User Preferences and Constraints
6. Decisions Already Made
7. Terminology and Entities
8. Important Technical Details
9. Key Conclusions
10. Rejected / Superseded Approaches
11. Current State
12. Open Questions
13. Recent Conversation
14. Continuation Instructions

There are two generation modes, selected with `--context-mode`:

- **`deterministic`** (the default) — local, offline heuristics only: sentence
  and keyword extraction, no network calls, no API key, no LLM involved. This
  is genuinely **heuristic extraction, not semantic summarization** — it will
  miss nuance a human (or an LLM) would catch. The "Recent Conversation"
  section is always the verbatim tail of the transcript, never lossy, so the
  most recent exchange is never at the mercy of the heuristics.
- **`prompt`** — in addition to the same deterministic document, writes a
  `summarization-prompt.md` containing the transcript plus instructions for
  producing a higher-quality `context.md`. Paste that file into any LLM (this
  one or another) to get a better-written handoff document back.

How much of the transcript's tail is preserved verbatim in that "Recent
Conversation" section is controlled by two flags on `extract` and `prompt`:
`--recent-messages <N>` (default `30`) caps it by message count, and
`--recent-chars <N>` (no default — opt-in) additionally caps it by character
count. **When both are given, the stricter one wins** — whichever limit would
cut the tail shorter is the one that applies. At least one message is always
kept even if it alone exceeds `--recent-chars`, since a truncated final
message is worse than an oversized one.

### `prompt` (the command)

You don't have to run `extract` to get the summarization prompt — the
`prompt` subcommand prints exactly that file to stdout (or `--output FILE`)
on its own, so it's the fast path when all you want is a better `context.md`
via your own LLM of choice, without producing a `transcript.md`/`metadata.json`
you don't need yet. Real (truncated) output:

```
$ chatgpt-handoff prompt tests/fixtures/sample-export.json --conversation conv-linear-0001
# Task

You are given `transcript.md`: the complete history of a ChatGPT conversation that hit its length limit. Produce a single Markdown document, `context.md`, that lets a model with no memory of this conversation **continue** it.
```

It continues with a *Source conversation* block giving the title, id,
created/updated timestamps, and size (for this fixture: `Rust CLI design
notes`, `conv-linear-0001`, 8 messages / ~191 words), then ten numbered rules
(don't summarize away specifics, distinguish fact from speculation, preserve
open questions verbatim, etc.), and ends by requiring the same 14 headings,
in the same order, listed above.

### `metadata.json`

Every `extract` run also writes a machine-readable `metadata.json` alongside
`context.md` and `transcript.md` — provenance and statistics for scripts or
other tools consuming the package, without having to re-parse the Markdown.
Real output for one of the fixtures in this repository
(`chatgpt-handoff extract tests/fixtures/sample-export.json --conversation conv-linear-0001 --output /tmp/demo --force`):

```json
{
  "active_branch_messages": 8,
  "alternative_branches": 0,
  "approx_characters": 1195,
  "approx_words": 191,
  "assistant_messages": 4,
  "branch_strategy": "current-node",
  "context_mode": "deterministic",
  "conversation_id": "conv-linear-0001",
  "created_at": "2025-08-12T12:00:00Z",
  "generated_by": "chatgpt-handoff 0.1.0",
  "recent_messages_preserved": 8,
  "source": "tests/fixtures/sample-export.json",
  "title": "Rust CLI design notes",
  "total_nodes": 9,
  "updated_at": "2025-08-12T13:00:00Z",
  "user_messages": 4,
  "warnings": []
}
```

`total_nodes` counts every node in the conversation's graph (including
message-less scaffolding and any abandoned regenerations), while the
`*_messages` counts describe only the reconstructed active branch — so a
non-zero gap between them, or a non-zero `alternative_branches`, tells a
consumer that other branches existed even though only one was rendered.
`branch_strategy` records which strategy actually produced the branch
(`current-node` normally, `longest-path` after a fallback — see
[Active-branch semantics](#active-branch-semantics)), and `warnings` surfaces
any recoverable graph damage found along the way.

### `--raw`

Pass `--raw` to also write `raw-conversation.json`: the original, unmodified
JSON object for that conversation straight out of the source export,
including any fields the tolerant domain model discards. Useful when you need
something `chatgpt-handoff` itself doesn't model yet.

## Privacy

Everything runs locally. There are no network calls anywhere in this tool —
not for search, not for context generation, not for anything. Your export
never leaves your machine. Because a ChatGPT export is untrusted input (it's
attacker-controllable in principle, and definitely contains attacker-supplied
*content* — pasted text, filenames, titles), it's parsed defensively
throughout; see [Security notes](#security-notes) below. This repository
never contains real export data — see [`tests/fixtures/README.md`](tests/fixtures/README.md).

## Limitations

- Context generation quality is heuristic, not semantic (see above) — the
  `prompt` mode exists specifically to work around this.
- Only the **active branch** is extracted. Abandoned regenerations and edited-
  over messages are counted (in `inspect` / statistics output) but never
  rendered — the tool tells you a conversation had, say, 3 alternative
  branches, but only ever shows you the one you were looking at.
- Attachments (images, audio, files) become short textual markers in the
  transcript (e.g. `[image attachment]`), never the original bytes — nothing
  is extracted from or decoded out of the export's asset pointers.
- Canvas/tool/code-execution payloads are rendered as fenced code blocks or
  concise markers, not replayed or executed.
- The export schema is expected to drift over time as ChatGPT changes its
  export format. Unknown fields are ignored and unrecognized `content_type`
  values degrade to a `[<type> content omitted]` marker instead of failing the
  load — but a genuinely new shape will render as a marker until the tool is
  updated to understand it.

## Expected export format

`chatgpt-handoff` accepts either a bare JSON array of conversations, or an
object wrapping that array under a `conversations` key. Annotated shape of one
conversation:

```jsonc
{
  // Present in real exports as "id"; "conversation_id" is accepted as a
  // fallback; a conversation with neither gets a synthetic "unknown-N" id.
  "id": "conv-linear-0001",
  "title": "Rust CLI design notes",
  "create_time": 1755000000.123456,   // fractional Unix seconds; null is fine
  "update_time": 1755003600.654321,
  // The leaf of the branch you were last looking at. May be missing or
  // dangling in a damaged export — the tool falls back gracefully either way.
  "current_node": "node-08-assistant",
  "mapping": {
    "node-08-assistant": {
      "id": "node-08-assistant",         // the mapping key is authoritative
      "parent": "node-07-user",
      "children": [],
      "message": {                        // null for message-less scaffold nodes
        "author": { "role": "assistant", "name": null },
        "create_time": 1755003600.654321,
        "content": {
          "content_type": "text",         // "multimodal_text", "code", … also
          "parts": ["…"]                  // handled; unknown types survive parsing
        },
        "metadata": {}
      }
    }
  }
}
```

Unknown top-level and node fields are silently ignored. Unknown
`content_type` values are preserved rather than rejected, so a ChatGPT export
format change degrades output quality — it never breaks the load.

## Security notes

Export files are treated as hostile input throughout, because they can
contain attacker-supplied content (pasted logs, hostile titles) even in an
export that is otherwise legitimate:

- **Zip-slip protection.** Archive entry names are validated before anything
  is read: absolute paths, drive-qualified paths (`C:\...`), and any `..`
  path component are refused outright, for both `/` and `\` separators.
- **Decompression size limit.** Every archive entry is checked against a
  configurable byte cap — twice: once against the size the zip header
  *declares*, and once against the bytes actually delivered, since the
  declared size is written by whoever built the archive and can lie. Raise
  the cap with `--max-unpacked-bytes` if a legitimate export trips it.
- **Terminal-control sanitization.** Anything echoed to a terminal or written
  into a generated document — conversation titles, conversation **ids**,
  archive entry names — has C0/C1 control characters and Unicode bidirectional
  *override* characters stripped first, closing off cursor-movement and
  right-to-left display-spoofing tricks. Legitimate directional *marks* (used
  by real Hebrew/Arabic text) are left alone.
- **Single-line flattening of ids and titles.** Both are single-line fields in
  practice and attacker-supplied in principle, so whitespace in them is
  collapsed before display. Without this, a newline embedded in a title or id
  fabricates an entire extra row of `list` output — a conversation that does
  not exist, with a date and title of the attacker's choosing. The raw id is
  still used for lookups and in `--json` payloads, where nothing interprets
  the bytes.

## Development

```bash
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
```

CI runs the same checks on Linux and macOS for every push and pull request
against `main`/`master` (see [`.github/workflows/ci.yml`](.github/workflows/ci.yml)).
Windows is not currently a required CI target.

## License

Dual-licensed under your choice of [MIT](LICENSE-MIT) or
[Apache License, Version 2.0](LICENSE-APACHE).
