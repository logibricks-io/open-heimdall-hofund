//! Pi-to-omp session carry without converting agent files.

use std::path::Path;

use hofund_plugin_api::SessionRecord;

use crate::resume::{ResumeCommand, split_instance};

/// Whether a Pi session can be carried to omp without file conversion.
#[must_use]
pub fn pi_to_omp(record: &SessionRecord, omp_present: bool, read_only: bool) -> bool {
    omp_present
        && !read_only
        && split_instance(&record.agent).1.is_none()
        && split_instance(&record.agent).0 == "pi"
        && !record.cwd.is_empty()
        && !record.path.as_os_str().is_empty()
}

/// Builds `omp --fork <path>` argv for a Pi session.
#[must_use]
pub fn pi_to_omp_command(
    record: &SessionRecord,
    omp_present: bool,
    read_only: bool,
) -> Option<ResumeCommand> {
    if !pi_to_omp(record, omp_present, read_only) {
        return None;
    }
    let path = path_to_argv(&record.path)?;
    Some(ResumeCommand {
        argv: vec!["omp".to_owned(), "--fork".to_owned(), path],
        cwd: record.cwd.clone(),
    })
}

fn path_to_argv(path: &Path) -> Option<String> {
    if path.as_os_str().is_empty() {
        return None;
    }
    Some(path.to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use hofund_plugin_api::SessionRecord;

    use super::*;

    fn pi_record(cwd: &str, path: &str) -> SessionRecord {
        let mut record = SessionRecord::new("pi", "pi:one", PathBuf::from(path));
        record.cwd = cwd.to_owned();
        record
    }

    #[test]
    fn carry_builds_omp_fork_argv() {
        let path = if cfg!(windows) {
            r"C:\Users\me\.pi\agent\sessions\work\pi.jsonl"
        } else {
            "/home/me/.pi/agent/sessions/work/pi.jsonl"
        };
        let cmd = pi_to_omp_command(&pi_record("/work/proj", path), true, false).expect("carry");
        assert_eq!(
            cmd.argv,
            vec!["omp".to_owned(), "--fork".to_owned(), path.to_owned()]
        );
        assert_eq!(cmd.cwd, "/work/proj");
    }

    #[test]
    fn carry_refuses_wsl_and_missing_omp() {
        let mut record = pi_record("/work", "/work/pi.jsonl");
        record.agent = "pi@wsl:Ubuntu".to_owned();
        assert!(pi_to_omp_command(&record, true, false).is_none());
        assert!(pi_to_omp_command(&pi_record("/work", "/work/pi.jsonl"), false, false).is_none());
        assert!(pi_to_omp_command(&pi_record("/work", "/work/pi.jsonl"), true, true).is_none());
    }
}
