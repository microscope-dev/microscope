mod inspect;
mod profile;
pub mod protocol;
mod syscalls;
#[cfg(test)]
mod tests;

use std::sync::{Arc, OnceLock};

use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::capture::Capture;

pub use syscalls::end_text;

pub type SourceLines<'a> = &'a dyn Fn(&str, u64, u64) -> Result<Vec<String>, String>;
pub type BaseLookup<'a> = &'a dyn Fn(&str) -> Option<Arc<CaptureView>>;

pub const HOSTED_MAX_TIMELINE_LANES: usize = 2_048;
pub const HOSTED_MAX_TIMELINE_CELLS: usize = 524_288;

#[derive(Clone, Copy, Default)]
pub struct AnswerOptions<'a> {
    pub live: bool,
    pub base: Option<BaseLookup<'a>>,
    pub source_lines: Option<SourceLines<'a>>,
    pub max_timeline_lanes: Option<usize>,
    pub max_timeline_cells: Option<usize>,
}

impl AnswerOptions<'_> {
    #[must_use]
    pub const fn hosted() -> Self {
        Self {
            live: false,
            base: None,
            source_lines: None,
            max_timeline_lanes: Some(HOSTED_MAX_TIMELINE_LANES),
            max_timeline_cells: Some(HOSTED_MAX_TIMELINE_CELLS),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ViewError {
    #[error("{0}")]
    Invalid(String),
    #[error("{0}")]
    Failed(String),
}

pub struct CaptureView {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) capture: Arc<Capture>,
    pub(crate) file: Option<protocol::ProfileFile>,
    syscalls: OnceLock<Arc<syscalls::Trace>>,
    inspection: OnceLock<Value>,
}

impl CaptureView {
    #[must_use]
    pub const fn new(
        capture: Arc<Capture>,
        id: String,
        name: String,
        file: Option<protocol::ProfileFile>,
    ) -> Self {
        Self {
            id,
            name,
            capture,
            file,
            syscalls: OnceLock::new(),
            inspection: OnceLock::new(),
        }
    }

    #[must_use]
    pub fn capture(&self) -> &Capture {
        &self.capture
    }

    pub fn resolve_query(
        &self,
        query: &protocol::ProfileQuery,
    ) -> Result<crate::analysis::Query, ViewError> {
        let keys =
            crate::analysis::Keys::new(&self.capture, profile::granularity(query.granularity));
        profile::resolve_view(self, &keys, query)
    }

    fn trace(&self) -> &Arc<syscalls::Trace> {
        self.syscalls
            .get_or_init(|| Arc::new(syscalls::Trace::of(&self.capture)))
    }

    #[expect(
        clippy::too_many_lines,
        reason = "dispatches every read-only profile and syscall query method"
    )]
    pub fn answer(
        &self,
        method: &str,
        params: Value,
        opts: AnswerOptions<'_>,
    ) -> Result<(Value, Vec<u8>), ViewError> {
        match method {
            "profile" => Ok((profile::to_value(&profile::summary(self)), Vec::new())),
            "profileInspect" => Ok((
                self.inspection
                    .get_or_init(|| {
                        let _span = crate::span!("web profile inspect");
                        profile::to_value(&inspect::inspection(&self.capture, self.file.clone()))
                    })
                    .clone(),
                Vec::new(),
            )),
            "profileTree" => {
                let _span = crate::span!("web profile tree");
                let query: protocol::ProfileQuery = decode(params)?;
                profile::tree(self, &query, opts.base)
            }
            "profileFunctions" => {
                let _span = crate::span!("web profile functions");
                let query: protocol::ProfileQuery = decode(params)?;
                Ok((
                    profile::to_value(&profile::functions(self, &query, opts.base)?),
                    Vec::new(),
                ))
            }
            "profileTimeline" => {
                let _span = crate::span!("web profile timeline");
                let query: protocol::ProfileTimelineQuery = decode(params)?;
                Ok((
                    profile::to_value(&profile::timeline(self, &query, opts)?),
                    Vec::new(),
                ))
            }
            "profileZones" => {
                let _span = crate::span!("web profile zones");
                let query: protocol::ProfileZonesQuery = decode(params)?;
                Ok(profile::zones(&self.capture, &query))
            }
            "profileZoneStats" => {
                let query: protocol::ProfileZoneStatsQuery = decode(params)?;
                Ok((
                    profile::to_value(&profile::zone_stats(&self.capture, &query)),
                    Vec::new(),
                ))
            }
            "profileLines" => {
                let _span = crate::span!("web profile lines");
                let query: protocol::ProfileFunctionQuery = decode(params)?;
                Ok((
                    profile::to_value(&profile::lines(self, &query, opts.source_lines)?),
                    Vec::new(),
                ))
            }
            "profileCode" => Ok((
                profile::to_value(&protocol::ProfileCode {
                    weight: 0,
                    module: None,
                    blocks: Vec::new(),
                    unplaced: Vec::new(),
                    unavailable: Some(
                        "Disassembly is only available in local microscope web".to_owned(),
                    ),
                    functions: Vec::new(),
                    files: Vec::new(),
                    unlined: None,
                    inlined_into: None,
                }),
                Vec::new(),
            )),
            "syscallSummary" => {
                let query: protocol::SyscallSummaryQuery = decode(params)?;
                Ok((
                    profile::to_value(&syscalls::summary(&self.capture, self.trace(), &query)?),
                    Vec::new(),
                ))
            }
            "syscallHeatmap" => {
                let query: protocol::SyscallHeatmapQuery = decode(params)?;
                Ok((
                    profile::to_value(&syscalls::heatmap(&self.capture, self.trace(), &query)?),
                    Vec::new(),
                ))
            }
            "syscallDetail" => {
                let query: protocol::SyscallDetailQuery = decode(params)?;
                Ok((
                    profile::to_value(&syscalls::detail(&self.capture, self.trace(), &query)?),
                    Vec::new(),
                ))
            }
            "syscallLog" => {
                let query: protocol::SyscallLogQuery = decode(params)?;
                Ok((
                    profile::to_value(&syscalls::log(&self.capture, self.trace(), &query)?),
                    Vec::new(),
                ))
            }
            "syscallCall" => {
                let query: protocol::SyscallCallQuery = decode(params)?;
                Ok((
                    profile::to_value(&syscalls::call(
                        &self.capture,
                        self.trace(),
                        &query,
                        opts.source_lines,
                    )?),
                    Vec::new(),
                ))
            }
            "syscallFiles" => {
                let query: protocol::SyscallQuery = decode(params)?;
                Ok((
                    profile::to_value(&syscalls::files(&self.capture, self.trace(), &query)?),
                    Vec::new(),
                ))
            }
            "syscallNetwork" => {
                let query: protocol::SyscallQuery = decode(params)?;
                Ok((
                    profile::to_value(&syscalls::network(&self.capture, self.trace(), &query)?),
                    Vec::new(),
                ))
            }
            "syscallProcesses" => {
                let query: protocol::SyscallQuery = decode(params)?;
                Ok((
                    profile::to_value(&syscalls::processes(&self.capture, self.trace(), &query)?),
                    Vec::new(),
                ))
            }
            "syscallInFlight" => Ok((
                profile::to_value(&syscalls::in_flight(&self.capture, opts.live)),
                Vec::new(),
            )),
            other => Err(ViewError::Invalid(format!(
                "unknown profile method {other}"
            ))),
        }
    }
}

fn decode<T: DeserializeOwned>(params: Value) -> Result<T, ViewError> {
    serde_json::from_value(params).map_err(|error| ViewError::Invalid(error.to_string()))
}
