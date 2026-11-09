//! Section bytes, decompressed only once the size a compressed section's
//! header claims passes our limits. The header is input: decompressing to
//! whatever it claims lets a few hostile bytes take gigabytes, and an
//! allocation that fails aborts the process.

use std::borrow::Cow;
use std::io::Read as _;

use object::{CompressedData, CompressionFormat, ObjectSection as _};
use rayon::prelude::*;

/// Claims up to this many bytes pass whatever their ratio. Small sections of
/// real programs compress up to 66 times (a Rust `.debug_ranges` of 78 KB in
/// 1.2 KB), and holding one costs little.
const SMALL_CLAIM: u64 = 1 << 20;

/// How many times its compressed bytes a larger section may claim. With
/// zlib, no section over 1 MiB in the test programs or the benchmark corpus
/// compresses more than 14 times.
const MOST_RATIO: u64 = 64;

/// The most bytes one file's compressed sections may claim to decompress
/// to by default. The largest file we test, microscope's own debug build,
/// has 58 MiB; this leaves room for programs hundreds of times larger, and
/// with [`MOST_RATIO`] reaching it takes 256 MiB of compressed input.
pub(super) const MOST_PER_FILE: u64 = 16 << 30;

#[derive(Debug, thiserror::Error)]
pub(super) enum SectionError {
    #[error("malformed ELF file: {0}")]
    Object(#[from] object::Error),

    #[error(
        "its {section} section claims to decompress to {claimed} bytes from {compressed}, more \
         than {MOST_RATIO} times as many"
    )]
    Ratio {
        section: String,
        claimed: u64,
        compressed: u64,
    },

    #[error(
        "its compressed sections claim to decompress to {claimed} bytes, more than the {limit} \
         one file may"
    )]
    Total { claimed: u64, limit: u64 },

    #[error("there is no memory for the {claimed} bytes its {section} section decompresses to")]
    NoMemory { section: String, claimed: u64 },

    #[error("its {section} section is not the {format} data of {claimed} bytes its header claims")]
    Data {
        section: String,
        format: &'static str,
        claimed: u64,
    },

    #[error("its {section} section is compressed in a format other than zlib or zstd")]
    Format { section: String },
}

/// The bytes of each of `sections`, empty for one that is missing. The
/// compressed ones are decompressed in parallel, and only when each claims
/// no more than [`MOST_RATIO`] times its compressed bytes and together they
/// claim no more than `most` bytes.
pub(super) fn read<'data>(
    sections: Vec<Option<object::Section<'data, '_>>>,
    most: u64,
) -> Result<Vec<Cow<'data, [u8]>>, SectionError> {
    let mut claimed = 0_u64;
    let mut claims = Vec::with_capacity(sections.len());
    for section in sections {
        let Some(section) = section else {
            claims.push(None);
            continue;
        };

        let data = section.compressed_data()?;
        let name = if data.format == CompressionFormat::None {
            String::new()
        } else {
            section.name().unwrap_or("unnamed").to_owned()
        };

        let compressed = data.data.len() as u64;
        if data.format != CompressionFormat::None
            && data.uncompressed_size > SMALL_CLAIM
            && data.uncompressed_size > compressed.saturating_mul(MOST_RATIO)
        {
            return Err(SectionError::Ratio {
                section: name,
                claimed: data.uncompressed_size,
                compressed,
            });
        }

        if data.format != CompressionFormat::None {
            claimed = claimed.saturating_add(data.uncompressed_size);
        }
        claims.push(Some((name, data)));
    }

    if claimed > most {
        return Err(SectionError::Total {
            claimed,
            limit: most,
        });
    }

    let read = claims
        .into_par_iter()
        .map(|claim| match claim {
            Some((name, data)) => decompress(&name, data),
            None => Ok(Cow::Borrowed(&[][..])),
        })
        .collect::<Vec<_>>();

    // In order, so the error reported is the first section's.
    read.into_iter().collect()
}

/// The bytes of `section`, empty when it is missing, as [`read`] reads them.
pub(super) fn read_one<'data>(
    section: Option<object::Section<'data, '_>>,
    most: u64,
) -> Result<Cow<'data, [u8]>, SectionError> {
    Ok(read(vec![section], most)?.pop().unwrap_or_default())
}

/// `data`'s bytes, which [`read`] has checked the claimed size of.
fn decompress<'data>(
    name: &str,
    data: CompressedData<'data>,
) -> Result<Cow<'data, [u8]>, SectionError> {
    let claimed = data.uncompressed_size;
    let format = match data.format {
        CompressionFormat::None => return Ok(Cow::Borrowed(data.data)),
        CompressionFormat::Zlib => "zlib",
        CompressionFormat::Zstandard => "zstd",
        _ => {
            return Err(SectionError::Format {
                section: name.to_owned(),
            });
        }
    };
    let wrong = || SectionError::Data {
        section: name.to_owned(),
        format,
        claimed,
    };
    let size = usize::try_from(claimed).map_err(|_| wrong())?;
    let mut decompressed = Vec::new();
    decompressed
        .try_reserve_exact(size)
        .map_err(|_| SectionError::NoMemory {
            section: name.to_owned(),
            claimed,
        })?;

    // zlib decompresses into what the claim reserved and no further.
    if data.format == CompressionFormat::Zlib {
        let mut zlib = flate2::Decompress::new(true);
        let status = zlib
            .decompress_vec(
                data.data,
                &mut decompressed,
                flate2::FlushDecompress::Finish,
            )
            .map_err(|_| wrong())?;
        if status != flate2::Status::StreamEnd
            || zlib.total_in() != data.data.len() as u64
            || decompressed.len() != size
        {
            return Err(wrong());
        }
        return Ok(Cow::Owned(decompressed));
    }

    // A zstd stream that claims little can still grow without bound, so
    // read no more than the claim and one byte past it. A section may hold
    // several frames, and skippable frames among them.
    let mut input = data.data;
    while !input.is_empty() {
        let decoder = match ruzstd::decoding::StreamingDecoder::new(&mut input) {
            Ok(decoder) => decoder,
            Err(ruzstd::decoding::errors::FrameDecoderError::ReadFrameHeaderError(
                ruzstd::decoding::errors::ReadFrameHeaderError::SkipFrame { length, .. },
            )) => {
                input = usize::try_from(length)
                    .ok()
                    .and_then(|length| input.get(length..))
                    .ok_or_else(wrong)?;
                continue;
            }
            Err(_) => return Err(wrong()),
        };

        let left = claimed - decompressed.len() as u64;
        decoder
            .take(left + 1)
            .read_to_end(&mut decompressed)
            .map_err(|_| wrong())?;
        if decompressed.len() > size {
            return Err(wrong());
        }
    }

    if decompressed.len() != size {
        return Err(wrong());
    }

    Ok(Cow::Owned(decompressed))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn zstd(data: &[u8]) -> Vec<u8> {
        ruzstd::encoding::compress_to_vec(data, ruzstd::encoding::CompressionLevel::Fastest)
    }

    fn claiming(data: &[u8], uncompressed_size: u64) -> CompressedData<'_> {
        CompressedData {
            format: CompressionFormat::Zstandard,
            data,
            uncompressed_size,
        }
    }

    /// A zstd frame of `blocks` RLE blocks, each 128 KiB of one byte: four
    /// bytes of input for each block, as a bomb is built.
    fn rle_frame(blocks: usize) -> Vec<u8> {
        // Magic, then a descriptor with no content size and a 128 KiB window.
        let mut frame = vec![0x28, 0xb5, 0x2f, 0xfd, 0x00, 0x38];
        for index in 0..blocks {
            let last = u32::from(index + 1 == blocks);
            let header = last | (1 << 1) | ((128 << 10) << 3);
            frame.extend_from_slice(&header.to_le_bytes()[..3]);
            frame.push(0xaa);
        }
        frame
    }

    /// A zstd frame of one compressed block holding `sequences` sequences,
    /// each a literal byte and a 65,539-byte match of the byte before: three
    /// bytes of input for each 64 KiB of output (RUSTSEC-2026-0344).
    fn sequence_bomb(sequences: u16) -> Vec<u8> {
        assert!((128..0x7f00).contains(&sequences));
        let literals = usize::from(sequences);

        // Raw literals with a 20-bit size.
        let mut block = vec![
            (0b11 << 2) | u8::try_from(literals & 0xf).expect("four bits"),
            u8::try_from((literals >> 4) & 0xff).expect("a byte"),
            u8::try_from(literals >> 12).expect("a byte"),
        ];
        block.resize(block.len() + literals, b'a');

        // The count, then every code RLE: literal length 1, repeat offset
        // 1, and match length code 52, whose 16 extra bits are all zero. The
        // bitstream ends in its padding marker.
        let [high, low] = sequences.to_be_bytes();
        block.extend_from_slice(&[high | 0x80, low, 0x54, 1, 0, 52]);
        block.resize(block.len() + 2 * literals, 0);
        block.push(1);

        // Magic, then a descriptor with no content size and an 8 MiB
        // window, then the block's header: last, compressed, its size.
        let mut frame = vec![0x28, 0xb5, 0x2f, 0xfd, 0x00, 0x68];
        let header = 1 | (2 << 1) | (u32::try_from(block.len()).expect("a block") << 3);
        frame.extend_from_slice(&header.to_le_bytes()[..3]);
        frame.extend_from_slice(&block);
        frame
    }

    /// A zlib stream decompresses only to exactly its claim, and only when
    /// it ends where the section does.
    #[test]
    fn zlib_is_read_to_its_claim() {
        use std::io::Write as _;

        let text = b"the quick brown fox jumps over the lazy dog ".repeat(100);
        let mut encoder =
            flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(&text).expect("compressed");
        let stream = encoder.finish().expect("compressed");
        let zlib = |data, claim: usize| {
            decompress(
                "s",
                CompressedData {
                    format: CompressionFormat::Zlib,
                    data,
                    uncompressed_size: claim as u64,
                },
            )
        };

        assert_eq!(zlib(&stream, text.len()).expect("the stream"), text);
        for claim in [text.len() - 1, text.len() + 1] {
            assert!(matches!(
                zlib(&stream, claim),
                Err(SectionError::Data { .. })
            ));
        }

        let trailing = [&stream[..], &[0]].concat();
        let cut = &stream[..stream.len() - 1];
        for data in [&trailing[..], cut] {
            assert!(matches!(
                zlib(data, text.len()),
                Err(SectionError::Data { .. })
            ));
        }
    }

    /// A stream is read only to its claim: one longer fails having held the
    /// claim and a block past it, not all of its bytes, and one shorter
    /// fails too. Frames are read in turn and skippable frames skipped.
    #[test]
    fn zstd_is_read_to_its_claim_and_no_further() {
        use crate::test_memory::memory_cap::{start_totals, stop_totals, totals};

        let text = b"the quick brown fox jumps over the lazy dog ".repeat(100);
        let mut frames = zstd(&text);
        // A skippable frame of four bytes, then the stream again.
        frames.extend_from_slice(&[0x50, 0x2a, 0x4d, 0x18, 4, 0, 0, 0, 1, 2, 3, 4]);
        frames.extend_from_slice(&zstd(&text));
        let both = [&text[..], &text[..]].concat();
        assert_eq!(
            decompress("s", claiming(&frames, both.len() as u64)).expect("two frames"),
            both
        );

        for claim in [both.len() - 1, both.len() + 1] {
            assert!(matches!(
                decompress("s", claiming(&frames, claim as u64)),
                Err(SectionError::Data { .. })
            ));
        }

        // 64 MiB from 2 or 3 KiB, claiming 1 KiB: in blocks of 128 KiB, and in
        // one block that claims more than a block may hold.
        for bomb in [rle_frame(512), sequence_bomb(1024)] {
            start_totals();
            let before = totals();
            let result = decompress(".debug_info", claiming(&bomb, 1024));
            let used = totals().since(&before).allocated.bytes;
            stop_totals();
            assert!(
                matches!(result, Err(SectionError::Data { .. })),
                "{result:?}"
            );
            assert!(used < 4 << 20, "{used} bytes");
        }
    }
}
