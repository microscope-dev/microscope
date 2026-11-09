//! Where the stack of a thread whose start the debugger did not see began,
//! as glibc records it, for a thread it attached to or one a core saved.

use nix::unistd::Pid;

use super::native::InspectionOps;
use super::{Controller, Inferior};

impl<P: InspectionOps> Controller<P> {
    /// An address at or above the stack pointer `pid` began with, which a
    /// zero return address lower in its stack cannot be its creator's
    /// mark of the stack's end; `None` where glibc does not say.
    ///
    /// For the first thread it is glibc's `__libc_stack_end`, the stack
    /// pointer the kernel started the process with. Any other thread glibc
    /// made has its descriptor at its thread pointer, at the top of its
    /// stack, and began just below it. glibc marks neither end with a
    /// zero: `_start` and `clone3` leave the return address undefined. A
    /// thread pointer whose descriptor names another thread, as one that
    /// raw `clone` made without its own, says nothing.
    pub(super) fn unseen_stack_start(&self, inferior: &Inferior, pid: Pid) -> Option<u64> {
        if pid == inferior.tgid {
            let end = self.symbol_address(inferior, "__libc_stack_end")?;
            return self.ptrace.read_word(pid, end).ok().filter(|&end| end != 0);
        }

        let thread_pointer = self.ptrace.registers(pid).ok()?.fs_base;
        // glibc's thread control block begins with its own address.
        if thread_pointer == 0 || self.ptrace.read_word(pid, thread_pointer).ok()? != thread_pointer
        {
            return None;
        }

        // `_thread_db_pthread_tid`: the field's width in bits and element
        // count, then its offset, each 32 bits.
        let descriptor = self.symbol_address(inferior, "_thread_db_pthread_tid")?;
        let shape = self.ptrace.read_word(pid, descriptor).ok()?;
        if shape != 32 | 1 << 32 {
            return None;
        }
        let low = |word: u64| {
            word.to_le_bytes()[..4]
                .try_into()
                .ok()
                .map(u32::from_le_bytes)
        };
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
