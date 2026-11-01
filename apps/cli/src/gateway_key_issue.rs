//! Parsing and presentation for `gateway-key issue`.

use hofund_control_client::hofund_control_proto::{
    GatewayKeyAction, GatewayKeyRequest, GatewayKeyResponse,
};
use hofund_secrets::{IssuedKey, KeyLimit, expiry_text, parse_ttl};

#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Command {
    pub ttl_seconds: u64,
    pub name: String,
    pub models: Vec<String>,
    pub accounts: Vec<String>,
    pub tokens: u64,
    pub cost: String,
    pub read_only: bool,
    pub from: String,
    pub json: bool,
}

pub(crate) fn parse(args: &[&str], json: bool, read_only: bool) -> Result<Command, String> {
    let mut command = Command {
        json,
        read_only,
        ..Command::default()
    };
    let mut index = 0;
    while index < args.len() {
        let flag = args[index];
        if !matches!(
            flag,
            "--ttl" | "--name" | "--models" | "--accounts" | "--tokens" | "--cost" | "--from"
        ) {
            return Err(crate::gateway_key::HELP.to_owned());
        }
        let Some(value) = args.get(index + 1) else {
            return Err(crate::gateway_key::HELP.to_owned());
        };
        match flag {
            "--ttl" => {
                command.ttl_seconds = parse_ttl(value)
                    .map_err(|_| "--ttl is a duration such as 30m, 1h or 24h".to_owned())?
                    .as_secs();
            }
            "--name" => command.name = (*value).to_owned(),
            "--models" => command.models = list(value),
            "--accounts" => command.accounts = list(value),
            "--tokens" => {
                command.tokens = value
                    .parse()
                    .ok()
                    .filter(|tokens| *tokens > 0)
                    .ok_or_else(|| "--tokens is a whole number above 0".to_owned())?;
            }
            "--cost" => {
                value
                    .parse::<f64>()
                    .ok()
                    .filter(|cost| cost.is_finite() && *cost > 0.0)
                    .ok_or_else(|| "--cost is an amount in USD above 0".to_owned())?;
                command.cost = (*value).to_owned();
            }
            "--from" => command.from = (*value).to_owned(),
            _ => unreachable!(),
        }
        index += 2;
    }
    if command.ttl_seconds == 0 {
        return Err(crate::gateway_key::HELP.to_owned());
    }
    Ok(command)
}

pub(crate) fn request(command: &Command) -> Result<GatewayKeyRequest, String> {
    let mut request = GatewayKeyRequest::new(GatewayKeyAction::Issue).for_id(&command.from);
    request.name = command.name.clone();
    request.ttl_seconds = Some(command.ttl_seconds as f64);
    request.models = command.models.clone();
    request.accounts = command.accounts.clone();
    request.tokens = command.tokens;
    request.cost = if command.cost.is_empty() {
        0.0
    } else {
        command
            .cost
            .parse()
            .map_err(|_| "invalid --cost".to_owned())?
    };
    request.read_only = command.read_only;
    Ok(request)
}

pub(crate) fn json(response: &GatewayKeyResponse, secret: String) -> Result<String, String> {
    let id = response
        .issued_id
        .as_deref()
        .ok_or_else(|| "the daemon did not identify the issued key".to_owned())?;
    let key = response
        .keys
        .iter()
        .find(|key| key.id == id)
        .ok_or_else(|| "the issued key is absent from the daemon response".to_owned())?;
    let issued = IssuedKey {
        id: key.id.clone(),
        name: key.name.clone(),
        key: secret,
        expires: expiry_text(
            key.expires
                .ok_or_else(|| "issued key has no expiry".to_owned())?,
        ),
        issuer: key.issuer.clone(),
        models: key.models.clone(),
        accounts: key.accounts.clone(),
        limit: key.limits.first().map(|limit| KeyLimit {
            period: limit.period.clone(),
            days: limit.days,
            hours: limit.hours,
            tokens: limit.tokens,
            cost: limit.cost,
            cache_reads: limit.cache_reads,
            since: limit.since,
        }),
        read_only: key.read_only,
    };
    serde_json::to_string_pretty(&issued).map_err(|error| error.to_string())
}

fn list(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .collect()
}

#[cfg(test)]
mod tests {
    use hofund_control_client::hofund_control_proto::{GatewayKey, GatewayKeyLimit};

    use super::*;

    #[test]
    fn s9_issue_parses_every_option_and_json_returns_the_one_time_secret() {
        let command = parse(
            &[
                "--ttl",
                "1h30m",
                "--name",
                "Sandbox",
                "--models",
                "primary/a, primary/b",
                "--accounts",
                "primary/a",
                "--tokens",
                "50000",
                "--cost",
                "2.50",
                "--from",
                "parent",
            ],
            true,
            true,
        )
        .expect("parse");
        assert_eq!(command.ttl_seconds, 5_400);
        assert_eq!(command.models, ["primary/a", "primary/b"]);
        assert!(command.read_only && command.json);
        let request = request(&command).expect("request");
        assert_eq!(request.action, GatewayKeyAction::Issue);
        assert_eq!(request.id, "parent");
        assert_eq!(request.tokens, 50_000);
        assert_eq!(request.cost, 2.5);
        assert!(request.read_only);
        for bad in [
            &[][..],
            &["--ttl"][..],
            &["--ttl", "soon"][..],
            &["--ttl", "1h", "--tokens", "-3"][..],
            &["--ttl", "1h", "--what", "x"][..],
        ] {
            assert!(parse(bad, false, false).is_err());
        }

        let mut limit = GatewayKeyLimit::default();
        limit.period = "hours".to_owned();
        limit.hours = 2;
        limit.tokens = 50_000;
        let mut key = GatewayKey::default();
        key.id = "child".to_owned();
        key.name = "Sandbox".to_owned();
        key.expires = Some(1_000_000);
        key.issuer = "parent".to_owned();
        key.read_only = true;
        key.limits = vec![limit];
        let mut response = GatewayKeyResponse::default();
        response.issued_id = Some("child".to_owned());
        response.secret = Some("one-time-secret".to_owned());
        response.keys = vec![key];
        let text = json(&response, "one-time-secret".to_owned()).expect("json");
        let value: serde_json::Value = serde_json::from_str(&text).expect("json value");
        assert_eq!(value["id"], "child");
        assert!(value["key"] == "one-time-secret");
        assert_eq!(value["issuer"], "parent");
        assert_eq!(value["limit"]["hours"], 2);
        assert_eq!(value["readOnly"], true);
        assert!(!format!("{response:?}").contains("one-time-secret"));
    }
}
