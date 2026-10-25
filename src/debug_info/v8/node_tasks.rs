//! Where Node keeps its event loop's work, which no `v8dbg_` constant and
//! only some `nodedbg_` variables say: decoded from small functions Node
//! exports, each checked against the instruction shape it must have, or
//! refused by name.
//!
//! - `v8::Isolate::GetCurrentContext` loads the isolate's current context
//!   first: `mov r, [rdi + X]; test r, r`.
//! - `node::timers::BindingData::SetupTimers` reads the current context's
//!   embedder data, compares its context-tag slot with
//!   `ContextEmbedderTag::kNodeContextTagPtr`, loads the `Environment`
//!   from another slot, and passes `[env + realm]`, its principal realm, to
//!   the realm's setters (`src/timers.cc`).
//! - `node::PrincipalRealm::set_timers_callback_function` disposes and
//!   stores the realm's global handle at one offset (`env_properties.h`'s
//!   `PER_REALM_STRONG_PERSISTENT_VALUES`).
//! - `node::AsyncWrap::MemoryInfoName` indexes `provider_names` by
//!   `provider_type_` (`src/async_wrap.cc`).
//! - `node::AsyncWrap::GetAsyncId` reads the wrapper from its object's
//!   embedder field and the async id from the wrapper.

use std::collections::BTreeMap;
use std::sync::Arc;

use iced_x86::{Decoder, DecoderOptions, Instruction, Mnemonic, OpKind, Register};
use object::{Object, ObjectSection, ObjectSymbol, SectionKind, SymbolKind};

use crate::ImageAddress;
use crate::image::v8::{Constant, Field, NODEDBG, NodeTasks, UvPool, V8Layout};

const GET_CURRENT_CONTEXT: &str = "_ZN2v87Isolate17GetCurrentContextEv";
const SETUP_TIMERS: &str =
    "_ZN4node6timers11BindingData11SetupTimersERKN2v820FunctionCallbackInfoINS2_5ValueEEE";
const SET_TIMERS_CALLBACK: &str =
    "_ZN4node14PrincipalRealm28set_timers_callback_functionEN2v85LocalINS1_8FunctionEEE";
const MEMORY_INFO_NAME: &str = "_ZNK4node9AsyncWrap14MemoryInfoNameEv";
const GET_ASYNC_ID: &str =
    "_ZN4node9AsyncWrap10GetAsyncIdERKN2v820FunctionCallbackInfoINS1_5ValueEEE";
const CONTEXT_TAG: &str = "_ZN4node18ContextEmbedderTag18kNodeContextTagPtrE";

/// The most bytes of one function decoded.
const MOST_BYTES: u64 = 1024;

/// The static variables of libuv's `threadpool.c` the walk reads.
const POOL: [&str; 5] = ["threads", "nthreads", "idle_threads", "cond", "mutex"];

/// Where Node keeps its event loop's work in `object`, whose V8 objects
/// have `layout`.
pub(super) fn layout(
    object: &object::File<'_>,
    layout: Result<&V8Layout, &Arc<str>>,
) -> Result<NodeTasks, Arc<str>> {
    let layout = layout.map_err(|reason| -> Arc<str> {
        format!("Node's tasks need V8's layout, which is refused: {reason}").into()
    })?;
    let symbol = |name: &str| named(object, name).map(|(address, _)| address);
    let needed = |name: &str| -> Result<u64, Arc<str>> {
        symbol(name).ok_or_else(|| format!("the module names no {name}").into())
    };

    let current_context =
        current_context(&code(object, GET_CURRENT_CONTEXT)?, GET_CURRENT_CONTEXT)?;

    let tag = layout
        .constant(Constant::HeapObjectTag)
        .ok_or("the layout has no heap object tag")?
        .cast_unsigned();
    let elements = layout
        .field(Field::ContextElements)
        .ok_or("the layout has no context elements")?;
    let index = layout
        .constant(Constant::ContextEmbedderDataIndex)
        .ok_or("the layout has no embedder data slot")?
        .cast_unsigned();
    let embedder_data = (elements + 8 * index).wrapping_sub(tag);
    let setup = setup_timers(&code(object, SETUP_TIMERS)?, embedder_data, SETUP_TIMERS)?;

    let timers_callback = realm_slot(&code(object, SET_TIMERS_CALLBACK)?, SET_TIMERS_CALLBACK)?;
    let (provider_type, provider_names) =
        provider(&code(object, MEMORY_INFO_NAME)?, MEMORY_INFO_NAME)?;
    let (wrapper, async_id) = async_id(&code(object, GET_ASYNC_ID)?, GET_ASYNC_ID)?;

    let mut nodedbg = [ImageAddress::new(0); NODEDBG.len()];
    for (slot, name) in nodedbg.iter_mut().zip(NODEDBG) {
        *slot = ImageAddress::new(needed(name)?);
    }

    Ok(NodeTasks {
        current_context,
        environment_slot: setup.environment,
        tag_slot: setup.tag,
        context_tag: ImageAddress::new(needed(CONTEXT_TAG)?),
        principal_realm: setup.realm,
        timers_callback,
        provider_type,
        provider_names: ImageAddress::new(provider_names),
        async_id,
        object_wrapper: wrapper.wrapping_add(tag),
        nodedbg,
        pool: pool(object),
    })
}

/// The address and size of the definition named `name`.
fn named(object: &object::File<'_>, name: &str) -> Option<(u64, u64)> {
    object
        .symbols()
        .chain(object.dynamic_symbols())
        .find(|symbol| symbol.is_definition() && symbol.name() == Ok(name))
        .map(|symbol| (symbol.address(), symbol.size()))
}

/// The instructions of the function named `name`, over its whole size.
fn code(object: &object::File<'_>, name: &str) -> Result<Vec<Instruction>, Arc<str>> {
    let (address, size) = object
        .symbols()
        .chain(object.dynamic_symbols())
        .find(|symbol| {
            symbol.kind() == SymbolKind::Text && symbol.is_definition() && symbol.name() == Ok(name)
        })
        .map(|symbol| (symbol.address(), symbol.size()))
        .ok_or_else(|| -> Arc<str> { format!("the module names no {name}").into() })?;
    let size = if size == 0 {
        MOST_BYTES
    } else {
        size.min(MOST_BYTES)
    };
    let bytes = object
        .sections()
        .find_map(|section| {
            if section.kind() != SectionKind::Text {
                return None;
            }
            let at = usize::try_from(address.checked_sub(section.address())?).ok()?;
            let data = section.data().ok()?;
            let end = at.checked_add(usize::try_from(size).ok()?)?.min(data.len());
            data.get(at..end)
        })
        .ok_or_else(|| -> Arc<str> { format!("{name} has no code in the module").into() })?;
    let mut decoder = Decoder::with_ip(64, bytes, address, DecoderOptions::NONE);
    let mut found = Vec::new();
    while decoder.can_decode() {
        let instruction = decoder.decode();
        if instruction.is_invalid() {
            break;
        }
        found.push(instruction);
    }
    Ok(found)
}

fn unexpected(what: &str) -> Arc<str> {
    format!("{what} does not have the shape microscope reads").into()
}

/// The 64-bit register `instruction` loads from `[base + displacement]`,
/// with no index or segment, and the base and displacement.
fn load(instruction: &Instruction) -> Option<(Register, Register, u64)> {
    (instruction.mnemonic() == Mnemonic::Mov
        && instruction.op_count() == 2
        && instruction.op0_kind() == OpKind::Register
        && instruction.op0_register().is_gpr64()
        && instruction.op1_kind() == OpKind::Memory
        && instruction.memory_index() == Register::None
        && instruction.segment_prefix() == Register::None
        && instruction.memory_base() != Register::RIP)
        .then(|| {
            (
                instruction.op0_register(),
                instruction.memory_base(),
                instruction.memory_displacement64(),
            )
        })
}

fn is_prologue(instruction: &Instruction) -> bool {
    instruction.mnemonic() == Mnemonic::Endbr64
        || super::super::x86_64::is_push_register(instruction, Register::RBP)
        || super::super::x86_64::is_register_move(instruction, Register::RBP, Register::RSP)
}

/// `GetCurrentContext`'s first load, `mov r, [rdi + X]`, followed by
/// `test r, r`: X.
fn current_context(code: &[Instruction], what: &str) -> Result<u64, Arc<str>> {
    let mut body = code.iter().filter(|instruction| !is_prologue(instruction));
    let (Some(first), Some(second)) = (body.next(), body.next()) else {
        return Err(unexpected(what));
    };
    match load(first) {
        Some((to, Register::RDI, offset))
            if second.mnemonic() == Mnemonic::Test
                && second.op0_kind() == OpKind::Register
                && second.op1_kind() == OpKind::Register
                && second.op0_register() == to
                && second.op1_register() == to =>
        {
            Ok(offset)
        }
        _ => Err(unexpected(what)),
    }
}

/// What a register of `SetupTimers` holds, as far as its loads say.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Held {
    /// A native context's embedder data.
    EmbedderData,

    /// The word at this offset of the embedder data.
    Slot(u64),

    /// A RIP-relative address, or a word read from one.
    Static,
}

/// What `SetupTimers` says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Setup {
    environment: u64,
    tag: u64,
    realm: u64,
}

/// The embedder data slots `SetupTimers` reads, the one it compares (the
/// context tag) and the one whose word it passes `[word + realm]` from
/// (the `Environment`), and the realm's offset. Every register is followed
/// in order through the function's code; a call forgets the registers it
/// may change. `embedder_data` is where a native context keeps its
/// embedder data, from its tagged address.
fn setup_timers(code: &[Instruction], embedder_data: u64, what: &str) -> Result<Setup, Arc<str>> {
    const CALLER_SAVED: [Register; 9] = [
        Register::RAX,
        Register::RCX,
        Register::RDX,
        Register::RSI,
        Register::RDI,
        Register::R8,
        Register::R9,
        Register::R10,
        Register::R11,
    ];
    let mut held: BTreeMap<Register, Held> = BTreeMap::new();
    let mut tags = Vec::new();
    let mut realms: Vec<(u64, u64)> = Vec::new();

    for instruction in code {
        let mnemonic = instruction.mnemonic();
        if mnemonic == Mnemonic::Cmp {
            let mut compared = Vec::new();
            for operand in 0..2 {
                match instruction.op_kind(operand) {
                    OpKind::Register => {
                        compared.push(held.get(&instruction.op_register(operand).full_register()));
                    }
                    OpKind::Memory if instruction.memory_index() == Register::None => {
                        let base = instruction.memory_base();
                        if base == Register::RIP {
                            compared.push(Some(&Held::Static));
                        } else if held.get(&base) == Some(&Held::EmbedderData) {
                            tags.push(instruction.memory_displacement64());
                        } else if instruction.memory_displacement64() == 0 {
                            compared.push(held.get(&base));
                        }
                    }
                    _ => {}
                }
            }
            if compared.contains(&Some(&Held::Static)) {
                for slot in compared.iter().filter_map(|held| match held {
                    Some(Held::Slot(slot)) => Some(*slot),
                    _ => None,
                }) {
                    tags.push(slot);
                }
            }
            continue;
        }
        if matches!(mnemonic, Mnemonic::Call) {
            for register in CALLER_SAVED {
                held.remove(&register);
            }
            continue;
        }
        if let Some((to, base, offset)) = load(instruction) {
            let value = match held.get(&base) {
                _ if offset == embedder_data => Some(Held::EmbedderData),
                Some(Held::EmbedderData) => Some(Held::Slot(offset)),
                Some(Held::Slot(slot)) if to == Register::RDI => {
                    realms.push((*slot, offset));
                    None
                }
                _ => None,
            };
            match value {
                Some(value) => held.insert(to, value),
                None => held.remove(&to),
            };
            continue;
        }
        if matches!(mnemonic, Mnemonic::Mov | Mnemonic::Lea)
            && instruction.op0_kind() == OpKind::Register
            && instruction.op1_kind() == OpKind::Memory
            && instruction.memory_base() == Register::RIP
        {
            held.insert(instruction.op0_register().full_register(), Held::Static);
            continue;
        }
        if instruction.op_count() > 0
            && instruction.op0_kind() == OpKind::Register
            && mnemonic != Mnemonic::Test
            && mnemonic != Mnemonic::Push
        {
            held.remove(&instruction.op0_register().full_register());
        }
    }

    tags.sort_unstable();
    tags.dedup();
    realms.sort_unstable();
    realms.dedup();
    match (tags.as_slice(), realms.as_slice()) {
        ([tag], [(environment, realm)]) if tag != environment => Ok(Setup {
            environment: *environment,
            tag: *tag,
            realm: *realm,
        }),
        _ => Err(format!(
            "{what} does not have the shape microscope reads: tag slots {tags:x?}, realm loads \
             {realms:x?}"
        )
        .into()),
    }
}

/// The offset `set_timers_callback_function` disposes and stores its realm's
/// global handle at: every store through `this` must be to it, and it must
/// be loaded first.
fn realm_slot(code: &[Instruction], what: &str) -> Result<u64, Arc<str>> {
    let mut this = vec![Register::RDI];
    let mut loads = Vec::new();
    let mut stores = Vec::new();
    for instruction in code.iter().filter(|instruction| !is_prologue(instruction)) {
        if instruction.mnemonic() == Mnemonic::Mov
            && instruction.op0_kind() == OpKind::Register
            && instruction.op1_kind() == OpKind::Register
            && this.contains(&instruction.op1_register())
        {
            this.push(instruction.op0_register());
            continue;
        }
        if instruction.mnemonic() == Mnemonic::Mov
            && instruction.op0_kind() == OpKind::Memory
            && instruction.memory_index() == Register::None
            && this.contains(&instruction.memory_base())
        {
            stores.push(instruction.memory_displacement64());
            continue;
        }
        if let Some((to, base, offset)) = load(instruction)
            && this.contains(&base)
        {
            loads.push(offset);
            this.retain(|register| *register != to);
            continue;
        }
        if instruction.mnemonic() == Mnemonic::Ret {
            break;
        }
    }
    stores.sort_unstable();
    stores.dedup();
    match stores.as_slice() {
        [slot] if loads.contains(slot) => Ok(*slot),
        _ => Err(unexpected(what)),
    }
}

/// `MemoryInfoName`'s `provider_type_` offset and the address of the
/// provider names' table: `mov{sxd} i, [rdi + P]`, then `mov rax, [T + i *
/// 8]`, or `lea t, [rip + T]; mov rax, [t + i * 8]`.
fn provider(code: &[Instruction], what: &str) -> Result<(u64, u64), Arc<str>> {
    let mut body = code.iter().filter(|instruction| !is_prologue(instruction));
    let first = body.next().ok_or_else(|| unexpected(what))?;
    if !matches!(first.mnemonic(), Mnemonic::Movsxd | Mnemonic::Mov)
        || first.op0_kind() != OpKind::Register
        || first.op1_kind() != OpKind::Memory
        || first.memory_base() != Register::RDI
        || first.memory_index() != Register::None
        || first.memory_size().size() != 4
    {
        return Err(unexpected(what));
    }
    let offset = first.memory_displacement64();
    let index = first.op0_register().full_register();
    let mut table = None;
    for instruction in body {
        if instruction.mnemonic() == Mnemonic::Lea
            && instruction.is_ip_rel_memory_operand()
            && instruction.op0_kind() == OpKind::Register
        {
            table = Some((
                instruction.op0_register(),
                instruction.ip_rel_memory_address(),
            ));
            continue;
        }
        if instruction.mnemonic() == Mnemonic::Mov
            && instruction.op1_kind() == OpKind::Memory
            && instruction.memory_index() == index
            && instruction.memory_index_scale() == 8
        {
            return match (instruction.memory_base(), table) {
                (Register::None, _) => Ok((offset, instruction.memory_displacement64())),
                (base, Some((register, address)))
                    if base == register && instruction.memory_displacement64() == 0 =>
                {
                    Ok((offset, address))
                }
                _ => Err(unexpected(what)),
            };
        }
        if instruction.mnemonic() == Mnemonic::Ret {
            break;
        }
    }
    Err(unexpected(what))
}

/// `GetAsyncId`'s fast path: `mov w, [o + E]` reads the wrapper from the
/// object's embedder field, and `movsd xmm0, [w + A]` its async id: E, from
/// the object's tagged address, and A.
fn async_id(code: &[Instruction], what: &str) -> Result<(u64, u64), Arc<str>> {
    let mut wrappers: BTreeMap<Register, u64> = BTreeMap::new();
    let mut found = Vec::new();
    for instruction in code {
        if instruction.mnemonic() == Mnemonic::Movsd
            && instruction.op0_kind() == OpKind::Register
            && instruction.op1_kind() == OpKind::Memory
            && instruction.memory_index() == Register::None
            && let Some(field) = wrappers.get(&instruction.memory_base())
        {
            found.push((*field, instruction.memory_displacement64()));
            continue;
        }
        if let Some((to, _, offset)) = load(instruction) {
            wrappers.insert(to, offset);
            continue;
        }
        if instruction.mnemonic() == Mnemonic::Call {
            // A call returns its result in rax and may change the rest of
            // the caller-saved registers.
            wrappers.clear();
            continue;
        }
        if instruction.op_count() > 0
            && instruction.op0_kind() == OpKind::Register
            && !matches!(instruction.mnemonic(), Mnemonic::Test | Mnemonic::Cmp)
        {
            wrappers.remove(&instruction.op0_register().full_register());
        }
    }
    found.sort_unstable();
    found.dedup();
    match found.as_slice() {
        [(field, id)] => Ok((*field, *id)),
        _ => Err(unexpected(what)),
    }
}

/// libuv's thread pool, where `object` links libuv and keeps the local
/// symbols of `threadpool.c`: each named once after that file's symbol.
fn pool(object: &object::File<'_>) -> Option<UvPool> {
    let mut file = None;
    let mut found: BTreeMap<&str, Vec<u64>> = BTreeMap::new();
    for symbol in object.symbols() {
        if symbol.kind() == SymbolKind::File {
            file = symbol.name().ok();
            continue;
        }
        if file != Some("threadpool.c") || symbol.kind() != SymbolKind::Data {
            continue;
        }
        if let Ok(name) = symbol.name()
            && POOL.contains(&name)
        {
            found.entry(name).or_default().push(symbol.address());
        }
    }
    let one = |name: &str| match found.get(name).map(Vec::as_slice) {
        Some([address]) => Some(ImageAddress::new(*address)),
        _ => None,
    };
    Some(UvPool {
        threads: one("threads")?,
        nthreads: one("nthreads")?,
        idle_threads: one("idle_threads")?,
        cond: one("cond")?,
        mutex: one("mutex")?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode(parts: &[&[u8]]) -> Vec<Instruction> {
        let bytes = parts.concat();
        let mut decoder = Decoder::with_ip(64, &bytes, 0x1000, DecoderOptions::NONE);
        std::iter::from_fn(|| decoder.can_decode().then(|| decoder.decode())).collect()
    }

    const RET: &[u8] = &[0xc3];

    #[test]
    fn the_current_context_is_the_first_load_from_the_isolate() {
        // mov rax, [rdi + 0x158]
        let load = &[0x48, 0x8b, 0x87, 0x58, 0x01, 0, 0][..];
        // test rax, rax; then test rdx, rdx
        assert_eq!(
            current_context(&decode(&[load, &[0x48, 0x85, 0xc0]]), "26"),
            Ok(0x158)
        );
        assert!(current_context(&decode(&[load, &[0x48, 0x85, 0xd2]]), "planted").is_err());
    }

    /// Node 22's shape: the tag compared through a register loaded from the
    /// embedder data, and the environment's realm passed in rdi.
    #[test]
    fn setup_timers_gives_the_environment_tag_and_realm() {
        let context = &[0x49, 0x8b, 0x04, 0x24][..]; // mov rax, [r12]
        let data = &[0x48, 0x8b, 0x40, 0x2f][..]; // mov rax, [rax + 0x2f]
        let tag = &[0x48, 0x8b, 0x90, 0x47, 0x01, 0, 0][..]; // mov rdx, [rax + 0x147]
        let compare = &[0x48, 0x39, 0x15, 0, 0x01, 0, 0][..]; // cmp [rip + 0x100], rdx
        let environment = &[0x4c, 0x8b, 0xa0, 0x0f, 0x01, 0, 0][..]; // mov r12, [rax + 0x10f]
        let realm = &[0x49, 0x8b, 0xbc, 0x24, 0x80, 0x0b, 0, 0][..]; // mov rdi, [r12 + 0xb80]
        let other = &[0x49, 0x8b, 0xbc, 0x24, 0x88, 0x0b, 0, 0][..]; // mov rdi, [r12 + 0xb88]

        let shape = decode(&[context, data, tag, compare, environment, realm]);
        assert_eq!(
            setup_timers(&shape, 0x2f, "22"),
            Ok(Setup {
                environment: 0x10f,
                tag: 0x147,
                realm: 0xb80
            })
        );
        assert!(setup_timers(&shape, 0x37, "planted").is_err());
        let disagreeing = decode(&[context, data, tag, compare, environment, realm, other]);
        assert!(setup_timers(&disagreeing, 0x2f, "planted").is_err());
        let uncompared = decode(&[context, data, tag, environment, realm]);
        assert!(setup_timers(&uncompared, 0x2f, "planted").is_err());
    }

    #[test]
    fn the_realm_slot_is_the_one_stored_through_this() {
        let this = &[0x48, 0x89, 0xfb][..]; // mov rbx, rdi
        let isolate = &[0x4c, 0x8b, 0xaf, 0xb8, 0, 0, 0][..]; // mov r13, [rdi + 0xb8]
        let old = &[0x48, 0x8b, 0xbf, 0x88, 0x02, 0, 0][..]; // mov rdi, [rdi + 0x288]
        let store = &[0x48, 0x89, 0x83, 0x88, 0x02, 0, 0][..]; // mov [rbx + 0x288], rax
        assert_eq!(
            realm_slot(&decode(&[this, isolate, old, store, RET]), "26"),
            Ok(0x288)
        );
        assert!(realm_slot(&decode(&[this, isolate, store, RET]), "planted").is_err());
    }

    #[test]
    fn memory_info_name_gives_the_provider_table() {
        let index = &[0x48, 0x63, 0x47, 0x30][..]; // movsxd rax, [rdi + 0x30]
        // mov rax, [rax * 8 + 0x340b060], and with a scale of 4
        let table = &[0x48, 0x8b, 0x04, 0xc5, 0x60, 0xb0, 0x40, 0x03][..];
        let scaled = &[0x48, 0x8b, 0x04, 0x85, 0x60, 0xb0, 0x40, 0x03][..];
        assert_eq!(
            provider(&decode(&[index, table, RET]), "22"),
            Ok((0x30, 0x340_b060))
        );
        assert!(provider(&decode(&[index, scaled, RET]), "planted").is_err());
    }

    #[test]
    fn get_async_id_gives_the_wrapper_field_and_the_id() {
        let wrapper = &[0x49, 0x8b, 0x46, 0x27][..]; // mov rax, [r14 + 0x27]
        let id = &[0xf2, 0x0f, 0x10, 0x40, 0x38][..]; // movsd xmm0, [rax + 0x38]
        let test = &[0x48, 0x85, 0xc0][..]; // test rax, rax
        let call = &[0xe8, 0, 0, 0, 0][..];
        assert_eq!(
            async_id(&decode(&[wrapper, test, id]), "26"),
            Ok((0x27, 0x38))
        );
        assert!(async_id(&decode(&[wrapper, call, id]), "planted").is_err());
        assert!(async_id(&decode(&[id]), "planted").is_err());
    }
}
