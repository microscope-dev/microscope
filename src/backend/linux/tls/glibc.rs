//! `glibc` `TLS` and thread-list traversal driven by `_thread_db_*` layout descriptors.
//!
//! Reading `glibc`'s own exported descriptors avoids hardcoding internal `pthread`/`link_map`
//! offsets across `glibc` versions.

use std::collections::BTreeSet;

use nix::unistd::Pid;

use super::{DESCRIPTOR_SIZE, ProcessServices, TlsError, dtv, failed, read_word, thread_pointer};

const C_LIBRARY: &str = "libc.so.6";

const LOADER: &str = "ld-linux-x86-64.so.2";

const MAX_SLOTINFO_LISTS: usize = 1 << 16;

const UNALLOCATED_BIT: u64 = 1;

const DTV_SLOT_SIZE: u64 = 16;

const EXECUTABLE_MODULE: u64 = 1;

const NO_TLS_OFFSET: u64 = 0;
const FORCED_DYNAMIC_TLS_OFFSET: u64 = u64::MAX;

/// Decoded 12-byte `glibc` `_thread_db` descriptor: `(bit_width, element_count, byte_offset)`.
#[derive(Debug, Clone, Copy)]
struct Field {
    name: &'static str,
    bits: u32,

    count: u32,
    offset: i32,
}

impl Field {
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

        if field.bits == 0 || !field.bits.is_multiple_of(8) {
            return Err(failed(format!(
                "{name} describes an unusable {}-bit field",
                field.bits
            )));
        }

        Ok(field)
    }

    fn fetch(&self, field: Field, base: u64, index: u64) -> Result<u64, TlsError> {
        if !matches!(field.bits, 8 | 32 | 64) {
            return Err(failed(format!(
                "{} is a {}-bit field, not a value",
                field.name, field.bits
            )));
        }

        let address = field.address(base, index)?;
        let mut bytes = [0; 8];
        let width = (field.bits / 8) as usize;
        let value = bytes
            .get_mut(..width)
            .ok_or_else(|| failed(format!("{} is wider than a word", field.name)))?;
        self.read(address, value)?;
        Ok(u64::from_le_bytes(bytes))
    }

    fn fetch_named(&self, name: &'static str, base: u64, index: u64) -> Result<u64, TlsError> {
        self.fetch(self.field(name)?, base, index)
    }

    fn rtld_global(&self) -> Result<Option<u64>, TlsError> {
        Ok(match self.symbol(C_LIBRARY, "__nptl_rtld_global") {
            Ok(pointer) => Some(self.fetch_named("_thread_db___nptl_rtld_global", pointer, 0)?),
            Err(_) => self.symbol(LOADER, "_rtld_global").ok(),
        })
    }

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

    fn static_block(&self, thread_pointer: u64, map: u64) -> Result<u64, TlsError> {
        let offset = self.fetch_named("_thread_db_link_map_l_tls_offset", map, 0)?;
        if offset == NO_TLS_OFFSET || offset == FORCED_DYNAMIC_TLS_OFFSET {
            return Err(TlsError::Deferred);
        }

        thread_pointer.checked_sub(offset).ok_or_else(|| {
            failed(format!(
                "static TLS offset {offset:#x} exceeds the thread pointer"
            ))
        })
    }

    /// Resolves a thread's `TLS` block base from its `DTV`, falling back to static `TLS`
    /// (`thread_pointer - l_tls_offset`) when the `DTV` generation is stale or the slot is lazy.
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

const MAX_THREADS: usize = 1 << 20;

/// Walks `glibc`'s `_dl_stack_user` (caller-supplied stacks) and `_dl_stack_used`
/// (`glibc`-allocated stacks) circular lists to collect `(tid, tcb_address)` for live threads.
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

            let tid = glibc.fetch(id, descriptor, 0)?;
            if tid != 0 && tid <= u64::from(i32::MAX.cast_unsigned()) {
                threads.push((tid, descriptor));
            }

            node = glibc.fetch(next, node, 0)?;
        }
    }

    Ok(threads)
}

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

    let thread_pointer = services
        .registers(thread)
        .ok_or_else(|| failed(format!("the registers of thread {thread} are unavailable")))?
        .thread_pointer();
    if thread_pointer == 0 {
        return Err(TlsError::Deferred);
    }

    glibc
        .block(thread_pointer, module)?
        .checked_add(offset)
        .ok_or_else(|| failed("the TLS address overflows"))
}

pub(super) fn static_offset(
    services: &dyn ProcessServices,
    link_map: u64,
) -> Result<Option<u64>, TlsError> {
    let glibc = Glibc { services };
    let offset = glibc.fetch_named("_thread_db_link_map_l_tls_offset", link_map, 0)?;
    Ok((offset != NO_TLS_OFFSET && offset != FORCED_DYNAMIC_TLS_OFFSET).then_some(offset))
}

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

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use proptest::prelude::*;
    use proptest::sample::{Index, subsequence};

    use super::*;
    use crate::backend::linux::Registers;
    use crate::backend::linux::arch::GENERAL_REGISTER_COUNT;

    const THREAD_POINTER: u64 = 0x7000_0000;
    const DTV: u64 = 0x6000_0000;
    const LINK_MAP: u64 = 0x5000_0000;
    const RTLD_GLOBAL: u64 = 0x4000_0000;
    const FIRST_LIST: u64 = 0x3000_0000;
    const SECOND_LIST: u64 = 0x3100_0000;
    const NPTL_RTLD_GLOBAL: u64 = 0x2000_0000;
    const DESCRIPTORS: u64 = 0x1000_0000;
    const FIRST_LIST_LENGTH: u64 = 64;

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

        fn registers(&self, _: Pid) -> Option<Registers> {
            let mut registers = Registers::from_general_registers([0; GENERAL_REGISTER_COUNT]);
            registers.set_thread_pointer(self.thread_pointer);
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
    fn a_static_program_has_no_thread_list() {
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

    #[derive(Debug, Clone, Copy)]
    enum Loader {
        Pointer,

        Exported,

        Static,
    }

    #[derive(Debug, Clone)]
    struct Model {
        loader: Loader,

        link_map: [u32; 2],

        dtv_pointer: u32,

        head: u32,

        list: [u32; 3],

        entry: [u32; 3],

        slot: [u32; 4],

        widths: [u32; 4],
        lists: Vec<u64>,
        module: u64,
        module_generation: u64,
        dtv_generation: u64,

        block: u64,
        static_offset: u64,
        thread_pointer: u64,
    }

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
            prop::bool::weighted(0.1),
            0_u64..8,
            0_u64..8,
            prop_oneof![
                2 => (0x1000_0000_u64..0x2000_0000).prop_map(|block| block & !0xf),
                1 => Just(u64::MAX),
                1 => (0_u64..u64::MAX / 2).prop_map(|value| value * 2 + 1),
            ],
            prop_oneof![
                1 => Just(NO_TLS_OFFSET),
                1 => Just(FORCED_DYNAMIC_TLS_OFFSET),
                4 => 1_u64..0x10000,
                1 => THREAD_POINTER + 1..FORCED_DYNAMIC_TLS_OFFSET,
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

    fn word(words: u32) -> i32 {
        i32::try_from(words * 8).expect("small offset")
    }

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

        fn loader_state(&self, process: &mut Process) -> u64 {
            let rtld_global = match self.loader {
                Loader::Pointer => {
                    process
                        .symbols
                        .insert("__nptl_rtld_global", NPTL_RTLD_GLOBAL);
                    process.descriptor("_thread_db___nptl_rtld_global", 64, 1, 0);
                    process.word(NPTL_RTLD_GLOBAL, RTLD_GLOBAL);

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

            self.thread_pointer
                .checked_sub(self.static_offset)
                .map(|block| block + 0x24)
                .ok_or_else(|| {
                    failed(format!(
                        "static TLS offset {:#x} exceeds the thread pointer",
                        self.static_offset
                    ))
                })
        }
    }

    impl Process {
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
    }

    #[test]
    fn layouts_mostly_reach_the_walk() {
        use proptest::strategy::ValueTree;
        use proptest::test_runner::TestRunner;

        let mut runner = TestRunner::deterministic();
        let strategy = model();
        let (mut walked, mut found, mut past) = (0, 0, 0);
        for _ in 0..1000 {
            let model = strategy.new_tree(&mut runner).expect("a model").current();
            if model.module == 0 || model.thread_pointer == 0 {
                continue;
            }
            walked += 1;
            match model.expected() {
                Ok(_) => found += 1,
                Err(TlsError::Failed(_)) => past += 1,
                Err(_) => {}
            }
        }

        assert!(walked >= 700, "{walked} of 1000 reach the walk");
        assert!(found >= 400, "{found} of 1000 find a block");
        assert!(
            past >= 20,
            "{past} of 1000 find the offset past the thread pointer"
        );
    }

    proptest! {
        #![proptest_config(ProptestConfig {
            fork: true,
            timeout: 10_000,
            ..ProptestConfig::default()
        })]

        /// Loader state with bytes missing anywhere yields the block glibc
        /// finds or a reason, never another address. Bytes overwritten as
        /// well yield an address or a reason, and never a hang or a panic,
        /// which the fork and the timeout catch and still shrink.
        #[test]
        fn damaged_loader_state_is_the_block_or_a_reason(
            model in model(),
            damage in proptest::collection::vec(
                (any::<Index>(), prop::option::of(any::<u8>())),
                1..16,
            ),
        ) {
            let mut process = model.process();
            let addresses: Vec<u64> = process.memory.keys().copied().collect();
            let mut overwritten = false;

            for (index, byte) in damage {
                let address = *index.get(&addresses);
                match byte {
                    Some(byte) => {
                        overwritten = true;
                        process.memory.insert(address, byte);
                    }
                    None => {
                        process.memory.remove(&address);
                    }
                }
            }

            let address = process.address();
            if !overwritten && address.is_ok() {
                prop_assert_eq!(address, model.expected());
            }
        }
    }

    #[derive(Debug, Clone, Copy)]
    enum Links {
        Intact,
        Cycle { from: Index, to: Index },
        Broken { at: Index },
    }

    proptest! {
        #![proptest_config(ProptestConfig {
            fork: true,
            timeout: 10_000,
            ..ProptestConfig::default()
        })]

        /// glibc's two lists of threads give every thread whose id is a
        /// live pid_t, in list order, and a list that does not lead back
        /// to its head fails and names itself.
        #[test]
        fn thread_lists_give_their_live_threads(
            (given, allocated, links, damaged) in (0..5_usize, 0..5_usize).prop_flat_map(
                |(given, allocated)| {
                    let tid = prop_oneof![
                        1 => Just(0_u64),
                        4 => 1_u64..=u64::from(i32::MAX.cast_unsigned()),
                        1 => Just(u64::from(i32::MAX.cast_unsigned())),
                        1 => Just(1 << 31),
                        1 => Just(u64::from(u32::MAX)),
                    ];
                    let places = subsequence((0..16_u64).collect::<Vec<_>>(), given + allocated)
                        .prop_shuffle();
                    let links = prop_oneof![
                        2 => Just(Links::Intact),
                        1 => (any::<Index>(), any::<Index>())
                            .prop_map(|(from, to)| Links::Cycle { from, to }),
                        1 => any::<Index>().prop_map(|at| Links::Broken { at }),
                    ];
                    (
                        places,
                        proptest::collection::vec(tid, given + allocated),
                        links,
                        any::<bool>(),
                    )
                        .prop_map(move |(places, tids, links, second)| {
                            let threads = places
                                .into_iter()
                                .map(|place| 0x7100_0000 + place * 0x1000)
                                .zip(tids)
                                .collect::<Vec<_>>();
                            let (first, rest) = threads.split_at(given);
                            (first.to_vec(), rest.to_vec(), links, second)
                        })
                }
            )
        ) {
            const LIST: u64 = 0x2c0;
            let mut process = threaded(&given, &allocated);
            let list = if damaged { &allocated } else { &given };
            let name = if damaged { "_dl_stack_used" } else { "_dl_stack_user" };

            let failure = match (links, list.is_empty()) {
                (Links::Intact, _) | (_, true) => None,
                (Links::Cycle { from, to }, false) => {
                    let from = from.index(list.len());
                    let to = to.index(from + 1);
                    process.word(list[from].0 + LIST, list[to].0 + LIST);
                    Some(format!("{name} does not lead back to its head"))
                }
                (Links::Broken { at }, false) => {
                    let at = at.index(list.len());
                    process.word(list[at].0 + LIST, 0x9900_0000);
                    Some("unreadable".to_owned())
                }
            };

            let threads = thread_pointers(&process);
            match failure {
                None => {
                    let live = given
                        .iter()
                        .chain(&allocated)
                        .filter(|(_, tid)| *tid != 0 && *tid <= u64::from(i32::MAX.cast_unsigned()))
                        .map(|&(descriptor, tid)| (tid, descriptor))
                        .collect::<Vec<_>>();
                    prop_assert_eq!(threads, Ok(live));
                }
                Some(reason) => match threads {
                    Err(TlsError::Failed(message)) => {
                        prop_assert!(message.contains(&reason), "{message}");
                    }
                    threads => prop_assert!(false, "expected {reason}, got {threads:?}"),
                },
            }
        }
    }
}
