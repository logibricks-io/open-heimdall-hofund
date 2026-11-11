//! Bounded transcript reads and body-only Markdown rendering.

use std::fmt::Write as _;
use std::io;

use chrono::{Local, TimeZone};
use hofund_plugin_api::{
    PluginError, SessionCompression, SessionFile, SessionRecord, SessionStore, SessionTranscript,
};
use thiserror::Error;

use crate::index::read_incremental;
use crate::open::{OpenError, open_session_file};

const MAX_TRANSCRIPT_BYTES: usize = 400_000;
const MAX_PART_CHARS: usize = 8_000;

/// A bounded transcript read failure.
#[derive(Debug, Error)]
pub enum SessionReadError {
    /// The listed file failed containment or file-type checks.
    #[error("session file is not readable")]
    Open(#[from] OpenError),
    /// The file could not be read.
    #[error("session transcript I/O failed")]
    Io(#[from] io::Error),
    /// The plugin could not parse the transcript.
    #[error("session transcript parser failed")]
    Plugin(#[from] PluginError),
}

/// Reads a listed session file through its plugin's incremental parser.
///
/// Consent and selection are checked by the caller before this function.
///
/// # Errors
///
/// Returns [`SessionReadError`] when opening, bounded reading, or parsing fails.
pub fn transcript(
    root: &std::path::Path,
    store: &dyn SessionStore,
    file: &SessionFile,
) -> Result<SessionTranscript, SessionReadError> {
    let mut parser = store.transcript_parser(file)?;
    let mut opened = open_session_file(root, &file.path)?;
    let mut parser_error = None;
    let mut on_value = |value: &serde_json::Value| {
        if parser_error.is_none()
            && let Err(error) = parser.push(value)
        {
            parser_error = Some(error);
        }
    };
    let outcome = match file.compression {
        SessionCompression::None => read_incremental(&mut opened, 0, &mut on_value)?,
        SessionCompression::Zstd => {
            let mut decoder = zstd::stream::read::Decoder::new(opened)?;
            read_incremental(&mut decoder, 0, &mut on_value)?
        }
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "unsupported session compression",
            )
            .into());
        }
    };
    if let Some(error) = parser_error {
        return Err(error.into());
    }
    let mut transcript = parser.finish()?;
    transcript.cut |= outcome.cut;
    bound(&mut transcript);
    Ok(transcript)
}

fn bound(transcript: &mut SessionTranscript) {
    let mut bytes = 0_usize;
    let mut cut = transcript.cut;
    for part in &mut transcript.parts {
        part.text = part.text.trim().to_owned();
        let chars = part.text.chars().count();
        if chars > MAX_PART_CHARS {
            part.text = part.text.chars().take(MAX_PART_CHARS).collect();
            cut = true;
        }
        if bytes >= MAX_TRANSCRIPT_BYTES {
            cut = true;
            part.text.clear();
            continue;
        }
        let room = MAX_TRANSCRIPT_BYTES - bytes;
        if part.text.len() > room {
            let mut end = room;
            while !part.text.is_char_boundary(end) {
                end -= 1;
            }
            part.text.truncate(end);
            cut = true;
        }
        bytes = bytes.saturating_add(part.text.len());
    }
    transcript.parts.retain(|part| !part.text.is_empty());
    transcript.cut = cut;
}

/// Renders a session transcript as Markdown without writing a file.
#[must_use]
pub fn markdown(record: &SessionRecord, transcript: &SessionTranscript) -> String {
    let mut out = String::new();
    let title = one_line(record.title.trim());
    let _ = writeln!(
        out,
        "# {}\n",
        if title.is_empty() {
            "(no prompt)"
        } else {
            &title
        }
    );
    metadata(&mut out, "Agent", &record.agent);
    metadata(&mut out, "Session ID", &format!("`{}`", record.id));
    metadata(&mut out, "Folder", &record.cwd);
    if let Some(started) = when(record.start_ms) {
        metadata(&mut out, "Started", &started);
    }
    if let Some(last) = when(record.last_ms) {
        metadata(&mut out, "Last active", &last);
    }
    metadata(&mut out, "Models", &record.model_ids().join(", "));
    metadata(&mut out, "File", &record.path.to_string_lossy());
    if transcript.cut {
        out.push_str("\n*Not all of this session was kept: what follows is what is left of it.*\n");
    }
    out.push_str("\n---\n");
    let mut who = "";
    let mut said = false;
    for part in &transcript.parts {
        let text = part.text.trim();
        if text.is_empty() {
            continue;
        }
        said = true;
        if part.role != "tool" && part.role != who {
            who = &part.role;
            let heading = if who == "assistant" {
                "Assistant"
            } else {
                "User"
            };
            let _ = write!(out, "\n## {heading}\n");
        }
        out.push('\n');
        match part.kind.as_str() {
            "thinking" => folded(&mut out, "Thinking", text, false),
            "tool_use" => {
                let name = if part.name.is_empty() {
                    "tool"
                } else {
                    &part.name
                };
                let _ = writeln!(out, "**Tool call: {}**\n", one_line(name));
                let language = if text.starts_with(['{', '[']) {
                    "json"
                } else {
                    ""
                };
                fenced(&mut out, language, text);
            }
            "tool_result" => {
                let label = if part.name.is_empty() {
                    "Tool result".to_owned()
                } else {
                    format!("Tool result: {}", one_line(&part.name))
                };
                folded(&mut out, &label, text, true);
            }
            "context" => folded(&mut out, "Context", text, true),
            "image" => out.push_str("*[image]*\n"),
            _ => {
                out.push_str(text);
                out.push('\n');
            }
        }
    }
    if !said {
        out.push_str("\n*Nothing was said in this session.*\n");
    }
    out
}

fn metadata(out: &mut String, key: &str, value: &str) {
    if !value.is_empty() {
        let _ = writeln!(out, "- **{key}:** {value}");
    }
}

fn folded(out: &mut String, summary: &str, text: &str, code: bool) {
    let summary = summary
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;");
    let _ = writeln!(out, "<details>\n<summary>{summary}</summary>\n");
    if code {
        fenced(out, "", text);
    } else {
        out.push_str(text);
        out.push('\n');
    }
    out.push_str("\n</details>\n");
}

fn fenced(out: &mut String, language: &str, text: &str) {
    let mut longest = 0;
    let mut run = 0;
    for character in text.chars() {
        if character == '`' {
            run += 1;
            longest = longest.max(run);
        } else {
            run = 0;
        }
    }
    let fence = "`".repeat((longest + 1).max(3));
    let _ = writeln!(out, "{fence}{language}\n{text}\n{fence}");
}

fn when(milliseconds: i64) -> Option<String> {
    (milliseconds != 0)
        .then(|| Local.timestamp_millis_opt(milliseconds).single())
        .flatten()
        .map(|time| time.format("%Y-%m-%d %H:%M:%S %:z").to_string())
}

fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use hofund_plugin_api::SessionPart;

    use super::*;

    #[test]
    fn markdown_is_rendered_in_memory_with_safe_fences() {
        let mut record = SessionRecord::new("fake", "fake:one", "/sessions/one.jsonl");
        record.title = "A title".to_owned();
        record.cwd = "/work".to_owned();
        let mut transcript = SessionTranscript::default();
        transcript.parts = vec![
            SessionPart::new("user", "text", "", "Question"),
            SessionPart::new("assistant", "tool_use", "reader", "{\"run\":```}"),
        ];
        let markdown = markdown(&record, &transcript);
        assert!(markdown.starts_with("# A title\n"));
        assert!(markdown.contains("## User"));
        assert!(markdown.contains("**Tool call: reader**"));
        assert!(markdown.contains("````json"));
    }

    #[test]
    fn transcript_bounds_trim_and_cut_parts() {
        let mut transcript = SessionTranscript::default();
        transcript.parts = vec![SessionPart::new(
            "user",
            "text",
            "",
            format!(" {} ", "x".repeat(MAX_PART_CHARS + 1)),
        )];
        bound(&mut transcript);
        assert!(transcript.cut);
        assert_eq!(transcript.parts[0].text.chars().count(), MAX_PART_CHARS);
    }
}
