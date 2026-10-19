//! The little of protocol buffers that pprof's schema uses: varints and
//! length-delimited fields, read without trusting a byte and written in
//! field order.

/// A field's value, as its wire type gives it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Value<'a> {
    Varint(u64),
    Bytes(&'a [u8]),
    Fixed64(u64),
    Fixed32(u32),
}

/// The fields of one message, in order.
pub struct Fields<'a> {
    bytes: &'a [u8],
}

impl<'a> Fields<'a> {
    pub const fn new(bytes: &'a [u8]) -> Self {
        Self { bytes }
    }
}

impl<'a> Iterator for Fields<'a> {
    type Item = Result<(u64, Value<'a>), String>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.bytes.is_empty() {
            return None;
        }

        Some(self.field())
    }
}

impl<'a> Fields<'a> {
    fn field(&mut self) -> Result<(u64, Value<'a>), String> {
        let key = varint(&mut self.bytes)?;
        let number = key >> 3;
        let value = match key & 7 {
            0 => Value::Varint(varint(&mut self.bytes)?),
            1 => Value::Fixed64(u64::from_le_bytes(
                take(&mut self.bytes, 8)?
                    .try_into()
                    .map_err(|_| "a fixed64 field is cut short".to_owned())?,
            )),
            2 => {
                let length = usize::try_from(varint(&mut self.bytes)?)
                    .map_err(|_| "a field is longer than memory".to_owned())?;
                Value::Bytes(take(&mut self.bytes, length)?)
            }
            5 => Value::Fixed32(u32::from_le_bytes(
                take(&mut self.bytes, 4)?
                    .try_into()
                    .map_err(|_| "a fixed32 field is cut short".to_owned())?,
            )),
            wire => {
                return Err(format!(
                    "field {number} has wire type {wire}, which pprof never uses"
                ));
            }
        };

        if number == 0 {
            return Err("a field is numbered 0".to_owned());
        }

        Ok((number, value))
    }
}

fn take<'a>(bytes: &mut &'a [u8], length: usize) -> Result<&'a [u8], String> {
    if bytes.len() < length {
        return Err(format!(
            "a field of {length} bytes has only {} left",
            bytes.len()
        ));
    }

    let (taken, rest) = bytes.split_at(length);
    *bytes = rest;
    Ok(taken)
}

fn varint(bytes: &mut &[u8]) -> Result<u64, String> {
    let mut value = 0_u64;

    for shift in (0..64).step_by(7) {
        let [byte, rest @ ..] = *bytes else {
            return Err("a varint is cut short".to_owned());
        };
        *bytes = rest;
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Ok(value);
        }
    }

    Err("a varint is longer than ten bytes".to_owned())
}

impl<'a> Value<'a> {
    /// The value as a number.
    pub fn number(self, field: &str) -> Result<u64, String> {
        match self {
            Self::Varint(value) | Self::Fixed64(value) => Ok(value),
            Self::Fixed32(value) => Ok(u64::from(value)),
            Self::Bytes(_) => Err(format!("{field} is not a number")),
        }
    }

    /// The value as bytes: a message, or a string.
    pub fn bytes(self, field: &str) -> Result<&'a [u8], String> {
        match self {
            Self::Bytes(bytes) => Ok(bytes),
            _ => Err(format!("{field} is not a message")),
        }
    }

    /// Appends the numbers of a repeated field, packed or not.
    pub fn numbers(self, into: &mut Vec<u64>, field: &str) -> Result<(), String> {
        match self {
            Self::Bytes(mut bytes) => {
                while !bytes.is_empty() {
                    into.push(varint(&mut bytes)?);
                }
            }
            value => into.push(value.number(field)?),
        }

        Ok(())
    }
}

/// A message being written.
#[derive(Debug, Default)]
pub struct Message {
    pub bytes: Vec<u8>,
}

impl Message {
    pub fn number(&mut self, field: u64, value: u64) {
        if value != 0 {
            put_varint(&mut self.bytes, field << 3);
            put_varint(&mut self.bytes, value);
        }
    }

    /// A signed number, as pprof's `int64` fields hold it.
    pub fn signed(&mut self, field: u64, value: i64) {
        self.number(field, value.cast_unsigned());
    }

    pub fn bytes(&mut self, field: u64, value: &[u8]) {
        put_varint(&mut self.bytes, (field << 3) | 2);
        put_varint(&mut self.bytes, value.len() as u64);
        self.bytes.extend_from_slice(value);
    }

    pub fn message(&mut self, field: u64, write: impl FnOnce(&mut Self)) {
        let mut inner = Self::default();
        write(&mut inner);
        self.bytes(field, &inner.bytes);
    }

    /// A repeated number field, packed.
    pub fn packed(&mut self, field: u64, values: impl IntoIterator<Item = u64>) {
        let mut packed = Vec::new();

        for value in values {
            put_varint(&mut packed, value);
        }

        if !packed.is_empty() {
            self.bytes(field, &packed);
        }
    }
}

#[expect(
    clippy::cast_possible_truncation,
    reason = "a varint's bytes hold seven bits each"
)]
fn put_varint(bytes: &mut Vec<u8>, mut value: u64) {
    while value >= 0x80 {
        bytes.push((value as u8) | 0x80);
        value >>= 7;
    }

    bytes.push(value as u8);
}
