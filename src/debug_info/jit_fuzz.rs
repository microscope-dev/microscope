//! The fuzz harness for what JIT compilers say about the code they
//! generate: a perf map, and the GDB JIT interface's list of objects, read
//! from a process's memory. The input's first byte says how much of the
//! rest is the map; what follows it is the process's memory, at [`BASE`].

use std::path::Path;
use std::sync::Arc;

use super::gdb_jit;
use super::perf_map::{self, Jitted};
use crate::unwind::{DwarfRegister, MemoryReader, RegisterFile};
use crate::{AddressRange, ImageAddress, ModuleImageId, VirtualAddress};

/// Where the input's memory lies in the process.
const BASE: u64 = 0x1000;

/// The most bytes of one function's code that are read.
const MOST_CODE: u64 = 4096;

struct Memory<'a>(&'a [u8]);

impl Memory<'_> {
    fn read(&self, address: u64, size: usize) -> Option<Vec<u8>> {
        let start = usize::try_from(address.checked_sub(BASE)?).ok()?;
        self.0
            .get(start..start.checked_add(size)?)
            .map(<[u8]>::to_vec)
    }
}

impl MemoryReader for Memory<'_> {
    fn read_u64(&mut self, address: VirtualAddress) -> Option<u64> {
        let bytes = self.read(address.get(), 8)?;
        Some(u64::from_le_bytes(bytes.try_into().ok()?))
    }
}

pub fn fuzz(data: &[u8]) {
    let Some((&split, rest)) = data.split_first() else {
        return;
    };

    let (map, memory) = rest.split_at((usize::from(split) * 8).min(rest.len()));
    let memory = Memory(memory);
    perf_maps(map, &memory);
    jit_objects(&memory);
}

/// Loads the functions a perf map names, with their code read from
/// `memory`, as both kinds of module, and unwinds from each.
fn perf_maps(map: &[u8], memory: &Memory<'_>) {
    let (entries, complete) = perf_map::parse(map);
    debug_assert!(complete <= map.len());
    debug_assert!(entries.iter().all(|entry| entry.start < entry.end));

    let jitted: Vec<Jitted> = entries
        .into_iter()
        .take(64)
        .map(|entry| {
            let length = (entry.end - entry.start).min(MOST_CODE);
            let code = usize::try_from(length)
                .ok()
                .and_then(|length| memory.read(entry.start, length))
                .map(Arc::from);
            Jitted { entry, code }
        })
        .collect();

    let Some(start) = jitted.iter().map(|function| function.entry.start).min() else {
        return;
    };
    let end = jitted
        .iter()
        .map(|function| function.entry.end)
        .max()
        .unwrap_or(start);
    let range = AddressRange {
        start: ImageAddress::new(start),
        end: ImageAddress::new(end),
    };

    let path = Path::new("/tmp/perf-1.map");
    let registers = RegisterFile::new(
        [(6, BASE + 0x40), (7, BASE + 0x80), (16, start)]
            .map(|(register, value)| (DwarfRegister::new(register), value)),
    );
    for info in [
        perf_map::load(path, ModuleImageId::new(0), range, &jitted),
        perf_map::load_runtime_code(path, ModuleImageId::new(0), range, &jitted),
    ]
    .into_iter()
    .flatten()
    {
        for function in &jitted {
            let Jitted { entry, .. } = function;
            for address in [entry.start, entry.start + 1, entry.end - 1, entry.end] {
                let address = ImageAddress::new(address);
                let _ = info.image.symbolize(address);
                let _ = info.unwind.cfa(address, &registers, &mut Memory(memory.0));
                let _ = info
                    .unwind
                    .unwind(address, &registers, &mut Memory(memory.0));
            }
        }
    }
}

/// Reads the objects the descriptor at the start of `memory` lists, then
/// again with what the first read knew, as a reader does at its next stop.
fn jit_objects(memory: &Memory<'_>) {
    let mut witness = Vec::new();
    gdb_jit::witness(BASE, |at, size| memory.read(at, size), &mut witness);

    let mut registered = gdb_jit::Registered::default();
    let mut budget = 1 << 20;
    gdb_jit::registered(
        BASE,
        &mut budget,
        |at, size| memory.read(at, size),
        &mut registered,
    );
    debug_assert!(budget <= 1 << 20);

    for object in &registered.objects {
        let _ = object.name();
        let _ = object.code();
        if let Some(outline) = gdb_jit::outline(object.bytes.len() as u64, |at, size| {
            memory.read(object.address.checked_add(at)?, size)
        }) {
            registered.known.insert(object.address, outline);
        }
    }

    registered.objects.clear();
    let mut budget = 1 << 20;
    gdb_jit::registered(
        BASE,
        &mut budget,
        |at, size| memory.read(at, size),
        &mut registered,
    );
}
