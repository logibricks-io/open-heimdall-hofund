//! G-10 serve role fields until R-7a lands on main.

use hofund_control_client::hofund_control_proto::StatusResponse;

/// Optional `StatusResponse` fields introduced by G-10 (R-7a).
pub trait StatusServeFields {
    /// Who spawned the daemon (`tui`, `serve`, `desktop`, or empty).
    fn serves(&self) -> Option<&str>;

    /// Owning process id, or `0` when unknown.
    fn owned_by_pid(&self) -> Option<u32>;
}

impl StatusServeFields for StatusResponse {
    fn serves(&self) -> Option<&str> {
        // TODO R-7a: read `serves` from `StatusResponse` when the field exists.
        let _ = self;
        None
    }

    fn owned_by_pid(&self) -> Option<u32> {
        // TODO R-7a: read `owned_by_pid` from `StatusResponse` when the field exists.
        let _ = self;
        None
    }
}

#[cfg(test)]
mod tests {
    use super::StatusServeFields;
    use hofund_control_client::hofund_control_proto::StatusResponse;

    struct Fake {
        serves: Option<String>,
        owner: Option<u32>,
    }

    impl StatusServeFields for Fake {
        fn serves(&self) -> Option<&str> {
            self.serves.as_deref()
        }

        fn owned_by_pid(&self) -> Option<u32> {
            self.owner
        }
    }

    #[test]
    fn fake_implementor_feeds_handoff_tests() {
        let fake = Fake {
            serves: Some("tui".to_owned()),
            owner: Some(42),
        };
        assert_eq!(fake.serves(), Some("tui"));
        assert_eq!(fake.owned_by_pid(), Some(42));
        let _live = StatusResponse::default();
    }
}
