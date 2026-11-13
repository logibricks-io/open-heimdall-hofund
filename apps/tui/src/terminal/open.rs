//! Opens a sign-in URL without printing it or inheriting terminal output.

use std::process::{Command, Stdio};

pub(crate) fn open_url(url: &str) -> Result<(), String> {
    let mut command = Command::new(open_program());
    #[cfg(target_os = "windows")]
    command.args(["url.dll,FileProtocolHandler", url]);
    #[cfg(not(target_os = "windows"))]
    command.arg(url);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command
        .status()
        .map_err(|_| "could not ask the OS to open the sign-in page".to_owned())
        .and_then(|status| {
            if status.success() {
                Ok(())
            } else {
                Err("the OS could not open the sign-in page".to_owned())
            }
        })
}

#[cfg(target_os = "macos")]
fn open_program() -> &'static str {
    "open"
}

#[cfg(all(unix, not(target_os = "macos")))]
fn open_program() -> &'static str {
    "xdg-open"
}

#[cfg(target_os = "windows")]
fn open_program() -> &'static str {
    "rundll32"
}

#[cfg(test)]
mod tests {
    #[test]
    fn opener_is_the_platform_tool() {
        #[cfg(target_os = "macos")]
        assert_eq!(super::open_program(), "open");
        #[cfg(all(unix, not(target_os = "macos")))]
        assert_eq!(super::open_program(), "xdg-open");
        #[cfg(target_os = "windows")]
        assert_eq!(super::open_program(), "rundll32");
    }
}
