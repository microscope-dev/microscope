use std::process::Command;

use proptest::prelude::*;

use super::{
    BuildId, ParseBudget, SymbolError, SymbolIndex, demangle_symbol, extract_build_id,
    has_debug_or_symtab, is_public_system_path, parse_elf, strip_to_debug_elf,
};

#[test]
fn build_id_validation_and_roundtrip() {
    let valid = BuildId::parse("AbCdEf0123456789").unwrap();
    assert_eq!(valid.as_str(), "abcdef0123456789");
    let bytes = valid.to_bytes();
    assert_eq!(bytes, [0xab, 0xcd, 0xef, 0x01, 0x23, 0x45, 0x67, 0x89]);
    assert_eq!(BuildId::from_bytes(&bytes).unwrap(), valid);

    for bad in ["", "a", "abc", "zzzz", "01 23", &"a".repeat(130)] {
        assert!(matches!(
            BuildId::parse(bad),
            Err(SymbolError::InvalidBuildId)
        ));
    }
}

#[test]
fn public_system_paths_classify_correctly() {
    assert!(is_public_system_path("/usr/lib/x86_64-linux-gnu/libc.so.6"));
    assert!(is_public_system_path("/lib64/ld-linux-x86-64.so.2"));
    assert!(is_public_system_path("/nix/store/abc-glibc/lib/libc.so.6"));
    assert!(!is_public_system_path("/srv/app/checkout-api"));
    assert!(!is_public_system_path("/home/dev/target/release/server"));
    assert!(!is_public_system_path("relative/lib/foo.so"));
    assert!(!is_public_system_path("[vdso]"));
}

#[test]
fn demangling_covers_rust_cpp_and_plain() {
    assert_eq!(
        demangle_symbol("_RNvCsdHzz05DFzbQ_5crash9crash_now"),
        "crash::crash_now"
    );
    assert_eq!(
        demangle_symbol("_ZNSt6thread6detachEv"),
        "std::thread::detach()"
    );
    assert_eq!(demangle_symbol("plain_c_function"), "plain_c_function");
}

#[test]
fn parses_current_test_binary_and_roundtrips_index() {
    let exe = std::env::current_exe().unwrap();
    let bytes = std::fs::read(&exe).unwrap();
    let Ok(bid) = extract_build_id(&bytes) else {
        return;
    };
    assert!(has_debug_or_symtab(&bytes));

    let (index, meta) = parse_elf(&bytes, &ParseBudget::SERVER).unwrap();
    assert_eq!(meta.build_id, bid);
    assert!(meta.symbol_count > 0);
    assert!(meta.range_count > 0);

    let encoded = index.encode();
    let decoded = SymbolIndex::decode(&encoded, &ParseBudget::SERVER).unwrap();
    assert_eq!(index, decoded);

    let mut corrupted = encoded;
    let last = corrupted.len() - 1;
    corrupted[last] ^= 0xff;
    assert!(matches!(
        SymbolIndex::decode(&corrupted, &ParseBudget::SERVER),
        Err(SymbolError::ChecksumMismatch)
    ));
}

#[test]
fn compiled_c_fixture_with_inline_and_strip_to_debug() {
    let cc = ["cc", "gcc", "clang"].into_iter().find(|bin| {
        Command::new(bin)
            .arg("--version")
            .output()
            .is_ok_and(|o| o.status.success())
    });
    let Some(cc) = cc else {
        return;
    };

    let dir = std::env::temp_dir().join(format!(
        "microscope-sym-test-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos())
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let src = dir.join("fixture.c");
    let bin = dir.join("fixture");
    std::fs::write(
        &src,
        r"
static inline __attribute__((always_inline)) int inner_work(volatile int *p) {
    int sum = 0;
    for (int i = 0; i < 64; i++) {
        sum += (*p) + i;
    }
    return sum;
}

__attribute__((noinline)) int outer_work(volatile int *p) {
    return inner_work(p) + 1;
}

int main(void) {
    volatile int x = 7;
    return outer_work(&x) & 1;
}
",
    )
    .unwrap();

    let status = Command::new(cc)
        .args(["-O2", "-g", "-Wl,--build-id=sha1", "-o"])
        .arg(&bin)
        .arg(&src)
        .status()
        .unwrap();
    assert!(status.success(), "failed to compile C fixture");

    let elf_bytes = std::fs::read(&bin).unwrap();
    let bid = extract_build_id(&elf_bytes).unwrap();
    let (index, meta) = parse_elf(&elf_bytes, &ParseBudget::SERVER).unwrap();
    assert_eq!(meta.build_id, bid);
    assert!(meta.has_dwarf);
    assert!(index.contains_function("outer_work"));
    assert!(index.contains_function("inner_work"));

    let mut saw_inline_chain = false;
    for span in index.spans() {
        let frames = index.lookup(span.start);
        let names: Vec<&str> = frames.iter().map(|f| f.name.as_str()).collect();
        if names.contains(&"inner_work") && names.contains(&"outer_work") {
            assert_eq!(names[0], "inner_work");
            assert!(
                frames[0]
                    .file
                    .as_deref()
                    .unwrap_or("")
                    .ends_with("fixture.c")
            );
            assert!(frames[0].line > 0);
            saw_inline_chain = true;
            break;
        }
    }
    assert!(
        saw_inline_chain,
        "expected inline chain inner_work -> outer_work"
    );

    let stripped_debug = strip_to_debug_elf(&elf_bytes).unwrap();
    assert_eq!(extract_build_id(&stripped_debug).unwrap(), bid);
    let (stripped_index, stripped_meta) = parse_elf(&stripped_debug, &ParseBudget::SERVER).unwrap();
    assert_eq!(stripped_meta.build_id, bid);
    assert!(stripped_meta.has_dwarf);
    assert!(stripped_index.contains_function("outer_work"));
    assert!(stripped_index.contains_function("inner_work"));

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn strip_to_debug_elf_rejects_oversized_elf_header_without_panicking() {
    let mut elf = vec![0_u8; 256];
    elf[0..4].copy_from_slice(&[0x7f, b'E', b'L', b'F']);
    elf[4] = 2;
    elf[5] = 1;
    elf[40..48].copy_from_slice(&64_u64.to_le_bytes());
    elf[52..54].copy_from_slice(&65344_u16.to_le_bytes());
    elf[58..60].copy_from_slice(&64_u16.to_le_bytes());
    elf[60..62].copy_from_slice(&1_u16.to_le_bytes());
    elf[62..64].copy_from_slice(&0_u16.to_le_bytes());
    assert!(strip_to_debug_elf(&elf).is_err());
}

proptest! {
    #[test]
    fn random_bytes_never_panic_parsers(data in proptest::collection::vec(any::<u8>(), 0..1024)) {
        let _ = extract_build_id(&data);
        let _ = has_debug_or_symtab(&data);
        let _ = parse_elf(&data, &ParseBudget::FUZZ);
        let _ = SymbolIndex::decode(&data, &ParseBudget::FUZZ);
        let _ = strip_to_debug_elf(&data);
    }
}
