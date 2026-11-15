




    .text


    .p2align 4
    .globl disasm_marked_data
    .type disasm_marked_data, @function
disasm_marked_data:
    .cfi_startproc
    jmp 1f
    .byte 0x48, 0xb8
    .globl disasm_marked_resume
    .type disasm_marked_resume, @function
disasm_marked_resume:
1:
    movl $1, %eax
    ret
    .cfi_endproc
    .size disasm_marked_resume, . - disasm_marked_resume
    .size disasm_marked_data, . - disasm_marked_data


    .p2align 4
    .globl disasm_hidden_data
    .type disasm_hidden_data, @function
disasm_hidden_data:
    .cfi_startproc
    jmp 1f
    .byte 0x48, 0xb8
1:
    movl $1, %eax
    ret
    .cfi_endproc
    .size disasm_hidden_data, . - disasm_hidden_data

    .section .note.GNU-stack, "", @progbits
