//! Reading a program's instrumentation from outside it: switching its
//! sites on, draining its threads' rings as the recording goes, and
//! switching them off when it ends. The program is never stopped and
//! never waits, except as it exits, for a moment, to be read to its end.

use std::collections::BTreeMap;
use std::sync::Arc;

use foldhash::HashMap;
use microscope_probes::abi;

use super::probes::{self, Anchors, Decoder, Event, RingHeader, Table};
use super::record::Processor;
use crate::backend::perf;
use crate::capture::records::AccountKind;
use crate::model::{Pid, Tid};

/// Which sites a recording switches on.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum ProbeSelection {
    Off,

    /// Every site of every module.
    #[default]
    All,

    /// The sites of these names; a name that ends in `*` is a prefix.
    Named(Vec<String>),
}

impl ProbeSelection {
    fn selects(&self, name: &str) -> bool {
        match self {
            Self::Off => false,
            Self::All => true,
            Self::Named(names) => names.iter().any(|wanted| {
                wanted
                    .strip_suffix('*')
                    .map_or(wanted == name, |prefix| name.starts_with(prefix))
            }),
        }
    }
}

impl std::fmt::Display for ProbeSelection {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Off => formatter.write_str("off"),
            Self::All => formatter.write_str("all"),
            Self::Named(names) => formatter.write_str(&names.join(",")),
        }
    }
}

/// A module the processor found mapped whose file has instrumentation.
#[derive(Debug, Clone)]
pub struct Found {
    pub module: u32,
    pub bias: crate::LoadBias,
    pub table: Arc<Table>,
}

/// What microscope knows of one ring.
#[derive(Debug, Clone, Copy, Default)]
struct Ring {
    /// The first position not yet read.
    tail: u64,

    /// The thread that writes it, and its row.
    thread: Option<(Tid, u32)>,
    interrupted: u64,

    /// Whether the thread wrote records before microscope began to read them.
    before: bool,

    /// Whether the ring broke the ABI, which is counted once.
    broken: bool,
}

/// One module's instrumentation in the recorded process.
struct Instance {
    control: u64,
    module: u32,

    /// Whether the program may define sites as it runs (ABI 2).
    defines: bool,

    /// How many of its run-time sites have been read.
    defined: u32,

    /// Whether its control block says microscope listens, which it does
    /// once any site is on.
    on: bool,

    /// Each key's site row.
    sites: HashMap<u32, u32>,

    /// The state bytes switched on.
    states: Vec<u64>,
    rings: BTreeMap<u64, Ring>,
    dropped: u64,
}

/// Reads one process's instrumentation.
pub struct Collector {
    pid: u32,
    selection: ProbeSelection,
    timeout: u32,
    instances: Vec<Instance>,
    anchors: Anchors,
    clock: u16,
    decoder: Decoder,
    buffer: Vec<u8>,

    /// Whether the process can no longer be read.
    gone: bool,
}

impl Collector {
    /// Reads process `pid`'s instrumentation, which switches itself off
    /// once microscope has not read it for `timeout`.
    pub fn new(pid: u32, selection: ProbeSelection, timeout: std::time::Duration) -> Self {
        Self {
            pid,
            selection,
            timeout: u32::try_from(timeout.as_millis())
                .unwrap_or(u32::MAX)
                .max(1),
            instances: Vec::new(),
            anchors: Anchors::new(perf::tsc_anchor()),
            clock: if perf::tsc_is_the_clock() {
                abi::CLOCK_TSC
            } else {
                abi::CLOCK_MONOTONIC
            },
            decoder: Decoder::default(),
            buffer: Vec::new(),
            gone: false,
        }
    }

    /// Switches on the sites of every module the processor has found
    /// mapped since, then reads every ring.
    pub fn drain(&mut self, processor: &mut Processor) {
        let _span = crate::span!("probes drain");
        let (executed, found) = processor.found_probes(Pid::new(self.pid));
        if executed {
            self.executed(processor);
        }

        for found in found {
            self.adopt(processor, &found);
        }

        if self.gone {
            return;
        }

        self.anchors.push(perf::tsc_anchor());
        let heartbeat = monotonic().to_le_bytes();
        for index in 0..self.instances.len() {
            if self.instances[index].defines {
                self.define(processor, index);
            }
            let instance = &self.instances[index];
            if !instance.on && !instance.states.is_empty() {
                self.switch_on(index);
            }
            if self.gone {
                return;
            }
            let instance = &self.instances[index];
            if !instance.on {
                continue;
            }
            if perf::write_memory(self.pid, instance.control + 24, &heartbeat).is_err()
                || !self.read_instance(processor, index)
            {
                self.gone = true;
                return;
            }
        }
    }

    /// Switches every site off and reads what the rings hold, if the
    /// process is still there to read.
    pub fn finish(&mut self, processor: &mut Processor, alive: bool) {
        if !alive || self.gone {
            return;
        }

        for instance in &self.instances {
            for &state in &instance.states {
                let _ = perf::write_memory(self.pid, state, &[0]);
            }

            if instance.on {
                let _ = perf::write_memory(self.pid, instance.control + 12, &0_u32.to_le_bytes());
            }
        }

        self.drain(processor);
    }

    /// Forgets the instrumentation of the program the process replaced. Its
    /// threads' zones end where their records were last read: what they
    /// wrote after cannot be read.
    fn executed(&mut self, processor: &mut Processor) {
        record!("probes: the process executed a program");
        for instance in self.instances.drain(..) {
            for ring in instance.rings.values() {
                if let Some((_, row)) = ring.thread {
                    processor.probe_thread_ended(row);
                }
            }
        }

        // The new program may be read even if the old one could not be.
        self.gone = false;
    }

    /// Takes a module's instrumentation, and switches on its sites the
    /// recording selects. A module that may define sites as it runs is
    /// kept though none is selected yet.
    fn adopt(&mut self, processor: &mut Processor, found: &Found) {
        let image = |address| crate::ImageAddress::new(address);
        let control = found.bias.wrapping_load(image(found.table.control)).get();
        let mut instance = Instance {
            control,
            module: found.module,
            defines: found.table.abi >= 2,
            defined: 0,
            on: false,
            sites: HashMap::default(),
            states: Vec::new(),
            rings: BTreeMap::new(),
            dropped: 0,
        };

        for site in &found.table.sites {
            let row = processor.probe_site(found.module, site);
            instance.sites.insert(site.key, row);
            if self.selection.selects(&site.name) {
                instance
                    .states
                    .push(found.bias.wrapping_load(image(site.state)).get());
            }
        }

        let defines = instance.defines && self.selection != ProbeSelection::Off;
        if instance.states.is_empty() && !defines {
            return;
        }

        self.instances.push(instance);
        let index = self.instances.len() - 1;
        if self.instances[index].defines {
            self.define(processor, index);
        }
        if !self.instances[index].states.is_empty() {
            self.switch_on(index);
        }
    }

    /// Reads the sites a module defined as it ran since they were last
    /// read, and switches on those the recording selects; once its
    /// control block is on, at once.
    fn define(&mut self, processor: &mut Processor, index: usize) {
        let control = self.instances[index].control;
        let mut block = [0; abi::CONTROL_SIZE];
        let table = match perf::read_memory(self.pid, control, &mut block) {
            Ok(read) => probes::control(&block[..read]).map_or(0, |block| block.sites),
            Err(error) => {
                self.gone |= gone(&error);
                return;
            }
        };
        if table == 0 {
            return;
        }

        let mut header = [0; abi::SITES_HEADER];
        let count = match perf::read_memory(self.pid, table, &mut header) {
            Ok(read) => probes::runtime_sites(&header[..read]),
            Err(error) => {
                self.gone |= gone(&error);
                return;
            }
        };
        let count = match count {
            Ok(count) => count,
            // Only the flight recorder, which release builds leave out,
            // reads the problem; the profile counts it.
            #[cfg_attr(not(debug_assertions), expect(unused_variables))]
            Err(problem) => {
                // Read once: a table that breaks the ABI stays broken.
                record!("probes: module {}: {problem}", self.instances[index].module);
                processor
                    .builder()
                    .count(AccountKind::ProbeMalformed as u32, 1);
                self.instances[index].defines = false;
                return;
            }
        };

        let pid = self.pid;
        let text = |address: u64, length: u32| {
            let mut bytes = vec![0; length as usize];
            perf::read_memory(pid, address, &mut bytes)
                .ok()
                .filter(|&read| read == bytes.len())
                .map(|_| bytes)
        };
        let mut switched = Vec::new();
        while self.instances[index].defined < count {
            let site = self.instances[index].defined;
            self.instances[index].defined += 1;
            let mut record = [0; abi::RUNTIME_SITE_SIZE];
            let address = probes::runtime_site_address(table, site);
            let read = perf::read_memory(pid, address, &mut record)
                .map_err(|error| error.to_string())
                .and_then(|read| probes::runtime_site(table, site, &record[..read], text));
            let site = match read {
                Ok(site) => site,
                #[cfg_attr(not(debug_assertions), expect(unused_variables))]
                Err(problem) => {
                    // Its records name a key no site has, and are counted.
                    record!("probes: module {}: {problem}", self.instances[index].module);
                    processor
                        .builder()
                        .count(AccountKind::ProbeMalformed as u32, 1);
                    continue;
                }
            };
            let instance = &mut self.instances[index];
            let row = processor.probe_site(instance.module, &site);
            instance.sites.insert(site.key, row);
            if self.selection.selects(&site.name) {
                instance.states.push(site.state);
                switched.push(site.state);
            }
        }

        if self.instances[index].on
            && !switched
                .iter()
                .all(|&state| perf::write_memory(pid, state, &[1]).is_ok())
        {
            self.gone = true;
        }
    }

    /// Switches a module's sites on: the rings its threads already have are
    /// read from where they are now.
    fn switch_on(&mut self, index: usize) {
        let control = self.instances[index].control;
        let Some(registry) = self.registry(control) else {
            self.gone = true;
            return;
        };

        let mut rings = BTreeMap::new();
        for address in registry.1 {
            if let Ok(Some(header)) = self.header(address) {
                if header.state == abi::RING_RETIRED {
                    let _ =
                        perf::write_memory(self.pid, address + 12, &abi::RING_FREE.to_le_bytes());
                }

                rings.insert(
                    address,
                    Ring {
                        tail: header.head,
                        thread: None,
                        interrupted: header.interrupted,
                        before: true,
                        broken: false,
                    },
                );
            }
        }

        // The sites are on before the control block says so, so that a
        // program that asks whether microscope listens may rely on them.
        let writes = [
            (control + 10, self.clock.to_le_bytes().to_vec()),
            (control + 24, monotonic().to_le_bytes().to_vec()),
            (control + 48, 0_u32.to_le_bytes().to_vec()),
            (control + 52, self.timeout.to_le_bytes().to_vec()),
        ];
        let instance = &mut self.instances[index];
        instance.dropped = registry.0.dropped;
        instance.rings = rings;
        let on = writes
            .iter()
            .all(|(address, bytes)| perf::write_memory(self.pid, *address, bytes).is_ok())
            && instance
                .states
                .iter()
                .all(|&state| perf::write_memory(self.pid, state, &[1]).is_ok())
            && perf::write_memory(self.pid, control + 12, &1_u32.to_le_bytes()).is_ok();
        record!(
            "probes: switched on {} sites of module {} at {control:#x}: {on}",
            instance.states.len(),
            instance.module
        );
        instance.on = on;
        if !on {
            self.gone = true;
        }
    }

    /// A control block, and the rings its registry lists.
    fn registry(&mut self, control: u64) -> Option<(probes::Control, Vec<u64>)> {
        self.buffer
            .resize(abi::CONTROL_SIZE + abi::MOST_RINGS * 8, 0);
        let read =
            perf::read_memory(self.pid, control, &mut self.buffer[..abi::CONTROL_SIZE]).ok()?;
        let block = probes::control(&self.buffer[..read]).ok()?;
        let length = block.rings as usize * 8;
        let slots = &mut self.buffer[abi::CONTROL_SIZE..abi::CONTROL_SIZE + length];
        let read = perf::read_memory(self.pid, control + abi::CONTROL_SIZE as u64, slots).ok()?;
        let rings = slots[..read]
            .as_chunks::<8>()
            .0
            .iter()
            .map(|slot| u64::from_le_bytes(*slot))
            .filter(|&address| address != 0)
            .collect();
        Some((block, rings))
    }

    /// A ring's header, or `None` if it breaks the ABI; `Err` if the
    /// process has gone.
    fn header(&self, address: u64) -> Result<Option<RingHeader>, Gone> {
        let mut bytes = [0; abi::RING_HEADER];
        match perf::read_memory(self.pid, address, &mut bytes) {
            Ok(read) => Ok(probes::ring_header(&bytes[..read]).ok()),
            Err(error) if gone(&error) => Err(Gone),
            Err(_) => Ok(None),
        }
    }

    /// Reads every ring of one instance: whether the process could be read.
    fn read_instance(&mut self, processor: &mut Processor, index: usize) -> bool {
        let control = self.instances[index].control;
        let Some((block, rings)) = self.registry(control) else {
            return false;
        };

        let instance = &mut self.instances[index];
        let dropped = block.dropped.saturating_sub(instance.dropped);
        instance.dropped = instance.dropped.max(block.dropped);
        processor
            .builder()
            .count(AccountKind::ProbeNoRing as u32, dropped);
        for address in rings {
            self.read_ring(processor, index, address);
            if self.gone {
                return false;
            }
        }

        if block.exiting {
            record!("probes: the program is exiting; every ring is read");
            let _ = perf::write_memory(self.pid, control + 48, &1_u32.to_le_bytes());
        }

        true
    }

    /// Reads the records a ring has taken since it was last read. A ring
    /// that cannot be read once the process has gone is not malformed.
    fn read_ring(&mut self, processor: &mut Processor, index: usize, address: u64) {
        let pid = self.pid;
        let Ok(header) = self.header(address) else {
            self.gone = true;
            return;
        };

        let ring = self.instances[index].rings.entry(address).or_default();
        let Some(header) = header else {
            if !std::mem::replace(&mut ring.broken, true) {
                processor
                    .builder()
                    .count(AccountKind::ProbeMalformed as u32, 1);
            }

            return;
        };

        if !matches!(header.state, abi::RING_ACTIVE | abi::RING_RETIRED) {
            ring.tail = header.head;
            return;
        }

        // A ring a new thread took ends the zones of the thread before.
        let thread = match ring.thread {
            Some((tid, row)) if tid == header.thread => row,
            previous => {
                if let Some((_, row)) = previous {
                    processor.probe_thread_ended(row);
                }

                let row = processor.probe_thread(Pid::new(pid), header.thread);
                ring.thread = Some((header.thread, row));
                row
            }
        };

        processor.builder().count(
            AccountKind::ProbeInterrupted as u32,
            header.interrupted.saturating_sub(ring.interrupted),
        );
        ring.interrupted = ring.interrupted.max(header.interrupted);
        let (span, mut overwritten) = probes::copy_span(ring.tail, header);
        let mut records = std::mem::take(&mut self.buffer);
        records.clear();
        records.resize(probes::record_bytes(span.end - span.start), 0);
        let mut copied = 0;
        let base = address + abi::RING_HEADER as u64;
        for slots in probes::slots(&span, header.size) {
            let length = slots.len();
            if length == 0 {
                continue;
            }

            match perf::read_memory(
                pid,
                base + slots.start as u64,
                &mut records[copied..copied + length],
            ) {
                Ok(read) if read == length => copied += length,
                Err(error) if gone(&error) => {
                    self.buffer = records;
                    self.gone = true;
                    return;
                }
                _ => {}
            }
        }

        // What the thread may have written over while it was copied.
        let mut head = [0; 8];
        let moved = perf::read_memory(pid, address + 64, &mut head)
            .map_or(header.head, |_| u64::from_le_bytes(head));
        let first = probes::first_whole(moved, header.size).max(span.start);
        let skipped = first.min(span.end) - span.start;
        overwritten += skipped;
        let skip = probes::record_bytes(skipped);
        let ring = self.instances[index].rings.entry(address).or_default();
        ring.tail = span.end;
        let before = std::mem::take(&mut ring.before);
        if copied < records.len() {
            processor
                .builder()
                .count(AccountKind::ProbeMalformed as u32, 1);
        } else {
            if overwritten > 0 || before {
                processor.probe_lost(thread);
            }

            processor
                .builder()
                .count(AccountKind::ProbeOverwritten as u32, overwritten);
            self.decoder.clear();
            self.decoder.decode(&records[skip..]);
            self.apply(processor, index, thread, header.clock);
        }

        self.buffer = records;
        if header.state == abi::RING_RETIRED {
            let _ = perf::write_memory(pid, address + 12, &abi::RING_FREE.to_le_bytes());
            processor.probe_thread_ended(thread);
            if let Some(ring) = self.instances[index].rings.get_mut(&address) {
                ring.thread = None;
            }
        }
    }

    /// Hands the decoded events of one thread's ring to the processor.
    fn apply(&mut self, processor: &mut Processor, index: usize, thread: u32, clock: u16) {
        let decoder = &mut self.decoder;
        let builder = processor.builder();
        builder.count(AccountKind::ProbeMalformed as u32, decoder.malformed);
        builder.count(AccountKind::ProbeOverwritten as u32, decoder.orphaned);
        (decoder.malformed, decoder.orphaned) = (0, 0);
        let mut unknown = 0;
        for decoded in &decoder.events {
            let Some(&site) = self.instances[index].sites.get(&decoded.key) else {
                unknown += 1;
                continue;
            };

            let time = if clock == abi::CLOCK_TSC {
                self.anchors.monotonic(decoded.time)
            } else {
                decoded.time
            };

            processor.probe_event(
                thread,
                &Event {
                    time: time.cast_signed(),
                    site,
                    kind: decoded.kind,
                    value: decoded.value,
                    text: decoder.text(decoded),
                },
            );
        }

        processor
            .builder()
            .count(AccountKind::ProbeMalformed as u32, unknown);
        processor.builder().count(
            AccountKind::ProbeEvents as u32,
            decoder.events.len() as u64 - unknown,
        );
    }
}

/// The process has gone: its memory can no longer be read.
struct Gone;

fn gone(error: &std::io::Error) -> bool {
    error.raw_os_error() == Some(nix::errno::Errno::ESRCH as i32)
}

fn monotonic() -> u64 {
    nix::time::clock_gettime(nix::time::ClockId::CLOCK_MONOTONIC).map_or(0, |time| {
        u64::try_from(time.tv_sec()).unwrap_or(0) * 1_000_000_000
            + u64::try_from(time.tv_nsec()).unwrap_or(0)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::debug_info::DebugFileSearch;
    use crate::profiler::record;

    /// A process can exit between the reads of its registry and of its
    /// rings, after its last records were read. Its rings are then gone,
    /// not malformed.
    #[test]
    fn the_rings_of_a_process_that_has_gone_are_not_malformed() {
        let mut child = std::process::Command::new("true")
            .spawn()
            .expect("a process");
        let pid = child.id();
        child.wait().expect("its exit");
        let mut processor = record::Processor::new(DebugFileSearch::default(), 64);
        let mut collector =
            Collector::new(pid, ProbeSelection::All, std::time::Duration::from_secs(1));
        collector.instances.push(Instance {
            control: 0x1000,
            module: 0,
            defines: false,
            defined: 0,
            on: true,
            sites: HashMap::default(),
            states: Vec::new(),
            rings: BTreeMap::from([(0x2000, Ring::default()), (0x3000, Ring::default())]),
            dropped: 0,
        });
        for ring in [0x2000, 0x3000] {
            collector.read_ring(&mut processor, 0, ring);
        }

        assert!(collector.gone);
        assert_eq!(
            processor
                .builder()
                .account(AccountKind::ProbeMalformed as u32),
            0
        );
    }
}
