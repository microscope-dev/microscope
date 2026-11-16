#[cfg(target_arch = "x86_64")]
mod x86_64;

#[cfg(target_arch = "x86_64")]
pub(super) use x86_64::{
    AUDIT_ARCH_X86_64, BREAKPOINT_OPCODE, ELF_MACHINE, FLOATING_REGISTERS_SIZE, FloatingRegisters,
    GENERAL_REGISTER_COUNT, NATIVE_AUDIT_ARCH, PROGRAM_COUNTER, PRSTATUS_SIZE, Registers,
    SYSCALL_INSTRUCTION, caller_variable_register, floating_variable_register,
    is_floating_register, is_program_counter, stack_after_return,
};
