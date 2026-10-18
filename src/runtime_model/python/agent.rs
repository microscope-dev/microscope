//! Whole-state scripts. PEP 768 executes each in a fresh namespace, so the
//! monitoring callbacks and generation live in one interpreter-local module.

pub const SOURCE: &str = include_str!("agent.py");

/// How the filename the agent's code is compiled under begins; it ends with
/// `>`. Like `<string>`, it names no file, so no program's code has it.
pub const FILENAME_PREFIX: &str = "<microscope agent ";

/// Whether code compiled under `filename` is the agent's.
pub fn is_agent_filename(filename: &str) -> bool {
    filename.starts_with(FILENAME_PREFIX) && filename.ends_with('>')
}

/// A bootstrap whose caller supplies `__microscope_library`,
/// `__microscope_generation`, and `__microscope_probes` in its namespace,
/// and may supply `__microscope_step` and `__microscope_exceptions`.
/// Keeping this separate also lets a cooperative fixture load the agent.
pub fn bootstrap() -> Result<String, serde_json::Error> {
    let source = serde_json::to_string(SOURCE)?;
    Ok(format!(
        r"import sys as _sys
import _thread as _thread
_candidate = type(_sys)('_microscope_agent')
_candidate._bootstrap_lock = _thread.RLock()
_agent = _sys.modules.setdefault('_microscope_agent', _candidate)
try:
    with _agent._bootstrap_lock:
        if '_microscope_loaded' not in _agent.__dict__:
            exec(compile({source}, '{FILENAME_PREFIX}' + __microscope_library + '>', 'exec'), _agent.__dict__)
        elif _agent._microscope_loaded != 2:
            raise RuntimeError('incompatible microscope agent transport')
    _agent.apply(__microscope_library, __microscope_generation, __microscope_probes,
                 globals().get('__microscope_step'), globals().get('__microscope_exceptions', 0))
except BaseException:
    # A consumed script must report initialization failure through its own
    # library, even if a previous agent owns the interpreter-local module.
    if _sys.version_info >= (3, 14):
        try:
            import ctypes as _ctypes
            _failure_library = _ctypes.PyDLL(__microscope_library)
            _failure_hook = _failure_library.microscope_python_hook
            _failure_hook.argtypes = (_ctypes.c_uint64,) * 3
            _failure_hook.restype = None
            _failure_hook(3, __microscope_generation, 4)
        except BaseException:
            pass  # No acknowledgement: the controller retains the script.
    raise
"
    ))
}

/// The transport's flat wire record: event, report kind, number, file, line,
/// qualified name. Policy-specific breakpoint/step records stay in the runtime.
pub type Probe<'a> = (&'a str, u8, u64, &'a str, u32, &'a str);

/// The step's wire record: number, kind, native thread, how many callers
/// its frame has, and its code's address.
pub type Step = (u64, u8, u64, u32, u64);

/// The assignment's wire record: number, native thread, how many callers
/// its frame has, its code's address, the variable's name, and the value,
/// as `["int", "DECIMAL"]`, `["float", BITS]`, `["bool", 0 or 1]`,
/// `["str", TEXT]`, `["none"]`, or `["object", ADDRESS]`.
pub type Assignment = (u64, u64, u32, u64, String, serde_json::Value);

/// The exceptions that stop, one bit each, as the agent numbers them in its
/// commands and reports: each raised, and each uncaught.
pub const RAISED: u64 = 1;
pub const UNCAUGHT: u64 = 2;

/// A step's kinds, as the agent numbers them. A step back begins where
/// its thread runs native code its frame called, and ends at the thread's
/// next line in that frame or a caller.
pub const INTO: u8 = 0;
pub const OVER: u8 = 1;
pub const OUT: u8 = 2;
pub const BACK: u8 = 3;

/// A standalone whole-state script, executable with fresh globals, which
/// stops at the exceptions whose bits `exceptions` sets.
pub fn script(
    library: &str,
    generation: u64,
    probes: &[Probe<'_>],
    step: Option<Step>,
    exceptions: u64,
) -> Result<String, serde_json::Error> {
    let library = serde_json::to_string(library)?;
    let probes = serde_json::to_string(probes)?;
    // Integers only, which JSON and Python write alike.
    let step = step.map_or_else(
        || Ok("None".to_owned()),
        |step| serde_json::to_string(&step),
    )?;
    Ok(format!(
        "__microscope_library = {library}\n\
         __microscope_generation = {generation}\n\
         __microscope_probes = {probes}\n\
         __microscope_step = {step}\n\
         __microscope_exceptions = {exceptions}\n{}",
        bootstrap()?
    ))
}

/// The hook's three unsigned 64-bit C arguments, decoded at its entry.
/// The report and identifier live in registers belonging to this thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Report {
    Armed {
        generation: u64,
    },
    Breakpoint {
        number: u64,
    },
    /// Several breakpoint hits of one event, which stop once together:
    /// `count` nonzero numbers in an array at `address`, which the calling
    /// thread keeps alive while it is in the hook.
    Breakpoints {
        count: usize,
        address: u64,
    },
    StepEnded {
        number: u64,
    },
    /// The step could not be followed, for `reason`.
    StepLost {
        number: u64,
        reason: StepLoss,
    },
    /// The frame a step into is in calls the C function or method at
    /// `callable`, which the stack keeps alive while the hook runs.
    StepCalls {
        number: u64,
        callable: u64,
    },
    /// An exception of the kind `bit` names, [`RAISED`] or [`UNCAUGHT`], in
    /// a record at `address` that the calling thread keeps alive while it is
    /// in the hook.
    Exception {
        bit: u64,
        address: u64,
    },
    Failed {
        generation: u64,
        failure: Failure,
    },
    /// The assignment is made, or why it could not be.
    Assigned {
        number: u64,
        failure: Option<AssignmentFailure>,
    },
}

/// Why the agent could not make an assignment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AssignmentFailure {
    /// The frame is not where microscope found it.
    Frame,
    /// No name of the frame finds the value's object any more.
    Object,
    /// The interpreter refused to store it.
    Refused,
}

impl AssignmentFailure {
    pub const fn description(self) -> &'static str {
        match self {
            Self::Frame => "the frame is no longer where microscope found it",
            Self::Object => "no name of the frame finds the value's object any more",
            Self::Refused => "the interpreter refused to store the value",
        }
    }
}

/// Why the agent could not follow a step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepLoss {
    /// Its frame was not where the debugger found it.
    Frame,
    /// Its thread ran on before the step began.
    Late,
}

impl StepLoss {
    pub const fn description(self) -> &'static str {
        match self {
            Self::Frame => "the stepping frame is not where microscope found it",
            Self::Late => "the stepping thread ran on before the step began",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Failure {
    ToolExhausted,
    Configuration,
    Cleanup,
    Initialization,
    Reentry,
    Interrupted,
}

impl Failure {
    pub const fn description(self) -> &'static str {
        match self {
            Self::ToolExhausted => "all sys.monitoring tool IDs are in use",
            Self::Configuration => {
                "a monitoring callback or configuration failed; the agent is disarmed"
            }
            Self::Cleanup => "monitoring cleanup failed; its previous state is unconfirmed",
            Self::Initialization => {
                "agent initialization failed; any previous installation is unconfirmed"
            }
            Self::Reentry => {
                "user code reentered agent configuration; monitoring coverage is unconfirmed and the agent is disarmed"
            }
            Self::Interrupted => {
                "an exception interrupted the agent; the interruption is preserved and the agent is disarmed"
            }
        }
    }
}

/// The most breakpoint hits one report carries: far more than the probes
/// that can match one event, and a bounded read of the target.
pub const MAX_HITS: u64 = 4096;

pub fn decode(event: u64, number: u64, argument: u64) -> Result<Report, std::sync::Arc<str>> {
    match (event, number, argument) {
        (0, generation, 0..=6) => Ok(Report::Armed { generation }),
        (1, number, 0) if number != 0 => Ok(Report::Breakpoint { number }),
        (1, count @ 2..=MAX_HITS, address) if address != 0 => Ok(Report::Breakpoints {
            // At most `MAX_HITS`, so it fits.
            count: usize::try_from(count).unwrap_or(usize::MAX),
            address,
        }),
        (2, number, 0) if number != 0 => Ok(Report::StepEnded { number }),
        (2, number, reason @ 1..=2) if number != 0 => Ok(Report::StepLost {
            number,
            reason: if reason == 1 {
                StepLoss::Frame
            } else {
                StepLoss::Late
            },
        }),
        (4, bit @ (RAISED | UNCAUGHT), address) if address != 0 => {
            Ok(Report::Exception { bit, address })
        }
        (5, number, callable) if number != 0 && callable != 0 => {
            Ok(Report::StepCalls { number, callable })
        }
        (6, number, status @ 0..=3) if number != 0 => Ok(Report::Assigned {
            number,
            failure: match status {
                0 => None,
                1 => Some(AssignmentFailure::Frame),
                2 => Some(AssignmentFailure::Object),
                _ => Some(AssignmentFailure::Refused),
            },
        }),
        (3, generation, 1..=6) => Ok(Report::Failed {
            generation,
            failure: match argument {
                1 => Failure::ToolExhausted,
                2 => Failure::Configuration,
                3 => Failure::Cleanup,
                4 => Failure::Initialization,
                5 => Failure::Reentry,
                _ => Failure::Interrupted,
            },
        }),
        _ => Err("invalid Python agent hook report".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::{Failure, Report, decode};

    /// A step back crosses in a script as its kind, and a step into's call
    /// of a C function crosses the hook as the callable's address.
    #[test]
    fn a_step_back_and_a_steps_c_call_cross_whole() {
        let script = super::script("lib.so", 1, &[], Some((3, super::BACK, 7, 2, 0x1000)), 0)
            .expect("serialize the step");
        assert!(script.contains("__microscope_step = [3,3,7,2,4096]\n"));
        assert_eq!(
            decode(5, 3, 0x7000),
            Ok(Report::StepCalls {
                number: 3,
                callable: 0x7000
            })
        );
        assert!(decode(5, 0, 0x7000).is_err());
        assert!(decode(5, 3, 0).is_err());
    }

    #[test]
    fn a_whole_state_script_escapes_its_literals() {
        let script = super::script(
            "a\"\\\nb.so",
            u64::MAX,
            &[("LINE", 1, 42, "quoted\".py", 7, "")],
            Some((5, super::OVER, 7, 2, 0x1000)),
            3,
        )
        .expect("serialize the complete state");
        assert!(script.contains("__microscope_generation = 18446744073709551615"));
        assert!(script.contains("__microscope_step = [5,1,7,2,4096]\n"));
        assert!(script.contains("__microscope_exceptions = 3\n"));
        assert!(
            super::script("a.so", 1, &[], None, 0)
                .expect("serialize the state")
                .contains("__microscope_step = None\n")
        );
        assert!(script.contains(r#"a\"\\\nb.so"#));
        assert!(script.contains("_candidate._bootstrap_lock = _thread.RLock()"));
        assert!(script.contains("_sys.modules.setdefault('_microscope_agent', _candidate)"));
        assert!(super::super::hook::library().starts_with(b"\x7fELF"));
    }

    /// The agent numbers a step's kinds and losses as the runtime does.
    #[test]
    fn the_agent_numbers_steps_as_the_runtime_does() {
        assert!(super::SOURCE.contains(&format!(
            "INTO, OVER, OUT, BACK = {}, {}, {}, {}",
            super::INTO,
            super::OVER,
            super::OUT,
            super::BACK
        )));
        assert!(super::SOURCE.contains("return [(2, step[0], 1)]  # The step's frame"));
        assert!(super::SOURCE.contains("return [(2, step[0], 2)]  # Its thread ran on"));
        let (frame, late) = (
            super::StepLoss::Frame.description(),
            super::StepLoss::Late.description(),
        );
        assert!(!frame.is_empty() && !late.is_empty() && frame != late);
    }

    /// The agent numbers an assignment's outcomes as the runtime does, and
    /// reads the command's assignment as the runtime writes it.
    #[test]
    fn the_agent_numbers_assignments_as_the_runtime_does() {
        for (status, failure) in [
            (1, super::AssignmentFailure::Frame),
            (2, super::AssignmentFailure::Object),
            (3, super::AssignmentFailure::Refused),
        ] {
            assert!(super::SOURCE.contains(&format!("return [(6, number, {status})]")));
            assert_eq!(
                super::decode(6, 8, status),
                Ok(super::Report::Assigned {
                    number: 8,
                    failure: Some(failure)
                })
            );
            assert!(!failure.description().is_empty());
        }
        assert!(super::SOURCE.contains("return [(6, number, 0)]"));
        assert!(super::decode(6, 8, 4).is_err());
        let assignment: super::Assignment = (
            1,
            2,
            0,
            3,
            "x".to_owned(),
            serde_json::json!(["int", "-12"]),
        );
        assert_eq!(
            serde_json::to_string(&assignment).ok().as_deref(),
            Some(r#"[1,2,0,3,"x",["int","-12"]]"#)
        );
    }

    /// Only the name the bootstrap compiles the agent under is the agent's;
    /// a program's file, or other code compiled from a string, is not.
    #[test]
    fn only_the_agents_code_has_its_filename() {
        let script = super::bootstrap().expect("the bootstrap");
        assert!(script.contains("'<microscope agent ' + __microscope_library + '>'"));
        assert!(super::is_agent_filename(
            "<microscope agent /tmp/.microscope-1/hook.so>"
        ));
        for filename in [
            "<string>",
            "/srv/app.py",
            "<microscope agent /tmp/hook.so",
            "microscope agent /tmp/hook.so>",
            "/srv/<microscope agent x>.py",
        ] {
            assert!(!super::is_agent_filename(filename), "{filename}");
        }
    }

    #[test]
    fn reports_are_exact_and_malformed_reports_are_visible() {
        assert_eq!(decode(0, 7, 6), Ok(Report::Armed { generation: 7 }));
        assert_eq!(decode(1, 42, 0), Ok(Report::Breakpoint { number: 42 }));
        assert_eq!(
            decode(1, 2, 0x1000),
            Ok(Report::Breakpoints {
                count: 2,
                address: 0x1000
            })
        );
        assert_eq!(
            decode(1, super::MAX_HITS, 0x1000),
            Ok(Report::Breakpoints {
                count: 4096,
                address: 0x1000
            })
        );
        assert_eq!(decode(2, 43, 0), Ok(Report::StepEnded { number: 43 }));
        assert_eq!(
            decode(2, 43, 1),
            Ok(Report::StepLost {
                number: 43,
                reason: super::StepLoss::Frame
            })
        );
        assert_eq!(
            decode(2, 43, 2),
            Ok(Report::StepLost {
                number: 43,
                reason: super::StepLoss::Late
            })
        );
        for bit in [super::RAISED, super::UNCAUGHT] {
            assert_eq!(
                decode(4, bit, 0x1000),
                Ok(Report::Exception {
                    bit,
                    address: 0x1000
                })
            );
        }
        for (argument, failure) in [
            (1, Failure::ToolExhausted),
            (2, Failure::Configuration),
            (3, Failure::Cleanup),
            (4, Failure::Initialization),
            (5, Failure::Reentry),
            (6, Failure::Interrupted),
        ] {
            assert_eq!(
                decode(3, 7, argument),
                Ok(Report::Failed {
                    generation: 7,
                    failure
                })
            );
            assert!(!failure.description().is_empty());
        }
        for (event, number, argument) in [
            (0, 1, 7),
            (1, 0, 0),
            (1, 1, 1),
            (1, 0, 0x1000),
            (1, super::MAX_HITS + 1, 0x1000),
            (2, 0, 0),
            (2, 0, 1),
            (2, 1, 3),
            (3, 1, 0),
            (3, 1, 7),
            (4, 0, 0x1000),
            (4, 3, 0x1000),
            (4, 1, 0),
            (4, 1, 0),
            (u64::MAX, 1, 0),
        ] {
            assert!(decode(event, number, argument).is_err());
        }
    }
}
