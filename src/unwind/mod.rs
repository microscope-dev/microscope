use std::collections::HashSet;

use crate::debug_info::UnwindInfo;
use crate::{ImageAddress, LoadedModule, ModuleImage, UnwindTermination, VirtualAddress};

pub mod code_copies;
pub mod sample;
pub mod x86_64;

#[cfg(target_arch = "x86_64")]
pub use x86_64::{
    ARGUMENTS, CALL_CLOBBERED_REGISTERS, CALLEE_SAVED_REGISTERS, FRAME_POINTER,
    GO_CALL_CLOBBERED_REGISTERS, PROGRAM_COUNTER, STACK_POINTER, interrupted_registers,
};

pub const DEFAULT_MAX_FRAMES: usize = 256;

#[derive(Clone, Copy)]
pub struct UnwindModule<'a> {
    pub loaded: LoadedModule,
    pub image: &'a ModuleImage,
    pub unwind: &'a dyn UnwindInfo,
}

pub fn unwind_module_for<'a>(
    modules: &[UnwindModule<'a>],
    address: VirtualAddress,
) -> Option<(UnwindModule<'a>, ImageAddress)> {
    unwind_module_at(modules, address)
        .and_then(|(index, image_address)| Some((*modules.get(index)?, image_address)))
}

pub trait Modules {
    fn module(&self, index: usize) -> UnwindModule<'_>;

    fn module_at(&self, address: VirtualAddress) -> Option<(usize, ImageAddress)>;
}

impl Modules for [UnwindModule<'_>] {
    #[expect(
        clippy::indexing_slicing,
        reason = "callers pass an index that module_at found in this slice"
    )]
    fn module(&self, index: usize) -> UnwindModule<'_> {
        debug_assert!(index < self.len(), "module {index} of {}", self.len());
        self[index]
    }

    fn module_at(&self, address: VirtualAddress) -> Option<(usize, ImageAddress)> {
        unwind_module_at(self, address)
    }
}

pub fn unwind_module_at(
    modules: &[UnwindModule<'_>],
    address: VirtualAddress,
) -> Option<(usize, ImageAddress)> {
    modules.iter().enumerate().find_map(|(index, module)| {
        module
            .loaded
            .load_bias
            .unload(address)
            .filter(|image_address| {
                module.loaded.windows(*image_address)
                    && module.image.contains_address(*image_address)
            })
            .map(|image_address| (index, image_address))
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FramePointerRule {
    Kept,

    NotKept,

    Outermost,
    NoUnwindInfo,
}

/// Probes `unwind` at `address` with synthetic `RBP`/`RSP` values and a
/// bitwise-inverting memory reader to test whether CFI preserves the standard
/// frame-pointer chain (`CFA == RBP + 16`, saved `RBP` at `[RBP]`, caller at `[RBP + 8]`).
pub fn frame_pointer_rule<U: UnwindInfo + ?Sized>(
    unwind: &U,
    address: ImageAddress,
) -> FramePointerRule {
    const FRAME: u64 = 0x4000_0000_0000_0000;
    const STACK: u64 = 0x2000_0000_0000_0000;

    struct Named;

    impl MemoryReader for Named {
        fn read_u64(&mut self, address: VirtualAddress) -> Option<u64> {
            Some(!address.get())
        }
    }

    let registers = RegisterFile::new([(FRAME_POINTER, FRAME), (STACK_POINTER, STACK)]);
    match unwind.unwind(address, &registers, &mut Named) {
        Ok(step) => match step.registers.get(PROGRAM_COUNTER) {
            None => FramePointerRule::Outermost,
            Some(caller)
                if step.cfa.get() == FRAME + 16
                    && caller == !(FRAME + 8)
                    && step.registers.get(FRAME_POINTER) == Some(!FRAME) =>
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

    #[must_use]
    #[expect(
        clippy::indexing_slicing,
        reason = "at < N, the length of both arrays; a const fn cannot use iterators"
    )]
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

#[derive(Debug, Clone)]
pub struct FrameContext {
    pub instruction: VirtualAddress,
    pub cfa: Option<VirtualAddress>,
    pub signal_frame: bool,
}

const DENSE_REGISTERS: usize = 64;

/// Registers `0..64` live in a dense inline array guarded by the `known` bitmask
/// to avoid heap allocation on hot unwind paths; higher register numbers spill
/// into the sorted `others` vector.
#[derive(Clone, PartialEq, Eq)]
pub struct RegisterFile {
    known: u64,

    values: [u64; DENSE_REGISTERS],

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
        if let Some(value) = self.values.get(number) {
            return (self.known & (1 << number) != 0).then_some(*value);
        }

        self.others
            .binary_search_by_key(&register, |(other, _)| *other)
            .ok()
            .and_then(|at| self.others.get(at))
            .map(|(_, value)| *value)
    }

    pub fn set(&mut self, register: DwarfRegister, value: u64) {
        let number = usize::from(register.0);
        if let Some(slot) = self.values.get_mut(number) {
            self.known |= 1 << number;
            *slot = value;
            return;
        }

        match self
            .others
            .binary_search_by_key(&register, |(other, _)| *other)
        {
            Ok(at) => {
                if let Some(other) = self.others.get_mut(at) {
                    other.1 = value;
                }
            }
            Err(at) => self.others.insert(at, (register, value)),
        }
    }

    pub fn remove(&mut self, register: DwarfRegister) {
        let number = usize::from(register.0);
        if let Some(slot) = self.values.get_mut(number) {
            self.known &= !(1 << number);
            *slot = 0;
            return;
        }

        self.others.retain(|(other, _)| *other != register);
    }

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

    pub fn iter(&self) -> impl Iterator<Item = (DwarfRegister, u64)> + '_ {
        self.values
            .iter()
            .enumerate()
            .filter(|(number, _)| self.known & (1 << number) != 0)
            .map(|(number, value)| {
                #[expect(clippy::cast_possible_truncation, reason = "below 64")]
                (DwarfRegister(number as u16), *value)
            })
            .chain(self.others.iter().copied())
    }
}

impl std::fmt::Debug for RegisterFile {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_map().entries(self.iter()).finish()
    }
}

pub trait MemoryReader {
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
        if !visited.insert((current.cfa, current.instruction)) {
            break UnwindTermination::CycleDetected;
        }

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
    use proptest::prelude::*;

    use super::*;

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

    #[derive(Debug, Clone)]
    enum Change {
        Set(u16, u64),
        Remove(u16),

        Retain(u16),
    }

    fn number() -> impl Strategy<Value = u16> {
        prop_oneof![
            prop::sample::select(vec![0, 1, 6, 16, 62, 63, 64, 65, 700, u16::MAX]),
            0_u16..80,
            any::<u16>(),
        ]
    }

    proptest::proptest! {
        /// A file holds what a map of the same changes holds, and two
        /// files of the same registers are equal however they got them.
        #[test]
        fn a_register_file_is_a_map_by_number(
            changes in proptest::collection::vec(
                prop_oneof![
                    4 => (number(), crate::test_strategies::u64_edges())
                        .prop_map(|(number, value)| Change::Set(number, value)),
                    2 => number().prop_map(Change::Remove),
                    1 => (0_u16..3).prop_map(Change::Retain),
                ],
                0..24,
            ),
        ) {
            let mut file = RegisterFile::new([]);
            let mut model = std::collections::BTreeMap::new();
            for change in changes {
                match change {
                    Change::Set(number, value) => {
                        file.set(DwarfRegister::new(number), value);
                        model.insert(number, value);
                    }
                    Change::Remove(number) => {
                        file.remove(DwarfRegister::new(number));
                        model.remove(&number);
                    }
                    Change::Retain(remainder) => {
                        file.retain(|register| register.number() % 3 == remainder);
                        model.retain(|number, _| number % 3 == remainder);
                    }
                }

                let held = model
                    .iter()
                    .map(|(number, value)| (DwarfRegister::new(*number), *value))
                    .collect::<Vec<_>>();
                prop_assert_eq!(file.iter().collect::<Vec<_>>(), held.clone());
                for number in [0, 63, 64, u16::MAX].into_iter().chain(model.keys().copied()) {
                    prop_assert_eq!(
                        file.get(DwarfRegister::new(number)),
                        model.get(&number).copied()
                    );
                }
                prop_assert_eq!(&file, &RegisterFile::new(held));
            }
        }
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
