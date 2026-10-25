//! Model lists and connectivity probes for one configured provider.
//!
//! `GET /v1/models` on the gateway stays the route table. This module asks
//! the upstream, or reads a catalog preset, and writes `hofund-models.json`
//! beside the config file.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use hofund_config::Config;
use hofund_provider::{
    ListQuery, ModelListKind, ProbeReport, Wire, diff_ids, list_models, model_list_kind,
    probe_wires, refresh_due, run_probe, wire_name,
};
use hofund_usage::{Ledger, Row};
use serde::{Deserialize, Serialize};

use crate::{DaemonError, Planned, beside, build_adapter, plan, show};

/// Printed probe lines, and whether every probe succeeded.
#[derive(Debug)]
pub struct TestReport {
    /// One line per protocol, already formatted.
    pub text: String,
    /// True when every probe returned a completion.
    pub ok: bool,
}

/// Lists the configured protocol for provider `id`.
///
/// A catalog preset is printed from the recorded ids and is not fetched.
/// A live preset is fetched. Added and removed ids are printed only when a
/// previous successful list for that protocol is in the cache.
///
/// # Errors
///
/// Returns [`DaemonError`] when the provider is missing, the preset cannot
/// be built, or the live list fails. Nothing is sent for a preset that has
/// no endpoint.
pub async fn provider_models(path: &Path, id: &str) -> Result<String, DaemonError> {
    let config = load_config(path)?;
    let provider = find_provider(&config, id)?;
    let planned = plan(provider)?;
    let kind = model_list_kind(
        provider.preset.as_deref(),
        provider.region.as_deref(),
        &provider.location,
    )
    .map_err(show)?;
    let wire = wire_name(planned.wire);
    let cache_path = model_cache_path(path);
    let mut cache = ModelCache::load(&cache_path)?;
    let (ids, source) = match kind {
        ModelListKind::Catalog(ids) => (ids, "catalog".to_owned()),
        ModelListKind::Live => {
            let ids = fetch_live(provider, &planned).await?;
            (ids, planned.base_url.clone())
        }
    };
    let previous = cache.ok_ids(id, wire);
    let diff = previous.as_ref().map(|before| diff_ids(before, &ids));
    cache.store_ok(id, wire, &ids, unix_now());
    cache.save(&cache_path)?;
    Ok(format_models(wire, &source, &ids, diff.as_ref()))
}

/// Probes every protocol provider `id` can speak.
///
/// The model is the first id from a successful cached list, or from the
/// catalog. A live provider with no cache is listed once, on its configured
/// protocol, and that first id is sent on every probe. A probe that sends a
/// request writes one usage row. A preset that cannot be built returns the
/// resolve error and writes nothing.
///
/// # Errors
///
/// Returns [`DaemonError`] when the provider is missing, the preset cannot
/// be built, or the usage database cannot be opened. A probe that the
/// upstream rejects is a line in [`TestReport`], not an error.
pub async fn provider_test(path: &Path, id: &str) -> Result<TestReport, DaemonError> {
    let config = load_config(path)?;
    let provider = find_provider(&config, id)?;
    let planned = plan(provider)?;
    let wires = speakable(provider, &planned)?;
    let cache_path = model_cache_path(path);
    let mut cache = ModelCache::load(&cache_path)?;
    let model = match model_for_test(provider, &planned, &mut cache, &cache_path).await {
        Ok(model) => model,
        Err(error) => {
            let text = wires
                .iter()
                .map(|wire| format!("{} fail {}", wire_name(wire.wire), error))
                .collect::<Vec<_>>()
                .join("\n");
            return Ok(TestReport {
                text: format!("{text}\n"),
                ok: false,
            });
        }
    };
    let ledger = Ledger::open(&beside(path, &config.usage_file)).map_err(show)?;
    let mut lines = Vec::new();
    let mut ok = true;
    for wire in &wires {
        let planned = Planned {
            base_url: wire.base_url.clone(),
            wire: wire.wire,
            credential: wire.credential,
        };
        let adapter = build_adapter(provider, &planned)?;
        let report = run_probe(adapter.as_ref(), wire.wire, &model).await;
        if report.sent {
            record_probe(&ledger, id, &report)?;
        }
        if !report.ok {
            ok = false;
        }
        lines.push(format_probe(wire.wire, &report));
    }
    Ok(TestReport {
        text: format!("{}\n", lines.join("\n")),
        ok,
    })
}

/// Fetches every configured provider whose `refresh_every` is due.
///
/// Catalog presets are skipped. One provider's failure is printed and does
/// not stop the others.
///
/// # Errors
///
/// Returns [`DaemonError`] when the config cannot be read.
pub async fn refresh_scheduled(path: &Path, now_unix: i64) -> Result<(), DaemonError> {
    let config = load_config(path)?;
    let cache_path = model_cache_path(path);
    let mut cache = ModelCache::load(&cache_path)?;
    for provider in &config.providers {
        let Some(every) = provider.refresh_every else {
            continue;
        };
        let planned = match plan(provider) {
            Ok(planned) => planned,
            Err(error) => {
                eprintln!("model list {}: {error}", provider.id);
                continue;
            }
        };
        let kind = match model_list_kind(
            provider.preset.as_deref(),
            provider.region.as_deref(),
            &provider.location,
        ) {
            Ok(kind) => kind,
            Err(error) => {
                eprintln!("model list {}: {error}", provider.id);
                continue;
            }
        };
        if !matches!(kind, ModelListKind::Live) {
            continue;
        }
        let wire = wire_name(planned.wire);
        let entry = cache.entry(&provider.id, wire);
        let (fetched, attempt, attempt_ok) = entry.map_or((0, 0, true), |entry| {
            (entry.fetched_unix, entry.attempt_unix, entry.attempt_ok)
        });
        if !refresh_due(every, fetched, attempt, attempt_ok, now_unix) {
            continue;
        }
        match fetch_live(provider, &planned).await {
            Ok(ids) => cache.store_ok(&provider.id, wire, &ids, now_unix),
            Err(error) => {
                eprintln!("model list {}: {error}", provider.id);
                cache.store_fail(&provider.id, wire, now_unix);
            }
        }
    }
    cache.save(&cache_path)?;
    Ok(())
}

/// Refreshes due lists until the task is aborted.
#[must_use]
pub fn spawn_model_refresh(path: PathBuf) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(60));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            if let Err(error) = refresh_scheduled(&path, unix_now()).await {
                eprintln!("model list: {error}");
            }
        }
    })
}

/// Runs [`provider_models`] on a private runtime.
///
/// # Errors
///
/// Returns [`DaemonError`] when the runtime fails or [`provider_models`] does.
pub fn provider_models_blocking(path: &Path, id: &str) -> Result<String, DaemonError> {
    block_on(provider_models(path, id))
}

/// Runs [`provider_test`] on a private runtime.
///
/// # Errors
///
/// Returns [`DaemonError`] when the runtime fails or [`provider_test`] does.
pub fn provider_test_blocking(path: &Path, id: &str) -> Result<TestReport, DaemonError> {
    block_on(provider_test(path, id))
}

fn block_on<T>(
    future: impl std::future::Future<Output = Result<T, DaemonError>>,
) -> Result<T, DaemonError> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(show)?;
    runtime.block_on(future)
}

fn load_config(path: &Path) -> Result<Config, DaemonError> {
    hofund_config::load_path(path).map_err(show)
}

fn find_provider<'a>(
    config: &'a Config,
    id: &str,
) -> Result<&'a hofund_config::Provider, DaemonError> {
    config
        .providers
        .iter()
        .find(|provider| provider.id == id)
        .ok_or_else(|| DaemonError::new(format!("no provider `{id}`")))
}

fn speakable(
    provider: &hofund_config::Provider,
    planned: &Planned,
) -> Result<Vec<hofund_provider::Resolved>, DaemonError> {
    let Some(preset) = provider.preset.as_deref() else {
        return Ok(vec![hofund_provider::Resolved {
            preset_id: String::new(),
            name: provider.id.clone(),
            wire: planned.wire,
            base_url: planned.base_url.clone(),
            credential: planned.credential,
        }]);
    };
    let wire = match provider.preset_wire {
        Some(hofund_config::PresetWire::Chat) => Some(Wire::Chat),
        Some(hofund_config::PresetWire::Responses) => Some(Wire::Responses),
        Some(hofund_config::PresetWire::Messages) => Some(Wire::Messages),
        Some(hofund_config::PresetWire::Generative) => Some(Wire::Generative),
        Some(hofund_config::PresetWire::Converse) => Some(Wire::Converse),
        None => None,
    };
    probe_wires(&hofund_provider::Resolve {
        id: preset,
        region: provider.region.as_deref(),
        wire,
        base_url: &provider.base_url,
        project: &provider.project,
        location: &provider.location,
    })
    .map_err(show)
}

async fn model_for_test(
    provider: &hofund_config::Provider,
    planned: &Planned,
    cache: &mut ModelCache,
    cache_path: &Path,
) -> Result<String, String> {
    let wire = wire_name(planned.wire);
    if let Some(ids) = cache.ok_ids(&provider.id, wire)
        && let Some(model) = ids.into_iter().find(|id| !id.is_empty())
    {
        return Ok(model);
    }
    let kind = model_list_kind(
        provider.preset.as_deref(),
        provider.region.as_deref(),
        &provider.location,
    )
    .map_err(|error| error.to_string())?;
    let (ids, live) = match kind {
        ModelListKind::Catalog(ids) => (ids, false),
        ModelListKind::Live => (
            fetch_live(provider, planned)
                .await
                .map_err(|error| error.to_string())?,
            true,
        ),
    };
    if live {
        cache.store_ok(&provider.id, wire, &ids, unix_now());
        cache.save(cache_path).map_err(|error| error.to_string())?;
    }
    ids.into_iter()
        .find(|id| !id.is_empty())
        .ok_or_else(|| "no model to try: refresh the model list".to_owned())
}

async fn fetch_live(
    provider: &hofund_config::Provider,
    planned: &Planned,
) -> Result<Vec<String>, DaemonError> {
    let api_key = hofund_secrets::resolve(&provider.api_key).map_err(show)?;
    let headers: Vec<(String, String)> = provider
        .headers
        .iter()
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect();
    let query = ListQuery {
        base_url: &planned.base_url,
        wire: planned.wire,
        credential: planned.credential,
        api_key: &api_key,
        headers: &headers,
        preset_id: provider.preset.as_deref().unwrap_or(""),
    };
    list_models(&query).await.map_err(show)
}

fn record_probe(ledger: &Ledger, upstream: &str, report: &ProbeReport) -> Result<(), DaemonError> {
    let status = report
        .status
        .map(|status| status.to_string())
        .unwrap_or_else(|| "error".to_owned());
    ledger
        .record(&Row {
            created_unix: unix_now(),
            model: report.model.clone(),
            upstream: upstream.to_owned(),
            input_tokens: report.input_tokens,
            output_tokens: report.output_tokens,
            latency_ms: report.latency_ms,
            status,
        })
        .map_err(show)
}

fn format_models(
    wire: &str,
    source: &str,
    ids: &[String],
    diff: Option<&hofund_provider::IdDiff>,
) -> String {
    let mut lines = vec![format!("{} models on {wire} from {source}", ids.len())];
    if let Some(diff) = diff {
        for id in &diff.added {
            lines.push(format!("+ {id}"));
        }
        for id in &diff.removed {
            lines.push(format!("- {id}"));
        }
    }
    lines.extend(ids.iter().cloned());
    format!("{}\n", lines.join("\n"))
}

fn format_probe(wire: Wire, report: &ProbeReport) -> String {
    let mark = if report.ok { "ok" } else { "fail" };
    if !report.sent {
        return format!("{} {mark} {}", wire_name(wire), report.error);
    }
    let status = report
        .status
        .map(|status| status.to_string())
        .unwrap_or_else(|| "-".to_owned());
    let mut line = format!(
        "{} {mark} {status} {}ms {}",
        wire_name(wire),
        report.latency_ms,
        report.model
    );
    if !report.error.is_empty() {
        line.push(' ');
        line.push_str(&report.error);
    }
    line
}

fn model_cache_path(config: &Path) -> PathBuf {
    beside(config, "hofund-models.json")
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| i64::try_from(duration.as_secs()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct ModelCache {
    #[serde(default)]
    providers: BTreeMap<String, ProviderCache>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct ProviderCache {
    #[serde(default)]
    wires: BTreeMap<String, WireCache>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct WireCache {
    ids: Vec<String>,
    fetched_unix: i64,
    attempt_unix: i64,
    attempt_ok: bool,
}

impl ModelCache {
    fn load(path: &Path) -> Result<Self, DaemonError> {
        match fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(|error| {
                DaemonError::new(format!("{} is not a model cache: {error}", path.display()))
            }),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(DaemonError::new(format!(
                "read {}: {error}",
                path.display()
            ))),
        }
    }

    fn save(&self, path: &Path) -> Result<(), DaemonError> {
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            fs::create_dir_all(parent).map_err(show)?;
        }
        let bytes = serde_json::to_vec_pretty(self).map_err(show)?;
        let temporary = path.with_extension("json.tmp");
        fs::write(&temporary, bytes).map_err(show)?;
        fs::rename(&temporary, path).map_err(show)?;
        Ok(())
    }

    fn entry(&self, id: &str, wire: &str) -> Option<&WireCache> {
        self.providers
            .get(id)
            .and_then(|provider| provider.wires.get(wire))
    }

    fn ok_ids(&self, id: &str, wire: &str) -> Option<Vec<String>> {
        self.entry(id, wire)
            .filter(|entry| entry.fetched_unix > 0)
            .map(|entry| entry.ids.clone())
    }

    fn store_ok(&mut self, id: &str, wire: &str, ids: &[String], now_unix: i64) {
        self.providers
            .entry(id.to_owned())
            .or_default()
            .wires
            .insert(
                wire.to_owned(),
                WireCache {
                    ids: ids.to_vec(),
                    fetched_unix: now_unix,
                    attempt_unix: now_unix,
                    attempt_ok: true,
                },
            );
    }

    fn store_fail(&mut self, id: &str, wire: &str, now_unix: i64) {
        let entry = self
            .providers
            .entry(id.to_owned())
            .or_default()
            .wires
            .entry(wire.to_owned())
            .or_insert(WireCache {
                ids: Vec::new(),
                fetched_unix: 0,
                attempt_unix: 0,
                attempt_ok: false,
            });
        entry.attempt_unix = now_unix;
        entry.attempt_ok = false;
    }
}
