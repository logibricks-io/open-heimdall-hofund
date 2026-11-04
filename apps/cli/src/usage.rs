//! `hofund usage` argument parsing.

use hofund_control_client::hofund_control_proto::UsageBreakdownBy;

/// Parsed `hofund usage` invocation.
pub(crate) enum UsageCommand {
    Report { by: Option<UsageBreakdownBy> },
}

const USAGE: &str = "usage: hofund usage [--by agent|provider-key|gateway-key] [--config PATH]";

/// Parses flags after the `usage` verb.
///
/// # Errors
///
/// Returns a usage message when a flag is unknown or malformed.
pub(crate) fn parse(args: &[&str]) -> Result<UsageCommand, String> {
    let mut by = None;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match *arg {
            "--by" => {
                let Some(value) = iter.next() else {
                    return Err(format!(
                        "{USAGE}\n--by needs agent, provider-key, or gateway-key"
                    ));
                };
                by = Some(parse_by(value)?);
            }
            other => {
                return Err(format!("{USAGE}\nunknown flag {other:?}"));
            }
        }
    }
    Ok(UsageCommand::Report { by })
}

fn parse_by(value: &str) -> Result<UsageBreakdownBy, String> {
    match value {
        "agent" => Ok(UsageBreakdownBy::Agent),
        "provider-key" => Ok(UsageBreakdownBy::ProviderKey),
        "gateway-key" => Ok(UsageBreakdownBy::GatewayKey),
        other => Err(format!(
            "{USAGE}\nunknown breakdown {other:?}; expected agent, provider-key, or gateway-key"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::{parse, parse_by};

    #[test]
    fn u1c_13_cli_usage_parses_by_flag() {
        let parsed = parse(&["--by", "provider-key"]).expect("parse");
        match parsed {
            super::UsageCommand::Report { by } => {
                assert_eq!(
                    by,
                    Some(
                        hofund_control_client::hofund_control_proto::UsageBreakdownBy::ProviderKey
                    )
                );
            }
        }
        assert!(parse(&["--by"]).is_err());
        assert!(parse(&["--by", "nope"]).is_err());
        assert!(parse(&["--period", "day"]).is_err());
        assert_eq!(
            parse_by("gateway-key").expect("gateway-key"),
            hofund_control_client::hofund_control_proto::UsageBreakdownBy::GatewayKey
        );
    }
}
