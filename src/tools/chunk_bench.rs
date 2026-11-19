use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use serde::Serialize;

use crate::capture::Capture;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, clap::ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum Layout {
    Rows,
    Columns,
}

#[derive(Debug, Clone, Serialize)]
pub struct Row {
    pub file: String,
    pub layout: Layout,
    pub level: i32,
    pub section: usize,
    pub span_ns: i64,
    pub raw: usize,
    pub sections: usize,
    pub compressed: usize,
    pub encode_ns: u64,
    pub decode_ns: u64,
}

impl Row {
    #[must_use]
    pub fn share(&self) -> f64 {
        ratio(self.compressed, self.raw)
    }

    #[must_use]
    pub fn per_day(&self) -> Option<f64> {
        (self.span_ns > 0).then(|| per(self.compressed, 86_400.0e9, self.span_ns))
    }

    #[must_use]
    pub fn encode_mb_per_second(&self) -> f64 {
        per(
            self.raw,
            1e3,
            i64::try_from(self.encode_ns).unwrap_or(i64::MAX),
        )
    }

    #[must_use]
    pub fn decode_mb_per_second(&self) -> f64 {
        per(
            self.raw,
            1e3,
            i64::try_from(self.decode_ns).unwrap_or(i64::MAX),
        )
    }
}

#[expect(clippy::cast_precision_loss, reason = "a report of sizes")]
fn ratio(part: usize, whole: usize) -> f64 {
    part as f64 / whole.max(1) as f64
}

#[expect(clippy::cast_precision_loss, reason = "a report of sizes and times")]
fn per(bytes: usize, scale: f64, ns: i64) -> f64 {
    bytes as f64 * scale / ns.max(1) as f64
}

struct Table<'a> {
    stride: usize,
    bytes: &'a [u8],
}

fn tables(capture: &Capture) -> Vec<Table<'_>> {
    capture
        .tables()
        .into_iter()
        .filter(|(_, _, _, bytes)| !bytes.is_empty())
        .map(|(_, stride, _, bytes)| Table {
            stride: stride.max(1),
            bytes,
        })
        .collect()
}

#[must_use]
pub fn shuffle(rows: &[u8], stride: usize) -> Vec<u8> {
    assert!(stride > 0 && rows.len().is_multiple_of(stride));
    (0..stride)
        .flat_map(|byte| {
            rows.chunks_exact(stride)
                .filter_map(move |row| row.get(byte).copied())
        })
        .collect()
}

#[must_use]
pub fn unshuffle(columns: &[u8], stride: usize) -> Vec<u8> {
    assert!(stride > 0 && columns.len().is_multiple_of(stride));
    if columns.is_empty() {
        return Vec::new();
    }

    let count = columns.len() / stride;
    let columns: Vec<&[u8]> = columns.chunks_exact(count).collect();
    (0..count)
        .flat_map(|row| {
            columns
                .iter()
                .filter_map(move |column| column.get(row).copied())
        })
        .collect()
}

fn sections<'a>(table: &Table<'a>, section: usize) -> impl Iterator<Item = &'a [u8]> {
    let rows = (section / table.stride).max(1);
    table.bytes.chunks(rows * table.stride)
}

fn encode(raw: &[u8], stride: usize, layout: Layout, level: i32) -> Result<Vec<u8>> {
    let laid = match layout {
        Layout::Rows => std::borrow::Cow::Borrowed(raw),
        Layout::Columns => std::borrow::Cow::Owned(shuffle(raw, stride)),
    };

    Ok(zstd::bulk::compress(&laid, level)?)
}

fn decode(encoded: &[u8], length: usize, stride: usize, layout: Layout) -> Result<Vec<u8>> {
    let laid = zstd::bulk::decompress(encoded, length)?;
    ensure!(
        laid.len() == length,
        "a section decoded to {} bytes, not {length}",
        laid.len()
    );
    Ok(match layout {
        Layout::Rows => laid,
        Layout::Columns => unshuffle(&laid, stride),
    })
}

pub fn measure(
    path: &Path,
    layouts: &[Layout],
    levels: &[i32],
    sizes: &[usize],
) -> Result<Vec<Row>> {
    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let read = crate::capture::file::read(&bytes)
        .with_context(|| format!("reading {}", path.display()))?;
    let capture = read.capture;
    let (start, end) = capture.span();
    let tables = tables(&capture);

    let mut rows = Vec::new();
    for &layout in layouts {
        for &level in levels {
            for &section in sizes {
                let mut row = Row {
                    file: path.display().to_string(),
                    layout,
                    level,
                    section,
                    span_ns: end - start,
                    raw: 0,
                    sections: 0,
                    compressed: 0,
                    encode_ns: 0,
                    decode_ns: 0,
                };

                for table in &tables {
                    for raw in sections(table, section) {
                        let began = Instant::now();
                        let encoded = encode(raw, table.stride, layout, level)?;
                        let encoded_at = Instant::now();
                        let decoded = decode(&encoded, raw.len(), table.stride, layout)?;
                        let decoded_at = Instant::now();
                        if decoded != raw {
                            bail!("{}: a section did not decode to its input", path.display());
                        }

                        row.raw += raw.len();
                        row.sections += 1;
                        row.compressed += encoded.len();
                        row.encode_ns += nanoseconds(encoded_at - began);
                        row.decode_ns += nanoseconds(decoded_at - encoded_at);
                    }
                }

                rows.push(row);
            }
        }
    }

    Ok(rows)
}

fn nanoseconds(elapsed: Duration) -> u64 {
    u64::try_from(elapsed.as_nanos()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    proptest! {
        #[test]
        fn columns_put_each_byte_of_a_row_in_its_own_column_and_back(
            stride in 1usize..40,
            rows in prop::collection::vec(any::<u8>(), 0..400),
        ) {
            let rows = &rows[..rows.len() / stride * stride];
            let columns = shuffle(rows, stride);
            let count = rows.len() / stride;
            for (index, &value) in rows.iter().enumerate() {
                prop_assert_eq!(columns[(index % stride) * count + index / stride], value);
            }

            prop_assert_eq!(unshuffle(&columns, stride), rows);
        }
    }
}
