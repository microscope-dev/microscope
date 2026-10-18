//! Data breakpoints: watchpoints on values the client picks.

use std::collections::{BTreeMap, HashMap};

use microscope::{
    VirtualAddress, WatchAccess, WatchScope, WatchpointHit, WatchpointId, WatchpointOptions,
    WatchpointSpec,
};
use serde_json::{Value, json};

use super::protocol::{self, DataBreakpointInfoArguments, ErrorBody, SetDataBreakpointsArguments};
use super::session::{Closed, Session, error, parse};

/// One data breakpoint the client set.
#[derive(Debug, Clone)]
pub struct DataEntry {
    pub id: i64,
    data_id: String,
    access: WatchAccess,

    /// The condition and hit condition as the client wrote them, blank ones
    /// as none.
    conditions: Conditions,
    pub watchpoint: Result<WatchpointId, String>,

    /// Whether the console disabled its watchpoint, as the client was last
    /// told.
    disabled: bool,
}

/// A data breakpoint's condition and hit condition as the client wrote
/// them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Conditions {
    condition: Option<String>,
    hit_condition: Option<String>,
}

impl Conditions {
    fn of(breakpoint: &protocol::DataBreakpoint) -> Self {
        let given = |text: &Option<String>| {
            text.as_deref()
                .map(str::trim)
                .filter(|text| !text.is_empty())
                .map(str::to_owned)
        };

        Self {
            condition: given(&breakpoint.condition),
            hit_condition: given(&breakpoint.hit_condition),
        }
    }

    /// The debugger's options for them, or why they have none.
    fn options(&self) -> microscope::Result<WatchpointOptions> {
        Ok(WatchpointOptions {
            hit_condition: self.hit_condition.as_deref().map(str::parse).transpose()?,
            condition: self
                .condition
                .as_deref()
                .map(microscope::Condition::parse)
                .transpose()?,
            ..WatchpointOptions::default()
        })
    }
}

/// The data breakpoints of a session.
#[derive(Debug, Default)]
pub struct Data {
    /// What each data id the client was given watches.
    targets: HashMap<String, WatchpointSpec>,
    next_target: u64,
    pub entries: Vec<DataEntry>,
}

impl Data {
    /// The client's ids of the data breakpoints a stop hit.
    pub fn hit(&self, hits: &[WatchpointHit]) -> Vec<i64> {
        self.entries
            .iter()
            .filter(|entry| {
                hits.iter().any(|hit| {
                    entry
                        .watchpoint
                        .as_ref()
                        .is_ok_and(|id| *id == hit.watchpoint)
                })
            })
            .map(|entry| entry.id)
            .collect()
    }

    /// Forgets the data breakpoints of watchpoints that no longer exist and
    /// returns their client ids.
    pub fn forget(&mut self, watchpoints: &[WatchpointId]) -> Vec<i64> {
        let mut forgotten = Vec::new();
        self.entries.retain(|entry| {
            let gone = entry
                .watchpoint
                .as_ref()
                .is_ok_and(|id| watchpoints.contains(id));
            if gone {
                forgotten.push(entry.id);
            }
            !gone
        });
        forgotten
    }
}

/// The protocol's name for an access kind. A `write` data breakpoint stops
/// when the value changes, as clients present it ("Break on Value Change")
/// and as gdb's and lldb's adapters arm it; its `store` mode stops at every
/// store instead, even of the value already held.
const fn access_name(access: WatchAccess) -> Option<&'static str> {
    match access {
        WatchAccess::Change => Some("write"),
        WatchAccess::Write => None,
        WatchAccess::Read => Some("read"),
        WatchAccess::ReadWrite => Some("readWrite"),
    }
}

/// The modes a data breakpoint may have, as `initialize` reports them.
pub fn modes() -> Value {
    json!([
        {
            "mode": "change",
            "label": "On Change",
            "description": "Stop when a store changes the value",
            "appliesTo": ["data"],
        },
        {
            "mode": "store",
            "label": "On Every Store",
            "description": "Stop at every store, even of the value already held",
            "appliesTo": ["data"],
        },
    ])
}

/// The accesses a client's data breakpoint watches, from its access type
/// and mode.
fn access_of(access_type: Option<&str>, mode: Option<&str>) -> Result<WatchAccess, String> {
    let change = match mode {
        None | Some("change") => true,
        Some("store") => false,
        Some(other) => return Err(format!("unknown data breakpoint mode '{other}'")),
    };

    match (access_type, change) {
        (None | Some("write"), true) => Ok(WatchAccess::Change),
        (None | Some("write"), false) => Ok(WatchAccess::Write),
        // Every access includes every store.
        (Some("readWrite"), _) if mode != Some("change") => Ok(WatchAccess::ReadWrite),
        (Some("read"), true) if mode.is_none() => Ok(WatchAccess::Read),
        (Some("read" | "readWrite"), true) => {
            Err("the change mode applies only to write data breakpoints".to_owned())
        }
        (Some("read"), false) => {
            Err("the store mode does not apply to read data breakpoints".to_owned())
        }
        (Some(other), _) => Err(format!("unknown access type '{other}'")),
    }
}

impl Session {
    pub(super) async fn data_breakpoint_info(
        &mut self,
        arguments: Value,
    ) -> Result<Value, ErrorBody> {
        let arguments =
            parse::<DataBreakpointInfoArguments>(arguments, "dataBreakpointInfo arguments")?;
        let Ok(stop) = self.current_stop() else {
            return Ok(unwatchable("the program must be stopped to watch a value"));
        };

        let handle = self.target_handle()?;
        let (spec, description) = if arguments.as_address == Some(true) {
            let Some(address) = protocol::address(&arguments.name) else {
                return Ok(unwatchable(&format!(
                    "'{}' is not an address",
                    arguments.name
                )));
            };

            let bytes = arguments
                .bytes
                .and_then(|bytes| u64::try_from(bytes).ok())
                .filter(|bytes| *bytes != 0)
                .unwrap_or(1);
            (
                WatchpointSpec::Location {
                    address: VirtualAddress::new(address),
                    byte_size: bytes,
                },
                format!("{bytes} bytes at {address:#x}"),
            )
        } else {
            let (context, expression) = if let Some(reference) = arguments.variables_reference {
                let Some((context, path)) = self.references.child_path(reference, &arguments.name)
                else {
                    return Ok(unwatchable(&format!("{} cannot be named", arguments.name)));
                };

                (context, path)
            } else {
                let context = match arguments.frame_id {
                    Some(frame) => self.references.frame_context(frame).ok_or_else(|| {
                        ErrorBody::new(format!("frame reference {frame} is stale"))
                    })?,
                    None => stop.innermost(),
                };

                match microscope::Expression::parse(arguments.name.trim()) {
                    Ok(expression) => (context, expression),
                    Err(error) => return Ok(unwatchable(&error.to_string())),
                }
            };

            match handle.at(context).resolve_watch_target(&expression).await {
                Ok(target) => {
                    let description = format!(
                        "{} ({} bytes at {}{})",
                        target.expression(),
                        target.byte_size(),
                        target.address(),
                        scope_text(target.scope())
                    );
                    (WatchpointSpec::Target(Box::new(target)), description)
                }
                Err(error) => return Ok(unwatchable(&error.to_string())),
            }
        };

        self.data.next_target += 1;
        let data_id = format!("data-{}", self.data.next_target);
        self.data.targets.insert(data_id.clone(), spec);
        let access = handle
            .watchpoint_capabilities()
            .access
            .iter()
            .filter_map(|access| access_name(*access))
            .collect::<Vec<_>>();
        Ok(json!({
            "dataId": data_id,
            "description": description,
            "accessTypes": access,
            "canPersist": false,
        }))
    }

    pub(super) async fn set_data_breakpoints(
        &mut self,
        arguments: Value,
    ) -> Result<Value, ErrorBody> {
        let arguments =
            parse::<SetDataBreakpointsArguments>(arguments, "setDataBreakpoints arguments")?;

        // Before a program is loaded, nothing is watched, and each data
        // breakpoint says why.
        let handle = self.target_handle().ok();
        let wanted = arguments
            .breakpoints
            .iter()
            .map(|breakpoint| {
                let access = access_of(
                    breakpoint.access_type.as_deref(),
                    breakpoint.mode.as_deref(),
                );
                (breakpoint, access)
            })
            .collect::<Vec<_>>();

        // Watchpoints use scarce debug registers, so release first, along
        // with those whose new conditions leave them unarmed. Kept entries
        // are keyed by their place in the request.
        let mut kept = BTreeMap::new();
        let mut previous = std::mem::take(&mut self.data.entries).into_iter();
        while let Some(mut entry) = previous.next() {
            let index = (0..wanted.len()).find(|index| {
                let (breakpoint, access) = &wanted[*index];
                breakpoint.data_id == entry.data_id
                    && access.as_ref() == Ok(&entry.access)
                    && !kept.contains_key(index)
            });
            let Ok(watchpoint) = entry.watchpoint else {
                continue;
            };

            let options = index.map(|index| {
                let conditions = Conditions::of(wanted[index].0);
                let options = conditions.options();
                (index, conditions, options)
            });
            if let Some((index, conditions, Ok(options))) = options {
                kept.insert(index, self.amend_data(entry, conditions, options).await);
                continue;
            }

            if let Some(handle) = &handle {
                match handle.remove_watchpoint(watchpoint).await {
                    Ok(_) | Err(microscope::Error::WatchpointNotFound(_)) => {}
                    Err(error) => {
                        // Keep every entry that may still be armed, so a
                        // later request can release it.
                        self.data.entries =
                            kept.into_values().chain([entry]).chain(previous).collect();
                        return Err(self::error(error));
                    }
                }
            }

            if let Some((index, conditions, Err(error))) = options {
                entry.conditions = conditions;
                entry.watchpoint = Err(error.to_string());
                kept.insert(index, entry);
            }
        }

        let mut entries = Vec::new();
        for (index, (breakpoint, access)) in wanted.into_iter().enumerate() {
            let entry = match access {
                Ok(access) => {
                    if let Some(entry) = kept.remove(&index) {
                        entry
                    } else {
                        let watchpoint = self.install_data(breakpoint, access).await;
                        DataEntry {
                            id: self.breakpoints.allocate_id(),
                            data_id: breakpoint.data_id.clone(),
                            access,
                            conditions: Conditions::of(breakpoint),
                            watchpoint,
                            disabled: false,
                        }
                    }
                }
                Err(message) => DataEntry {
                    id: self.breakpoints.allocate_id(),
                    data_id: breakpoint.data_id.clone(),
                    access: WatchAccess::Change,
                    conditions: Conditions::of(breakpoint),
                    watchpoint: Err(message),
                    disabled: false,
                },
            };

            entries.push(entry);
        }

        let body = entries.iter().map(data_json).collect::<Vec<_>>();
        self.data.entries = entries;
        Ok(json!({"breakpoints": body}))
    }

    /// Applies the conditions a kept data breakpoint was sent with to its
    /// watchpoint, which keeps its hits.
    async fn amend_data(
        &self,
        mut entry: DataEntry,
        conditions: Conditions,
        options: WatchpointOptions,
    ) -> DataEntry {
        if entry.conditions == conditions {
            return entry;
        }

        let (Ok(watchpoint), Ok(handle)) = (entry.watchpoint.clone(), self.target_handle()) else {
            return entry;
        };

        let amended = match handle
            .set_watchpoint_condition(watchpoint, options.condition)
            .await
        {
            Ok(_) => handle
                .set_watchpoint_hit_condition(watchpoint, options.hit_condition)
                .await
                .map(drop),
            Err(error) => Err(error),
        };

        entry.conditions = conditions;
        if let Err(error) = amended {
            entry.watchpoint = Err(error.to_string());
        }

        entry
    }

    async fn install_data(
        &self,
        breakpoint: &protocol::DataBreakpoint,
        access: WatchAccess,
    ) -> Result<WatchpointId, String> {
        let options = Conditions::of(breakpoint)
            .options()
            .map_err(|error| error.to_string())?;
        let spec = self.data.targets.get(&breakpoint.data_id).ok_or_else(|| {
            format!(
                "unknown data id '{}'; ask for it with dataBreakpointInfo",
                breakpoint.data_id
            )
        })?;
        let handle = self.target_handle().map_err(|error| error.format)?;
        handle
            .add_watchpoint_with(spec.clone(), access, options)
            .await
            .map(|watchpoint| watchpoint.id)
            .map_err(|error| error.to_string())
    }

    /// Removes the client's data breakpoints whose watchpoints are gone,
    /// such as those the console deleted.
    pub(super) async fn sync_data(&mut self) -> Result<(), Closed> {
        let Ok(handle) = self.target_handle() else {
            return Ok(());
        };

        let Ok(snapshot) = handle.snapshot().await else {
            return Ok(());
        };

        let gone = self
            .data
            .entries
            .iter()
            .filter_map(|entry| entry.watchpoint.as_ref().ok().copied())
            .filter(|id| {
                !snapshot
                    .watchpoints
                    .iter()
                    .any(|watchpoint| watchpoint.id == *id)
            })
            .collect::<Vec<_>>();
        for id in self.data.forget(&gone) {
            self.data_removed(id).await?;
        }

        // The console enables and disables watchpoints too.
        let mut changed = Vec::new();
        for entry in &mut self.data.entries {
            let Ok(id) = entry.watchpoint else {
                continue;
            };

            let disabled = snapshot
                .watchpoints
                .iter()
                .any(|watchpoint| watchpoint.id == id && !watchpoint.enabled);
            if disabled != entry.disabled {
                entry.disabled = disabled;
                changed.push(data_json(entry));
            }
        }

        for breakpoint in changed {
            self.client
                .event(
                    "breakpoint",
                    json!({"reason": "changed", "breakpoint": breakpoint}),
                )
                .await?;
        }

        Ok(())
    }

    async fn data_removed(&self, id: i64) -> Result<(), Closed> {
        self.client
            .event(
                "breakpoint",
                json!({"reason": "removed", "breakpoint": {"id": id, "verified": false}}),
            )
            .await
    }

    /// Reports data breakpoints whose watched storage ended.
    pub(super) async fn data_invalidated(
        &mut self,
        invalidated: &[microscope::InvalidatedWatchpoint],
    ) -> Result<(), Closed> {
        for entry in invalidated {
            for id in self.data.forget(&[entry.watchpoint.id]) {
                self.data_removed(id).await?;
                self.client
                    .console(format!(
                        "data breakpoint {id} on {} was removed: {}",
                        crate::cli::format::watch_subject(&entry.watchpoint),
                        crate::cli::format::invalidation_text(entry.reason)
                    ))
                    .await?;
            }
        }

        Ok(())
    }

    /// Describes what a data breakpoint stop's accesses did to each value.
    pub(super) async fn watch_description(&self, hits: &[WatchpointHit]) -> String {
        let Ok(handle) = self.target_handle() else {
            return String::new();
        };

        let watchpoints = handle
            .snapshot()
            .await
            .map(|snapshot| snapshot.watchpoints)
            .unwrap_or_default();
        let image = handle.module_image();
        hits.iter()
            .map(|hit| {
                let watchpoint = watchpoints
                    .iter()
                    .find(|watchpoint| watchpoint.id == hit.watchpoint);
                let subject = watchpoint.map_or_else(
                    || format!("watchpoint {}", hit.watchpoint),
                    crate::cli::format::watch_subject,
                );
                let type_info = watchpoint.and_then(|watchpoint| watchpoint.type_info.as_ref());
                let (previous, current) =
                    crate::cli::value::hit_values(hit, type_info, Some(image));

                if hit.changed() {
                    format!("{subject} changed from {previous} to {current}")
                } else if watchpoint
                    .is_some_and(|watchpoint| watchpoint.access == WatchAccess::Write)
                {
                    format!("{subject} was written; it is still {current}")
                } else {
                    format!("{subject} was accessed; it is {current}")
                }
            })
            .collect::<Vec<_>>()
            .join("; ")
    }
}

fn scope_text(scope: &WatchScope) -> String {
    match scope {
        WatchScope::Location | WatchScope::Static { .. } => String::new(),
        WatchScope::ThreadLocal { thread } => format!(", thread {thread}'s instance"),
        WatchScope::Frame { .. } | WatchScope::Task { .. } | WatchScope::Interpreted { .. } => {
            ", until its function returns".to_owned()
        }
    }
}

fn unwatchable(reason: &str) -> Value {
    json!({"dataId": null, "description": reason})
}

fn data_json(entry: &DataEntry) -> Value {
    match &entry.watchpoint {
        Ok(id) if entry.disabled => json!({
            "id": entry.id,
            "verified": false,
            "message": format!("disabled; enable w{id} in the debug console"),
        }),
        Ok(_) => json!({"id": entry.id, "verified": true}),
        Err(message) => {
            json!({"id": entry.id, "verified": false, "message": message, "reason": "failed"})
        }
    }
}
