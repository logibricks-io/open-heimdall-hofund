//! Runtime commands, each spoken through the control client.
//!
//! A command first selects its target with [`crate::connect::select`]. With
//! a running daemon the client talks to it; with no daemon the same client
//! calls start one-shot `hofundd` per request, so both paths print the same
//! output. `config reload`, `shutdown`, and `status` have their documented
//! offline forms instead.

use std::path::Path;
use std::time::Duration;

use hofund_control_client::hofund_control_proto::{
    AgentAction, AgentConnectRequest, AgentDisconnectRequest, AgentModelRequest, AgentPlanRequest,
    AgentState, AgentStatusEntry, ConfigApplyRequest, ConfigValidateRequest, FieldIssue,
    ReloadRequest, RouteQuery, ShutdownRequest, TokenRotateRequest, UsageResponse,
};
use hofund_control_client::{Client, ClientError, probe_model_health, resolve_config_id};

use crate::connect::{Target, one_shot_client, select};
use crate::local;

/// The bound on the model `/health` probe.
pub(crate) const HEALTHCHECK_TIMEOUT: Duration = Duration::from_secs(3);

/// What `config reload` prints with no daemon.
pub(crate) const RELOAD_NOOP: &str = "no daemon running; nothing to reload";

/// What `shutdown` prints with no daemon.
pub(crate) const SHUTDOWN_NOOP: &str = "no daemon running; nothing to shut down";

/// The client for a command that works without a daemon through one-shot.
pub(crate) async fn client(config: &Path) -> Result<Client, String> {
    match select(config).await? {
        Target::Daemon(client) => Ok(client),
        Target::NoDaemon => one_shot_client(config),
    }
}

/// The message for a client failure: the server's own message when it sent
/// one, with any field issues on their own lines.
pub(crate) fn failure(error: ClientError) -> String {
    match error {
        ClientError::Server { error, .. } => {
            let mut message = error.error.message;
            push_issues(&mut message, &error.error.details);
            message
        }
        other => other.to_string(),
    }
}

pub(crate) fn push_issues(out: &mut String, issues: &[FieldIssue]) {
    for issue in issues {
        out.push_str(&format!("\n  {}: {}", issue.field, issue.message));
    }
}

/// `hofund config check`: full validation, or the labeled structural check
/// when `hofundd` is not installed and no daemon runs.
pub(crate) async fn check(config: &Path) -> Result<String, String> {
    let text = std::fs::read_to_string(config)
        .map_err(|error| format!("{}: {error}", config.display()))?;
    let client = match select(config).await? {
        Target::Daemon(client) => client,
        Target::NoDaemon => match one_shot_client(config) {
            Ok(client) => client,
            Err(_) => return local::structural_check(config),
        },
    };
    let response = client
        .validate_config(ConfigValidateRequest::new(text))
        .await
        .map_err(|error| format!("{}: {}", config.display(), failure(error)))?;
    if response.valid {
        let mut out = format!("config ok: {}", config.display());
        for field in &response.pending_restart {
            out.push_str(&format!("\n  {field}: takes effect after a restart"));
        }
        return Ok(out);
    }
    let mut message = format!("{}: invalid config", config.display());
    push_issues(&mut message, &response.issues);
    Err(message)
}

/// `hofund config apply <file>`: replace the config with `source`'s text,
/// guarded by the disk revision read just before.
pub(crate) async fn apply(config: &Path, source: &Path) -> Result<String, String> {
    let text = std::fs::read_to_string(source)
        .map_err(|error| format!("{}: {error}", source.display()))?;
    let client = client(config).await?;
    let current = client.config().await.map_err(failure)?;
    let response = client
        .apply_config(ConfigApplyRequest::new(current.disk_revision, text))
        .await
        .map_err(failure)?;
    let mut out = format!(
        "config applied: {} (revision {})",
        config.display(),
        response.disk_revision
    );
    for field in &response.pending_restart {
        out.push_str(&format!("\n  {field}: takes effect after a restart"));
    }
    push_issues(&mut out, &response.not_applied);
    Ok(out)
}

/// `hofund config reload`: ask a running daemon to reread the file; with no
/// daemon there is nothing to reload.
pub(crate) async fn reload(config: &Path) -> Result<String, String> {
    let Target::Daemon(client) = select(config).await? else {
        return Ok(RELOAD_NOOP.to_owned());
    };
    let current = client.config().await.map_err(failure)?;
    let response = client
        .reload_config(ReloadRequest::new(current.disk_revision))
        .await
        .map_err(failure)?;
    let mut out = format!("config reloaded: generation {}", response.generation);
    for field in &response.pending_restart {
        out.push_str(&format!("\n  {field}: takes effect after a restart"));
    }
    push_issues(&mut out, &response.not_applied);
    Ok(out)
}

/// `hofund shutdown`: ask a running daemon to drain and stop.
pub(crate) async fn shutdown(config: &Path) -> Result<String, String> {
    let Target::Daemon(client) = select(config).await? else {
        return Ok(SHUTDOWN_NOOP.to_owned());
    };
    let response = client
        .shutdown(ShutdownRequest::new())
        .await
        .map_err(failure)?;
    Ok(if response.draining {
        "daemon is draining and will exit".to_owned()
    } else {
        "daemon did not start draining".to_owned()
    })
}

/// `hofund status`: the daemon's status, or a local report with no daemon.
pub(crate) async fn status(config: &Path) -> Result<String, String> {
    let client = match select(config).await? {
        Target::Daemon(client) => client,
        Target::NoDaemon => return offline_status(config).await,
    };
    let status = client.status().await.map_err(failure)?;
    let listener = &status.model_listener;
    let mut out = format!(
        "daemon running (pid {})\nconfig: {}\nmodel address: {} ({})",
        status.pid,
        config.display(),
        listener.running.as_deref().unwrap_or(&listener.effective),
        listen_source(listener.effective_source),
    );
    out.push_str(&format!(
        "\ngeneration: {}\ndisk revision: {}\nruntime revision: {}\nin flight: {}",
        status.generation, status.disk_revision, status.runtime_revision, status.in_flight
    ));
    for field in &status.pending_restart {
        out.push_str(&format!("\n  {field}: takes effect after a restart"));
    }
    push_issues(&mut out, &status.not_applied);
    Ok(out)
}

fn listen_source(
    source: hofund_control_client::hofund_control_proto::ListenSource,
) -> &'static str {
    use hofund_control_client::hofund_control_proto::ListenSource;
    match source {
        ListenSource::Environment => "environment",
        ListenSource::Config => "config",
        _ => "unknown",
    }
}

/// The offline status report: config path and id, disk revision, and the
/// effective model address with its source.
async fn offline_status(config: &Path) -> Result<String, String> {
    let id = resolve_config_id(config).map_err(|error| error.to_string())?;
    let env = local::listen_env();
    let (listen, source) = local::model_listen(config, env.as_deref())?;
    let revision = match one_shot_client(config) {
        Ok(client) => client.config().await.map_err(failure)?.disk_revision,
        Err(_) => "unknown (hofundd is unavailable)".to_owned(),
    };
    Ok(format!(
        "daemon not running\nconfig: {}\nconfig id: {id}\ndisk revision: {revision}\nmodel address: {listen} ({})",
        config.display(),
        local::source_label(source),
    ))
}

/// `hofund healthcheck`: GET the model listener's `/health` directly.
pub(crate) async fn healthcheck(config: &Path) -> Result<String, String> {
    let env = local::listen_env();
    let (listen, _) = local::model_listen(config, env.as_deref())?;
    probe_model_health(listen, HEALTHCHECK_TIMEOUT)
        .await
        .map(|()| {
            format!(
                "ok: hofund gateway at {}",
                hofund_control_client::probe_address(listen)
            )
        })
        .map_err(|error| error.to_string())
}

use crate::usage::{UsageOutputFormat, UsageReportOptions, options_to_query, usage_json};

/// `hofund usage`: usage table, CSV, or JSON from the control API.
pub(crate) async fn usage(config: &Path, options: UsageReportOptions) -> Result<String, String> {
    let client = client(config).await?;
    let query = options_to_query(&options);
    let response = client.usage(query).await.map_err(failure)?;
    match options.format {
        UsageOutputFormat::Json => usage_json(&response),
        UsageOutputFormat::Csv => usage_csv(&response),
        UsageOutputFormat::Table if response.breakdown_by.is_some() => {
            Ok(usage_breakdown_table(&response))
        }
        UsageOutputFormat::Table => Ok(usage_table(&response)),
    }
}

fn usage_csv(response: &UsageResponse) -> Result<String, String> {
    let rows: Vec<hofund_usage::Row> = response
        .rows
        .iter()
        .map(|row| hofund_usage::Row {
            created_unix: i64::try_from(row.created_at_ms / 1000).unwrap_or(0),
            model: row.model.clone(),
            upstream: row.provider.clone(),
            input_tokens: row.input_tokens.and_then(|v| i64::try_from(v).ok()),
            output_tokens: row.output_tokens.and_then(|v| i64::try_from(v).ok()),
            latency_ms: i64::try_from(row.duration_ms).unwrap_or(0),
            status: row.status.clone(),
            cache_read_tokens: row.cache_read_tokens.and_then(|v| i64::try_from(v).ok()),
            cache_write_5m_tokens: row
                .cache_write_5m_tokens
                .and_then(|v| i64::try_from(v).ok()),
            cache_write_1h_tokens: row
                .cache_write_1h_tokens
                .and_then(|v| i64::try_from(v).ok()),
            reasoning_tokens: row.reasoning_tokens.and_then(|v| i64::try_from(v).ok()),
            reasoning_hidden: row.reasoning_hidden,
            priced: row.priced,
            cost_usd: row.cost_usd.or(row.cost),
            caller_key_id: row.caller_key_id.clone(),
            caller_key_name: row.caller_key_name.clone(),
            caller_issuer_id: row.caller_issuer_id.clone(),
            provider_key_id: row.provider_key_id.clone(),
            outcome: row.outcome.clone(),
            client_agent: row.client_agent.clone(),
            ttft_ms: row.ttft_ms.and_then(|v| i64::try_from(v).ok()),
            session_id: row.session_id.clone(),
            served_model: row.served_model.clone(),
            flow_ms: None,
            first_text_ms: None,
            first_reasoning_ms: None,
            ..Default::default()
        })
        .collect();
    hofund_usage::format_csv(&rows).map_err(|error| error.to_string())
}

/// `hofund route`: the latest routing decisions from the daemon trace.
pub(crate) async fn route(config: &Path) -> Result<String, String> {
    let client = client(config).await?;
    let response = client
        .route_decisions(RouteQuery::default())
        .await
        .map_err(failure)?;
    let mut out = String::new();
    for row in response.decisions {
        out.push_str(&format!(
            "#{} seq={} asked={} served={}/{} status={:?}\n",
            row.id, row.seq, row.asked, row.served_provider, row.served_member, row.status,
        ));
        if let Some(moved) = &row.moved {
            out.push_str(&format!("  moved to {} ({})\n", moved.to, moved.why));
        }
        for candidate in &row.candidates {
            if candidate.skipped {
                out.push_str(&format!(
                    "  skip {}@{} order={}\n",
                    candidate.provider, candidate.member, candidate.order
                ));
            }
        }
        for try_row in &row.tries {
            if !try_row.fail.is_empty() {
                out.push_str(&format!(
                    "  try {}@{} status={:?} {}\n",
                    try_row.provider, try_row.member, try_row.status, try_row.fail
                ));
            }
        }
    }
    if out.is_empty() {
        out.push_str("no routing decisions recorded yet\n");
    }
    Ok(out)
}

/// Renders `response` in the fixed-column layout `hofund usage` has always
/// printed.
pub(crate) fn usage_table(response: &UsageResponse) -> String {
    let header = [
        "MODEL",
        "UPSTREAM",
        "IN",
        "OUT",
        "LATENCY_MS",
        "TTFT_MS",
        "TOKENS_S",
        "STATUS",
        "TIME",
    ];
    let body: Vec<[String; 9]> = response
        .rows
        .iter()
        .map(|row| {
            [
                row.model.clone(),
                row.provider.clone(),
                tokens(row.input_tokens),
                tokens(row.output_tokens),
                row.duration_ms.to_string(),
                row.ttft_ms.map(|ms| ms.to_string()).unwrap_or_default(),
                row.tokens_per_second
                    .map(|speed| format!("{speed:.1}"))
                    .unwrap_or_default(),
                row.status.clone(),
                local_stamp(row.created_at_ms),
            ]
        })
        .collect();
    let mut widths = header.map(str::len);
    for cells in &body {
        for (width, cell) in widths.iter_mut().zip(cells) {
            *width = (*width).max(cell.len());
        }
    }
    let mut out = String::new();
    push_line(&mut out, &header.map(str::to_owned), &widths);
    for cells in body {
        push_line(&mut out, &cells, &widths);
    }
    let totals = &response.totals;
    out.push_str(&format!(
        "totals  requests={}  in={}  out={}  cache_read={}  cache_write_5m={}  cache_write_1h={}  reasoning={}  cost={:.4} {}  unpriced={}  mean_ttft_ms={}  mean_tokens_per_second={}\n",
        totals.requests,
        totals.input_tokens,
        totals.output_tokens,
        totals.cache_read_tokens,
        totals.cache_write_5m_tokens,
        totals.cache_write_1h_tokens,
        totals.reasoning_tokens,
        totals.cost,
        response.currency,
        totals.unpriced,
        totals
            .mean_ttft_ms
            .map(|value| format!("{value:.0}"))
            .unwrap_or_else(|| "-".to_owned()),
        totals
            .mean_tokens_per_second
            .map(|value| format!("{value:.1}"))
            .unwrap_or_else(|| "-".to_owned()),
    ));
    if response.rows_truncated {
        out.push_str("(older rows omitted; totals cover every request)\n");
    }
    out
}

/// Renders grouped totals when the client asked for a breakdown dimension.
pub(crate) fn usage_breakdown_table(response: &UsageResponse) -> String {
    let by = response
        .breakdown_by
        .map(|dimension| dimension.as_query_value())
        .unwrap_or("unknown");
    let header = [
        "ID",
        "LABEL",
        "REQUESTS",
        "IN",
        "OUT",
        "COST",
        "UNPRICED",
        "MEAN_TTFT_MS",
        "MEAN_TOKENS_S",
    ];
    let body: Vec<[String; 9]> = response
        .breakdown
        .iter()
        .map(|group| {
            [
                group.id.clone(),
                group.label.clone(),
                group.requests.to_string(),
                group.input_tokens.to_string(),
                group.output_tokens.to_string(),
                format!("{:.4} {}", group.cost, response.currency),
                group.unpriced.to_string(),
                group
                    .mean_ttft_ms
                    .map(|value| format!("{value:.0}"))
                    .unwrap_or_else(|| "-".to_owned()),
                group
                    .mean_tokens_per_second
                    .map(|value| format!("{value:.1}"))
                    .unwrap_or_else(|| "-".to_owned()),
            ]
        })
        .collect();
    let mut widths = header.map(str::len);
    for cells in &body {
        for (width, cell) in widths.iter_mut().zip(cells) {
            *width = (*width).max(cell.len());
        }
    }
    let mut out = format!("breakdown by {by}\n");
    push_line(&mut out, &header.map(str::to_owned), &widths);
    for cells in body {
        push_line(&mut out, &cells, &widths);
    }
    let totals = &response.totals;
    out.push_str(&format!(
        "totals  requests={}  in={}  out={}  cost={:.4} {}  unpriced={}\n",
        totals.requests,
        totals.input_tokens,
        totals.output_tokens,
        totals.cost,
        response.currency,
        totals.unpriced,
    ));
    out
}

fn tokens(value: Option<u64>) -> String {
    value.map_or_else(|| "-".to_owned(), |value| value.to_string())
}

fn local_stamp(unix_ms: u64) -> String {
    i64::try_from(unix_ms / 1000)
        .ok()
        .and_then(|secs| chrono::DateTime::<chrono::Utc>::from_timestamp(secs, 0))
        .map_or_else(
            || (unix_ms / 1000).to_string(),
            |utc| {
                utc.with_timezone(&chrono::Local)
                    .format("%Y-%m-%d %H:%M:%S")
                    .to_string()
            },
        )
}

fn push_line(out: &mut String, cells: &[String], widths: &[usize]) {
    for (index, (cell, width)) in cells.iter().zip(widths).enumerate() {
        if index > 0 {
            out.push_str("  ");
        }
        out.push_str(cell);
        if index + 1 < cells.len() {
            out.extend(std::iter::repeat_n(' ', width.saturating_sub(cell.len())));
        }
    }
    out.push('\n');
}

/// `hofund agent list`.
pub(crate) async fn agent_list(config: &Path) -> Result<String, String> {
    let client = client(config).await?;
    let response = client.agents().await.map_err(failure)?;
    let mut out = String::from("id\tname\tinstalled");
    for agent in &response.agents {
        let installed = if agent.installed { "yes" } else { "no" };
        out.push_str(&format!(
            "\n{}\t{}\t{installed}",
            agent.id, agent.display_name
        ));
    }
    Ok(out)
}

/// `hofund agent status [<agent>]`.
pub(crate) async fn agent_status(config: &Path, only: Option<&str>) -> Result<String, String> {
    let client = client(config).await?;
    let response = client.agent_status().await.map_err(failure)?;
    let rows: Vec<_> = response
        .agents
        .iter()
        .filter(|entry| only.is_none_or(|id| entry.id == id))
        .collect();
    if let Some(id) = only
        && rows.is_empty()
    {
        return Err(format!("unknown agent `{id}`"));
    }
    Ok(agent_status_table(&rows))
}

/// The `hofund agent status` table, then one `warning:` line per warning.
fn agent_status_table(rows: &[&AgentStatusEntry]) -> String {
    let mut out = String::from("id\tstate\tmodel");
    let mut warnings = String::new();
    for entry in rows {
        for warning in &entry.warnings {
            warnings.push_str(&format!("\nwarning: {}: {warning}", entry.id));
        }
        let state = match entry.state {
            AgentState::Connected => "connected",
            AgentState::Disconnected => "disconnected",
            AgentState::Drifted => "drifted",
            _ => "unknown",
        };
        out.push_str(&format!(
            "\n{}\t{state}\t{}",
            entry.id,
            entry.model.as_deref().unwrap_or("-")
        ));
    }
    out.push_str(&warnings);
    out
}

/// `hofund agent connect <name> [--dry-run]`: plan, then connect against
/// that plan's revision so a file changed in between is not overwritten.
pub(crate) async fn agent_connect(
    config: &Path,
    name: &str,
    dry_run: bool,
) -> Result<String, String> {
    let client = client(config).await?;
    let plan = client
        .plan_agent(name, AgentPlanRequest::new(AgentAction::Connect, false))
        .await
        .map_err(failure)?;
    let mut out = String::new();
    if dry_run {
        if plan.changes.is_empty() {
            out.push_str(&format!("{name}: already matches\n"));
        }
        for change in &plan.changes {
            // The daemon already replaced the token with [redacted].
            out.push_str(&change.diff);
        }
    } else {
        let response = client
            .connect_agent(name, AgentConnectRequest::new(Some(plan.plan_revision)))
            .await
            .map_err(failure)?;
        if response.changed_paths.is_empty() {
            out.push_str(&format!("{name}: already matches\n"));
        }
        for path in &response.changed_paths {
            out.push_str(&format!("{name}: wrote {path}\n"));
        }
        for warning in &response.warnings {
            eprintln!("warning: {warning}");
        }
    }
    if name == "opencode" {
        let models = client.route_models().await.map_err(failure)?;
        out.push_str(&opencode_models_note(models.models.len()));
        out.push('\n');
    }
    Ok(out.trim_end_matches('\n').to_owned())
}

/// What `hofund agent connect opencode` prints about the models object.
pub(crate) fn opencode_models_note(model_count: usize) -> String {
    if model_count == 0 {
        "opencode: models stays {}. The config has no routes, and this build does not invent a model catalog.".to_owned()
    } else {
        format!(
            "opencode: models lists {model_count} route name(s), each with name and an empty variants object. Modalities, context limits, and reasoning levels are not invented."
        )
    }
}

/// `hofund agent disconnect <name> [--force]`.
pub(crate) async fn agent_disconnect(
    config: &Path,
    name: &str,
    force: bool,
) -> Result<String, String> {
    let client = client(config).await?;
    let response = client
        .disconnect_agent(name, AgentDisconnectRequest::new(force, None))
        .await
        .map_err(failure)?;
    for warning in &response.warnings {
        eprintln!("{warning}");
    }
    Ok(if response.changed_paths.is_empty() {
        format!("{name}: nothing to restore")
    } else {
        format!("{name}: restored")
    })
}

/// `hofund agent set <name> model=<id|default>`: the daemon (or one-shot)
/// sets or clears the model and connects the agent when setting one.
pub(crate) async fn agent_set(
    config: &Path,
    name: &str,
    model: Option<&str>,
) -> Result<String, String> {
    let client = client(config).await?;
    let response = client
        .set_agent_model(name, AgentModelRequest::new(model.map(str::to_owned)))
        .await
        .map_err(failure)?;
    for warning in &response.warnings {
        eprintln!("{warning}");
    }
    let mut out = format!("{name}: model {}", model.unwrap_or("default"));
    if response.changed_paths.is_empty() {
        out.push_str(" (unchanged)");
    }
    Ok(out)
}

/// `hofund token rotate`: the daemon (or one-shot) rotates the token and
/// rewrites connected agents.
pub(crate) async fn token_rotate(config: &Path) -> Result<String, String> {
    let client = client(config).await?;
    let response = client
        .rotate_token(TokenRotateRequest::new())
        .await
        .map_err(failure)?;
    let mut out = response.token.clone();
    let mut failed = Vec::new();
    for agent in &response.agents {
        if agent.ok {
            out.push_str(&format!("\n{}: updated", agent.id));
        } else {
            failed.push(agent);
        }
    }
    eprintln!("a running gateway reads the token file on the next request");
    if failed.is_empty() {
        return Ok(out);
    }
    println!("{out}");
    let mut message = String::new();
    for agent in failed {
        message.push_str(&format!(
            "{}: {}\nrun: hofund agent connect {}\n",
            agent.id,
            agent.message.as_deref().unwrap_or("refresh failed"),
            agent.id
        ));
    }
    message.push_str(
        "the token file was rotated; a connected agent still needs `hofund agent connect`",
    );
    Err(message)
}

#[cfg(test)]
mod tests {
    use super::{
        agent_status_table, opencode_models_note, tokens, usage_breakdown_table, usage_table,
    };
    use hofund_control_client::hofund_control_proto::{
        AgentState, AgentStatusEntry, UsageBreakdownBy, UsageBreakdownGroup, UsageResponse,
        UsageRow,
    };

    /// #463: store warnings print under the table, one line each.
    #[test]
    fn agent_status_prints_warnings_under_the_table() {
        let mut zed = AgentStatusEntry::default();
        zed.id = "zed".to_owned();
        zed.state = AgentState::Disconnected;
        zed.warnings = vec!["store locked".to_owned(), "second".to_owned()];
        let mut codex = AgentStatusEntry::default();
        codex.id = "codex".to_owned();
        codex.state = AgentState::Connected;
        codex.model = Some("gpt".to_owned());
        assert_eq!(
            agent_status_table(&[&zed, &codex]),
            "id\tstate\tmodel\nzed\tdisconnected\t-\ncodex\tconnected\tgpt\nwarning: zed: store locked\nwarning: zed: second"
        );
        assert_eq!(
            agent_status_table(&[&codex]),
            "id\tstate\tmodel\ncodex\tconnected\tgpt"
        );
    }

    #[test]
    fn u1b_13_cli_usage_totals_show_cost_currency_and_unpriced() {
        let mut response = UsageResponse::default();
        let mut row = UsageRow::default();
        row.model = "m".to_owned();
        row.provider = "p".to_owned();
        row.input_tokens = Some(3);
        row.output_tokens = None;
        row.duration_ms = 12;
        row.ttft_ms = Some(4);
        row.tokens_per_second = Some(25.0);
        row.status = "200".to_owned();
        row.created_at_ms = 1_700_000_000_000;
        response.rows.push(row);
        response.totals.requests = 1;
        response.totals.input_tokens = 3;
        let table = usage_table(&response);
        let mut lines = table.lines();
        assert_eq!(
            lines.next(),
            Some("MODEL  UPSTREAM  IN  OUT  LATENCY_MS  TTFT_MS  TOKENS_S  STATUS  TIME")
        );
        let row = lines.next().expect("row");
        assert!(
            row.starts_with("m      p         3   -    12          4        25.0      200     "),
            "{row:?}"
        );
        assert_eq!(
            lines.next(),
            Some(
                "totals  requests=1  in=3  out=0  cache_read=0  cache_write_5m=0  cache_write_1h=0  reasoning=0  cost=0.0000 USD  unpriced=0  mean_ttft_ms=-  mean_tokens_per_second=-"
            )
        );
        assert_eq!(lines.next(), None);
        response.rows_truncated = true;
        assert!(usage_table(&response).contains("older rows omitted"));
        assert_eq!(tokens(None), "-");
        assert_eq!(tokens(Some(0)), "0");
    }

    #[test]
    fn u1c_14_cli_usage_breakdown_table_renders_groups() {
        let mut response = UsageResponse::default();
        response.breakdown_by = Some(UsageBreakdownBy::ProviderKey);
        let mut group = UsageBreakdownGroup::default();
        group.id = "aabbccddee".to_owned();
        group.label = "…ddee".to_owned();
        group.requests = 2;
        group.input_tokens = 30;
        group.output_tokens = 4;
        group.cost = 1.25;
        group.unpriced = 1;
        response.breakdown.push(group);
        response.totals.requests = 2;
        response.totals.input_tokens = 30;
        response.totals.output_tokens = 4;
        response.totals.cost = 1.25;
        response.totals.unpriced = 1;
        let table = usage_breakdown_table(&response);
        assert!(table.starts_with("breakdown by provider-key\n"));
        assert!(table.contains("aabbccddee"));
        assert!(table.contains("…ddee"));
        assert!(table.contains("totals  requests=2"));
        assert!(table.contains("cost=1.2500 USD"));
    }

    #[test]
    fn opencode_note_names_the_route_count() {
        assert!(opencode_models_note(0).contains("models stays {}"));
        assert!(opencode_models_note(2).contains("2 route"));
    }
}
