






    .section .data.rel.ro, "aw"
    .p2align 3


    .globl disasm_slot
    .type disasm_slot, @object
disasm_slot:
    .quad disasm_target_one
    .size disasm_slot, . - disasm_slot



    .globl disasm_methods
    .type disasm_methods, @object
disasm_methods:
    .quad disasm_target_one
    .quad disasm_target_two
    .size disasm_methods, . - disasm_methods


    .globl disasm_cases
    .type disasm_cases, @object
disasm_cases:
    .quad disasm_case_zero
    .quad disasm_case_one
    .size disasm_cases, . - disasm_cases


    .section .tbss, "awT", @nobits
    .p2align 3
    .type disasm_thread_slot, @tls_object
disasm_thread_slot:
    .zero 8
    .size disasm_thread_slot, . - disasm_thread_slot

    .text

    .p2align 4
    .globl disasm_target_one
    .type disasm_target_one, @function
disasm_target_one:
    .cfi_startproc
    movl $0x1, %eax
    ret
    .cfi_endproc
    .size disasm_target_one, . - disasm_target_one

    .p2align 4
    .globl disasm_target_two
    .type disasm_target_two, @function
disasm_target_two:
    .cfi_startproc
    movl $0x2, %eax
    ret
    .cfi_endproc
    .size disasm_target_two, . - disasm_target_two

    .p2align 4
    .globl disasm_target_four
    .type disasm_target_four, @function
disasm_target_four:
    .cfi_startproc
    movl $0x4, %eax
    ret
    .cfi_endproc
    .size disasm_target_four, . - disasm_target_four


    .p2align 4
    .globl disasm_indirect
    .type disasm_indirect, @function
disasm_indirect:
    .cfi_startproc
    pushq %rbx
    .cfi_def_cfa_offset 16
    .cfi_offset %rbx, -16
    pushq %r12
    .cfi_def_cfa_offset 24
    .cfi_offset %r12, -24

    subq $8, %rsp
    .cfi_def_cfa_offset 32
    movl %edi, %r12d
    xorl %ebx, %ebx


disasm_call_slot:
    call *disasm_slot(%rip)
    orl %eax, %ebx


    leaq disasm_methods(%rip), %rax
disasm_call_method:
    call *8(%rax)
    orl %eax, %ebx


    leaq disasm_target_four(%rip), %rcx
disasm_call_register:
    call *%rcx
    orl %eax, %ebx


    leaq disasm_target_two(%rip), %rax
    movq %rax, %fs:disasm_thread_slot@tpoff
disasm_call_thread:
    call *%fs:disasm_thread_slot@tpoff
    shll $3, %eax
    orl %eax, %ebx



    movq $-8, %rdi
disasm_call_got:
    call *labs@GOTPCREL(%rip)
    orl %eax, %ebx


    leaq disasm_cases(%rip), %rdx
    movslq %r12d, %rax
disasm_jump_table:
    jmp *(%rdx,%rax,8)
disasm_case_zero:
    orl $0x40, %ebx
    jmp 1f
disasm_case_one:
    orl $0x20, %ebx
1:
    movl %ebx, %eax
    addq $8, %rsp
    .cfi_def_cfa_offset 24
    popq %r12
    .cfi_def_cfa_offset 16
    popq %rbx
    .cfi_def_cfa_offset 8
disasm_return:
    ret
    .cfi_endproc
    .size disasm_indirect, . - disasm_indirect



    .p2align 4
    .globl disasm_indirect_forms
    .type disasm_indirect_forms, @function
disasm_indirect_forms:
    .cfi_startproc
    jmp *0
    ljmp *(%rax)

    .byte 0x66, 0xff, 0xe0
    iretq
    .cfi_endproc
    .size disasm_indirect_forms, . - disasm_indirect_forms

    .section .note.GNU-stack, "", @progbits
