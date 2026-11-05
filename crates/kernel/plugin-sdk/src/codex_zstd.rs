//! Zstd compression for Codex `/responses` turns.
//!
//! Codex CLI sends POST bodies to `{codex_base}/responses` with
//! `Content-Encoding: zstd` at level 3. Other paths and methods stay plain JSON.

use std::cell::Cell;

use url::Url;

thread_local! {
    static TEST_OVERRIDE: Cell<Option<bool>> = const { Cell::new(None) };
}

#[cfg(test)]
thread_local! {
    static COMPRESS_FAIL: Cell<bool> = const { Cell::new(false) };
}

fn compression_enabled() -> bool {
    TEST_OVERRIDE.with(Cell::get).unwrap_or(true)
}

/// Sets whether Codex turns are zstd-compressed on this thread. Returns a restore closure.
///
/// The override is thread-local: call this on the same thread that sends the HTTP request.
#[doc(hidden)]
pub fn codex_zstd_for_tests(on: bool) -> impl FnOnce() {
    TEST_OVERRIDE.with(|slot| {
        let was = slot.replace(Some(on));
        move || {
            TEST_OVERRIDE.with(|slot| slot.set(was));
        }
    })
}

#[cfg(test)]
pub(crate) fn force_compress_fail_for_tests(on: bool) -> impl FnOnce() {
    COMPRESS_FAIL.with(|slot| {
        let was = slot.replace(on);
        move || {
            COMPRESS_FAIL.set(was);
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
    match compress_level3(body) {
        Ok(compressed) => (compressed, true),
        Err(()) => (body.to_vec(), false),
    }
}

/// Decompresses a zstd turn body in unit tests (max `limit` bytes).
#[must_use]
pub fn decompress_turn(body: &[u8], limit: usize) -> Option<Vec<u8>> {
    zstd::bulk::decompress(body, limit).ok()
}

fn compress_level3(body: &[u8]) -> Result<Vec<u8>, ()> {
    #[cfg(test)]
    if COMPRESS_FAIL.with(Cell::get) {
        return Err(());
    }
    zstd::bulk::compress(body, 3).map_err(|_| ())
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

    #[test]
    fn r47a_e_compress_failure_stays_plain() {
        let _zstd = codex_zstd_for_tests(true);
        let _fail = force_compress_fail_for_tests(true);
        let body = br#"{"model":"gpt-5","input":"hello"}"#.repeat(1000);
        let turn = "https://chatgpt.com/backend-api/codex/responses";
        let (out, ok) = maybe_compress_turn("POST", turn, None, &body);
        assert!(!ok);
        assert_eq!(out, body);
    }
}
