//! Minimal routing rule line parsing for control-plane validation.

use crate::ConfigError;

/// Parsed routing rule constraints used when validating group rule lines.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ParsedRule {
    /// Agent ids from `use=` clauses.
    pub uses: Vec<String>,
    /// Token bounds from `tokens=` clauses as `(min, max)` pairs.
    pub token_ranges: Vec<(u64, u64)>,
}

/// Parses one rule line. Only `use=` and `tokens=` are recognized for now.
///
/// # Errors
///
/// Returns [`ConfigError::Invalid`] when a clause is malformed.
pub fn parse_rule_line(line: &str) -> Result<ParsedRule, ConfigError> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Err(ConfigError::Invalid(
            "a rule line must not be empty".to_owned(),
        ));
    }
    let mut parsed = ParsedRule::default();
    for token in trimmed.split_whitespace() {
        if let Some(rest) = token.strip_prefix("use=") {
            let id = rest.trim();
            if id.is_empty() {
                return Err(ConfigError::Invalid(
                    "use= needs a non-empty agent id".to_owned(),
                ));
            }
            parsed.uses.push(id.to_owned());
        } else if let Some(rest) = token.strip_prefix("tokens=") {
            parsed
                .token_ranges
                .push(parse_tokens(rest).map_err(ConfigError::Invalid)?);
        } else {
            return Err(ConfigError::Invalid(format!(
                "unknown rule clause `{token}`; only use= and tokens= are supported"
            )));
        }
    }
    Ok(parsed)
}

fn parse_tokens(text: &str) -> Result<(u64, u64), String> {
    let (left, right) = text
        .split_once('-')
        .ok_or_else(|| format!("tokens= `{text}` needs min-max"))?;
    let min = left
        .trim()
        .parse::<u64>()
        .map_err(|_| format!("tokens= min `{left}` is not a number"))?;
    let max = right
        .trim()
        .parse::<u64>()
        .map_err(|_| format!("tokens= max `{right}` is not a number"))?;
    if min > max {
        return Err(format!("tokens= range `{text}` has min greater than max"));
    }
    Ok((min, max))
}

#[cfg(test)]
mod tests {
    use super::parse_rule_line;

    #[test]
    fn r7a_ruleparse_use_and_tokens() {
        let rule = parse_rule_line("use=codex tokens=1000-8000").expect("parse");
        assert_eq!(rule.uses, ["codex"]);
        assert_eq!(rule.token_ranges, [(1000, 8000)]);
    }

    #[test]
    fn r7a_ruleparse_rejects_unknown_clause() {
        assert!(parse_rule_line("intent=chat").is_err());
    }
}
