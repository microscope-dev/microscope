use super::{DwarfRegister, MemoryReader, RegisterFile};
use crate::VirtualAddress;

pub const RAX: DwarfRegister = DwarfRegister(0);

pub const RDX: DwarfRegister = DwarfRegister(1);

pub const RCX: DwarfRegister = DwarfRegister(2);

pub const RBX: DwarfRegister = DwarfRegister(3);

pub const RSI: DwarfRegister = DwarfRegister(4);

pub const RDI: DwarfRegister = DwarfRegister(5);

pub const RBP: DwarfRegister = DwarfRegister(6);

pub const RSP: DwarfRegister = DwarfRegister(7);

pub const R8: DwarfRegister = DwarfRegister(8);

pub const R9: DwarfRegister = DwarfRegister(9);

pub const R12: DwarfRegister = DwarfRegister(12);

pub const RIP: DwarfRegister = DwarfRegister(16);

pub const FS_BASE: DwarfRegister = DwarfRegister(58);

pub const GS_BASE: DwarfRegister = DwarfRegister(59);

pub const PROGRAM_COUNTER: DwarfRegister = RIP;

pub const STACK_POINTER: DwarfRegister = RSP;

pub const FRAME_POINTER: DwarfRegister = RBP;

pub const ARGUMENTS: [DwarfRegister; 6] = [RDI, RSI, RDX, RCX, R8, R9];

pub const CALL_CLOBBERED_REGISTERS: [DwarfRegister; 34] = DwarfRegister::each([
    0, 1, 2, 4, 5, 8, 9, 10, 11, 49, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30, 31,
    32, 33, 34, 35, 36, 37, 38, 39, 40,
]);

pub const CALLEE_SAVED_REGISTERS: [DwarfRegister; 6] = DwarfRegister::each([3, 6, 12, 13, 14, 15]);

pub const GO_CALL_CLOBBERED_REGISTERS: [DwarfRegister; 40] = DwarfRegister::each([
    0, 1, 2, 3, 4, 5, 6, 8, 9, 10, 11, 12, 13, 14, 15, 49, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26,
    27, 28, 29, 30, 31, 32, 33, 34, 35, 36, 37, 38, 39, 40,
]);

pub const UCONTEXT_MCONTEXT: u64 = 40;

pub const SIGCONTEXT_REGISTERS: [DwarfRegister; 17] =
    DwarfRegister::each([8, 9, 10, 11, 12, 13, 14, 15, 5, 4, 6, 3, 1, 0, 2, 7, 16]);

pub fn interrupted_registers(
    stack_pointer: u64,
    memory: &mut dyn MemoryReader,
) -> Result<RegisterFile, VirtualAddress> {
    let mut registers = RegisterFile::new([]);
    for (slot, register) in SIGCONTEXT_REGISTERS.iter().enumerate() {
        let address = stack_pointer
            .checked_add(UCONTEXT_MCONTEXT + 8 * slot as u64)
            .map(VirtualAddress::new)
            .ok_or_else(|| VirtualAddress::new(stack_pointer))?;
        registers.set(*register, memory.read_u64(address).ok_or(address)?);
    }

    Ok(registers)
}
