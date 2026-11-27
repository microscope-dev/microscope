//! Recovers a high-address stack ceiling for attached threads whose creation was not observed
//! (used by `zero_return_address` to bound stack walks).
//!
//! Reads glibc's `__libc_stack_end` (or `thread_pointer` / TCB for worker threads) or musl's
//! `pthread` / `auxv` fields, cross-checked against `/proc/<pid>/auxv` and the thread pointer.

use nix::unistd::Pid;

use super::native::InspectionOps;
use super::tls::CLibrary;
use super::{Controller, Inferior};

const MUSL_TID: u64 = 0x30;
const MUSL_STACK: u64 = 0x58;
const MUSL_AUXV: u64 = 8;

impl<P: InspectionOps> Controller<P> {
    pub(super) fn unseen_stack_start(&self, inferior: &Inferior, pid: Pid) -> Option<u64> {
        if self.main.c_library == CLibrary::Musl {
            return self.musl_stack_start(inferior, pid);
        }

        if pid == inferior.tgid {
            let end = self.symbol_address(inferior, "__libc_stack_end")?;
            return self.ptrace.read_word(pid, end).ok().filter(|&end| end != 0);
        }

        let thread_pointer = self.own_thread_pointer(pid)?;

        let descriptor = self.symbol_address(inferior, "_thread_db_pthread_tid")?;
        let shape = self.ptrace.read_word(pid, descriptor).ok()?;
        if shape != 32 | 1 << 32 {
            return None;
        }
        let offset = low(self
            .ptrace
            .read_word(pid, descriptor.checked_add(8)?)
            .ok()?)?;
        let tid = self
            .ptrace
            .read_word(
                pid,
                thread_pointer.checked_add_signed(i64::from(offset.cast_signed()))?,
            )
            .ok()?;
        (low(tid)? == pid.as_raw().cast_unsigned()).then_some(thread_pointer)
    }

    fn musl_stack_start(&self, inferior: &Inferior, pid: Pid) -> Option<u64> {
        if pid == inferior.tgid {
            let libc = self.symbol_address(inferior, "__libc")?;
            let auxv = self
                .ptrace
                .read_word(pid, libc.checked_add(MUSL_AUXV)?)
                .ok()?;
            let kept = self.ptrace.auxv(pid).filter(|kept| !kept.is_empty())?;
            let words = self
                .ptrace
                .read_block(pid, auxv, kept.len().checked_mul(16)?)?;
            let (words, _) = words.as_chunks::<8>();
            let read = words
                .as_chunks::<2>()
                .0
                .iter()
                .map(|[key, value]| (u64::from_le_bytes(*key), u64::from_le_bytes(*value)));
            return read.eq(kept.iter().copied()).then_some(auxv);
        }

        let thread_pointer = self.own_thread_pointer(pid)?;
        let tid = self
            .ptrace
            .read_word(pid, thread_pointer.checked_add(MUSL_TID)?)
            .ok()?;
        if low(tid)? != pid.as_raw().cast_unsigned() {
            return None;
        }
        self.ptrace
            .read_word(pid, thread_pointer.checked_add(MUSL_STACK)?)
            .ok()
            .filter(|&stack| stack != 0)
    }

    fn own_thread_pointer(&self, pid: Pid) -> Option<u64> {
        let thread_pointer = self.ptrace.registers(pid).ok()?.thread_pointer();
        (thread_pointer != 0 && self.ptrace.read_word(pid, thread_pointer).ok()? == thread_pointer)
            .then_some(thread_pointer)
    }

    fn symbol_address(&self, inferior: &Inferior, name: &str) -> Option<u64> {
        let mut found = self
            .unwind_modules(inferior)
            .into_iter()
            .filter_map(|module| {
                let symbol = module.image.symbol_named(name).ok()?;
                module.loaded.virtual_address(symbol.address()).ok()
            });
        let address = found.next()?;
        found.next().is_none().then_some(address.get())
    }
}

fn low(word: u64) -> Option<u32> {
    word.to_le_bytes()[..4]
        .try_into()
        .ok()
        .map(u32::from_le_bytes)
}
