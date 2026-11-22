use object::elf;
use object::{Endian as _, Endianness};

use super::SymbolError;

const ELF_MAGIC: [u8; 4] = [0x7f, b'E', b'L', b'F'];

#[expect(
    clippy::too_many_lines,
    reason = "repacks ELF64 program headers, debug/symtab sections, and updated section headers in one pass"
)]
pub fn strip_to_debug_elf(bytes: &[u8]) -> Result<Vec<u8>, SymbolError> {
    if bytes.len() < 64 || bytes[..4] != ELF_MAGIC {
        return Err(SymbolError::NotElf("missing ELF magic".to_owned()));
    }
    let class = bytes[4];
    let data = bytes[5];
    if class != elf::ELFCLASS64 {
        return Ok(bytes.to_vec());
    }
    let endian = match data {
        elf::ELFDATA2LSB => Endianness::Little,
        elf::ELFDATA2MSB => Endianness::Big,
        _ => return Err(SymbolError::NotElf("unknown ELF endianness".to_owned())),
    };

    let prog_hdr_off = read_u64(bytes, 32, endian)?;
    let sec_hdr_off = read_u64(bytes, 40, endian)?;
    let elf_hdr_size = usize::from(read_u16(bytes, 52, endian)?);
    let prog_entry_size = usize::from(read_u16(bytes, 54, endian)?);
    let prog_count = usize::from(read_u16(bytes, 56, endian)?);
    let sec_entry_size = usize::from(read_u16(bytes, 58, endian)?);
    let sec_count = usize::from(read_u16(bytes, 60, endian)?);
    let shstr_index = usize::from(read_u16(bytes, 62, endian)?);

    if elf_hdr_size < 64 || sec_entry_size < 64 || sec_count == 0 || shstr_index >= sec_count {
        return Ok(bytes.to_vec());
    }
    if elf_hdr_size > bytes.len() {
        return Err(SymbolError::NotElf(
            "ELF header extends past file".to_owned(),
        ));
    }
    let ph_off =
        usize::try_from(prog_hdr_off).map_err(|_| SymbolError::NotElf("phoff".to_owned()))?;
    let ph_len = prog_entry_size
        .checked_mul(prog_count)
        .ok_or_else(|| SymbolError::NotElf("ph size".to_owned()))?;
    let sh_off =
        usize::try_from(sec_hdr_off).map_err(|_| SymbolError::NotElf("shoff".to_owned()))?;
    let sh_len = sec_entry_size
        .checked_mul(sec_count)
        .ok_or_else(|| SymbolError::NotElf("sh size".to_owned()))?;
    if ph_off
        .checked_add(ph_len)
        .is_none_or(|end| end > bytes.len())
        || sh_off
            .checked_add(sh_len)
            .is_none_or(|end| end > bytes.len())
    {
        return Err(SymbolError::NotElf(
            "ELF headers extend past file".to_owned(),
        ));
    }

    let shstr_hdr_off = sh_off
        .checked_add(
            shstr_index
                .checked_mul(sec_entry_size)
                .ok_or_else(|| SymbolError::NotElf("shstrtab header offset".to_owned()))?,
        )
        .ok_or_else(|| SymbolError::NotElf("shstrtab header offset".to_owned()))?;
    let shstr_offset = usize::try_from(read_u64(bytes, shstr_hdr_off + 24, endian)?)
        .map_err(|_| SymbolError::NotElf("shstrtab offset".to_owned()))?;
    let shstr_size = usize::try_from(read_u64(bytes, shstr_hdr_off + 32, endian)?)
        .map_err(|_| SymbolError::NotElf("shstrtab size".to_owned()))?;
    let shstr_end = shstr_offset
        .checked_add(shstr_size)
        .ok_or_else(|| SymbolError::NotElf("truncated shstrtab".to_owned()))?;
    let shstr = bytes
        .get(shstr_offset..shstr_end)
        .ok_or_else(|| SymbolError::NotElf("truncated shstrtab".to_owned()))?;

    let mut headers: Vec<Vec<u8>> = Vec::with_capacity(sec_count);
    let mut keep_section: Vec<bool> = vec![false; sec_count];
    let mut kept_linked: Vec<usize> = Vec::new();

    for (i, keep_slot) in keep_section.iter_mut().enumerate() {
        let base = sh_off + i * sec_entry_size;
        let hdr = bytes
            .get(base..base + sec_entry_size)
            .ok_or_else(|| SymbolError::NotElf("truncated section header".to_owned()))?
            .to_vec();
        let sh_name = read_u32(&hdr, 0, endian)? as usize;
        let sh_type = read_u32(&hdr, 4, endian)?;
        let sh_link = read_u32(&hdr, 40, endian)? as usize;
        let name = read_cstr(shstr, sh_name);

        let keep = i == shstr_index || should_keep_section(name, sh_type);
        if keep {
            *keep_slot = true;
            if sh_link > 0 && sh_link < sec_count {
                kept_linked.push(sh_link);
            }
        }
        headers.push(hdr);
    }
    for link in kept_linked {
        if let Some(slot) = keep_section.get_mut(link) {
            *slot = true;
        }
    }

    let mut out = Vec::new();
    let elf_hdr = bytes
        .get(..elf_hdr_size)
        .ok_or_else(|| SymbolError::NotElf("truncated ELF header".to_owned()))?;
    out.extend_from_slice(elf_hdr);

    let written_ph_offset = if prog_count > 0 && ph_len > 0 {
        let aligned = align_up(out.len(), 8);
        if aligned.saturating_add(ph_len) >= bytes.len() {
            return Ok(bytes.to_vec());
        }
        out.resize(aligned, 0);
        let pos = out.len();
        let ph_slice = bytes
            .get(ph_off..ph_off + ph_len)
            .ok_or_else(|| SymbolError::NotElf("truncated program headers".to_owned()))?;
        out.extend_from_slice(ph_slice);
        pos as u64
    } else {
        0
    };

    for (i, hdr) in headers.iter_mut().enumerate() {
        let sh_type = read_u32(hdr, 4, endian)?;
        if sh_type == elf::SHT_NULL || sh_type == elf::SHT_NOBITS {
            continue;
        }
        let sec_off = usize::try_from(read_u64(hdr, 24, endian)?)
            .map_err(|_| SymbolError::NotElf("section offset".to_owned()))?;
        let sec_size = usize::try_from(read_u64(hdr, 32, endian)?)
            .map_err(|_| SymbolError::NotElf("section size".to_owned()))?;
        let sec_align = usize::try_from(read_u64(hdr, 48, endian)?)
            .unwrap_or(1)
            .clamp(1, 4096);

        if keep_section.get(i).copied().unwrap_or(false) {
            let sec_end = sec_off
                .checked_add(sec_size)
                .ok_or_else(|| SymbolError::NotElf("truncated section".to_owned()))?;
            let slice = bytes
                .get(sec_off..sec_end)
                .ok_or_else(|| SymbolError::NotElf("truncated section".to_owned()))?;
            let aligned = align_up(out.len(), sec_align);
            if aligned.saturating_add(slice.len()) >= bytes.len() {
                return Ok(bytes.to_vec());
            }
            out.resize(aligned, 0);
            let new_off = out.len() as u64;
            out.extend_from_slice(slice);
            write_u64(hdr, 24, new_off, endian);
        } else {
            write_u32(hdr, 4, elf::SHT_NOBITS, endian);
            write_u64(hdr, 24, 0, endian);
        }
    }

    let written_sh_offset = align_up(out.len(), 8);
    if written_sh_offset.saturating_add(sh_len) >= bytes.len() {
        return Ok(bytes.to_vec());
    }
    out.resize(written_sh_offset, 0);
    for hdr in &headers {
        out.extend_from_slice(hdr);
    }

    write_u64(&mut out, 32, written_ph_offset, endian);
    write_u64(&mut out, 40, written_sh_offset as u64, endian);

    if out.len() >= bytes.len() {
        return Ok(bytes.to_vec());
    }
    Ok(out)
}

fn should_keep_section(name: &str, sh_type: u32) -> bool {
    if matches!(
        sh_type,
        elf::SHT_NOTE | elf::SHT_SYMTAB | elf::SHT_DYNSYM | elf::SHT_SYMTAB_SHNDX
    ) {
        return true;
    }
    name.starts_with(".debug")
        || name.starts_with(".zdebug")
        || name.starts_with(".note")
        || matches!(
            name,
            ".symtab"
                | ".strtab"
                | ".dynsym"
                | ".dynstr"
                | ".shstrtab"
                | ".gopclntab"
                | ".gnu_debugdata"
                | ".gnu_debuglink"
                | ".gnu_debugaltlink"
        )
}

fn read_cstr(table: &[u8], offset: usize) -> &str {
    let Some(slice) = table.get(offset..) else {
        return "";
    };
    let end = slice.iter().position(|&b| b == 0).unwrap_or(slice.len());
    std::str::from_utf8(&slice[..end]).unwrap_or("")
}

const fn align_up(value: usize, align: usize) -> usize {
    if align <= 1 {
        return value;
    }
    let rem = value % align;
    if rem == 0 {
        value
    } else {
        value.saturating_add(align - rem)
    }
}

fn read_u16(bytes: &[u8], offset: usize, endian: Endianness) -> Result<u16, SymbolError> {
    let b = bytes
        .get(offset..offset + 2)
        .ok_or_else(|| SymbolError::NotElf("truncated u16".to_owned()))?;
    Ok(endian.read_u16([b[0], b[1]]))
}

fn read_u32(bytes: &[u8], offset: usize, endian: Endianness) -> Result<u32, SymbolError> {
    let b = bytes
        .get(offset..offset + 4)
        .ok_or_else(|| SymbolError::NotElf("truncated u32".to_owned()))?;
    Ok(endian.read_u32([b[0], b[1], b[2], b[3]]))
}

fn read_u64(bytes: &[u8], offset: usize, endian: Endianness) -> Result<u64, SymbolError> {
    let b = bytes
        .get(offset..offset + 8)
        .ok_or_else(|| SymbolError::NotElf("truncated u64".to_owned()))?;
    Ok(endian.read_u64([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]))
}

fn write_u32(bytes: &mut [u8], offset: usize, value: u32, endian: Endianness) {
    let raw = endian.write_u32(value);
    if let Some(dst) = bytes.get_mut(offset..offset + 4) {
        dst.copy_from_slice(&raw);
    }
}

fn write_u64(bytes: &mut [u8], offset: usize, value: u64, endian: Endianness) {
    let raw = endian.write_u64(value);
    if let Some(dst) = bytes.get_mut(offset..offset + 8) {
        dst.copy_from_slice(&raw);
    }
}
