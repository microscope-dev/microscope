use std::{collections::BTreeMap, sync::Arc, time::Duration};

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use microscope_data::{
    object::TimeUnit,
    sanitize::{Action, Builtin, HashKey, Rule, RuleSpec, Sanitizer},
};

use crate::{
    agent::{
        config::{Limits, SanitizeConfig},
        select::{RuleAction, RuleKind, SelectedProcess, SelectionRule},
        wire::{BurstDirective, PolicyDirective, SanitizerPolicyDirective},
    },
    backend::perf,
    profiler::{
        Cuts, Preset, RecordOptions, SMALL_STACK_COPY, SharedBpfEngine, StackCopy, SyscallLevel,
        budget::Budget,
    },
};

const AGENT_DEV_MAX_SYSCALL_CAP: u32 = 20_000;
const MIN_CPU_BUDGET_SHARE: f64 = 0.002;
const MIN_MEMORY_BOUNDED_RING_PAGES: usize = 2;
const MIN_QUEUE_BYTES_PER_GROUP: usize = 64 * 1024;
const MIN_BPF_RING_BYTES_PER_CPU: usize =
    crate::backend::syscall_engines::bpf::SMALLEST_RING as usize;

pub const fn preset_code(preset: Preset) -> u8 {
    match preset {
        Preset::Production => 0,
        Preset::Development => 1,
        Preset::Maximum => 2,
    }
}

pub const fn preset_rank(preset: Preset) -> u8 {
    match preset {
        Preset::Production => 0,
        Preset::Development => 1,
        Preset::Maximum => 2,
    }
}

pub const fn time_unit_rank(unit: TimeUnit) -> u8 {
    match unit {
        TimeUnit::Ms => 0,
        TimeUnit::Us => 1,
        TimeUnit::Ns => 2,
    }
}

pub const fn clamp_preset(preset: Preset, max_preset: Option<Preset>) -> Preset {
    match max_preset {
        Some(cap) if preset_rank(preset) > preset_rank(cap) => cap,
        _ => preset,
    }
}

pub const fn clamp_time_unit(unit: TimeUnit, min_unit: Option<TimeUnit>) -> TimeUnit {
    match min_unit {
        Some(cap) if time_unit_rank(unit) > time_unit_rank(cap) => cap,
        _ => unit,
    }
}

pub fn parse_policy_preset(name: &str) -> Option<Preset> {
    match name {
        "prod" | "production" => Some(Preset::Production),
        "dev" | "development" => Some(Preset::Development),
        "max" | "maximum" => Some(Preset::Maximum),
        _ => None,
    }
}

pub fn parse_policy_time_unit(name: &str) -> Option<TimeUnit> {
    match name {
        "1ns" | "ns" => Some(TimeUnit::Ns),
        "1us" | "1µs" | "us" | "µs" => Some(TimeUnit::Us),
        "1ms" | "ms" => Some(TimeUnit::Ms),
        _ => None,
    }
}

pub fn build_sanitizers(
    local: &SanitizeConfig,
    remote: Option<&SanitizerPolicyDirective>,
) -> (Arc<Sanitizer>, BTreeMap<String, Arc<Sanitizer>>) {
    match try_build_sanitizers(local, remote) {
        Ok(pair) => pair,
        Err(error) => {
            eprintln!(
                "microscope agent: invalid remote sanitizer policy ({error}); keeping local sanitizer only"
            );
            try_build_sanitizers(local, None).unwrap_or_else(|_| {
                (
                    Arc::new(Sanitizer::new(
                        &Builtin::DEFAULT,
                        Vec::new(),
                        HashKey::new(b"microscope-agent-local"),
                    )),
                    BTreeMap::new(),
                )
            })
        }
    }
}

pub type BuiltSanitizers = (Arc<Sanitizer>, BTreeMap<String, Arc<Sanitizer>>);

pub fn try_build_sanitizers(
    local: &SanitizeConfig,
    remote: Option<&SanitizerPolicyDirective>,
) -> Result<BuiltSanitizers, String> {
    let mut builtins: Vec<Builtin> = Builtin::DEFAULT
        .iter()
        .copied()
        .filter(|b| !local.disable_builtins.contains(b))
        .collect();
    if let Some(rem) = remote {
        for name in &rem.enabled_builtins {
            let b = name
                .parse::<Builtin>()
                .map_err(|e| format!("unknown builtin rule {name:?}: {e}"))?;
            if !local.disable_builtins.contains(&b) && !builtins.contains(&b) {
                builtins.push(b);
            }
        }
    }

    let key_bytes: Vec<u8> = match remote.and_then(|r| r.hash_key.as_deref()) {
        Some(b64) => {
            let decoded = URL_SAFE_NO_PAD
                .decode(b64)
                .map_err(|e| format!("invalid base64 hash_key: {e}"))?;
            if decoded.is_empty() {
                return Err("empty sanitizer hash_key".to_owned());
            }
            decoded
        }
        None => b"microscope-agent-local".to_vec(),
    };

    let mut org_specs: Vec<RuleSpec> = local.rules.clone();
    let mut per_project_specs: BTreeMap<String, Vec<RuleSpec>> = BTreeMap::new();
    if let Some(rem) = remote {
        for r in &rem.rules {
            let action = Action::parse(&r.action).ok_or_else(|| {
                format!(
                    "unknown sanitizer action {:?} on rule {:?}",
                    r.action, r.name
                )
            })?;
            let spec = RuleSpec {
                name: r.name.clone(),
                fields: r.fields.clone(),
                pattern: r.pattern.clone(),
                action,
            };
            Rule::new(&spec).map_err(|e| format!("invalid rule {:?}: {e}", r.name))?;
            if let Some(proj) = &r.project {
                per_project_specs
                    .entry(proj.to_ascii_lowercase())
                    .or_default()
                    .push(spec);
            } else {
                org_specs.push(spec);
            }
        }
    }

    let compile = |specs: &[RuleSpec]| -> Result<Vec<Rule>, String> {
        specs
            .iter()
            .map(|spec| Rule::new(spec).map_err(|e| format!("invalid rule {:?}: {e}", spec.name)))
            .collect()
    };

    let org_sanitizer = Arc::new(Sanitizer::new(
        &builtins,
        compile(&org_specs)?,
        HashKey::new(&key_bytes),
    ));

    let mut project_sanitizers = BTreeMap::new();
    for (proj_key, extra_specs) in per_project_specs {
        let mut combined = org_specs.clone();
        combined.extend(extra_specs);
        project_sanitizers.insert(
            proj_key,
            Arc::new(Sanitizer::new(
                &builtins,
                compile(&combined)?,
                HashKey::new(&key_bytes),
            )),
        );
    }

    Ok((org_sanitizer, project_sanitizers))
}

pub fn remote_selection_rules(policy: Option<&PolicyDirective>) -> Vec<SelectionRule> {
    let Some(pol) = policy else {
        return Vec::new();
    };
    pol.rules
        .iter()
        .filter_map(|r| {
            Some(SelectionRule {
                kind: RuleKind::parse(&r.kind)?,
                pattern: r.pattern.clone(),
                action: RuleAction::parse(&r.action)?,
                project: r.project.clone(),
                service: r.service.clone(),
            })
        })
        .collect()
}

pub fn effective_policy_for_process(
    proc_sel: &SelectedProcess,
    base_preset: Preset,
    base_unit: TimeUnit,
    policy: Option<&PolicyDirective>,
    bursts: &[BurstDirective],
    limits: &Limits,
    now_ns: i64,
) -> (Preset, TimeUnit) {
    let mut preset = base_preset;
    let mut unit = base_unit;

    if let Some(pol) = policy {
        if let Some(p) = pol.preset.as_deref().and_then(parse_policy_preset) {
            preset = p;
        }
        if let Some(u) = pol.time_unit.as_deref().and_then(parse_policy_time_unit) {
            unit = u;
        }
        if let Some(proj) = &proc_sel.project
            && let Some(pp) = pol
                .project_policies
                .iter()
                .find(|p| p.project.eq_ignore_ascii_case(proj))
        {
            if let Some(p) = parse_policy_preset(&pp.preset) {
                preset = p;
            }
            if let Some(u) = parse_policy_time_unit(&pp.time_unit) {
                unit = u;
            }
        }
    }

    for b in bursts {
        if b.ends_at_ns.is_some_and(|ends| ends <= now_ns) {
            continue;
        }
        let project_matches = match (&b.project, &proc_sel.project) {
            (None, _) => true,
            (Some(bp), Some(pp)) => bp.eq_ignore_ascii_case(pp),
            (Some(_), None) => false,
        };
        if project_matches {
            if let Some(p) = parse_policy_preset(&b.preset)
                && preset_rank(p) >= preset_rank(preset)
            {
                preset = p;
            }
            if let Some(u) = b.time_unit.as_deref().and_then(parse_policy_time_unit)
                && time_unit_rank(u) >= time_unit_rank(unit)
            {
                unit = u;
            }
        }
    }

    (
        clamp_preset(preset, limits.max_preset),
        clamp_time_unit(unit, limits.min_time_unit),
    )
}

pub fn page_size_bytes() -> usize {
    nix::unistd::sysconf(nix::unistd::SysconfVar::PAGE_SIZE)
        .ok()
        .flatten()
        .and_then(|v| usize::try_from(v).ok())
        .filter(|&v| v >= 4096)
        .unwrap_or(4096)
}

const fn floor_power_of_two(n: usize) -> usize {
    if n == 0 {
        0
    } else {
        1_usize << (usize::BITS - 1 - n.leading_zeros())
    }
}

pub fn max_concurrent_groups_for_cpus(limits: &Limits, cpus: usize) -> usize {
    let Some(memory_mib) = limits.memory_mib else {
        return usize::MAX;
    };
    let budget_bytes = usize::try_from(memory_mib)
        .unwrap_or(usize::MAX)
        .saturating_mul(1024 * 1024);
    let bpf_reserved = bpf_max_ring_bytes_for_cpus(limits, cpus).map_or(0, |cap| {
        usize::try_from(cap)
            .unwrap_or(usize::MAX)
            .saturating_mul(cpus.max(1))
    });
    let remaining = budget_bytes.saturating_sub(bpf_reserved);
    let min_group_bytes = (MIN_MEMORY_BOUNDED_RING_PAGES + 1)
        .saturating_mul(page_size_bytes())
        .saturating_mul(cpus.max(1))
        .saturating_add(MIN_QUEUE_BYTES_PER_GROUP);
    remaining / min_group_bytes.max(1)
}

pub fn max_concurrent_groups(limits: &Limits) -> usize {
    let cpus = perf::online_cpus().map_or(1, |c| c.len().max(1));
    max_concurrent_groups_for_cpus(limits, cpus)
}

pub fn bpf_max_ring_bytes_for_cpus(limits: &Limits, cpus: usize) -> Option<u32> {
    let memory_mib = limits.memory_mib?;
    let budget_bytes = usize::try_from(memory_mib)
        .unwrap_or(usize::MAX)
        .saturating_mul(1024 * 1024);
    let bpf_budget = budget_bytes / 4;
    let per_cpu = floor_power_of_two(bpf_budget / cpus.max(1));
    (per_cpu >= MIN_BPF_RING_BYTES_PER_CPU).then(|| u32::try_from(per_cpu).unwrap_or(u32::MAX))
}

pub fn syscalls_fit_memory_limit_for_cpus(limits: &Limits, cpus: usize) -> bool {
    limits.memory_mib.is_none() || bpf_max_ring_bytes_for_cpus(limits, cpus).is_some()
}

pub fn syscalls_fit_memory_limit(limits: &Limits) -> bool {
    let cpus = perf::online_cpus().map_or(1, |c| c.len().max(1));
    syscalls_fit_memory_limit_for_cpus(limits, cpus)
}

pub fn build_record_options(
    preset: Preset,
    chunk_ms: u64,
    limits: &Limits,
    group_count: usize,
    shared_bpf: Option<&SharedBpfEngine>,
    allow_syscalls: bool,
) -> RecordOptions {
    let cpus = perf::online_cpus().map_or(1, |c| c.len().max(1));
    build_record_options_for_cpus(
        preset,
        chunk_ms,
        limits,
        group_count,
        cpus,
        shared_bpf,
        allow_syscalls,
    )
}

pub fn build_record_options_for_cpus(
    preset: Preset,
    chunk_ms: u64,
    limits: &Limits,
    group_count: usize,
    cpus: usize,
    shared_bpf: Option<&SharedBpfEngine>,
    allow_syscalls: bool,
) -> RecordOptions {
    let mut options = RecordOptions::preset(preset);
    options.cuts = Cuts::Segments(Duration::from_millis(chunk_ms));
    options.symbols_later = false;
    let groups = group_count.max(1);
    #[expect(
        clippy::cast_precision_loss,
        reason = "group count is at most 3 concurrent presets"
    )]
    let group_divisor = groups as f64;
    if let Some(cpu_pct) = limits.cpu_percent {
        let mut budget = options.budget.unwrap_or(Budget::PRODUCTION);
        budget.cpu =
            ((f64::from(cpu_pct) / 100.0) / group_divisor).clamp(MIN_CPU_BUDGET_SHARE, 1.0);
        options.budget = Some(budget);
    } else if groups > 1
        && let Some(mut budget) = options.budget
    {
        budget.cpu = (budget.cpu / group_divisor).clamp(MIN_CPU_BUDGET_SHARE, 1.0);
        options.budget = Some(budget);
    }
    let bpf_ring_cap = bpf_max_ring_bytes_for_cpus(limits, cpus);
    let allow_syscalls = allow_syscalls && syscalls_fit_memory_limit_for_cpus(limits, cpus);
    if let Some(memory_mib) = limits.memory_mib {
        let total_budget = usize::try_from(memory_mib)
            .unwrap_or(usize::MAX)
            .saturating_mul(1024 * 1024);
        let bpf_reserved = bpf_ring_cap.map_or(0, |cap| {
            usize::try_from(cap)
                .unwrap_or(usize::MAX)
                .saturating_mul(cpus.max(1))
        });
        let remaining = total_budget.saturating_sub(bpf_reserved);
        let ring_budget_bytes = remaining / 2;
        let per_ring_bytes = ring_budget_bytes / (groups.saturating_mul(cpus.max(1)));
        let max_pages = (per_ring_bytes / page_size_bytes()).saturating_sub(1);
        let bounded = floor_power_of_two(max_pages).max(MIN_MEMORY_BOUNDED_RING_PAGES);
        options.ring_pages = options.ring_pages.min(bounded);
        if options.ring_pages < 16 {
            options.stack = StackCopy::Bytes(SMALL_STACK_COPY);
        }
        let actual_perf_bytes = (options.ring_pages + 1)
            .saturating_mul(page_size_bytes())
            .saturating_mul(groups.saturating_mul(cpus.max(1)));
        let queue_pool = remaining.saturating_sub(actual_perf_bytes);
        let queue_cap = (queue_pool / groups).max(MIN_QUEUE_BYTES_PER_GROUP.min(queue_pool));
        options.max_queue_bytes = Some(queue_cap);
    }
    if allow_syscalls {
        options.syscalls.max_ring_bytes = bpf_ring_cap;
        if let Some(bpf) = shared_bpf {
            if preset != Preset::Production {
                options.syscalls.kept_per_second = Some(AGENT_DEV_MAX_SYSCALL_CAP);
            }
            options.syscalls.shared_bpf = Some(bpf.clone());
        } else if preset != Preset::Production {
            options.syscalls.kept_per_second = Some(AGENT_DEV_MAX_SYSCALL_CAP);
        }
    } else {
        options.syscalls.level = SyscallLevel::Off;
        options.syscalls.kept_per_second = None;
        options.syscalls.max_ring_bytes = None;
    }
    options
}
