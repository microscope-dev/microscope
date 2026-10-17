//! A dependency-free ELF64 shared library. Its only function preserves the
//! System V argument registers and returns. No compiler, relocations, libc,
//! or libpython is needed. Section headers let the image loader find the
//! same export the dynamic linker finds.

pub const SYMBOL: &str = "microscope_python_hook";

#[cfg_attr(
    not(test),
    expect(dead_code, reason = "agent injection consumes this in P4.7")
)]
pub fn library() -> Vec<u8> {
    let mut elf = vec![0; 1152];
    elf[..16].copy_from_slice(b"\x7fELF\x02\x01\x01\0\0\0\0\0\0\0\0\0");
    let mut put = |offset: usize, bytes: &[u8]| {
        elf[offset..offset + bytes.len()].copy_from_slice(bytes);
    };
    put(16, &3_u16.to_le_bytes()); // ET_DYN
    put(18, &62_u16.to_le_bytes()); // EM_X86_64
    put(20, &1_u32.to_le_bytes());
    put(32, &64_u64.to_le_bytes()); // program headers
    put(40, &640_u64.to_le_bytes()); // section headers
    for (offset, value) in [(52, 64_u16), (54, 56), (56, 3), (58, 64), (60, 8), (62, 6)] {
        put(offset, &value.to_le_bytes());
    }

    // PT_LOAD (read/execute), PT_DYNAMIC (read), PT_GNU_STACK (read/write).
    for (index, kind, flags, offset, size, alignment) in [
        (0, 1_u32, 5_u32, 0_u64, 1152_u64, 4096_u64),
        (1, 2, 4, 368, 112, 8),
        (2, 0x6474_e551, 6, 0, 0, 16),
    ] {
        let at = 64 + index * 56;
        put(at, &kind.to_le_bytes());
        put(at + 4, &flags.to_le_bytes());
        for displacement in [8, 16, 24] {
            put(at + displacement, &offset.to_le_bytes());
        }
        put(at + 32, &size.to_le_bytes());
        put(at + 40, &size.to_le_bytes());
        put(at + 48, &alignment.to_le_bytes());
    }

    // ENDBR64; RET. The entry supports indirect-branch tracking.
    put(256, &[0xf3, 0x0f, 0x1e, 0xfa, 0xc3]);
    // .dynsym: the null symbol, then the global function.
    put(288, &1_u32.to_le_bytes());
    put(292, &[0x12, 0]);
    put(294, &1_u16.to_le_bytes());
    put(296, &256_u64.to_le_bytes());
    put(304, &5_u64.to_le_bytes());
    put(312, format!("\0{SYMBOL}\0").as_bytes());
    // SysV hash: one bucket and two chains, bucket 0 names symbol 1.
    for (index, value) in [1_u32, 2, 1, 0, 0].into_iter().enumerate() {
        put(344 + index * 4, &value.to_le_bytes());
    }
    for (index, (tag, value)) in [
        (4_u64, 344_u64), // DT_HASH
        (5, 312),         // DT_STRTAB
        (6, 264),         // DT_SYMTAB
        (10, 24),         // DT_STRSZ
        (11, 24),         // DT_SYMENT
        (0, 0),           // DT_NULL
    ]
    .into_iter()
    .enumerate()
    {
        put(368 + index * 16, &tag.to_le_bytes());
        put(376 + index * 16, &value.to_le_bytes());
    }
    put(
        480,
        b"\0.text\0.dynsym\0.dynstr\0.hash\0.dynamic\0.shstrtab\0.eh_frame\0",
    );
    // .eh_frame: CFA = rsp + 8; the caller's PC is at CFA - 8. The hook
    // changes neither. zR uses a PC-relative signed 32-bit initial address.
    put(
        544,
        &[
            20, 0, 0, 0, 0, 0, 0, 0, 1, b'z', b'R', 0, 1, 0x78, 16, 1, 0x1b, 0x0c, 7, 8, 0x90, 1,
            0, 0,
        ],
    );
    put(568, &20_u32.to_le_bytes());
    put(572, &28_u32.to_le_bytes()); // distance back to the CIE
    put(576, &(-320_i32).to_le_bytes()); // 256 - 576
    put(580, &5_u32.to_le_bytes());
    // Augmentation length, padding, and the terminator are all zero.
    // name, type, flags, address/file offset, size, link, alignment, entry size.
    for (index, (name, kind, flags, offset, size, link, alignment, entry)) in [
        (1_u32, 1_u32, 6_u64, 256_u64, 5_u64, 0_u32, 4_u64, 0_u64),
        (7, 11, 2, 264, 48, 3, 8, 24),
        (15, 3, 2, 312, 24, 0, 1, 0),
        (23, 5, 2, 344, 20, 2, 4, 4),
        (29, 6, 2, 368, 112, 3, 8, 16),
        (38, 3, 0, 480, 58, 0, 1, 0),
        (48, 1, 2, 544, 52, 0, 8, 0),
    ]
    .into_iter()
    .enumerate()
    {
        let at = 640 + (index + 1) * 64;
        put(at, &name.to_le_bytes());
        put(at + 4, &kind.to_le_bytes());
        put(at + 8, &flags.to_le_bytes());
        if flags != 0 {
            put(at + 16, &offset.to_le_bytes());
        }
        put(at + 24, &offset.to_le_bytes());
        put(at + 32, &size.to_le_bytes());
        put(at + 40, &link.to_le_bytes());
        if kind == 11 {
            put(at + 44, &1_u32.to_le_bytes()); // first non-local symbol
        }
        put(at + 48, &alignment.to_le_bytes());
        put(at + 56, &entry.to_le_bytes());
    }
    elf
}
