//! Sessions and Library read-path probes (GET only).

use hofund_control_client::ClientError;

use crate::state::PageId;

/// Control GET path for a deferred page, when present.
#[must_use]
pub fn read_path(page: PageId) -> Option<&'static str> {
    match page {
        PageId::Sessions => Some("/v1/hofund/sessions?days=7"),
        PageId::Library => Some("/v1/hofund/library"),
        _ => None,
    }
}

/// Outcome of probing a deferred read route.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProbeOutcome {
    /// HTTP 404: route not shipped in this build.
    NotInBuild,
    /// HTTP 2xx: empty shell until a later row fills rows.
    Ready,
    /// Any other failure.
    Error(String),
}

impl ProbeOutcome {
    /// Maps a control GET result to a probe outcome.
    #[must_use]
    pub fn from_get_result(result: Result<(), ClientError>) -> Self {
        match result {
            Ok(()) => Self::Ready,
            Err(error) => {
                if error
                    .http_status()
                    .is_some_and(|status| status.as_u16() == 404)
                {
                    Self::NotInBuild
                } else {
                    Self::Error(error.to_string())
                }
            }
        }
    }
}

/// Applies a probe outcome to the page shell message state.
pub fn apply_probe_to_shell(shell: &mut super::PageShell, outcome: ProbeOutcome) {
    match outcome {
        ProbeOutcome::NotInBuild => {
            shell.set_not_in_build(true);
        }
        ProbeOutcome::Ready => {
            shell.set_message("Ready (empty shell).");
        }
        ProbeOutcome::Error(message) => {
            shell.set_message(format!("Could not load: {message}"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{ProbeOutcome, apply_probe_to_shell, read_path};
    use crate::pages::PageShell;
    use crate::state::PageId;

    #[test]
    fn apply_not_in_build_shows_deferred_message() {
        let mut shell = PageShell::new(PageId::Sessions, "Loading");
        apply_probe_to_shell(&mut shell, ProbeOutcome::NotInBuild);
        assert!(shell.message().contains("not in this build yet"));
    }

    #[test]
    fn sessions_and_library_have_paths() {
        assert!(read_path(PageId::Sessions).is_some());
        assert!(read_path(PageId::Library).is_some());
        assert!(read_path(PageId::Agents).is_none());
    }
}
