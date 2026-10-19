//! Memory and disassembly requests.

use microscope::{
    DisassemblyQuery, DisassemblyRange, InstructionContent, MAX_WINDOW_AFTER, MAX_WINDOW_BEFORE,
    MemoryReadCompletion, StopContext, VirtualAddress,
};
use serde_json::{Map, Value, json};

use super::protocol::{
    self, DisassembleArguments, ErrorBody, ReadMemoryArguments, WriteMemoryArguments,
};

use super::session::{Session, error, parse};

/// The most bytes one `readMemory` request returns.
const MAX_READ: u64 = 1024 * 1024;

/// The most bytes the debugger reads at once.
const READ_CHUNK: u64 = 64 * 1024;

/// The most instructions one `disassemble` request returns.
const MAX_INSTRUCTIONS: i64 = 16 * 1024;

const PAGE_SIZE: u64 = 4096;

impl Session {
    pub(super) async fn read_memory(&self, arguments: Value) -> Result<Value, ErrorBody> {
        let arguments = parse::<ReadMemoryArguments>(arguments, "readMemory arguments")?;
        self.current_stop()?;
        let address = reference(&arguments.memory_reference, arguments.offset)?;
        let requested = u64::try_from(arguments.count)
            .map_err(|_| ErrorBody::new("the byte count must not be negative"))?
            .min(MAX_READ);
        let handle = self.target_handle()?;
        let mut bytes = Vec::new();
        let mut unreadable = 0;

        while (bytes.len() as u64) < requested {
            let next = address.saturating_add(bytes.len() as u64);
            let count = (requested - bytes.len() as u64).min(READ_CHUNK);
            let read = handle
                .read_memory(VirtualAddress::new(next), count)
                .await
                .map_err(error)?;
            bytes.extend_from_slice(&read.bytes);
            if let MemoryReadCompletion::Incomplete { next_address, .. } = read.completion {
                // The rest of the page is unreadable too; past it, nothing
                // is known.
                let page_end = (next_address.get() / PAGE_SIZE + 1) * PAGE_SIZE;
                unreadable = (page_end - next_address.get()).min(requested - bytes.len() as u64);
                break;
            }
        }

        let mut body = json!({"address": format!("{address:#x}"), "data": base64(&bytes)});
        if unreadable != 0 {
            body["unreadableBytes"] = unreadable.into();
        }

        Ok(body)
    }

    pub(super) async fn write_memory(&mut self, arguments: Value) -> Result<Value, ErrorBody> {
        let arguments = parse::<WriteMemoryArguments>(arguments, "writeMemory arguments")?;
        self.current_stop()?;
        let address = reference(&arguments.memory_reference, arguments.offset)?;
        let bytes =
            unbase64(&arguments.data).ok_or_else(|| ErrorBody::new("the data is not base64"))?;
        let handle = self.target_handle()?;
        let written = handle
            .write_memory(VirtualAddress::new(address), &bytes)
            .await
            .map_err(error)?;
        if written != bytes.len() as u64 && arguments.allow_partial != Some(true) {
            return Err(ErrorBody::new(format!(
                "only {written} of {} bytes could be written at {address:#x}",
                bytes.len()
            )));
        }

        self.forget_reads();

        if self.support().memory_events {
            self.client
                .event(
                    "memory",
                    json!({"memoryReference": format!("{address:#x}"), "offset": 0, "count": written}),
                )
                .await?;
        }

        self.invalidate_values().await;
        Ok(json!({"offset": 0, "bytesWritten": written}))
    }

    pub(super) async fn disassemble(&mut self, arguments: Value) -> Result<Value, ErrorBody> {
        let arguments = parse::<DisassembleArguments>(arguments, "disassemble arguments")?;
        let stop = self.current_stop()?;
        let anchor = reference(&arguments.memory_reference, arguments.offset)?;
        if !(0..=MAX_INSTRUCTIONS).contains(&arguments.instruction_count) {
            return Err(ErrorBody::new(format!(
                "the instruction count must be between 0 and {MAX_INSTRUCTIONS}"
            )));
        }

        if arguments.instruction_count == 0 {
            return Ok(json!({"instructions": []}));
        }

        let first = arguments.instruction_offset.unwrap_or(0);
        let plan = Plan::new(first, arguments.instruction_count);
        let context = stop.innermost();
        let handle = self.target_handle()?;
        let instructions = self.decode(context, anchor, plan).await?;
        let modules = handle.loaded_modules().await.map_err(error)?;
        let program_counter = self.program_counter(context).await;
        let mut rows = Vec::with_capacity(plan.count);
        let mut previous_file = None;

        for row in plan.rows(&instructions) {
            let instruction = match row {
                Row::Padding(address) => {
                    rows.push(json!({
                        "address": format!("{address:#x}"),
                        "instruction": "??",
                        "presentationHint": "invalid",
                    }));
                    previous_file = None;
                    continue;
                }
                Row::Instruction(instruction) => instruction,
            };

            let row = self
                .instruction_row(instruction, program_counter, &modules, &mut previous_file)
                .await;
            rows.push(row);
        }

        Ok(json!({"instructions": rows}))
    }

    /// One decoded instruction as a row, naming its source when the source
    /// file differs from the previous row's.
    async fn instruction_row(
        &mut self,
        instruction: &microscope::DisassembledInstruction,
        program_counter: Option<VirtualAddress>,
        modules: &microscope::LoadedModuleSnapshot,
        previous_file: &mut Option<(microscope::ModuleId, microscope::SourceFileId)>,
    ) -> Value {
        let module = instruction
            .location
            .module
            .as_ref()
            .map(|module| module.module);
        let mut body = Map::new();
        body.insert(
            "address".to_owned(),
            format!("{:#x}", instruction.address.get()).into(),
        );
        body.insert(
            "instructionBytes".to_owned(),
            instruction
                .bytes
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<Vec<_>>()
                .join(" ")
                .into(),
        );
        let text = match &instruction.content {
            InstructionContent::Decoded(decoded) => crate::cli::format::instruction_text(
                decoded,
                Some(instruction.address) == program_counter,
                module,
                modules,
                crate::cli::terminal::Renderer::new(false),
            ),
            InstructionContent::Invalid => "(bad)".to_owned(),
            InstructionContent::Truncated => "(truncated)".to_owned(),
        };

        if !matches!(instruction.content, InstructionContent::Decoded(_)) {
            body.insert("presentationHint".to_owned(), "invalid".into());
        }

        body.insert("instruction".to_owned(), text.into());

        if let Some(symbol) = instruction
            .location
            .module
            .as_ref()
            .and_then(|module| module.image.symbol.as_ref())
        {
            let name = symbol
                .demangled_name()
                .unwrap_or_else(|| symbol.name.to_string());
            body.insert("symbol".to_owned(), name.into());
        }

        if let (Some(source), Some(module)) = (&instruction.source, module)
            && let Some(image) = self.image(module).await
            && let Some(file) = image.source_file(source.file)
        {
            // The source is given once for each run of one file.
            if *previous_file != Some((module, source.file)) {
                body.insert(
                    "location".to_owned(),
                    super::sources::source_json(&self.local_path(&file.path)),
                );
                *previous_file = Some((module, source.file));
            }

            body.insert(
                "line".to_owned(),
                self.line_to_client(source.line.get()).into(),
            );
            if let Some(column) = source.column {
                body.insert(
                    "column".to_owned(),
                    self.column_to_client(column.get()).into(),
                );
            }
        } else {
            *previous_file = None;
        }

        Value::Object(body)
    }

    /// Decodes the instructions a plan needs around `anchor`, in windows no
    /// larger than the debugger decodes at once.
    async fn decode(
        &self,
        context: StopContext,
        anchor: u64,
        plan: Plan,
    ) -> Result<Decoded, ErrorBody> {
        let handle = self.target_handle()?;
        let view = handle.at(context);
        let window = |address: u64, before: u32, after: u32| DisassemblyQuery {
            range: DisassemblyRange::Window {
                address: VirtualAddress::new(address),
                before,
                after,
            },
            syntax: self.syntax(),
        };

        let mut decoded = Decoded {
            address: anchor,
            instructions: Vec::new(),
            anchor: 0,
        };

        let disassembly = view
            .disassemble(window(
                anchor,
                window_size(plan.before, MAX_WINDOW_BEFORE),
                window_size(plan.after, MAX_WINDOW_AFTER),
            ))
            .await
            .map_err(error)?;
        let microscope::DisassemblyView::Window { block, .. } = disassembly.view else {
            return Err(ErrorBody::new(
                "internal: a window query returned no window",
            ));
        };

        decoded.anchor = block
            .instructions
            .iter()
            .position(|instruction| instruction.address.get() >= anchor)
            .unwrap_or(block.instructions.len());
        decoded.instructions = block.instructions.to_vec();

        // Later windows continue after the end, or before the beginning.
        while decoded.instructions.len() - decoded.anchor < plan.after {
            let Some(last) = decoded.instructions.last() else {
                break;
            };

            let wanted = plan.after - (decoded.instructions.len() - decoded.anchor);
            let more = view
                .disassemble(window(
                    last.end().get(),
                    0,
                    window_size(wanted, MAX_WINDOW_AFTER),
                ))
                .await
                .map_err(error)?;
            let microscope::DisassemblyView::Window { block, .. } = more.view else {
                return Err(ErrorBody::new(
                    "internal: a window query returned no window",
                ));
            };

            if block.instructions.is_empty() {
                break;
            }

            decoded
                .instructions
                .extend(block.instructions.iter().cloned());
        }

        while decoded.anchor < plan.before {
            let Some(first) = decoded.instructions.first() else {
                break;
            };

            let wanted = window_size(plan.before - decoded.anchor, MAX_WINDOW_BEFORE);
            let more = view
                .disassemble(window(first.address.get(), wanted, 0))
                .await
                .map_err(error)?;
            let microscope::DisassemblyView::Window { block, .. } = more.view else {
                return Err(ErrorBody::new(
                    "internal: a window query returned no window",
                ));
            };

            let earlier = block
                .instructions
                .iter()
                .take_while(|instruction| instruction.address < first.address)
                .cloned()
                .collect::<Vec<_>>();
            if earlier.is_empty() {
                break;
            }

            decoded.anchor += earlier.len();
            decoded.instructions.splice(0..0, earlier);
        }

        Ok(decoded)
    }

    async fn program_counter(&mut self, context: StopContext) -> Option<VirtualAddress> {
        let stop = self.current_stop().ok()?;
        let trace = self.backtrace(&stop, context.execution).await.ok()?;
        trace.frames.first().and_then(|frame| frame.instruction)
    }
}

/// Instructions decoded around an anchor address.
struct Decoded {
    /// The anchor address.
    address: u64,
    instructions: Vec<microscope::DisassembledInstruction>,

    /// The index of the first instruction at or after the anchor.
    anchor: usize,
}

/// Which rows a `disassemble` request asks for, relative to its anchor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Plan {
    /// The first row's offset from the anchor in instructions.
    first: i64,
    count: usize,

    /// Instructions needed before the anchor and from it onward.
    before: usize,
    after: usize,
}

enum Row<'a> {
    Instruction(&'a microscope::DisassembledInstruction),

    /// No proven instruction is known here; the address keeps rows in
    /// order, one byte beyond the nearest known edge.
    Padding(u64),
}

impl Plan {
    fn new(first: i64, count: i64) -> Self {
        let end = first.saturating_add(count);
        Self {
            first,
            count: usize::try_from(count).unwrap_or(0),
            before: usize::try_from(first.min(0).unsigned_abs()).unwrap_or(usize::MAX),
            after: usize::try_from(end.max(0)).unwrap_or(usize::MAX),
        }
    }

    /// Exactly `count` rows: decoded instructions where there are some, and
    /// padding before and after them.
    fn rows<'a>(&self, decoded: &'a Decoded) -> impl Iterator<Item = Row<'a>> + 'a {
        let instructions = &decoded.instructions;
        let anchor = i64::try_from(decoded.anchor).unwrap_or(i64::MAX);
        let length = i64::try_from(instructions.len()).unwrap_or(i64::MAX);
        let first = self.first;
        (0..self.count).map(move |row| {
            let index = anchor + first + i64::try_from(row).unwrap_or(i64::MAX);
            if let Some(instruction) = usize::try_from(index)
                .ok()
                .and_then(|index| instructions.get(index))
            {
                return Row::Instruction(instruction);
            }

            Row::Padding(padding_address(
                instructions,
                index,
                length,
                decoded.address,
            ))
        })
    }
}

/// `count` as a window size, held to `max`: a larger count gets the most a
/// window holds.
fn window_size(count: usize, max: u32) -> u32 {
    u32::try_from(count).unwrap_or(u32::MAX).min(max)
}

fn padding_address(
    instructions: &[microscope::DisassembledInstruction],
    index: i64,
    length: i64,
    anchor: u64,
) -> u64 {
    match (instructions.first(), instructions.last()) {
        (Some(first), _) if index < 0 => first.address.get().saturating_sub(index.unsigned_abs()),
        (_, Some(last)) => last
            .end()
            .get()
            .saturating_add(u64::try_from(index - length).unwrap_or(0)),
        _ => anchor.saturating_add_signed(index),
    }
}

/// Reads a memory reference with a byte offset.
fn reference(reference: &str, offset: Option<i64>) -> Result<u64, ErrorBody> {
    protocol::address(reference)
        .and_then(|address| protocol::offset(address, offset))
        .ok_or_else(|| {
            ErrorBody::new(format!(
                "invalid memory reference '{reference}' with offset {}",
                offset.unwrap_or(0)
            ))
        })
}

/// Decodes standard base64, with or without padding.
fn unbase64(text: &str) -> Option<Vec<u8>> {
    let value = |byte: u8| match byte {
        b'A'..=b'Z' => Some(byte - b'A'),
        b'a'..=b'z' => Some(byte - b'a' + 26),
        b'0'..=b'9' => Some(byte - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    };

    let digits = text.trim().trim_end_matches('=').as_bytes();
    let mut bytes = Vec::with_capacity(digits.len() * 3 / 4);

    for chunk in digits.chunks(4) {
        if chunk.len() == 1 {
            return None;
        }

        let word = chunk
            .iter()
            .enumerate()
            .try_fold(0_u32, |word, (index, digit)| {
                Some(word | u32::from(value(*digit)?) << (18 - 6 * index))
            })?;
        let decoded = word.to_be_bytes();
        bytes.extend_from_slice(&decoded[1..chunk.len()]);
    }

    Some(bytes)
}

/// Encodes bytes as standard base64 with padding.
fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut text = String::with_capacity(bytes.len().div_ceil(3) * 4);

    for chunk in bytes.chunks(3) {
        let word = chunk.iter().enumerate().fold(0_u32, |word, (index, byte)| {
            word | u32::from(*byte) << (16 - 8 * index)
        });

        for position in 0..4 {
            if position <= chunk.len() {
                text.push(char::from(
                    ALPHABET[(word >> (18 - 6 * position) & 63) as usize],
                ));
            } else {
                text.push('=');
            }
        }
    }

    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_the_standard_encoding_and_rejects_malformed_text() {
        for (bytes, text) in [
            (&b""[..], ""),
            (b"f", "Zg=="),
            (b"fo", "Zm8="),
            (b"foo", "Zm9v"),
            (b"foob", "Zm9vYg=="),
            (b"\xff\x00\xfe", "/wD+"),
        ] {
            assert_eq!(base64(bytes), text);
            assert_eq!(unbase64(text).as_deref(), Some(bytes));
        }

        // Clients may leave out the padding.
        assert_eq!(unbase64("Zg"), Some(b"f".to_vec()));
        assert_eq!(unbase64("Z"), None);
        assert_eq!(unbase64("Zm9v!"), None);
    }
}
