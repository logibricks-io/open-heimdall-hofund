//! Relay a sign-in page URL when hofund is invoked as `BROWSER`.
//!
//! Claude Code is given `BROWSER` pointing at the hofund executable (or a
//! documented wrapper) and [`OPENED_URL_ENV`] naming a file. When invoked with
//! a single `https://` argument, the URL is written to that file and the
//! process exits without running the rest of the CLI.

use std::io::Write;
use std::path::Path;

/// Names the file that receives the page Claude Code asked to open.
pub const OPENED_URL_ENV: &str = "HOFUND_OPENED_URL";

/// When `OPENED_URL_ENV` is set and `args` is one `https://` URL, writes the
/// URL to that file and returns `true`. The caller should exit successfully and
/// must not start the gateway.
#[must_use]
pub fn took_opened_url(args: &[impl AsRef<str>]) -> bool {
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
        assert!(!took_opened_url(&["https://claude.ai/oauth"]));
    }
}
