//! Zstd compression for Codex `/responses` turns.
//!
//! Codex CLI sends POST bodies to `{codex_base}/responses` with
//! `Content-Encoding: zstd` at level 3. Other paths and methods stay plain JSON.

use std::cell::Cell;
use std::sync::atomic::{AtomicBool, Ordering};

use url::Url;

/// When false, turns stay plain JSON (unit tests against mock backends).
static ENABLED: AtomicBool = AtomicBool::new(true);

thread_local! {
    static TEST_OVERRIDE: Cell<Option<bool>> = const { Cell::new(None) };
}

fn compression_enabled() -> bool {
    TEST_OVERRIDE.with(|slot| {
        slot.get()
            .unwrap_or_else(|| ENABLED.load(Ordering::Relaxed))
    })
}

/// Sets whether Codex turns are zstd-compressed, for tests. Returns a restore closure.
pub fn codex_zstd_for_tests(on: bool) -> impl FnOnce() {
    TEST_OVERRIDE.with(|slot| {
        let was = slot.replace(Some(on));
        move || {
            TEST_OVERRIDE.with(|slot| slot.set(was));
        }
    })
}

/// Whether `url` is a Codex backend POST to `/responses`.
#[must_use]
pub fn codex_responses_turn(url: &str) -> bool {
    let Ok(parsed) = Url::parse(url) else {
        return false;
    };
    parsed.path().ends_with("/codex/responses")
}

/// Compresses `body` for a Codex turn when `method` is POST, `url` is
/// [`codex_responses_turn`], compression is enabled, and `content_encoding`
/// is absent or `identity`. Returns the bytes to send and whether they are
/// zstd-compressed.
#[must_use]
pub fn maybe_compress_turn(
    method: &str,
    url: &str,
    content_encoding: Option<&str>,
    body: &[u8],
) -> (Vec<u8>, bool) {
    if method != "POST" || !compression_enabled() || !codex_responses_turn(url) {
        return (body.to_vec(), false);
    }
    if content_encoding.is_some_and(|enc| !enc.eq_ignore_ascii_case("identity")) {
        return (body.to_vec(), false);
    }
    (compress_level3(body), true)
}

/// Decompresses a zstd turn body in unit tests (max `limit` bytes).
#[must_use]
pub fn decompress_turn(body: &[u8], limit: usize) -> Option<Vec<u8>> {
    zstd::bulk::decompress(body, limit).ok()
}

fn compress_level3(body: &[u8]) -> Vec<u8> {
    zstd::bulk::compress(body, 3).unwrap_or_else(|_| body.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn r47a_e_codex_zstd_only_on_responses_turn() {
        let _restore = codex_zstd_for_tests(true);
        let body = br#"{"model":"gpt-5","input":"hello"}"#.repeat(1000);
        let turn = "https://chatgpt.com/backend-api/codex/responses";
        let (z, ok) = maybe_compress_turn("POST", turn, None, &body);
        assert!(ok, "turn is compressed");
        assert!(z.len() < body.len() / 10);
        let plain = zstd::bulk::decompress(&z, body.len()).expect("decompress");
        assert_eq!(plain, body);

        for url in [
            "https://chatgpt.com/backend-api/codex/responses/compact",
            "https://chatgpt.com/backend-api/codex/models",
            "https://api.openai.com/v1/responses",
        ] {
            let (out, ok) = maybe_compress_turn("POST", url, None, &body);
            assert!(!ok, "{url} must stay plain");
            assert_eq!(out, body);
        }
        let (out, ok) = maybe_compress_turn("GET", turn, None, &body);
        assert!(!ok);
        assert_eq!(out, body);

        let (out, ok) = maybe_compress_turn("POST", turn, Some("gzip"), &body);
        assert!(!ok);
        assert_eq!(out, body);
    }
}
