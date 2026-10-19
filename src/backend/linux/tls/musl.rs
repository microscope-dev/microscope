//! Thread-local storage addresses in musl's thread layout.
//!
//! musl has no thread debugging library, but the start of its thread
//! descriptor is ABI that its own assembly depends on: it is the thread
//! control block, whose second word points at the thread's DTV. The DTV's
//! first word counts the modules it has slots for, and slot N holds the
//! address of TLS module N's block.
//!
//! musl gives a thread every module's block before the module's code can
//! run: a new thread copies every block, and `dlopen` extends every thread's
//! DTV before it runs the library's code. A DTV with too few slots belongs to
//! a thread caught in between, or to a musl older than 1.1.22, which gave
//! threads a `dlopen`ed library's block only when they first used it.

use std::num::NonZeroU64;

use nix::unistd::Pid;

use super::{ProcessServices, TlsError, dtv, failed, read_word, thread_pointer};

const WORD_SIZE: u64 = 8;

/// The address of `offset` within the TLS block of module `module` for
/// `thread`.
pub(super) fn tls_address(
    services: &dyn ProcessServices,
    thread: Pid,
    module: NonZeroU64,
    offset: u64,
) -> Result<u64, TlsError> {
    let dtv = dtv(services, thread_pointer(services, thread)?)?;
    if module.get() > read_word(services, Some(dtv))? {
        return Err(TlsError::Deferred);
    }

    let block = read_word(
        services,
        module
            .get()
            .checked_mul(WORD_SIZE)
            .and_then(|slot| dtv.checked_add(slot)),
    )?;
    if block == 0 {
        return Err(TlsError::Deferred);
    }

    block
        .checked_add(offset)
        .ok_or_else(|| failed("the TLS address overflows"))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use nix::libc;

    use super::*;
    use crate::backend::linux::core_dump::{GENERAL_REGISTER_COUNT, user_registers};

    const THREAD_POINTER: u64 = 0x7000_0000;
    const DTV: u64 = 0x6000_0000;

    struct Process {
        words: BTreeMap<u64, u64>,
        thread_pointer: u64,
    }

    impl ProcessServices for Process {
        fn read(&self, address: u64, output: &mut [u8]) -> bool {
            let Some(word) = self.words.get(&address) else {
                return false;
            };

            output.copy_from_slice(&word.to_le_bytes()[..output.len()]);
            true
        }

        fn registers(&self, _: Pid) -> Option<libc::user_regs_struct> {
            let mut registers = user_registers([0; GENERAL_REGISTER_COUNT]);
            registers.fs_base = self.thread_pointer;
            Some(registers)
        }

        fn lookup_symbol(&self, _: Option<&str>, _: &str) -> Option<u64> {
            None
        }
    }

    impl Process {
        /// A thread whose DTV has a slot for each of `blocks`.
        fn new(blocks: &[u64]) -> Self {
            let mut words = BTreeMap::from([
                (THREAD_POINTER, THREAD_POINTER),
                (THREAD_POINTER + 8, DTV),
                (DTV, blocks.len() as u64),
            ]);

            for (slot, block) in (1..).zip(blocks) {
                words.insert(DTV + 8 * slot, *block);
            }

            Self {
                words,
                thread_pointer: THREAD_POINTER,
            }
        }

        fn address(&self, module: u64) -> Result<u64, TlsError> {
            let module = NonZeroU64::new(module).expect("module numbers start at one");
            tls_address(self, Pid::from_raw(1), module, 0x24)
        }
    }

    #[test]
    fn blocks_are_found_through_the_dtv_unless_the_thread_lacks_them() {
        let process = Process::new(&[0x1000, 0x2000, 0x3000]);
        assert_eq!(process.address(1), Ok(0x1024));
        assert_eq!(process.address(3), Ok(0x3024));

        // A module loaded after the thread's DTV was last extended, an empty
        // slot, and a thread before its thread pointer is set have no block.
        assert_eq!(process.address(4), Err(TlsError::Deferred));
        assert_eq!(
            Process::new(&[0x1000, 0]).address(2),
            Err(TlsError::Deferred)
        );
        let mut early = Process::new(&[0x1000]);
        early.thread_pointer = 0;
        assert_eq!(early.address(1), Err(TlsError::Deferred));

        let failure = |process: &Process| match process.address(1) {
            Err(TlsError::Failed(message)) => message,
            result => panic!("expected a failure, got {result:?}"),
        };

        // A thread pointer that does not address a descriptor of its own is
        // not musl's.
        let mut foreign = Process::new(&[0x1000]);
        foreign.words.insert(THREAD_POINTER, 0);
        assert!(failure(&foreign).contains("does not address a thread control block"));
        let mut unreadable = Process::new(&[0x1000]);
        unreadable.words.remove(&DTV);
        assert!(failure(&unreadable).contains("0x60000000 is unreadable"));
        let mut overflowing = Process::new(&[0x1000]);
        overflowing.words.insert(DTV + 8, u64::MAX);
        assert!(failure(&overflowing).contains("TLS address overflows"));
    }
}
