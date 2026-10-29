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
    pub path: Arc<str>,
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
    path: Arc<str>,

    /// Whether the program may define sites as it runs (ABI 2).
    defines: bool,

    /// How many of its run-time sites have been read.
    defined: u32,

    /// Whether it was said that the program defined every run-time site
    /// its table holds, so that the sites it defines after are not.
    full: bool,

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

    /// Whether the recording is ending, and so switches no site on.
    finishing: bool,
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
            finishing: false,
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
            if !self.instances[index].on && self.wants_on(index) {
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

        // Once the control block says microscope has stopped, no site the
        // program defines switches itself on, and the last read below
        // finds every site defined before.
        self.finishing = true;
        for instance in &self.instances {
            if instance.on {
                let _ = perf::write_memory(self.pid, instance.control + 12, &0_u32.to_le_bytes());
            }

            for &state in &instance.states {
                let _ = perf::write_memory(self.pid, state, &[0]);
            }
        }

        let known: Vec<usize> = self
            .instances
            .iter()
            .map(|instance| instance.states.len())
            .collect();
        self.drain(processor);
        if self.gone {
            return;
        }

        // The sites the program defined since, which it may have switched
        // on as it defined them.
        for (instance, known) in self.instances.iter().zip(known) {
            for &state in instance.states.iter().skip(known) {
                let _ = perf::write_memory(self.pid, state, &[0]);
            }
        }
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
            path: Arc::clone(&found.path),
            defines: found.table.abi >= 2,
            defined: 0,
            full: false,
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
        if self.wants_on(index) {
            self.switch_on(index);
        }
    }

    /// Whether a module's control block is to be switched on: once a site
    /// it selects is known, or, recording every site, once the module may
    /// define one, which is then on from its definition.
    fn wants_on(&self, index: usize) -> bool {
        let instance = &self.instances[index];
        !self.finishing
            && (!instance.states.is_empty()
                || (instance.defines && self.selection == ProbeSelection::All))
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

        let most = u32::try_from(abi::MOST_RUNTIME_SITES).unwrap_or(u32::MAX);
        if count >= most && !self.instances[index].full {
            self.instances[index].full = true;
            let text = format!(
                "{}: the program defined all {most} of its run-time sites, so the sites \
                 it defines after them record nothing",
                self.instances[index].path
            );
            processor.probe_problem(&text);
        }

        let pid = self.pid;
        let vanished = std::cell::Cell::new(false);
        let text = |address: u64, length: u32| {
            let mut bytes = vec![0; length as usize];
            match perf::read_memory(pid, address, &mut bytes) {
                Ok(read) => (read == bytes.len()).then_some(bytes),
                Err(error) => {
                    vanished.set(vanished.get() || gone(&error));
                    None
                }
            }
        };

        let mut switched = Vec::new();

        while self.instances[index].defined < count {
            let site = self.instances[index].defined;
            self.instances[index].defined += 1;
            let mut record = [0; abi::RUNTIME_SITE_SIZE];
            let address = probes::runtime_site_address(table, site);
            let read = match perf::read_memory(pid, address, &mut record) {
                Ok(read) => probes::runtime_site(table, site, &record[..read], text),
                // A process that has gone broke nothing.
                Err(error) if gone(&error) => {
                    self.gone = true;
                    return;
                }
                Err(error) => Err(error.to_string()),
            };

            let site = match read {
                Ok(site) => site,
                // Its texts went with the process, which broke nothing.
                Err(_) if vanished.get() => {
                    self.gone = true;
                    return;
                }
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
            && !self.finishing
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

        // A program that switched itself on wrote its records for this
        // recording, and they are read from the first its rings still hold.
        let armed = registry.0.armed;
        for address in registry.1 {
            if let Ok(Some(header)) = self.header(address) {
                if header.state == abi::RING_RETIRED && !armed {
                    let _ =
                        perf::write_memory(self.pid, address + 12, &abi::RING_FREE.to_le_bytes());
                }

                rings.insert(
                    address,
                    Ring {
                        tail: if armed { 0 } else { header.head },
                        thread: None,
                        interrupted: if armed { 0 } else { header.interrupted },
                        before: !armed,
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

        // Recording every site, a run-time site is on from its definition,
        // not only once the collector next reads it.
        let enable = if self.selection == ProbeSelection::All {
            abi::ENABLED | abi::ENABLE_DEFINED
        } else {
            abi::ENABLED
        };

        let instance = &mut self.instances[index];
        instance.dropped = if armed { 0 } else { registry.0.dropped };
        instance.rings = rings;
        let on = writes
            .iter()
            .all(|(address, bytes)| perf::write_memory(self.pid, *address, bytes).is_ok())
            && instance
                .states
                .iter()
                .all(|&state| perf::write_memory(self.pid, state, &[1]).is_ok())
            && perf::write_memory(self.pid, control + 12, &enable.to_le_bytes()).is_ok();
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
        if block.enable == 0 && !self.finishing {
            // Only the program clears it while microscope reads, once it
            // has gone too long without a read. The next drain switches
            // its sites on again.
            instance.on = false;
            let text = format!(
                "{}: the program switched its sites off when microscope went more than {} ms \
                 without reading them, and they recorded nothing until they were switched on again",
                instance.path, self.timeout
            );
            processor.probe_problem(&text);
        }

        // A program that exits defined its last sites before it said so,
        // perhaps since this drain read its definitions: their records are
        // in the rings read below.
        if block.exiting && instance.defines {
            self.define(processor, index);
            if self.gone {
                return false;
            }
        }

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
        if let Some((tid, row)) = ring.thread
            && tid != header.thread
        {
            processor.probe_thread_ended(row);
            ring.thread = None;
        }

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
        ring.before |= overwritten > 0;

        if copied < records.len() {
            processor
                .builder()
                .count(AccountKind::ProbeMalformed as u32, 1);
        } else {
            processor
                .builder()
                .count(AccountKind::ProbeOverwritten as u32, overwritten);
            self.decoder.clear();
            self.decoder.decode(&records[skip..]);
            self.apply(processor, index, address, header);
        }

        self.buffer = records;
        if header.state == abi::RING_RETIRED {
            let _ = perf::write_memory(pid, address + 12, &abi::RING_FREE.to_le_bytes());
            if let Some(ring) = self.instances[index].rings.get_mut(&address) {
                if let Some((_, thread)) = ring.thread.take() {
                    processor.probe_thread_ended(thread);
                }
                ring.before = false;
            }
        }
    }

    /// Hands the decoded events of one thread's ring to the processor.
    ///
    /// A site the program defined since its definitions were last read,
    /// switched on as it was, may have records here already. The program
    /// counts a site before it records one, so a read of the definitions
    /// after the ring's finds every site the ring's records name.
    fn apply(&mut self, processor: &mut Processor, index: usize, address: u64, header: RingHeader) {
        let instance = &self.instances[index];
        if instance.defines
            && self
                .decoder
                .events
                .iter()
                .any(|decoded| !instance.sites.contains_key(&decoded.key))
        {
            self.define(processor, index);
        }

        let decoder = &mut self.decoder;
        let builder = processor.builder();
        builder.count(AccountKind::ProbeMalformed as u32, decoder.malformed);
        builder.count(AccountKind::ProbeOverwritten as u32, decoder.orphaned);
        (decoder.malformed, decoder.orphaned) = (0, 0);
        let mut unknown = 0;
        let instance = &mut self.instances[index];
        let ring = instance.rings.entry(address).or_default();

        for decoded in &decoder.events {
            let Some(&site) = instance.sites.get(&decoded.key) else {
                unknown += 1;
                continue;
            };

            let time = if header.clock == abi::CLOCK_TSC {
                self.anchors.monotonic(decoded.time)
            } else {
                decoded.time
            }
            .cast_signed();

            // Perf may already have reported the thread's exit. Resolve its
            // lifetime at the event's time, not at the later drain time.
            let (_, thread) = *ring.thread.get_or_insert_with(|| {
                (
                    header.thread,
                    processor.probe_thread(Pid::new(self.pid), header.thread, time),
                )
            });
            if std::mem::take(&mut ring.before) {
                processor.probe_lost(thread);
            }

            processor.probe_event(
                thread,
                &Event {
                    time,
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

    /// Perf can report a worker's exit before its ring is first read.
    /// Its zones still belong to the named worker, on either SDK clock.
    #[test]
    fn zones_read_after_a_workers_exit_keep_its_name() {
        for clock in [abi::CLOCK_MONOTONIC, abi::CLOCK_TSC] {
            for exited in [false, true] {
                let pid = Pid::new(std::process::id());
                let tid = Tid::new(7);
                let mut processor = Processor::new(DebugFileSearch::default(), 64);
                // PERF_RECORD_COMM, followed by its sample_id trailer.
                let comm = |tid: Tid, time: u64| {
                    let mut bytes = [0; 64];
                    bytes[..4].copy_from_slice(&3_u32.to_le_bytes());
                    bytes[6..8].copy_from_slice(&64_u16.to_le_bytes());
                    bytes[8..12].copy_from_slice(&pid.get().to_le_bytes());
                    bytes[12..16].copy_from_slice(&tid.get().to_le_bytes());
                    bytes[16..23].copy_from_slice(b"worker\0");
                    bytes[40..48].copy_from_slice(&time.to_le_bytes());
                    bytes
                };
                processor.ingest(&comm(tid, 1_000), None);
                if exited {
                    // PERF_RECORD_EXIT puts the thread ID after the parent PID.
                    let mut exit = comm(tid, 2_000);
                    exit[..4].copy_from_slice(&4_u32.to_le_bytes());
                    exit[16..20].copy_from_slice(&tid.get().to_le_bytes());
                    processor.ingest(&exit, None);
                }
                processor.ingest(&comm(pid.leader(), 3_000), None);
                let site = processor.builder().site(
                    "work",
                    "",
                    0,
                    crate::capture::records::NONE,
                    abi::SITE_ZONE,
                );

                let mut bytes =
                    vec![0; abi::RING_HEADER + (1 << abi::LEAST_RING_SIZE) * abi::RECORD];
                bytes[..4].copy_from_slice(&abi::RING_MAGIC.to_le_bytes());
                bytes[4] = abi::RING_VERSION;
                bytes[5] = u8::try_from(abi::LEAST_RING_SIZE).unwrap();
                bytes[6] = u8::try_from(clock).unwrap();
                bytes[8..12].copy_from_slice(&tid.get().to_le_bytes());
                bytes[12..16].copy_from_slice(&abi::RING_ACTIVE.to_le_bytes());
                let address = bytes.as_ptr() as u64;
                let mut collector = Collector::new(
                    pid.get(),
                    ProbeSelection::All,
                    std::time::Duration::from_secs(1),
                );
                collector.anchors = Anchors::new((10_000, 1_000));
                collector.anchors.push((20_000, 2_000));
                collector.instances.push(Instance {
                    control: 0,
                    module: 0,
                    path: "probes".into(),
                    defines: false,
                    defined: 0,
                    full: false,
                    on: true,
                    sites: HashMap::from_iter([(1, site)]),
                    states: Vec::new(),
                    rings: BTreeMap::new(),
                    dropped: 0,
                });

                // Seeing an empty ring must not choose the thread's lifetime
                // from the drain time before an event supplies its own time.
                collector.read_ring(&mut processor, 0, address);
                for (index, (time, kind)) in [(1_100_u64, abi::ZONE_BEGIN), (1_900, abi::ZONE_END)]
                    .into_iter()
                    .enumerate()
                {
                    let time = if clock == abi::CLOCK_TSC {
                        time * 10
                    } else {
                        time
                    };
                    let offset = abi::RING_HEADER + index * abi::RECORD;
                    bytes[offset..offset + 8].copy_from_slice(&time.to_le_bytes());
                    bytes[offset + 8..offset + 12].copy_from_slice(&1_u32.to_le_bytes());
                    bytes[offset + 12] = kind;
                }
                bytes[64..72].copy_from_slice(&2_u64.to_le_bytes());
                bytes[12..16].copy_from_slice(&abi::RING_RETIRED.to_le_bytes());
                collector.read_ring(&mut processor, 0, address);

                let profile = crate::profiler::Profile {
                    capture: processor.finish(),
                };
                let profile = crate::profiler::Profile::from_bytes(&profile.to_bytes().unwrap())
                    .unwrap()
                    .profile;
                let threads = profile.threads();
                assert_eq!(threads.len(), 1, "clock={clock}, exited={exited}");
                assert_eq!(threads[0].tid, tid.get());
                assert_eq!(
                    threads[0].name.as_deref(),
                    Some("worker"),
                    "clock={clock}, exited={exited}"
                );
                let zones = profile.zones();
                assert_eq!(zones.len(), 1);
                assert_eq!((zones[0].start, zones[0].end), (1_100, 1_900));
                assert_eq!(zones[0].flags, 0);
            }
        }
    }

    /// The instrumentation of process `pid`, which runs `program`, as a
    /// recording finds it, its module one of `processor`'s.
    fn found(pid: u32, program: &std::path::Path, processor: &mut Processor) -> Found {
        use object::{Object as _, ObjectSegment as _};

        let bytes = std::fs::read(program).expect("run `just build-test-programs`");
        let file = object::File::parse(bytes.as_slice()).expect("an ELF file");
        let table = probes::table(&file)
            .expect("its instrumentation")
            .expect("the probes SDK's control block");
        // The mapping of the file's start is its first segment's.
        let first = file
            .segments()
            .map(|segment| segment.address())
            .min()
            .expect("a segment");
        // The child may not have mapped the program yet when `spawn` returns.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let bias = loop {
            let maps = std::fs::read_to_string(format!("/proc/{pid}/maps")).expect("its mappings");
            let start = maps.lines().find_map(|line| {
                let fields: Vec<&str> = line.split_whitespace().collect();
                (fields.len() == 6
                    && fields[2] == "00000000"
                    && std::path::Path::new(fields[5]) == program)
                    .then(|| fields[0].split_once('-'))
                    .flatten()
                    .and_then(|(start, _)| u64::from_str_radix(start, 16).ok())
            });
            if let Some(start) = start {
                let bias = crate::LoadBias::between(
                    crate::ImageAddress::new(first & !0xfff),
                    crate::VirtualAddress::new(start),
                );
                let control = bias.wrapping_load(crate::ImageAddress::new(table.control));
                let mut block = [0; abi::CONTROL_SIZE];
                if perf::read_memory(pid, control.get(), &mut block)
                    .is_ok_and(|read| probes::control(&block[..read]).is_ok())
                {
                    break bias;
                }
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the mapping of the program's probe control block"
            );
            std::thread::sleep(std::time::Duration::from_millis(1));
        };
        Found {
            module: processor.builder().module("probes", None, [0; 16], 0),
            path: "probes".into(),
            bias,
            table: Arc::new(table),
        }
    }

    /// The first segment can be mapped before the control block is: a
    /// header-only mapping must not make the program ready for collection.
    #[test]
    fn a_program_is_found_though_it_is_not_yet_mapped() {
        use std::io::BufRead as _;

        let program = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("build/test-programs/probes")
            .canonicalize()
            .expect("run `just build-test-programs`");
        let python = std::path::PathBuf::from(
            std::env::var_os("MICROSCOPE_CPYTHON").expect("run tests in the dev shell"),
        )
        .join("3.14/python");
        let mut child = std::process::Command::new(python)
            .args([
                "-c",
                "import mmap, os, sys, time
file = open(sys.argv[1], 'rb')
header = mmap.mmap(file.fileno(), 4096, prot=mmap.PROT_READ)
print('MAPPED', flush=True)
time.sleep(0.3)
os.execv(sys.argv[1], [sys.argv[1], 'records'])",
            ])
            .arg(&program)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .expect("the loader");
        let mut lines = std::io::BufReader::new(child.stdout.take().expect("its output")).lines();
        assert_eq!(lines.next().expect("a line").expect("its text"), "MAPPED");
        let mut processor = record::Processor::new(DebugFileSearch::default(), 64);
        let found = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let found = found(child.id(), &program, &mut processor);
            let control = found
                .bias
                .wrapping_load(crate::ImageAddress::new(found.table.control));
            let mut block = [0; abi::CONTROL_SIZE];
            let read = perf::read_memory(child.id(), control.get(), &mut block)
                .expect("the control block is mapped");
            probes::control(&block[..read]).expect("the control block is valid");
        }));
        drop(child.stdin.take());
        child.wait().expect("its exit");
        assert!(found.is_ok(), "the program was not found");
    }

    /// A site the program defines as a recording ends is off once it has
    /// ended, though recording every site switched it on as it was
    /// defined: the recording's last read switches no site on, and the
    /// sites it finds are switched off.
    #[test]
    fn a_site_defined_as_a_recording_ends_is_off_once_it_has() {
        use std::io::{BufRead as _, Write as _};

        let program = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("build/test-programs/probes")
            .canonicalize()
            .expect("run `just build-test-programs`");
        let mut child = std::process::Command::new(&program)
            .arg("defines")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .expect("the program");
        let pid = child.id();
        let mut names = child.stdin.take().expect("its input");
        let mut lines = std::io::BufReader::new(child.stdout.take().expect("its output")).lines();
        let mut define = |name: &str| {
            writeln!(names, "{name}").expect("name a site");
            let line = lines.next().expect("a site").expect("its line");
            let address = line.strip_prefix("SITE 0x").expect("a site's address");
            u64::from_str_radix(address, 16).expect("an address")
        };
        let state = |address: u64| {
            let mut byte = [0];
            perf::read_memory(pid, address, &mut byte).expect("its state");
            byte[0]
        };

        let early = define("early");
        let mut processor = record::Processor::new(DebugFileSearch::default(), 64);
        let mut collector = Collector::new(
            pid,
            ProbeSelection::All,
            std::time::Duration::from_secs(600),
        );
        let found = found(pid, &program, &mut processor);
        collector.adopt(&mut processor, &found);
        collector.drain(&mut processor);
        assert_eq!(state(early), 1);

        let late = define("late");
        assert_eq!(
            state(late),
            1,
            "recording every site, a site is on from its definition"
        );
        collector.finish(&mut processor, true);
        let states = (state(early), state(late));
        drop(names);
        child.wait().expect("its exit");
        assert_eq!(states, (0, 0), "a site is left on");
    }

    /// A program whose sites switched themselves off, as they do when
    /// microscope goes too long without reading them, is switched on again
    /// at the next read, which says what was lost.
    #[test]
    fn sites_that_switched_themselves_off_are_switched_on_again() {
        use std::io::{BufRead as _, Write as _};

        let program = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("build/test-programs/probes")
            .canonicalize()
            .expect("run `just build-test-programs`");
        let mut child = std::process::Command::new(&program)
            .arg("defines")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .expect("the program");
        let pid = child.id();
        let mut names = child.stdin.take().expect("its input");
        let mut lines = std::io::BufReader::new(child.stdout.take().expect("its output")).lines();
        writeln!(names, "work").expect("name a site");
        let line = lines.next().expect("a site").expect("its line");
        let site = u64::from_str_radix(line.strip_prefix("SITE 0x").expect("an address"), 16)
            .expect("an address");
        let state = || {
            let mut byte = [0];
            perf::read_memory(pid, site, &mut byte).expect("its state");
            byte[0]
        };

        let mut processor = record::Processor::new(DebugFileSearch::default(), 64);
        let found = found(pid, &program, &mut processor);
        let control = found
            .bias
            .wrapping_load(crate::ImageAddress::new(found.table.control));
        let mut collector = Collector::new(
            pid,
            ProbeSelection::All,
            std::time::Duration::from_secs(600),
        );
        collector.adopt(&mut processor, &found);
        assert_eq!(state(), 1);

        // What the program does once microscope has gone quiet.
        perf::write_memory(pid, control.get() + 12, &0_u32.to_le_bytes()).expect("disable");
        perf::write_memory(pid, site, &[0]).expect("switch the site off");
        collector.drain(&mut processor);
        collector.drain(&mut processor);
        let on = state();
        drop(names);
        child.wait().expect("its exit");
        assert_eq!(on, 1, "the site stays off");
        let problems = processor.snapshot().markers;
        assert_eq!(problems.len(), 1, "{problems:?}");
    }

    /// A program that defines a site and records it just before it exits
    /// has both read before it is told it may go: a read that finds it
    /// exiting reads its definitions again, though a read began just
    /// before it defined the site.
    #[test]
    fn a_site_defined_as_the_program_exits_is_read_with_its_records() {
        use std::io::{BufRead as _, Write as _};

        let program = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("build/test-programs/probes")
            .canonicalize()
            .expect("run `just build-test-programs`");
        let mut child = std::process::Command::new(&program)
            .arg("exiting")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .expect("the program");
        let pid = child.id();
        let mut processor = record::Processor::new(DebugFileSearch::default(), 64);
        let found = found(pid, &program, &mut processor);
        let control = found
            .bias
            .wrapping_load(crate::ImageAddress::new(found.table.control))
            .get();
        let mut collector = Collector::new(
            pid,
            ProbeSelection::All,
            std::time::Duration::from_secs(600),
        );
        collector.adopt(&mut processor, &found);
        collector.drain(&mut processor);
        let mut lines = std::io::BufReader::new(child.stdout.take().expect("its output")).lines();
        assert_eq!(
            lines.next().expect("a line").expect("its text"),
            "LISTENING"
        );

        // The drain's definitions are read; then the program defines a
        // site, records it, and waits as it exits.
        writeln!(child.stdin.take().expect("its input")).expect("let it go");
        assert_eq!(lines.next().expect("a line").expect("its text"), "DONE");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let mut word = [0; 4];
            perf::read_memory(pid, control + 44, &mut word).expect("its control block");
            if word != [0; 4] {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the program never exited"
            );
            std::thread::yield_now();
        }
        assert!(collector.read_instance(&mut processor, 0));
        child.wait().expect("its exit");

        let builder = processor.builder();
        assert_eq!(builder.account(AccountKind::ProbeMalformed as u32), 0);
        assert_eq!(builder.account(AccountKind::ProbeEvents as u32), 2);
    }

    /// A program that goes on running defines a site and records it
    /// between a read's definitions and its rings: the records name a
    /// site the read has not seen, which it reads before it counts them.
    #[test]
    fn a_site_defined_between_the_reads_of_definitions_and_rings_is_read_with_its_records() {
        use std::io::{BufRead as _, Write as _};

        let program = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("build/test-programs/probes")
            .canonicalize()
            .expect("run `just build-test-programs`");
        let mut child = std::process::Command::new(&program)
            .arg("records")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .expect("the program");
        let pid = child.id();
        let mut processor = record::Processor::new(DebugFileSearch::default(), 64);
        let found = found(pid, &program, &mut processor);
        let mut collector = Collector::new(
            pid,
            ProbeSelection::All,
            std::time::Duration::from_secs(600),
        );
        collector.adopt(&mut processor, &found);
        collector.drain(&mut processor);

        // The drain's definitions are read; then the program defines a
        // site and records it, and the rings are read.
        let mut names = child.stdin.take().expect("its input");
        let mut lines = std::io::BufReader::new(child.stdout.take().expect("its output")).lines();
        writeln!(names, "late").expect("name a site");
        assert_eq!(lines.next().expect("a line").expect("its text"), "RECORDED");
        let read = collector.read_instance(&mut processor, 0);
        drop(names);
        child.wait().expect("its exit");

        assert!(read);
        let builder = processor.builder();
        assert_eq!(builder.account(AccountKind::ProbeMalformed as u32), 0);
        assert_eq!(builder.account(AccountKind::ProbeEvents as u32), 2);
    }

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
            path: "probes".into(),
            defines: false,
            defined: 0,
            full: false,
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
