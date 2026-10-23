//! The portable resource gate; broad profiling stays in `bench`.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

#[derive(clap::Args)]
pub struct Args {
    /// The immutable Nix corpus, not locally built test fixtures.
    #[arg(long)]
    corpus: PathBuf,

    /// Reviewed input hashes and absolute resource ceilings.
    #[arg(long)]
    budgets: PathBuf,

    /// Save the measurements, including provenance, as JSON.
    #[arg(long)]
    out: Option<PathBuf>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Budgets {
    schema: u32,
    measurement: String,
    workloads: BTreeMap<String, Budget>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Budget {
    sha256: String,
    allocations: u64,
    allocated_bytes: u64,
}

#[derive(Serialize)]
struct Report {
    schema: u32,
    measurement: String,
    provenance: super::Provenance,
    workloads: BTreeMap<String, Usage>,
}

#[derive(Clone, Serialize)]
struct Usage {
    sha256: String,
    allocations: Option<u64>,
    allocated_bytes: Option<u64>,
}

fn configuration() -> String {
    format!(
        "cold-load/jobs=1/system-alloc={}/debug-assertions={}",
        cfg!(feature = "system-alloc"),
        cfg!(debug_assertions)
    )
}

fn check_inputs(budgets: &Budgets, report: &Report) -> Result<()> {
    ensure!(
        budgets.schema == 1 && report.schema == 1 && budgets.measurement == report.measurement,
        "input/configuration mismatch: expected schema {} and {:?}, got schema {} and {:?}",
        budgets.schema,
        budgets.measurement,
        report.schema,
        report.measurement
    );
    ensure!(
        !budgets.workloads.is_empty(),
        "input/configuration mismatch: smoke budgets contain no workloads"
    );
    ensure!(
        report.workloads.keys().eq(budgets.workloads.keys()),
        "input/configuration mismatch: expected workloads {:?}, got {:?}",
        budgets.workloads.keys().collect::<Vec<_>>(),
        report.workloads.keys().collect::<Vec<_>>()
    );
    for (name, usage) in &report.workloads {
        ensure!(
            usage.sha256 == budgets.workloads[name].sha256,
            "input/configuration mismatch: {name} SHA-256 is {}, expected {}; review the corpus build, not the resource budgets",
            usage.sha256,
            budgets.workloads[name].sha256
        );
    }
    Ok(())
}

fn check(budgets: &Budgets, report: &Report) -> Result<()> {
    check_inputs(budgets, report)?;
    for (name, usage) in &report.workloads {
        let budget = &budgets.workloads[name];
        for (metric, actual, limit) in [
            ("allocations", usage.allocations, budget.allocations),
            (
                "allocated bytes",
                usage.allocated_bytes,
                budget.allocated_bytes,
            ),
        ] {
            let actual = actual
                .with_context(|| format!("invalid measurement: {name} has no {metric} counter"))?;
            ensure!(
                actual <= limit,
                "resource regression: {name} used {actual} {metric}, budget {limit}"
            );
        }
    }
    Ok(())
}

fn inputs(corpus: &Path) -> Result<BTreeMap<String, Usage>> {
    std::fs::read_dir(corpus)
        .with_context(|| {
            format!(
                "input/configuration mismatch: cannot read {}",
                corpus.display()
            )
        })?
        .map(|entry| {
            let entry = entry?;
            let name = entry.file_name().into_string().map_err(|_| {
                anyhow::anyhow!("input/configuration mismatch: non-UTF-8 corpus name")
            })?;
            let bytes = std::fs::read(entry.path())?;
            let mut digest = String::with_capacity(64);
            for byte in Sha256::digest(bytes) {
                write!(digest, "{byte:02x}")?;
            }
            Ok((
                name,
                Usage {
                    sha256: digest,
                    allocations: None,
                    allocated_bytes: None,
                },
            ))
        })
        .collect()
}

pub fn run(args: &Args) -> Result<bool> {
    let budgets: Budgets = serde_json::from_slice(&std::fs::read(&args.budgets)?)
        .context("input/configuration mismatch: invalid smoke budgets")?;
    let mut report = Report {
        schema: 1,
        measurement: configuration(),
        provenance: super::provenance(),
        workloads: inputs(&args.corpus)?,
    };
    check_inputs(&budgets, &report)?;
    for (name, usage) in &mut report.workloads {
        let measured = super::load_once(&args.corpus.join(name), 1, false)?;
        usage.allocations = measured.report.summary.allocations;
        usage.allocated_bytes = measured.report.summary.allocated_bytes;
    }

    if let Some(out) = &args.out {
        std::fs::write(out, serde_json::to_string_pretty(&report)?)?;
    }
    for (name, usage) in &report.workloads {
        if let (Some(allocations), Some(bytes)) = (usage.allocations, usage.allocated_bytes) {
            println!("{name}: {allocations} allocations, {bytes} allocated bytes");
        } else {
            println!("{name}: allocation counters unavailable");
        }
    }
    check(&budgets, &report)?;
    println!("smoke resource budgets passed");
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn case() -> (Budgets, Report) {
        let budgets = Budgets {
            schema: 1,
            measurement: "test contract".into(),
            workloads: BTreeMap::from([(
                "sample".into(),
                Budget {
                    sha256: "immutable input".into(),
                    allocations: 120,
                    allocated_bytes: 18_000,
                },
            )]),
        };
        let report = Report {
            schema: 1,
            measurement: "test contract".into(),
            provenance: super::super::provenance(),
            workloads: BTreeMap::from([(
                "sample".into(),
                Usage {
                    sha256: "immutable input".into(),
                    allocations: Some(119),
                    allocated_bytes: Some(17_999),
                },
            )]),
        };
        (budgets, report)
    }

    #[test]
    fn resource_ceilings_allow_headroom_but_catch_more_blocks_and_larger_blocks() {
        let (budgets, mut report) = case();
        assert!(check(&budgets, &report).is_ok());
        let usage = report.workloads.get_mut("sample").unwrap();
        usage.allocations = Some(120);
        usage.allocated_bytes = Some(18_000);
        assert!(
            check(&budgets, &report).is_ok(),
            "the ceilings are inclusive"
        );

        report.workloads.get_mut("sample").unwrap().allocations = Some(121);
        assert!(
            check(&budgets, &report).is_err(),
            "one block over the budget"
        );
        let usage = report.workloads.get_mut("sample").unwrap();
        usage.allocations = Some(20);
        usage.allocated_bytes = Some(18_001);
        assert!(check(&budgets, &report).is_err(), "fewer but larger blocks");
    }

    #[test]
    fn missing_counters_and_incomplete_runs_are_not_improvements() {
        let (budgets, mut report) = case();
        let original = report.workloads["sample"].clone();
        report.workloads.get_mut("sample").unwrap().allocations = None;
        assert!(check(&budgets, &report).is_err());
        report.workloads.insert("sample".into(), original);
        report.workloads.get_mut("sample").unwrap().allocated_bytes = None;
        assert!(check(&budgets, &report).is_err());
        report.workloads.clear();
        assert!(check(&budgets, &report).is_err());
        let empty = Budgets {
            workloads: BTreeMap::new(),
            ..budgets
        };
        assert!(
            check(&empty, &report).is_err(),
            "an empty gate proves nothing"
        );
    }

    #[test]
    fn changed_inputs_extra_workloads_and_incompatible_contracts_fail() {
        let (budgets, mut report) = case();
        report.workloads.get_mut("sample").unwrap().sha256 = "changed bytes".into();
        assert!(check(&budgets, &report).is_err());
        report.workloads.get_mut("sample").unwrap().sha256 = "immutable input".into();
        report
            .workloads
            .insert("unexpected".into(), report.workloads["sample"].clone());
        assert!(check(&budgets, &report).is_err());
        report.workloads.remove("unexpected");
        report.measurement = "other allocator or workers".into();
        assert!(check(&budgets, &report).is_err());
        report.measurement = "test contract".into();
        report.schema = 2;
        assert!(check(&budgets, &report).is_err());
    }
}
