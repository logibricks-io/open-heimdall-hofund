//! `gateway-key oidc` commands (local rules file).

use std::path::Path;

use hofund_secrets::KeyStore;
use hofund_secrets::oidc::{RuleDraft, RuleStore};

const USAGE: &str = "usage: hofund gateway-key oidc [list] | add --key <id> --repo owner/name [--owner <owner>] [--ref refs/heads/main] [--environment <name>] [--claim name=value]… [--ttl 1h] [--issuer URL] [--audience <aud>] | remove <rule id>";

pub(crate) fn parse(args: &[&str]) -> Result<Option<Command>, String> {
    if args.first().copied() != Some("oidc") {
        return Ok(None);
    }
    let args = &args[1..];
    if args.is_empty() || args[0] == "list" {
        if args.len() > 1 {
            return Err(USAGE.to_owned());
        }
        return Ok(Some(Command::List));
    }
    match args[0] {
        "remove" => {
            if args.len() != 2 {
                return Err(USAGE.to_owned());
            }
            Ok(Some(Command::Remove {
                id: args[1].to_owned(),
            }))
        }
        "add" => parse_add(&args[1..]),
        _ => Err(USAGE.to_owned()),
    }
}

fn parse_add(args: &[&str]) -> Result<Option<Command>, String> {
    let mut draft = RuleDraft::default();
    let mut index = 0;
    while index + 1 < args.len() {
        let flag = args[index];
        let value = args[index + 1];
        if flag.starts_with("--") {
            match flag {
                "--key" => draft.key = value.to_owned(),
                "--repo" => {
                    draft.claims.insert("repository".to_owned(), value.to_owned());
                }
                "--owner" => {
                    draft
                        .claims
                        .insert("repository_owner".to_owned(), value.to_owned());
                }
                "--ref" => {
                    draft.claims.insert("ref".to_owned(), value.to_owned());
                }
                "--environment" => {
                    draft.claims.insert("environment".to_owned(), value.to_owned());
                }
                "--claim" => {
                    let (name, val) = value.split_once('=').ok_or_else(|| USAGE.to_owned())?;
                    draft.claims.insert(name.to_owned(), val.to_owned());
                }
                "--ttl" => {
                    let duration = hofund_secrets::parse_ttl(value)
                        .map_err(|_| format!("--ttl is a duration such as 1h or 30m: {value:?}"))?;
                    draft.ttl = i64::try_from(duration.as_secs()).unwrap_or(-1);
                    if draft.ttl == 0 {
                        draft.ttl = -1;
                    }
                }
                "--issuer" => draft.issuer = value.to_owned(),
                "--audience" => draft.audience = value.to_owned(),
                _ => return Err(USAGE.to_owned()),
            }
            index += 2;
        } else {
            return Err(USAGE.to_owned());
        }
    }
    if draft.key.is_empty() {
        return Err(USAGE.to_owned());
    }
    Ok(Some(Command::Add(draft)))
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Command {
    List,
    Add(RuleDraft),
    Remove { id: String },
}

pub(crate) fn run(config: &Path, command: &Command) -> Result<String, String> {
    let loaded = hofund_config::load_path(config)
        .map_err(|error| format!("{}: {error}", config.display()))?;
    let token_path = crate::local::beside(config, &loaded.token_file);
    let keys = KeyStore::beside_token(&token_path);
    keys.list().map_err(|error| error.to_string())?;
    let rules = RuleStore::beside_token(&token_path);
    match command {
        Command::List => list(&rules),
        Command::Add(draft) => add(&rules, &keys, draft.clone()),
        Command::Remove { id } => {
            rules.remove(id).map_err(|error| error.to_string())?;
            Ok("OIDC rule removed".to_owned())
        }
    }
}

fn list(rules: &RuleStore) -> Result<String, String> {
    let listed = rules.list().map_err(|error| error.to_string())?;
    if listed.is_empty() {
        return Ok(
            "No OIDC rules: hofund gateway-key oidc add --key <id> --repo owner/name".to_owned(),
        );
    }
    let mut lines = vec!["ID\tKEY\tTTL\tAUDIENCE\tCLAIMS\tISSUER".to_owned()];
    for rule in listed {
        let claims = rule
            .claims
            .iter()
            .map(|(name, value)| format!("{name}={value}"))
            .collect::<Vec<_>>()
            .join(" ");
        lines.push(format!(
            "{}\t{}\t{:?}\t{}\t{}\t{}",
            rule.id,
            rule.key,
            rule.lifetime(),
            rule.audience,
            claims,
            rule.issuer
        ));
    }
    Ok(lines.join("\n"))
}

fn add(rules: &RuleStore, keys: &KeyStore, draft: RuleDraft) -> Result<String, String> {
    let rule = rules.add(draft, keys).map_err(|error| error.to_string())?;
    Ok(format!(
        "OIDC rule {}: a token from {} with audience {} and {} claim(s) matched gets a key issued under {}, living up to {:?}.\nPOST it to /v1/hofund/keys/oidc as Authorization: Bearer <token>.\n",
        rule.id,
        rule.issuer,
        rule.audience,
        rule.claims.len(),
        rule.key,
        rule.lifetime()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn d28_cli_parse_oidc_add_and_list() {
        let command = parse(&["oidc", "list"]).expect("parse").expect("command");
        assert_eq!(command, Command::List);
        let command = parse(&[
            "oidc",
            "add",
            "--key",
            "abc",
            "--repo",
            "acme/app",
            "--ttl",
            "30m",
        ])
        .expect("parse")
        .expect("command");
        let Command::Add(draft) = command else {
            panic!("expected add");
        };
        assert_eq!(draft.key, "abc");
        assert_eq!(draft.claims.get("repository").map(String::as_str), Some("acme/app"));
    }
}
