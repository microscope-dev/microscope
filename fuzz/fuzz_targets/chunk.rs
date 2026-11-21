#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = microscope::object::BinaryFuse8::from_bytes(data);
    let _ = microscope::object::decode_section_index(data);
    if let Ok(reader) = microscope::object::ObjectReader::open(data) {
        for idx in 0..reader.parts().len() {
            if let Ok(sections) = reader.part_sections(idx, None) {
                let _ = microscope::object::decode_tables(&sections);
                let _ = microscope::object::decode_flame_tables(&sections);
            }
        }
    }
    if let Ok(reader) = microscope::object::ObjectReader::open_recoverable(data) {
        for idx in 0..reader.parts().len() {
            if let Ok(sections) = reader.part_sections(idx, None) {
                let _ = microscope::object::decode_tables(&sections);
            }
        }
    }
});
