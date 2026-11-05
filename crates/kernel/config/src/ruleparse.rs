//! Routing rule line parsing for control-plane validation and Without rewrites.

use crate::ConfigError;

/// Parsed routing rule constraints used when validating group rule lines.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ParsedRule {
    /// Primary `use=` or `pause=` target refs.
    pub uses: Vec<String>,
    /// Whether this is a `pause=` rule.
    pub pause: bool,
}

const KNOWN_KEYS: &[&str] = &[
    "use",
    "model",
    "to",
    "pause",
    "skip",
    "tokens",
    "context",
    "longer",
    "images",
    "image",
    "compact",
    "compacting",
    "compaction",
    "effort",
    "reasoning",
    "thinking",
    "use-effort",
    "useeffort",
    "set-effort",
    "seteffort",
    "agents",
    "agent",
    "intent",
    "asks",
    "about",
    "time",
    "hours",
    "between",
    "tz",
    "zone",
    "timezone",
    "time-zone",
    "days",
    "day",
    "folders",
    "folder",
    "in",
    "project",
    "projects",
    "except-folders",
    "except-folder",
    "exceptfolders",
    "not-in",
    "outside",
    "classifier",
    "classify",
    "by",
    "at",
];

/// Parses one rule line using the reference routing rule grammar.
///
/// # Errors
///
/// Returns [`ConfigError::Invalid`] when a clause is malformed.
pub fn parse_rule_line(line: &str) -> Result<ParsedRule, ConfigError> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Err(rule_error("a rule line must not be empty"));
    }
    let words = rule_words(trimmed);
    if words.is_empty() {
        return Err(rule_error("a rule line must not be empty"));
    }
    let mut parsed = ParsedRule::default();
    let mut saw_use = false;
    for word in words {
        let (key, value, has_value) = split_word(&word);
        let key = key.to_ascii_lowercase();
        if !KNOWN_KEYS.contains(&key.as_str()) {
            return Err(rule_error(format!(
                "unknown rule clause `{word}` (use, pause, tokens, images, effort, agents, intent, compact, time, days, tz, folders, except-folders, classifier, at)"
            )));
        }
        match key.as_str() {
            "use" | "model" | "to" => {
                saw_use = true;
                parsed.pause = false;
                parsed.uses.extend(split_csv(value));
            }
            "pause" | "skip" => {
                if !has_value || value.trim().is_empty() {
                    return Err(rule_error("pause= needs a member ref"));
                }
                saw_use = true;
                parsed.pause = true;
                parsed.uses = vec![value.trim().to_owned()];
            }
            "tokens" | "context" | "longer" => {
                parse_tokens(value).map_err(rule_error)?;
            }
            "images" | "image" => validate_yes_no(value, "images")?,
            "compact" | "compacting" | "compaction" => validate_yes_no(value, "compact")?,
            "effort" | "reasoning" | "thinking" => {
                let effort = if has_value { value.trim() } else { "on" };
                if effort.is_empty() {
                    return Err(rule_error("effort= needs a level"));
                }
            }
            "use-effort" | "useeffort" | "set-effort" | "seteffort" => {
                if value.trim().is_empty() {
                    return Err(rule_error("use-effort= needs a level"));
                }
            }
            "agents" | "agent" => {
                if split_csv(value).is_empty() {
                    return Err(rule_error("agents= needs at least one agent id"));
                }
            }
            "intent" | "asks" | "about" => {
                if value.trim().is_empty() {
                    return Err(rule_error(
                        r#"intent says what the message asks for: intent="writing or fixing tests""#,
                    ));
                }
            }
            "time" | "hours" | "between" => parse_time_window(value).map_err(rule_error)?,
            "days" | "day" => {
                if value.trim().is_empty() {
                    return Err(rule_error("days= needs weekday names"));
                }
            }
            "tz" | "zone" | "timezone" | "time-zone" => {
                if value.trim().is_empty() {
                    return Err(rule_error("tz= needs an IANA time zone name"));
                }
            }
            "folders" | "folder" | "in" | "project" | "projects" => {
                if split_csv(value).is_empty() {
                    return Err(rule_error("folders= needs at least one folder path"));
                }
            }
            "except-folders" | "except-folder" | "exceptfolders" | "not-in" | "outside" => {
                if split_csv(value).is_empty() {
                    return Err(rule_error("except-folders= needs at least one folder path"));
                }
            }
            "classifier" | "classify" | "by" => {
                if value.trim().is_empty() {
                    return Err(rule_error(
                        "classifier= needs the model that classifies intent",
                    ));
                }
            }
            "at" => {
                let place: u32 = value
                    .trim()
                    .parse()
                    .map_err(|_| rule_error(format!("at is a place from 1, not `{value}`")))?;
                if place < 1 {
                    return Err(rule_error(format!("at is a place from 1, not `{value}`")));
                }
            }
            _ => {}
        }
    }
    if !saw_use || parsed.uses.is_empty() {
        return Err(rule_error("use=<model> or pause=<model> is missing"));
    }
    Ok(parsed)
}

/// Rewrites one stored rule line after a member ref was removed (`Rule.Without`).
///
/// Returns `None` when the line should be dropped.
#[must_use]
pub fn rule_without_line(line: &str, removed: &str) -> Option<String> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }
    let Ok(mut parsed) = parse_rule_line(trimmed) else {
        return Some(line.to_owned());
    };
    let removed = removed.trim();
    parsed.uses.retain(|target| !refs_equal(target, removed));
    if parsed.uses.is_empty() {
        return None;
    }
    Some(rebuild_rule_line(trimmed, &parsed))
}

fn refs_equal(left: &str, right: &str) -> bool {
    left.trim().eq_ignore_ascii_case(right.trim())
}

fn rebuild_rule_line(original: &str, parsed: &ParsedRule) -> String {
    let words = rule_words(original);
    let mut out = Vec::new();
    let mut replaced_primary = false;
    for word in words {
        let (key, _, _) = split_word(&word);
        let lower = key.to_ascii_lowercase();
        if matches!(lower.as_str(), "use" | "model" | "to") && !parsed.pause {
            if !replaced_primary {
                out.push(format!("use={}", parsed.uses.join(",")));
                replaced_primary = true;
            }
            continue;
        }
        if matches!(lower.as_str(), "pause" | "skip") && parsed.pause {
            if !replaced_primary {
                out.push(format!("pause={}", parsed.uses[0]));
                replaced_primary = true;
            }
            continue;
        }
        out.push(word);
    }
    if !replaced_primary {
        if parsed.pause {
            out.insert(0, format!("pause={}", parsed.uses[0]));
        } else {
            out.insert(0, format!("use={}", parsed.uses.join(",")));
        }
    }
    out.join(" ")
}

fn rule_words(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut word = String::new();
    let mut quoted = false;
    let mut some = false;
    for ch in line.chars() {
        match ch {
            '"' => {
                quoted = !quoted;
                word.push(ch);
                some = true;
            }
            ' ' | '\t' if !quoted => {
                if some {
                    out.push(word.clone());
                    word.clear();
                    some = false;
                }
            }
            _ => {
                word.push(ch);
                some = true;
            }
        }
    }
    if some {
        out.push(word);
    }
    out
}

fn split_word(word: &str) -> (&str, &str, bool) {
    if let Some((key, value)) = word.split_once('=') {
        (key, value, true)
    } else {
        (word, "", false)
    }
}

fn split_csv(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .map(str::to_owned)
        .collect()
}

fn parse_tokens(text: &str) -> Result<(), String> {
    let trimmed = text.trim();
    if trimmed.contains('-') {
        let (left, right) = trimmed
            .split_once('-')
            .ok_or_else(|| format!("tokens= `{text}` needs min-max"))?;
        parse_token_amount(left)?;
        parse_token_amount(right)?;
        return Ok(());
    }
    parse_token_amount(trimmed)?;
    Ok(())
}

fn parse_token_amount(text: &str) -> Result<u64, String> {
    let mut s = text.trim().to_ascii_lowercase().replace('_', "");
    let mut mul = 1_u64;
    if s.ends_with('k') {
        mul = 1_000;
        s.pop();
    } else if s.ends_with('m') {
        mul = 1_000_000;
        s.pop();
    }
    let base: f64 = s
        .parse()
        .map_err(|_| format!("tokens `{text}` is not a length (200000, 200k, 1m)"))?;
    if base < 0.0 {
        return Err(format!(
            "tokens `{text}` is not a length (200000, 200k, 1m)"
        ));
    }
    let _ = (base * mul as f64) as u64;
    Ok((base * mul as f64) as u64)
}

fn validate_yes_no(value: &str, field: &str) -> Result<(), ConfigError> {
    let lower = value.trim().to_ascii_lowercase();
    if lower.is_empty()
        || matches!(
            lower.as_str(),
            "yes" | "true" | "on" | "1" | "no" | "false" | "off" | "0"
        )
    {
        Ok(())
    } else {
        Err(rule_error(format!(
            "{field} takes no value (or yes/no), not `{value}`"
        )))
    }
}

fn parse_time_window(value: &str) -> Result<(), String> {
    let (hours, _zone) = value
        .split_once('@')
        .map(|(left, right)| (left, Some(right)))
        .unwrap_or((value, None));
    let (from, to) = hours
        .split_once('-')
        .ok_or_else(|| format!("time is hours of the day: time=09:00-18:00, not `{value}`"))?;
    if from.trim().is_empty() || to.trim().is_empty() {
        return Err(format!(
            "time is hours of the day: time=09:00-18:00, not `{value}`"
        ));
    }
    Ok(())
}

fn rule_error(message: impl Into<String>) -> ConfigError {
    ConfigError::Invalid(message.into())
}

#[cfg(test)]
mod tests {
    use super::{parse_rule_line, rule_without_line};

    #[test]
    fn r7a_ruleparse_accepts_reference_clauses() {
        parse_rule_line("use=codex tokens=200k images effort=high").expect("parse");
        parse_rule_line(r#"use=a,b agents=codex intent="chat""#).expect("parse");
        parse_rule_line("pause=chat/model time=09:00-18:00 days=mon-fri").expect("parse");
    }

    #[test]
    fn r7a_ruleparse_rejects_unknown_clause() {
        assert!(parse_rule_line("not-a-clause=yes").is_err());
    }

    #[test]
    fn r7a_rule_without_drops_last_target() {
        assert!(rule_without_line("use=only/model", "only/model").is_none());
    }

    #[test]
    fn r7a_rule_without_rewrites_use_list() {
        let line = rule_without_line("use=a/m,b/big tokens=5", "a/m").expect("line");
        assert!(line.starts_with("use=b/big"));
    }
}
