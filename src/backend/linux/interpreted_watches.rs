//! Watches of interpreted variables whose storage a language runtime moves
//! or abandons, as V8's collector moves the contexts that hold a frame's
//! captured variables and rewrites the frame's slots that name moved
//! objects, and its on-stack replacement leaves an interpreter frame's
//! registers for optimized code's.
//!
//! A collection runs on the thread of the isolate whose objects it moves,
//! and the isolate runs none of its JavaScript meanwhile. So the watches
//! of that thread's frames are put aside from the collector's entry, where
//! each is noted by how its frame reaches its word, until its return,
//! where each is placed where its frame reaches the word now, its last
//! value taken anew. The collector's writes are its own moves, never the
//! program's stores, and are not reported. A replacement's entry ends the
//! watches of the replaced frame's own slots. Each site is a breakpoint
//! planted only while an interpreted watch exists; one a thread with no
//! such watch reaches stops nothing.

use std::collections::{BTreeMap, BTreeSet};

use nix::unistd::Pid;

use crate::protocol::{InvalidatedWatchpoint, WatchScope, WatchpointId, WatchpointInvalidation};
use crate::runtime_model::InterpretedPlace;
use crate::{AddressRange, Error, Result, RuntimeId, ThreadId, VirtualAddress};

use super::breakpoints::remove_breakpoint_owner_from;
use super::debug_registers;
use super::frames::unwind_module_for;
use super::native::{InspectionOps, LinuxTraceOps};
use super::registers::x86_64_registers;
use super::runtimes::BoundRuntime;
use super::{BreakpointOwner, Controller, Edit, Inferior, debug_pid, debug_thread_id};

/// The collections and frame replacements the debugger follows.
#[derive(Debug, Default)]
pub(super) struct InterpretedMoves {
    /// The collector's entry in each runtime with a watch.
    collectors: BTreeMap<VirtualAddress, RuntimeId>,

    /// The entries of each such runtime's frame replacers, with the saved
    /// frame pointers between each entry's and the replaced frame's.
    replacers: BTreeMap<VirtualAddress, (RuntimeId, u8)>,

    /// The collections under way on threads with watches.
    collecting: Vec<Collection>,

    /// Where the collections under way return, each planted.
    returns: BTreeSet<VirtualAddress>,

    /// The frames replacements began on, by thread and frame pointer,
    /// whose watches the next follow ends.
    replacing: Vec<(ThreadId, RuntimeId, VirtualAddress)>,

    /// Whether each interpreted watch is of a frame's own slot, which
    /// alone a replacement ends, as noted at the stop that added it.
    frame_slots: BTreeMap<WatchpointId, bool>,
}

impl InterpretedMoves {
    /// Forgets a site whose code went away.
    pub(super) fn forget(&mut self, address: VirtualAddress) {
        self.collectors.remove(&address);
        self.replacers.remove(&address);
        self.returns.remove(&address);
    }

    /// Every site planted.
    pub(super) fn planted(&self) -> BTreeSet<VirtualAddress> {
        self.collectors
            .keys()
            .chain(self.replacers.keys())
            .chain(&self.returns)
            .copied()
            .collect()
    }

    /// Whether an interpreted watch of `scope` is set aside while a
    /// collection on its thread moves its objects: it holds no debug
    /// registers until the collection returns.
    pub(super) fn sets_aside(&self, scope: &WatchScope) -> bool {
        let WatchScope::Interpreted {
            thread, runtime, ..
        } = *scope
        else {
            return false;
        };
        self.collecting.iter().any(|collection| {
            !collection.returned
                && collection.runtime == runtime
                && debug_thread_id(collection.thread) == thread
        })
    }
}

/// One collection on a thread with watches.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Collection {
    thread: Pid,
    runtime: RuntimeId,
    returns_to: VirtualAddress,
    stack: u64,

    /// How each watch set aside for it is reached, once the follow noted
    /// it at the collection's entry.
    places: Option<BTreeMap<WatchpointId, InterpretedPlace>>,

    /// Whether the collector returned, so the objects are where they go.
    returned: bool,
}

impl<P: InspectionOps> Controller<P> {
    /// Whether the interpreted frame at `frame` that `pid`'s native stack
    /// holds, as `runtime` keeps its frames, has left it, so that a watch
    /// of one of its slots ends; `hit` says the thread just wrote the
    /// watched slot. The frame returned or unwound when the innermost
    /// frame's canonical frame address is above the frame's, or, in the
    /// runtime's code, which keeps rbp as its frame pointer, when rbp is
    /// above the frame. A new frame takes its place when the runtime's
    /// frame builder runs at the frame's address: as its unwind rules
    /// prove, or, where they cannot, as rbp shows when the builder wrote
    /// the slot, which it writes only in the frame it builds. Nothing
    /// proved keeps the watch.
    pub(super) fn stack_frame_left(
        &self,
        inferior: &Inferior,
        runtime: &BoundRuntime,
        (pid, hit): (Pid, bool),
        frame: VirtualAddress,
    ) -> Result<bool> {
        let Some(builders) = runtime.model.stack_frame_builders() else {
            return Ok(false);
        };
        let registers = self.ptrace.registers(pid)?;
        let instruction = VirtualAddress::new(registers.rip);
        let context = crate::unwind::FrameContext {
            instruction,
            cfa: None,
            signal_frame: false,
        };
        let cfa = self
            .caller_provider(
                inferior,
                &self.runtimes(inferior),
                pid,
                x86_64_registers(&registers),
                true,
            )
            .frame_cfa(&context)
            .ok()
            .map(VirtualAddress::get);
        let own = frame
            .get()
            .checked_add(16)
            .ok_or_else(|| Error::internal("a frame's address leaves room for its caller's"))?;
        let modules = self.unwind_modules(inferior);
        let (in_runtime, in_builder) =
            unwind_module_for(&modules, instruction).map_or((false, false), |(module, address)| {
                let own_image = module.loaded.image == runtime.module.image;
                (
                    (own_image && runtime.model.owns_code(address))
                        || self.generated_code_modules().contains(&module.loaded.id),
                    own_image && builders.iter().any(|range| range.contains(address)),
                )
            });
        let returned =
            cfa.is_some_and(|cfa| cfa > own) || (in_runtime && registers.rbp > frame.get());
        if returned {
            record!(
                "the frame at {frame} on {pid} returned: the innermost frame is at {cfa:x?}, rbp {:#x}",
                registers.rbp
            );
            return Ok(true);
        }
        let building = in_builder
            && (cfa == Some(own) || (hit && cfa.is_none() && registers.rbp == frame.get()));
        if building {
            record!("a new frame takes the place of the frame at {frame} on {pid}");
        }
        Ok(building)
    }
}

impl<P: LinuxTraceOps> Controller<P> {
    /// Whether each interpreted watch is of a frame's own slot, noted
    /// once at the stop that added it, which the sync that follows the add
    /// runs at. A replacer's entry, which a loop may reach on each
    /// iteration while V8 compiles its replacement, is planted only for
    /// frames' own slots; a watch whose place is unknown counts as one.
    fn frame_slots(
        &self,
        inferior: &Inferior,
        runtimes: &[BoundRuntime],
    ) -> BTreeMap<WatchpointId, bool> {
        let mut frame_slots = BTreeMap::new();
        for (&id, record) in &inferior.watch.watchpoints {
            let WatchScope::Interpreted {
                thread,
                runtime,
                frame,
            } = record.watchpoint.scope
            else {
                continue;
            };
            let in_frame = match inferior.interpreted_moves.frame_slots.get(&id) {
                Some(&in_frame) => in_frame,
                None => runtimes
                    .iter()
                    .find(|bound| bound.id == runtime)
                    .zip(debug_pid(thread).ok())
                    .is_none_or(|(bound, reader)| {
                        !matches!(
                            self.with_runtime_stop(inferior, bound, reader, |stop| {
                                bound.model.interpreted_place(
                                    stop,
                                    frame,
                                    record.watchpoint.address,
                                )
                            }),
                            Ok(InterpretedPlace::Scope { .. })
                        )
                    }),
            };
            frame_slots.insert(id, in_frame);
        }
        frame_slots
    }

    /// Plants the collector's and the frame replacers' entries in each
    /// runtime with an interpreted watch, and removes the rest, with the
    /// returns of collections no watch waits for any more.
    pub(super) fn sync_interpreted_movers(&mut self) -> Result<()> {
        let Some(inferior) = self.inferior.as_ref() else {
            return Ok(());
        };

        let watched = watched_threads(inferior);
        let runtimes = self
            .runtimes(inferior)
            .into_iter()
            .filter(|runtime| watched.iter().any(|(_, id)| *id == runtime.id))
            .collect::<Vec<_>>();
        let frame_slots = self.frame_slots(inferior, &runtimes);
        let collectors = runtimes
            .iter()
            .filter_map(|runtime| {
                let entry = runtime.model.collector()?;
                Some((runtime.module.virtual_address(entry).ok()?, runtime.id))
            })
            .collect::<BTreeMap<_, _>>();
        // The replacers are builtins, which V8 may run from a copy it made
        // of the image's code: each mapping of the image is planted.
        let mappings = self
            .loaded_images(inferior)
            .map(|(module, _)| module)
            .collect::<Vec<_>>();
        let mut replacers = BTreeMap::new();
        for runtime in runtimes.iter().filter(|runtime| {
            inferior.watch.watchpoints.iter().any(|(id, record)| {
                matches!(record.watchpoint.scope, WatchScope::Interpreted { runtime: watched, .. } if watched == runtime.id)
                    && frame_slots.get(id).copied().unwrap_or(true)
            })
        }) {
            for replacer in runtime.model.frame_replacers() {
                for module in mappings
                    .iter()
                    .filter(|module| module.image == runtime.module.image)
                {
                    if let Ok(address) = module.virtual_address(replacer.entry) {
                        replacers.insert(address, (runtime.id, replacer.links));
                    }
                }
            }
        }

        let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
        inferior.interpreted_moves.frame_slots = frame_slots;
        inferior.interpreted_moves.collecting.retain(|collection| {
            !collection.returned
                && watched.contains(&(debug_thread_id(collection.thread), collection.runtime))
        });
        let returns = inferior
            .interpreted_moves
            .collecting
            .iter()
            .map(|collection| collection.returns_to)
            .collect::<BTreeSet<_>>();
        let planted = inferior.interpreted_moves.planted();
        let needed = collectors
            .keys()
            .chain(replacers.keys())
            .chain(&returns)
            .copied()
            .collect::<BTreeSet<_>>();

        // The record changes with each site, so that a failed edit leaves
        // it naming exactly the sites planted.
        for &address in planted.difference(&needed) {
            remove_breakpoint_owner_from(
                &self.ptrace,
                inferior,
                address,
                BreakpointOwner::InterpretedMove,
            )?;
            inferior.interpreted_moves.forget(address);
        }

        let pid = inferior.memory_thread();
        for &address in needed.difference(&planted) {
            self.ptrace.install_breakpoint(
                pid,
                &mut inferior.breakpoints,
                address,
                BreakpointOwner::InterpretedMove,
            )?;
            if let Some(&runtime) = collectors.get(&address) {
                inferior
                    .interpreted_moves
                    .collectors
                    .insert(address, runtime);
            } else if let Some(&replacer) = replacers.get(&address) {
                inferior
                    .interpreted_moves
                    .replacers
                    .insert(address, replacer);
            } else {
                inferior.interpreted_moves.returns.insert(address);
            }
        }

        inferior.interpreted_moves.collectors = collectors;
        inferior.interpreted_moves.replacers = replacers;
        inferior.interpreted_moves.returns = returns;
        Ok(())
    }

    /// Notes a thread that reached a collector's entry or return, or a
    /// frame replacer's entry, if `address` is one and the thread's frames
    /// have watches, and stops every thread to follow it.
    pub(super) fn note_interpreted_move(
        &mut self,
        pid: Pid,
        address: VirtualAddress,
    ) -> Result<()> {
        let inferior = self.inferior.as_ref().ok_or(Error::NotRunning)?;
        let moves = &inferior.interpreted_moves;
        let thread = debug_thread_id(pid);
        let watched = watched_threads(inferior);

        if let Some(&runtime) = moves.collectors.get(&address) {
            if !watched.contains(&(thread, runtime)) {
                return Ok(());
            }
            let registers = self.ptrace.registers(pid)?;
            let returns_to = VirtualAddress::new(self.ptrace.read_word(pid, registers.rsp)?);
            record!("a collection begins on {pid}, returning to {returns_to}");
            let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
            inferior.interpreted_moves.collecting.push(Collection {
                thread: pid,
                runtime,
                returns_to,
                stack: registers.rsp.wrapping_add(8),
                places: None,
                returned: false,
            });
            return self.queue_interpreted_follow();
        }

        if let Some(&(runtime, links)) = moves.replacers.get(&address) {
            let mut frame = self.ptrace.registers(pid)?.rbp;
            for _ in 0..links {
                frame = self.ptrace.read_word(pid, frame)?;
            }
            let frame = VirtualAddress::new(frame);
            let replaced = inferior.watch.watchpoints.values().any(|record| {
                record.watchpoint.scope
                    == WatchScope::Interpreted {
                        thread,
                        runtime,
                        frame,
                    }
            });
            if !replaced {
                return Ok(());
            }
            record!("the frame at {frame} on {pid} is replaced");
            let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
            inferior
                .interpreted_moves
                .replacing
                .push((thread, runtime, frame));
            return self.queue_interpreted_follow();
        }

        if !moves.returns.contains(&address) {
            return Ok(());
        }
        let registers = self.ptrace.registers(pid)?;
        let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
        if let Some(collection) =
            inferior
                .interpreted_moves
                .collecting
                .iter_mut()
                .find(|collection| {
                    collection.thread == pid
                        && collection.returns_to == address
                        && collection.stack == registers.rsp
                        && !collection.returned
                })
        {
            record!("the collection on {pid} returned");
            collection.returned = true;
            return self.queue_interpreted_follow();
        }

        Ok(())
    }

    fn queue_interpreted_follow(&mut self) -> Result<()> {
        self.queue_internal_edit(Edit::FollowInterpreted)
    }

    /// With every thread stopped, ends the watches of replaced frames'
    /// slots, sets the watches of threads whose collections began aside,
    /// and places those whose collections returned where their frames
    /// reach their words now. A watch that could not be is ended, rather
    /// than left watching memory its object may have left.
    pub(super) fn follow_interpreted(&mut self) {
        #[cfg_attr(
            not(debug_assertions),
            expect(unused_variables, reason = "only recorded")
        )]
        if let Err(error) = self.try_follow_interpreted() {
            record!("interpreted watches could not follow their objects: {error}");
            let collecting = self.inferior.as_ref().map_or_else(Vec::new, |inferior| {
                inferior
                    .interpreted_moves
                    .collecting
                    .iter()
                    .map(|collection| (debug_thread_id(collection.thread), collection.runtime))
                    .collect()
            });
            #[cfg_attr(
                not(debug_assertions),
                expect(unused_variables, reason = "only recorded")
            )]
            if let Err(error) = self.end_interpreted_watches(
                |scope| matches!(scope, WatchScope::Interpreted { thread, runtime, .. } if collecting.contains(&(*thread, *runtime))),
                WatchpointInvalidation::ObjectMoved,
            ) {
                record!("interpreted watches could not be ended: {error}");
            }
        }
    }

    #[expect(
        clippy::too_many_lines,
        reason = "one pass sets aside, places, and ends each watch"
    )]
    fn try_follow_interpreted(&mut self) -> Result<()> {
        let mut ended = Vec::new();
        self.note_replaced_frames(&mut ended)?;
        self.note_collection_places(&mut ended)?;

        let inferior = self.inferior.as_ref().ok_or(Error::NotRunning)?;
        let mut plan = inferior.watch.plan.clone();
        let mut placed = BTreeMap::new();
        for collection in &inferior.interpreted_moves.collecting {
            let Some(noted) = &collection.places else {
                continue;
            };
            for (&id, &place) in noted {
                plan = plan.without_watchpoint(id);
                if !collection.returned {
                    continue;
                }
                let Some(record) = inferior.watch.watchpoints.get(&id) else {
                    continue;
                };
                let WatchScope::Interpreted { frame, .. } = record.watchpoint.scope else {
                    continue;
                };
                let Some(bound) = self
                    .runtimes(inferior)
                    .into_iter()
                    .find(|bound| bound.id == collection.runtime)
                else {
                    ended.push((id, WatchpointInvalidation::ObjectMoved));
                    continue;
                };
                let found = self.with_runtime_stop(inferior, &bound, collection.thread, |stop| {
                    bound.model.interpreted_placed(stop, frame, place)
                });
                let watch = match found {
                    Ok(watch) => watch,
                    #[cfg_attr(
                        not(debug_assertions),
                        expect(unused_variables, reason = "only recorded")
                    )]
                    Err(reason) => {
                        record!("watchpoint {id} cannot be placed again: {reason}");
                        ended.push((id, WatchpointInvalidation::ObjectMoved));
                        continue;
                    }
                };
                let access = super::watchpoints::slot_access(record.watchpoint.access);
                let next = debug_registers::split_range(watch.address.get(), watch.byte_size)
                    .ok()
                    .and_then(|chunks| {
                        if !record.watchpoint.enabled {
                            return Some((None, chunks));
                        }
                        plan.with_watchpoint(id, &chunks, access)
                            .ok()
                            .map(|next| (Some(next), chunks))
                    });
                match next {
                    Some((next, chunks)) => {
                        if let Some(next) = next {
                            plan = next;
                        }
                        placed.insert(id, (watch, chunks));
                    }
                    None => ended.push((id, WatchpointInvalidation::ObjectMoved)),
                }
            }
        }

        self.arm_all_threads(plan)?;

        let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
        inferior
            .interpreted_moves
            .collecting
            .retain(|collection| !collection.returned);
        let mut moved = false;
        for (id, (watch, chunks)) in &placed {
            let record = inferior
                .watch
                .watchpoints
                .get_mut(id)
                .ok_or_else(|| Error::internal("a placed watchpoint exists"))?;
            if watch.address != record.watchpoint.address {
                record!(
                    "watchpoint {id} follows its object from {} to {}",
                    record.watchpoint.address,
                    watch.address
                );
                moved = true;
            }
            record.watchpoint.address = watch.address;
            record.watchpoint.coverage = chunks
                .iter()
                .map(|chunk| AddressRange {
                    start: VirtualAddress::new(chunk.address),
                    end: VirtualAddress::new(chunk.end()),
                })
                .collect::<Vec<_>>()
                .into();
            record.code = Some(watch.code);
        }

        // The collection's rewrites of a word that names a moved object
        // are not the program's, so the value each watch reports a change
        // from is the one it finds now.
        let inferior = self.inferior.as_ref().ok_or(Error::NotRunning)?;
        let observed = placed
            .keys()
            .filter_map(|id| {
                inferior
                    .watch
                    .watchpoints
                    .get(id)
                    .map(|record| (*id, record))
            })
            .map(|(id, record)| self.observe_record(record).map(|observed| (id, observed)))
            .collect::<Result<Vec<_>>>()?;
        let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
        for (id, (bytes, value)) in observed {
            if let Some(record) = inferior.watch.watchpoints.get_mut(&id) {
                record.observed = bytes;
                record.value = value;
            }
        }

        if moved {
            self.publish_watchpoints_changed();
        }

        let invalidated = {
            let inferior = self.inferior.as_ref().ok_or(Error::NotRunning)?;
            ended
                .iter()
                .filter_map(|(id, reason)| {
                    inferior
                        .watch
                        .watchpoints
                        .get(id)
                        .map(|record| InvalidatedWatchpoint {
                            watchpoint: record.watchpoint.clone(),
                            reason: *reason,
                        })
                })
                .collect::<Vec<_>>()
        };
        self.remove_invalidated_watchpoints(invalidated)?;
        self.sync_interpreted_movers()
    }

    /// Notes for ending the watches of the slots of each frame a
    /// replacement began on, which its optimized code no longer keeps.
    fn note_replaced_frames(
        &mut self,
        ended: &mut Vec<(WatchpointId, WatchpointInvalidation)>,
    ) -> Result<()> {
        let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
        let replacing = std::mem::take(&mut inferior.interpreted_moves.replacing);
        let inferior = self.inferior.as_ref().ok_or(Error::NotRunning)?;
        for (thread, runtime, frame) in replacing {
            let Some(bound) = self
                .runtimes(inferior)
                .into_iter()
                .find(|bound| bound.id == runtime)
            else {
                continue;
            };
            let reader = debug_pid(thread)?;
            for (&id, record) in &inferior.watch.watchpoints {
                if record.watchpoint.scope
                    != (WatchScope::Interpreted {
                        thread,
                        runtime,
                        frame,
                    })
                {
                    continue;
                }
                let place = self.with_runtime_stop(inferior, &bound, reader, |stop| {
                    bound
                        .model
                        .interpreted_place(stop, frame, record.watchpoint.address)
                });
                // A context the frame reaches stays where it is, and is
                // still the variable's after the frame's code changes.
                if matches!(place, Ok(InterpretedPlace::Frame(_))) {
                    ended.push((id, WatchpointInvalidation::FrameReplaced));
                }
            }
        }
        Ok(())
    }

    /// Notes how the frame of each watch a collection that just began sets
    /// aside reaches its word, ending a watch whose frame ended or reaches
    /// it no more.
    fn note_collection_places(
        &mut self,
        ended: &mut Vec<(WatchpointId, WatchpointInvalidation)>,
    ) -> Result<()> {
        let inferior = self.inferior.as_ref().ok_or(Error::NotRunning)?;
        let mut noted = Vec::new();
        for (index, collection) in inferior.interpreted_moves.collecting.iter().enumerate() {
            if collection.places.is_some() {
                continue;
            }
            let Some(bound) = self
                .runtimes(inferior)
                .into_iter()
                .find(|bound| bound.id == collection.runtime)
            else {
                continue;
            };
            let thread = debug_thread_id(collection.thread);
            let mut places = BTreeMap::new();
            for (&id, record) in &inferior.watch.watchpoints {
                let WatchScope::Interpreted {
                    thread: owner,
                    runtime,
                    frame,
                } = record.watchpoint.scope
                else {
                    continue;
                };
                if owner != thread
                    || runtime != collection.runtime
                    || ended.iter().any(|(ending, _)| *ending == id)
                {
                    continue;
                }
                if matches!(
                    self.interpreted_watch_live(inferior, record, false)?,
                    Some(false)
                ) {
                    ended.push((id, WatchpointInvalidation::ScopeExited));
                    continue;
                }
                let place = self.with_runtime_stop(inferior, &bound, collection.thread, |stop| {
                    bound
                        .model
                        .interpreted_place(stop, frame, record.watchpoint.address)
                });
                match place {
                    Ok(place) => {
                        places.insert(id, place);
                    }
                    #[cfg_attr(
                        not(debug_assertions),
                        expect(unused_variables, reason = "only recorded")
                    )]
                    Err(reason) => {
                        record!("watchpoint {id} cannot follow the collection: {reason}");
                        ended.push((id, WatchpointInvalidation::ObjectMoved));
                    }
                }
            }
            noted.push((index, places));
        }
        let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
        for (index, places) in noted {
            if let Some(collection) = inferior.interpreted_moves.collecting.get_mut(index) {
                collection.places = Some(places);
            }
        }
        Ok(())
    }

    /// Ends the interpreted watches whose scope `ending` takes.
    fn end_interpreted_watches(
        &mut self,
        ending: impl Fn(&WatchScope) -> bool,
        reason: WatchpointInvalidation,
    ) -> Result<()> {
        let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
        inferior.interpreted_moves.collecting.clear();
        let invalidated = inferior
            .watch
            .watchpoints
            .values()
            .filter(|record| ending(&record.watchpoint.scope))
            .map(|record| InvalidatedWatchpoint {
                watchpoint: record.watchpoint.clone(),
                reason,
            })
            .collect::<Vec<_>>();
        self.remove_invalidated_watchpoints(invalidated)
    }
}

/// The threads and runtimes whose interpreted frames have watches.
fn watched_threads(inferior: &Inferior) -> BTreeSet<(ThreadId, RuntimeId)> {
    inferior
        .watch
        .watchpoints
        .values()
        .filter_map(|record| match record.watchpoint.scope {
            WatchScope::Interpreted {
                thread, runtime, ..
            } => Some((thread, runtime)),
            _ => None,
        })
        .collect()
}
