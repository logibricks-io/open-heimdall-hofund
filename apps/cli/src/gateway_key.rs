//! Named gateway-key commands over the control plane.

use std::path::Path;

use hofund_control_client::hofund_control_proto::{
    GatewayKey, GatewayKeyAction, GatewayKeyRequest, GatewayKeyResponse,
};
use hofund_secrets::held_models;

use crate::commands::{client, failure};

pub(crate) const HELP: &str = "\
usage: hofund gateway-key [list] [--json]
       hofund gateway-key add <name> [--key <value> | --key -]
       hofund gateway-key rotate <id>
       hofund gateway-key copy <id>
       hofund gateway-key rename <id> <name>
       hofund gateway-key on|off|remove <id>
       hofund gateway-key move <id> <place>
       hofund gateway-key models <id> [all | <provider>/<model>|<provider>/* ...]
       hofund gateway-key accounts <id> [all | <provider>/<account>|<provider>/<key id> ...]
       hofund gateway-key quotas <id> [on | off]
       hofund gateway-key access <id> [read-only | full]
       hofund gateway-key issuer <id> [on | off]
       hofund gateway-key limit <id> [--tokens N --cost USD --per WINDOW --days N --hours N --cache-reads --index N]
       hofund gateway-key reset-limit <id> [N]

Add and rotate print the new secret; copy prints a key's secret again. The
list shows only a masked form. Secrets are kept readable in caller-keys.json,
which only its owner can read. Model and account scopes take effect on the
next request; the running gateway rereads the store for every call.
";

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Command {
    List {
        json: bool,
    },
    Add {
        name: String,
        secret: Option<String>,
    },
    Rotate {
        id: String,
    },
    Copy {
        id: String,
    },
    Rename {
        id: String,
        name: String,
    },
    On {
        id: String,
    },
    Off {
        id: String,
    },
    Remove {
        id: String,
    },
    Move {
        id: String,
        place: usize,
    },
    Models {
        id: String,
        models: Vec<String>,
    },
    Accounts {
        id: String,
        accounts: Vec<String>,
    },
    Quotas {
        id: String,
        quotas: Option<bool>,
    },
    Access {
        id: String,
        read_only: Option<bool>,
    },
    Issuer {
        id: String,
        issues: Option<bool>,
    },
    Limits(crate::gateway_key_limit::Command),
}

pub(crate) fn parse(
    words: &[&str],
    secret: Option<String>,
    json: bool,
) -> Result<Option<Command>, String> {
    let ["gateway-key", rest @ ..] = words else {
        return Ok(None);
    };
    if let Some(command) = crate::gateway_key_limit::parse(words, &secret, json)? {
        return Ok(Some(Command::Limits(command)));
    }
    let (action, args) = rest.split_first().unwrap_or((&"list", &[][..]));
    let action = *action;
    let one = |_action: &str| -> Result<String, String> {
        if args.len() == 1 && !json && secret.is_none() {
            return Ok(args[0].to_owned());
        }
        Err(HELP.to_owned())
    };
    let command = match (action, args) {
        ("list", []) if secret.is_none() => Command::List { json },
        ("add", [name]) if !json => Command::Add {
            name: (*name).to_owned(),
            secret,
        },
        ("rotate", _) => Command::Rotate { id: one("rotate")? },
        ("copy", _) => Command::Copy { id: one("copy")? },
        ("rename", [id, name]) if !json && secret.is_none() => Command::Rename {
            id: (*id).to_owned(),
            name: (*name).to_owned(),
        },
        ("on", _) => Command::On { id: one("on")? },
        ("off", _) => Command::Off { id: one("off")? },
        ("remove", _) => Command::Remove { id: one("remove")? },
        ("move", [id, place]) if !json && secret.is_none() => Command::Move {
            id: (*id).to_owned(),
            place: place
                .parse()
                .map_err(|_| "a key's place is a whole number from 1".to_owned())?,
        },
        ("models", [id, models @ ..]) if !json && secret.is_none() => Command::Models {
            id: (*id).to_owned(),
            models: scope(models),
        },
        ("accounts", [id, accounts @ ..]) if !json && secret.is_none() => Command::Accounts {
            id: (*id).to_owned(),
            accounts: scope(accounts),
        },
        ("quotas", [id]) if !json && secret.is_none() => Command::Quotas {
            id: (*id).to_owned(),
            quotas: None,
        },
        ("quotas", [id, value]) if !json && secret.is_none() => Command::Quotas {
            id: (*id).to_owned(),
            quotas: Some(on_off(value)?),
        },
        ("access", [id]) if !json && secret.is_none() => Command::Access {
            id: (*id).to_owned(),
            read_only: None,
        },
        ("access", [id, value]) if !json && secret.is_none() => Command::Access {
            id: (*id).to_owned(),
            read_only: Some(match *value {
                "read-only" => true,
                "full" => false,
                _ => return Err(HELP.to_owned()),
            }),
        },
        ("issuer", [id]) if !json && secret.is_none() => Command::Issuer {
            id: (*id).to_owned(),
            issues: None,
        },
        ("issuer", [id, value]) if !json && secret.is_none() => Command::Issuer {
            id: (*id).to_owned(),
            issues: Some(on_off(value)?),
        },
        _ => return Err(HELP.to_owned()),
    };
    Ok(Some(command))
}

fn scope(values: &[&str]) -> Vec<String> {
    if values == ["all"] {
        Vec::new()
    } else {
        values.iter().map(|value| (*value).to_owned()).collect()
    }
}

fn on_off(value: &str) -> Result<bool, String> {
    match value {
        "on" => Ok(true),
        "off" => Ok(false),
        _ => Err(HELP.to_owned()),
    }
}

pub(crate) async fn run(config: &Path, command: &Command) -> Result<String, String> {
    let client = client(config).await?;
    if let Command::Limits(command) = command {
        return crate::gateway_key_limit::run(&client, command).await;
    }
    if let Command::List { json } = command {
        let response = client.gateway_keys().await.map_err(failure)?;
        return if *json {
            serde_json::to_string_pretty(&response.keys).map_err(|error| error.to_string())
        } else {
            Ok(list_table(&response.keys))
        };
    }
    if matches!(
        command,
        Command::Quotas { quotas: None, .. }
            | Command::Access {
                read_only: None,
                ..
            }
            | Command::Issuer { issues: None, .. }
    ) {
        let response = client.gateway_keys().await.map_err(failure)?;
        let id = match command {
            Command::Quotas { id, .. }
            | Command::Access { id, .. }
            | Command::Issuer { id, .. } => id,
            _ => unreachable!(),
        };
        let key = find(&response, id)?;
        return Ok(inspection_words(command, key).expect("inspection command"));
    }
    let (mut request, id, verb) = mutation(command)?;
    if let Command::Add {
        secret: Some(value),
        ..
    } = command
        && value == "-"
    {
        use std::io::Read as _;
        let mut secret = String::new();
        std::io::stdin()
            .take(4096)
            .read_to_string(&mut secret)
            .map_err(|error| error.to_string())?;
        let secret = secret.trim();
        if secret.is_empty() {
            return Err("no key came on stdin".to_owned());
        }
        request.secret = Some(secret.to_owned());
    }
    let response = client.change_gateway_key(request).await.map_err(failure)?;
    if let Some(secret) = response.secret {
        return Ok(secret);
    }
    Ok(match verb {
        Verb::Rotate | Verb::Add | Verb::Copy => unreachable!(),
        Verb::Rename => "Gateway key renamed".to_owned(),
        Verb::On => "Gateway key enabled".to_owned(),
        Verb::Off => "Gateway key disabled".to_owned(),
        Verb::Remove => "Gateway key removed".to_owned(),
        Verb::Move(place) => format!("Gateway key moved to place {place}"),
        Verb::Models => model_words(find(&response, &id)?),
        Verb::Accounts => account_words(find(&response, &id)?),
        Verb::Quotas => {
            let key = find(&response, &id)?;
            if key.quotas {
                format!(
                    "{}: told the quotas of the providers and accounts it may use",
                    key.name
                )
            } else {
                format!(
                    "{}: told no quotas when held to a budget, models or accounts",
                    key.name
                )
            }
        }
        Verb::Access => {
            let key = find(&response, &id)?;
            if key.read_only {
                format!(
                    "{}: read-only, it reads quotas and usage and uses no model or write",
                    key.name
                )
            } else {
                format!(
                    "{}: full access, models and stats as its other limits let it",
                    key.name
                )
            }
        }
        Verb::Issuer => {
            let key = find(&response, &id)?;
            if key.issues {
                format!(
                    "{}: may issue short-lived keys, each held to its own scopes",
                    key.name
                )
            } else {
                format!("{}: issues no keys", key.name)
            }
        }
    })
}

enum Verb {
    Add,
    Rotate,
    Copy,
    Rename,
    On,
    Off,
    Remove,
    Move(usize),
    Models,
    Accounts,
    Quotas,
    Access,
    Issuer,
}

fn mutation(command: &Command) -> Result<(GatewayKeyRequest, String, Verb), String> {
    let (action, id, verb) = match command {
        Command::Add { name, secret } => {
            let mut request = GatewayKeyRequest::new(GatewayKeyAction::Add);
            request.name = name.clone();
            request.secret = secret.clone();
            return Ok((request, String::new(), Verb::Add));
        }
        Command::Rotate { id } => (GatewayKeyAction::Rotate, id, Verb::Rotate),
        Command::Copy { id } => (GatewayKeyAction::Copy, id, Verb::Copy),
        Command::Rename { id, name } => {
            let mut request = GatewayKeyRequest::new(GatewayKeyAction::Rename);
            request.name = name.clone();
            return Ok((request.for_id(id), id.clone(), Verb::Rename));
        }
        Command::On { id } => (GatewayKeyAction::On, id, Verb::On),
        Command::Off { id } => (GatewayKeyAction::Off, id, Verb::Off),
        Command::Remove { id } => (GatewayKeyAction::Remove, id, Verb::Remove),
        Command::Move { id, place } => {
            let mut request = GatewayKeyRequest::new(GatewayKeyAction::Move);
            request.place = Some(*place);
            return Ok((request.for_id(id), id.clone(), Verb::Move(*place)));
        }
        Command::Models { id, models } => {
            let mut request = GatewayKeyRequest::new(GatewayKeyAction::Models);
            request.models = models.clone();
            return Ok((request.for_id(id), id.clone(), Verb::Models));
        }
        Command::Accounts { id, accounts } => {
            let mut request = GatewayKeyRequest::new(GatewayKeyAction::Accounts);
            request.accounts = accounts.clone();
            return Ok((request.for_id(id), id.clone(), Verb::Accounts));
        }
        Command::Quotas { id, quotas } => {
            let mut request = GatewayKeyRequest::new(GatewayKeyAction::Quotas);
            request.quotas = quotas.unwrap_or(false);
            return Ok((request.for_id(id), id.clone(), Verb::Quotas));
        }
        Command::Access { id, read_only } => {
            let mut request = GatewayKeyRequest::new(GatewayKeyAction::Access);
            request.read_only = read_only.unwrap_or(false);
            return Ok((request.for_id(id), id.clone(), Verb::Access));
        }
        Command::Issuer { id, issues } => {
            let mut request = GatewayKeyRequest::new(GatewayKeyAction::Issuer);
            request.issues = issues.unwrap_or(false);
            return Ok((request.for_id(id), id.clone(), Verb::Issuer));
        }
        Command::List { .. } | Command::Limits(_) => {
            return Err("not a gateway-key mutation".to_owned());
        }
    };
    Ok((GatewayKeyRequest::new(action).for_id(id), id.clone(), verb))
}

fn find<'a>(response: &'a GatewayKeyResponse, id: &str) -> Result<&'a GatewayKey, String> {
    response
        .keys
        .iter()
        .find(|key| key.id == id)
        .ok_or_else(|| "Key not found".to_owned())
}

fn inspection_words(command: &Command, key: &GatewayKey) -> Option<String> {
    match command {
        Command::Quotas { quotas: None, .. } => Some(if key.quotas {
            format!(
                "{}: told the quotas of the providers and accounts it may use",
                key.name
            )
        } else {
            format!(
                "{}: told no quotas when held to a budget, models or accounts",
                key.name
            )
        }),
        Command::Access {
            read_only: None, ..
        } => Some(if key.read_only {
            format!(
                "{}: read-only, it reads quotas and usage and uses no model or write",
                key.name
            )
        } else {
            format!(
                "{}: full access, models and stats as its other limits let it",
                key.name
            )
        }),
        Command::Issuer { issues: None, .. } => Some(if key.issues {
            format!(
                "{}: may issue short-lived keys, each held to its own scopes",
                key.name
            )
        } else {
            format!("{}: issues no keys", key.name)
        }),
        _ => None,
    }
}

fn model_words(key: &GatewayKey) -> String {
    let models = held_models(&key.models, &key.accounts);
    if models.is_empty() {
        format!("{}: every model", key.name)
    } else if key.models.is_empty() {
        format!(
            "{}: only its accounts' providers' models, {}",
            key.name,
            models.join(", ")
        )
    } else {
        format!("{}: only {}", key.name, models.join(", "))
    }
}

fn account_words(key: &GatewayKey) -> String {
    if key.accounts.is_empty() {
        format!("{}: every account", key.name)
    } else {
        format!("{}: only {}", key.name, key.accounts.join(", "))
    }
}

fn list_table(keys: &[GatewayKey]) -> String {
    let mut out =
        String::from("ID\tNAME\tSTATE\tGATEWAY KEY\tLIMIT\tMODELS\tACCOUNTS\tACCESS\tUNTIL");
    for key in keys {
        let status = if key.off { "disabled" } else { "enabled" };
        let models = held_models(&key.models, &key.accounts);
        let models = if models.is_empty() {
            "all".to_owned()
        } else {
            models.join(",")
        };
        let accounts = if key.accounts.is_empty() {
            "all".to_owned()
        } else {
            key.accounts.join(",")
        };
        let access = if key.read_only { "read-only" } else { "full" };
        let limit = crate::gateway_key_limit::column(key);
        let until = key
            .expires
            .map_or_else(|| "-".to_owned(), |value| value.to_string());
        let name: String = key
            .name
            .chars()
            .map(|character| {
                if character.is_control() {
                    ' '
                } else {
                    character
                }
            })
            .collect();
        out.push_str(&format!(
            "\n{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
            key.id, name, status, key.masked, limit, models, accounts, access, until
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words<'a>(values: &'a [&'a str]) -> Vec<&'a str> {
        values.to_vec()
    }

    #[test]
    fn k11_cli_parses_actions_and_list_columns() {
        assert_eq!(
            parse(&words(&["gateway-key", "list"]), None, true),
            Ok(Some(Command::List { json: true }))
        );
        assert_eq!(
            parse(
                &words(&["gateway-key", "add", "Work"]),
                Some("own-secret".to_owned()),
                false
            ),
            Ok(Some(Command::Add {
                name: "Work".to_owned(),
                secret: Some("own-secret".to_owned())
            }))
        );
        for (args, expected) in [
            (
                words(&["gateway-key", "rotate", "k"]),
                Command::Rotate { id: "k".to_owned() },
            ),
            (
                words(&["gateway-key", "copy", "k"]),
                Command::Copy { id: "k".to_owned() },
            ),
            (
                words(&["gateway-key", "rename", "k", "New"]),
                Command::Rename {
                    id: "k".to_owned(),
                    name: "New".to_owned(),
                },
            ),
            (
                words(&["gateway-key", "on", "k"]),
                Command::On { id: "k".to_owned() },
            ),
            (
                words(&["gateway-key", "off", "k"]),
                Command::Off { id: "k".to_owned() },
            ),
            (
                words(&["gateway-key", "remove", "k"]),
                Command::Remove { id: "k".to_owned() },
            ),
            (
                words(&["gateway-key", "move", "k", "1"]),
                Command::Move {
                    id: "k".to_owned(),
                    place: 1,
                },
            ),
            (
                words(&["gateway-key", "models", "k", "primary/*"]),
                Command::Models {
                    id: "k".to_owned(),
                    models: vec!["primary/*".to_owned()],
                },
            ),
            (
                words(&["gateway-key", "accounts", "k", "all"]),
                Command::Accounts {
                    id: "k".to_owned(),
                    accounts: Vec::new(),
                },
            ),
            (
                words(&["gateway-key", "quotas", "k", "on"]),
                Command::Quotas {
                    id: "k".to_owned(),
                    quotas: Some(true),
                },
            ),
            (
                words(&["gateway-key", "access", "k", "read-only"]),
                Command::Access {
                    id: "k".to_owned(),
                    read_only: Some(true),
                },
            ),
            (
                words(&["gateway-key", "issuer", "k", "off"]),
                Command::Issuer {
                    id: "k".to_owned(),
                    issues: Some(false),
                },
            ),
        ] {
            assert_eq!(parse(&args, None, false), Ok(Some(expected)));
        }
        let mut key = GatewayKey::default();
        key.id = "k".to_owned();
        key.name = "Held".to_owned();
        key.accounts = vec!["primary/a".to_owned()];
        key.masked = "hofund_key_…abcdef".to_owned();
        let table = list_table(&[key]);
        assert!(
            table.starts_with(
                "ID\tNAME\tSTATE\tGATEWAY KEY\tLIMIT\tMODELS\tACCOUNTS\tACCESS\tUNTIL"
            )
        );
        assert!(table.contains("\thofund_key_…abcdef\t"));
        assert!(parse(&words(&["gateway-key", "copy"]), None, false).is_err());
        assert!(table.contains("primary/*"));
        assert!(table.contains("full"));
        assert!(!table.contains("read-only"));
    }
}
