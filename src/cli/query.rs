use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Result, bail};
use clap::{Args, Subcommand};
use serde_json::{Value, json};

use super::auth::http_agent;
use super::cloud::{
    CONTROL_TIMEOUT, fetch_projects, get_json_authed, post_json_authed, resolve_target,
};

#[derive(Args)]
pub struct QueryArgs {
    #[command(subcommand)]
    pub command: QueryCommand,
}

#[derive(Subcommand)]
pub enum QueryCommand {
    /// Query aggregated fleet flame graph / top functions.
    Flame {
        #[command(flatten)]
        scope: QueryScopeArgs,

        /// Invert call stacks (callee to caller).
        #[arg(long)]
        invert: bool,

        /// Print indented call tree instead of a flat top-functions table.
        #[arg(long)]
        tree: bool,

        /// Maximum rows to print in human output.
        #[arg(long, default_value_t = 30)]
        limit: usize,

        /// Minimum total share percentage to include in human output.
        #[arg(long, default_value_t = 0.5)]
        min_percent: f64,
    },

    /// Query CPU over time grouped by a dimension.
    OverTime {
        #[command(flatten)]
        scope: QueryScopeArgs,

        /// Dimension to group series by.
        #[arg(long, default_value = "service.version")]
        group_by: String,
    },

    /// Query per-machine CPU usage and anomaly ranking.
    ByMachine {
        #[command(flatten)]
        scope: QueryScopeArgs,

        /// Maximum machines to print in human output.
        #[arg(long, default_value_t = 30)]
        limit: usize,
    },

    /// Query CPU breakdown by a dimension.
    Breakdown {
        #[command(flatten)]
        scope: QueryScopeArgs,

        /// Dimension to break down by (e.g. service.name, process.name).
        #[arg(long, default_value = "service.name")]
        dimension: String,

        /// Maximum rows to print in human output.
        #[arg(long, default_value_t = 30)]
        limit: usize,
    },

    /// List available projects, machines, and dimensions in scope.
    Dimensions {
        #[command(flatten)]
        scope: QueryScopeArgs,
    },
}

#[derive(Args, Clone)]
pub struct QueryScopeArgs {
    /// Microscope server URL (defaults to `MICROSCOPE_SERVER` or logged-in server).
    #[arg(long, value_name = "URL")]
    pub server: Option<String>,

    /// Personal API token (msu_...).
    #[arg(long, value_name = "TOKEN")]
    pub token: Option<String>,

    /// Extra PEM root certificates for the server.
    #[arg(long, value_name = "FILE")]
    pub ca_file: Option<PathBuf>,

    /// Organization ID.
    #[arg(long, value_name = "ORG_ID")]
    pub org: Option<i64>,

    /// Project name or numeric ID. May be repeated.
    #[arg(short = 'p', long = "project", value_name = "PROJECT")]
    pub projects: Vec<String>,

    /// Filter expression: DIM=VAL to include or DIM!=VAL to exclude. May be repeated.
    #[arg(short = 'f', long = "filter", value_name = "DIM=VAL")]
    pub filters: Vec<String>,

    /// Relative lookback window (e.g. 15m, 1h, 24h, 7d).
    #[arg(long, value_name = "DURATION", conflicts_with_all = ["from", "to"])]
    pub last: Option<String>,

    /// Absolute window start (unix nanoseconds or unix seconds).
    #[arg(long, value_name = "TIMESTAMP")]
    pub from: Option<String>,

    /// Absolute window end (unix nanoseconds or unix seconds).
    #[arg(long, value_name = "TIMESTAMP")]
    pub to: Option<String>,

    /// Recorded session ID to query (reuses the session's pinned data and scope).
    #[arg(long, value_name = "SESSION_ID")]
    pub session: Option<i64>,

    /// Compare mode: previous-period or previous-version.
    #[arg(long, value_name = "MODE")]
    pub compare: Option<String>,

    /// Flush live agents before querying.
    #[arg(long)]
    pub fresh: bool,

    /// Emit raw API JSON.
    #[arg(long)]
    pub json: bool,
}

pub fn run(args: &QueryArgs) -> ExitCode {
    match execute(args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error:#}");
            ExitCode::FAILURE
        }
    }
}

#[expect(
    clippy::too_many_lines,
    reason = "dispatches all five query subcommands"
)]
fn execute(args: &QueryArgs) -> Result<()> {
    let scope_args = match &args.command {
        QueryCommand::Flame { scope, .. }
        | QueryCommand::OverTime { scope, .. }
        | QueryCommand::ByMachine { scope, .. }
        | QueryCommand::Breakdown { scope, .. }
        | QueryCommand::Dimensions { scope } => scope,
    };

    let (server, token) =
        resolve_target(scope_args.server.as_deref(), scope_args.token.as_deref())?;
    let agent = http_agent(CONTROL_TIMEOUT, scope_args.ca_file.as_deref())?;
    let scope = build_scope(&agent, &server, &token, scope_args)?;

    match &args.command {
        QueryCommand::Flame {
            invert,
            tree,
            limit,
            min_percent,
            ..
        } => {
            let mut body = serde_json::Map::new();
            body.insert("scope".into(), scope);
            body.insert("invert".into(), json!(*invert));
            body.insert("fresh".into(), json!(scope_args.fresh));
            if let Some(session_id) = scope_args.session {
                body.insert("session_id".into(), json!(session_id));
            }
            let resp = post_json_authed(
                &agent,
                &server,
                &token,
                "/api/query/flame",
                &Value::Object(body),
            )?;
            if scope_args.json {
                println!("{}", serde_json::to_string_pretty(&resp)?);
            } else {
                print_flame(&resp, *tree, *limit, *min_percent);
            }
        }
        QueryCommand::OverTime { group_by, .. } => {
            let mut body = serde_json::Map::new();
            body.insert("scope".into(), scope);
            body.insert("group_by".into(), json!(group_by));
            body.insert("fresh".into(), json!(scope_args.fresh));
            if let Some(session_id) = scope_args.session {
                body.insert("session_id".into(), json!(session_id));
            }
            let resp = post_json_authed(
                &agent,
                &server,
                &token,
                "/api/query/over-time",
                &Value::Object(body),
            )?;
            if scope_args.json {
                println!("{}", serde_json::to_string_pretty(&resp)?);
            } else {
                print_over_time(&resp);
            }
        }
        QueryCommand::ByMachine { limit, .. } => {
            let mut body = serde_json::Map::new();
            body.insert("scope".into(), scope);
            body.insert("fresh".into(), json!(scope_args.fresh));
            if let Some(session_id) = scope_args.session {
                body.insert("session_id".into(), json!(session_id));
            }
            let resp = post_json_authed(
                &agent,
                &server,
                &token,
                "/api/query/by-machine",
                &Value::Object(body),
            )?;
            if scope_args.json {
                println!("{}", serde_json::to_string_pretty(&resp)?);
            } else {
                print_by_machine(&resp, *limit);
            }
        }
        QueryCommand::Breakdown {
            dimension, limit, ..
        } => {
            let mut body = serde_json::Map::new();
            body.insert("scope".into(), scope);
            body.insert("dimension".into(), json!(dimension));
            body.insert("fresh".into(), json!(scope_args.fresh));
            if let Some(session_id) = scope_args.session {
                body.insert("session_id".into(), json!(session_id));
            }
            let resp = post_json_authed(
                &agent,
                &server,
                &token,
                "/api/query/breakdown",
                &Value::Object(body),
            )?;
            if scope_args.json {
                println!("{}", serde_json::to_string_pretty(&resp)?);
            } else {
                print_breakdown(&resp, *limit);
            }
        }
        QueryCommand::Dimensions { .. } => {
            let mut body = serde_json::Map::new();
            body.insert("scope".into(), scope);
            body.insert("fresh".into(), json!(scope_args.fresh));
            if let Some(session_id) = scope_args.session {
                body.insert("session_id".into(), json!(session_id));
            }
            let resp = post_json_authed(
                &agent,
                &server,
                &token,
                "/api/query/dimensions",
                &Value::Object(body),
            )?;
            if scope_args.json {
                println!("{}", serde_json::to_string_pretty(&resp)?);
            } else {
                print_dimensions(&resp);
            }
        }
    }

    Ok(())
}

fn build_scope(
    agent: &ureq::Agent,
    server: &microscope::agent::wire::Server,
    token: &str,
    args: &QueryScopeArgs,
) -> Result<Value> {
    let mut base_scope = if let Some(session_id) = args.session {
        let sess = get_json_authed(agent, server, token, &format!("/api/sessions/{session_id}"))?;
        if sess.get("locked").and_then(Value::as_bool) == Some(true) {
            bail!("session {session_id} is locked; you lack project read access");
        }
        sess.get("scope")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default()
    } else {
        serde_json::Map::new()
    };

    if let Some(org_id) = args.org {
        base_scope.insert("org_id".into(), json!(org_id));
    }

    if !args.projects.is_empty() {
        let project_ids = resolve_project_ids(agent, server, token, args.org, &args.projects)?;
        base_scope.insert("project_ids".into(), json!(project_ids));
    }

    if !args.filters.is_empty() {
        let mut parsed = Vec::with_capacity(args.filters.len());
        for raw in &args.filters {
            parsed.push(parse_filter(raw)?);
        }
        base_scope.insert("filters".into(), json!(parsed));
    }

    if args.from.is_some() || args.to.is_some() {
        let now_ns = unix_now_ns();
        let end_time = match args.to.as_deref() {
            Some(raw) => parse_timestamp_ns(raw)?,
            None => now_ns,
        };
        let start_time = match args.from.as_deref() {
            Some(raw) => parse_timestamp_ns(raw)?,
            None => end_time.saturating_sub(3600_i64.saturating_mul(1_000_000_000)),
        };
        if start_time >= end_time {
            bail!("--from must be earlier than --to");
        }
        base_scope.insert(
            "time_range".into(),
            json!({
                "kind": "absolute",
                "start_time": start_time,
                "end_time": end_time,
            }),
        );
    } else if let Some(last) = args.last.as_deref() {
        let duration_secs = parse_duration_secs(last)?;
        base_scope.insert(
            "time_range".into(),
            json!({
                "kind": "relative",
                "duration_secs": duration_secs,
            }),
        );
    } else if !base_scope.contains_key("time_range") {
        base_scope.insert(
            "time_range".into(),
            json!({
                "kind": "relative",
                "duration_secs": 3600_i64,
            }),
        );
    }

    if let Some(compare) = args.compare.as_deref() {
        let compare_val = match compare {
            "previous-period" | "previous_period" => json!({ "kind": "previous_period" }),
            "previous-version" | "previous_version" => json!({ "kind": "previous_version" }),
            other => bail!(
                "unsupported --compare value {other:?}; use previous-period or previous-version"
            ),
        };
        base_scope.insert("compare".into(), compare_val);
    }

    Ok(Value::Object(base_scope))
}

fn resolve_project_ids(
    agent: &ureq::Agent,
    server: &microscope::agent::wire::Server,
    token: &str,
    org_id: Option<i64>,
    names_or_ids: &[String],
) -> Result<Vec<i64>> {
    let mut ids = Vec::with_capacity(names_or_ids.len());
    let mut need_lookup = false;
    for raw in names_or_ids {
        if let Ok(id) = raw.trim().parse::<i64>() {
            ids.push(id);
        } else {
            need_lookup = true;
        }
    }
    if !need_lookup {
        return Ok(ids);
    }

    let projects = fetch_projects(agent, server, token)?;
    ids.clear();
    for raw in names_or_ids {
        let trimmed = raw.trim();
        if let Ok(id) = trimmed.parse::<i64>() {
            ids.push(id);
            continue;
        }
        let matches: Vec<i64> = projects
            .iter()
            .filter(|p| {
                p.name.eq_ignore_ascii_case(trimmed)
                    && org_id.is_none_or(|oid| p.org_id() == Some(oid))
            })
            .map(|p| p.id)
            .collect();
        match matches.as_slice() {
            [only] => ids.push(*only),
            [] => bail!("no readable project named {trimmed:?}"),
            _ => bail!("multiple projects match {trimmed:?}; pass --org or numeric project ID"),
        }
    }
    Ok(ids)
}

pub fn parse_duration_secs(raw: &str) -> Result<i64> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        bail!("duration cannot be empty");
    }
    let (num_str, mult) = match trimmed.as_bytes().last() {
        Some(b's') => (&trimmed[..trimmed.len() - 1], 1_i64),
        Some(b'm') => (&trimmed[..trimmed.len() - 1], 60_i64),
        Some(b'h') => (&trimmed[..trimmed.len() - 1], 3600_i64),
        Some(b'd') => (&trimmed[..trimmed.len() - 1], 86_400_i64),
        _ => (trimmed, 1_i64),
    };
    let n: i64 = num_str
        .parse()
        .map_err(|_| anyhow::anyhow!("invalid duration {raw:?}; examples: 15m, 1h, 24h, 7d"))?;
    if n <= 0 {
        bail!("duration must be positive");
    }
    n.checked_mul(mult)
        .ok_or_else(|| anyhow::anyhow!("duration overflow: {raw:?}"))
}

pub fn parse_timestamp_ns(raw: &str) -> Result<i64> {
    let trimmed = raw.trim();
    if let Ok(n) = trimmed.parse::<i64>() {
        return Ok(if n.abs() < 10_000_000_000 {
            n.saturating_mul(1_000_000_000)
        } else if n.abs() < 10_000_000_000_000 {
            n.saturating_mul(1_000_000)
        } else {
            n
        });
    }
    bail!("invalid timestamp {raw:?}; pass unix seconds or unix nanoseconds")
}

pub fn parse_filter(raw: &str) -> Result<Value> {
    if let Some((dim, val)) = raw.split_once("!=") {
        let dim = dim.trim();
        let val = val.trim();
        if dim.is_empty() || val.is_empty() {
            bail!("invalid filter {raw:?}; expected DIM!=VAL");
        }
        return Ok(json!({
            "dimension": dim,
            "value": val,
            "exclude": true,
        }));
    }
    if let Some((dim, val)) = raw.split_once('=') {
        let dim = dim.trim();
        let val = val.trim();
        if dim.is_empty() || val.is_empty() {
            bail!("invalid filter {raw:?}; expected DIM=VAL");
        }
        return Ok(json!({
            "dimension": dim,
            "value": val,
            "exclude": false,
        }));
    }
    bail!("invalid filter {raw:?}; expected DIM=VAL or DIM!=VAL")
}

fn unix_now_ns() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_nanos()).unwrap_or(i64::MAX))
}

#[expect(
    clippy::cast_precision_loss,
    clippy::too_many_lines,
    reason = "flame summary formatting handles tree and flat modes"
)]
fn print_flame(resp: &Value, tree: bool, limit: usize, min_percent: f64) {
    if resp.get("locked").and_then(Value::as_bool) == Some(true) {
        println!("Scope is locked (missing read permission on one or more projects).");
        return;
    }
    let total_weight = resp.get("weight").and_then(Value::as_u64).unwrap_or(0) as f64;
    let samples = resp.get("samples").and_then(Value::as_u64).unwrap_or(0);
    if total_weight <= 0.0 {
        println!("No samples in scope.");
        return;
    }

    let keys = resp
        .get("keys")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let own = resp
        .get("own")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let total = resp
        .get("total")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let key_idx = resp
        .get("key")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let depth = resp
        .get("depth")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    let key_name = |idx: usize| -> String {
        keys.get(idx)
            .and_then(|k| k.get("name"))
            .and_then(Value::as_str)
            .unwrap_or("?")
            .to_owned()
    };

    println!(
        "Total: {} ({samples} samples, {} blocks)",
        format_ns(total_weight),
        resp.get("blocks").and_then(Value::as_u64).unwrap_or(0),
    );

    if tree {
        println!("{:>7}  {:>7}  Function", "Total%", "Own%");
        let mut shown = 0_usize;
        for i in 0..key_idx.len() {
            if shown >= limit {
                break;
            }
            let tot = total.get(i).and_then(Value::as_f64).unwrap_or(0.0);
            let tot_pct = tot * 100.0 / total_weight;
            if tot_pct < min_percent {
                continue;
            }
            let own_v = own.get(i).and_then(Value::as_f64).unwrap_or(0.0);
            let own_pct = own_v * 100.0 / total_weight;
            let d = usize::try_from(depth.get(i).and_then(Value::as_u64).unwrap_or(0))
                .unwrap_or(0)
                .min(24);
            let k =
                usize::try_from(key_idx.get(i).and_then(Value::as_u64).unwrap_or(0)).unwrap_or(0);
            println!(
                "{tot_pct:>6.2}%  {own_pct:>6.2}%  {}{}",
                "  ".repeat(d),
                key_name(k)
            );
            shown += 1;
        }
        return;
    }

    let mut by_fn: BTreeMap<String, (f64, f64)> = BTreeMap::new();
    for i in 0..key_idx.len() {
        let k = usize::try_from(key_idx.get(i).and_then(Value::as_u64).unwrap_or(0)).unwrap_or(0);
        let name = key_name(k);
        let own_v = own.get(i).and_then(Value::as_f64).unwrap_or(0.0);
        let tot_v = total.get(i).and_then(Value::as_f64).unwrap_or(0.0);
        let entry = by_fn.entry(name).or_insert((0.0, 0.0));
        entry.0 += own_v;
        if tot_v > entry.1 {
            entry.1 = tot_v;
        }
    }

    let mut rows: Vec<(String, f64, f64)> = by_fn
        .into_iter()
        .map(|(name, (o, t))| (name, o, t))
        .collect();
    rows.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| b.2.total_cmp(&a.2)));

    println!("{:>7}  {:>7}  Function", "Own%", "Total%");
    for (name, o, t) in rows.into_iter().take(limit) {
        let tot_pct = t * 100.0 / total_weight;
        if tot_pct < min_percent {
            continue;
        }
        let own_pct = o * 100.0 / total_weight;
        println!("{own_pct:>6.2}%  {tot_pct:>6.2}%  {name}");
    }
}

fn print_over_time(resp: &Value) {
    let group_by = resp
        .get("group_by")
        .and_then(Value::as_str)
        .unwrap_or("all");
    let series = resp
        .get("series")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if series.is_empty() {
        println!("No time-series data in scope.");
        return;
    }
    println!("Grouped by {group_by} ({} series):", series.len());
    println!("{:<32}  {:>12}  {:>12}", "Series", "Total", "Peak");
    for s in series {
        let label = s.get("label").and_then(Value::as_str).unwrap_or("(other)");
        let values = s
            .get("values")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let sum: f64 = values.iter().filter_map(Value::as_f64).sum();
        let peak: f64 = values
            .iter()
            .filter_map(Value::as_f64)
            .fold(0.0_f64, f64::max);
        println!(
            "{label:<32}  {:>12}  {:>12}",
            format_ns(sum),
            format_ns(peak)
        );
    }
}

fn print_by_machine(resp: &Value, limit: usize) {
    let machines = resp
        .get("machines")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if machines.is_empty() {
        println!("No machines in scope.");
        return;
    }
    println!(
        "{:<28}  {:<12}  {:>12}  {:>8}",
        "Hostname", "Environment", "CPU", "Ratio"
    );
    for m in machines.into_iter().take(limit) {
        let host = m.get("hostname").and_then(Value::as_str).unwrap_or("?");
        let env = m.get("environment").and_then(Value::as_str).unwrap_or("");
        let total = m.get("total").and_then(Value::as_f64).unwrap_or(0.0);
        let ratio = m.get("ratio").and_then(Value::as_f64).unwrap_or(0.0);
        println!(
            "{host:<28}  {env:<12}  {:>12}  {ratio:>7.2}x",
            format_ns(total)
        );
    }
}

fn print_breakdown(resp: &Value, limit: usize) {
    let dim = resp
        .get("dimension")
        .and_then(Value::as_str)
        .unwrap_or("dimension");
    let total = resp.get("total").and_then(Value::as_f64).unwrap_or(0.0);
    let rows = resp
        .get("rows")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if rows.is_empty() {
        println!("No breakdown data for {dim}.");
        return;
    }
    println!("{:>7}  {:>12}  {dim}", "Share%", "CPU");
    for row in rows.into_iter().take(limit) {
        let value = row.get("value").and_then(Value::as_str).unwrap_or("(none)");
        let cpu = row.get("total").and_then(Value::as_f64).unwrap_or(0.0);
        let pct = if total > 0.0 {
            cpu * 100.0 / total
        } else {
            0.0
        };
        println!("{pct:>6.2}%  {:>12}  {value}", format_ns(cpu));
    }
}

fn print_dimensions(resp: &Value) {
    if let Some(projects) = resp.get("projects").and_then(Value::as_array) {
        println!("Projects ({}):", projects.len());
        for p in projects {
            let id = p.get("id").and_then(Value::as_i64).unwrap_or(0);
            let name = p.get("name").and_then(Value::as_str).unwrap_or("?");
            println!("  {id}: {name}");
        }
    }
    if let Some(machines) = resp.get("machines").and_then(Value::as_array) {
        println!("Machines ({}):", machines.len());
        for m in machines {
            let host = m.get("hostname").and_then(Value::as_str).unwrap_or("?");
            let env = m.get("environment").and_then(Value::as_str).unwrap_or("");
            println!("  {host} ({env})");
        }
    }
    for section in ["indexed", "adhoc"] {
        if let Some(map) = resp.get(section).and_then(Value::as_object) {
            for (dim, vals) in map {
                let items: Vec<&str> = vals
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .collect();
                if !items.is_empty() {
                    println!("{dim}: {}", items.join(", "));
                }
            }
        }
    }
}

fn format_ns(ns: f64) -> String {
    if ns >= 1e9 {
        format!("{:.2}s", ns / 1e9)
    } else if ns >= 1e6 {
        format!("{:.1}ms", ns / 1e6)
    } else if ns >= 1e3 {
        format!("{:.1}us", ns / 1e3)
    } else {
        format!("{ns:.0}ns")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_durations_and_filters() {
        assert_eq!(parse_duration_secs("30s").unwrap(), 30);
        assert_eq!(parse_duration_secs("15m").unwrap(), 900);
        assert_eq!(parse_duration_secs("2h").unwrap(), 7200);
        assert_eq!(parse_duration_secs("7d").unwrap(), 604_800);
        assert!(parse_duration_secs("0h").is_err());

        let inc = parse_filter("service.name=checkout").unwrap();
        assert_eq!(inc["dimension"], "service.name");
        assert_eq!(inc["value"], "checkout");
        assert_eq!(inc["exclude"], false);

        let exc = parse_filter("host.name!=canary-1").unwrap();
        assert_eq!(exc["dimension"], "host.name");
        assert_eq!(exc["value"], "canary-1");
        assert_eq!(exc["exclude"], true);
    }
}
