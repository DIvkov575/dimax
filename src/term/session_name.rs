//! Derives a human-readable name for a server-pane whose foreground
//! process is a recognized AI coding CLI session -- so an unnamed pane
//! running one of these shows up as something meaningful instead of a
//! bare UUID prefix.
//!
//! Claude Code and Codex are detected via their own on-disk session
//! transcript, which the command line is inspected for a
//! session-identifying flag or the tool name to locate:
//! - **Claude Code** carries an explicit `--session-id <uuid>` or
//!   `--resume <uuid>` argument, which names its transcript file
//!   directly (`~/.claude/projects/<encoded-cwd>/<uuid>.jsonl`) --
//!   Claude Code also periodically appends an `ai-title` line to that
//!   transcript, a short auto-generated summary of the conversation
//!   intended for exactly this kind of display use. The most recent one
//!   is what gets returned.
//! - **Codex** has no such flag or title field at all -- its CLI
//!   invocation carries no session identifier a caller can observe, so
//!   the only bridge back to a transcript file is matching `cwd` against
//!   each rollout file's own recorded `cwd` (best-effort: ambiguous if
//!   more than one Codex session ever ran in the same directory, in
//!   which case whichever file was modified most recently wins). Since
//!   there's no title either, the first real user message's first few
//!   words stand in for one.
//!
//! **opencode**, **omp** ("Oh My Pi"), and **herdr** have no documented
//! on-disk transcript format to mine a real title from, so they're
//! recognized by binary name alone and simply named after the tool
//! itself -- still a real improvement over an anonymous short id, without
//! guessing at an unverified file format.

use crate::protocol::SessionKind;
use std::path::{Path, PathBuf};

/// Classify a foreground process's short name (`ForegroundProcessInfo::
/// process_name`, e.g. `"claude"`, `"codex"`) as one of the recognized
/// tools, if any -- see [`SessionKind`]. Binary-name matching only --
/// the same rules [`is_claude_invocation`]/[`is_codex_invocation`]/
/// [`named_by_binary_only`] use for title-lookup dispatch, just without
/// needing the full command line those also inspect for `claude`'s
/// `--session-id`/`--resume` flag (irrelevant for classification, only
/// for finding its transcript). Decoupled from [`derive_session_name`]'s
/// title lookup (which only ever runs once, while a pane is still
/// unnamed -- see `daemon::state::State::server_list`): this is meant
/// to run on every pane on every `server_list` call, since it needs
/// nothing more than the `process_name` string `ServerPane::
/// foreground_info` already fetches.
pub fn classify(process_name: &str) -> Option<SessionKind> {
    let name = process_name.to_lowercase();
    if name.contains("claude") {
        return Some(SessionKind::Claude);
    }
    if name.contains("codex") {
        return Some(SessionKind::Codex);
    }
    if name.contains("opencode") {
        return Some(SessionKind::Opencode);
    }
    // Exact match only -- see `named_by_binary_only`'s doc comment on
    // why a substring match on "omp" false-positives too easily.
    if name == "omp" {
        return Some(SessionKind::Omp);
    }
    if name.contains("herdr") {
        return Some(SessionKind::Herdr);
    }
    None
}

/// Look up a display name for the CLI session running as `cmd` (the
/// foreground process's full command line, `argv[0]` first) with
/// current directory `cwd`. Returns `None` if `cmd` isn't recognized as
/// one of the tools in this module's doc comment, no title could be
/// resolved, or `$HOME` isn't set -- all expected outcomes for the vast
/// majority of server-panes (plain shells, editors, etc.), not error
/// conditions.
pub fn derive_session_name(cmd: &[String], cwd: &str) -> Option<String> {
    let home = std::env::var("HOME").ok()?;
    derive_session_name_under(cmd, cwd, Path::new(&home))
}

/// [`derive_session_name`] with `home` passed explicitly rather than
/// read from `$HOME` -- the actual logic, and what every test below
/// calls directly against a temp directory instead of mutating the
/// real process environment (which would race other tests running in
/// parallel in the same process).
fn derive_session_name_under(cmd: &[String], cwd: &str, home: &Path) -> Option<String> {
    if let Some(uuid) = claude_session_id(cmd) {
        return claude_title_for_session(home, &uuid);
    }
    if is_codex_invocation(cmd) {
        return codex_title_for_cwd(home, cwd);
    }
    if let Some(name) = named_by_binary_only(cmd) {
        return Some(name.to_string());
    }
    None
}

/// Tools recognized purely by their binary name, each just named after
/// itself -- see the module doc comment for why these don't get a
/// transcript-derived title the way Claude Code and Codex do.
fn named_by_binary_only(cmd: &[String]) -> Option<&'static str> {
    let arg0 = cmd.first()?;
    let file_name = Path::new(arg0)
        .file_name()?
        .to_string_lossy()
        .to_lowercase();
    if file_name.contains("opencode") {
        return Some("opencode");
    }
    // "omp" is short enough that a substring match would false-positive
    // on unrelated binaries (e.g. "compass" contains "omp") -- exact
    // match only.
    if file_name == "omp" {
        return Some("omp");
    }
    if file_name.contains("herdr") {
        return Some("herdr");
    }
    None
}

/// `true` if `cmd`'s first element looks like a Claude Code CLI
/// invocation -- the binary name contains "claude" (covers the plain
/// `claude` on `$PATH`, and the versioned paths under
/// `~/.toolbox/tools/claude-code/<version>/claude` /
/// `~/.local/share/claude/versions/<version>` seen in practice).
fn is_claude_invocation(cmd: &[String]) -> bool {
    cmd.first().is_some_and(|arg0| {
        Path::new(arg0)
            .file_name()
            .is_some_and(|name| name.to_string_lossy().to_lowercase().contains("claude"))
    })
}

fn is_codex_invocation(cmd: &[String]) -> bool {
    cmd.first().is_some_and(|arg0| {
        Path::new(arg0)
            .file_name()
            .is_some_and(|name| name.to_string_lossy().to_lowercase().contains("codex"))
    })
}

/// Extract the session UUID from a Claude Code command line's
/// `--session-id <uuid>` or `--resume <uuid>` argument, if present.
/// Returns `None` for a `claude` invocation with neither flag (a brand
/// new, not-yet-`--resume`d session) -- expected for most freshly
/// spawned panes, not a parse failure.
fn claude_session_id(cmd: &[String]) -> Option<String> {
    if !is_claude_invocation(cmd) {
        return None;
    }
    cmd.iter()
        .zip(cmd.iter().skip(1))
        .find(|(flag, _)| flag.as_str() == "--session-id" || flag.as_str() == "--resume")
        .map(|(_, value)| value.clone())
}

/// The most recently modified `<home>/.claude/projects/*/<uuid>.jsonl`
/// file (the `*` -- an encoded cwd -- doesn't need to be known: the
/// uuid alone is unique across every project directory).
fn find_claude_transcript(home: &Path, session_id: &str) -> Option<PathBuf> {
    let projects_dir = home.join(".claude").join("projects");
    let filename = format!("{session_id}.jsonl");
    let mut matches: Vec<PathBuf> = std::fs::read_dir(&projects_dir)
        .ok()?
        .filter_map(Result::ok)
        .map(|entry| entry.path().join(&filename))
        .filter(|path| path.is_file())
        .collect();
    matches.sort_by_key(|path| std::fs::metadata(path).and_then(|m| m.modified()).ok());
    matches.pop()
}

/// The most recent `ai-title` value recorded in `session_id`'s
/// transcript, or `None` if the transcript doesn't exist yet, has no
/// title line yet (very early in the conversation), or fails to parse.
fn claude_title_for_session(home: &Path, session_id: &str) -> Option<String> {
    let path = find_claude_transcript(home, session_id)?;
    last_matching_json_field(&path, "ai-title", "aiTitle")
}

/// Scan `path` line-by-line from the end, one JSON object per line
/// (`.jsonl`), returning the first line (i.e. most recent) whose
/// `"type"` field equals `type_value` and which carries a non-empty
/// string at `field_name`. These transcripts are append-only and can
/// grow into the hundreds of megabytes (observed in practice), so this
/// reads the file backwards in fixed-size chunks rather than loading it
/// whole or scanning forward from the start -- the target line is
/// always near the end, and a multi-hundred-MB `read_to_string` for a
/// single trailing field would be wasteful on every attach-menu open.
fn last_matching_json_field(path: &Path, type_value: &str, field_name: &str) -> Option<String> {
    for line in ReverseLineReader::new(path)?.take(MAX_LINES_SCANNED_FROM_END) {
        let Ok(obj) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        if obj.get("type").and_then(|v| v.as_str()) != Some(type_value) {
            continue;
        }
        if let Some(value) = obj.get(field_name).and_then(|v| v.as_str())
            && !value.is_empty()
        {
            return Some(value.to_string());
        }
    }
    None
}

/// How far back to search for a matching line before giving up. The
/// target line type (`ai-title`, or a Codex `user_message`) is
/// typically emitted often enough to land well within this window even
/// in an active session, and this bounds the worst case (a huge
/// transcript with no matching line at all -- e.g. a session that
/// hasn't reached its first title yet) to a fixed amount of work.
const MAX_LINES_SCANNED_FROM_END: usize = 2000;

/// Chunk size for `ReverseLineReader`'s backward reads. Large enough
/// that a single title/user-message line (routinely a few hundred bytes
/// to a few KB) almost never straddles more than two chunks.
const REVERSE_READ_CHUNK: usize = 64 * 1024;

/// Yields complete lines from a file starting at its last line and
/// working backward, without loading the whole file into memory.
/// Built for exactly one caller (`last_matching_json_field`), so it only
/// implements what that needs: `Iterator<Item = String>`, lossy UTF-8
/// (a `.jsonl` transcript is not expected to contain invalid UTF-8, but
/// a lossy decode is cheap insurance against ever panicking on one),
/// and no seek-back-and-retry-forward complexity -- it keeps a buffer of
/// not-yet-yielded bytes read from progressively earlier chunks and
/// peels one line at a time off the end of that buffer.
struct ReverseLineReader {
    file: std::fs::File,
    /// Byte offset in `file` of the start of `buf`'s content -- the
    /// next chunk read (if `buf` runs out of complete lines) starts
    /// immediately before this.
    pos: u64,
    /// Bytes read so far but not yet yielded as lines, in file order
    /// (not reversed) -- new chunks are prepended as `pos` moves
    /// backward.
    buf: Vec<u8>,
}

impl ReverseLineReader {
    fn new(path: &Path) -> Option<Self> {
        let file = std::fs::File::open(path).ok()?;
        let pos = file.metadata().ok()?.len();
        Some(Self {
            file,
            pos,
            buf: Vec::new(),
        })
    }

    /// Pull one more chunk from `file` immediately before `pos`,
    /// prepending it to `buf`. Returns `false` if already at the start
    /// of the file (nothing more to read).
    fn read_prev_chunk(&mut self) -> bool {
        if self.pos == 0 {
            return false;
        }
        use std::io::{Read, Seek, SeekFrom};
        let chunk_len = REVERSE_READ_CHUNK.min(self.pos as usize);
        let start = self.pos - chunk_len as u64;
        let mut chunk = vec![0u8; chunk_len];
        if self.file.seek(SeekFrom::Start(start)).is_err()
            || self.file.read_exact(&mut chunk).is_err()
        {
            return false;
        }
        chunk.extend_from_slice(&self.buf);
        self.buf = chunk;
        self.pos = start;
        true
    }
}

impl Iterator for ReverseLineReader {
    type Item = String;

    fn next(&mut self) -> Option<String> {
        loop {
            if let Some(newline_idx) = self.buf.iter().rposition(|&b| b == b'\n') {
                let line = self.buf.split_off(newline_idx + 1);
                self.buf.pop(); // drop the newline itself
                if line.is_empty() {
                    // A trailing blank line (e.g. the file ends with
                    // "...}\n") -- skip it and keep looking rather than
                    // yielding an empty string as if it were content.
                    continue;
                }
                return Some(String::from_utf8_lossy(&line).into_owned());
            }
            // No newline in what's buffered yet -- either more of the
            // file to pull in, or (once `pos` hits 0) this is the
            // file's first line with no leading newline before it.
            if !self.read_prev_chunk() {
                if self.buf.is_empty() {
                    return None;
                }
                return Some(String::from_utf8_lossy(&std::mem::take(&mut self.buf)).into_owned());
            }
        }
    }
}

/// The most recently modified Codex rollout file (under
/// `<home>/.codex/sessions/**/*.jsonl`) whose recorded
/// `session_meta.cwd` matches `cwd` exactly. Ambiguous by construction
/// if more than one Codex session has ever run in the same directory --
/// see module doc.
fn find_codex_transcript(home: &Path, cwd: &str) -> Option<PathBuf> {
    let sessions_dir = home.join(".codex").join("sessions");
    let mut matches: Vec<PathBuf> = Vec::new();
    visit_jsonl_files(&sessions_dir, &mut matches);
    matches.retain(|path| rollout_cwd(path).as_deref() == Some(cwd));
    matches.sort_by_key(|path| std::fs::metadata(path).and_then(|m| m.modified()).ok());
    matches.pop()
}

/// Recursively collect every `.jsonl` file under `dir` into `out`.
/// Codex rollouts live under `sessions/<year>/<month>/<day>/*.jsonl`, a
/// depth not worth hard-coding -- this just walks whatever's there.
fn visit_jsonl_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        if path.is_dir() {
            visit_jsonl_files(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "jsonl") {
            out.push(path);
        }
    }
}

/// Read just enough of a Codex rollout file's first line to recover its
/// `session_meta.payload.cwd` -- unlike the title/`ai-title` lookup,
/// this is always the very first line, so no backward scan is needed.
fn rollout_cwd(path: &Path) -> Option<String> {
    let file = std::fs::File::open(path).ok()?;
    let first_line = std::io::BufRead::lines(std::io::BufReader::new(file))
        .next()?
        .ok()?;
    let obj: serde_json::Value = serde_json::from_str(&first_line).ok()?;
    obj.get("payload")?.get("cwd")?.as_str().map(str::to_string)
}

/// A short display name derived from `cwd`'s Codex rollout's first real
/// user message -- see module doc for why Codex needs this fallback
/// instead of a real title field. Truncated at a word boundary to keep
/// it short enough to display in the same column a Claude `ai-title`
/// would occupy.
fn codex_title_for_cwd(home: &Path, cwd: &str) -> Option<String> {
    let path = find_codex_transcript(home, cwd)?;
    let first_message = first_codex_user_message(&path)?;
    Some(truncate_at_word_boundary(&first_message, 40))
}

/// The first `event_msg`/`user_message` line's `message` text -- the
/// first thing the *user* actually typed, as opposed to the injected
/// `AGENTS.md`/environment-context turns that precede it in
/// `response_item` entries (see the module doc's investigation: those
/// aren't representative of what the session is "about").
fn first_codex_user_message(path: &Path) -> Option<String> {
    let file = std::fs::File::open(path).ok()?;
    for line in std::io::BufRead::lines(std::io::BufReader::new(file)) {
        let Ok(line) = line else { continue };
        let Ok(obj) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        if obj.get("type").and_then(|v| v.as_str()) != Some("event_msg") {
            continue;
        }
        let payload = obj.get("payload")?;
        if payload.get("type").and_then(|v| v.as_str()) != Some("user_message") {
            continue;
        }
        if let Some(message) = payload.get("message").and_then(|v| v.as_str())
            && !message.trim().is_empty()
        {
            return Some(message.trim().to_string());
        }
    }
    None
}

/// Truncate `s` to at most `max_chars`, breaking at the last preceding
/// whitespace rather than mid-word, then flattening any internal
/// newlines to spaces (a multi-line first message would otherwise
/// render as a garbled single menu row).
fn truncate_at_word_boundary(s: &str, max_chars: usize) -> String {
    let flattened: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    let chars: Vec<char> = flattened.chars().collect();
    if chars.len() <= max_chars {
        return flattened;
    }
    let truncated = &chars[..max_chars];
    match truncated.iter().rposition(|c| c.is_whitespace()) {
        Some(idx) if idx > 0 => truncated[..idx].iter().collect(),
        _ => truncated.iter().collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_recognizes_every_supported_tool() {
        assert_eq!(classify("claude"), Some(SessionKind::Claude));
        assert_eq!(classify("codex"), Some(SessionKind::Codex));
        assert_eq!(classify("opencode"), Some(SessionKind::Opencode));
        assert_eq!(classify("omp"), Some(SessionKind::Omp));
        assert_eq!(classify("herdr"), Some(SessionKind::Herdr));
    }

    #[test]
    fn classify_is_case_insensitive() {
        assert_eq!(classify("Claude"), Some(SessionKind::Claude));
        assert_eq!(classify("CODEX"), Some(SessionKind::Codex));
    }

    #[test]
    fn classify_omp_requires_an_exact_match() {
        // Same false-positive concern as `named_by_binary_only`: "omp"
        // is short enough that a substring match would misclassify an
        // unrelated binary.
        assert_eq!(classify("compass"), None);
        assert_eq!(classify("omp"), Some(SessionKind::Omp));
    }

    #[test]
    fn classify_returns_none_for_unrecognized_processes() {
        assert_eq!(classify("bash"), None);
        assert_eq!(classify("vim"), None);
        assert_eq!(classify(""), None);
    }

    #[test]
    fn is_claude_invocation_matches_plain_and_versioned_paths() {
        assert!(is_claude_invocation(&["claude".to_string()]));
        assert!(is_claude_invocation(&[
            "/Users/dev/.toolbox/tools/claude-code/2.1.221.618/claude".to_string()
        ]));
        assert!(!is_claude_invocation(&["bash".to_string()]));
        assert!(!is_claude_invocation(&[]));
    }

    #[test]
    fn is_codex_invocation_matches_plain_and_versioned_paths() {
        assert!(is_codex_invocation(&["codex".to_string()]));
        assert!(is_codex_invocation(&[
            "/Users/dev/.toolbox/tools/codex/0.146.0/codex".to_string()
        ]));
        assert!(!is_codex_invocation(&["zsh".to_string()]));
    }

    #[test]
    fn claude_session_id_reads_session_id_flag() {
        let cmd = vec![
            "claude".to_string(),
            "--session-id".to_string(),
            "abc-123".to_string(),
        ];
        assert_eq!(claude_session_id(&cmd), Some("abc-123".to_string()));
    }

    #[test]
    fn claude_session_id_reads_resume_flag() {
        let cmd = vec![
            "claude".to_string(),
            "--resume".to_string(),
            "xyz-789".to_string(),
        ];
        assert_eq!(claude_session_id(&cmd), Some("xyz-789".to_string()));
    }

    #[test]
    fn claude_session_id_none_without_either_flag() {
        let cmd = vec![
            "claude".to_string(),
            "--dangerously-skip-permissions".to_string(),
        ];
        assert_eq!(claude_session_id(&cmd), None);
    }

    #[test]
    fn claude_session_id_none_for_non_claude_command() {
        let cmd = vec![
            "bash".to_string(),
            "--session-id".to_string(),
            "abc".to_string(),
        ];
        assert_eq!(claude_session_id(&cmd), None);
    }

    #[test]
    fn truncate_at_word_boundary_leaves_short_text_untouched() {
        assert_eq!(truncate_at_word_boundary("fix the bug", 40), "fix the bug");
    }

    #[test]
    fn truncate_at_word_boundary_breaks_on_whitespace_not_mid_word() {
        let long = "implement a brand new feature for the dashboard widget system";
        let result = truncate_at_word_boundary(long, 20);
        assert!(result.chars().count() <= 20);
        // Every word in the (space-joined) result must be a complete
        // word from the original -- none chopped mid-word.
        let original_words: std::collections::HashSet<&str> = long.split_whitespace().collect();
        assert!(
            result
                .split_whitespace()
                .all(|w| original_words.contains(w)),
            "result {result:?} contains a word not present whole in the original"
        );
    }

    #[test]
    fn truncate_at_word_boundary_flattens_newlines() {
        assert_eq!(
            truncate_at_word_boundary("line one\nline two", 40),
            "line one line two"
        );
    }

    fn write_jsonl(path: &Path, lines: &[&str]) {
        std::fs::write(path, lines.join("\n") + "\n").unwrap();
    }

    #[test]
    fn last_matching_json_field_finds_the_most_recent_matching_line() {
        let dir =
            std::env::temp_dir().join(format!("dmx-session-name-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("transcript.jsonl");
        write_jsonl(
            &path,
            &[
                r#"{"type":"other"}"#,
                r#"{"type":"ai-title","aiTitle":"first title"}"#,
                r#"{"type":"other"}"#,
                r#"{"type":"ai-title","aiTitle":"latest title"}"#,
            ],
        );
        assert_eq!(
            last_matching_json_field(&path, "ai-title", "aiTitle"),
            Some("latest title".to_string())
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn last_matching_json_field_none_when_no_line_matches() {
        let dir =
            std::env::temp_dir().join(format!("dmx-session-name-test-{}", std::process::id() + 1));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("transcript.jsonl");
        write_jsonl(&path, &[r#"{"type":"other"}"#]);
        assert_eq!(last_matching_json_field(&path, "ai-title", "aiTitle"), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn last_matching_json_field_scans_across_a_chunk_boundary() {
        // Force at least two backward-read chunks by padding the file
        // well past REVERSE_READ_CHUNK, with the matching line at the
        // very start (i.e. the last chunk read).
        let dir =
            std::env::temp_dir().join(format!("dmx-session-name-test-{}", std::process::id() + 2));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("transcript.jsonl");
        let padding_line = format!(r#"{{"type":"other","pad":"{}"}}"#, "x".repeat(200));
        let mut lines: Vec<String> = Vec::new();
        lines.push(r#"{"type":"ai-title","aiTitle":"early title"}"#.to_string());
        for _ in 0..(REVERSE_READ_CHUNK / padding_line.len() + 10) {
            lines.push(padding_line.clone());
        }
        std::fs::write(&path, lines.join("\n") + "\n").unwrap();
        assert_eq!(
            last_matching_json_field(&path, "ai-title", "aiTitle"),
            Some("early title".to_string())
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn reverse_line_reader_yields_lines_in_reverse_order() {
        let dir =
            std::env::temp_dir().join(format!("dmx-session-name-test-{}", std::process::id() + 3));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("plain.txt");
        std::fs::write(&path, "one\ntwo\nthree\n").unwrap();
        let lines: Vec<String> = ReverseLineReader::new(&path).unwrap().collect();
        assert_eq!(
            lines,
            vec!["three".to_string(), "two".to_string(), "one".to_string()]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn reverse_line_reader_handles_a_file_with_no_trailing_newline() {
        let dir =
            std::env::temp_dir().join(format!("dmx-session-name-test-{}", std::process::id() + 4));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("no-trailing-newline.txt");
        std::fs::write(&path, "one\ntwo").unwrap();
        let lines: Vec<String> = ReverseLineReader::new(&path).unwrap().collect();
        assert_eq!(lines, vec!["two".to_string(), "one".to_string()]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn first_codex_user_message_skips_non_user_message_lines() {
        let dir =
            std::env::temp_dir().join(format!("dmx-session-name-test-{}", std::process::id() + 5));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("rollout.jsonl");
        write_jsonl(
            &path,
            &[
                r#"{"type":"session_meta","payload":{"cwd":"/tmp"}}"#,
                r#"{"type":"response_item","payload":{"type":"message","role":"developer"}}"#,
                r#"{"type":"event_msg","payload":{"type":"user_message","message":"fix the bug in parser.rs"}}"#,
                r#"{"type":"event_msg","payload":{"type":"user_message","message":"a later message"}}"#,
            ],
        );
        assert_eq!(
            first_codex_user_message(&path),
            Some("fix the bug in parser.rs".to_string())
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn find_codex_transcript_matches_by_cwd_and_picks_most_recent() {
        let dir =
            std::env::temp_dir().join(format!("dmx-session-name-test-{}", std::process::id() + 6));
        let sessions_dir = dir
            .join(".codex")
            .join("sessions")
            .join("2026")
            .join("01")
            .join("01");
        std::fs::create_dir_all(&sessions_dir).unwrap();

        let other_cwd_path = sessions_dir.join("other-cwd.jsonl");
        write_jsonl(
            &other_cwd_path,
            &[r#"{"type":"session_meta","payload":{"cwd":"/elsewhere"}}"#],
        );

        let older_path = sessions_dir.join("older.jsonl");
        write_jsonl(
            &older_path,
            &[r#"{"type":"session_meta","payload":{"cwd":"/target"}}"#],
        );

        let newer_path = sessions_dir.join("newer.jsonl");
        write_jsonl(
            &newer_path,
            &[r#"{"type":"session_meta","payload":{"cwd":"/target"}}"#],
        );

        // Ensure a distinguishable, real mtime ordering rather than
        // relying on filesystem write timing to happen to differ.
        let now = std::time::SystemTime::now();
        std::fs::File::open(&older_path)
            .unwrap()
            .set_modified(now - std::time::Duration::from_secs(60))
            .unwrap();
        std::fs::File::open(&newer_path)
            .unwrap()
            .set_modified(now)
            .unwrap();

        let found = find_codex_transcript(&dir, "/target");

        assert_eq!(found, Some(newer_path));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn derive_session_name_under_resolves_a_claude_session() {
        let dir =
            std::env::temp_dir().join(format!("dmx-session-name-test-{}", std::process::id() + 7));
        let projects_dir = dir
            .join(".claude")
            .join("projects")
            .join("-some-encoded-cwd");
        std::fs::create_dir_all(&projects_dir).unwrap();
        write_jsonl(
            &projects_dir.join("abc-123.jsonl"),
            &[r#"{"type":"ai-title","aiTitle":"fix the parser"}"#],
        );

        let cmd = vec![
            "claude".to_string(),
            "--resume".to_string(),
            "abc-123".to_string(),
        ];
        assert_eq!(
            derive_session_name_under(&cmd, "/whatever", &dir),
            Some("fix the parser".to_string())
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn derive_session_name_under_resolves_a_codex_session_by_cwd() {
        let dir =
            std::env::temp_dir().join(format!("dmx-session-name-test-{}", std::process::id() + 8));
        let sessions_dir = dir
            .join(".codex")
            .join("sessions")
            .join("2026")
            .join("01")
            .join("01");
        std::fs::create_dir_all(&sessions_dir).unwrap();
        write_jsonl(
            &sessions_dir.join("rollout.jsonl"),
            &[
                r#"{"type":"session_meta","payload":{"cwd":"/target"}}"#,
                r#"{"type":"event_msg","payload":{"type":"user_message","message":"add tests for the parser"}}"#,
            ],
        );

        let cmd = vec!["/opt/homebrew/bin/codex".to_string()];
        assert_eq!(
            derive_session_name_under(&cmd, "/target", &dir),
            Some("add tests for the parser".to_string())
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn derive_session_name_under_none_for_an_unrecognized_command() {
        let dir =
            std::env::temp_dir().join(format!("dmx-session-name-test-{}", std::process::id() + 9));
        let cmd = vec!["bash".to_string()];
        assert_eq!(derive_session_name_under(&cmd, "/whatever", &dir), None);
    }

    #[test]
    fn named_by_binary_only_matches_opencode_plain_and_versioned_paths() {
        assert_eq!(
            named_by_binary_only(&["opencode".to_string()]),
            Some("opencode")
        );
        assert_eq!(
            named_by_binary_only(&["/Users/dev/.local/bin/opencode".to_string()]),
            Some("opencode")
        );
    }

    #[test]
    fn named_by_binary_only_matches_omp_exactly_not_as_a_substring() {
        assert_eq!(named_by_binary_only(&["omp".to_string()]), Some("omp"));
        assert_eq!(
            named_by_binary_only(&["/opt/homebrew/bin/omp".to_string()]),
            Some("omp")
        );
        assert_eq!(named_by_binary_only(&["compass".to_string()]), None);
    }

    #[test]
    fn named_by_binary_only_matches_herdr() {
        assert_eq!(named_by_binary_only(&["herdr".to_string()]), Some("herdr"));
        assert_eq!(
            named_by_binary_only(&["/usr/local/bin/herdr".to_string()]),
            Some("herdr")
        );
    }

    #[test]
    fn named_by_binary_only_none_for_unrelated_commands() {
        assert_eq!(named_by_binary_only(&["bash".to_string()]), None);
        assert_eq!(named_by_binary_only(&[]), None);
    }

    #[test]
    fn derive_session_name_under_names_an_opencode_pane_after_the_tool() {
        let dir =
            std::env::temp_dir().join(format!("dmx-session-name-test-{}", std::process::id() + 10));
        let cmd = vec!["opencode".to_string()];
        assert_eq!(
            derive_session_name_under(&cmd, "/whatever", &dir),
            Some("opencode".to_string())
        );
    }
}
