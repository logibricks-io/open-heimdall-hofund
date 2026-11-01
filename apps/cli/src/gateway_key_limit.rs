//! Parsing and presentation for gateway-key spending limits.

use hofund_control_client::Client;
use hofund_control_client::hofund_control_proto::{
    GatewayKey, GatewayKeyAction, GatewayKeyLimit, GatewayKeyRequest,
};
use hofund_secrets::{MAX_DAYS, MAX_HOURS};

use crate::commands::failure;

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Command {
    Show {
        id: String,
    },
    Off {
        id: String,
    },
    Reset {
        id: String,
        index: Option<usize>,
    },
    Remove {
        id: String,
        index: usize,
    },
    Set {
        id: String,
        limit: LimitArgs,
        index: Option<usize>,
        add: bool,
    },
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct LimitArgs {
    period: String,
    days: u32,
    hours: u32,
    tokens: Option<u64>,
    cost: Option<String>,
    cache_reads: bool,
}

pub(crate) fn parse(
    words: &[&str],
    secret: &Option<String>,
    json: bool,
) -> Result<Option<Command>, String> {
    let ["gateway-key", action, args @ ..] = words else {
        return Ok(None);
    };
    if !matches!(*action, "limit" | "reset-limit") {
        return Ok(None);
    }
    if secret.is_some() || json {
        return Err(crate::gateway_key::HELP.to_owned());
    }
    let Some((id, rest)) = args.split_first() else {
        return Err(crate::gateway_key::HELP.to_owned());
    };
    let id = (*id).to_owned();
    if *action == "reset-limit" {
        let index = parse_reset_index(rest)?;
        return Ok(Some(Command::Reset { id, index }));
    }
    let command = match rest {
        [] | ["list"] => Command::Show { id },
        ["off"] => Command::Off { id },
        ["reset", rest @ ..] => Command::Reset {
            id,
            index: parse_reset_index(rest)?,
        },
        ["remove", number] => Command::Remove {
            id,
            index: parse_index(number)?,
        },
        ["add", rest @ ..] => parse_set(id, rest, true)?,
        _ => parse_set(id, rest, false)?,
    };
    Ok(Some(command))
}

fn parse_reset_index(rest: &[&str]) -> Result<Option<usize>, String> {
    match rest {
        [] => Ok(None),
        [number] => parse_index(number).map(Some),
        ["--index", number] => parse_index(number).map(Some),
        _ => Err(crate::gateway_key::HELP.to_owned()),
    }
}

fn parse_set(id: String, words: &[&str], add: bool) -> Result<Command, String> {
    let mut limit = LimitArgs::default();
    let mut index = None;
    let mut at = 0;
    if let Some(window) = words.first().filter(|word| !word.starts_with("--")) {
        set_window(&mut limit, window)?;
        at = 1;
    }
    while at < words.len() {
        let (flag, attached) = words[at]
            .split_once('=')
            .map_or((words[at], None), |(flag, value)| (flag, Some(value)));
        at += 1;
        if flag == "--cache-reads" && attached.is_none() {
            limit.cache_reads = true;
            continue;
        }
        let value = match attached {
            Some(value) => value,
            None => {
                let Some(value) = words.get(at) else {
                    return Err(format!("{flag} needs a value"));
                };
                at += 1;
                value
            }
        };
        match flag {
            "--per" if matches!(value, "days" | "hours") => {
                limit.period = value.to_owned();
            }
            "--per" => set_window(&mut limit, value)?,
            "--days" => {
                limit.days = parse_cycle(value, MAX_DAYS, "days")?;
                if limit.period.is_empty() || limit.period == "day" {
                    limit.period = "days".to_owned();
                }
            }
            "--hours" => {
                limit.hours = parse_cycle(value, MAX_HOURS, "hours")?;
                if limit.period.is_empty() || limit.period == "day" {
                    limit.period = "hours".to_owned();
                }
            }
            "--tokens" => limit.tokens = Some(parse_tokens(value)?),
            "--cost" => {
                value
                    .trim_start_matches('$')
                    .parse::<f64>()
                    .map_err(|_| format!("--cost is US dollars, like 5 or 2.50: {value:?}"))?;
                limit.cost = Some(value.to_owned());
            }
            "--index" if !add => index = Some(parse_index(value)?),
            _ => return Err(crate::gateway_key::HELP.to_owned()),
        }
    }
    if limit.period.is_empty() {
        limit.period = "day".to_owned();
    }
    let has_cost = limit.cost.as_deref().is_some_and(|value| {
        value
            .trim_start_matches('$')
            .parse::<f64>()
            .is_ok_and(|cost| cost > 0.0)
    });
    if limit.tokens.is_none() && !has_cost {
        return Err("say --tokens, --cost or both, or off for no limit".to_owned());
    }
    Ok(Command::Set {
        id,
        limit,
        index,
        add,
    })
}

fn set_window(limit: &mut LimitArgs, window: &str) -> Result<(), String> {
    limit.period = window.to_owned();
    if matches!(window, "day" | "week" | "month") {
        return Ok(());
    }
    if let Some(number) = window.strip_suffix('d') {
        limit.days = parse_cycle(number, MAX_DAYS, "days")?;
        limit.period = "days".to_owned();
        return Ok(());
    }
    if let Some(number) = window.strip_suffix('h') {
        limit.hours = parse_cycle(number, MAX_HOURS, "hours")?;
        limit.period = "hours".to_owned();
        return Ok(());
    }
    Err(format!(
        "a limit's period is day, week, month, <N>d (every N days) or <N>h (every N hours), not {window:?}"
    ))
}

fn parse_cycle(value: &str, maximum: u32, unit: &str) -> Result<u32, String> {
    let parsed = value
        .parse::<u32>()
        .ok()
        .filter(|n| (1..=maximum).contains(n));
    parsed.ok_or_else(|| {
        format!("an N-{unit} cycle is a whole number from 1 to {maximum}: {value:?}")
    })
}

fn parse_tokens(value: &str) -> Result<u64, String> {
    let (number, multiplier) = match value.as_bytes().last() {
        Some(b'k' | b'K') => (&value[..value.len() - 1], 1_000),
        Some(b'm' | b'M') => (&value[..value.len() - 1], 1_000_000),
        _ => (value, 1),
    };
    number
        .parse::<u64>()
        .ok()
        .and_then(|number| number.checked_mul(multiplier))
        .filter(|number| *number > 0)
        .ok_or_else(|| format!("--tokens is a count above 0, like 1000000 or 2m: {value:?}"))
}

fn parse_index(value: &str) -> Result<usize, String> {
    value
        .parse::<usize>()
        .ok()
        .filter(|index| *index > 0)
        .map(|index| index - 1)
        .ok_or_else(|| "a limit's number is a whole number from 1".to_owned())
}

pub(crate) async fn run(client: &Client, command: &Command) -> Result<String, String> {
    let (id, request) = match command {
        Command::Show { id } => {
            let response = client.gateway_keys().await.map_err(failure)?;
            let key = find(&response.keys, id)?;
            return Ok(render(key));
        }
        Command::Off { id } => (id, GatewayKeyRequest::new(GatewayKeyAction::Limit)),
        Command::Reset { id, index } => {
            let mut request = GatewayKeyRequest::new(GatewayKeyAction::ResetLimit);
            request.index = *index;
            (id, request)
        }
        Command::Remove { id, index } => {
            let mut request = GatewayKeyRequest::new(GatewayKeyAction::Limit);
            request.index = Some(*index);
            (id, request)
        }
        Command::Set {
            id,
            limit,
            index,
            add,
        } => {
            let response = client.gateway_keys().await.map_err(failure)?;
            let existing = &find(&response.keys, id)?.limits;
            let index = if *add {
                Some(existing.len())
            } else if let Some(index) = index {
                Some(*index)
            } else if existing.len() > 1 {
                existing
                    .iter()
                    .position(|current| same_window(current, limit))
                    .ok_or_else(|| {
                        "the key has several limits and none in that window; use add or --index"
                            .to_owned()
                    })?
                    .into()
            } else {
                None
            };
            let mut request = GatewayKeyRequest::new(GatewayKeyAction::Limit);
            let mut changed = GatewayKeyLimit::default();
            changed.period = limit.period.clone();
            changed.days = limit.days;
            changed.hours = limit.hours;
            changed.tokens = limit.tokens.unwrap_or(0);
            changed.cost = limit
                .cost
                .as_deref()
                .unwrap_or("0")
                .trim_start_matches('$')
                .parse()
                .map_err(|_| "invalid --cost".to_owned())?;
            changed.cache_reads = limit.cache_reads;
            request.limit = Some(changed);
            request.index = index;
            (id, request)
        }
    };
    let request = request.for_id(id);
    let response = client.change_gateway_key(request).await.map_err(failure)?;
    Ok(render(find(&response.keys, id)?))
}

fn same_window(current: &GatewayKeyLimit, next: &LimitArgs) -> bool {
    current.period == next.period && current.days == next.days && current.hours == next.hours
}

fn find<'a>(keys: &'a [GatewayKey], id: &str) -> Result<&'a GatewayKey, String> {
    keys.iter()
        .find(|key| key.id == id)
        .ok_or_else(|| "Key not found".to_owned())
}

pub(crate) fn column(key: &GatewayKey) -> String {
    if key.limits.is_empty() {
        return "-".to_owned();
    }
    key.limits
        .iter()
        .map(|limit| {
            let mut caps = Vec::new();
            if limit.tokens > 0 {
                caps.push(format!("{} tokens", limit.tokens));
            }
            if limit.cost > 0.0 {
                caps.push(format!("${:.2}", limit.cost));
            }
            format!("{} {}", caps.join(" + "), per_phrase(limit))
        })
        .collect::<Vec<_>>()
        .join("; ")
}

fn render(key: &GatewayKey) -> String {
    if key.limits.is_empty() {
        return format!("{}: no limit", key.name);
    }
    let mut out = format!("Key\t{} ({})", key.name, key.id);
    for (index, limit) in key.limits.iter().enumerate() {
        out.push_str(&format!("\nLimit {}\t{}", index + 1, per_phrase(limit)));
        if limit.tokens > 0 {
            out.push_str(&format!("\nTokens\tcap {}", limit.tokens));
        }
        if limit.cost > 0.0 {
            out.push_str(&format!("\nCost\tcap ${:.2}", limit.cost));
        }
        out.push_str(&format!(
            "\nCache reads\t{}",
            if limit.cache_reads {
                "counted"
            } else {
                "not counted"
            }
        ));
    }
    out
}

fn per_phrase(limit: &GatewayKeyLimit) -> String {
    match limit.period.as_str() {
        "days" => format!("every {} days", limit.days),
        "hours" => format!("every {} hours", limit.hours),
        _ => format!("per {}", limit.period),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words<'a>(values: &'a [&'a str]) -> Vec<&'a str> {
        values.to_vec()
    }

    #[test]
    fn l9_cli_limit_and_reset_limit_commands() {
        let month = parse(
            &words(&["gateway-key", "limit", "k", "month", "--cost", "5"]),
            &None,
            false,
        )
        .expect("parse")
        .expect("limit");
        let Command::Set { limit, .. } = month else {
            panic!("expected set")
        };
        assert_eq!(limit.period, "month");
        assert_eq!(limit.cost.as_deref(), Some("5"));

        let flags = parse(
            &words(&[
                "gateway-key",
                "limit",
                "k",
                "--per",
                "hours",
                "--hours",
                "5",
                "--tokens",
                "2m",
                "--cache-reads",
                "--index",
                "2",
            ]),
            &None,
            false,
        )
        .expect("parse")
        .expect("limit");
        let Command::Set {
            limit, index, add, ..
        } = flags
        else {
            panic!("expected set")
        };
        assert_eq!(limit.period, "hours");
        assert_eq!(limit.hours, 5);
        assert_eq!(limit.tokens, Some(2_000_000));
        assert!(limit.cache_reads);
        assert_eq!(index, Some(1));
        assert!(!add);

        assert_eq!(
            parse(
                &words(&["gateway-key", "reset-limit", "k", "2"]),
                &None,
                false,
            ),
            Ok(Some(Command::Reset {
                id: "k".to_owned(),
                index: Some(1),
            }))
        );
        assert!(matches!(
            parse(
                &words(&["gateway-key", "limit", "k", "add", "10d", "--tokens", "20"]),
                &None,
                false,
            ),
            Ok(Some(Command::Set { add: true, .. }))
        ));
        assert!(matches!(
            parse(&words(&["gateway-key", "limit", "k", "off"]), &None, false),
            Ok(Some(Command::Off { .. }))
        ));
        assert!(
            parse(
                &words(&["gateway-key", "limit", "k", "month"]),
                &None,
                false,
            )
            .is_err()
        );
        assert!(
            parse(
                &words(&["gateway-key", "limit", "k", "8761h", "--tokens", "1"]),
                &None,
                false,
            )
            .is_err()
        );
    }

    #[test]
    fn l9_limit_column_and_render_name_every_limit() {
        let mut limit = GatewayKeyLimit::default();
        limit.period = "hours".to_owned();
        limit.hours = 5;
        limit.tokens = 100;
        limit.cost = 2.5;
        let mut key = GatewayKey::default();
        key.name = "Held".to_owned();
        key.id = "k".to_owned();
        key.limits = vec![limit];
        assert!(column(&key).contains("every 5 hours"));
        let rendered = render(&key);
        assert!(rendered.contains("Limit 1"));
        assert!(rendered.contains("cap 100"));
        assert!(rendered.contains("cap $2.50"));
    }
}
