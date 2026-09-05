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

> **Project status:** the domain model, export parsing (JSON and zip), active-
> branch reconstruction, fuzzy search, selector resolution, and Markdown
> transcript rendering are implemented and tested. Context-document generation
> (`context.md`) and the command-line interface itself are still under active
> development. The usage below describes the intended shape of the CLI; if
> you're reading this before a tagged release, check that the binary
> implements the command you want before scripting against it.

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
# or a conversations.json you've already unzipped)
chatgpt-handoff list ~/Downloads/chatgpt-export.zip

# Fuzzy-search conversation titles (and, optionally, message bodies) —
# Hebrew, Arabic, and other non-Latin scripts work the same as ASCII
chatgpt-handoff find conversations.json "iboga"

# Show one conversation, selected by an exact or fuzzy title
chatgpt-handoff show conversations.json --title "איבוגה גמילה מאופיאטים"

# Render just the active branch as a standalone archival Markdown transcript
chatgpt-handoff transcript conversations.json --conversation conv-linear-0001

# The main workflow: reconstruct the active branch and write both
# transcript.md and context.md for one conversation
chatgpt-handoff extract conversations.json --title "Rust CLI design notes"
```

A successful `extract` writes its files atomically into an output directory
(nothing is overwritten unless you pass `--force`) and reports what it wrote:

```
$ chatgpt-handoff extract conversations.json --title "Rust CLI design notes"
wrote 2 files to handoff/conv-linear-0001/
  handoff/conv-linear-0001/transcript.md
  handoff/conv-linear-0001/context.md
```

`prompt` mode additionally writes a `summarization-prompt.md` alongside the
other two files — see [Context generation](#context-generation) below.

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

## Context generation

`context.md` is a fixed 14-section handoff document, generated from the
reconstructed active branch. Every section always appears — a section with
nothing found in it says so explicitly, so a model reading the document can
tell "nothing here" apart from "never considered":

```
# Conversation Handoff

## Conversation
## Purpose
## Important Background
## Established Facts
## User Preferences and Constraints
## Decisions Already Made
## Terminology and Entities
## Important Technical Details
## Key Conclusions
## Rejected / Superseded Approaches
## Current State
## Open Questions
## Recent Conversation
## Continuation Instructions
```

There are two generation modes:

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
- **Terminal-control sanitization.** Anything that gets echoed to a terminal —
  conversation titles, archive entry names — has C0/C1 control characters and
  Unicode bidirectional *override* characters stripped first, closing off
  cursor-movement and right-to-left display-spoofing tricks. Legitimate
  directional *marks* (used by real Hebrew/Arabic text) are left alone.

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
