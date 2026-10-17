//! A dependency-free ELF64 transport. The hook preserves argument registers
//! and returns. A separate reader loads the published command sequence in
//! one aligned native instruction; command data occupies a writable segment.
pub const SYMBOL: &str = "microscope_python_hook";
pub const SEQUENCE_SYMBOL: &str = "microscope_python_sequence";
pub const CONTROL_SYMBOL: &str = "microscope_python_control";
pub const CONTROL_OFFSET: usize = 4096;
pub const CONTROL_HEADER: usize = 32;
pub const CONTROL_CAPACITY: u32 = 64 * 1024;
pub const CONTROL_SIZE: usize = CONTROL_HEADER + CONTROL_CAPACITY as usize;
pub const CONTROL_MAGIC: &[u8; 8] = b"MICPYC02";

#[expect(
    clippy::too_many_lines,
    reason = "one fixed ELF layout keeps offsets auditable together"
)]
pub fn library() -> Vec<u8> {
    let mut elf = vec![0; CONTROL_OFFSET + CONTROL_SIZE];
    elf[..16].copy_from_slice(b"\x7fELF\x02\x01\x01\0\0\0\0\0\0\0\0\0");
    let mut put = |offset: usize, bytes: &[u8]| {
        elf[offset..offset + bytes.len()].copy_from_slice(bytes);
    };
    put(16, &3_u16.to_le_bytes());
    put(18, &62_u16.to_le_bytes());
    put(20, &1_u32.to_le_bytes());
    put(32, &64_u64.to_le_bytes());
    put(40, &1024_u64.to_le_bytes());
    for (offset, value) in [(52, 64_u16), (54, 56), (56, 4), (58, 64), (60, 9), (62, 6)] {
        put(offset, &value.to_le_bytes());
    }
    for (index, kind, flags, offset, size, alignment) in [
        (0, 1_u32, 5_u32, 0_u64, 1600_u64, 4096_u64),
        (1, 1, 6, 4096, CONTROL_SIZE as u64, 4096),
        (2, 2, 4, 752, 96, 8),
        (3, 0x6474_e551, 6, 0, 0, 16),
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
    put(512, &[0xf3, 0x0f, 0x1e, 0xfa, 0xc3]);
    // ENDBR64; one aligned 64-bit load of the published sequence; RET.
    put(528, &[0xf3, 0x0f, 0x1e, 0xfa, 0x48, 0x8b, 0x05]);
    put(535, &(4104_i32 - 539).to_le_bytes());
    put(539, &[0xc3]);
    // Null symbol at 544; the function at 568; the control object at 592.
    put(568, &1_u32.to_le_bytes());
    put(572, &[0x12, 0]);
    put(574, &1_u16.to_le_bytes());
    put(576, &512_u64.to_le_bytes());
    put(584, &5_u64.to_le_bytes());
    put(592, &24_u32.to_le_bytes());
    put(596, &[0x11, 0]);
    put(598, &8_u16.to_le_bytes());
    put(600, &(CONTROL_OFFSET as u64).to_le_bytes());
    put(608, &(CONTROL_SIZE as u64).to_le_bytes());
    put(616, &50_u32.to_le_bytes());
    put(620, &[0x12, 0]);
    put(622, &1_u16.to_le_bytes());
    put(624, &528_u64.to_le_bytes());
    put(632, &12_u64.to_le_bytes());
    let strings = format!("\0{SYMBOL}\0{CONTROL_SYMBOL}\0{SEQUENCE_SYMBOL}\0");
    put(640, strings.as_bytes());
    for (index, value) in [1_u32, 4, 1, 0, 2, 3, 0].into_iter().enumerate() {
        put(720 + index * 4, &value.to_le_bytes());
    }
    for (index, (tag, value)) in [
        (4_u64, 720_u64),
        (5, 640),
        (6, 544),
        (10, strings.len() as u64),
        (11, 24),
        (0, 0),
    ]
    .into_iter()
    .enumerate()
    {
        put(752 + index * 16, &tag.to_le_bytes());
        put(760 + index * 16, &value.to_le_bytes());
    }
    put(
        864,
        b"\0.text\0.dynsym\0.dynstr\0.hash\0.dynamic\0.shstrtab\0.eh_frame\0.data\0",
    );
    put(
        928,
        &[
            20, 0, 0, 0, 0, 0, 0, 0, 1, b'z', b'R', 0, 1, 0x78, 16, 1, 0x1b, 0x0c, 7, 8, 0x90, 1,
            0, 0,
        ],
    );
    put(952, &20_u32.to_le_bytes());
    put(956, &28_u32.to_le_bytes());
    put(960, &(-448_i32).to_le_bytes());
    put(964, &5_u32.to_le_bytes());
    put(976, &20_u32.to_le_bytes());
    put(980, &52_u32.to_le_bytes());
    put(984, &(-456_i32).to_le_bytes());
    put(988, &12_u32.to_le_bytes());
    put(CONTROL_OFFSET, CONTROL_MAGIC);
    put(CONTROL_OFFSET + 20, &CONTROL_CAPACITY.to_le_bytes());
    for (index, (name, kind, flags, offset, size, link, alignment, entry)) in [
        (1_u32, 1_u32, 6_u64, 512_u64, 28_u64, 0_u32, 4_u64, 0_u64),
        (7, 11, 2, 544, 96, 3, 8, 24),
        (15, 3, 2, 640, strings.len() as u64, 0, 1, 0),
        (23, 5, 2, 720, 28, 2, 4, 4),
        (29, 6, 2, 752, 96, 3, 8, 16),
        (38, 3, 0, 864, 64, 0, 1, 0),
        (48, 1, 2, 928, 76, 0, 8, 0),
        (
            58,
            1,
            3,
            CONTROL_OFFSET as u64,
            CONTROL_SIZE as u64,
            0,
            8,
            0,
        ),
    ]
    .into_iter()
    .enumerate()
    {
        let at = 1024 + (index + 1) * 64;
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
            put(at + 44, &1_u32.to_le_bytes());
        }
        put(at + 48, &alignment.to_le_bytes());
        put(at + 56, &entry.to_le_bytes());
    }
    elf
}
