//! Foreground agent resume with `Command` argv (no shell).

use std::path::Path;
use std::process::Command;

use hofund_control_proto::SessionResume;

/// Runs `resume` in the foreground after leaving the alternate screen.
///
/// # Errors
///
/// Returns when argv is empty, cwd is missing, or the command fails to start.
pub fn run_foreground(resume: &SessionResume) -> Result<(), String> {
    let Some(program) = resume.argv.first() else {
        return Err("empty argv".to_owned());
    };
    if resume.cwd.is_empty() {
        return Err("missing cwd".to_owned());
    }
    let cwd = Path::new(&resume.cwd);
    if !cwd.is_dir() {
        return Err(format!(
            "its folder {} isn't there any more",
            tilde_display(&resume.cwd)
        ));
    }
    let mut command = Command::new(program);
    command.args(&resume.argv[1..]).current_dir(cwd);
    command
        .status()
        .map_err(|error| format!("resume: {error}"))?;
    Ok(())
}

#[must_use]
fn tilde_display(path: &str) -> String {
    if let Some(home) = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")) {
        let home = home.to_string_lossy();
        let prefix = format!("{home}/");
        if let Some(rest) = path.strip_prefix(prefix.as_str()) {
            return format!("~/{rest}");
        }
    }
    path.to_owned()
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::run_foreground;
    use hofund_control_proto::SessionResume;

    #[test]
    fn empty_argv_is_rejected() {
        let mut resume = SessionResume::default();
        resume.cwd = "/".to_owned();
        let err = run_foreground(&resume).expect_err("argv");
        assert!(err.contains("empty argv"));
    }

    #[test]
    #[cfg(unix)]
    fn command_uses_argv_not_shell() {
        let dir = PathBuf::from("/tmp/hofund-tui-resume-test");
        let _ = std::fs::create_dir_all(&dir);
        let mut resume = SessionResume::default();
        resume.argv = vec!["/bin/echo".to_owned(), "hofund-r44".to_owned()];
        resume.cwd = dir.to_string_lossy().into_owned();
        assert!(run_foreground(&resume).is_ok());
    }
}
