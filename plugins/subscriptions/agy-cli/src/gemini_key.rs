//! Gemini API key stored by the agy CLI.
//!
//! Offered only when `modelProvider` is `gemini`. The key is read from the
//! caller-supplied environment first. This function does not read the process
//! environment and does not add a provider row; that offer has no registry yet.

use std::fmt;

use hofund_plugin_api::SecretString;
use hofund_plugin_sdk::json::{self, Value};

/// A Gemini API key agy saved for itself.
pub struct GeminiKeyOffer {
    key: SecretString,
    base: String,
}

impl GeminiKeyOffer {
    /// The API key. Do not log it.
    #[must_use]
    pub fn key(&self) -> &SecretString {
        &self.key
    }

    /// The Gemini base, with `/v1beta` when the saved base omitted it.
    #[must_use]
    pub fn base(&self) -> &str {
        &self.base
    }
}

impl fmt::Debug for GeminiKeyOffer {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("GeminiKeyOffer")
            .field("key", &"<redacted>")
            .field("base", &self.base)
            .finish()
    }
}

/// Why a Gemini provider setting was not offered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GeminiKeySkip {
    /// `modelProvider` is `gemini` and no key was supplied.
    NoKey,
    /// The base points at this gateway.
    PointsAtGateway,
}

/// Reads agy's `settings.json` text.
///
/// `None` means there is nothing to offer (not the Gemini provider, or the
/// file is empty). `Some(Err)` is a skipped Gemini provider. `Some(Ok)` is a
/// key to import. `env_key` wins over a key in the file. JSON comments and a
/// leading BOM are ignored.
#[must_use]
pub fn read_gemini_key(
    settings: &str,
    env_key: Option<&str>,
    env_base: Option<&str>,
    gateway_addrs: &[&str],
) -> Option<Result<GeminiKeyOffer, GeminiKeySkip>> {
    let settings = strip_jsonc(settings);
    let settings = settings.trim();
    if settings.is_empty() {
        return None;
    }
    let Ok(value) = json::from_str::<Value>(settings) else {
        return None;
    };
    let provider = value
        .get("modelProvider")
        .and_then(Value::as_str)
        .unwrap_or("");
    if !provider.eq_ignore_ascii_case("gemini") {
        return None;
    }
    let key = env_key
        .map(str::trim)
        .filter(|key| !key.is_empty())
        .or_else(|| {
            value
                .get("GEMINI_API_KEY")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|key| !key.is_empty())
        });
    let Some(key) = key else {
        return Some(Err(GeminiKeySkip::NoKey));
    };
    let base = gemini_base(
        env_base.or_else(|| value.get("GOOGLE_GEMINI_BASE_URL").and_then(Value::as_str)),
    );
    if points_at_gateway(&base, gateway_addrs) {
        return Some(Err(GeminiKeySkip::PointsAtGateway));
    }
    Some(Ok(GeminiKeyOffer {
        key: SecretString::from(key.to_owned()),
        base,
    }))
}

/// `Same` when `offer` is already one of `existing`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GeminiKeyStatus {
    /// The key is not stored yet.
    New,
    /// The same key is already stored.
    Same,
}

/// Compares `offer` with keys already stored. The comparison does not print the key.
#[must_use]
pub fn gemini_key_status(offer: &GeminiKeyOffer, existing: &[&str]) -> GeminiKeyStatus {
    let key = hofund_plugin_sdk::oauth::secret_text(offer.key());
    if existing.contains(&key) {
        GeminiKeyStatus::Same
    } else {
        GeminiKeyStatus::New
    }
}

fn gemini_base(raw: Option<&str>) -> String {
    let base = raw.unwrap_or("").trim();
    if base.is_empty() || base.ends_with("/v1beta") {
        base.to_owned()
    } else {
        format!("{}/v1beta", base.trim_end_matches('/'))
    }
}

fn points_at_gateway(base: &str, gateway_addrs: &[&str]) -> bool {
    gateway_addrs.iter().any(|addr| {
        let addr = addr.trim();
        !addr.is_empty() && base.contains(&format!("//{addr}"))
    })
}

fn strip_jsonc(text: &str) -> String {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut out = String::new();
    let mut chars = text.chars().peekable();
    let mut in_string = false;
    let mut escape = false;
    while let Some(character) = chars.next() {
        if in_string {
            out.push(character);
            if escape {
                escape = false;
                continue;
            }
            if character == '\\' {
                escape = true;
                continue;
            }
            if character == '"' {
                in_string = false;
            }
            continue;
        }
        if character == '"' {
            in_string = true;
            out.push(character);
            continue;
        }
        if character == '/' && chars.peek() == Some(&'/') {
            chars.next();
            for next in chars.by_ref() {
                if next == '\n' {
                    out.push('\n');
                    break;
                }
            }
            continue;
        }
        out.push(character);
    }
    out
}
