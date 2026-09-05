//! End-to-end tests driving the real binary against synthetic fixtures.
//!
//! These are the tests that prove the *product* works: that the active branch
//! really excludes abandoned regenerations, that `--json` stdout stays
//! machine-readable, that we never clobber files, and that a ZIP export is
//! handled identically to a loose `conversations.json`.

use std::path::{Path, PathBuf};
use std::process::Command;

use assert_cmd::prelude::*;
use predicates::prelude::*;

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

fn cli() -> Command {
    Command::cargo_bin("chatgpt-handoff").expect("the binary must build")
}

fn tempdir() -> tempfile::TempDir {
    tempfile::tempdir().expect("a temporary directory")
}

/// Repackage a JSON fixture into a ZIP that mirrors a real ChatGPT export
/// (`conversations.json` plus the other files OpenAI ships).
fn zip_fixture(dir: &Path, json: &Path) -> PathBuf {
    let path = dir.join("chatgpt-export.zip");
    let file = std::fs::File::create(&path).expect("create zip");
    let mut writer = zip::ZipWriter::new(file);
    let options: zip::write::FileOptions<'_, ()> =
        zip::write::FileOptions::default().compression_method(zip::CompressionMethod::Deflated);

    let body = std::fs::read(json).expect("read fixture");
    writer
        .start_file("conversations.json", options)
        .expect("start entry");
    std::io::Write::write_all(&mut writer, &body).expect("write entry");

    writer
        .start_file("chat.html", options)
        .expect("start entry");
    std::io::Write::write_all(&mut writer, b"<html>decoy</html>").expect("write entry");

    writer.finish().expect("finish zip");
    path
}

fn stdout_of(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

// ------------------------------------------------------------------ list --

#[test]
fn list_shows_every_conversation() {
    cli()
        .args(["list"])
        .arg(fixture("sample-export.json"))
        .assert()
        .success()
        .stdout(predicate::str::contains("conv-linear-0001"))
        .stdout(predicate::str::contains("conv-branch-0002"))
        .stdout(predicate::str::contains("conv-hebrew-0003"))
        .stdout(predicate::str::contains("איבוגה גמילה מאופיאטים"));
}

#[test]
fn list_json_stdout_is_pure_json() {
    let output = cli()
        .args(["list", "--json"])
        .arg(fixture("sample-export.json"))
        .output()
        .expect("run");
    assert!(output.status.success());

    let parsed: serde_json::Value = serde_json::from_slice(&output.stdout)
        .expect("stdout must be parseable JSON and nothing else");
    let conversations = parsed["conversations"]
        .as_array()
        .expect("conversations array");
    assert_eq!(conversations.len(), 3);
    // Timestamps default to RFC 3339 UTC.
    assert!(
        conversations[0]["updated_at"]
            .as_str()
            .is_some_and(|s| s.ends_with('Z')),
        "expected UTC RFC 3339, got {:?}",
        conversations[0]["updated_at"]
    );
}

#[test]
fn list_respects_limit_and_sort() {
    let output = cli()
        .args(["list", "--json", "--sort", "title", "--limit", "1"])
        .arg(fixture("sample-export.json"))
        .output()
        .expect("run");
    let parsed: serde_json::Value = serde_json::from_slice(&output.stdout).expect("json");
    assert_eq!(parsed["conversations"].as_array().map(Vec::len), Some(1));
}

#[test]
fn list_on_an_empty_export_is_not_an_error() {
    cli()
        .args(["list"])
        .arg(fixture("empty-export.json"))
        .assert()
        .success();
}

#[test]
fn list_reads_the_wrapped_object_shape() {
    cli()
        .args(["list"])
        .arg(fixture("wrapped-export.json"))
        .assert()
        .success()
        .stdout(predicate::str::contains("conversation(s)"));
}

// ------------------------------------------------------------------ find --

#[test]
fn find_matches_an_english_title() {
    cli()
        .args(["find"])
        .arg(fixture("sample-export.json"))
        .arg("rust cli")
        .assert()
        .success()
        .stdout(predicate::str::contains("Rust CLI design notes"));
}

#[test]
fn find_matches_a_hebrew_title() {
    let output = cli()
        .args(["find"])
        .arg(fixture("sample-export.json"))
        .arg("איבוגה")
        .arg("--json")
        .output()
        .expect("run");
    let parsed: serde_json::Value = serde_json::from_slice(&output.stdout).expect("json");
    let matches = parsed["matches"].as_array().expect("matches array");
    assert!(
        !matches.is_empty(),
        "Hebrew query must match the Hebrew title"
    );
    assert_eq!(matches[0]["id"], "conv-hebrew-0003");
}

#[test]
fn find_with_no_match_exits_successfully_and_says_so() {
    cli()
        .args(["find"])
        .arg(fixture("sample-export.json"))
        .arg("zzzzzzzzznotathing")
        .assert()
        .success()
        .stdout(predicate::str::contains("No conversation matches"));
}

// ------------------------------------------------------------------ show --

#[test]
fn show_reports_branch_statistics() {
    let output = cli()
        .args(["show"])
        .arg(fixture("sample-export.json"))
        .args(["--conversation", "conv-linear-0001", "--json"])
        .output()
        .expect("run");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let parsed: serde_json::Value = serde_json::from_slice(&output.stdout).expect("json");
    assert_eq!(parsed["conversation_id"], "conv-linear-0001");
    assert!(parsed["active_branch_messages"].as_u64().unwrap_or(0) > 0);
    assert!(parsed["user_messages"].as_u64().unwrap_or(0) > 0);
    assert!(parsed["assistant_messages"].as_u64().unwrap_or(0) > 0);
    assert!(parsed["total_nodes"].as_u64().unwrap_or(0) > 0);
}

#[test]
fn show_counts_the_abandoned_regeneration_as_an_alternative_branch() {
    let output = cli()
        .args(["show"])
        .arg(fixture("sample-export.json"))
        .args(["--conversation", "conv-branch-0002", "--json"])
        .output()
        .expect("run");
    let parsed: serde_json::Value = serde_json::from_slice(&output.stdout).expect("json");
    assert!(
        parsed["alternative_branches"].as_u64().unwrap_or(0) >= 1,
        "the regenerated sibling must be counted: {parsed}"
    );
    assert!(
        parsed["total_nodes"].as_u64().unwrap_or(0)
            > parsed["active_branch_messages"].as_u64().unwrap_or(0),
        "the graph must be larger than the active branch"
    );
}

// ------------------------------------------------------------ transcript --

#[test]
fn transcript_has_the_documented_shape() {
    let output = cli()
        .args(["transcript"])
        .arg(fixture("sample-export.json"))
        .args(["--conversation", "conv-linear-0001"])
        .output()
        .expect("run");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let text = stdout_of(&output);
    assert!(text.starts_with("# "), "must open with the title heading");
    assert!(text.contains("Conversation ID: conv-linear-0001"));
    assert!(text.contains("Created: "));
    assert!(text.contains("Updated: "));
    assert!(text.contains("\n---\n"));
    assert!(text.contains("## User"));
    assert!(text.contains("## Assistant"));
    // No graph internals may leak into the transcript.
    assert!(!text.contains("\"parent\""));
    assert!(!text.contains("content_type"));
    assert!(!text.contains("mapping"));
}

/// The headline behavior of the whole tool: only the branch the user was
/// actually on is rendered.
#[test]
fn transcript_excludes_abandoned_regenerations() {
    let output = cli()
        .args(["transcript"])
        .arg(fixture("sample-export.json"))
        .args(["--conversation", "conv-branch-0002"])
        .output()
        .expect("run");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let text = stdout_of(&output);
    assert!(
        text.contains("SECOND REGENERATION"),
        "the kept branch must be present:\n{text}"
    );
    assert!(
        !text.contains("FIRST REGENERATION"),
        "the abandoned branch must NOT be present:\n{text}"
    );
}

#[test]
fn transcript_preserves_hebrew_exactly() {
    let output = cli()
        .args(["transcript"])
        .arg(fixture("sample-export.json"))
        .args(["--conversation", "conv-hebrew-0003"])
        .output()
        .expect("run");
    let text = stdout_of(&output);
    assert!(text.contains("איבוגה"));
    assert!(
        std::str::from_utf8(&output.stdout).is_ok(),
        "output must be valid UTF-8"
    );
}

#[test]
fn transcript_writes_to_a_file_and_refuses_to_clobber() {
    let dir = tempdir();
    let target = dir.path().join("old-chat.md");

    cli()
        .args(["transcript"])
        .arg(fixture("sample-export.json"))
        .args(["--conversation", "conv-linear-0001"])
        .arg("--output")
        .arg(&target)
        .assert()
        .success();
    assert!(target.exists());

    cli()
        .args(["transcript"])
        .arg(fixture("sample-export.json"))
        .args(["--conversation", "conv-linear-0001"])
        .arg("--output")
        .arg(&target)
        .assert()
        .failure()
        .stderr(predicate::str::contains("--force"));

    cli()
        .args(["transcript"])
        .arg(fixture("sample-export.json"))
        .args(["--conversation", "conv-linear-0001"])
        .arg("--output")
        .arg(&target)
        .arg("--force")
        .assert()
        .success();
}

// --------------------------------------------------------------- extract --

#[test]
fn extract_creates_the_handoff_package() {
    let dir = tempdir();
    let out = dir.path().join("handoff");

    let output = cli()
        .args(["extract"])
        .arg(fixture("sample-export.json"))
        .args(["--conversation", "conv-linear-0001"])
        .arg("--output")
        .arg(&out)
        .output()
        .expect("run");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    for name in ["context.md", "transcript.md", "metadata.json"] {
        assert!(out.join(name).exists(), "{name} must be created");
    }
    // Not requested, so not written.
    assert!(!out.join("summarization-prompt.md").exists());
    assert!(!out.join("raw-conversation.json").exists());

    let summary = stdout_of(&output);
    assert!(summary.contains("Created handoff package:"));
    assert!(summary.contains("Active branch:"));
    assert!(summary.contains("Recent context preserved:"));

    let metadata: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(out.join("metadata.json")).expect("read"))
            .expect("metadata.json must be valid JSON");
    assert_eq!(metadata["conversation_id"], "conv-linear-0001");
    assert!(metadata["active_branch_messages"].as_u64().unwrap_or(0) > 0);
    assert!(metadata["source"].as_str().is_some());
}

#[test]
fn extracted_context_has_every_documented_section() {
    let dir = tempdir();
    let out = dir.path().join("handoff");
    cli()
        .args(["extract"])
        .arg(fixture("sample-export.json"))
        .args(["--conversation", "conv-linear-0001"])
        .arg("--output")
        .arg(&out)
        .assert()
        .success();

    let context = std::fs::read_to_string(out.join("context.md")).expect("read context.md");
    assert!(context.starts_with("# Conversation Handoff"));
    for heading in [
        "## Conversation",
        "## Purpose",
        "## Important Background",
        "## Established Facts",
        "## User Preferences and Constraints",
        "## Decisions Already Made",
        "## Terminology and Entities",
        "## Important Technical Details",
        "## Key Conclusions",
        "## Rejected / Superseded Approaches",
        "## Current State",
        "## Open Questions",
        "## Recent Conversation",
        "## Continuation Instructions",
    ] {
        assert!(context.contains(heading), "missing section {heading}");
    }
    assert!(context.contains("transcript.md"));
    assert!(context.contains("Do not restart the discussion from scratch."));
}

#[test]
fn extract_refuses_to_clobber_without_force() {
    let dir = tempdir();
    let out = dir.path().join("handoff");

    cli()
        .args(["extract"])
        .arg(fixture("sample-export.json"))
        .args(["--conversation", "conv-linear-0001"])
        .arg("--output")
        .arg(&out)
        .assert()
        .success();

    cli()
        .args(["extract"])
        .arg(fixture("sample-export.json"))
        .args(["--conversation", "conv-linear-0001"])
        .arg("--output")
        .arg(&out)
        .assert()
        .failure()
        .stderr(predicate::str::contains("already exists"))
        .stderr(predicate::str::contains("--force"));

    cli()
        .args(["extract"])
        .arg(fixture("sample-export.json"))
        .args(["--conversation", "conv-linear-0001"])
        .arg("--output")
        .arg(&out)
        .arg("--force")
        .assert()
        .success();
}

#[test]
fn extract_prompt_mode_adds_the_summarization_prompt() {
    let dir = tempdir();
    let out = dir.path().join("handoff");
    cli()
        .args(["extract"])
        .arg(fixture("sample-export.json"))
        .args([
            "--conversation",
            "conv-linear-0001",
            "--context-mode",
            "prompt",
        ])
        .arg("--output")
        .arg(&out)
        .assert()
        .success();

    let prompt = std::fs::read_to_string(out.join("summarization-prompt.md")).expect("prompt file");
    assert!(prompt.contains("Continuation Instructions"));
    assert!(prompt.to_lowercase().contains("transcript"));
    assert!(
        out.join("context.md").exists(),
        "context.md is still produced"
    );
}

#[test]
fn extract_honours_the_recent_message_budget() {
    let dir = tempdir();
    let out = dir.path().join("handoff");
    let output = cli()
        .args(["extract"])
        .arg(fixture("sample-export.json"))
        .args([
            "--conversation",
            "conv-linear-0001",
            "--recent-messages",
            "2",
        ])
        .arg("--output")
        .arg(&out)
        .arg("--json")
        .output()
        .expect("run");
    let parsed: serde_json::Value = serde_json::from_slice(&output.stdout).expect("json");
    assert_eq!(parsed["metadata"]["recent_messages_preserved"], 2);
}

#[test]
fn extract_selects_by_partial_title_positionally() {
    let dir = tempdir();
    let out = dir.path().join("handoff");
    cli()
        .args(["extract"])
        .arg(fixture("sample-export.json"))
        .arg("Rust CLI design notes")
        .arg("--output")
        .arg(&out)
        .assert()
        .success();
    assert!(out.join("context.md").exists());
}

#[test]
fn extract_raw_writes_the_untouched_source_json() {
    let dir = tempdir();
    let out = dir.path().join("handoff");
    cli()
        .args(["extract"])
        .arg(fixture("sample-export.json"))
        .args(["--conversation", "conv-linear-0001"])
        .arg("--raw")
        .arg("--output")
        .arg(&out)
        .assert()
        .success();

    let raw: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(out.join("raw-conversation.json")).expect("read"),
    )
    .expect("valid JSON");
    assert!(raw.get("mapping").is_some(), "raw JSON keeps the mapping");
}

// ---------------------------------------------------------------- prompt --

#[test]
fn prompt_command_emits_a_vendor_neutral_prompt() {
    let output = cli()
        .args(["prompt"])
        .arg(fixture("sample-export.json"))
        .args(["--conversation", "conv-linear-0001"])
        .output()
        .expect("run");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let text = stdout_of(&output).to_lowercase();
    for phrase in [
        "do not",
        "decision",
        "terminology",
        "open question",
        "superseded",
        "recent",
    ] {
        assert!(text.contains(phrase), "prompt should mention {phrase:?}");
    }
}

// --------------------------------------------------------------- inspect --

#[test]
fn inspect_reports_the_graph_and_lists_nodes() {
    let output = cli()
        .args(["inspect"])
        .arg(fixture("sample-export.json"))
        .args(["--conversation", "conv-branch-0002", "--nodes", "--json"])
        .output()
        .expect("run");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let parsed: serde_json::Value = serde_json::from_slice(&output.stdout).expect("json");
    let nodes = parsed["nodes"].as_array().expect("nodes array");
    assert!(nodes.iter().any(|n| n["on_active_branch"] == true));
    assert!(
        nodes.iter().any(|n| n["on_active_branch"] == false),
        "the abandoned sibling must be visible to inspect even though it is off-branch"
    );
}

// ------------------------------------------------------------------- zip --

#[test]
fn zip_input_behaves_exactly_like_loose_json() {
    let dir = tempdir();
    let archive = zip_fixture(dir.path(), &fixture("sample-export.json"));

    cli()
        .args(["list"])
        .arg(&archive)
        .assert()
        .success()
        .stdout(predicate::str::contains("conv-linear-0001"));

    let out = dir.path().join("handoff");
    cli()
        .args(["extract"])
        .arg(&archive)
        .args(["--conversation", "conv-linear-0001"])
        .arg("--output")
        .arg(&out)
        .assert()
        .success();
    assert!(out.join("context.md").exists());
}

#[test]
fn a_zip_renamed_to_json_is_still_detected() {
    let dir = tempdir();
    let archive = zip_fixture(dir.path(), &fixture("sample-export.json"));
    let disguised = dir.path().join("conversations.json");
    std::fs::rename(&archive, &disguised).expect("rename");

    cli()
        .args(["list"])
        .arg(&disguised)
        .assert()
        .success()
        .stdout(predicate::str::contains("conv-linear-0001"));
}

// --------------------------------------------------- robustness / errors --

#[test]
fn a_malformed_graph_still_produces_output_and_never_panics() {
    let output = cli()
        .args(["list", "--json"])
        .arg(fixture("malformed-export.json"))
        .output()
        .expect("run");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let parsed: serde_json::Value = serde_json::from_slice(&output.stdout).expect("json");
    let conversations = parsed["conversations"].as_array().expect("array");
    assert!(!conversations.is_empty());

    // Every conversation in the damaged fixture must be renderable without a panic.
    for conversation in conversations {
        let Some(id) = conversation["id"].as_str() else {
            continue;
        };
        let result = cli()
            .args(["transcript"])
            .arg(fixture("malformed-export.json"))
            .args(["--conversation", id])
            .output()
            .expect("run");
        let stderr = String::from_utf8_lossy(&result.stderr);
        assert!(
            !stderr.contains("panicked"),
            "conversation {id} panicked:\n{stderr}"
        );
    }
}

#[test]
fn hostile_titles_are_stripped_of_terminal_control_sequences() {
    let output = cli()
        .args(["list"])
        .arg(fixture("malformed-export.json"))
        .output()
        .expect("run");
    let text = stdout_of(&output);
    assert!(!text.contains('\u{202e}'), "bidi override must be stripped");
    assert!(!text.contains('\u{1b}'), "ANSI escape must be stripped");
}

#[test]
fn a_missing_input_file_fails_cleanly() {
    cli()
        .args(["list", "/nonexistent/path/to/conversations.json"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("error:"))
        .stderr(predicate::str::is_match("(?i)panicked").unwrap().not());
}

#[test]
fn a_selector_matching_nothing_fails_with_a_useful_message() {
    cli()
        .args(["show"])
        .arg(fixture("sample-export.json"))
        .args(["--conversation", "does-not-exist-anywhere"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("no conversation"));
}

/// Refusing to guess is only half of correct behavior: the user must also be
/// able to see what matched, or they cannot narrow the selector.
#[test]
fn an_ambiguous_selector_lists_the_candidates_instead_of_guessing() {
    let output = cli()
        .args(["show"])
        .arg(fixture("ambiguous-export.json"))
        .arg("Meeting notes")
        .output()
        .expect("run");
    assert!(!output.status.success(), "must not silently pick one");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("ambiguous"), "{stderr}");
    assert!(
        stderr.contains("amb-0001"),
        "candidate ids must be listed:\n{stderr}"
    );
    assert!(
        stderr.contains("amb-0002"),
        "candidate ids must be listed:\n{stderr}"
    );
    assert!(
        stderr.contains("--conversation") || stderr.contains("--pick"),
        "must tell the user how to disambiguate:\n{stderr}"
    );
}

#[test]
fn a_command_needing_a_conversation_refuses_without_a_selector() {
    cli()
        .args(["show"])
        .arg(fixture("sample-export.json"))
        .assert()
        .failure()
        .stderr(predicate::str::contains("--conversation"));
}

#[test]
fn normal_invocations_are_quiet_on_stderr() {
    let output = cli()
        .args(["list"])
        .arg(fixture("sample-export.json"))
        .output()
        .expect("run");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.trim().is_empty(),
        "a healthy run must not log anything: {stderr}"
    );
}

#[test]
fn verbose_logging_goes_to_stderr_not_stdout() {
    let output = cli()
        .args(["-vv", "list", "--json"])
        .arg(fixture("sample-export.json"))
        .output()
        .expect("run");
    serde_json::from_slice::<serde_json::Value>(&output.stdout)
        .expect("stdout must stay pure JSON even at -vv");
    assert!(
        !String::from_utf8_lossy(&output.stderr).trim().is_empty(),
        "-vv must actually log something"
    );
}

#[test]
fn help_and_version_work() {
    cli().arg("--help").assert().success();
    cli().arg("--version").assert().success();
    for verb in [
        "list",
        "find",
        "show",
        "transcript",
        "extract",
        "prompt",
        "inspect",
    ] {
        cli().args([verb, "--help"]).assert().success();
    }
}
