//! What microscope's BPF programs share. Their C sources live here beside
//! this module: `build.rs` compiles each `*.bpf.c` into `OUT_DIR`, and every
//! program writes the records of `records.h`, which the Rust side mirrors
//! field for field. The syscall engine's loader is
//! `super::syscalls::bpf`; the profiler's is [`sampler`].

pub mod sampler;

/// An object's BTF: each struct's size and its members' names and byte
/// offsets, which tests hold the Rust mirrors of its structs to.
#[cfg(test)]
pub fn btf_structs(object: &[u8]) -> std::collections::BTreeMap<String, (u32, Vec<(String, u32)>)> {
    use object::{Object as _, ObjectSection as _};
    let file = object::File::parse(object).expect("the object parses");
    let section = file.section_by_name(".BTF").expect("a .BTF section");
    let data = section.data().expect("its bytes");
    let u32_at = |at: usize| u32::from_le_bytes(data[at..at + 4].try_into().expect("4"));
    let header_len = u32_at(4) as usize;
    let (type_off, type_len) = (u32_at(8) as usize, u32_at(12) as usize);
    let str_off = u32_at(16) as usize;
    let types = header_len + type_off;
    let strings = header_len + str_off;
    let name = |offset: u32| {
        let start = strings + offset as usize;
        let end = start + data[start..].iter().position(|&b| b == 0).expect("NUL");
        String::from_utf8_lossy(&data[start..end]).into_owned()
    };

    let mut structs = std::collections::BTreeMap::new();
    let mut at = types;
    while at < types + type_len {
        let (name_off, info, size) = (u32_at(at), u32_at(at + 4), u32_at(at + 8));
        let kind = (info >> 24) & 0x1f;
        let vlen = (info & 0xffff) as usize;
        at += 12;

        // Each kind's trailing data, by the BTF format.
        let trailing = match kind {
            1 | 14 | 15 | 17 => 4,
            3 => 12,
            4 | 5 => 12 * vlen,
            6 | 13 => 8 * vlen,
            19 => 16 * vlen,
            _ => 0,
        };

        if kind == 4 {
            let members = (0..vlen)
                .map(|index| {
                    let member = at + index * 12;
                    (name(u32_at(member)), (u32_at(member + 8) & 0x00ff_ffff) / 8)
                })
                .collect();
            structs.insert(name(name_off), (size, members));
        }

        at += trailing;
    }

    structs
}

/// Holds each Rust mirror of a C struct to the struct in an object's BTF:
/// each entry is `(mirror, "c_name", [fields])`, the fields in order.
#[cfg(test)]
macro_rules! assert_mirrors {
    ($object:expr, $(($type:ty, $c:literal, [$($field:ident),* $(,)?])),* $(,)?) => {{
        let structs = $crate::backend::linux::bpf::btf_structs($object);
        $(
            let (c_size, c_fields) = structs
                .get($c)
                .unwrap_or_else(|| panic!("the object has no struct {}", $c));
            assert_eq!(*c_size as usize, size_of::<$type>(), "struct {}'s size", $c);
            let rust: Vec<(String, u32)> = vec![$(
                (
                    stringify!($field).to_owned(),
                    u32::try_from(std::mem::offset_of!($type, $field)).expect("small"),
                )
            ),*];
            assert_eq!(c_fields, &rust, "struct {}'s fields", $c);
        )*
    }};
}

#[cfg(test)]
pub(crate) use assert_mirrors;
