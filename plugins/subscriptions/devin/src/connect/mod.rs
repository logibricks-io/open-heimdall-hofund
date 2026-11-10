//! Devin Connect message tables. HTTP transport remains outside this module.

use hofund_plugin_sdk::protobuf::ProtoBuf;

pub(crate) mod chat;
pub(crate) mod configs;

pub(crate) const CLI_VERSION: &str = "3000.11.3";

pub(crate) fn metadata(session_token: &str, os: &str) -> Vec<u8> {
    ProtoBuf::new()
        .string(1, "devin-cli")
        .string(2, CLI_VERSION)
        .string(3, session_token)
        .string(4, "en")
        .string(5, os_name(os))
        .string(7, CLI_VERSION)
        .string(12, "chisel")
        .string(28, "chisel")
        .finish()
}

fn os_name(os: &str) -> &str {
    match os {
        "macos" | "darwin" => "darwin",
        "windows" => "windows",
        _ => "linux",
    }
}

#[cfg(test)]
mod tests {
    use hofund_plugin_sdk::protobuf::fields;

    use super::{CLI_VERSION, metadata, os_name};

    #[test]
    fn metadata_uses_the_pinned_cli_fields() {
        let encoded = metadata("session-secret", "macos");
        let parsed = fields(&encoded);
        let values: Vec<(u32, String)> = parsed
            .iter()
            .map(|field| {
                (
                    field.number,
                    field.as_string().expect("metadata string").to_owned(),
                )
            })
            .collect();
        assert_eq!(
            values,
            [
                (1, "devin-cli".into()),
                (2, CLI_VERSION.into()),
                (3, "session-secret".into()),
                (4, "en".into()),
                (5, "darwin".into()),
                (7, CLI_VERSION.into()),
                (12, "chisel".into()),
                (28, "chisel".into()),
            ]
        );
        assert_eq!(os_name("windows"), "windows");
        assert_eq!(os_name("linux"), "linux");
        assert_eq!(os_name("freebsd"), "linux");
    }
}
