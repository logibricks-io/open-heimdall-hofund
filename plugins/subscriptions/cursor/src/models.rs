//! Parsing `cursor-agent models` and folding families.

use hofund_plugin_api::{ModelList, ModelSpec, PluginError};

const DEFAULT_CONTEXT: u64 = 200_000;

/// One parsed row before families are folded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RawModel {
    id: String,
    name: String,
    context: u64,
}

/// Parses cursor-agent model list text.
pub(crate) fn parse_list(text: &str) -> Vec<RawModel> {
    let mut out = Vec::new();
    for line in strip_ansi(text).lines() {
        let line = line.trim();
        if let Some((id, name)) = parse_line(line) {
            let context = cursor_context(&id, &name);
            out.push(RawModel { id, name, context });
        }
    }
    out
}

fn parse_line(line: &str) -> Option<(String, String)> {
    let (id, rest) = line.split_once(" - ")?;
    let id = id.trim();
    if id.is_empty() {
        return None;
    }
    let first = id.chars().next()?;
    if !first.is_ascii_alphanumeric() {
        return None;
    }
    if !id
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | ':' | '_' | '-'))
    {
        return None;
    }
    let mut name = rest.replace('\u{200b}', "");
    name = name.split_whitespace().collect::<Vec<_>>().join(" ");
    for suffix in ["(default)", "(current)"] {
        if let Some(stripped) = name.strip_suffix(suffix) {
            name = stripped.trim().to_owned();
        }
    }
    Some((id.to_owned(), name))
}

fn cursor_context(id: &str, name: &str) -> u64 {
    if let Some(m) = million_in_name(name) {
        return m;
    }
    let mut base = id.strip_prefix("cursor-").unwrap_or(id);
    loop {
        let next = strip_variant_suffix(base);
        if next == base {
            break;
        }
        base = next;
    }
    if base == "auto" {
        return DEFAULT_CONTEXT;
    }
    DEFAULT_CONTEXT
}

fn million_in_name(name: &str) -> Option<u64> {
    for word in name.split_whitespace() {
        let digits = word.trim_matches(|ch: char| !ch.is_ascii_digit());
        if (word.contains('M') || word.contains('m'))
            && let Ok(n) = digits.parse::<u64>()
        {
            return Some(n * 1_000_000);
        }
    }
    None
}

fn strip_variant_suffix(id: &str) -> &str {
    for suffix in [
        "-fast",
        "-none",
        "-low",
        "-medium",
        "-high",
        "-xhigh",
        "-extra-high",
        "-max",
        "-thinking",
    ] {
        if let Some(base) = id.strip_suffix(suffix)
            && !base.is_empty()
        {
            return base;
        }
    }
    id
}

struct Family {
    id: String,
    variants: Vec<RawModel>,
    efforts: Vec<String>,
}

/// Folds variant ids into one model per family.
pub(crate) fn collapse(raw: Vec<RawModel>) -> Vec<RawModel> {
    let mut families: Vec<Family> = Vec::new();
    let mut index = std::collections::HashMap::new();
    for model in raw {
        let (family, effort) = split_id(&model.id);
        let entry = index.entry(family.clone()).or_insert_with(|| {
            let pos = families.len();
            families.push(Family {
                id: family.clone(),
                variants: Vec::new(),
                efforts: Vec::new(),
            });
            pos
        });
        families[*entry].variants.push(model);
        families[*entry].efforts.push(effort);
    }
    let mut out = Vec::new();
    for family in families {
        if family.variants.len() == 1 {
            out.push(family.variants[0].clone());
            continue;
        }
        out.push(family.model());
    }
    without_capacity(out)
}

fn split_id(id: &str) -> (String, String) {
    let mut s = id.to_string();
    let mut fast = false;
    let mut thinking = false;
    if let Some(base) = s.strip_suffix("-fast")
        && !base.is_empty()
    {
        s = base.to_owned();
        fast = true;
    }
    if let Some(base) = s.strip_suffix("-thinking")
        && !base.is_empty()
    {
        s = base.to_owned();
        thinking = true;
    }
    let mut effort = String::new();
    for (word, level) in [
        ("extra-high", "xhigh"),
        ("xhigh", "xhigh"),
        ("minimal", "minimal"),
        ("none", "none"),
        ("low", "low"),
        ("medium", "medium"),
        ("high", "high"),
        ("max", "max"),
    ] {
        let marker = format!("-{word}");
        if let Some(base) = s.strip_suffix(&marker)
            && !base.is_empty()
        {
            s = base.to_owned();
            effort = level.to_owned();
            break;
        }
    }
    if !thinking
        && let Some(base) = s.strip_suffix("-thinking")
        && !base.is_empty()
    {
        s = base.to_owned();
        thinking = true;
    }
    if thinking {
        s.push_str("-thinking");
    }
    if fast {
        s.push_str("-fast");
    }
    (s, effort)
}

impl Family {
    fn by_effort(&self) -> std::collections::HashMap<String, String> {
        let mut out = std::collections::HashMap::new();
        let mut default_index = self.efforts.iter().position(|e| e.is_empty());
        for (i, variant) in self.variants.iter().enumerate() {
            let effort = self.efforts[i].clone();
            out.entry(effort.clone())
                .or_insert_with(|| variant.id.clone());
            if default_index.is_none() && !effort.is_empty() {
                let label = effort_label(&effort);
                if !name_contains(&variant.name, label) {
                    default_index = Some(i);
                }
            }
        }
        if default_index.is_none() {
            default_index = self.efforts.iter().position(|e| e == "medium");
        }
        let def = default_index.unwrap_or(0);
        out.entry(String::new())
            .or_insert_with(|| self.variants[def].id.clone());
        out
    }

    fn model(&self) -> RawModel {
        let map = self.by_effort();
        let def_id = map
            .get("")
            .cloned()
            .unwrap_or_else(|| self.variants[0].id.clone());
        let mut name = String::new();
        let mut context = 0_u64;
        for (i, variant) in self.variants.iter().enumerate() {
            if variant.id == def_id {
                let effort = &self.efforts[i];
                name = variant.name.clone();
                if !effort.is_empty() {
                    name = remove_words(&name, effort_label(effort));
                }
            }
            if variant.context > 0 && (context == 0 || variant.context < context) {
                context = variant.context;
            }
        }
        RawModel {
            id: self.id.clone(),
            name,
            context,
        }
    }
}

fn effort_label(level: &str) -> &str {
    match level {
        "xhigh" => "Extra High",
        "minimal" => "Minimal",
        "none" => "None",
        "low" => "Low",
        "medium" => "Medium",
        "high" => "High",
        "max" => "Max",
        _ => "",
    }
}

fn name_contains(name: &str, words: &str) -> bool {
    words.is_empty()
        || name
            .to_ascii_lowercase()
            .contains(&words.to_ascii_lowercase())
}

fn remove_words(name: &str, words: &str) -> String {
    if words.is_empty() {
        return name.to_owned();
    }
    let name_words: Vec<_> = name.split_whitespace().collect();
    let strip: Vec<_> = words.split_whitespace().collect();
    if strip.is_empty() || strip.len() > name_words.len() {
        return name.to_owned();
    }
    for start in 0..=name_words.len() - strip.len() {
        if name_words[start..start + strip.len()] == strip[..] {
            let mut out = name_words[..start].to_vec();
            out.extend_from_slice(&name_words[start + strip.len()..]);
            return out.join(" ");
        }
    }
    name.to_owned()
}

fn without_capacity(models: Vec<RawModel>) -> Vec<RawModel> {
    let strip = |name: &str| -> String {
        name.split_whitespace()
            .filter(|word| !word.ends_with('M') && !word.contains("(1M)"))
            .collect::<Vec<_>>()
            .join(" ")
    };
    let mut counts = std::collections::HashMap::new();
    for model in &models {
        *counts.entry(model.name.to_ascii_lowercase()).or_insert(0) += 1;
        let stripped = strip(&model.name);
        if stripped != model.name {
            *counts.entry(stripped.to_ascii_lowercase()).or_insert(0) += 1;
        }
    }
    models
        .into_iter()
        .map(|mut model| {
            let stripped = strip(&model.name);
            if stripped != model.name
                && !stripped.is_empty()
                && counts
                    .get(&stripped.to_ascii_lowercase())
                    .copied()
                    .unwrap_or(0)
                    <= 1
            {
                model.name = stripped;
            }
            model
        })
        .collect()
}

/// Builds the API model list from CLI text.
pub(crate) fn model_list(text: &str) -> Result<ModelList, PluginError> {
    let collapsed = collapse(parse_list(text));
    let specs = collapsed
        .into_iter()
        .map(|model| {
            ModelSpec::new(model.id, Some(model.name)).map(|spec| spec.with_context(model.context))
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(ModelList::new(specs, None))
}

fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\u{1b}' {
            for next in chars.by_ref() {
                if next.is_ascii_alphabetic() {
                    break;
                }
            }
            continue;
        }
        out.push(ch);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = "Available models\n\nauto - Auto (default)\n\
gpt-5.3-codex-low - Codex 5.3 Low\ngpt-5.3-codex - Codex 5.3\ngpt-5.3-codex-high - Codex 5.3 High\ngpt-5.3-codex-high-fast - Codex 5.3 High Fast\n\
grok-4.7-low - Grok 4.7  Low\ngrok-4.7-low-fast - Grok 4.7  Low Fast\ngrok-4.7-medium - Grok 4.7  Medium\ngrok-4.7-medium-fast - Grok 4.7  Medium Fast\n\
grok-4.7-xhigh - Grok 4.7  Extra High\ngrok-4.7-xhigh-fast - Grok 4.7  Extra High Fast\n\
claude-opus-4-7-low - Claude Opus 4.7 1M Low\nclaude-opus-4-7-xhigh - Claude Opus 4.7 1M\nclaude-opus-4-7-max - Claude Opus 4.7 1M Max\n\
claude-opus-4-7-thinking-low - Claude Opus 4.7 1M Low Thinking\nclaude-opus-4-7-thinking-xhigh - Claude Opus 4.7 1M Thinking\n\
claude-4.6-opus-high-thinking - Claude Opus 4.6 1M Thinking\nclaude-4.6-opus-max-thinking - Claude Opus 4.6 1M Max Thinking\n\
gpt-5.5-none - GPT-5.5 1M None\ngpt-5.5-medium - GPT-5.5 1M\ngpt-5.5-extra-high - GPT-5.5 1M Extra High\n\
gpt-5.5-medium-fast - GPT-5.5 Fast\ngpt-5.5-extra-high-fast - GPT-5.5 Extra High Fast\n\
claude-4.6-sonnet-medium-thinking - Claude Sonnet 4.6 1M Thinking\ncomposer-2.5 - Composer 2.5\n\nTip: use --model <id>\n";

    #[test]
    fn xhigh_on_short_name_does_not_panic() {
        let _ = collapse(parse_list("auto - Auto Extra High\n"));
    }

    #[test]
    fn collapse_matches_reference_families() {
        let raw = parse_list(FIXTURE);
        let collapsed = collapse(raw.clone());
        let rows: Vec<String> = collapsed
            .iter()
            .map(|m| format!("{}|{}|{}", m.id, m.name, m.context))
            .collect();
        assert!(rows.iter().any(|row| row.starts_with("auto|Auto|")));
        assert!(
            rows.iter()
                .any(|row| row.starts_with("composer-2.5|Composer 2.5|"))
        );
        assert!(raw.len() > collapsed.len());
    }

    #[test]
    fn strip_ansi_removes_escape_sequences() {
        assert_eq!(
            strip_ansi("\u{1b}[4mhttps://example.test/x\u{1b}[24m"),
            "https://example.test/x"
        );
    }

    #[test]
    fn context_from_name() {
        assert_eq!(
            cursor_context("claude-opus-4-7-low", "Claude Opus 4.7 1M Low"),
            1_000_000
        );
        assert_eq!(cursor_context("composer-2.5", "Composer 2.5"), 200_000);
    }
}
