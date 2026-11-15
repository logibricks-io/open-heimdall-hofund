//! Relay a sign-in page URL when this binary is invoked as `BROWSER`.
//!
//! Claude Code is given `BROWSER` pointing at this executable and
//! `HOFUND_OPENED_URL` naming a file. When invoked with a single `https://`
//! argument, the URL is written to that file and the process exits without
//! running the rest of the CLI. Kept here so `hofund-cli` does not depend on
//! `hofund-plugin-sdk`.

use std::io::Write;
use std::path::Path;

/// Names the file that receives the page Claude Code asked to open.
const OPENED_URL_ENV: &str = "HOFUND_OPENED_URL";

/// When `HOFUND_OPENED_URL` is set and `args` is one `https://` URL, writes the
/// URL to that file and returns `true`. The caller should exit successfully and
/// must not start the gateway.
#[must_use]
pub(crate) fn took_opened_url(args: &[impl AsRef<str>]) -> bool {
    let path = match std::env::var(OPENED_URL_ENV) {
        Ok(value) if !value.is_empty() => value,
        _ => return false,
    };
    if args.len() != 1 {
        return false;
    }
    let url = args[0].as_ref();
    if !url.starts_with("https://") {
        return false;
    }
    write_opened_url(Path::new(&path), url)
}

fn write_opened_url(path: &Path, url: &str) -> bool {
    match std::fs::File::create(path).and_then(|mut file| file.write_all(url.as_bytes())) {
        Ok(()) => true,
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::took_opened_url;

    #[test]
    fn opened_url_relay_rejects_without_env_or_bad_args() {
        assert!(!took_opened_url(&["http://insecure.example"]));
        assert!(!took_opened_url(&["https://a", "https://b"]));
        // Env is unset in this process, so a well-formed URL is also rejected.
        assert!(!took_opened_url(&["https://claude.ai/oauth"]));
    }
}
