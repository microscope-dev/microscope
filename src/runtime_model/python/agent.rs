//! Whole-state scripts. PEP 768 executes each in a fresh namespace, so the
//! monitoring callbacks and generation live in one interpreter-local module.

pub const SOURCE: &str = include_str!("agent.py");

/// A bootstrap whose caller supplies `__microscope_library`,
/// `__microscope_generation`, and `__microscope_probes` in its namespace.
/// Keeping this separate also lets a cooperative fixture load the agent.
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "agent injection consumes this in P4.7")
)]
pub fn bootstrap() -> Result<String, serde_json::Error> {
    let source = serde_json::to_string(SOURCE)?;
    Ok(format!(
        r"import sys as _sys
import _thread as _thread
_candidate = type(_sys)('_microscope_agent')
_candidate._bootstrap_lock = _thread.RLock()
_agent = _sys.modules.setdefault('_microscope_agent', _candidate)
with _agent._bootstrap_lock:
    if '_microscope_loaded' not in _agent.__dict__:
        exec({source}, _agent.__dict__)
    elif _agent._microscope_loaded != 1:
        raise RuntimeError('incompatible microscope agent transport')
_agent.apply(__microscope_library, __microscope_generation, __microscope_probes)
"
    ))
}

/// The transport's flat wire record: event, report kind, number, file, line,
/// qualified name. Policy-specific breakpoint/step records stay in the runtime.
pub type Probe<'a> = (&'a str, u8, u64, &'a str, u32, &'a str);

/// A standalone whole-state script, executable with fresh globals.
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "agent injection consumes this in P4.7")
)]
pub fn script(
    library: &str,
    generation: u64,
    probes: &[Probe<'_>],
) -> Result<String, serde_json::Error> {
    let library = serde_json::to_string(library)?;
    let probes = serde_json::to_string(probes)?;
    Ok(format!(
        "__microscope_library = {library}\n\
         __microscope_generation = {generation}\n\
         __microscope_probes = {probes}\n{}",
        bootstrap()?
    ))
}

/// The hook's three unsigned 64-bit C arguments, decoded at its entry.
/// The report and identifier live in registers belonging to this thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Report {
    Armed { generation: u64 },
    Breakpoint { number: u64 },
    StepEnded { number: u64 },
}

pub fn decode(event: u64, number: u64, argument: u64) -> Result<Report, std::sync::Arc<str>> {
    match (event, number, argument) {
        (0, generation, 0..=6) => Ok(Report::Armed { generation }),
        (1, number, 0) if number != 0 => Ok(Report::Breakpoint { number }),
        (2, number, 0) if number != 0 => Ok(Report::StepEnded { number }),
        (3, generation, 1) => Err(format!(
            "Python agent generation {generation}: all sys.monitoring tool IDs are in use"
        ).into()),
        (3, generation, 2) => Err(format!(
            "Python agent generation {generation}: a monitoring callback or configuration failed; the agent is disarmed"
        ).into()),
        _ => Err("invalid Python agent hook report".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::{Report, decode};

    #[test]
    fn a_whole_state_script_escapes_its_literals() {
        let script = super::script(
            "a\"\\\nb.so",
            u64::MAX,
            &[("LINE", 1, 42, "quoted\".py", 7, "")],
        )
        .expect("serialize the complete state");
        assert!(script.contains("__microscope_generation = 18446744073709551615"));
        assert!(script.contains(r#"a\"\\\nb.so"#));
        assert!(script.contains("_candidate._bootstrap_lock = _thread.RLock()"));
        assert!(script.contains("_sys.modules.setdefault('_microscope_agent', _candidate)"));
        assert!(super::super::hook::library().starts_with(b"\x7fELF"));
    }

    #[test]
    fn reports_are_exact_and_malformed_reports_are_visible() {
        assert_eq!(decode(0, 7, 6), Ok(Report::Armed { generation: 7 }));
        assert_eq!(decode(1, 42, 0), Ok(Report::Breakpoint { number: 42 }));
        assert_eq!(decode(2, 43, 0), Ok(Report::StepEnded { number: 43 }));
        assert!(decode(3, 7, 1).unwrap_err().contains("tool IDs"));
        assert!(decode(3, 7, 2).unwrap_err().contains("disarmed"));
        for (event, number, argument) in [
            (0, 1, 7),
            (1, 0, 0),
            (1, 1, 1),
            (2, 0, 0),
            (2, 1, 1),
            (3, 1, 0),
            (4, 1, 0),
            (u64::MAX, 1, 0),
        ] {
            assert!(decode(event, number, argument).is_err());
        }
    }
}
