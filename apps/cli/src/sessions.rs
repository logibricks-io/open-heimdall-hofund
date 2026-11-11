//! Local session listing, daily totals, and read consent.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::{DateTime, Days, Local, NaiveTime, TimeZone};
use hofund_config::load_path;
use hofund_control_client::hofund_control_proto::API_VERSION;
use hofund_model_catalog::{EffectivePricer, ListPriceLookup, ModelCatalog, ReqwestFetcher};
use hofund_plugin_api::{SessionRecord, SessionStore, SessionTokens};
use hofund_sessions::consent::Consent;
use hofund_sessions::index::SessionIndex;
use hofund_sessions::pricing::{TokenPrice, price_records, token_cost};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use hofund_plugin_agent_cindy::CindyStore;
use hofund_plugin_agent_claude::ClaudeStore;
use hofund_plugin_agent_codex::CodexStore;
use hofund_plugin_agent_raft::RaftStore;

pub(crate) const HELP: &str = "\
usage: hofund sessions [--days N|today|all] [--model MODEL] [--folder FOLDER] [--limit N] [--json]
       hofund sessions read [on|off all|AGENT…]

List the newest 20 sessions by default. --days (bare: 7), today, all, or the
stats subcommand prints daily totals instead. Reads stay off until consent is
explicitly granted. This command never resumes, deletes, exports, or records
session files.
";

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Flags {
    pub days: Option<String>,
    pub model: Option<String>,
    pub folder: Option<String>,
    pub limit: Option<String>,
    pub json: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Command {
    List(Options),
    Read(Option<ReadChange>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Options {
    pub(crate) days: Option<usize>,
    model: String,
    folder: String,
    limit: usize,
    json: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ReadChange {
    on: bool,
    targets: Vec<String>,
}

pub(crate) fn parse(words: &[&str], flags: Flags) -> Result<Command, String> {
    if words.first() == Some(&"read") {
        if flags != Flags::default() {
            return Err("sessions read takes no listing flags".to_owned());
        }
        if words.len() == 1 {
            return Ok(Command::Read(None));
        }
        if words.len() < 3 || !matches!(words[1], "on" | "off") {
            return Err(HELP.to_owned());
        }
        return Ok(Command::Read(Some(ReadChange {
            on: words[1] == "on",
            targets: words[2..].iter().map(|word| (*word).to_owned()).collect(),
        })));
    }
    let days = match flags.days.as_deref() {
        None => None,
        Some("today" | "1d") => Some(1),
        Some("all" | "0") => Some(0),
        Some(value) => Some(
            value
                .strip_suffix('d')
                .unwrap_or(value)
                .parse::<usize>()
                .map_err(|_| "--days takes a number of days, today or all".to_owned())?,
        ),
    };
    let limit = flags
        .limit
        .as_deref()
        .unwrap_or("20")
        .parse::<usize>()
        .ok()
        .filter(|limit| *limit >= 1)
        .ok_or_else(|| "--limit takes a number of at least 1".to_owned())?;
    if !matches!(words, [] | ["stats"]) {
        return Err(format!("unknown sessions argument `{}`", words.join(" ")));
    }
    Ok(Command::List(Options {
        days: days.or(words.first().filter(|word| **word == "stats").map(|_| 7)),
        model: flags.model.unwrap_or_default(),
        folder: flags.folder.unwrap_or_default(),
        limit,
        json: flags.json,
    }))
}

pub(crate) fn run(config: &Path, command: &Command) -> Result<String, String> {
    let claude = ClaudeStore::new();
    let cindy = CindyStore::new();
    let codex = CodexStore::new();
    let raft = RaftStore::new();
    let stores: [&dyn SessionStore; 4] = [&claude, &cindy, &codex, &raft];
    run_with(config, command, &stores, Local::now())
}

fn run_with(
    config: &Path,
    command: &Command,
    stores: &[&dyn SessionStore],
    now: DateTime<Local>,
) -> Result<String, String> {
    let config_dir = config_dir(config);
    let mut consent = Consent::new(&config_dir);
    consent.settle();
    match command {
        Command::Read(change) => read_change(&mut consent, stores, change.as_ref()),
        Command::List(options) => {
            let mut index = SessionIndex::load(&config_dir, &consent);
            index
                .refresh(&consent, stores)
                .map_err(|error| error.to_string())?;
            let mut records = index.list(&consent);
            let pricing = pricing(config);
            for record in &mut records {
                price_record(record, pricing.as_ref());
            }
            if let Some(days) = options.days {
                stats(&records, options, days, now, pricing.as_ref())
            } else {
                list(&records, options, now, &consent)
            }
        }
    }
}

fn read_change(
    consent: &mut Consent,
    stores: &[&dyn SessionStore],
    change: Option<&ReadChange>,
) -> Result<String, String> {
    let Some(change) = change else {
        return Ok(read_status(consent, stores));
    };
    let found: Vec<&str> = stores
        .iter()
        .filter(|store| store.locator().present())
        .map(|store| store.agent())
        .collect();
    for target in &change.targets {
        if target != "all" && !found.contains(&target.as_str()) {
            return Err(format!(
                "unknown agent `{target}`; hofund sessions read lists found agents"
            ));
        }
    }
    let mut all = None;
    let mut agents = Vec::new();
    for target in &change.targets {
        if target == "all" {
            all = Some(change.on);
            agents.extend(found.iter().map(|agent| ((*agent).to_owned(), change.on)));
        } else {
            agents.push((target.clone(), change.on));
        }
    }
    consent
        .set_reads(all, agents)
        .map_err(|error| error.to_string())?;
    Ok(read_status(consent, stores))
}

fn read_status(consent: &Consent, stores: &[&dyn SessionStore]) -> String {
    let reads = consent.reads();
    let mut lines = vec![
        format!("asked: {}", reads.asked),
        format!("all: {}", reads.all),
    ];
    let mut agents: Vec<&str> = stores
        .iter()
        .filter(|store| store.locator().present())
        .map(|store| store.agent())
        .collect();
    agents.extend(reads.agents.keys().map(String::as_str));
    agents.sort_unstable();
    agents.dedup();
    for agent in agents {
        lines.push(format!(
            "{agent}: {}",
            if consent.allows(agent) { "on" } else { "off" }
        ));
    }
    lines.join("\n")
}

fn list(
    records: &[SessionRecord],
    options: &Options,
    now: DateTime<Local>,
    consent: &Consent,
) -> Result<String, String> {
    let filtered: Vec<SessionRecord> = records
        .iter()
        .filter(|record| matches(record, options))
        .cloned()
        .collect();
    let shown = filtered.iter().take(options.limit).collect::<Vec<_>>();
    if options.json {
        return list_json(&shown, consent);
    }
    if shown.is_empty() {
        return Ok(if consent.reads().asked {
            "no sessions".to_owned()
        } else {
            "no sessions; reads are off (use hofund sessions read on all)".to_owned()
        });
    }
    let mut rows = vec![vec![
        "AGE".to_owned(),
        "AGENT".to_owned(),
        "FOLDER".to_owned(),
        "TOKENS".to_owned(),
        "COST".to_owned(),
        "TITLE".to_owned(),
    ]];
    for record in shown {
        rows.push(vec![
            age(record.last_ms, now),
            record.agent.clone(),
            folder_base(&record.cwd),
            record.tokens().total().to_string(),
            cost_text(record.cost, record.unpriced),
            record.title.clone(),
        ]);
    }
    Ok(table(&rows))
}

fn list_json(shown: &[&SessionRecord], consent: &Consent) -> Result<String, String> {
    let sessions: Vec<Value> = shown.iter().map(|record| session_json(record)).collect();
    let reads = consent.reads();
    let enabled: Vec<&str> = reads
        .agents
        .iter()
        .filter(|(_, on)| **on)
        .map(|(agent, _)| agent.as_str())
        .collect();
    let value = json!({
        "version": API_VERSION,
        "revision": revision(&sessions),
        "consent": {"asked": reads.asked, "all": reads.all, "enabled": enabled},
        "sessions": sessions,
    });
    serde_json::to_string_pretty(&value).map_err(|error| error.to_string())
}

fn session_json(record: &SessionRecord) -> Value {
    let tokens = record.tokens();
    json!({
        "id": record.id,
        "agent": record.agent,
        "title": record.title,
        "cwd": record.cwd,
        "last_ms": record.last_ms,
        "models": record.model_ids(),
        "tokens": tokens_json(tokens),
        "cost": record.cost,
        "unpriced": record.unpriced,
        "resume": Value::Null,
        "read_only": false,
        "gateway": false,
    })
}

fn stats(
    records: &[SessionRecord],
    options: &Options,
    days: usize,
    now: DateTime<Local>,
    pricing: Option<&EffectivePricer>,
) -> Result<String, String> {
    let since = if days == 0 {
        None
    } else {
        let back = u64::try_from(days - 1).unwrap_or(u64::MAX);
        let date = now
            .date_naive()
            .checked_sub_days(Days::new(back))
            .unwrap_or(now.date_naive());
        now.timezone()
            .from_local_datetime(&date.and_time(NaiveTime::MIN))
            .earliest()
            .map(|date| date.format("%Y-%m-%d").to_string())
    };
    let mut totals = Stats::default();
    for record in records.iter().filter(|record| matches(record, options)) {
        for day in &record.days {
            if since
                .as_deref()
                .is_some_and(|since| day.date.as_str() < since)
            {
                continue;
            }
            let price =
                pricing.and_then(|pricing| price_for_model(pricing, &record.agent, &day.model));
            let cost = price.map_or(0.0, |price| token_cost(price, day.tokens));
            totals.add(day, record, cost, price.is_none());
        }
    }
    if options.json {
        return totals.json();
    }
    Ok(totals.table())
}

#[derive(Default)]
struct Stats {
    tokens: SessionTokens,
    cost: f64,
    unpriced_models: std::collections::BTreeSet<String>,
    days: BTreeMap<String, StatShare>,
    models: BTreeMap<String, StatShare>,
    folders: BTreeMap<String, StatShare>,
}

#[derive(Clone, Default)]
struct StatShare {
    tokens: SessionTokens,
    cost: f64,
}

impl Stats {
    fn add(
        &mut self,
        day: &hofund_plugin_api::SessionDay,
        record: &SessionRecord,
        cost: f64,
        unpriced: bool,
    ) {
        self.tokens.add(day.tokens);
        self.cost += cost;
        if unpriced {
            self.unpriced_models
                .insert(format!("{}:{}", record.id, day.model));
        }
        self.days
            .entry(day.date.clone())
            .or_default()
            .add(day.tokens, cost);
        self.models
            .entry(day.model.clone())
            .or_default()
            .add(day.tokens, cost);
        self.folders
            .entry(record.cwd.clone())
            .or_default()
            .add(day.tokens, cost);
    }

    fn unpriced(&self) -> u64 {
        u64::try_from(self.unpriced_models.len()).unwrap_or(u64::MAX)
    }

    fn table(&self) -> String {
        if self.tokens.is_zero() {
            return "nothing in this range".to_owned();
        }
        let mut out = format!(
            "tokens: {}  cost: {}  unpriced: {}\nDATE       TOKENS     COST",
            self.tokens.total(),
            cost_text(self.cost, self.unpriced()),
            self.unpriced()
        );
        for (date, share) in &self.days {
            out.push_str(&format!(
                "\n{date}  {}  {}",
                share.tokens.total(),
                cost_text(share.cost, 0)
            ));
        }
        out
    }

    fn json(&self) -> Result<String, String> {
        serde_json::to_string_pretty(&json!({
            "version": API_VERSION,
            "revision": revision(&self.days.keys().collect::<Vec<_>>()),
            "tokens": tokens_json(self.tokens),
            "cost": self.cost,
            "unpriced": self.unpriced(),
            "days": self.days.iter().map(|(date, share)| json!({
                "date": date, "tokens": tokens_json(share.tokens), "cost": share.cost,
            })).collect::<Vec<_>>(),
            "models": shares_json(&self.models),
            "folders": shares_json(&self.folders),
        }))
        .map_err(|error| error.to_string())
    }
}

impl StatShare {
    fn add(&mut self, tokens: SessionTokens, cost: f64) {
        self.tokens.add(tokens);
        self.cost += cost;
    }
}

fn shares_json(shares: &BTreeMap<String, StatShare>) -> Vec<Value> {
    shares
        .iter()
        .map(|(name, share)| json!({"name": name, "tokens": tokens_json(share.tokens)}))
        .collect()
}

fn matches(record: &SessionRecord, options: &Options) -> bool {
    let model = options.model.to_lowercase();
    let folder = options.folder.to_lowercase();
    (model.is_empty()
        || record
            .models
            .keys()
            .any(|name| name.to_lowercase().contains(&model)))
        && (folder.is_empty() || record.cwd.to_lowercase().contains(&folder))
}

fn tokens_json(tokens: SessionTokens) -> Value {
    json!({
        "input": tokens.input,
        "output": tokens.output,
        "cache_read": tokens.cache_read,
        "cache_write": tokens.cache_write,
    })
}

fn revision(value: &impl serde::Serialize) -> String {
    let bytes = serde_json::to_vec(value).unwrap_or_default();
    format!("{:x}", Sha256::digest(bytes))
}

fn age(last_ms: i64, now: DateTime<Local>) -> String {
    let seconds = (now.timestamp_millis() - last_ms).max(0) / 1000;
    if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 3600 {
        format!("{}m", seconds / 60)
    } else if seconds < 86_400 {
        format!("{}h", seconds / 3600)
    } else {
        format!("{}d", seconds / 86_400)
    }
}

fn folder_base(cwd: &str) -> String {
    if cwd.is_empty() {
        return "-".to_owned();
    }
    Path::new(cwd)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(cwd)
        .to_owned()
}

fn cost_text(cost: f64, unpriced: u64) -> String {
    if cost == 0.0 && unpriced == 0 {
        "-".to_owned()
    } else {
        format!("${cost:.4}{}", if unpriced > 0 { "+" } else { "" })
    }
}

fn table(rows: &[Vec<String>]) -> String {
    let mut widths = vec![0; rows.first().map_or(0, Vec::len)];
    for row in rows {
        for (width, cell) in widths.iter_mut().zip(row) {
            *width = (*width).max(cell.chars().count());
        }
    }
    rows.iter()
        .map(|row| {
            row.iter()
                .enumerate()
                .map(|(index, cell)| {
                    let width = widths[index];
                    if index + 1 == row.len() {
                        cell.clone()
                    } else {
                        format!("{cell:<width$}")
                    }
                })
                .collect::<Vec<_>>()
                .join("  ")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn config_dir(config: &Path) -> PathBuf {
    config
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf()
}

struct NoListPrices;

impl ListPriceLookup for NoListPrices {
    fn list_price(&self, _provider_id: &str, _model: &str) -> Option<hofund_model_catalog::Price> {
        None
    }
}

fn pricing(config: &Path) -> Option<EffectivePricer> {
    let settings = load_path(config).ok()?;
    let data_root = config_dir(config).join(".hofund");
    let fetcher = Arc::new(ReqwestFetcher::new().ok()?);
    let catalog = ModelCatalog::open(&data_root, settings.catalog, fetcher).ok()?;
    Some(EffectivePricer::new(
        settings.usage,
        catalog,
        Arc::new(NoListPrices),
    ))
}

fn price_record(record: &mut SessionRecord, pricing: Option<&EffectivePricer>) {
    price_records(std::slice::from_mut(record), |agent, model| {
        pricing.and_then(|pricing| price_for_model(pricing, agent, model))
    });
}

fn price_for_model(pricing: &EffectivePricer, agent: &str, model: &str) -> Option<TokenPrice> {
    let (provider, bare) = model.split_once('/').unwrap_or((agent, model));
    pricing
        .resolve(provider, bare, None, false)
        .map(|price| TokenPrice {
            input: price.input,
            output: price.output,
            cache_read: price.cache_read,
            cache_write: price.cache_write,
            cache_write_1h: price.cache_write_1h,
        })
}

#[cfg(test)]
mod tests {
    use hofund_plugin_api::{SessionDay, SessionFile};

    use super::*;

    fn record() -> SessionRecord {
        let mut record = SessionRecord::new("fake", "fake:one", "unused.jsonl");
        record.cwd = "/work/project".to_owned();
        record.title = "A title".to_owned();
        record.last_ms = 1_000;
        record.add_usage("model", SessionTokens::new(4, 3, 2, 1));
        record.add_day(SessionDay::new(
            "2026-11-10",
            "model",
            SessionTokens::new(4, 3, 2, 1),
        ));
        record
    }

    fn settled() -> (tempfile::TempDir, Consent) {
        let temp = tempfile::tempdir().expect("temp dir");
        let mut consent = Consent::new(temp.path());
        consent.settle();
        (temp, consent)
    }

    #[test]
    fn list_has_required_columns_and_default_limit() {
        let parsed = parse(&[], Flags::default()).expect("parse");
        let Command::List(options) = parsed else {
            panic!("list");
        };
        assert_eq!(options.limit, 20);
        let now = Local.timestamp_millis_opt(11_000).unwrap();
        let (_temp, consent) = settled();
        let out = list(&[record()], &options, now, &consent).expect("list");
        assert!(out.starts_with("AGE"));
        assert!(out.contains("10s"));
        assert!(out.contains("project"));
        assert!(out.contains("A title"));
    }

    #[test]
    fn configured_prices_apply_to_sessions_and_days_offline() {
        let temp = tempfile::tempdir().expect("temp dir");
        let config = temp.path().join("hofund.toml");
        std::fs::write(
            &config,
            "[usage.prices.\"*/model\"]\ninput = 1.0\noutput = 1.0\ncache_read = 1.0\ncache_write = 1.0\n",
        )
        .expect("config");
        let pricing = pricing(&config).expect("pricing");
        let mut record = record();
        price_record(&mut record, Some(&pricing));
        assert!((record.cost - 0.00001).abs() < f64::EPSILON);
        let options = Options {
            days: Some(0),
            json: true,
            ..list_options()
        };
        let out = stats(&[record], &options, 0, Local::now(), Some(&pricing)).expect("stats");
        let value: Value = serde_json::from_str(&out).expect("JSON");
        assert_eq!(value["cost"], 0.00001);
    }

    #[test]
    fn stats_group_local_days_and_filters() {
        let record = record();
        let options = Options {
            days: Some(0),
            model: String::new(),
            folder: "project".to_owned(),
            limit: 20,
            json: false,
        };
        let out = stats(&[record], &options, 0, Local::now(), None).expect("stats");
        assert!(out.contains("2026-11-10"));
        assert!(out.contains("tokens: 10"));
    }

    #[test]
    fn read_change_rejects_unknown_before_writing() {
        let temp = tempfile::tempdir().expect("temp dir");
        let mut consent = Consent::new(temp.path());
        consent.settle();
        let change = ReadChange {
            on: true,
            targets: vec!["unknown".to_owned()],
        };
        assert!(read_change(&mut consent, &[], Some(&change)).is_err());
        assert_eq!(
            std::fs::read_to_string(consent.path()).expect("consent"),
            "{\"asked\":false}\n"
        );
    }

    #[test]
    fn cindy_reader_is_consent_gated_through_index() {
        let temp = tempfile::tempdir().expect("temp dir");
        let config = temp.path().join("hofund.toml");
        let config_root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../plugins/agents/cindy/tests/fixtures/config")
            .canonicalize()
            .expect("cindy fixtures");
        let cindy = CindyStore::with_config_root(config_root);
        let stores: [&dyn SessionStore; 1] = [&cindy];
        let list = Command::List(Options {
            days: None,
            model: String::new(),
            folder: String::new(),
            limit: 20,
            json: false,
        });
        let off = run_with(&config, &list, &stores, Local::now()).expect("off list");
        assert!(off.contains("reads are off"));
        let on = Command::Read(Some(ReadChange {
            on: true,
            targets: vec!["cindy".to_owned()],
        }));
        run_with(&config, &on, &stores, Local::now()).expect("consent");
        let listed = run_with(&config, &list, &stores, Local::now()).expect("list");
        assert!(listed.contains("Ship the cindy reader"), "{listed}");
        assert!(listed.contains("Global build session"), "{listed}");
    }

    #[test]
    fn raft_reader_is_consent_gated_through_index() {
        let temp = tempfile::tempdir().expect("temp dir");
        let config = temp.path().join("hofund.toml");
        let raft_root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../plugins/agents/raft/tests/fixtures/root")
            .canonicalize()
            .expect("raft fixtures");
        let raft = RaftStore::with_root(raft_root);
        let stores: [&dyn SessionStore; 1] = [&raft];
        let list = Command::List(Options {
            days: None,
            model: String::new(),
            folder: String::new(),
            limit: 20,
            json: false,
        });
        let off = run_with(&config, &list, &stores, Local::now()).expect("off list");
        assert!(off.contains("reads are off"));
        let on = Command::Read(Some(ReadChange {
            on: true,
            targets: vec!["raft".to_owned()],
        }));
        run_with(&config, &on, &stores, Local::now()).expect("consent");
        let listed = run_with(&config, &list, &stores, Local::now()).expect("list");
        assert!(listed.contains("Ship the raft reader"), "{listed}");
        assert!(listed.contains("Builtin agent run"), "{listed}");
    }

    #[test]
    fn real_readers_are_consent_gated_and_read_only() {
        let temp = tempfile::tempdir().expect("temp dir");
        let config = temp.path().join("hofund.toml");
        let plugins = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../plugins/agents")
            .canonicalize()
            .expect("plugin fixtures");
        let claude_root = plugins.join("claude-code/tests/fixtures/root");
        let codex_root = plugins.join("codex/tests/fixtures/home");
        let main =
            claude_root.join("projects/-work-app/11111111-2222-3333-4444-555555555555.jsonl");
        let packed = codex_root.join(
            "archived_sessions/rollout-2026-09-21T10-00-00-02a0bdc9-b5fd-7e63-9658-68bc8dce5ecd.jsonl.zst",
        );
        let before = [
            std::fs::read(&main).expect("main"),
            std::fs::read(&packed).expect("packed"),
        ];
        let claude = ClaudeStore::with_root(claude_root);
        let codex = CodexStore::with_root(codex_root);
        let stores: [&dyn SessionStore; 2] = [&claude, &codex];
        let list = Command::List(Options {
            days: None,
            model: String::new(),
            folder: String::new(),
            limit: 20,
            json: false,
        });
        let off = run_with(&config, &list, &stores, Local::now()).expect("off list");
        assert!(off.contains("reads are off"));
        assert!(!temp.path().join("state/sessions-index.json").exists());
        let on = Command::Read(Some(ReadChange {
            on: true,
            targets: vec!["all".to_owned()],
        }));
        let status = run_with(&config, &on, &stores, Local::now()).expect("consent");
        assert!(status.contains("asked: true"));
        assert!(status.contains("claude: on"));
        assert!(status.contains("codex: on"));
        let listed = run_with(&config, &list, &stores, Local::now()).expect("list");
        let cache = std::fs::read_to_string(temp.path().join("state/sessions-index.json"))
            .expect("session cache");
        assert!(listed.contains("Login repair"), "{listed}\n{cache}");
        assert!(listed.contains("Add a README"));
        assert!(listed.contains("Packed rollout"));
        let daily = Command::List(Options {
            days: Some(0),
            ..list_options()
        });
        let stats = run_with(&config, &daily, &stores, Local::now()).expect("stats");
        assert!(stats.contains("2026-09-20"));
        assert_eq!(std::fs::read(&main).expect("main"), before[0]);
        assert_eq!(std::fs::read(&packed).expect("packed"), before[1]);
    }

    fn list_options() -> Options {
        Options {
            days: None,
            model: String::new(),
            folder: String::new(),
            limit: 20,
            json: false,
        }
    }

    #[test]
    fn json_list_uses_session_wire_fields() {
        let record = record();
        let options = Options {
            days: None,
            model: String::new(),
            folder: String::new(),
            limit: 20,
            json: true,
        };
        let (_temp, consent) = settled();
        let out = list(&[record], &options, Local::now(), &consent).expect("JSON");
        let value: Value = serde_json::from_str(&out).expect("parse JSON");
        assert_eq!(value["sessions"][0]["id"], "fake:one");
        assert!(value["sessions"][0]["resume"].is_null());
        assert!(value["sessions"][0].get("path").is_none());
    }

    #[test]
    fn parser_accepts_days_forms() {
        for (word, days) in [("today", 1), ("all", 0), ("30", 30)] {
            let parsed = parse(
                &[],
                Flags {
                    days: Some(word.to_owned()),
                    ..Flags::default()
                },
            )
            .expect("days");
            let Command::List(options) = parsed else {
                panic!("list");
            };
            assert_eq!(options.days, Some(days));
        }
    }

    #[test]
    fn session_file_type_remains_plugin_owned() {
        let _ = SessionFile::new("id", "unused");
    }
}
