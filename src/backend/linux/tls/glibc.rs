//! Thread-local storage addresses read from glibc's structures.
//!
//! glibc exports a `_thread_db_*` descriptor for every structure field a
//! debugger needs to find a thread's TLS: the field's width in bits, its
//! element count, and its offset. This module follows glibc's own lookup,
//! `td_thr_tlsbase`, step by step, reading every offset from the inferior's
//! descriptors, so a C library of any version describes itself. glibc has
//! kept that lookup unchanged since 2.27, but for where it finds the loader's
//! state. Before 2.34, glibc defines the descriptors in libpthread, which a
//! program that creates no threads need not load, and then they are missing.
//!
//! A statically linked program contains the descriptors only with glibc's
//! thread library, and is read without them: its executable is its only TLS
//! module, which the start of glibc's thread control block locates.

use std::collections::BTreeSet;

use nix::unistd::Pid;

use super::{DESCRIPTOR_SIZE, ProcessServices, TlsError, dtv, failed, read_word, thread_pointer};

/// The C library, where glibc since 2.34 defines the descriptors. Lookups
/// fall back to any module that defines them, as libpthread does before.
const C_LIBRARY: &str = "libc.so.6";

/// The dynamic loader, which exports its own state.
const LOADER: &str = "ld-linux-x86-64.so.2";

/// A loader with more TLS slotinfo lists than this is corrupt.
const MAX_SLOTINFO_LISTS: usize = 1 << 16;

/// `TLS_DTV_UNALLOCATED` and every other odd DTV pointer mark a block the
/// thread has not allocated.
const UNALLOCATED_BIT: u64 = 1;

/// The size of a DTV slot: a block's address and the allocation to free.
const DTV_SLOT_SIZE: u64 = 16;

/// The TLS module number glibc gives a statically linked executable.
const EXECUTABLE_MODULE: u64 = 1;

/// `l_tls_offset` values that name no static TLS block.
const NO_TLS_OFFSET: u64 = 0;
const FORCED_DYNAMIC_TLS_OFFSET: u64 = u64::MAX;

/// One `_thread_db_*` field descriptor.
#[derive(Debug, Clone, Copy)]
struct Field {
    name: &'static str,
    bits: u32,

    /// The element count of an array field; zero for a flexible array.
    count: u32,
    offset: i32,
}

impl Field {
    /// The address of element `index` of this field in the structure at
    /// `base`.
    fn address(self, base: u64, index: u64) -> Result<u64, TlsError> {
        if self.count != 0 && index >= u64::from(self.count) {
            return Err(failed(format!(
                "index {index} is outside {}, which has {} elements",
                self.name, self.count
            )));
        }

        index
            .checked_mul(u64::from(self.bits / 8))
            .and_then(|element| {
                base.checked_add_signed(i64::from(self.offset))?
                    .checked_add(element)
            })
            .ok_or_else(|| failed(format!("{} at {base:#x} overflows", self.name)))
    }
}

struct Glibc<'a> {
    services: &'a dyn ProcessServices,
}

impl Glibc<'_> {
    fn symbol(&self, object: &str, name: &str) -> Result<u64, TlsError> {
        self.services
            .lookup_symbol(Some(object), name)
            .ok_or_else(|| failed(format!("the C library does not define {name}")))
    }

    fn read(&self, address: u64, output: &mut [u8]) -> Result<(), TlsError> {
        if self.services.read(address, output) {
            Ok(())
        } else {
            Err(failed(format!(
                "the C library's thread state at {address:#x} is unreadable"
            )))
        }
    }

    fn field(&self, name: &'static str) -> Result<Field, TlsError> {
        let Some(address) = self.services.lookup_symbol(Some(C_LIBRARY), name) else {
            return Err(failed(format!(
                "no loaded module defines {name}, which glibc before 2.34 defines only in \
                 libpthread"
            )));
        };

        self.field_at(name, address)
    }

    fn field_at(&self, name: &'static str, address: u64) -> Result<Field, TlsError> {
        let mut bytes = [0; DESCRIPTOR_SIZE];
        if !self.services.read_descriptor(address, &mut bytes) {
            return Err(failed(format!(
                "the C library's thread state at {address:#x} is unreadable"
            )));
        }
        let (words, _) = bytes.as_chunks::<4>();
        let word = |index: usize| -> Result<u32, TlsError> {
            let word = words
                .get(index)
                .ok_or_else(|| failed("a TLS field descriptor is truncated"))?;
            Ok(u32::from_le_bytes(*word))
        };

        let field = Field {
            name,
            bits: word(0)?,
            count: word(1)?,
            offset: i32::from_le_bytes(word(2)?.to_le_bytes()),
        };

        // A zero width marks a field this C library does not have. Element
        // widths are whole bytes, and x86-64 descriptors are never
        // byte-swapped.
        if field.bits == 0 || !field.bits.is_multiple_of(8) {
            return Err(failed(format!(
                "{name} describes an unusable {}-bit field",
                field.bits
            )));
        }

        Ok(field)
    }

    /// Reads element `index` of `field` in the structure at `base`,
    /// zero-extended.
    fn fetch(&self, field: Field, base: u64, index: u64) -> Result<u64, TlsError> {
        // Only these widths are values; wider fields are structures.
        if !matches!(field.bits, 8 | 32 | 64) {
            return Err(failed(format!(
                "{} is a {}-bit field, not a value",
                field.name, field.bits
            )));
        }

        let address = field.address(base, index)?;
        let mut bytes = [0; 8];
        let width = (field.bits / 8) as usize;
        self.read(address, &mut bytes[..width])?;
        Ok(u64::from_le_bytes(bytes))
    }

    fn fetch_named(&self, name: &'static str, base: u64, index: u64) -> Result<u64, TlsError> {
        self.fetch(self.field(name)?, base, index)
    }

    /// The loader's global state, which a dynamically linked C library
    /// since 2.34 points at and the loader exports itself before; `None`
    /// for a statically linked C library, which keeps what it holds in
    /// variables of its own.
    fn rtld_global(&self) -> Result<Option<u64>, TlsError> {
        Ok(match self.symbol(C_LIBRARY, "__nptl_rtld_global") {
            Ok(pointer) => Some(self.fetch_named("_thread_db___nptl_rtld_global", pointer, 0)?),
            Err(_) => self.symbol(LOADER, "_rtld_global").ok(),
        })
    }

    /// The head of the loader's list of DTV slotinfo arrays.
    fn slotinfo_lists(&self) -> Result<u64, TlsError> {
        if let Some(rtld_global) = self.rtld_global()? {
            return self.fetch_named(
                "_thread_db_rtld_global__dl_tls_dtv_slotinfo_list",
                rtld_global,
                0,
            );
        }

        let variable = self.symbol(C_LIBRARY, "_dl_tls_dtv_slotinfo_list")?;
        self.fetch_named("_thread_db__dl_tls_dtv_slotinfo_list", variable, 0)
    }

    /// The address of module `module`'s DTV slotinfo entry.
    fn slotinfo(&self, module: u64) -> Result<u64, TlsError> {
        let length = self.field("_thread_db_dtv_slotinfo_list_len")?;
        let next = self.field("_thread_db_dtv_slotinfo_list_next")?;
        let entries = self.field("_thread_db_dtv_slotinfo_list_slotinfo")?;
        let mut list = self.slotinfo_lists()?;
        let mut first = 0_u64;
        let mut visited = BTreeSet::new();

        while list != 0 {
            if visited.len() == MAX_SLOTINFO_LISTS || !visited.insert(list) {
                return Err(failed("the loader's TLS slotinfo lists form a cycle"));
            }

            let entries_here = self.fetch(length, list, 0)?;
            let end = first
                .checked_add(entries_here)
                .ok_or_else(|| failed("the loader's TLS slotinfo list lengths overflow"))?;
            if module < end {
                return entries.address(list, module - first);
            }

            first = end;
            list = self.fetch(next, list, 0)?;
        }

        Err(failed(format!(
            "the loader has no TLS slot for module {module}"
        )))
    }

    /// The thread's static TLS block of the module at `map`, if it has one.
    fn static_block(&self, thread_pointer: u64, map: u64) -> Result<u64, TlsError> {
        let offset = self.fetch_named("_thread_db_link_map_l_tls_offset", map, 0)?;
        if offset == NO_TLS_OFFSET || offset == FORCED_DYNAMIC_TLS_OFFSET {
            return Err(TlsError::Deferred);
        }

        // x86-64 places static blocks below the thread pointer.
        thread_pointer.checked_sub(offset).ok_or_else(|| {
            failed(format!(
                "static TLS offset {offset:#x} exceeds the thread pointer"
            ))
        })
    }

    /// Follows `td_thr_tlsbase`: the module's block in the thread's DTV
    /// when the DTV is current and the block allocated, and otherwise its
    /// static block.
    fn block(&self, thread_pointer: u64, module: u64) -> Result<u64, TlsError> {
        let entry = self.slotinfo(module)?;
        let map = self.fetch_named("_thread_db_dtv_slotinfo_map", entry, 0)?;
        if map == 0 {
            return Err(failed(format!("TLS module {module} is not loaded")));
        }

        let generation = self.fetch_named("_thread_db_dtv_slotinfo_gen", entry, 0)?;
        let dtv = self.fetch_named("_thread_db_pthread_dtvp", thread_pointer, 0)?;
        let slots = self.field("_thread_db_dtv_dtv")?;
        let counter = slots.address(dtv, 0)?;
        let dtv_generation = self.fetch_named("_thread_db_dtv_t_counter", counter, 0)?;

        // A DTV older than the module may not have its slot at all.
        if dtv_generation < generation {
            return self.static_block(thread_pointer, map);
        }

        let slot = slots.address(dtv, module)?;
        let block = self.fetch_named("_thread_db_dtv_t_pointer_val", slot, 0)?;
        if block & UNALLOCATED_BIT != 0 {
            return self.static_block(thread_pointer, map);
        }

        Ok(block)
    }
}

/// Reads the target-namespace TID from the saved thread's own TCB, using
/// an unambiguous descriptor supplied by the core's verified images.
pub(super) fn target_thread_id(
    services: &dyn ProcessServices,
    thread: Pid,
    descriptor: u64,
) -> Result<u64, TlsError> {
    let glibc = Glibc { services };
    let id = glibc.field_at("_thread_db_pthread_tid", descriptor)?;
    if id.bits != 32 || id.count != 1 {
        return Err(failed("the C library's thread id is not one pid_t"));
    }
    let pointer = thread_pointer(services, thread)?;
    let tid = glibc.fetch(id, pointer, 0)?;
    if tid == 0 || tid > i32::MAX as u64 {
        return Err(failed("the saved thread has no live target-namespace id"));
    }
    Ok(tid)
}

/// The most threads glibc's lists hold before they count as corrupt.
const MAX_THREADS: usize = 1 << 20;

/// Each live thread's id and descriptor, which x86-64 glibc keeps at the
/// thread's pointer: the loader's lists of the threads whose stacks the
/// program gave, the first thread's among them, and of those whose stacks
/// glibc allocated, walked as `td_ta_thr_iter` walks them. A thread that
/// has exited, whose id glibc cleared, is left out.
///
/// The process may be running, so a list may change while it is read: a
/// list that does not lead back to its head within the bound fails.
pub(super) fn thread_pointers(services: &dyn ProcessServices) -> Result<Vec<(u64, u64)>, TlsError> {
    let glibc = Glibc { services };
    let rtld_global = glibc.rtld_global()?.ok_or_else(|| {
        failed("a statically linked glibc keeps its thread lists in variables microscope does not read")
    })?;

    let next = glibc.field("_thread_db_list_t_next")?;
    let link = glibc.field("_thread_db_pthread_list")?;
    let id = glibc.field("_thread_db_pthread_tid")?;
    let mut threads = Vec::new();

    for list in [
        "_thread_db_rtld_global__dl_stack_user",
        "_thread_db_rtld_global__dl_stack_used",
    ] {
        let head = glibc.field(list)?.address(rtld_global, 0)?;
        let mut node = glibc.fetch(next, head, 0)?;
        let mut visited = BTreeSet::new();

        while node != head {
            if visited.len() == MAX_THREADS || !visited.insert(node) {
                return Err(failed(format!("{list} does not lead back to its head")));
            }

            let descriptor = node
                .checked_add_signed(-i64::from(link.offset))
                .ok_or_else(|| failed(format!("a thread in {list} at {node:#x} overflows")))?;

            // A pid_t: zero once the thread exited, and never negative for
            // a thread.
            let tid = glibc.fetch(id, descriptor, 0)?;
            if tid != 0 && tid <= u64::from(i32::MAX.cast_unsigned()) {
                threads.push((tid, descriptor));
            }

            node = glibc.fetch(next, node, 0)?;
        }
    }

    Ok(threads)
}

/// The address of `offset` within the TLS block of the module whose loader
/// `link_map` is given, for `thread`.
pub(super) fn tls_address(
    services: &dyn ProcessServices,
    thread: Pid,
    link_map: u64,
    offset: u64,
) -> Result<u64, TlsError> {
    let glibc = Glibc { services };
    let module = glibc.fetch_named("_thread_db_link_map_l_tls_modid", link_map, 0)?;
    if module == 0 {
        return Err(TlsError::NoTls);
    }

    // x86-64 glibc keeps the thread descriptor at the thread pointer.
    let thread_pointer = services
        .registers(thread)
        .ok_or_else(|| failed(format!("the registers of thread {thread} are unavailable")))?
        .fs_base;
    if thread_pointer == 0 {
        return Err(TlsError::Deferred);
    }

    glibc
        .block(thread_pointer, module)?
        .checked_add(offset)
        .ok_or_else(|| failed("the TLS address overflows"))
}

/// The offset below every thread's pointer of the static TLS block of the
/// module whose `link_map` is at `link_map`, if glibc gave it one, which it
/// keeps for the module's life: the loader places the block of a module
/// loaded with the program, or given static TLS later, at that offset in
/// every thread, and copies it into the threads already running.
pub(super) fn static_offset(
    services: &dyn ProcessServices,
    link_map: u64,
) -> Result<Option<u64>, TlsError> {
    let glibc = Glibc { services };
    let offset = glibc.fetch_named("_thread_db_link_map_l_tls_offset", link_map, 0)?;
    Ok((offset != NO_TLS_OFFSET && offset != FORCED_DYNAMIC_TLS_OFFSET).then_some(offset))
}

/// The address of `offset` within the TLS block of a statically linked
/// program's executable, for `thread`.
///
/// The start of glibc's x86-64 thread control block is fixed, as GCC's
/// stack protector reads its canary at offset 0x28, after the DTV pointer.
/// The DTV pointer addresses slot 0 of 16-byte slots, and slot N begins with
/// TLS module N's block. glibc numbers the executable module 1, and gives
/// every thread its block before the thread runs.
pub(super) fn static_executable_tls_address(
    services: &dyn ProcessServices,
    thread: Pid,
    offset: u64,
) -> Result<u64, TlsError> {
    let thread_pointer = thread_pointer(services, thread)?;
    let dtv = dtv(services, thread_pointer)?;
    let block = read_word(services, dtv.checked_add(EXECUTABLE_MODULE * DTV_SLOT_SIZE))?;
    if block == 0 || block & UNALLOCATED_BIT != 0 {
        return Err(TlsError::Deferred);
    }

    // The executable's block is static, so it lies below the thread pointer.
    if block >= thread_pointer {
        return Err(failed(format!(
            "the executable's TLS block {block:#x} is not below the thread pointer \
             {thread_pointer:#x}"
        )));
    }

    block
        .checked_add(offset)
        .ok_or_else(|| failed("the TLS address overflows"))
}

/// Locates TLS blocks in loader state made of arbitrary bytes, exercising
/// every bound the lookups check on the inferior's memory. The input names
/// each symbol's place in it, the thread pointer, the link map, and the
/// offset; then it is the memory itself.
#[cfg(feature = "fuzzing")]
pub(super) fn fuzz(data: &[u8]) {
    use crate::backend::linux::core_dump::{GENERAL_REGISTER_COUNT, user_registers};

    const BASE: u64 = 0x1000;
    const SYMBOLS: [&str; 17] = [
        "__nptl_rtld_global",
        "_rtld_global",
        "_dl_tls_dtv_slotinfo_list",
        "_thread_db___nptl_rtld_global",
        "_thread_db_rtld_global__dl_tls_dtv_slotinfo_list",
        "_thread_db__dl_tls_dtv_slotinfo_list",
        "_thread_db_link_map_l_tls_modid",
        "_thread_db_link_map_l_tls_offset",
        "_thread_db_dtv_slotinfo_list_len",
        "_thread_db_dtv_slotinfo_list_next",
        "_thread_db_dtv_slotinfo_list_slotinfo",
        "_thread_db_dtv_slotinfo_gen",
        "_thread_db_dtv_slotinfo_map",
        "_thread_db_pthread_dtvp",
        "_thread_db_dtv_dtv",
        "_thread_db_dtv_t_counter",
        "_thread_db_dtv_t_pointer_val",
    ];

    // Where in the memory each symbol is, the thread pointer, the link map,
    // and the offset, each a 16-bit place; 0xffff names nothing.
    const HEADER: usize = (SYMBOLS.len() + 3) * 2;

    struct Memory<'a> {
        header: &'a [u8],
        bytes: &'a [u8],
    }

    impl Memory<'_> {
        fn place(&self, index: usize) -> Option<u64> {
            let place = u16::from_le_bytes([self.header[index * 2], self.header[index * 2 + 1]]);
            (place != u16::MAX).then(|| BASE + u64::from(place))
        }
    }

    impl ProcessServices for Memory<'_> {
        fn read(&self, address: u64, output: &mut [u8]) -> bool {
            let source = address
                .checked_sub(BASE)
                .and_then(|start| usize::try_from(start).ok())
                .and_then(|start| self.bytes.get(start..start.checked_add(output.len())?));
            source.is_some_and(|source| {
                output.copy_from_slice(source);
                true
            })
        }

        fn registers(&self, _: Pid) -> Option<nix::libc::user_regs_struct> {
            let mut registers = user_registers([0; GENERAL_REGISTER_COUNT]);
            registers.fs_base = self.place(SYMBOLS.len()).unwrap_or(0);
            Some(registers)
        }

        fn lookup_symbol(&self, _: Option<&str>, symbol: &str) -> Option<u64> {
            let index = SYMBOLS.iter().position(|name| *name == symbol)?;
            self.place(index)
        }
    }

    let Some((header, bytes)) = data.split_at_checked(HEADER) else {
        return;
    };

    let memory = Memory { header, bytes };
    let link_map = memory.place(SYMBOLS.len() + 1).unwrap_or(0);
    let offset = memory
        .place(SYMBOLS.len() + 2)
        .map_or(0, |place| place - BASE);
    let thread = Pid::from_raw(1);
    let _ = tls_address(&memory, thread, link_map, offset);
    let _ = static_executable_tls_address(&memory, thread, offset);
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use nix::libc;
    use proptest::prelude::*;
    use proptest::sample::{Index, subsequence};

    use super::*;
    use crate::backend::linux::core_dump::{GENERAL_REGISTER_COUNT, user_registers};

    const THREAD_POINTER: u64 = 0x7000_0000;
    const DTV: u64 = 0x6000_0000;
    const LINK_MAP: u64 = 0x5000_0000;
    const RTLD_GLOBAL: u64 = 0x4000_0000;
    const FIRST_LIST: u64 = 0x3000_0000;
    const SECOND_LIST: u64 = 0x3100_0000;
    const NPTL_RTLD_GLOBAL: u64 = 0x2000_0000;
    const DESCRIPTORS: u64 = 0x1000_0000;
    const FIRST_LIST_LENGTH: u64 = 64;

    /// A process whose C library lays its structures out unlike any real
    /// glibc, so every offset must come from the descriptors.
    struct Process {
        memory: BTreeMap<u64, u8>,
        symbols: BTreeMap<&'static str, u64>,
        thread_pointer: u64,
    }

    impl ProcessServices for Process {
        fn read(&self, address: u64, output: &mut [u8]) -> bool {
            output.iter_mut().zip(address..).all(|(byte, address)| {
                self.memory
                    .get(&address)
                    .map(|value| *byte = *value)
                    .is_some()
            })
        }

        fn registers(&self, _: Pid) -> Option<libc::user_regs_struct> {
            let mut registers = user_registers([0; GENERAL_REGISTER_COUNT]);
            registers.fs_base = self.thread_pointer;
            Some(registers)
        }

        fn lookup_symbol(&self, _: Option<&str>, symbol: &str) -> Option<u64> {
            self.symbols.get(symbol).copied()
        }
    }

    impl Process {
        fn write(&mut self, address: u64, bytes: &[u8]) {
            for (address, byte) in (address..).zip(bytes) {
                self.memory.insert(address, *byte);
            }
        }

        fn word(&mut self, address: u64, value: u64) {
            self.write(address, &value.to_le_bytes());
        }

        fn descriptor(&mut self, name: &'static str, bits: u32, count: u32, offset: i32) {
            let address = DESCRIPTORS + 16 * self.symbols.len() as u64;
            self.symbols.insert(name, address);
            let words = [bits, count, u32::from_le_bytes(offset.to_le_bytes())];
            self.write(address, &words.map(u32::to_le_bytes).concat());
        }

        /// A current DTV whose slot for `module` holds `block`, the module's
        /// static TLS block lying `static_offset` below the thread pointer.
        fn new(module: u64, block: u64, static_offset: u64) -> Self {
            let mut process = Self {
                memory: BTreeMap::new(),
                symbols: BTreeMap::new(),
                thread_pointer: THREAD_POINTER,
            };

            process.word(THREAD_POINTER, THREAD_POINTER);
            process.descriptor("_thread_db_link_map_l_tls_modid", 64, 1, 0x430);
            process.descriptor("_thread_db_link_map_l_tls_offset", 64, 1, 0x428);
            process.descriptor("_thread_db___nptl_rtld_global", 64, 1, 0);
            process.descriptor(
                "_thread_db_rtld_global__dl_tls_dtv_slotinfo_list",
                64,
                1,
                0x100,
            );
            process.descriptor("_thread_db_dtv_slotinfo_list_len", 64, 1, 0x10);
            process.descriptor("_thread_db_dtv_slotinfo_list_next", 64, 1, 0x8);
            process.descriptor("_thread_db_dtv_slotinfo_list_slotinfo", 192, 0, 0x20);
            process.descriptor("_thread_db_dtv_slotinfo_gen", 64, 1, 0x10);
            process.descriptor("_thread_db_dtv_slotinfo_map", 64, 1, 0);
            process.descriptor("_thread_db_pthread_dtvp", 64, 1, 0x18);
            process.descriptor("_thread_db_dtv_dtv", 128, 0x1000, 0x10);
            process.descriptor("_thread_db_dtv_t_counter", 64, 1, 0);
            process.descriptor("_thread_db_dtv_t_pointer_val", 64, 1, 8);
            process
                .symbols
                .insert("__nptl_rtld_global", NPTL_RTLD_GLOBAL);
            process.word(NPTL_RTLD_GLOBAL, RTLD_GLOBAL);
            process.word(RTLD_GLOBAL + 0x100, FIRST_LIST);
            process.word(FIRST_LIST + 0x10, FIRST_LIST_LENGTH);
            process.word(FIRST_LIST + 0x8, SECOND_LIST);
            process.word(SECOND_LIST + 0x10, FIRST_LIST_LENGTH);
            process.word(SECOND_LIST + 0x8, 0);
            process.word(LINK_MAP + 0x430, module);
            process.word(LINK_MAP + 0x428, static_offset);
            process.slotinfo(module, 2, LINK_MAP);
            process.word(THREAD_POINTER + 0x18, DTV);
            process.generation(2);
            process.word(DTV + 0x10 + 16 * module + 8, block);
            process
        }

        fn slotinfo(&mut self, module: u64, generation: u64, map: u64) {
            let (list, index) = if module < FIRST_LIST_LENGTH {
                (FIRST_LIST, module)
            } else {
                (SECOND_LIST, module - FIRST_LIST_LENGTH)
            };

            let entry = list + 0x20 + 24 * index;
            self.word(entry + 0x10, generation);
            self.word(entry, map);
        }

        fn generation(&mut self, generation: u64) {
            self.word(DTV + 0x10, generation);
        }

        fn address(&self) -> Result<u64, TlsError> {
            tls_address(self, Pid::from_raw(1), LINK_MAP, 0x24)
        }
    }

    #[test]
    fn blocks_follow_the_dtv_and_fall_back_to_static_tls() {
        const BLOCK: u64 = 0x5555_0000;
        let static_block = THREAD_POINTER - 0x80 + 0x24;

        // A current DTV holds an allocated block, in either slotinfo list.
        assert_eq!(Process::new(3, BLOCK, 0x80).address(), Ok(BLOCK + 0x24));
        assert_eq!(Process::new(70, BLOCK, 0x80).address(), Ok(BLOCK + 0x24));

        // An unallocated slot, or a DTV older than the module, leaves only
        // the module's static block.
        let unallocated = Process::new(3, u64::MAX, 0x80);
        assert_eq!(unallocated.address(), Ok(static_block));
        let mut stale = Process::new(3, BLOCK, 0x80);
        stale.generation(1);
        assert_eq!(stale.address(), Ok(static_block));

        for no_static_block in [NO_TLS_OFFSET, FORCED_DYNAMIC_TLS_OFFSET] {
            let mut stale = Process::new(3, BLOCK, no_static_block);
            stale.generation(1);
            assert_eq!(stale.address(), Err(TlsError::Deferred));
            assert_eq!(
                Process::new(3, u64::MAX, no_static_block).address(),
                Err(TlsError::Deferred)
            );
        }

        // A thread before its thread pointer is set, and a module without
        // TLS, have no block.
        let mut early = Process::new(3, BLOCK, 0x80);
        early.thread_pointer = 0;
        assert_eq!(early.address(), Err(TlsError::Deferred));
        assert_eq!(Process::new(0, BLOCK, 0x80).address(), Err(TlsError::NoTls));

        // glibc before 2.34 has no pointer to the loader's state in libc;
        // the loader exports that state itself.
        let mut before_2_34 = Process::new(3, BLOCK, 0x80);
        before_2_34.symbols.remove("__nptl_rtld_global");
        before_2_34.symbols.remove("_thread_db___nptl_rtld_global");
        before_2_34.symbols.insert("_rtld_global", RTLD_GLOBAL);
        assert_eq!(before_2_34.address(), Ok(BLOCK + 0x24));

        // A statically linked C library holds the slotinfo list itself.
        let mut static_library = Process::new(3, BLOCK, 0x80);
        static_library.symbols.remove("__nptl_rtld_global");
        static_library
            .symbols
            .insert("_dl_tls_dtv_slotinfo_list", 0x2100_0000);
        static_library.word(0x2100_0000, FIRST_LIST);
        static_library.descriptor("_thread_db__dl_tls_dtv_slotinfo_list", 64, 1, 0);
        assert_eq!(static_library.address(), Ok(BLOCK + 0x24));
    }

    /// A process with glibc's two lists of threads in its loader state:
    /// those on stacks the program gave and those on stacks glibc
    /// allocated, each a list of (descriptor, thread id).
    fn threaded(given: &[(u64, u64)], allocated: &[(u64, u64)]) -> Process {
        const LIST: i32 = 0x2c0;
        const TID: i32 = 0x2d0;
        let mut process = Process::new(3, 0x5555_0000, 0x80);
        process.descriptor("_thread_db_list_t_next", 64, 1, 0);
        process.descriptor("_thread_db_pthread_list", 128, 1, LIST);
        process.descriptor("_thread_db_pthread_tid", 32, 1, TID);
        process.descriptor("_thread_db_rtld_global__dl_stack_user", 128, 1, 0x700);
        process.descriptor("_thread_db_rtld_global__dl_stack_used", 128, 1, 0x710);

        for (head, threads) in [
            (RTLD_GLOBAL + 0x700, given),
            (RTLD_GLOBAL + 0x710, allocated),
        ] {
            let mut previous = head;

            for &(descriptor, tid) in threads {
                let node = descriptor + u64::from(LIST.cast_unsigned());
                process.word(previous, node);
                process.write(
                    descriptor + u64::from(TID.cast_unsigned()),
                    &u32::try_from(tid).expect("a 32-bit id").to_le_bytes(),
                );
                previous = node;
            }

            process.word(previous, head);
        }

        process
    }

    #[test]
    fn saved_thread_identity_comes_from_its_tcb_and_validated_descriptor() {
        let mut process = Process::new(1, 0x5555_0000, 0x80);
        process.descriptor("_thread_db_pthread_tid", 32, 1, 0x2d0);
        process.write(THREAD_POINTER + 0x2d0, &7u32.to_le_bytes());
        let descriptor = process.symbols["_thread_db_pthread_tid"];
        // The note's ID is a host ID, not the target's ID of 7.
        assert_eq!(
            target_thread_id(&process, Pid::from_raw(5000), descriptor),
            Ok(7)
        );

        for id in [0, u32::MAX, 1 << 31] {
            process.write(THREAD_POINTER + 0x2d0, &id.to_le_bytes());
            assert!(target_thread_id(&process, Pid::from_raw(5000), descriptor).is_err());
        }
        process.write(THREAD_POINTER + 0x2d0, &7u32.to_le_bytes());

        for (bits, count) in [(64u32, 1u32), (32, 0), (32, 2), (0, 1)] {
            process.write(descriptor, &bits.to_le_bytes());
            process.write(descriptor + 4, &count.to_le_bytes());
            assert!(target_thread_id(&process, Pid::from_raw(5000), descriptor).is_err());
        }
        process.write(descriptor, &32u32.to_le_bytes());
        process.write(descriptor + 4, &1u32.to_le_bytes());
        process.memory.remove(&(THREAD_POINTER + 0x2d0));
        assert!(target_thread_id(&process, Pid::from_raw(5000), descriptor).is_err());
        process.thread_pointer = 0;
        assert!(target_thread_id(&process, Pid::from_raw(5000), descriptor).is_err());
    }

    #[test]
    fn thread_pointers_follow_both_lists_of_threads() {
        let process = threaded(
            &[(0x7100_0000, 100)],
            &[(0x7200_0000, 101), (0x7300_0000, 0)],
        );
        assert_eq!(
            thread_pointers(&process),
            Ok(vec![(100, 0x7100_0000), (101, 0x7200_0000)]),
            "the exited thread, whose id is cleared, is left out"
        );

        assert_eq!(thread_pointers(&threaded(&[], &[])), Ok(Vec::new()));
    }

    #[test]
    fn a_thread_list_that_never_returns_fails() {
        let mut cyclic = threaded(&[(0x7100_0000, 100), (0x7200_0000, 101)], &[]);
        cyclic.word(0x7200_0000 + 0x2c0, 0x7100_0000 + 0x2c0);
        let Err(TlsError::Failed(message)) = thread_pointers(&cyclic) else {
            panic!("a cycle is not a list");
        };

        assert!(message.contains("_dl_stack_user"), "{message}");

        let mut broken = threaded(&[(0x7100_0000, 100)], &[]);
        broken.word(0x7100_0000 + 0x2c0, 0x9900_0000);
        assert!(
            thread_pointers(&broken).is_err(),
            "an unreadable link fails"
        );

        let mut static_library = threaded(&[], &[]);
        static_library.symbols.remove("__nptl_rtld_global");
        assert!(thread_pointers(&static_library).is_err());
    }

    #[test]
    fn inconsistent_loader_state_fails_with_its_reason() {
        let failure = |process: &Process| match process.address() {
            Err(TlsError::Failed(message)) => message,
            result => panic!("expected a failure, got {result:?}"),
        };

        let beyond = Process::new(200, 0x5555_0000, 0x80);
        assert!(failure(&beyond).contains("no TLS slot for module 200"));
        let mut cyclic = Process::new(70, 0x5555_0000, 0x80);
        cyclic.word(SECOND_LIST + 0x8, FIRST_LIST);
        cyclic.word(LINK_MAP + 0x430, 300);
        assert!(failure(&cyclic).contains("cycle"));
        let mut overflowing = Process::new(70, 0x5555_0000, 0x80);
        overflowing.word(SECOND_LIST + 0x10, u64::MAX);
        assert!(failure(&overflowing).contains("slotinfo list lengths overflow"));
        let mut unloaded = Process::new(3, 0x5555_0000, 0x80);
        unloaded.slotinfo(3, 2, 0);
        assert!(failure(&unloaded).contains("TLS module 3 is not loaded"));

        // The module's DTV slot must exist in the described array.
        let mut short = Process::new(3, 0x5555_0000, 0x80);
        short.descriptor("_thread_db_dtv_dtv", 128, 3, 0x10);
        assert!(failure(&short).contains("outside _thread_db_dtv_dtv"));

        let mut undescribed = Process::new(3, 0x5555_0000, 0x80);
        undescribed.symbols.remove("_thread_db_pthread_dtvp");
        assert_eq!(
            failure(&undescribed),
            "no loaded module defines _thread_db_pthread_dtvp, which glibc before 2.34 \
             defines only in libpthread"
        );
        let mut empty = Process::new(3, 0x5555_0000, 0x80);
        empty.descriptor("_thread_db_pthread_dtvp", 0, 1, 0x18);
        assert!(failure(&empty).contains("unusable 0-bit field"));
        let mut structure = Process::new(3, 0x5555_0000, 0x80);
        structure.descriptor("_thread_db_dtv_slotinfo_gen", 128, 1, 0x10);
        assert!(failure(&structure).contains("not a value"));
        let mut unreadable = Process::new(3, 0x5555_0000, 0x80);
        unreadable.memory.remove(&(THREAD_POINTER + 0x18));
        assert!(failure(&unreadable).contains("unreadable"));
    }

    #[test]
    fn a_static_executables_block_is_in_dtv_slot_one() {
        // A thread of a program without descriptors, whose DTV slot 1 holds
        // `block`.
        let address = |block: u64| {
            let mut process = Process {
                memory: BTreeMap::new(),
                symbols: BTreeMap::new(),
                thread_pointer: THREAD_POINTER,
            };

            process.word(THREAD_POINTER, THREAD_POINTER);
            process.word(THREAD_POINTER + 8, DTV);
            process.word(DTV + 16, block);
            static_executable_tls_address(&process, Pid::from_raw(1), 0x24)
        };

        let block = THREAD_POINTER - 0x80;
        assert_eq!(address(block), Ok(block + 0x24));
        assert_eq!(address(0), Err(TlsError::Deferred));
        assert_eq!(address(u64::MAX), Err(TlsError::Deferred));
        assert!(matches!(
            address(THREAD_POINTER + 0x80),
            Err(TlsError::Failed(message)) if message.contains("not below the thread pointer")
        ));
    }

    /// How a C library leads to the loader's slotinfo lists.
    #[derive(Debug, Clone, Copy)]
    enum Loader {
        /// glibc since 2.34: libc points at the loader's state.
        Pointer,

        /// glibc before 2.34: the loader exports its state.
        Exported,

        /// A statically linked C library holds the lists' head itself.
        Static,
    }

    /// A C library whose descriptors lay its structures out in any way, and
    /// one thread and module in any state. Offsets and sizes are in words.
    #[derive(Debug, Clone)]
    struct Model {
        loader: Loader,

        /// `l_tls_modid` and `l_tls_offset` in the module's link map.
        link_map: [u32; 2],

        /// The DTV pointer in the thread descriptor.
        dtv_pointer: u32,

        /// The slotinfo lists' head in the loader's state.
        head: u32,

        /// A slotinfo list's length and next list, then its entries.
        list: [u32; 3],

        /// An entry's size, then its generation and link map.
        entry: [u32; 3],

        /// A DTV slot's size, its counter and block, which may share a word
        /// as in glibc's union, and where the slots start in the DTV.
        slot: [u32; 4],

        /// The widths of the module number, list lengths, generations, and
        /// the DTV's counter, in bits.
        widths: [u32; 4],
        lists: Vec<u64>,
        module: u64,
        module_generation: u64,
        dtv_generation: u64,

        /// The module's DTV slot: its block, or an odd value when the
        /// thread has not allocated it.
        block: u64,
        static_offset: u64,
        thread_pointer: u64,
    }

    /// `count` distinct word offsets below `limit`, in any order.
    fn offsets(limit: u32, count: usize) -> impl Strategy<Value = Vec<u32>> {
        subsequence((0..limit).collect::<Vec<_>>(), count).prop_shuffle()
    }

    fn width() -> impl Strategy<Value = u32> {
        prop_oneof![Just(32_u32), Just(64)]
    }

    fn model() -> impl Strategy<Value = Model> {
        let loader = prop_oneof![
            Just(Loader::Pointer),
            Just(Loader::Exported),
            Just(Loader::Static)
        ];
        let layout = (
            loader,
            offsets(8, 2),
            1_u32..16,
            0_u32..16,
            (offsets(4, 2), 0_u32..3),
            (offsets(4, 2), 0_u32..2),
            (2_u32..4, 0_u32..2, 0_u32..2, 0_u32..4),
            [width(), width(), width(), width()],
        );
        let lists = (2_u64..9, proptest::collection::vec(1_u64..9, 0..4));
        let state = (
            any::<Index>(),
            any::<bool>(),
            0_u64..8,
            0_u64..8,
            prop_oneof![
                (0x1000_0000_u64..0x2000_0000).prop_map(|block| block & !0xf),
                Just(u64::MAX),
                (0_u64..u64::MAX / 2).prop_map(|value| value * 2 + 1),
            ],
            prop_oneof![
                Just(NO_TLS_OFFSET),
                Just(FORCED_DYNAMIC_TLS_OFFSET),
                1_u64..0x10000
            ],
            prop_oneof![1 => Just(0_u64), 9 => Just(THREAD_POINTER)],
        );
        (layout, lists, state).prop_map(
            |(
                (
                    loader,
                    link_map,
                    dtv_pointer,
                    head,
                    (list, list_gap),
                    (entry, entry_gap),
                    slot,
                    widths,
                ),
                (first, rest),
                (
                    module,
                    no_tls,
                    module_generation,
                    dtv_generation,
                    block,
                    static_offset,
                    thread_pointer,
                ),
            )| {
                let lists: Vec<u64> = std::iter::once(first).chain(rest).collect();
                let modules = lists.iter().sum::<u64>();
                Model {
                    loader,
                    link_map: [link_map[0], link_map[1]],
                    dtv_pointer,
                    head,
                    list: [list[0], list[1], list[0].max(list[1]) + 1 + list_gap],
                    entry: [entry[0].max(entry[1]) + 1 + entry_gap, entry[0], entry[1]],
                    slot: slot.into(),
                    widths,
                    // Module 0 has no TLS; the loader numbers the rest from 1.
                    module: if no_tls {
                        0
                    } else {
                        let others = usize::try_from(modules - 1).expect("few modules");
                        1 + u64::try_from(module.index(others)).expect("few modules")
                    },
                    lists,
                    module_generation,
                    dtv_generation,
                    block,
                    static_offset,
                    thread_pointer,
                }
            },
        )
    }

    /// A byte offset of `words` words, as a descriptor holds it.
    fn word(words: u32) -> i32 {
        i32::try_from(words * 8).expect("small offset")
    }

    /// Where the `index`th word of a structure at `base` is.
    fn at(base: u64, index: u32) -> u64 {
        base + u64::from(index) * 8
    }

    impl Model {
        fn process(&self) -> Process {
            let mut process = Process {
                memory: BTreeMap::new(),
                symbols: BTreeMap::new(),
                thread_pointer: self.thread_pointer,
            };

            let [module_bits, ..] = self.widths;
            let [modid, tls_offset] = self.link_map;
            process.descriptor(
                "_thread_db_link_map_l_tls_modid",
                module_bits,
                1,
                word(modid),
            );
            process.descriptor("_thread_db_link_map_l_tls_offset", 64, 1, word(tls_offset));
            process.value(at(LINK_MAP, modid), module_bits, self.module);
            process.word(at(LINK_MAP, tls_offset), self.static_offset);
            let head = self.loader_state(&mut process);
            self.slotinfo_lists(&mut process, head);
            self.dtv(&mut process);
            process
        }

        /// The loader's state and the way to it, returning where it keeps
        /// the slotinfo lists' head.
        fn loader_state(&self, process: &mut Process) -> u64 {
            let rtld_global = match self.loader {
                Loader::Pointer => {
                    process
                        .symbols
                        .insert("__nptl_rtld_global", NPTL_RTLD_GLOBAL);
                    process.descriptor("_thread_db___nptl_rtld_global", 64, 1, 0);
                    process.word(NPTL_RTLD_GLOBAL, RTLD_GLOBAL);

                    // The loader exports its state too, which the pointer
                    // in libc takes precedence over, as in glibc.
                    process.symbols.insert("_rtld_global", 0x2200_0000);
                    RTLD_GLOBAL
                }
                Loader::Exported => {
                    process.symbols.insert("_rtld_global", RTLD_GLOBAL);
                    RTLD_GLOBAL
                }
                Loader::Static => {
                    const HEAD: u64 = 0x2100_0000;
                    process.symbols.insert("_dl_tls_dtv_slotinfo_list", HEAD);
                    process.descriptor("_thread_db__dl_tls_dtv_slotinfo_list", 64, 1, 0);
                    return HEAD;
                }
            };

            process.descriptor(
                "_thread_db_rtld_global__dl_tls_dtv_slotinfo_list",
                64,
                1,
                word(self.head),
            );
            at(rtld_global, self.head)
        }

        /// The slotinfo lists from `head`, with the module's entry in one of
        /// them.
        fn slotinfo_lists(&self, process: &mut Process, head: u64) {
            const LISTS: u64 = 0x3000_0000;
            const LIST_SPACING: u64 = 0x10_0000;
            let [_, length_bits, generation_bits, _] = self.widths;
            let [length, next, entries] = self.list;
            let [entry_words, generation, map] = self.entry;
            process.descriptor(
                "_thread_db_dtv_slotinfo_list_len",
                length_bits,
                1,
                word(length),
            );
            process.descriptor("_thread_db_dtv_slotinfo_list_next", 64, 1, word(next));
            process.descriptor(
                "_thread_db_dtv_slotinfo_list_slotinfo",
                entry_words * 64,
                0,
                word(entries),
            );
            process.descriptor(
                "_thread_db_dtv_slotinfo_gen",
                generation_bits,
                1,
                word(generation),
            );
            process.descriptor("_thread_db_dtv_slotinfo_map", 64, 1, word(map));
            process.word(head, LISTS);
            let mut first = 0;
            let mut list = LISTS;

            for (index, &count) in self.lists.iter().enumerate() {
                let following = if index + 1 == self.lists.len() {
                    0
                } else {
                    list + LIST_SPACING
                };

                process.value(at(list, length), length_bits, count);
                process.word(at(list, next), following);

                if (first..first + count).contains(&self.module) {
                    let entry =
                        at(list, entries) + (self.module - first) * u64::from(entry_words) * 8;
                    process.value(
                        at(entry, generation),
                        generation_bits,
                        self.module_generation,
                    );
                    process.word(at(entry, map), LINK_MAP);
                }

                first += count;
                list = following;
            }
        }

        /// The thread's DTV: its generation in slot 0's counter, and the
        /// module's block in its own slot.
        fn dtv(&self, process: &mut Process) {
            let [.., counter_bits] = self.widths;
            let [slot_words, counter, pointer, slots] = self.slot;
            process.descriptor("_thread_db_pthread_dtvp", 64, 1, word(self.dtv_pointer));
            process.descriptor("_thread_db_dtv_dtv", slot_words * 64, 0x1000, word(slots));
            process.descriptor("_thread_db_dtv_t_counter", counter_bits, 1, word(counter));
            process.descriptor("_thread_db_dtv_t_pointer_val", 64, 1, word(pointer));
            process.word(at(THREAD_POINTER, self.dtv_pointer), DTV);
            let slot = |index: u64| at(DTV, slots) + index * u64::from(slot_words) * 8;
            process.value(at(slot(0), counter), counter_bits, self.dtv_generation);
            process.word(at(slot(self.module), pointer), self.block);
        }

        /// What glibc's `td_thr_tlsbase` finds for this thread and module.
        fn expected(&self) -> Result<u64, TlsError> {
            if self.module == 0 {
                return Err(TlsError::NoTls);
            }

            if self.thread_pointer == 0 {
                return Err(TlsError::Deferred);
            }

            let allocated = self.block & UNALLOCATED_BIT == 0;
            if self.dtv_generation >= self.module_generation && allocated {
                return Ok(self.block + 0x24);
            }

            if self.static_offset == NO_TLS_OFFSET
                || self.static_offset == FORCED_DYNAMIC_TLS_OFFSET
            {
                return Err(TlsError::Deferred);
            }

            Ok(self.thread_pointer - self.static_offset + 0x24)
        }
    }

    impl Process {
        /// Writes the low `bits` of `value` at `address`.
        fn value(&mut self, address: u64, bits: u32, value: u64) {
            let bytes = value.to_le_bytes();
            self.write(address, &bytes[..bits as usize / 8]);
        }
    }

    proptest! {
        /// However a C library lays its structures out, the descriptors
        /// lead to the block glibc's own lookup finds.
        #[test]
        fn any_described_layout_leads_to_the_block_glibc_finds(model in model()) {
            prop_assert_eq!(model.process().address(), model.expected());
        }

        /// Loader state damaged anywhere yields an address or a reason,
        /// never a panic or a hang.
        #[test]
        fn damaged_loader_state_is_a_reason_not_a_panic(
            model in model(),
            damage in proptest::collection::vec((any::<Index>(), any::<u8>()), 1..16),
        ) {
            let mut process = model.process();
            let addresses: Vec<u64> = process.memory.keys().copied().collect();

            for (index, byte) in damage {
                process.memory.insert(*index.get(&addresses), byte);
            }

            let _ = process.address();
        }
    }
}
