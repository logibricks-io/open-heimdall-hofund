use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Days, Local, NaiveDate, NaiveTime, TimeZone};
use hofund_control_proto::{
    ControlError, ControlErrorKind, SessionConsent, SessionDayStats, SessionDetailResponse,
    SessionListQuery, SessionListResponse, SessionOverviewDay, SessionOverviewQuery,
    SessionOverviewResponse, SessionOverviewTop, SessionProgressResponse, SessionStatShare,
    SessionStatsQuery, SessionStatsResponse, SessionSummary, SessionTokenTotals,
    SessionTranscriptPart, SessionTranscriptResponse,
};
use hofund_gateway::control::ControlFailure;
use hofund_model_catalog::EffectivePricer;
use hofund_plugin_api::{SessionDay, SessionFile, SessionRecord, SessionStore, SessionTokens};
use hofund_sessions::consent::Consent;
use hofund_sessions::index::SessionIndex;
use hofund_sessions::pricing::{price_records, token_cost};

pub(crate) struct SessionControl {
    stores: Vec<Arc<dyn SessionStore>>,
    state: Mutex<SessionState>,
    pricing: Arc<EffectivePricer>,
    progress: IndexProgress,
}

struct SessionState {
    consent: Consent,
    index: SessionIndex,
}

#[derive(Default)]
struct IndexProgress {
    total: AtomicU64,
    done: AtomicU64,
    running: AtomicBool,
}

impl SessionControl {
    pub(crate) fn new(
        config_path: &Path,
        stores: Vec<Arc<dyn SessionStore>>,
        pricing: Arc<EffectivePricer>,
    ) -> Self {
        let config_dir = config_dir(config_path);
        let mut consent = Consent::new(&config_dir);
        consent.settle();
        let index = SessionIndex::load(&config_dir, &consent);
        Self {
            stores,
            state: Mutex::new(SessionState { consent, index }),
            pricing,
            progress: IndexProgress::default(),
        }
    }

    pub(crate) fn list(
        &self,
        query: SessionListQuery,
        now: DateTime<Local>,
    ) -> Result<SessionListResponse, ControlFailure> {
        let records = self.records()?;
        let since = since_day(now, query.days);
        let mut response = SessionListResponse::default();
        response.sessions = records
            .iter()
            .filter(|record| in_list_window(record, since))
            .filter(|record| list_matches(record, &query.model, &query.folder))
            .take(query.limit())
            .map(session_summary)
            .collect();
        response.consent = self.consent()?;
        seal(&mut response, |response| &mut response.revision);
        Ok(response)
    }

    pub(crate) fn stats(
        &self,
        query: SessionStatsQuery,
        now: DateTime<Local>,
    ) -> Result<SessionStatsResponse, ControlFailure> {
        let records = self.records()?;
        let since = since_day(now, query.days).map(|date| date.to_string());
        let mut totals = Stats::default();
        for record in records
            .iter()
            .filter(|record| list_matches(record, &query.model, &query.folder))
        {
            for day in &record.days {
                if since
                    .as_deref()
                    .is_some_and(|since| day.date.as_str() < since)
                {
                    continue;
                }
                let price = self.price_for(&record.agent, &day.model);
                let cost = price.map_or(0.0, |price| token_cost(price, day.tokens));
                totals.add(record, day, cost, price.is_none());
            }
        }
        let mut response = SessionStatsResponse::default();
        response.tokens = wire_tokens(totals.tokens);
        response.cost = totals.cost;
        response.unpriced = u64::try_from(totals.unpriced.len()).unwrap_or(u64::MAX);
        response.days = totals
            .days
            .into_iter()
            .map(|(date, share)| {
                let mut row = SessionDayStats::default();
                row.date = date;
                row.tokens = wire_tokens(share.tokens);
                row.cost = share.cost;
                row
            })
            .collect();
        response.models = stat_shares(totals.models);
        response.folders = stat_shares(totals.folders);
        seal(&mut response, |response| &mut response.revision);
        Ok(response)
    }

    pub(crate) fn overview(
        &self,
        query: SessionOverviewQuery,
        now: DateTime<Local>,
    ) -> Result<SessionOverviewResponse, ControlFailure> {
        let records = self.records()?;
        let since = since_day(now, query.days);
        let since_text = since.map(|date| date.to_string());
        let matching: Vec<&SessionRecord> = records
            .iter()
            .filter(|record| {
                (query.agent.is_empty() || record.agent == query.agent)
                    && (query.cwd.is_empty() || record.cwd == query.cwd)
                    && (query.model.is_empty() || record.models.contains_key(&query.model))
                    && in_overview_window(record, since, now)
            })
            .collect();
        let mut response = SessionOverviewResponse::default();
        response.count = u64::try_from(matching.len()).unwrap_or(u64::MAX);
        let mut costs: Vec<f64> = matching.iter().map(|record| record.cost).collect();
        costs.sort_by(f64::total_cmp);
        if !costs.is_empty() {
            let rank = ((costs.len() as f64) * 0.5).ceil() as usize;
            response.median_cost = costs[rank.saturating_sub(1)];
        }
        let mut days: BTreeMap<String, SessionOverviewDay> = BTreeMap::new();
        for record in &matching {
            let mut seen = BTreeSet::new();
            for day in &record.days {
                if since_text
                    .as_deref()
                    .is_some_and(|since| day.date.as_str() < since)
                    || !seen.insert(day.date.clone())
                {
                    continue;
                }
                let mut cost = 0.0;
                for share in record.days.iter().filter(|share| share.date == day.date) {
                    cost += self
                        .price_for(&record.agent, &share.model)
                        .map_or(0.0, |price| token_cost(price, share.tokens));
                }
                let entry = days.entry(day.date.clone()).or_default();
                entry.date.clone_from(&day.date);
                entry.count = entry.count.saturating_add(1);
                entry.cost += cost;
            }
        }
        response.by_day = days.into_values().collect();
        let mut top: Vec<&&SessionRecord> =
            matching.iter().filter(|record| record.cost > 0.0).collect();
        top.sort_by(|left, right| {
            right
                .cost
                .total_cmp(&left.cost)
                .then_with(|| left.id.cmp(&right.id))
        });
        response.top = top
            .into_iter()
            .take(8)
            .map(|record| {
                let mut row = SessionOverviewTop::default();
                row.id.clone_from(&record.id);
                row.title.clone_from(&record.title);
                row.cost = record.cost;
                row
            })
            .collect();
        Ok(response)
    }

    pub(crate) fn detail(&self, id: &str) -> Result<SessionDetailResponse, ControlFailure> {
        let record = self.session_record(id)?;
        let transcript_available = self
            .stores
            .iter()
            .find(|store| store.agent() == record.agent)
            .is_some_and(|store| store.transcript_available())
            && !record.path.as_os_str().is_empty();
        let mut response = SessionDetailResponse::default();
        response.session = session_summary(&record);
        response.transcript_available = transcript_available;
        Ok(response)
    }

    pub(crate) fn transcript(
        &self,
        id: &str,
        sub: Option<&str>,
    ) -> Result<SessionTranscriptResponse, ControlFailure> {
        let (_, transcript) = self.read_transcript(id, sub)?;
        let mut response = SessionTranscriptResponse::default();
        response.parts = transcript
            .parts
            .into_iter()
            .map(|part| {
                let mut out = SessionTranscriptPart::default();
                out.role = part.role;
                out.kind = part.kind;
                out.name = part.name;
                out.text = part.text;
                out
            })
            .collect();
        response.cut = transcript.cut;
        response.source = transcript.source;
        Ok(response)
    }

    pub(crate) fn markdown(&self, id: &str) -> Result<String, ControlFailure> {
        let (record, transcript) = self.read_transcript(id, None)?;
        Ok(hofund_sessions::read::markdown(&record, &transcript))
    }

    fn read_transcript(
        &self,
        id: &str,
        sub: Option<&str>,
    ) -> Result<(SessionRecord, hofund_plugin_api::SessionTranscript), ControlFailure> {
        self.check_read_consent(id)?;
        let record = self.session_record(id)?;
        if sub.is_some_and(|sub| !record.subagents.iter().any(|listed| listed == sub)) {
            return Err(session_not_found());
        }
        let store = self
            .stores
            .iter()
            .find(|store| store.agent() == record.agent)
            .ok_or_else(session_not_found)?;
        let (root, file) = self.session_file(store.as_ref(), &record, sub)?;
        let transcript = hofund_sessions::read::transcript(&root, store.as_ref(), &file)
            .map_err(|_| session_internal())?;
        Ok((record, transcript))
    }

    fn session_record(&self, id: &str) -> Result<SessionRecord, ControlFailure> {
        self.records()?
            .into_iter()
            .find(|record| record.id == id)
            .ok_or_else(session_not_found)
    }

    fn check_read_consent(&self, id: &str) -> Result<(), ControlFailure> {
        let state = self.state.lock().map_err(|_| session_internal())?;
        if let Some((agent, _)) = id.split_once(':')
            && !state.consent.allows(agent)
        {
            return Err(ControlError::new(
                ControlErrorKind::Forbidden,
                "session reads are not consented for this agent",
                "sessions",
            )
            .into());
        }
        Ok(())
    }

    fn session_file(
        &self,
        store: &dyn SessionStore,
        record: &SessionRecord,
        sub: Option<&str>,
    ) -> Result<(PathBuf, SessionFile), ControlFailure> {
        for root in store.locator().roots() {
            if !hofund_sessions::open::within_root(&root, &record.path) {
                continue;
            }
            let files = store.files(&root).map_err(|_| session_internal())?;
            let found = files.into_iter().find(|file| {
                if sub.is_none() {
                    file.path == record.path
                } else {
                    sub.is_some_and(|sub| {
                        file.id == sub || format!("{}:{}", record.agent, file.id) == sub
                    })
                }
            });
            if let Some(file) = found {
                return Ok((root, file));
            }
        }
        Err(session_not_found())
    }

    pub(crate) fn progress(&self) -> SessionProgressResponse {
        let mut response = SessionProgressResponse::default();
        response.files_total = self.progress.total.load(Ordering::SeqCst);
        response.files_done = self.progress.done.load(Ordering::SeqCst);
        response.running = self.progress.running.load(Ordering::SeqCst);
        response
    }

    fn records(&self) -> Result<Vec<SessionRecord>, ControlFailure> {
        self.progress.running.store(true, Ordering::SeqCst);
        self.progress.total.store(0, Ordering::SeqCst);
        self.progress.done.store(0, Ordering::SeqCst);
        let result = self.read_records();
        self.progress.running.store(false, Ordering::SeqCst);
        result
    }

    fn read_records(&self) -> Result<Vec<SessionRecord>, ControlFailure> {
        let mut state = self.state.lock().map_err(|_| session_internal())?;
        let stores: Vec<&dyn SessionStore> = self.stores.iter().map(AsRef::as_ref).collect();
        let SessionState { consent, index } = &mut *state;
        index
            .refresh_with_progress(consent, &stores, |total, done| {
                self.progress.total.store(total, Ordering::SeqCst);
                self.progress.done.store(done, Ordering::SeqCst);
            })
            .map_err(|_| session_internal())?;
        let mut records = index.list(consent);
        price_records(&mut records, |agent, model| self.price_for(agent, model));
        Ok(records)
    }

    fn consent(&self) -> Result<SessionConsent, ControlFailure> {
        let state = self.state.lock().map_err(|_| session_internal())?;
        let mut response = SessionConsent::default();
        response.asked = state.consent.reads().asked;
        response.all = state.consent.reads().all;
        response.enabled = state
            .consent
            .reads()
            .agents
            .iter()
            .filter(|(_, enabled)| **enabled)
            .map(|(agent, _)| agent.clone())
            .collect();
        Ok(response)
    }

    fn price_for(&self, agent: &str, model: &str) -> Option<hofund_usage::TokenPrice> {
        let (provider, bare) = model.split_once('/').unwrap_or((agent, model));
        self.pricing
            .resolve(provider, bare, None, false)
            .map(|price| hofund_usage::TokenPrice {
                input: price.input,
                output: price.output,
                cache_read: price.cache_read,
                cache_write: price.cache_write,
                cache_write_1h: price.cache_write_1h,
            })
    }
}

fn seal<T: serde::Serialize>(response: &mut T, mut revision: impl FnMut(&mut T) -> &mut String) {
    *revision(response) = String::new();
    let bytes = serde_json::to_vec(response).unwrap_or_default();
    *revision(response) = super::control::revision(&bytes);
}

fn session_summary(record: &SessionRecord) -> SessionSummary {
    let mut summary = SessionSummary::default();
    summary.id.clone_from(&record.id);
    summary.agent.clone_from(&record.agent);
    summary.title.clone_from(&record.title);
    summary.cwd.clone_from(&record.cwd);
    summary.last_ms = record.last_ms;
    summary.models = record.model_ids();
    summary.tokens = wire_tokens(record.tokens());
    summary.cost = record.cost;
    summary.unpriced = record.unpriced;
    summary.resume = None;
    summary.read_only = false;
    summary.gateway = false;
    summary
}

fn wire_tokens(tokens: SessionTokens) -> SessionTokenTotals {
    let mut out = SessionTokenTotals::default();
    out.input = tokens.input;
    out.output = tokens.output;
    out.cache_read = tokens.cache_read;
    out.cache_write = tokens.cache_write;
    out
}

fn config_dir(config_path: &Path) -> PathBuf {
    config_path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf()
}

fn since_day(now: DateTime<Local>, days: Option<u32>) -> Option<NaiveDate> {
    let days = days.filter(|days| *days > 0)?;
    let back = u64::from(days - 1);
    now.date_naive()
        .checked_sub_days(Days::new(back))
        .or(Some(now.date_naive()))
}

fn since_ms(date: NaiveDate) -> Option<i64> {
    Local
        .from_local_datetime(&date.and_time(NaiveTime::MIN))
        .earliest()
        .map(|date| date.timestamp_millis())
}

fn in_list_window(record: &SessionRecord, since: Option<NaiveDate>) -> bool {
    since.is_none_or(|since| since_ms(since).is_some_and(|since| record.last_ms >= since))
}

fn in_overview_window(
    record: &SessionRecord,
    since: Option<NaiveDate>,
    now: DateTime<Local>,
) -> bool {
    let Some(since) = since else {
        return true;
    };
    let since_text = since.to_string();
    record
        .days
        .iter()
        .any(|day| day.date.as_str() >= since_text.as_str())
        || (record.days.is_empty() && in_list_window(record, Some(since)))
        || (record.days.is_empty() && record.last_ms == 0 && since == now.date_naive())
}

fn list_matches(record: &SessionRecord, model: &str, folder: &str) -> bool {
    let model = model.to_lowercase();
    let folder = folder.to_lowercase();
    (model.is_empty()
        || record
            .models
            .keys()
            .any(|name| name.to_lowercase().contains(&model)))
        && (folder.is_empty() || record.cwd.to_lowercase().contains(&folder))
}

#[derive(Default)]
struct Stats {
    tokens: SessionTokens,
    cost: f64,
    unpriced: BTreeSet<String>,
    days: BTreeMap<String, StatShare>,
    models: BTreeMap<String, StatShare>,
    folders: BTreeMap<String, StatShare>,
}

impl Stats {
    fn add(&mut self, record: &SessionRecord, day: &SessionDay, cost: f64, unpriced: bool) {
        self.tokens.add(day.tokens);
        self.cost += cost;
        if unpriced {
            self.unpriced.insert(format!("{}:{}", record.id, day.model));
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
}

#[derive(Default)]
struct StatShare {
    tokens: SessionTokens,
    cost: f64,
}

impl StatShare {
    fn add(&mut self, tokens: SessionTokens, cost: f64) {
        self.tokens.add(tokens);
        self.cost += cost;
    }
}

fn stat_shares(shares: BTreeMap<String, StatShare>) -> Vec<SessionStatShare> {
    shares
        .into_iter()
        .map(|(name, share)| {
            let mut out = SessionStatShare::default();
            out.name = name;
            out.tokens = wire_tokens(share.tokens);
            out
        })
        .collect()
}

fn session_not_found() -> ControlFailure {
    ControlError::new(
        ControlErrorKind::NotFound,
        "no such session or subagent",
        "sessions",
    )
    .into()
}

fn session_internal() -> ControlFailure {
    ControlError::new(
        ControlErrorKind::Internal,
        "session index refresh failed",
        "sessions",
    )
    .into()
}
