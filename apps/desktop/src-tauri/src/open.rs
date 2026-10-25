//! Opens the configuration file with the platform file opener.

use std::path::Path;
use std::process::Command;

use hofund_daemon::ensure_config_file;

pub(crate) fn open_config_file(path: &Path) -> Result<(), String> {
    ensure_config_file(path).map_err(|error| error.to_string())?;
    let status = open_command(path)
        .status()
        .map_err(|error| error.to_string())?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("could not open {}", path.display()))
    }
}

pub(crate) fn open_program() -> &'static str {
    #[cfg(target_os = "macos")]
    {
        "open"
    }
    #[cfg(target_os = "windows")]
    {
        "cmd"
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        "xdg-open"
    }
}

fn open_command(path: &Path) -> Command {
    let mut command = Command::new(open_program());
    if open_program() == "cmd" {
        command.args(["/C", "start", "", &path.display().to_string()]);
    } else {
        command.arg(path);
    }
    command
}

#[cfg(test)]
mod tests {
    use super::open_program;

    #[test]
    fn the_opener_is_the_platform_tool() {
        let program = open_program();
        #[cfg(target_os = "macos")]
        assert_eq!(program, "open");
        #[cfg(target_os = "windows")]
        assert_eq!(program, "cmd");
        #[cfg(all(unix, not(target_os = "macos")))]
        assert_eq!(program, "xdg-open");
    }
}
