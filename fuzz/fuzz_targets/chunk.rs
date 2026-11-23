#![no_main]

use libfuzzer_sys::arbitrary::Unstructured;
use libfuzzer_sys::fuzz_target;

use microscope::object::codec::{
    DecodeBudget, SectionReader, WHOLE_SECTION_GROUP, decode_binned_2b, decode_bp_tree_budgeted,
    decode_dict_u64_budgeted, decode_nonzero_u64, decode_sparse_histograms, decode_varints,
    decode_words, delta_by_key_decode_u64, delta_decode_i64, delta_decode_u64, relative_decode_i64,
    relative_decode_u64,
};
use microscope::object::{
    BinaryFuse8, DEFAULT_ZSTD_LEVEL, ObjectReader, SectionKind, decode_flame_tables_budgeted,
    decode_section_index, decode_tables_budgeted,
};
use microscope::sanitize::{Builtin, HashKey, Sanitizer, re_sanitize_sections_verbatim};
use microscope::symbols::{
    ParseBudget, SymbolIndex, extract_build_id, has_debug_or_symtab, parse_elf,
    read_elf_build_id_from_reader, strip_to_debug_elf,
};

const FUZZ_BUDGET: usize = 8 << 20;

fn fuzz_object_reader(reader: &ObjectReader<'_>, unverified: bool) {
    let sanitizer = Sanitizer::new(&Builtin::ALL, Vec::new(), HashKey::new(b"fuzz"));
    for idx in 0..reader.parts().len().min(8) {
        let sections = if unverified {
            reader.part_sections_unverified(idx)
        } else {
            reader.part_sections(idx, None)
        };
        if let Ok(sections) = sections {
            let _ = decode_tables_budgeted(&sections, FUZZ_BUDGET);
            let _ = decode_flame_tables_budgeted(&sections, FUZZ_BUDGET);
            let _ = re_sanitize_sections_verbatim(&sections, &sanitizer, DEFAULT_ZSTD_LEVEL);
        }
    }
}

fn fuzz_section_reader(data: &[u8]) {
    if data.len() < 2 {
        return;
    }
    let kind_raw = u16::from_le_bytes([data[0], data[1]]);
    let Some(kind) = SectionKind::from_number(kind_raw) else {
        return;
    };
    let budget = DecodeBudget::new(FUZZ_BUDGET);
    let Ok(section) = SectionReader::open_budgeted(data, kind, &budget) else {
        return;
    };
    let _ = section.read_single_page();
    for page_idx in 0..section.page_count().min(4) {
        let Ok(page) = section.read_page(page_idx) else {
            continue;
        };
        let rows = page.rows as usize;
        for col_id in 0..16_u8 {
            if let Some(col) = page.optional_column(WHOLE_SECTION_GROUP, col_id) {
                let _ = col.decode_u64(rows, Some(&budget));
                let _ = col.decode_u32(rows, Some(&budget));
                let _ = col.decode_u8(rows, Some(&budget));
            }
        }
        for group_idx in 0..page.groups.len().min(4) {
            let group_id = u16::try_from(group_idx).unwrap_or(u16::MAX);
            let group_rows = page.groups[group_idx].rows.get() as usize;
            for col_id in 0..8_u8 {
                if let Some(col) = page.optional_column(group_id, col_id) {
                    let _ = col.decode_u64(group_rows, Some(&budget));
                }
            }
        }
    }
}

fuzz_target!(|data: &[u8]| {
    if data.is_empty() {
        return;
    }

    let mut u = Unstructured::new(data);
    let mode: u8 = u.arbitrary().unwrap_or(0);
    let count = usize::from(u.arbitrary::<u16>().unwrap_or(0)).min(4096);
    let payload = u.take_rest();

    match mode % 8 {
        0 => {
            if let Ok(reader) = ObjectReader::open(data) {
                fuzz_object_reader(&reader, false);
            }
            if let Ok(reader) = ObjectReader::open_recoverable(data) {
                fuzz_object_reader(&reader, false);
            }
        }
        1 => {
            if let Ok(reader) = ObjectReader::open_unverified(payload) {
                fuzz_object_reader(&reader, true);
            }
            if let Ok(reader) = ObjectReader::open_unverified(data) {
                fuzz_object_reader(&reader, true);
            }
        }
        2 => {
            fuzz_section_reader(payload);
            fuzz_section_reader(data);
        }
        3 => {
            let budget = DecodeBudget::new(FUZZ_BUDGET);
            let _ = decode_varints(payload, count);
            let _ = decode_dict_u64_budgeted(payload, count, Some(&budget));
            let _ = decode_words(payload, count.min(256), usize::from((mode >> 3) & 7) + 1);
            let half = payload.len() / 2;
            let _ = decode_binned_2b(&payload[..half], &payload[half..], count);
            let _ = decode_bp_tree_budgeted(payload, count, Some(&budget));
            let _ = decode_sparse_histograms(payload, count.min(256), Some(&budget));
            let _ = decode_nonzero_u64(payload, count);
            let vals: Vec<u64> = payload.iter().take(count).map(|&b| u64::from(b)).collect();
            let keys: Vec<u32> = payload
                .iter()
                .take(vals.len())
                .map(|&b| u32::from(b))
                .collect();
            let _ = delta_decode_u64(&vals);
            let _ = delta_decode_i64(&vals, i64::from(mode));
            let _ = relative_decode_u64(&vals, &vals);
            let base_i64: Vec<i64> = vals.iter().map(|&v| v as i64).collect();
            let _ = relative_decode_i64(&vals, &base_i64);
            let _ = delta_by_key_decode_u64(&vals, &keys);
        }
        4 => {
            let _ = BinaryFuse8::from_bytes(payload);
            let _ = BinaryFuse8::from_bytes(data);
            let _ = decode_section_index(payload);
            let _ = decode_section_index(data);
        }
        5 => {
            let _ = SymbolIndex::decode(data, &ParseBudget::FUZZ);
            let _ = SymbolIndex::decode_unverified_for_fuzz(payload, &ParseBudget::FUZZ);
            let _ = SymbolIndex::decode_body_for_fuzz(payload, &ParseBudget::FUZZ);
        }
        6 => {
            let _ = extract_build_id(payload);
            let _ = has_debug_or_symtab(payload);
            let mut cursor = std::io::Cursor::new(payload);
            let _ = read_elf_build_id_from_reader(&mut cursor);
            let _ = strip_to_debug_elf(payload);
        }
        _ => {
            let _ = parse_elf(payload, &ParseBudget::FUZZ);
            if let Ok(idx) = SymbolIndex::decode_body_for_fuzz(payload, &ParseBudget::FUZZ) {
                let addr: u64 = u64::from(mode) << 12;
                let _ = idx.lookup(addr);
            }
        }
    }
});
