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
#[expect(
    clippy::too_many_lines,
    reason = "end-to-end fixture compiling C with inlining, verifying DWARF file/line/inline metadata, and testing strip_to_debug_elf"
)]
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
            assert!(
                frames[0]
                    .declared_file
                    .as_deref()
                    .unwrap_or("")
                    .ends_with("fixture.c"),
                "inlined callee must carry declared_file: {:?}",
                frames[0]
            );
            assert!(
                frames[1]
                    .file
                    .as_deref()
                    .unwrap_or("")
                    .ends_with("fixture.c"),
                "inline caller must carry call-site file: {:?}",
                frames[1]
            );
            assert!(frames[1].line > 0);
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
fn zlib_compressed_dwarf_larger_than_one_mebibyte_resolves_lines() {
    use object::{Object as _, ObjectSection as _};

    let dir = std::env::temp_dir().join(format!(
        "microscope-sym-zlib-test-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos())
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let src = dir.join("large.c");
    let bin = dir.join("large");

    let structs =
        (0..25_000).map(|i| format!("struct s{i} {{ int a; long b; char c[4]; double d; }};\n"));
    let source: String = std::iter::once("int main(void) { return 0; }\n".to_owned())
        .chain(structs)
        .collect();
    std::fs::write(&src, source).unwrap();

    let Ok(status) = Command::new("cc")
        .args([
            "-O0",
            "-g",
            "-gz=zlib",
            "-fno-eliminate-unused-debug-types",
            "-Wl,--build-id=sha1",
            "-o",
        ])
        .arg(&bin)
        .arg(&src)
        .status()
    else {
        return;
    };
    assert!(status.success(), "failed to compile large C fixture");
    let elf_bytes = std::fs::read(&bin).unwrap();
    let _ = std::fs::remove_dir_all(&dir);

    let file = object::File::parse(elf_bytes.as_slice()).unwrap();
    let debug_info = file
        .section_by_name(".debug_info")
        .unwrap()
        .compressed_data()
        .unwrap();
    assert_eq!(debug_info.format, object::CompressionFormat::Zlib);
    assert!(
        debug_info.uncompressed_size > 1024 * 1024,
        "fixture .debug_info must decompress past 1 MiB, got {}",
        debug_info.uncompressed_size
    );

    let (index, meta) = parse_elf(&elf_bytes, &ParseBudget::SERVER).unwrap();
    assert!(meta.has_dwarf);
    let main = index
        .find_function_address("main")
        .expect("main must be indexed");
    let frames = index.lookup(main);
    assert_eq!(frames[0].name, "main");
    assert!(
        frames[0].file.as_deref().unwrap_or("").ends_with("large.c"),
        "main must resolve to its source file: {frames:?}"
    );
    assert_eq!(frames[0].line, 1);
}

#[test]
fn symtab_only_c_fixture_survives_strip_to_debug_elf() {
    use std::process::Command;

    let Ok(cc) = std::env::var("CC").or_else(|_| Ok::<_, ()>("cc".to_owned())) else {
        return;
    };
    let dir = std::env::temp_dir().join(format!("microscope-symtab-test-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let src = dir.join("symtab.c");
    let bin = dir.join("symtab");
    std::fs::write(
        &src,
        r"
#include <stdint.h>
static volatile uint64_t sink = 0;
__attribute__((noinline)) uint64_t symtab_target_fn(uint64_t n) {
    uint64_t acc = 0;
    for (uint64_t i = 0; i < n; i++) {
        acc += i * 3ULL + 1ULL;
    }
    sink = acc;
    return acc;
}
int main(void) { return (int)(symtab_target_fn(10) & 0xff); }
",
    )
    .unwrap();
    let status = Command::new(cc)
        .args(["-O2", "-g0", "-Wl,--build-id=sha1", "-o"])
        .arg(&bin)
        .arg(&src)
        .status()
        .unwrap();
    assert!(status.success(), "failed compiling -g0 C fixture");

    let raw = std::fs::read(&bin).unwrap();
    let repacked = strip_to_debug_elf(&raw).unwrap();
    assert!(repacked.len() < raw.len());

    let (index, meta) = parse_elf(&repacked, &ParseBudget::SERVER).unwrap();
    assert!(meta.has_symtab);
    assert!(meta.range_count > 0);
    let addr = index
        .find_function_address("symtab_target_fn")
        .expect("symtab_target_fn must survive strip_to_debug_elf");
    let resolved = index.lookup(addr + 2);
    assert!(
        resolved.iter().any(|f| f.name == "symtab_target_fn"),
        "expected symtab_target_fn at {addr:#x}, got {resolved:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn stripped_go_binary_resolves_main_inside_executable_text() {
    use object::{Object as _, ObjectSection as _};
    use std::process::Command;

    if Command::new("go").arg("version").output().is_err() {
        return;
    }
    let dir = std::env::temp_dir().join(format!("microscope-go-sym-test-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let src = dir.join("main.go");
    let secret = b"MICROSCOPE_SECRET_NOPTRDATA_CANARY_9876543210";
    std::fs::write(
        &src,
        r#"package main
import "fmt"
var canary = [45]byte{'M','I','C','R','O','S','C','O','P','E','_','S','E','C','R','E','T','_','N','O','P','T','R','D','A','T','A','_','C','A','N','A','R','Y','_','9','8','7','6','5','4','3','2','1','0'}
func main() { fmt.Println("hello", canary[0]) }
"#,
    )
    .unwrap();

    let cases: &[(&str, &[&str], &str)] = &[
        (
            "goapp",
            &[
                "build",
                "-ldflags=-s -w -B 0x0123456789abcdef0123456789abcdef01234567",
            ],
            "0",
        ),
        (
            "goapp-pie-ext",
            &[
                "build",
                "-buildmode=pie",
                "-ldflags=-linkmode=external -s -w -B 0x0123456789abcdef0123456789abcdef01234567",
            ],
            "1",
        ),
    ];

    for (name, args, cgo) in cases {
        let bin = dir.join(name);
        let status = Command::new("go")
            .args(*args)
            .arg("-o")
            .arg(&bin)
            .arg(&src)
            .env("CGO_ENABLED", cgo)
            .env("GOCACHE", dir.join("gocache"))
            .status()
            .unwrap();
        if !status.success() && *cgo == "1" {
            continue;
        }
        assert!(
            status.success(),
            "failed compiling stripped Go fixture {name}"
        );
        let raw = std::fs::read(&bin).unwrap();
        let parsed_obj = object::File::parse(raw.as_slice()).unwrap();
        let text = parsed_obj.section_by_name(".text").unwrap();
        let text_start = text.address();
        let text_end = text_start + text.size();
        assert_eq!(
            extract_build_id(&raw).unwrap().as_str(),
            "0123456789abcdef0123456789abcdef01234567"
        );

        let (raw_index, raw_meta) = parse_elf(&raw, &ParseBudget::SERVER).unwrap();
        assert!(raw_meta.has_gopclntab);
        let raw_addr = raw_index
            .find_function_address("main.main")
            .expect("main.main must be indexed from raw .gopclntab");
        assert!(raw_addr >= text_start && raw_addr < text_end);

        let repacked = strip_to_debug_elf(&raw).unwrap();
        assert!(
            !repacked.windows(secret.len()).any(|w| w == secret),
            "repacked ELF for {name} must not retain initialized application data from .noptrdata"
        );
        let (repacked_index, repacked_meta) = parse_elf(&repacked, &ParseBudget::SERVER).unwrap();
        assert!(repacked_meta.has_gopclntab);
        let repacked_addr = repacked_index
            .find_function_address("main.main")
            .expect("main.main must be indexed from repacked .gopclntab");
        assert_eq!(
            repacked_addr, raw_addr,
            "repacked main.main address for {name} must match original ELF exactly"
        );
        let frames = repacked_index.lookup(repacked_addr);
        assert!(
            frames.iter().any(|f| f.name == "main.main"),
            "lookup({repacked_addr:#x}) must resolve main.main, got {frames:?}"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

fn elf_with_dwarf(dwarf: &mut gimli::write::DwarfUnit) -> Vec<u8> {
    let mut sections =
        gimli::write::Sections::new(gimli::write::EndianVec::new(gimli::LittleEndian));
    dwarf.write(&mut sections).unwrap();
    let mut debug = Vec::new();
    sections
        .for_each(|id, data| -> Result<(), ()> {
            debug.push((id.name(), data.slice().to_vec()));
            Ok(())
        })
        .unwrap();
    elf_with_debug_sections(&debug)
}

fn elf_with_debug_sections(debug: &[(&str, Vec<u8>)]) -> Vec<u8> {
    let mut elf = object::write::Object::new(
        object::BinaryFormat::Elf,
        object::Architecture::X86_64,
        object::Endianness::Little,
    );
    let text = elf.add_section(Vec::new(), b".text".to_vec(), object::SectionKind::Text);
    elf.append_section_data(text, &[0xc3; 16], 16);

    let note = elf.add_section(
        Vec::new(),
        b".note.gnu.build-id".to_vec(),
        object::SectionKind::Note,
    );
    let mut build_id = Vec::new();
    build_id.extend_from_slice(&4_u32.to_le_bytes());
    build_id.extend_from_slice(&20_u32.to_le_bytes());
    build_id.extend_from_slice(&object::elf::NT_GNU_BUILD_ID.to_le_bytes());
    build_id.extend_from_slice(b"GNU\0");
    build_id.extend_from_slice(&[0xab; 20]);
    elf.append_section_data(note, &build_id, 4);

    for (name, data) in debug.iter().filter(|(_, data)| !data.is_empty()) {
        let section = elf.add_section(
            Vec::new(),
            name.as_bytes().to_vec(),
            object::SectionKind::Debug,
        );
        elf.append_section_data(section, data, 1);
    }
    elf.write().unwrap()
}

fn uleb128(out: &mut Vec<u8>, mut value: u64) {
    loop {
        let byte = (value & 0x7f) as u8;
        value >>= 7;
        if value == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

#[test]
fn abbreviation_tables_cannot_multiply_parse_cost() {
    const ENTRIES: u64 = 300_000;
    const UNITS: usize = 200;

    let mut abbrev = Vec::new();
    uleb128(&mut abbrev, 1);
    uleb128(&mut abbrev, gimli::DW_TAG_compile_unit.0.into());
    abbrev.extend_from_slice(&[gimli::DW_CHILDREN_no.0, 0, 0]);
    for code in 2..ENTRIES {
        uleb128(&mut abbrev, code);
        uleb128(&mut abbrev, gimli::DW_TAG_subprogram.0.into());
        abbrev.push(gimli::DW_CHILDREN_no.0);
        for _ in 0..6 {
            uleb128(&mut abbrev, gimli::DW_AT_name.0.into());
            uleb128(&mut abbrev, gimli::DW_FORM_string.0.into());
        }
        abbrev.extend_from_slice(&[0, 0]);
    }
    abbrev.push(0);
    assert!(abbrev.len() < ParseBudget::SERVER.max_abbrev_bytes);

    let mut info = Vec::new();
    for _ in 0..UNITS {
        info.extend_from_slice(&8_u32.to_le_bytes());
        info.extend_from_slice(&4_u16.to_le_bytes());
        info.extend_from_slice(&0_u32.to_le_bytes());
        info.extend_from_slice(&[8, 1]);
    }

    let budget = ParseBudget {
        max_parse_time: std::time::Duration::from_millis(250),
        ..ParseBudget::SERVER
    };
    let started = std::time::Instant::now();
    let shared_table = parse_elf(
        &elf_with_debug_sections(&[(".debug_abbrev", abbrev), (".debug_info", info.clone())]),
        &budget,
    );
    let took = started.elapsed();
    assert!(
        matches!(shared_table, Err(SymbolError::BudgetExceeded(_))),
        "{UNITS} units sharing one {ENTRIES}-entry abbreviation table: {:?}",
        shared_table.map(|(_, meta)| meta)
    );
    assert!(took < std::time::Duration::from_secs(1), "took {took:?}");

    let oversized = vec![0_u8; ParseBudget::SERVER.max_abbrev_bytes + 1];
    let oversized = parse_elf(
        &elf_with_debug_sections(&[(".debug_abbrev", oversized), (".debug_info", info)]),
        &ParseBudget::SERVER,
    );
    assert!(
        matches!(oversized, Err(SymbolError::BudgetExceeded(_))),
        "{:?}",
        oversized.map(|(_, meta)| meta)
    );
}

fn peak_rss_bytes() -> u64 {
    let status = std::fs::read_to_string("/proc/self/status").unwrap();
    let kib: u64 = status
        .lines()
        .find_map(|line| line.strip_prefix("VmHWM:"))
        .and_then(|rest| rest.trim().strip_suffix("kB"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    kib * 1024
}

#[test]
fn many_ranges_under_deep_inlining_cost_memory_per_range_not_per_frame() {
    use gimli::write::{Address, AttributeValue, DwarfUnit, Range, RangeList};

    const RANGES: u64 = 300_000;
    const DEPTH: usize = 64;
    const ISOLATED: &str = "MICROSCOPE_SYMBOLS_ISOLATED_MEMORY_TEST";

    if std::env::var_os(ISOLATED).is_none() {
        let output = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "symbols::tests::many_ranges_under_deep_inlining_cost_memory_per_range_not_per_frame",
                "--nocapture",
            ])
            .env(ISOLATED, "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
        return;
    }

    let mut dwarf = DwarfUnit::new(gimli::Encoding {
        format: gimli::Format::Dwarf32,
        version: 5,
        address_size: 8,
    });
    let mut parent = dwarf.unit.root();
    for level in 0..DEPTH {
        let tag = if level == 0 {
            gimli::DW_TAG_subprogram
        } else {
            gimli::DW_TAG_inlined_subroutine
        };
        let die = dwarf.unit.add(parent, tag);
        dwarf.unit.get_mut(die).set(
            gimli::DW_AT_name,
            AttributeValue::String(format!("f{level}").into_bytes()),
        );
        parent = die;
    }
    let ranges = RangeList(
        (0..RANGES)
            .map(|i| Range::StartLength {
                begin: Address::Constant(0x1000 + 2 * i),
                length: 1,
            })
            .collect(),
    );
    let ranges = dwarf.unit.ranges.add(ranges);
    dwarf
        .unit
        .get_mut(parent)
        .set(gimli::DW_AT_ranges, AttributeValue::RangeListRef(ranges));
    let elf = elf_with_dwarf(&mut dwarf);
    drop(dwarf);

    std::fs::write("/proc/self/clear_refs", "5").unwrap();
    let before = peak_rss_bytes();
    let (index, _) = parse_elf(&elf, &ParseBudget::SERVER).unwrap();
    let used = peak_rss_bytes() - before;

    let frames = index.lookup(0x1000 + 2 * (RANGES - 1));
    let names: Vec<String> = frames.into_iter().map(|f| f.name).collect();
    let expected: Vec<String> = (0..DEPTH).rev().map(|level| format!("f{level}")).collect();
    assert_eq!(names, expected);
    assert_eq!(index.spans().len(), usize::try_from(RANGES).unwrap());

    assert!(
        used < 96 << 20,
        "parsing {RANGES} ranges at inline depth {DEPTH} raised peak RSS by {} MiB",
        used >> 20
    );
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
        let _ = SymbolIndex::decode_unverified_for_fuzz(&data, &ParseBudget::FUZZ);
        let _ = SymbolIndex::decode_body_for_fuzz(&data, &ParseBudget::FUZZ);
        let mut cursor = std::io::Cursor::new(&data);
        let _ = super::read_elf_build_id_from_reader(&mut cursor);
        let _ = strip_to_debug_elf(&data);
    }

    #[test]
    fn symbol_index_round_trips_and_lookup_matches_spans(
        span_specs in proptest::collection::vec(
            (1_u64..1024, 1_u64..256, 1_u32..500, "[a-z_]{1,12}", "[a-z_/]{1,12}"),
            1..24
        )
    ) {
        let mut strings = Vec::new();
        let mut files = Vec::new();
        let mut functions = Vec::new();
        let mut inlines = Vec::new();
        let mut spans = Vec::new();
        let mut line_spans = Vec::new();
        let mut symtab_spans = Vec::new();
        let mut cursor = 0x1000_u64;

        for (gap, len, line, fname, file_name) in span_specs {
            cursor = cursor.saturating_add(gap);
            let end = cursor.saturating_add(len);
            let name_id = u32::try_from(strings.len()).unwrap();
            strings.push(fname);
            let file_str_id = u32::try_from(strings.len()).unwrap();
            strings.push(file_name);
            let file_id = u32::try_from(files.len()).unwrap();
            files.push(file_str_id);
            let fn_id = u32::try_from(functions.len()).unwrap();
            functions.push(super::index::SymbolFunction {
                name: name_id,
                file: file_id,
                decl_line: line,
            });
            let inline_start = u32::try_from(inlines.len()).unwrap();
            inlines.push(super::index::InlineFrame {
                function: fn_id,
                call_file: super::index::NO_INDEX,
                call_line: 0,
            });
            spans.push(super::index::SymbolSpan {
                start: cursor,
                end,
                inline_start,
                inline_count: 1,
            });
            line_spans.push(super::index::LineSpan {
                start: cursor,
                end,
                file: file_id,
                line,
                column: 1,
            });
            symtab_spans.push(super::index::SymtabSpan {
                start: cursor,
                end,
                function: fn_id,
            });
            cursor = end;
        }

        let mut string_data = String::new();
        let mut string_offsets = Vec::with_capacity(strings.len() + 1);
        string_offsets.push(0_u32);
        for s in &strings {
            string_data.push_str(s);
            string_offsets.push(u32::try_from(string_data.len()).unwrap());
        }
        let index = SymbolIndex {
            build_id: BuildId::parse("0123456789abcdef").unwrap(),
            arch: "x86_64".to_owned(),
            has_dwarf: true,
            has_symtab: true,
            has_gopclntab: false,
            string_data,
            string_offsets,
            files,
            functions,
            inlines,
            spans: spans.clone(),
            line_spans,
            symtab_spans,
        };
        let encoded = index.encode();
        let decoded = SymbolIndex::decode(&encoded, &ParseBudget::SERVER).expect("decode");
        prop_assert_eq!(&decoded, &index);

        for span in &spans {
            let frames = decoded.lookup(span.start);
            prop_assert_eq!(frames.len(), 1);
            let frames_end = decoded.lookup(span.end - 1);
            prop_assert_eq!(frames_end.len(), 1);
        }
        prop_assert!(decoded.lookup(0).is_empty());

        for offset in 0..32 {
            let mut tampered = encoded.clone();
            tampered[offset] ^= 0x01;
            prop_assert!(SymbolIndex::decode(&tampered, &ParseBudget::SERVER).is_err());
        }
    }
}

#[test]
fn public_system_paths_reject_traversal_and_nul() {
    assert!(!is_public_system_path("/usr/lib/../../etc/passwd"));
    assert!(!is_public_system_path("/lib/foo/../bar"));
    assert!(!is_public_system_path("/nix/store/../secret/app"));
    assert!(!is_public_system_path("/usr/lib/libc.so\0/srv/app"));
}

#[test]
fn streaming_elf_build_id_matches_slice_extract() {
    let exe = std::env::current_exe().unwrap();
    let bytes = std::fs::read(&exe).unwrap();
    let expected = extract_build_id(&bytes).ok();
    let streamed = super::read_elf_build_id_from_file(&exe).ok().flatten();
    assert_eq!(streamed, expected);
}
