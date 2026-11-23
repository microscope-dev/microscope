use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context as _, Result, bail};
use clap::{Args, Subcommand};
use serde::Deserialize;
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

    /// Filter expression: DIM=VAL to include. May be repeated.
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

    /// Use only the explicit CLI scope flags instead of merging the saved session's scope (keeps --session for pinned data).
    #[arg(long)]
    pub exact_scope: bool,

    /// Compare mode: previous-period, previous-version, rest-of-group, shift:<duration>, or custom:<json>.
    #[arg(long, value_name = "MODE")]
    pub compare: Option<String>,

    /// Scope query to a selected function name.
    #[arg(long, value_name = "NAME", conflicts_with_all = ["select_syscall", "select_zone"])]
    pub select_function: Option<String>,

    /// Scope query to a selected syscall (NAME, ABI:NUMBER, or ABI:NUMBER:NAME).
    #[arg(long, value_name = "SPEC", conflicts_with_all = ["select_function", "select_zone"])]
    pub select_syscall: Option<String>,

    /// Scope query to a selected zone name.
    #[arg(long, value_name = "NAME", conflicts_with_all = ["select_function", "select_syscall"])]
    pub select_zone: Option<String>,

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
    let mut base_scope = if let Some(session_id) = args.session
        && !args.exact_scope
    {
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

    apply_scope_args(&mut base_scope, args, unix_now_ns())?;
    Ok(Value::Object(base_scope))
}

fn apply_scope_args(
    base_scope: &mut serde_json::Map<String, Value>,
    args: &QueryScopeArgs,
    now_ns: i64,
) -> Result<()> {
    if !args.filters.is_empty() {
        let mut parsed = Vec::with_capacity(args.filters.len());
        for raw in &args.filters {
            parsed.push(parse_filter(raw)?);
        }
        base_scope.insert("filters".into(), json!(parsed));
    }

    if args.from.is_some() || args.to.is_some() {
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
        let compare_val = parse_compare_for_scope(compare, base_scope, now_ns)?;
        base_scope.insert("compare".into(), compare_val);
    }

    if let Some(name) = args.select_function.as_deref() {
        let trimmed = name.trim();
        if trimmed.is_empty() {
            bail!("--select-function cannot be empty");
        }
        base_scope.insert(
            "selection".into(),
            json!({ "kind": "function", "name": trimmed }),
        );
    } else if let Some(name) = args.select_zone.as_deref() {
        let trimmed = name.trim();
        if trimmed.is_empty() {
            bail!("--select-zone cannot be empty");
        }
        base_scope.insert(
            "selection".into(),
            json!({ "kind": "zone", "name": trimmed }),
        );
    } else if let Some(spec) = args.select_syscall.as_deref() {
        base_scope.insert("selection".into(), parse_syscall_selection(spec)?);
    }

    Ok(())
}

const MAX_SHIFT_SECS: i64 = 366 * 86_400;
const RELATIVE_END_BUFFER_SECS: i64 = 60;

fn previous_period_compare(
    scope: &mut serde_json::Map<String, Value>,
    now_ns: i64,
) -> Result<Value> {
    let Some(range) = scope.get("time_range").and_then(Value::as_object) else {
        return Ok(json!({ "kind": "shift", "offset_secs": 3600 + RELATIVE_END_BUFFER_SECS }));
    };
    match range.get("kind").and_then(Value::as_str) {
        Some("absolute") => {
            let start = range.get("start_time").and_then(Value::as_i64).unwrap_or(0);
            let end = range.get("end_time").and_then(Value::as_i64).unwrap_or(0);
            let span = end.saturating_sub(start).max(1);
            Ok(json!({
                "kind": "custom",
                "filters": [],
                "time_range": {
                    "kind": "absolute",
                    "start_time": start.saturating_sub(span),
                    "end_time": start,
                }
            }))
        }
        Some("relative") => {
            let duration_secs = range
                .get("duration_secs")
                .and_then(Value::as_i64)
                .unwrap_or(3600);
            if !(60..=MAX_SHIFT_SECS).contains(&duration_secs) {
                bail!("relative duration must be between 60 seconds and 366 days");
            }
            let shifted = duration_secs.saturating_add(RELATIVE_END_BUFFER_SECS);
            if shifted <= MAX_SHIFT_SECS {
                Ok(json!({
                    "kind": "shift",
                    "offset_secs": shifted,
                }))
            } else {
                let span = duration_secs.saturating_mul(1_000_000_000).max(1);
                let start = now_ns.saturating_sub(span);
                scope.insert(
                    "time_range".into(),
                    json!({
                        "kind": "absolute",
                        "start_time": start,
                        "end_time": now_ns,
                    }),
                );
                Ok(json!({
                    "kind": "custom",
                    "filters": [],
                    "time_range": {
                        "kind": "absolute",
                        "start_time": start.saturating_sub(span),
                        "end_time": start,
                    }
                }))
            }
        }
        _ => Ok(json!({ "kind": "shift", "offset_secs": 3600 + RELATIVE_END_BUFFER_SECS })),
    }
}

fn parse_compare_for_scope(
    raw: &str,
    scope: &mut serde_json::Map<String, Value>,
    now_ns: i64,
) -> Result<Value> {
    let trimmed = raw.trim();
    if matches!(trimmed, "previous-period" | "previous_period") {
        return previous_period_compare(scope, now_ns);
    }
    parse_compare(trimmed, 3600 + RELATIVE_END_BUFFER_SECS)
}

pub fn parse_compare(raw: &str, window_secs: i64) -> Result<Value> {
    let trimmed = raw.trim();
    if let Some(rest) = trimmed.strip_prefix("shift:") {
        let offset_secs = parse_duration_secs(rest)?;
        if !(60..=366 * 86_400).contains(&offset_secs) {
            bail!("shift compare offset must be between 60s and 366d");
        }
        return Ok(json!({
            "kind": "shift",
            "offset_secs": offset_secs,
        }));
    }
    if let Some(rest) = trimmed.strip_prefix("custom:") {
        let parsed: Value = serde_json::from_str(rest).context("invalid custom compare JSON")?;
        let Some(obj) = parsed.as_object() else {
            bail!("custom compare must be a JSON object");
        };
        let filters = obj
            .get("filters")
            .cloned()
            .unwrap_or_else(|| Value::Array(Vec::new()));
        let time_range = obj.get("time_range").cloned().unwrap_or(Value::Null);
        return Ok(json!({
            "kind": "custom",
            "filters": filters,
            "time_range": time_range,
        }));
    }
    match trimmed {
        "previous-period" | "previous_period" => Ok(json!({
            "kind": "shift",
            "offset_secs": window_secs.clamp(60, 366 * 86_400),
        })),
        "previous-version" | "previous_version" => Ok(json!({ "kind": "previous_version" })),
        "rest-of-group" | "rest_of_group" => Ok(json!({ "kind": "rest_of_group" })),
        other => bail!(
            "unsupported --compare value {other:?}; use previous-period, previous-version, rest-of-group, shift:<duration>, or custom:<json>"
        ),
    }
}

pub fn parse_syscall_selection(raw: &str) -> Result<Value> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        bail!("--select-syscall cannot be empty");
    }
    let parts: Vec<&str> = trimmed.splitn(3, ':').collect();
    if let [abi_str, num_str, rest @ ..] = parts.as_slice()
        && let (Ok(abi), Ok(number)) = (abi_str.parse::<u8>(), num_str.parse::<u32>())
    {
        let name = rest.first().map(|s| s.trim()).filter(|s| !s.is_empty());
        return Ok(json!({
            "kind": "syscall",
            "abi": abi,
            "number": number,
            "name": name,
        }));
    }
    Ok(json!({
        "kind": "syscall",
        "abi": null,
        "number": null,
        "name": trimmed,
    }))
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
    let Some((lhs, val)) = raw.split_once('=') else {
        bail!("invalid filter {raw:?}; expected DIM=VAL");
    };
    if lhs.ends_with('!') {
        bail!("exclusion filter {raw:?} is not supported by the server; use DIM=VAL");
    }
    let dim = lhs.trim();
    let val = val.trim();
    if dim.is_empty() || val.is_empty() {
        bail!("invalid filter {raw:?}; expected DIM=VAL");
    }
    Ok(json!({
        "dimension": dim,
        "value": val,
    }))
}

fn unix_now_ns() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_nanos()).unwrap_or(i64::MAX))
}

fn print_flame(resp: &Value, tree: bool, limit: usize, min_percent: f64) {
    print!("{}", format_flame(resp, tree, limit, min_percent));
}

#[expect(
    clippy::too_many_lines,
    reason = "flame summary formatting handles tree and flat modes"
)]
fn format_flame(resp: &Value, tree: bool, limit: usize, min_percent: f64) -> String {
    use std::fmt::Write as _;

    let mut out = String::new();
    if resp.get("locked").and_then(Value::as_bool) == Some(true) {
        let _ = writeln!(
            out,
            "Scope is locked (missing read permission on one or more projects)."
        );
        return out;
    }
    let total_weight = resp.get("weight").and_then(Value::as_f64).unwrap_or(0.0);
    let samples = resp.get("samples").and_then(Value::as_u64).unwrap_or(0);
    if total_weight <= 0.0 {
        let _ = writeln!(out, "No samples in scope.");
        return out;
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

    let _ = writeln!(
        out,
        "Total: {} ({samples} samples, {} blocks)",
        format_ns(total_weight),
        resp.get("blocks").and_then(Value::as_u64).unwrap_or(0),
    );

    if tree {
        let _ = writeln!(out, "{:>7}  {:>7}  Function", "Total%", "Own%");
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
            let _ = writeln!(
                out,
                "{tot_pct:>6.2}%  {own_pct:>6.2}%  {}{}",
                "  ".repeat(d),
                key_name(k)
            );
            shown += 1;
        }
        return out;
    }

    let mut by_fn: BTreeMap<String, (f64, f64)> = BTreeMap::new();
    let mut stack: Vec<String> = Vec::new();
    for i in 0..key_idx.len() {
        let d = usize::try_from(depth.get(i).and_then(Value::as_u64).unwrap_or(0)).unwrap_or(0);
        stack.truncate(d);
        let k = usize::try_from(key_idx.get(i).and_then(Value::as_u64).unwrap_or(0)).unwrap_or(0);
        let name = key_name(k);
        let own_v = own.get(i).and_then(Value::as_f64).unwrap_or(0.0);
        let tot_v = total.get(i).and_then(Value::as_f64).unwrap_or(0.0);
        let recursive = stack.iter().any(|ancestor| ancestor == &name);
        let entry = by_fn.entry(name.clone()).or_insert((0.0, 0.0));
        entry.0 += own_v;
        if !recursive {
            entry.1 += tot_v;
        }
        stack.push(name);
    }

    let mut rows: Vec<(String, f64, f64)> = by_fn
        .into_iter()
        .filter(|(_, (_, t))| (*t * 100.0 / total_weight) >= min_percent)
        .map(|(name, (o, t))| (name, o, t))
        .collect();
    rows.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| b.2.total_cmp(&a.2)));

    let _ = writeln!(out, "{:>7}  {:>7}  Function", "Own%", "Total%");
    for (name, o, t) in rows.into_iter().take(limit) {
        let tot_pct = t * 100.0 / total_weight;
        let own_pct = o * 100.0 / total_weight;
        let _ = writeln!(out, "{own_pct:>6.2}%  {tot_pct:>6.2}%  {name}");
    }
    out
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct OverTimeSeriesResp {
    label: String,
    #[serde(default)]
    values: Vec<f64>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct OverTimeResp {
    #[serde(default)]
    locked: bool,
    #[serde(default)]
    unit: String,
    #[serde(default)]
    group_by: String,
    #[serde(default)]
    series: Vec<OverTimeSeriesResp>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct MachineRowResp {
    hostname: String,
    #[serde(default)]
    environment: String,
    #[serde(default)]
    overall: f64,
    #[serde(default)]
    distance_from_median: f64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ByMachineResp {
    #[serde(default)]
    locked: bool,
    #[serde(default)]
    unit: String,
    #[serde(default)]
    median: f64,
    #[serde(default)]
    machines: Vec<MachineRowResp>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct BreakdownRowResp {
    value: String,
    #[serde(default)]
    total: f64,
    #[serde(default)]
    share: f64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct BreakdownResp {
    #[serde(default)]
    locked: bool,
    #[serde(default)]
    dimension: String,
    #[serde(default)]
    unit: String,
    #[serde(default)]
    total: f64,
    #[serde(default)]
    rows: Vec<BreakdownRowResp>,
}

fn print_over_time(resp: &Value) {
    print!("{}", format_over_time(resp));
}

#[expect(
    clippy::cast_precision_loss,
    reason = "bucket counts are small integers converted for average calculation"
)]
fn format_over_time(resp: &Value) -> String {
    let Ok(parsed) = serde_json::from_value::<OverTimeResp>(resp.clone()) else {
        return "Unexpected over-time response.\n".to_owned();
    };
    if parsed.locked {
        return "Scope is locked (missing read permission on one or more projects).\n".to_owned();
    }
    if parsed.series.is_empty() {
        return "No time-series data in scope.\n".to_owned();
    }
    let mut out = String::new();
    let group_by = if parsed.group_by.is_empty() {
        "all"
    } else {
        parsed.group_by.as_str()
    };
    let is_ns = matches!(parsed.unit.as_str(), "nanoseconds" | "ns");
    let _ = writeln!(
        out,
        "Grouped by {group_by} ({} series, unit: {}):",
        parsed.series.len(),
        if parsed.unit.is_empty() {
            "raw"
        } else {
            parsed.unit.as_str()
        }
    );
    let summary_col = if is_ns { "Total" } else { "Avg" };
    let _ = writeln!(out, "{:<32}  {:>14}  {:>14}", "Series", summary_col, "Peak");
    for s in parsed.series {
        let count = s.values.len();
        let sum: f64 = s.values.iter().copied().sum();
        let summary_val = if is_ns || count == 0 {
            sum
        } else {
            sum / count as f64
        };
        let peak: f64 = s.values.iter().copied().fold(0.0_f64, f64::max);
        let _ = writeln!(
            out,
            "{:<32}  {:>14}  {:>14}",
            s.label,
            format_metric(summary_val, &parsed.unit),
            format_metric(peak, &parsed.unit),
        );
    }
    out
}

fn print_by_machine(resp: &Value, limit: usize) {
    print!("{}", format_by_machine(resp, limit));
}

fn format_by_machine(resp: &Value, limit: usize) -> String {
    let Ok(parsed) = serde_json::from_value::<ByMachineResp>(resp.clone()) else {
        return "Unexpected by-machine response.\n".to_owned();
    };
    if parsed.locked {
        return "Scope is locked (missing read permission on one or more projects).\n".to_owned();
    }
    if parsed.machines.is_empty() {
        return "No machines in scope.\n".to_owned();
    }
    let mut out = String::new();
    let _ = writeln!(
        out,
        "Median: {}",
        format_metric(parsed.median, &parsed.unit)
    );
    let _ = writeln!(
        out,
        "{:<28}  {:<12}  {:>14}  {:>14}",
        "Hostname", "Environment", "Overall", "Dist/Median"
    );
    for m in parsed.machines.into_iter().take(limit) {
        let _ = writeln!(
            out,
            "{:<28}  {:<12}  {:>14}  {:>14}",
            m.hostname,
            m.environment,
            format_metric(m.overall, &parsed.unit),
            format_metric(m.distance_from_median, &parsed.unit),
        );
    }
    out
}

fn print_breakdown(resp: &Value, limit: usize) {
    print!("{}", format_breakdown(resp, limit));
}

fn format_breakdown(resp: &Value, limit: usize) -> String {
    let Ok(parsed) = serde_json::from_value::<BreakdownResp>(resp.clone()) else {
        return "Unexpected breakdown response.\n".to_owned();
    };
    if parsed.locked {
        return "Scope is locked (missing read permission on one or more projects).\n".to_owned();
    }
    let dim = if parsed.dimension.is_empty() {
        "dimension"
    } else {
        parsed.dimension.as_str()
    };
    if parsed.rows.is_empty() {
        return format!("No breakdown data for {dim}.\n");
    }
    let mut out = String::new();
    let _ = writeln!(out, "{:>7}  {:>14}  {dim}", "Share%", "Value");
    for row in parsed.rows.into_iter().take(limit) {
        let pct = if row.share > 0.0 {
            row.share * 100.0
        } else if parsed.total > 0.0 {
            row.total * 100.0 / parsed.total
        } else {
            0.0
        };
        let _ = writeln!(
            out,
            "{pct:>6.2}%  {:>14}  {}",
            format_metric(row.total, &parsed.unit),
            row.value,
        );
    }
    out
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

fn format_metric(val: f64, unit: &str) -> String {
    match unit {
        "nanoseconds" | "ns" => format_ns(val),
        "percent" | "%" => format!("{val:.2}%"),
        "cores" => format!("{val:.3} cores"),
        "" => format!("{val:.2}"),
        other => format!("{val:.2} {other}"),
    }
}

fn format_ns(ns: f64) -> String {
    let abs = ns.abs();
    if abs >= 1e9 {
        format!("{:.2}s", ns / 1e9)
    } else if abs >= 1e6 {
        format!("{:.1}ms", ns / 1e6)
    } else if abs >= 1e3 {
        format!("{:.1}us", ns / 1e3)
    } else {
        format!("{ns:.0}ns")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_scope_args() -> QueryScopeArgs {
        QueryScopeArgs {
            server: None,
            token: None,
            ca_file: None,
            org: None,
            projects: Vec::new(),
            filters: Vec::new(),
            last: None,
            from: None,
            to: None,
            session: None,
            exact_scope: false,
            compare: None,
            select_function: None,
            select_syscall: None,
            select_zone: None,
            fresh: false,
            json: false,
        }
    }

    #[test]
    fn parses_durations_filters_compare_and_selection() {
        assert_eq!(parse_duration_secs("30s").unwrap(), 30);
        assert_eq!(parse_duration_secs("15m").unwrap(), 900);
        assert_eq!(parse_duration_secs("2h").unwrap(), 7200);
        assert_eq!(parse_duration_secs("7d").unwrap(), 604_800);
        assert!(parse_duration_secs("0h").is_err());

        let inc = parse_filter("service.name=checkout").unwrap();
        assert_eq!(
            inc,
            json!({ "dimension": "service.name", "value": "checkout" })
        );
        assert!(inc.get("exclude").is_none());
        let val_with_bang = parse_filter("service.name=a!=b").unwrap();
        assert_eq!(
            val_with_bang,
            json!({ "dimension": "service.name", "value": "a!=b" })
        );
        assert!(parse_filter("host.name!=canary-1").is_err());

        let mut args = empty_scope_args();
        args.last = Some("2h".into());
        args.filters = vec!["service.name=checkout".into()];
        args.compare = Some("previous-period".into());
        args.select_function = Some("process_request".into());
        let mut scope = serde_json::Map::new();
        apply_scope_args(&mut scope, &args, 1_000_000_000_000).unwrap();
        assert_eq!(
            scope["compare"],
            json!({ "kind": "shift", "offset_secs": 7260 })
        );
        assert_eq!(
            scope["selection"],
            json!({ "kind": "function", "name": "process_request" })
        );
    }

    #[test]
    fn previous_period_compare_handles_short_and_boundary_ranges() {
        let mut short_abs = empty_scope_args();
        short_abs.from = Some("1000".into());
        short_abs.to = Some("1030".into());
        short_abs.compare = Some("previous-period".into());
        let mut short_scope = serde_json::Map::new();
        apply_scope_args(&mut short_scope, &short_abs, 2_000_000_000_000).unwrap();
        assert_eq!(
            short_scope["compare"],
            json!({
                "kind": "custom",
                "filters": [],
                "time_range": {
                    "kind": "absolute",
                    "start_time": 970_000_000_000_i64,
                    "end_time": 1_000_000_000_000_i64,
                }
            })
        );

        let mut max_rel = empty_scope_args();
        max_rel.last = Some("366d".into());
        max_rel.compare = Some("previous-period".into());
        let mut max_scope = serde_json::Map::new();
        let now_ns = 2_000_000_000_000_000_000_i64;
        apply_scope_args(&mut max_scope, &max_rel, now_ns).unwrap();
        let dur_ns = 366_i64 * 86_400 * 1_000_000_000;
        let start = now_ns - dur_ns;
        assert_eq!(
            max_scope["time_range"],
            json!({
                "kind": "absolute",
                "start_time": start,
                "end_time": now_ns,
            })
        );
        assert_eq!(
            max_scope["compare"],
            json!({
                "kind": "custom",
                "filters": [],
                "time_range": {
                    "kind": "absolute",
                    "start_time": start - dur_ns,
                    "end_time": start,
                }
            })
        );

        let mut over_max = empty_scope_args();
        over_max.last = Some("367d".into());
        over_max.compare = Some("previous-period".into());
        assert!(apply_scope_args(&mut serde_json::Map::new(), &over_max, now_ns).is_err());

        let mut under_min = empty_scope_args();
        under_min.last = Some("30s".into());
        under_min.compare = Some("previous-period".into());
        assert!(apply_scope_args(&mut serde_json::Map::new(), &under_min, now_ns).is_err());

        assert_eq!(
            parse_compare("rest-of-group", 3600).unwrap(),
            json!({ "kind": "rest_of_group" })
        );
        assert_eq!(
            parse_compare("shift:30m", 3600).unwrap(),
            json!({ "kind": "shift", "offset_secs": 1800 })
        );
        assert_eq!(
            parse_syscall_selection("0:1:write").unwrap(),
            json!({ "kind": "syscall", "abi": 0, "number": 1, "name": "write" })
        );
        assert_eq!(
            parse_syscall_selection("futex").unwrap(),
            json!({ "kind": "syscall", "abi": null, "number": null, "name": "futex" })
        );
    }

    #[test]
    fn formats_services_by_machine_and_over_time_responses() {
        let by_machine = json!({
            "timestampsNs": [1000, 2000],
            "bucketNs": 60_000_000_000_i64,
            "unit": "percent",
            "median": 12.5,
            "machines": [{
                "machineId": 7,
                "hostname": "co-us1-c19",
                "environment": "prod",
                "overall": 42.25,
                "distanceFromMedian": 29.75,
                "values": [40.0, 44.5]
            }],
            "blocks": 1,
            "prunedBlocks": 0,
            "cacheHits": 0,
            "locked": false
        });
        let rendered_machine = format_by_machine(&by_machine, 10);
        assert!(
            rendered_machine.contains("co-us1-c19"),
            "{rendered_machine}"
        );
        assert!(rendered_machine.contains("42.25%"), "{rendered_machine}");
        assert!(rendered_machine.contains("29.75%"), "{rendered_machine}");
        assert!(!rendered_machine.contains("0ns"), "{rendered_machine}");

        let over_time = json!({
            "timestampsNs": [1000, 2000],
            "bucketNs": 60_000_000_000_i64,
            "unit": "cores",
            "groupBy": "service.version",
            "series": [{
                "label": "v2.4.1",
                "values": [1.5, 2.5]
            }],
            "deploys": [],
            "blocks": 1,
            "prunedBlocks": 0,
            "cacheHits": 0,
            "locked": false
        });
        let rendered_ot = format_over_time(&over_time);
        assert!(
            rendered_ot.contains("Grouped by service.version"),
            "{rendered_ot}"
        );
        assert!(rendered_ot.contains("2.000 cores"), "{rendered_ot}");
        assert!(rendered_ot.contains("2.500 cores"), "{rendered_ot}");
        assert!(!rendered_ot.contains("ns"), "{rendered_ot}");
    }

    #[test]
    fn flat_flame_does_not_double_count_recursion_and_formats_negative_ns() {
        let flame = json!({
            "weight": 100.0,
            "samples": 10,
            "unit": "nanoseconds",
            "keys": [{ "name": "recur" }, { "name": "leaf" }],
            "own": [10.0, 20.0, 70.0],
            "total": [100.0, 90.0, 70.0],
            "key": [0, 0, 1],
            "depth": [0, 1, 2],
            "blocks": 1,
            "prunedBlocks": 0,
            "cacheHits": 0,
            "locked": false,
            "truncated": false
        });
        let rendered = format_flame(&flame, false, 10, 0.0);
        assert!(rendered.contains("100.00%"), "{rendered}");
        assert!(
            !rendered.contains("190.00%"),
            "must not double-count recursion: {rendered}"
        );
        assert_eq!(format_ns(-5_000_000.0), "-5.0ms");
    }
}
