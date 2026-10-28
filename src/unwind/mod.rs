//! The generic unwind loop: it iterates the caller contexts a
//! [`CallerProvider`] reconstructs.

use std::collections::HashSet;

use crate::debug_info::UnwindInfo;
use crate::{ImageAddress, LoadedModule, ModuleImage, UnwindTermination, VirtualAddress};

pub mod code_copies;
pub mod sample;

pub const DEFAULT_MAX_FRAMES: usize = 256;

/// One loaded module's address mapping, metadata, and call-frame information.
#[derive(Clone, Copy)]
pub struct UnwindModule<'a> {
    pub loaded: LoadedModule,
    pub image: &'a ModuleImage,
    pub unwind: &'a dyn UnwindInfo,
}

/// Finds the module whose image describes `address`.
///
/// A process address space cannot map two images at one address, so the
/// first describing module is the only one.
pub fn unwind_module_for<'a>(
    modules: &[UnwindModule<'a>],
    address: VirtualAddress,
) -> Option<(UnwindModule<'a>, ImageAddress)> {
    unwind_module_at(modules, address).map(|(index, image_address)| (modules[index], image_address))
}

/// The index in `modules` of the module [`unwind_module_for`] finds.
pub fn unwind_module_at(
    modules: &[UnwindModule<'_>],
    address: VirtualAddress,
) -> Option<(usize, ImageAddress)> {
    modules.iter().enumerate().find_map(|(index, module)| {
        module
            .loaded
            .image_address(address)
            .ok()
            .filter(|image_address| module.image.contains_address(*image_address))
            .map(|image_address| (index, image_address))
    })
}

/// Whether a frame keeps the frame-pointer chain at an instruction: its
/// rbp holds the address of the word where it saved its caller's, with its
/// return address in the word above, as a walk of the chain assumes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FramePointerRule {
    Kept,

    /// The function's rules find its frame some other way.
    NotKept,

    /// The frame has no caller.
    Outermost,
    NoUnwindInfo,
}

/// Decides a [`FramePointerRule`] by unwinding with registers and memory
/// made up for the test: only rbp is known, and every word of memory holds
/// a value its address names. The frame keeps the chain exactly when its
/// rules find the frame from rbp, its caller's rbp in the word rbp points
/// to, and its return address in the next.
pub fn frame_pointer_rule<U: UnwindInfo + ?Sized>(
    unwind: &U,
    address: ImageAddress,
) -> FramePointerRule {
    /// A frame pointer no real frame has.
    const FRAME: u64 = 0x4000_0000_0000_0000;

    struct Named;

    impl MemoryReader for Named {
        fn read_u64(&mut self, address: VirtualAddress) -> Option<u64> {
            Some(!address.get())
        }
    }

    let registers = RegisterFile::new([(RBP, FRAME)]);
    match unwind.unwind(address, &registers, &mut Named) {
        Ok(step) => match step.registers.get(RETURN_ADDRESS) {
            None => FramePointerRule::Outermost,
            Some(caller)
                if step.cfa.get() == FRAME + 16
                    && caller == !(FRAME + 8)
                    && step.registers.get(RBP) == Some(!FRAME) =>
            {
                FramePointerRule::Kept
            }
            Some(_) => FramePointerRule::NotKept,
        },
        Err(UnwindTermination::Complete) => FramePointerRule::Outermost,
        Err(UnwindTermination::NoUnwindInfo { .. }) => FramePointerRule::NoUnwindInfo,
        Err(_) => FramePointerRule::NotKept,
    }
}

/// Where Linux's x86-64 `ucontext` keeps the interrupted registers: its
/// `uc_mcontext`, after `uc_flags`, `uc_link`, and `uc_stack`.
pub const UCONTEXT_MCONTEXT: u64 = 40;

/// The DWARF numbers of the general registers in the order the kernel's
/// `sigcontext` saves them, from r8 to rip.
pub const SIGCONTEXT_REGISTERS: [DwarfRegister; 17] =
    DwarfRegister::each([8, 9, 10, 11, 12, 13, 14, 15, 5, 4, 6, 3, 1, 0, 2, 7, 16]);

/// The registers a signal interrupted, from the `ucontext` the kernel
/// placed at `context`, the stack pointer of the trampoline its handler
/// returns to; `None` if `memory` lacks any of them.
pub fn interrupted_registers(context: u64, memory: &mut dyn MemoryReader) -> Option<RegisterFile> {
    let mut registers = RegisterFile::new([]);
    for (slot, register) in SIGCONTEXT_REGISTERS.iter().enumerate() {
        let address = context.checked_add(UCONTEXT_MCONTEXT + 8 * slot as u64)?;
        registers.set(*register, memory.read_u64(VirtualAddress::new(address))?);
    }

    Some(registers)
}

/// A register by its x86-64 psABI DWARF number, which is not its place in
/// any register table: DWARF register 1 is rdx, not rbx.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DwarfRegister(u16);

impl DwarfRegister {
    #[must_use]
    pub const fn new(number: u16) -> Self {
        Self(number)
    }

    #[must_use]
    pub const fn number(self) -> u16 {
        self.0
    }

    /// The registers `numbers` name, in order.
    #[must_use]
    pub const fn each<const N: usize>(numbers: [u16; N]) -> [Self; N] {
        let mut registers = [Self(0); N];
        let mut at = 0;
        while at < N {
            registers[at] = Self(numbers[at]);
            at += 1;
        }

        registers
    }
}

impl From<gimli::Register> for DwarfRegister {
    fn from(register: gimli::Register) -> Self {
        Self(register.0)
    }
}

impl std::fmt::Display for DwarfRegister {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(formatter)
    }
}

pub const RAX: DwarfRegister = DwarfRegister(0);

pub const RBX: DwarfRegister = DwarfRegister(3);

pub const RSI: DwarfRegister = DwarfRegister(4);

pub const RDI: DwarfRegister = DwarfRegister(5);

pub const RBP: DwarfRegister = DwarfRegister(6);

pub const RSP: DwarfRegister = DwarfRegister(7);

pub const R12: DwarfRegister = DwarfRegister(12);

/// The return address column, which in a frame's own registers holds its
/// instruction pointer.
pub const RETURN_ADDRESS: DwarfRegister = DwarfRegister(16);

/// The bases of FS and GS.
pub const FS_BASE: DwarfRegister = DwarfRegister(58);

pub const GS_BASE: DwarfRegister = DwarfRegister(59);

/// What the unwinder knows about one frame.
#[derive(Debug, Clone)]
pub struct FrameContext {
    pub instruction: VirtualAddress,
    pub cfa: Option<VirtualAddress>,
    pub signal_frame: bool,
}

/// Registers numbered below this live in [`RegisterFile`]'s array, which
/// covers x86-64's general registers, flags, and segment bases. Call-frame
/// information may name any number, so the rest are kept in a list.
const DENSE_REGISTERS: usize = 64;

/// Register values by DWARF number. An absent register is unknown.
///
/// A sampler unwinds every frame of every sample through one of these, so
/// it holds no allocation for the registers x86-64 has: copying it is one
/// copy of its array.
#[derive(Clone, PartialEq, Eq)]
pub struct RegisterFile {
    /// Bit `n` says whether register `n` is known.
    known: u64,

    /// Each register's value, 0 while unknown, so that equal files are
    /// equal arrays.
    values: [u64; DENSE_REGISTERS],

    /// Registers numbered from [`DENSE_REGISTERS`] up, sorted by number.
    others: Vec<(DwarfRegister, u64)>,
}

impl RegisterFile {
    pub fn new(values: impl IntoIterator<Item = (DwarfRegister, u64)>) -> Self {
        let mut registers = Self {
            known: 0,
            values: [0; DENSE_REGISTERS],
            others: Vec::new(),
        };
        for (register, value) in values {
            registers.set(register, value);
        }

        registers
    }

    pub fn get(&self, register: DwarfRegister) -> Option<u64> {
        let number = usize::from(register.0);
        if number < DENSE_REGISTERS {
            return (self.known & (1 << number) != 0).then(|| self.values[number]);
        }

        self.others
            .binary_search_by_key(&register, |(other, _)| *other)
            .ok()
            .map(|at| self.others[at].1)
    }

    pub fn set(&mut self, register: DwarfRegister, value: u64) {
        let number = usize::from(register.0);
        if number < DENSE_REGISTERS {
            self.known |= 1 << number;
            self.values[number] = value;
            return;
        }

        match self
            .others
            .binary_search_by_key(&register, |(other, _)| *other)
        {
            Ok(at) => self.others[at].1 = value,
            Err(at) => self.others.insert(at, (register, value)),
        }
    }

    pub fn remove(&mut self, register: DwarfRegister) {
        let number = usize::from(register.0);
        if number < DENSE_REGISTERS {
            self.known &= !(1 << number);
            self.values[number] = 0;
            return;
        }

        self.others.retain(|(other, _)| *other != register);
    }

    /// Forgets every register `keep` says not to keep.
    pub fn retain(&mut self, mut keep: impl FnMut(DwarfRegister) -> bool) {
        let mut known = self.known;
        while known != 0 {
            let number = known.trailing_zeros();
            known &= known - 1;
            #[expect(clippy::cast_possible_truncation, reason = "below 64")]
            if !keep(DwarfRegister(number as u16)) {
                self.remove(DwarfRegister(number as u16));
            }
        }

        self.others.retain(|(register, _)| keep(*register));
    }

    /// Every known register and its value, by number.
    pub fn iter(&self) -> impl Iterator<Item = (DwarfRegister, u64)> + '_ {
        (0..DENSE_REGISTERS)
            .filter(|number| self.known & (1 << number) != 0)
            .map(|number| {
                #[expect(clippy::cast_possible_truncation, reason = "below 64")]
                (DwarfRegister(number as u16), self.values[number])
            })
            .chain(self.others.iter().copied())
    }
}

impl std::fmt::Debug for RegisterFile {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_map().entries(self.iter()).finish()
    }
}

/// Reads target memory for unwind rules.
pub trait MemoryReader {
    /// Reads one native word, or `None` when the address is unreadable.
    fn read_u64(&mut self, address: VirtualAddress) -> Option<u64>;
}

pub struct UnwindStep {
    pub registers: RegisterFile,
    pub cfa: VirtualAddress,
    pub signal_frame: bool,
}

pub enum CallerResult {
    Caller(FrameContext),
    Finished(UnwindTermination),
}

pub trait CallerProvider {
    fn caller(&mut self, current: &FrameContext) -> CallerResult;
}

/// Unwinds from `initial` until the provider finishes, a frame repeats, or
/// `max_frames` frames are collected. `make_frame` sees each frame with the
/// provider that reconstructed it, so it can capture the frame's registers.
pub fn collect_frames<P: CallerProvider, F>(
    initial: FrameContext,
    provider: &mut P,
    mut make_frame: impl FnMut(u32, &FrameContext, &P) -> F,
    max_frames: usize,
) -> (Vec<F>, UnwindTermination) {
    debug_assert!(max_frames > 0, "a stack limited to no frames");
    let mut frames = Vec::new();
    let mut visited = HashSet::new();
    let mut current = initial;

    let termination = loop {
        // A repeated frame state would repeat every frame after it.
        if !visited.insert((current.cfa, current.instruction)) {
            break UnwindTermination::CycleDetected;
        }

        // Levels are numbered in 32 bits, so a stack deeper than that stops here.
        let Ok(level) = u32::try_from(frames.len()) else {
            break UnwindTermination::DepthLimit;
        };

        frames.push(make_frame(level, &current, provider));
        if frames.len() >= max_frames {
            break UnwindTermination::DepthLimit;
        }

        match provider.caller(&current) {
            CallerResult::Caller(caller) => current = caller,
            CallerResult::Finished(termination) => break termination,
        }
    };

    // No frame state repeats in a stack.
    debug_assert_eq!(
        visited.len(),
        frames.len(),
        "{} frames came from {} distinct frame states",
        frames.len(),
        visited.len()
    );
    (frames, termination)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Returns its callers last first, then finishes.
    struct SequenceProvider {
        callers: Vec<FrameContext>,
    }

    impl CallerProvider for SequenceProvider {
        fn caller(&mut self, _current: &FrameContext) -> CallerResult {
            self.callers.pop().map_or_else(
                || CallerResult::Finished(UnwindTermination::Complete),
                CallerResult::Caller,
            )
        }
    }

    fn context(instruction: u64, cfa: u64) -> FrameContext {
        FrameContext {
            instruction: VirtualAddress::new(instruction),
            cfa: Some(VirtualAddress::new(cfa)),
            signal_frame: false,
        }
    }

    #[test]
    fn a_register_file_keeps_any_number_and_forgets_what_it_is_told_to() {
        let register = DwarfRegister::new;
        let mut file =
            RegisterFile::new([(register(700), 7), (register(6), 60), (register(63), 1)]);
        file.set(register(64), 2);
        file.set(register(700), 70);
        assert_eq!(file.get(register(700)), Some(70));
        assert_eq!(file.get(register(5)), None);
        assert_eq!(
            file.iter().collect::<Vec<_>>(),
            [
                (register(6), 60),
                (register(63), 1),
                (register(64), 2),
                (register(700), 70)
            ]
        );

        // A register set and forgotten leaves the file as it was.
        let before = file.clone();
        file.set(register(0), 9);
        file.set(register(65), 9);
        file.remove(register(0));
        file.remove(register(65));
        assert_eq!(file, before);

        file.retain(|register| register.number() % 2 == 0);
        assert_eq!(
            file.iter().collect::<Vec<_>>(),
            [(register(6), 60), (register(64), 2), (register(700), 70)]
        );
    }

    #[test]
    fn collection_ends_with_the_provider_at_a_repeated_frame_or_at_the_limit() {
        let collect = |initial, callers, max_frames| {
            collect_frames(
                initial,
                &mut SequenceProvider { callers },
                |_, context, _| context.instruction.get(),
                max_frames,
            )
        };

        let callers = || vec![context(1, 10), context(2, 20)];
        assert_eq!(
            collect(context(3, 30), callers(), 16),
            (vec![3, 2, 1], UnwindTermination::Complete)
        );
        assert_eq!(
            collect(context(1, 10), vec![context(1, 10)], 16),
            (vec![1], UnwindTermination::CycleDetected)
        );
        assert_eq!(
            collect(context(3, 30), callers(), 1),
            (vec![3], UnwindTermination::DepthLimit)
        );
    }
}
