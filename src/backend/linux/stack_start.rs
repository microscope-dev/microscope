//! Where the stack of a thread whose start the debugger did not see began,
//! as its C library records it, for a thread it attached to or one a core
//! saved.

use nix::unistd::Pid;

use super::native::InspectionOps;
use super::tls::CLibrary;
use super::{Controller, Inferior};

/// Where musl 1.2.5's `struct pthread` keeps the thread's `tid` and the
/// top of its stack, which `pthread_getattr_np` reads, and where its
/// `struct __libc` keeps `auxv`. None of it is ABI, so each is used only
/// where another fact proves it.
const MUSL_TID: u64 = 0x30;
const MUSL_STACK: u64 = 0x58;
const MUSL_AUXV: u64 = 8;

impl<P: InspectionOps> Controller<P> {
    /// An address at or above the stack pointer `pid` began with, which a
    /// zero return address lower in its stack cannot be its creator's
    /// mark of the stack's end; `None` where the C library does not say.
    ///
    /// For glibc's first thread it is `__libc_stack_end`, the stack
    /// pointer the kernel started the process with. Any other thread glibc
    /// made has its descriptor at its thread pointer, at the top of its
    /// stack, and began just below it. glibc marks neither end with a
    /// zero: `_start` and `clone3` leave the return address undefined. A
    /// thread pointer whose descriptor names another thread, as one that
    /// raw `clone` made without its own, says nothing.
    pub(super) fn unseen_stack_start(&self, inferior: &Inferior, pid: Pid) -> Option<u64> {
        if self.main.c_library == CLibrary::Musl {
            return self.musl_stack_start(inferior, pid);
        }

        if pid == inferior.tgid {
            let end = self.symbol_address(inferior, "__libc_stack_end")?;
            return self.ptrace.read_word(pid, end).ok().filter(|&end| end != 0);
        }

        let thread_pointer = self.own_thread_pointer(pid)?;

        // `_thread_db_pthread_tid`: the field's width in bits and element
        // count, then its offset, each 32 bits.
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

    /// The same, as musl records it, which marks neither end with a zero.
    ///
    /// musl's first thread found the auxiliary vector above its arguments
    /// and environment, so above where its stack began, and keeps its
    /// address in `__libc.auxv`; the words there must be the vector the
    /// kernel kept. Another thread's descriptor, at its thread pointer,
    /// names the top of the stack the thread began below, once its `tid`
    /// names the thread.
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

    /// `pid`'s thread pointer, where its thread control block begins with
    /// its own address, as glibc's and musl's do.
    fn own_thread_pointer(&self, pid: Pid) -> Option<u64> {
        let thread_pointer = self.ptrace.registers(pid).ok()?.fs_base;
        (thread_pointer != 0 && self.ptrace.read_word(pid, thread_pointer).ok()? == thread_pointer)
            .then_some(thread_pointer)
    }

    /// Where the single definition of `name` among the process's modules
    /// is loaded.
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

/// The low 32 bits of `word`, as a field of that width reads.
fn low(word: u64) -> Option<u32> {
    word.to_le_bytes()[..4]
        .try_into()
        .ok()
        .map(u32::from_le_bytes)
}
