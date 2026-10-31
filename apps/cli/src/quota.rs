//! `hofund quota`: what is left of each subscription, its kept window
//! history, and waiting until an account has allowance again.
//!
//! Every renderer takes the clock as an argument so tests pin the time and
//! the zone; the commands pass `Local::now()`.

use std::fmt::Display;
use std::path::Path;
use std::process::ExitCode;
use std::time::Duration;

use chrono::{DateTime, Local, TimeDelta, TimeZone, Utc};
use hofund_control_client::hofund_control_proto::{
    Quota, QuotaHistory, QuotaHistoryQuery, QuotaKind, QuotaPoint, QuotaQuery, QuotaSpan,
    QuotaStanding, QuotaTime,
};

use crate::commands::{client, failure};

/// The shortest wait between two reads of `quota wait`.
const WAIT_MIN: Duration = Duration::from_secs(60);
/// The longest wait between two reads of `quota wait`.
const WAIT_MAX: Duration = Duration::from_secs(600);
/// How long after the soonest reset `quota wait` reads again.
const WAIT_MARGIN: Duration = Duration::from_secs(30);
/// How many readings of each window `quota history` prints.
const HISTORY_POINTS: usize = 8;

const WAIT_USAGE: &str = "hofund quota wait <provider|account> [--timeout <duration>] [--quiet]";

const LIST_FOOTER: &str = "  % is how much of a window is used · ↻ when it starts again · \
     --json for scripts, or GET /v1/hofund/quotas on the gateway";

const HISTORY_FOOTER: &str = "  % is how much was left · ↻ a new run of the window · \
     --json for every reading, or GET /v1/hofund/quotas/history on the gateway";

/// Help for `hofund quota`.
pub(crate) const QUOTA_HELP: &str = "\
usage: hofund quota [<provider|name|kind|alias>...] [--json] [--config PATH]
       hofund quota wait <provider|account> [--timeout <duration>] [--quiet]
       hofund quota history [<provider|account>...] [--days <n>] [--json]

`quota` prints what is left of every subscription hofund reads, asked of the
vendors now: one line per card with each window's use, when it starts again,
any balance or error, and how old a kept reading is. Names narrow the list by
provider, name, kind (subscription, plan, balance), or alias. --json prints
the cards for scripts, each with lastServedAt when it last answered through
the gateway and last: true on the latest. The gateway answers the same at
GET /v1/hofund/quotas with the bearer token.

`quota wait codex` returns once any account of the subscription has
allowance again (no window that stops it is used up). An account, or
<provider>/<account>, waits for that one alone. It reads again shortly after
the soonest reset, every 1 to 10 minutes, and says on stderr what it waits
for; --quiet says nothing. For a script that stopped its work:
  until codex exec ...; do hofund quota wait codex || break; done
Exit 0 once there is allowance, 1 when --timeout (30m, 6h) passes first, 2
for a name it does not know, 130 on Ctrl+C.

`quota history` is what hofund has read of each window over time, kept 45
days, a reading only when it changed: the latest readings of each window, or
with --json every one since --days days ago, as
GET /v1/hofund/quotas/history?days=&provider=&user= answers. A provider, an
account, or <provider>/<account> narrows it.
";

/// A parsed `hofund quota` command line.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum QuotaCommand {
    /// `quota [filters] [--json]`.
    List { filters: Vec<String>, json: bool },
    /// `quota history [filters] [--days N] [--json]`.
    History {
        filters: Vec<String>,
        days: Option<String>,
        json: bool,
    },
    /// `quota wait <target> [--timeout D] [--quiet]`.
    Wait {
        target: String,
        timeout: Option<Duration>,
        quiet: bool,
    },
    /// A command line that fails with this exit code and message.
    Invalid { code: u8, message: String },
    /// `help` anywhere in the words.
    Help,
}

/// Parses the words after `quota`. Errors carry their exit codes: 2 for a
/// `quota wait` usage error, 1 for the others.
pub(crate) fn parse(args: &[&str]) -> QuotaCommand {
    match args {
        ["wait", rest @ ..] => parse_wait(rest),
        ["history", rest @ ..] => parse_history(rest),
        [
            verb @ ("reset" | "auto-reset" | "credits" | "alert" | "refresh"),
            ..,
        ] => QuotaCommand::Invalid {
            code: 2,
            message: format!("hofund quota {verb} is not supported yet"),
        },
        rest => parse_list(rest),
    }
}

fn parse_list(args: &[&str]) -> QuotaCommand {
    let mut json = false;
    let mut filters = Vec::new();
    for arg in args {
        match *arg {
            "--json" | "-j" => json = true,
            "help" | "-h" | "--help" => return QuotaCommand::Help,
            other => filters.push(other.to_lowercase()),
        }
    }
    QuotaCommand::List { filters, json }
}

fn parse_history(args: &[&str]) -> QuotaCommand {
    let mut json = false;
    let mut days = None;
    let mut filters = Vec::new();
    let mut index = 0;
    while index < args.len() {
        let arg = args[index];
        match arg {
            "--json" | "-j" => json = true,
            "--days" if index + 1 < args.len() => {
                index += 1;
                days = Some(args[index].to_owned());
            }
            "help" | "-h" | "--help" => return QuotaCommand::Help,
            _ if arg.starts_with("--days=") => {
                days = Some(arg["--days=".len()..].to_owned());
            }
            _ if arg.starts_with('-') => {
                return QuotaCommand::Invalid {
                    code: 1,
                    message: format!("unknown flag {arg}\n{QUOTA_HELP}"),
                };
            }
            _ => filters.push(arg.to_lowercase()),
        }
        index += 1;
    }
    if let Some(value) = &days
        && !value.parse::<f64>().is_ok_and(|days| days > 0.0)
    {
        return QuotaCommand::Invalid {
            code: 1,
            message: format!("--days takes a number of days, such as 1 or 7, not {value:?}"),
        };
    }
    QuotaCommand::History {
        filters,
        days,
        json,
    }
}

fn parse_wait(args: &[&str]) -> QuotaCommand {
    let usage = |message: String| QuotaCommand::Invalid { code: 2, message };
    let mut target: Option<&str> = None;
    let mut timeout = None;
    let mut quiet = false;
    let mut index = 0;
    while index < args.len() {
        let arg = args[index];
        if matches!(arg, "help" | "-h" | "--help") {
            return QuotaCommand::Help;
        } else if matches!(arg, "--quiet" | "-q") {
            quiet = true;
        } else if arg == "--timeout" || arg.starts_with("--timeout=") {
            let value = if let Some(value) = arg.strip_prefix("--timeout=") {
                value
            } else {
                index += 1;
                match args.get(index) {
                    Some(value) => value,
                    None => {
                        return usage("--timeout needs a duration, such as 30m or 6h".to_owned());
                    }
                }
            };
            match parse_go_duration(value) {
                Some(duration) if !duration.is_zero() => timeout = Some(duration),
                _ => {
                    return usage(format!(
                        "--timeout is a duration such as 30m or 6h, not {value:?}"
                    ));
                }
            }
        } else if arg.starts_with('-') {
            return usage(format!("unknown flag {arg} · {WAIT_USAGE}"));
        } else if let Some(first) = target {
            return usage(format!(
                "one subscription or account at a time: {first} or {arg}"
            ));
        } else {
            target = Some(arg);
        }
        index += 1;
    }
    match target {
        Some(target) => QuotaCommand::Wait {
            target: target.to_owned(),
            timeout,
            quiet,
        },
        None => {
            usage("wait for what? hofund quota wait <provider|account>, such as codex".to_owned())
        }
    }
}

/// Runs a parsed `hofund quota` command and leaves with its exit code.
pub(crate) fn run(config: &Path, command: QuotaCommand) -> ExitCode {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("cannot start the async runtime: {error}");
            return ExitCode::FAILURE;
        }
    };
    let printed = |result: Result<String, String>| match result {
        Ok(out) => {
            if !out.is_empty() {
                println!("{out}");
            }
            ExitCode::SUCCESS
        }
        Err(message) => {
            eprintln!("{message}");
            ExitCode::FAILURE
        }
    };
    match command {
        QuotaCommand::Help => {
            println!("{QUOTA_HELP}");
            ExitCode::SUCCESS
        }
        QuotaCommand::Invalid { code, message } => {
            eprintln!("{message}");
            ExitCode::from(code)
        }
        QuotaCommand::List { filters, json } => printed(runtime.block_on(async {
            let client = client(config).await?;
            let response = client.quotas(QuotaQuery::fresh()).await.map_err(failure)?;
            render_list(
                response.data,
                &filters,
                json,
                &known_ids(config),
                &Local::now(),
            )
        })),
        QuotaCommand::History {
            filters,
            days,
            json,
        } => printed(runtime.block_on(async {
            let client = client(config).await?;
            let query = match days {
                Some(days) => QuotaHistoryQuery::default().with_days(days),
                None => QuotaHistoryQuery::default(),
            };
            let response = client.quota_history(query).await.map_err(failure)?;
            render_history(response.data, &filters, json, &Local::now())
        })),
        QuotaCommand::Wait {
            target,
            timeout,
            quiet,
        } => ExitCode::from(runtime.block_on(wait(config, &target, timeout, quiet))),
    }
}

/// The provider and subscription ids the config names, for telling a name
/// that has no card apart from one that is nothing at all.
fn known_ids(config: &Path) -> Vec<String> {
    hofund_config::load_path(config)
        .map(|loaded| {
            loaded
                .providers
                .iter()
                .map(|provider| provider.id.clone())
                .chain(loaded.subscriptions.iter().map(|sub| sub.id.clone()))
                .collect()
        })
        .unwrap_or_default()
}

fn is_known(known: &[String], name: &str) -> Option<String> {
    known
        .iter()
        .find(|id| id.eq_ignore_ascii_case(name))
        .cloned()
}

fn kind_label(kind: QuotaKind) -> &'static str {
    match kind {
        QuotaKind::Subscription => "subscription",
        QuotaKind::Plan => "plan",
        QuotaKind::Balance => "balance",
        QuotaKind::Unknown => "unknown",
    }
}

/// Whether a card matches any filter: its provider, name, kind, or alias,
/// without case.
fn matches_any(card: &Quota, filters: &[String]) -> bool {
    filters.iter().any(|filter| {
        card.provider.eq_ignore_ascii_case(filter)
            || card.name.eq_ignore_ascii_case(filter)
            || kind_label(card.kind).eq_ignore_ascii_case(filter)
            || (!card.alias.is_empty() && card.alias.eq_ignore_ascii_case(filter))
    })
}

/// Adds a card for each configured provider named that told nothing, and
/// fails on a name that is no provider at all.
fn with_untold(
    mut cards: Vec<Quota>,
    filters: &[String],
    known: &[String],
) -> Result<Vec<Quota>, String> {
    for filter in filters {
        if matches!(filter.as_str(), "subscription" | "plan" | "balance")
            || cards
                .iter()
                .any(|card| matches_any(card, std::slice::from_ref(filter)))
        {
            continue;
        }
        let Some(id) = is_known(known, filter) else {
            return Err(format!(
                "no provider {filter:?} · hofund provider list lists them"
            ));
        };
        cards.push(Quota {
            provider: id.clone(),
            name: id,
            kind: QuotaKind::Balance,
            error: "not configured · no balance endpoint known for it".to_owned(),
            ..Quota::default()
        });
    }
    Ok(cards)
}

/// `hofund quota`'s output: JSON, the table, or the empty-list note.
pub(crate) fn render_list<Tz: TimeZone>(
    cards: Vec<Quota>,
    filters: &[String],
    json: bool,
    known: &[String],
    now: &DateTime<Tz>,
) -> Result<String, String>
where
    Tz::Offset: Display,
{
    let kept: Vec<Quota> = cards
        .into_iter()
        .filter(|card| filters.is_empty() || matches_any(card, filters))
        .collect();
    let kept = with_untold(kept, filters, known)?;
    if json {
        return serde_json::to_string_pretty(&kept).map_err(|error| error.to_string());
    }
    if kept.is_empty() {
        if !filters.is_empty() {
            return Err(format!(
                "no subscription, plan or key balance of {}",
                filters.join(", ")
            ));
        }
        return Ok(
            "nothing to tell · a signed-in subscription, a coding plan or a key \
                   whose vendor tells its balance shows up here"
                .to_owned(),
        );
    }
    Ok(table(&kept, now))
}

fn table<Tz: TimeZone>(cards: &[Quota], now: &DateTime<Tz>) -> String
where
    Tz::Offset: Display,
{
    let titles: Vec<String> = cards
        .iter()
        .map(|card| title(card, &now.timezone()))
        .collect();
    let width = titles
        .iter()
        .map(|title| title.chars().count())
        .max()
        .unwrap_or(0);
    let mut out = String::new();
    for (card, title) in cards.iter().zip(&titles) {
        let mut line = format!("{title:<width$}  {:<12}", kind_label(card.kind));
        for window in &card.windows {
            line.push_str("  ");
            line.push_str(&window_cell(window, now));
        }
        if !card.balance.is_empty() {
            line.push_str(&format!("  {} left", card.balance));
        }
        if !card.error.is_empty() {
            line.push_str("  ");
            line.push_str(&card.error);
        }
        line.push_str(&reading_cell(
            card.as_of.as_ref(),
            card.read_at.as_ref(),
            &card.windows,
            now,
        ));
        out.push_str(&line);
        out.push('\n');
    }
    out.push_str(LIST_FOOTER);
    out
}

/// A card's title: provider, plan, plan term, and the alias and account.
fn title<Tz: TimeZone>(card: &Quota, zone: &Tz) -> String
where
    Tz::Offset: Display,
{
    let mut title = card.provider.clone();
    if !card.plan.is_empty() {
        title.push_str(" · ");
        title.push_str(&card.plan);
    }
    let term = plan_term(card.until.as_ref().and_then(instant), &card.renew, zone);
    if !term.is_empty() {
        title.push_str(" · ");
        title.push_str(&term);
    }
    match (card.alias.is_empty(), card.user.is_empty()) {
        (false, false) => title.push_str(&format!(" · {} ({})", card.alias, card.user)),
        (false, true) => title.push_str(&format!(" · {}", card.alias)),
        (true, false) => title.push_str(&format!(" · {}", card.user)),
        (true, true) => {}
    }
    title
}

/// Parses a wire timestamp; an unreadable one counts as absent.
fn instant(time: &QuotaTime) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(time.as_str())
        .ok()
        .map(|time| time.with_timezone(&Utc))
}

/// When a window starts again, on the clock: `14:30` today or earlier,
/// `tomorrow 09:00`, `Wed 14:30` within the week, else `Oct 3 14:30`.
pub(crate) fn reset_clock<Tz: TimeZone>(at: DateTime<Utc>, now: &DateTime<Tz>) -> String
where
    Tz::Offset: Display,
{
    let at = at.with_timezone(&now.timezone());
    let days = (at.date_naive() - now.date_naive()).num_days();
    match days {
        ..=0 => at.format("%H:%M").to_string(),
        1 => format!("tomorrow {}", at.format("%H:%M")),
        2..=6 => at.format("%a %H:%M").to_string(),
        _ => at.format("%b %-d %H:%M").to_string(),
    }
}

/// When a plan's paid time ends: `renews Oct 18`, `expires Oct 18`, or
/// `until Oct 18` when the vendor does not say which; empty with no end.
pub(crate) fn plan_term<Tz: TimeZone>(
    until: Option<DateTime<Utc>>,
    renew: &str,
    zone: &Tz,
) -> String
where
    Tz::Offset: Display,
{
    let Some(until) = until else {
        return String::new();
    };
    let day = until.with_timezone(zone).format("%b %-d").to_string();
    match renew {
        "auto" => format!("renews {day}"),
        "off" => format!("expires {day}"),
        _ => format!("until {day}"),
    }
}

/// A time left as `45m`, `2h13m`, or `3d4h`; `now` when none is.
pub(crate) fn until_short(left: TimeDelta) -> String {
    if left <= TimeDelta::zero() {
        return "now".to_owned();
    }
    let minutes = left.num_minutes();
    let hours = left.num_hours();
    if hours < 1 {
        format!("{minutes}m")
    } else if hours < 48 {
        format!("{hours}h{}m", minutes % 60)
    } else {
        format!("{}d{}h", hours / 24, hours % 24)
    }
}

/// A wallet amount as told: `6,291 points left`.
pub(crate) fn balance_text(balance: f64, unit: &str) -> String {
    let rounded = (balance * 100.0).round() / 100.0;
    let text = format!("{}", rounded.abs());
    let (whole, fraction) = text.split_once('.').unwrap_or((&text, ""));
    let mut grouped = String::new();
    for (index, digit) in whole.chars().enumerate() {
        if index > 0 && (whole.len() - index) % 3 == 0 {
            grouped.push(',');
        }
        grouped.push(digit);
    }
    if !fraction.is_empty() {
        grouped.push('.');
        grouped.push_str(fraction);
    }
    if rounded < 0.0 {
        grouped.insert(0, '-');
    }
    format!("{} left", format!("{grouped} {unit}").trim())
}

/// Shortens window names the way the table prints them: `5 hours` is `5h`.
fn short_name(name: &str) -> String {
    const SWAPS: [(&str, &str); 5] = [
        (" hours", "h"),
        (" hour", "h"),
        (" days", "d"),
        (" day", "d"),
        (" · ", " "),
    ];
    let mut out = String::new();
    let mut rest = name;
    'scan: while !rest.is_empty() {
        for (from, to) in SWAPS {
            if let Some(after) = rest.strip_prefix(from) {
                out.push_str(to);
                rest = after;
                continue 'scan;
            }
        }
        let mut chars = rest.chars();
        if let Some(next) = chars.next() {
            out.push(next);
        }
        rest = chars.as_str();
    }
    out
}

/// One window in a table line: `5h 42% ↻2h13m 14:13`.
pub(crate) fn window_cell<Tz: TimeZone>(window: &QuotaSpan, now: &DateTime<Tz>) -> String
where
    Tz::Offset: Display,
{
    let name = short_name(&window.name);
    if window.unlimited {
        return format!("{name} unlimited");
    }
    let mut cell = if let Some(balance) = window.balance {
        format!("{name} {}", balance_text(balance, &window.unit))
    } else if window.text {
        format!("{name} {}", window.display)
    } else if !window.display.is_empty() {
        format!("{name} {:.0}% ({})", window.used, window.display)
    } else {
        format!("{name} {:.0}%", window.used)
    };
    if let Some(at) = window.resets_at.as_ref().and_then(instant) {
        let now_utc = now.with_timezone(&Utc);
        if at <= now_utc {
            cell.push_str(&format!(
                " · reset time passed {}",
                at.with_timezone(&now.timezone()).format("%b %-d %H:%M")
            ));
        } else {
            cell.push_str(&format!(
                " ↻{} {}",
                until_short(at - now_utc),
                reset_clock(at, now)
            ));
        }
    }
    cell
}

/// How long ago, in a few words: `just now`, `12m ago`, `5h ago`, `Oct 3`.
fn ago<Tz: TimeZone>(at: DateTime<Utc>, now: &DateTime<Tz>) -> String
where
    Tz::Offset: Display,
{
    let since = now.with_timezone(&Utc) - at;
    if since < TimeDelta::minutes(1) {
        "just now".to_owned()
    } else if since < TimeDelta::hours(1) {
        format!("{}m ago", since.num_minutes())
    } else if since < TimeDelta::hours(48) {
        format!("{}h ago", since.num_hours())
    } else {
        at.with_timezone(&now.timezone())
            .format("%b %-d")
            .to_string()
    }
}

/// When a card was read: `as of … (cached)` for a kept reading standing in
/// for a failed one, `read 12m ago` for a reading handed back from the
/// cache, nothing for one just read.
fn reading_cell<Tz: TimeZone>(
    as_of: Option<&QuotaTime>,
    read_at: Option<&QuotaTime>,
    windows: &[QuotaSpan],
    now: &DateTime<Tz>,
) -> String
where
    Tz::Offset: Display,
{
    let now_utc = now.with_timezone(&Utc);
    let Some(as_of) = as_of.and_then(instant) else {
        return match read_at.and_then(instant) {
            Some(read) if now_utc - read >= TimeDelta::minutes(2) => {
                format!("  read {}", ago(read, now))
            }
            _ => String::new(),
        };
    };
    let mut cell = format!(
        "  as of {} (cached)",
        as_of.with_timezone(&now.timezone()).format("%b %-d %H:%M")
    );
    if windows.iter().any(|window| {
        window
            .resets_at
            .as_ref()
            .and_then(instant)
            .is_some_and(|at| at <= now_utc)
    }) {
        cell.push_str(" · expired window; current allowance unknown");
    }
    cell
}

/// `hofund quota history`'s output: JSON, each account's lines, or the
/// empty-history note.
pub(crate) fn render_history<Tz: TimeZone>(
    histories: Vec<QuotaHistory>,
    filters: &[String],
    json: bool,
    now: &DateTime<Tz>,
) -> Result<String, String>
where
    Tz::Offset: Display,
{
    let kept: Vec<QuotaHistory> = histories
        .into_iter()
        .filter(|history| {
            filters.is_empty()
                || filters.iter().any(|filter| {
                    *filter == history.provider
                        || *filter == history.user
                        || *filter == format!("{}/{}", history.provider, history.user)
                })
        })
        .collect();
    if json {
        return serde_json::to_string_pretty(&kept).map_err(|error| error.to_string());
    }
    if kept.is_empty() {
        if !filters.is_empty() {
            return Err(format!("no readings kept of {}", filters.join(", ")));
        }
        return Ok(
            "nothing kept yet · a subscription's or plan's windows are kept here \
                   as hofund reads them"
                .to_owned(),
        );
    }
    let zone = now.timezone();
    let mut out = String::new();
    for history in &kept {
        out.push_str(&history.provider);
        if !history.user.is_empty() {
            out.push_str(" · ");
            out.push_str(&history.user);
        }
        out.push('\n');
        for line in &history.lines {
            out.push_str(&format!(
                "  {}  {}\n",
                line.name,
                points_cell(&line.points, HISTORY_POINTS, &zone)
            ));
        }
    }
    out.push_str(HISTORY_FOOTER);
    Ok(out)
}

/// A window's last `count` readings, oldest first:
/// `Oct 4 09:12 80% · 10:40 61% · ↻ 14:02 100%`, the day only where it changes.
pub(crate) fn points_cell<Tz: TimeZone>(points: &[QuotaPoint], count: usize, zone: &Tz) -> String
where
    Tz::Offset: Display,
{
    let points = &points[points.len().saturating_sub(count)..];
    let mut day = String::new();
    let mut cells = Vec::new();
    for (index, point) in points.iter().enumerate() {
        let Some(at) = instant(&point.at) else {
            continue;
        };
        let at = at.with_timezone(zone);
        let mut cell = String::new();
        if index > 0 && new_run(&points[index - 1], point) {
            cell.push_str("↻ ");
        }
        let this_day = at.format("%b %-d").to_string();
        if this_day != day {
            cell.push_str(&format!("{this_day} "));
            day = this_day;
        }
        cell.push_str(&format!("{} {}%", at.format("%H:%M"), point.left));
        cells.push(cell);
    }
    cells.join(" · ")
}

/// Whether `later` is in a new run of its window: it resets more than ten
/// minutes away from `earlier`'s reset, or, without both resets, it has
/// more than five points more left.
pub(crate) fn new_run(earlier: &QuotaPoint, later: &QuotaPoint) -> bool {
    match (
        earlier.resets_at.as_ref().and_then(instant),
        later.resets_at.as_ref().and_then(instant),
    ) {
        (Some(before), Some(after)) => (after - before).abs() > TimeDelta::minutes(10),
        _ => later.left > earlier.left + 5.0,
    }
}

/// What `quota wait` waits on: every account of a provider (empty user) or
/// one account.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Target {
    provider: String,
    user: String,
    label: String,
}

impl Target {
    fn has(&self, standing: &QuotaStanding) -> bool {
        standing.provider.eq_ignore_ascii_case(&self.provider)
            && (self.user.is_empty() || standing.user.eq_ignore_ascii_case(&self.user))
    }
}

/// Finds what `arg` names among the standings: a provider by id or name,
/// `<provider>/<account>`, or one account by itself. `known` says whether
/// `arg` is a configured provider, for an error that tells the two apart.
pub(crate) fn resolve(
    arg: &str,
    standings: &[QuotaStanding],
    known: &dyn Fn(&str) -> bool,
) -> Result<Target, String> {
    let by_provider = |name: &str| {
        standings
            .iter()
            .find(|standing| {
                standing.provider.eq_ignore_ascii_case(name)
                    || standing.name.eq_ignore_ascii_case(name)
            })
            .map(|standing| standing.provider.clone())
    };
    if let Some(id) = by_provider(arg) {
        return Ok(Target {
            label: id.clone(),
            provider: id,
            user: String::new(),
        });
    }
    if let Some((provider, user)) = arg.split_once('/')
        && let Some(id) = by_provider(provider)
    {
        let target = Target {
            label: format!("{id} · {user}"),
            provider: id.clone(),
            user: user.to_owned(),
        };
        if standings.iter().any(|standing| target.has(standing)) {
            return Ok(target);
        }
        return Err(format!(
            "{id} has no account {user} · hofund quota {id} lists its accounts"
        ));
    }
    let mut hits: Vec<Target> = Vec::new();
    for standing in standings {
        if !standing.user.is_empty()
            && standing.user.eq_ignore_ascii_case(arg)
            && !hits.iter().any(|hit| hit.provider == standing.provider)
        {
            hits.push(Target {
                provider: standing.provider.clone(),
                user: standing.user.clone(),
                label: format!("{} · {}", standing.provider, standing.user),
            });
        }
    }
    match hits.len() {
        1 => return Ok(hits.remove(0)),
        0 => {}
        _ => {
            let names: Vec<String> = hits
                .iter()
                .map(|hit| format!("{}/{}", hit.provider, hit.user))
                .collect();
            return Err(format!(
                "{arg} is an account of more than one subscription: name one of {}",
                names.join(", ")
            ));
        }
    }
    if known(arg) {
        return Err(format!(
            "{arg} has no subscription or plan whose allowance hofund reads: sign in to it \
             first, or hofund quota lists what there is"
        ));
    }
    Err(format!(
        "no subscription, plan or account {arg} · hofund quota lists them"
    ))
}

/// What `quota wait` does after one read.
#[derive(Debug, PartialEq)]
pub(crate) enum Step {
    /// This account has allowance again.
    Ready(QuotaStanding),
    /// Say `line`, then read again after `wait`.
    Sleep { wait: Duration, line: String },
    /// The timeout passed.
    TimedOut,
}

/// The state `quota wait` keeps between reads: its deadline and how long it
/// backs off while nothing can be read.
#[derive(Debug)]
pub(crate) struct Waiter {
    deadline: Option<DateTime<Utc>>,
    backoff: Duration,
}

impl Waiter {
    /// A wait that started at `now`, ending after `timeout` when one is set.
    pub(crate) fn new(now: DateTime<Utc>, timeout: Option<Duration>) -> Self {
        Self {
            deadline: timeout
                .and_then(|timeout| TimeDelta::from_std(timeout).ok())
                .map(|timeout| now + timeout),
            backoff: WAIT_MIN,
        }
    }

    /// Decides what follows one read: `standings` as read, or `read_error`
    /// when the read itself failed.
    pub(crate) fn step<Tz: TimeZone>(
        &mut self,
        target: &Target,
        standings: &[QuotaStanding],
        read_error: Option<&str>,
        now: &DateTime<Tz>,
    ) -> Step
    where
        Tz::Offset: Display,
    {
        let now_utc = now.with_timezone(&Utc);
        let mut known = 0_usize;
        let mut unread = 0_usize;
        let mut why = read_error.unwrap_or_default().to_owned();
        let mut back: Option<(DateTime<Utc>, &str)> = None;
        for standing in standings.iter().filter(|standing| target.has(standing)) {
            if !standing.readable {
                unread += 1;
                if why.is_empty() {
                    why.clone_from(&standing.error);
                }
                continue;
            }
            if !standing.used_up {
                return Step::Ready(standing.clone());
            }
            known += 1;
            if let Some(at) = standing.back_at.as_ref().and_then(instant)
                && back.is_none_or(|(soonest, _)| at < soonest)
            {
                back = Some((at, &standing.user));
            }
        }
        let (mut wait, mut line) = if known == 0 {
            let wait = self.backoff;
            self.backoff = (self.backoff * 2).min(WAIT_MAX);
            let mut line = format!("{}: its allowance couldn't be read", target.label);
            if !why.is_empty() {
                line.push_str(&format!(" ({why})"));
            }
            line.push_str(&format!(" · trying again in {}", round_wait(wait)));
            (wait, line)
        } else if let Some((at, back_who)) = back {
            self.backoff = WAIT_MIN;
            let until = (at - now_utc)
                .to_std()
                .unwrap_or_default()
                .saturating_add(WAIT_MARGIN);
            let wait = until.clamp(WAIT_MIN, WAIT_MAX);
            let mut who = target.label.clone();
            if target.user.is_empty() && !back_who.is_empty() && known > 1 {
                who.push_str(&format!(" ({back_who} first)"));
            } else if target.user.is_empty() && known > 1 {
                who.push_str(" (every account)");
            }
            let again = now.clone() + TimeDelta::from_std(wait).unwrap_or_default();
            let line = format!(
                "{who} is used up until {} · checking again at {}",
                reset_clock(at, now),
                again.format("%H:%M")
            );
            (wait, line)
        } else {
            self.backoff = WAIT_MIN;
            let line = format!(
                "{} is used up, and when it starts again isn't told · checking again in {}",
                target.label,
                round_wait(WAIT_MAX)
            );
            (WAIT_MAX, line)
        };
        if known > 0 && unread > 0 {
            line.push_str(&format!(" · {unread} not read"));
        }
        if let Some(deadline) = self.deadline {
            if now_utc >= deadline {
                return Step::TimedOut;
            }
            wait = wait.min((deadline - now_utc).to_std().unwrap_or_default());
        }
        Step::Sleep { wait, line }
    }
}

/// `hofund quota wait`: reads until the target has allowance and returns the
/// exit code.
async fn wait(config: &Path, arg: &str, timeout: Option<Duration>, quiet: bool) -> u8 {
    let interrupted = tokio::signal::ctrl_c();
    tokio::pin!(interrupted);
    let read = || async {
        let client = client(config).await?;
        client
            .quotas(QuotaQuery::fresh())
            .await
            .map(|response| response.standings)
            .map_err(failure)
    };
    let first = tokio::select! {
        reading = read() => reading,
        _ = &mut interrupted => return 130,
    };
    let standings = match first {
        Ok(standings) => standings,
        Err(message) => {
            eprintln!("{message}");
            return 1;
        }
    };
    let known = known_ids(config);
    let target = match resolve(arg, &standings, &|name| is_known(&known, name).is_some()) {
        Ok(target) => target,
        Err(message) => {
            eprintln!("{message}");
            return 2;
        }
    };
    let mut waiter = Waiter::new(Utc::now(), timeout);
    let mut reading: Result<Vec<QuotaStanding>, String> = Ok(standings);
    loop {
        let (standings, why) = match &reading {
            Ok(standings) => (standings.as_slice(), None),
            Err(message) => (&[][..], Some(message.as_str())),
        };
        match waiter.step(&target, standings, why, &Local::now()) {
            Step::Ready(standing) => {
                if !quiet {
                    println!("✓ {} has allowance again", who(&standing));
                }
                return 0;
            }
            Step::TimedOut => {
                eprintln!(
                    "{} still has no allowance after {}",
                    target.label,
                    go_duration(timeout.unwrap_or_default())
                );
                return 1;
            }
            Step::Sleep { wait, line } => {
                if !quiet {
                    eprintln!("… {line}");
                }
                tokio::select! {
                    () = tokio::time::sleep(wait) => {}
                    _ = &mut interrupted => return 130,
                }
            }
        }
        reading = tokio::select! {
            reading = read() => reading,
            _ = &mut interrupted => return 130,
        };
    }
}

fn who(standing: &QuotaStanding) -> String {
    if standing.user.is_empty() {
        standing.provider.clone()
    } else {
        format!("{} · {}", standing.provider, standing.user)
    }
}

/// A wait in words, to the minute: two minutes read `2m`.
pub(crate) fn round_wait(wait: Duration) -> String {
    let minutes = (wait.as_nanos() + 30_000_000_000) / 60_000_000_000;
    let text = if minutes == 0 {
        "0s".to_owned()
    } else if minutes >= 60 {
        format!("{}h{}m0s", minutes / 60, minutes % 60)
    } else {
        format!("{minutes}m0s")
    };
    let text = text.strip_suffix("0s").unwrap_or(&text);
    text.strip_suffix("h0m").unwrap_or(text).to_owned()
}

/// A duration the way `--timeout` was written back: `30m0s`, `6h0m0s`,
/// `1m30s`, `500ms`.
pub(crate) fn go_duration(duration: Duration) -> String {
    const SECOND: u128 = 1_000_000_000;
    let nanos = duration.as_nanos();
    if nanos == 0 {
        return "0s".to_owned();
    }
    if nanos < SECOND {
        return if nanos < 1_000 {
            format!("{nanos}ns")
        } else if nanos < 1_000_000 {
            format!("{}µs", decimal(nanos, 1_000))
        } else {
            format!("{}ms", decimal(nanos, 1_000_000))
        };
    }
    let seconds = nanos / SECOND;
    let (hours, minutes) = (seconds / 3600, seconds / 60 % 60);
    let rest = decimal(seconds % 60 * SECOND + nanos % SECOND, SECOND);
    if hours > 0 {
        format!("{hours}h{minutes}m{rest}s")
    } else if minutes > 0 {
        format!("{minutes}m{rest}s")
    } else {
        format!("{rest}s")
    }
}

fn decimal(value: u128, scale: u128) -> String {
    let (whole, fraction) = (value / scale, value % scale);
    if fraction == 0 {
        return whole.to_string();
    }
    let digits = scale.ilog10() as usize;
    let fraction = format!("{fraction:0digits$}");
    format!("{whole}.{}", fraction.trim_end_matches('0'))
}

/// Parses a duration such as `30m`, `6h`, `1h30m`, or `1.5h`: decimal
/// numbers each with a unit of ns, us, µs, ms, s, m, or h. A sign other
/// than `+` is refused, since a wait is never negative.
pub(crate) fn parse_go_duration(text: &str) -> Option<Duration> {
    let mut rest = text.strip_prefix('+').unwrap_or(text);
    if rest == "0" {
        return Some(Duration::ZERO);
    }
    if rest.is_empty() {
        return None;
    }
    let mut total: u128 = 0;
    while !rest.is_empty() {
        let number_end = rest
            .find(|c: char| !c.is_ascii_digit() && c != '.')
            .unwrap_or(rest.len());
        let (number, after) = rest.split_at(number_end);
        let (whole, fraction) = number.split_once('.').unwrap_or((number, ""));
        if whole.is_empty() && fraction.is_empty() {
            return None;
        }
        if fraction.contains('.') {
            return None;
        }
        let unit_end = after
            .find(|c: char| c.is_ascii_digit() || c == '.')
            .unwrap_or(after.len());
        let (unit, next) = after.split_at(unit_end);
        let scale: u128 = match unit {
            "ns" => 1,
            "us" | "µs" | "μs" => 1_000,
            "ms" => 1_000_000,
            "s" => 1_000_000_000,
            "m" => 60_000_000_000,
            "h" => 3_600_000_000_000,
            _ => return None,
        };
        let whole: u128 = if whole.is_empty() {
            0
        } else {
            whole.parse().ok()?
        };
        let digits = &fraction[..fraction.len().min(18)];
        let part = if digits.is_empty() {
            0
        } else {
            let value: u128 = digits.parse().ok()?;
            value * scale / 10_u128.pow(u32::try_from(digits.len()).ok()?)
        };
        total = total.checked_add(whole.checked_mul(scale)?.checked_add(part)?)?;
        rest = next;
    }
    let nanos = i64::try_from(total).ok()?;
    Some(Duration::from_nanos(nanos.unsigned_abs()))
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use chrono::{DateTime, FixedOffset, TimeDelta, Utc};
    use hofund_control_client::hofund_control_proto::{
        Quota, QuotaHistory, QuotaKind, QuotaLine, QuotaPoint, QuotaSpan, QuotaStanding, QuotaTime,
    };

    use super::{
        QuotaCommand, Step, Target, Waiter, go_duration, new_run, parse, parse_go_duration,
        plan_term, points_cell, reading_cell, render_history, render_list, reset_clock, resolve,
        round_wait, until_short, window_cell,
    };

    fn zone() -> FixedOffset {
        FixedOffset::east_opt(8 * 3600).expect("offset")
    }

    /// Sunday 2026-11-01 10:00 at +08:00.
    fn now() -> DateTime<FixedOffset> {
        DateTime::parse_from_rfc3339("2026-11-01T10:00:00+08:00").expect("now")
    }

    fn utc(text: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(text)
            .expect("time")
            .with_timezone(&Utc)
    }

    fn later(delta: TimeDelta) -> QuotaTime {
        QuotaTime::new((now() + delta).to_rfc3339())
    }

    fn span(name: &str, used: f64) -> QuotaSpan {
        QuotaSpan {
            name: name.to_owned(),
            used,
            remaining: (100.0 - used).max(0.0),
            ..QuotaSpan::default()
        }
    }

    fn card(provider: &str, kind: QuotaKind) -> Quota {
        Quota {
            provider: provider.to_owned(),
            name: provider.to_owned(),
            kind,
            ..Quota::default()
        }
    }

    fn standing(provider: &str, user: &str) -> QuotaStanding {
        let mut standing = QuotaStanding::new(provider, user);
        standing.name = format!("{provider} name");
        standing.readable = true;
        standing
    }

    fn used_up(provider: &str, user: &str, back: Option<TimeDelta>) -> QuotaStanding {
        let mut standing = standing(provider, user);
        standing.used_up = true;
        standing.back_at = back.map(later);
        standing
    }

    fn filters(words: &[&str]) -> Vec<String> {
        match parse(words) {
            QuotaCommand::List { filters, .. } => filters,
            other => panic!("not a list: {other:?}"),
        }
    }

    fn sleep(step: Step) -> (Duration, String) {
        match step {
            Step::Sleep { wait, line } => (wait, line),
            other => panic!("expected a sleep, got {other:?}"),
        }
    }

    #[test]
    fn q2b_reset_clock_uses_calendar_day_buckets() {
        let now = now();
        let cases = [
            ("2026-11-01T14:30:00+08:00", "14:30"),
            ("2026-11-01T08:00:00+08:00", "08:00"),
            ("2026-10-30T23:10:00+08:00", "23:10"),
            ("2026-11-02T09:00:00+08:00", "tomorrow 09:00"),
            // 17:00 UTC is already the next day at +08:00
            ("2026-11-01T17:00:00Z", "tomorrow 01:00"),
            ("2026-11-03T14:30:00+08:00", "Tue 14:30"),
            ("2026-11-07T14:30:00+08:00", "Sat 14:30"),
            ("2026-11-08T14:30:00+08:00", "Nov 8 14:30"),
            ("2027-01-02T07:05:00+08:00", "Jan 2 07:05"),
        ];
        for (at, want) in cases {
            assert_eq!(reset_clock(utc(at), &now), want, "{at}");
        }
    }

    #[test]
    fn q2c_plan_term_covers_renewal_modes() {
        let until = Some(utc("2026-11-17T20:00:00Z"));
        assert_eq!(plan_term(until, "auto", &zone()), "renews Nov 18");
        assert_eq!(plan_term(until, "off", &zone()), "expires Nov 18");
        assert_eq!(plan_term(until, "", &zone()), "until Nov 18");
        assert_eq!(plan_term(until, "sometimes", &zone()), "until Nov 18");
        assert_eq!(plan_term(None, "auto", &zone()), "");
    }

    #[test]
    fn q7a_quota_json_and_filters() {
        let mut codex = card("codex", QuotaKind::Subscription);
        codex.name = "OpenAI Codex".to_owned();
        codex.user = "a@x.io".to_owned();
        codex.alias = "work".to_owned();
        codex.last = true;
        codex.last_served_at = Some(QuotaTime::new("2026-11-01T01:00:00Z"));
        let mut window = span("weekly", 40.0);
        window.runs_out_at = Some(QuotaTime::new("2026-11-03T00:00:00Z"));
        window.runs_out = 12.5;
        codex.windows = vec![window];
        let kimi = card("kimi", QuotaKind::Plan);
        let deepseek = card("deepseek", QuotaKind::Balance);
        let cards = vec![codex, kimi, deepseek];
        let known: Vec<String> = Vec::new();

        let all = render_list(cards.clone(), &[], true, &known, &now()).expect("json");
        assert!(
            all.starts_with("[\n  {\n    \"provider\": \"codex\""),
            "{all}"
        );
        let parsed: serde_json::Value = serde_json::from_str(&all).expect("parse");
        assert_eq!(parsed.as_array().map(Vec::len), Some(3));
        assert_eq!(parsed[0]["lastServedAt"], "2026-11-01T01:00:00Z");
        assert_eq!(parsed[0]["last"], true);
        assert_eq!(parsed[0]["windows"][0]["runsOutAt"], "2026-11-03T00:00:00Z");
        assert_eq!(parsed[0]["windows"][0]["runsOut"], 12.5);
        assert_eq!(parsed[1]["kind"], "plan");
        assert!(parsed[1].get("last").is_none());

        let pick = |words: &[&str]| {
            let out =
                render_list(cards.clone(), &filters(words), true, &known, &now()).expect("json");
            let parsed: serde_json::Value = serde_json::from_str(&out).expect("parse");
            parsed
                .as_array()
                .expect("array")
                .iter()
                .map(|card| card["provider"].as_str().unwrap_or_default().to_owned())
                .collect::<Vec<_>>()
        };
        assert_eq!(pick(&["CODEX", "--json"]), ["codex"], "provider, any case");
        assert_eq!(pick(&["openai codex", "-j"]), ["codex"], "display name");
        assert_eq!(pick(&["Plan", "--json"]), ["kimi"], "kind");
        assert_eq!(pick(&["WORK", "--json"]), ["codex"], "alias");
        assert_eq!(pick(&["balance", "kimi", "--json"]), ["kimi", "deepseek"]);
        assert_eq!(
            parse(&["--json"]),
            QuotaCommand::List {
                filters: Vec::new(),
                json: true
            }
        );
        let empty = render_list(Vec::new(), &[], true, &known, &now()).expect("json");
        assert_eq!(empty, "[]", "an empty report is an empty array for scripts");
    }

    #[test]
    fn q7m_reading_cell_names_a_handed_back_reading_after_two_minutes() {
        let read = later(TimeDelta::minutes(-3));
        assert_eq!(
            reading_cell(None, Some(&read), &[], &now()),
            "  read 3m ago"
        );
        let read = later(TimeDelta::minutes(-2));
        assert_eq!(
            reading_cell(None, Some(&read), &[], &now()),
            "  read 2m ago"
        );
        let fresh = later(TimeDelta::seconds(-90));
        assert_eq!(reading_cell(None, Some(&fresh), &[], &now()), "");
        assert_eq!(reading_cell(None, None, &[], &now()), "");
    }

    #[test]
    fn q7b_quota_table_matches_reference_cells() {
        let mut codex = card("codex", QuotaKind::Subscription);
        codex.plan = "plus".to_owned();
        codex.user = "a@x.io".to_owned();
        codex.alias = "work".to_owned();
        codex.until = Some(QuotaTime::new("2026-11-17T20:00:00Z"));
        codex.renew = "auto".to_owned();
        let mut five = span("5 hours", 42.4);
        five.resets_at = Some(later(TimeDelta::minutes(133)));
        let mut weekly = span("weekly · all models", 10.5);
        weekly.display = "10/100".to_owned();
        weekly.runs_out_at = Some(later(TimeDelta::days(1)));
        weekly.runs_out = 3.0;
        codex.windows = vec![five, weekly];

        let mut kimi = card("kimi", QuotaKind::Balance);
        kimi.balance = "12.5 CNY".to_owned();
        kimi.read_at = Some(later(TimeDelta::minutes(-15)));

        let mut claude = card("claude", QuotaKind::Subscription);
        claude.user = "b@y.io".to_owned();
        claude.error = "token expired".to_owned();
        claude.as_of = Some(QuotaTime::new("2026-10-31T01:00:00Z"));
        let mut day = span("1 day", 100.0);
        day.resets_at = Some(later(TimeDelta::hours(-1)));
        claude.windows = vec![day];

        let mut glm = card("glm", QuotaKind::Plan);
        glm.alias = "team".to_owned();
        let mut unlimited = span("monthly", 0.0);
        unlimited.unlimited = true;
        let mut text = span("prompts", 0.0);
        text.text = true;
        text.display = "120 left".to_owned();
        let mut wallet = span("points", 0.0);
        wallet.balance = Some(6291.0);
        wallet.unit = "points".to_owned();
        glm.windows = vec![unlimited, text, wallet];
        glm.read_at = Some(later(TimeDelta::seconds(-30)));

        let out =
            render_list(vec![codex, kimi, claude, glm], &[], false, &[], &now()).expect("table");
        let want = [
            "codex · plus · renews Nov 18 · work (a@x.io)  subscription  \
             5h 42% ↻2h13m 12:13  weekly all models 10% (10/100)",
            "kimi                                          balance       12.5 CNY left  \
             read 15m ago",
            "claude · b@y.io                               subscription  \
             1d 100% · reset time passed Nov 1 09:00  token expired  \
             as of Oct 31 09:00 (cached) · expired window; current allowance unknown",
            "glm · team                                    plan          \
             monthly unlimited  prompts 120 left  points 6,291 points left",
            "  % is how much of a window is used · ↻ when it starts again · \
             --json for scripts, or GET /v1/hofund/quotas on the gateway",
        ]
        .join("\n");
        assert_eq!(out, want);
        assert!(!out.contains("runs out"), "no runs-out column is invented");
    }

    #[test]
    fn q7c_quota_empty_unknown_and_expired_output() {
        let known = vec!["codex".to_owned(), "OpenRouter".to_owned()];
        assert_eq!(
            render_list(Vec::new(), &[], false, &known, &now()),
            Ok(
                "nothing to tell · a signed-in subscription, a coding plan or a key \
                whose vendor tells its balance shows up here"
                    .to_owned()
            )
        );
        assert_eq!(
            render_list(Vec::new(), &filters(&["Nope"]), false, &known, &now()),
            Err("no provider \"nope\" · hofund provider list lists them".to_owned())
        );
        assert_eq!(
            render_list(Vec::new(), &filters(&["plan"]), false, &known, &now()),
            Err("no subscription, plan or key balance of plan".to_owned())
        );
        let untold = render_list(Vec::new(), &filters(&["openrouter"]), true, &known, &now())
            .expect("untold");
        let parsed: serde_json::Value = serde_json::from_str(&untold).expect("parse");
        assert_eq!(parsed[0]["provider"], "OpenRouter");
        assert_eq!(parsed[0]["kind"], "balance");
        assert_eq!(parsed[0]["windows"], serde_json::json!([]));
        assert_eq!(
            parsed[0]["error"],
            "not configured · no balance endpoint known for it"
        );
        let mut expired = span("5 hours", 100.0);
        expired.resets_at = Some(later(TimeDelta::zero()));
        assert_eq!(
            window_cell(&expired, &now()),
            "5h 100% · reset time passed Nov 1 10:00",
            "a reset at now is passed, never a fresh window"
        );
        expired.resets_at = Some(later(TimeDelta::seconds(59)));
        assert_eq!(window_cell(&expired, &now()), "5h 100% ↻0m 10:00");
        expired.resets_at = Some(later(TimeDelta::hours(75)));
        assert_eq!(window_cell(&expired, &now()), "5h 100% ↻3d3h Wed 13:00");
        assert_eq!(until_short(TimeDelta::minutes(-1)), "now");
        assert_eq!(until_short(TimeDelta::minutes(59)), "59m");
        assert_eq!(until_short(TimeDelta::minutes(60)), "1h0m");
        assert_eq!(
            until_short(TimeDelta::hours(47) + TimeDelta::minutes(59)),
            "47h59m"
        );
        assert_eq!(until_short(TimeDelta::hours(48)), "2d0h");
        expired.resets_at = Some(QuotaTime::new("not a time"));
        assert_eq!(window_cell(&expired, &now()), "5h 100%");
    }

    #[test]
    fn q7d_wait_resolves_targets_and_available_allowance() {
        let standings = vec![
            standing("codex", "a@x.io"),
            standing("codex", "b@y.io"),
            standing("claude", "A@X.IO"),
        ];
        let known = |name: &str| name.eq_ignore_ascii_case("openai");
        let target = |arg: &str| resolve(arg, &standings, &known);
        let whole = Target {
            provider: "codex".to_owned(),
            user: String::new(),
            label: "codex".to_owned(),
        };
        assert_eq!(target("codex"), Ok(whole.clone()));
        assert_eq!(target("CODEX"), Ok(whole.clone()));
        assert_eq!(target("codex name"), Ok(whole.clone()), "by display name");
        assert_eq!(
            target("codex/A@x.io"),
            Ok(Target {
                provider: "codex".to_owned(),
                user: "A@x.io".to_owned(),
                label: "codex · A@x.io".to_owned(),
            })
        );
        assert_eq!(
            target("codex/zed"),
            Err("codex has no account zed · hofund quota codex lists its accounts".to_owned())
        );
        assert_eq!(
            target("b@y.io"),
            Ok(Target {
                provider: "codex".to_owned(),
                user: "b@y.io".to_owned(),
                label: "codex · b@y.io".to_owned(),
            })
        );
        assert_eq!(
            target("a@x.io"),
            Err(
                "a@x.io is an account of more than one subscription: name one of \
                 codex/a@x.io, claude/A@X.IO"
                    .to_owned()
            )
        );
        assert_eq!(
            target("OpenAI"),
            Err(
                "OpenAI has no subscription or plan whose allowance hofund reads: sign in \
                 to it first, or hofund quota lists what there is"
                    .to_owned()
            )
        );
        assert_eq!(
            target("nope"),
            Err("no subscription, plan or account nope · hofund quota lists them".to_owned())
        );

        let reading = vec![
            used_up("codex", "a@x.io", Some(TimeDelta::hours(1))),
            standing("codex", "b@y.io"),
        ];
        let mut waiter = Waiter::new(now().with_timezone(&Utc), None);
        assert_eq!(
            waiter.step(&whole, &reading, None, &now()),
            Step::Ready(standing("codex", "b@y.io")),
            "any account with allowance ends a provider's wait"
        );
        let one = resolve("codex/a@x.io", &reading, &known).expect("one");
        let (wait, line) = sleep(waiter.step(&one, &reading, None, &now()));
        assert_eq!(wait, Duration::from_secs(600));
        assert_eq!(
            line,
            "codex · a@x.io is used up until 11:00 · checking again at 10:10"
        );
        assert!(matches!(
            parse(&["wait", "codex", "claude"]),
            QuotaCommand::Invalid { code: 2, .. }
        ));
        assert_eq!(
            parse(&["wait"]),
            QuotaCommand::Invalid {
                code: 2,
                message: "wait for what? hofund quota wait <provider|account>, such as codex"
                    .to_owned()
            }
        );
    }

    #[test]
    fn q7e_wait_schedule_backoff_and_timeout() {
        let target = Target {
            provider: "codex".to_owned(),
            user: String::new(),
            label: "codex".to_owned(),
        };
        let start = now();
        let mut waiter = Waiter::new(start.with_timezone(&Utc), None);
        let soon = [used_up("codex", "a", Some(TimeDelta::minutes(5)))];
        assert_eq!(
            sleep(waiter.step(&target, &soon, None, &start)),
            (
                Duration::from_secs(330),
                "codex is used up until 10:05 · checking again at 10:05".to_owned()
            ),
            "shortly after the reset, with a 30s margin"
        );
        let imminent = [used_up("codex", "a", Some(TimeDelta::seconds(10)))];
        assert_eq!(
            sleep(waiter.step(&target, &imminent, None, &start)).0,
            Duration::from_secs(60)
        );
        let past = [used_up("codex", "a", Some(TimeDelta::minutes(-5)))];
        assert_eq!(
            sleep(waiter.step(&target, &past, None, &start)).0,
            Duration::from_secs(60)
        );
        let far = [used_up("codex", "a", Some(TimeDelta::days(2)))];
        assert_eq!(
            sleep(waiter.step(&target, &far, None, &start)),
            (
                Duration::from_secs(600),
                "codex is used up until Tue 10:00 · checking again at 10:10".to_owned()
            )
        );
        let untold = [used_up("codex", "a", None)];
        assert_eq!(
            sleep(waiter.step(&target, &untold, None, &start)),
            (
                Duration::from_secs(600),
                "codex is used up, and when it starts again isn't told · checking again in 10m"
                    .to_owned()
            )
        );

        let mut failing = standing("codex", "a");
        failing.readable = false;
        failing.error = "boom".to_owned();
        let failed = [failing];
        let waits: Vec<Duration> = (0..6)
            .map(|_| sleep(waiter.step(&target, &failed, None, &start)).0)
            .collect();
        assert_eq!(
            waits,
            [1, 2, 4, 8, 10, 10].map(|minutes| Duration::from_secs(minutes * 60))
        );
        assert_eq!(
            sleep(waiter.step(&target, &soon, None, &start)).0,
            Duration::from_secs(330)
        );
        assert_eq!(
            sleep(waiter.step(&target, &failed, None, &start)),
            (
                Duration::from_secs(60),
                "codex: its allowance couldn't be read (boom) · trying again in 1m".to_owned()
            ),
            "a readable look starts the backoff over"
        );

        let two = [
            used_up("codex", "late", Some(TimeDelta::hours(2))),
            used_up("codex", "early", Some(TimeDelta::minutes(5))),
            failed[0].clone(),
        ];
        assert_eq!(
            sleep(waiter.step(&target, &two, None, &start)).1,
            "codex (early first) is used up until 10:05 · checking again at 10:05 · 1 not read"
        );
        let providerwide = [
            used_up("codex", "", Some(TimeDelta::minutes(5))),
            used_up("codex", "", None),
        ];
        assert_eq!(
            sleep(waiter.step(&target, &providerwide, None, &start)).1,
            "codex (every account) is used up until 10:05 · checking again at 10:05"
        );

        let mut timed = Waiter::new(start.with_timezone(&Utc), Some(Duration::from_secs(180)));
        assert_eq!(
            sleep(timed.step(&target, &soon, None, &start)).0,
            Duration::from_secs(180),
            "the deadline caps the wait"
        );
        let at_deadline = start + TimeDelta::minutes(3);
        assert_eq!(
            timed.step(&target, &soon, None, &at_deadline),
            Step::TimedOut,
            "a final read at the deadline still decides"
        );
        assert_eq!(
            timed.step(&target, &[standing("codex", "a")], None, &at_deadline),
            Step::Ready(standing("codex", "a")),
            "allowance found by the final read wins over the deadline"
        );
    }

    #[test]
    fn q7f_wait_readability_blocking_quiet_and_exit_codes() {
        let target = Target {
            provider: "codex".to_owned(),
            user: String::new(),
            label: "codex".to_owned(),
        };
        let mut waiter = Waiter::new(now().with_timezone(&Utc), None);
        let mut cached = standing("codex", "a");
        cached.readable = false;
        assert_eq!(
            sleep(waiter.step(&target, &[cached], None, &now())).1,
            "codex: its allowance couldn't be read · trying again in 1m",
            "a cached or windowless reading proves nothing"
        );
        assert_eq!(
            sleep(waiter.step(&target, &[], Some("no daemon"), &now())).1,
            "codex: its allowance couldn't be read (no daemon) · trying again in 2m",
            "a failed read is retried"
        );
        let other = [standing("claude", "a")];
        assert!(
            matches!(
                waiter.step(&target, &other, None, &now()),
                Step::Sleep { .. }
            ),
            "another provider's allowance does not count"
        );
        assert_eq!(
            parse(&["wait", "codex", "-q", "--timeout", "1h30m"]),
            QuotaCommand::Wait {
                target: "codex".to_owned(),
                timeout: Some(Duration::from_secs(5400)),
                quiet: true
            }
        );
        assert_eq!(
            parse(&["wait", "--quiet", "--timeout=90s", "codex"]),
            QuotaCommand::Wait {
                target: "codex".to_owned(),
                timeout: Some(Duration::from_secs(90)),
                quiet: true
            }
        );
        for (words, message) in [
            (
                vec!["wait", "codex", "--timeout"],
                "--timeout needs a duration, such as 30m or 6h",
            ),
            (
                vec!["wait", "codex", "--timeout", "soon"],
                "--timeout is a duration such as 30m or 6h, not \"soon\"",
            ),
            (
                vec!["wait", "codex", "--timeout=0s"],
                "--timeout is a duration such as 30m or 6h, not \"0s\"",
            ),
            (
                vec!["wait", "codex", "--timeout=-5m"],
                "--timeout is a duration such as 30m or 6h, not \"-5m\"",
            ),
            (
                vec!["wait", "codex", "--loud"],
                "unknown flag --loud · hofund quota wait <provider|account> \
                 [--timeout <duration>] [--quiet]",
            ),
        ] {
            assert_eq!(
                parse(&words),
                QuotaCommand::Invalid {
                    code: 2,
                    message: message.to_owned()
                },
                "{words:?}"
            );
        }
        assert_eq!(parse(&["wait", "codex", "help"]), QuotaCommand::Help);
        assert_eq!(go_duration(Duration::from_secs(1800)), "30m0s");
        assert_eq!(go_duration(Duration::from_secs(6 * 3600)), "6h0m0s");
        assert_eq!(super::who(&standing("codex", "a@x.io")), "codex · a@x.io");
        assert_eq!(super::who(&standing("codex", "")), "codex");
    }

    #[test]
    fn q7j_history_parse_filter_and_render() {
        assert_eq!(
            parse(&["history", "Codex", "--days", "7", "-j"]),
            QuotaCommand::History {
                filters: vec!["codex".to_owned()],
                days: Some("7".to_owned()),
                json: true
            }
        );
        assert_eq!(
            parse(&["history", "--days=0.5"]),
            QuotaCommand::History {
                filters: Vec::new(),
                days: Some("0.5".to_owned()),
                json: false
            }
        );
        assert_eq!(
            parse(&["history", "--days", "0"]),
            QuotaCommand::Invalid {
                code: 1,
                message: "--days takes a number of days, such as 1 or 7, not \"0\"".to_owned()
            }
        );
        assert!(matches!(
            parse(&["history", "--days"]),
            QuotaCommand::Invalid { code: 1, message } if message.starts_with("unknown flag --days\n")
        ));
        assert!(matches!(
            parse(&["history", "--week"]),
            QuotaCommand::Invalid { code: 1, message } if message.starts_with("unknown flag --week\n")
        ));
        assert!(matches!(
            parse(&["refresh", "codex"]),
            QuotaCommand::Invalid { code: 2, .. }
        ));

        let point = |at: &str, left: f64, resets: Option<&str>| QuotaPoint {
            at: QuotaTime::new(at),
            left,
            start: None,
            resets_at: resets.map(QuotaTime::new),
        };
        let histories = vec![
            QuotaHistory {
                provider: "codex".to_owned(),
                user: "a@x.io".to_owned(),
                lines: vec![QuotaLine {
                    name: "5h".to_owned(),
                    points: vec![
                        point("2026-10-31T01:12:00Z", 80.0, Some("2026-10-31T05:00:00Z")),
                        point("2026-10-31T02:40:00Z", 61.5, Some("2026-10-31T05:03:00Z")),
                        point("2026-10-31T17:02:00Z", 100.0, Some("2026-10-31T22:00:00Z")),
                    ],
                }],
            },
            QuotaHistory {
                provider: "kimi".to_owned(),
                user: String::new(),
                lines: Vec::new(),
            },
        ];
        let out = render_history(histories.clone(), &[], false, &now()).expect("text");
        assert_eq!(
            out,
            "codex · a@x.io\n  5h  Oct 31 09:12 80% · 10:40 61.5% · ↻ Nov 1 01:02 100%\n\
             kimi\n  % is how much was left · ↻ a new run of the window · \
             --json for every reading, or GET /v1/hofund/quotas/history on the gateway"
        );
        let only = |filter: &str| {
            render_history(histories.clone(), &[filter.to_owned()], true, &now())
                .map(|json| json.matches("\"provider\"").count())
        };
        assert_eq!(only("codex"), Ok(1));
        assert_eq!(only("a@x.io"), Ok(1));
        assert_eq!(only("codex/a@x.io"), Ok(1));
        assert_eq!(only("kimi"), Ok(1));
        assert_eq!(only("claude"), Ok(0));
        assert_eq!(
            render_history(histories, &["claude".to_owned()], false, &now()),
            Err("no readings kept of claude".to_owned())
        );
        assert_eq!(
            render_history(Vec::new(), &[], false, &now()),
            Ok(
                "nothing kept yet · a subscription's or plan's windows are kept here as \
                hofund reads them"
                    .to_owned()
            )
        );
    }

    #[test]
    fn q7k_points_cell_keeps_last_readings_and_marks_new_runs() {
        let point = |minute: i64, left: f64, resets: Option<i64>| QuotaPoint {
            at: later(TimeDelta::minutes(minute)),
            left,
            start: None,
            resets_at: resets.map(|minute| later(TimeDelta::minutes(minute))),
        };
        let points: Vec<QuotaPoint> = (0..10).map(|n| point(n, 50.0, None)).collect();
        let cell = points_cell(&points, 8, &zone());
        assert_eq!(cell.matches(" · ").count(), 7, "{cell}");
        assert!(cell.starts_with("Nov 1 10:02 50%"), "{cell}");
        assert!(new_run(
            &point(0, 10.0, Some(60)),
            &point(1, 10.0, Some(71))
        ));
        assert!(!new_run(
            &point(0, 10.0, Some(60)),
            &point(1, 90.0, Some(70))
        ));
        assert!(new_run(
            &point(0, 10.0, Some(60)),
            &point(1, 10.0, Some(49))
        ));
        assert!(new_run(&point(0, 10.0, None), &point(1, 15.5, Some(10))));
        assert!(!new_run(&point(0, 10.0, None), &point(1, 15.0, None)));
    }

    #[test]
    fn q7l_durations_parse_and_print_like_the_reference() {
        for (text, seconds) in [
            ("30m", 1800.0),
            ("6h", 21600.0),
            ("1h30m", 5400.0),
            ("1.5h", 5400.0),
            ("+90s", 90.0),
            ("500ms", 0.5),
            (".5m", 30.0),
            ("2us", 0.000_002),
            ("0", 0.0),
        ] {
            assert_eq!(
                parse_go_duration(text).map(|duration| duration.as_secs_f64()),
                Some(seconds),
                "{text}"
            );
        }
        for bad in ["", "5", "m", "1x", "1.2.3s", "-5m", "."] {
            assert_eq!(parse_go_duration(bad), None, "{bad}");
        }
        assert_eq!(go_duration(Duration::from_secs(90)), "1m30s");
        assert_eq!(go_duration(Duration::from_millis(1500)), "1.5s");
        assert_eq!(go_duration(Duration::from_millis(500)), "500ms");
        assert_eq!(go_duration(Duration::from_micros(1500)), "1.5ms");
        assert_eq!(go_duration(Duration::from_nanos(1500)), "1.5µs");
        assert_eq!(go_duration(Duration::from_nanos(15)), "15ns");
        assert_eq!(go_duration(Duration::ZERO), "0s");
        assert_eq!(round_wait(Duration::from_secs(120)), "2m");
        assert_eq!(round_wait(Duration::from_secs(90)), "2m");
        assert_eq!(round_wait(Duration::from_secs(89)), "1m");
        assert_eq!(round_wait(Duration::from_secs(600)), "10m");
        assert_eq!(round_wait(Duration::from_secs(20)), "");
        assert_eq!(
            super::balance_text(1_234_567.891, "credits"),
            "1,234,567.89 credits left"
        );
        assert_eq!(super::balance_text(-1000.0, ""), "-1,000 left");
        assert_eq!(super::balance_text(12.0, "USD"), "12 USD left");
    }
}
