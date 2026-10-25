//! The async functions suspended in `await` that wait for the one a
//! thread runs: V8's await chain, which no stack holds.
//!
//! An async function resumed after an await runs inside the closure V8
//! made for the await (`AsyncFunctionAwaitResolveClosure` and its kin),
//! whose frame is outer to the function's own. The closure's context
//! extension is the function's generator object, and its promise is the
//! one the function's caller awaits. From there the chain follows V8's
//! `CaptureAsyncStackTrace` (`isolate.cc`, the same in 12.4, 13.6, and
//! 14.6): a pending promise's one reaction names who waits for it. A
//! reaction whose fulfill handler is an await's closure names a suspended
//! generator, whose function and bytecode offset make a frame, and whose
//! own promise is the next; `Promise.all`, `allSettled`, and `any` name
//! the capability they settle once every element does; a capability's
//! default resolve function names its promise; and any other reaction
//! the promise it settles. Each step reads an object of the type V8's
//! code proves it is, and the chain ends where V8's does.

use std::collections::BTreeSet;
use std::sync::Arc;

use super::objects::{Object, Problem, Reader};
use super::positions::Encoding;
use super::scripts::Table;
use crate::image::v8::{Builtin, Constant, Field};
use crate::runtime_model::{Awaiting, InterpretedFrame};
use crate::{CodeRole, VirtualAddress};

/// The most promises a chain follows.
const MAX_STEPS: usize = 256;

/// The longest bytecode array read to find a suspend's operands, as long
/// as the longest byte array the position tables read.
const MAX_BYTECODE: usize = 1 << 24;

/// The closures that resume an async function or generator after an
/// await, as `IsBuiltinAsyncFulfillHandler` and
/// `IsBuiltinAsyncRejectHandler` know them.
const RESUMES: [Builtin; 5] = [
    Builtin::AsyncFunctionAwaitResolveClosure,
    Builtin::AsyncFunctionAwaitRejectClosure,
    Builtin::AsyncGeneratorAwaitResolveClosure,
    Builtin::AsyncGeneratorAwaitRejectClosure,
    Builtin::AsyncGeneratorYieldWithAwaitResolveClosure,
];

/// The fulfill handlers of an await's reaction (`TryGetAsyncGenerator`).
const FULFILLS: [Builtin; 3] = [
    Builtin::AsyncFunctionAwaitResolveClosure,
    Builtin::AsyncGeneratorAwaitResolveClosure,
    Builtin::AsyncGeneratorYieldWithAwaitResolveClosure,
];

/// The numbers of the builtins a chain is read by, in the module's V8.
pub(super) type Builtins = crate::image::v8::Numbers;

/// The await chain of the async function the frame running `closure`
/// resumed, or `None` where `closure` resumes none. `builtins` are the
/// numbers of the builtins the chain is read by, or why they are unknown.
pub(super) fn awaiting(
    reader: &Reader<'_>,
    table: &mut Table,
    encoding: Encoding,
    builtins: &Result<Builtins, Arc<str>>,
    closure: Object,
) -> Option<Awaiting> {
    let shared = reader
        .typed_field(
            closure,
            Field::JSFunctionSharedFunctionInfo,
            Constant::SharedFunctionInfoType,
        )
        .ok()?;
    let id = super::call_sites::builtin_id(reader, shared).ok()??;
    let builtins = match builtins {
        Ok(builtins) => *builtins,
        Err(reason) => {
            return Some(Awaiting {
                frames: Vec::new(),
                end: Some(format!("the await chain cannot be read: {reason}").into()),
            });
        }
    };
    let resumed = RESUMES
        .into_iter()
        .find(|builtin| number(builtins, *builtin) == Some(id))?;

    let mut frames = Vec::new();
    let mut walk = Walk {
        reader,
        table,
        encoding,
        builtins,
        frames: &mut frames,
    };
    let end = walk.follow(closure, resumed).err();
    debug_assert!(
        frames.len() <= MAX_STEPS,
        "a chain of {} frames outgrew its steps",
        frames.len()
    );
    Some(Awaiting { frames, end })
}

fn number(builtins: Builtins, builtin: Builtin) -> Option<i64> {
    builtins
        .iter()
        .find(|(known, _)| *known == builtin)
        .map(|(_, number)| *number)
}

/// A chain as it is read.
struct Walk<'a, 'r> {
    reader: &'a Reader<'r>,
    table: &'a mut Table,
    encoding: Encoding,
    builtins: Builtins,
    frames: &'a mut Vec<InterpretedFrame>,
}

/// Why a chain's next step is not taken: it ends there, as V8's does, or
/// it cannot be read.
type Step = Result<Option<Object>, Problem>;

impl Walk<'_, '_> {
    fn is(&self, builtin: Builtin, id: Option<i64>) -> bool {
        id.is_some() && number(self.builtins, builtin) == id
    }

    /// Follows the chain from the closure `closure`, the builtin
    /// `resumed`, whose frame runs: its generator must be the one that
    /// runs, and its promise is the first awaited.
    fn follow(&mut self, closure: Object, resumed: Builtin) -> Result<(), Problem> {
        let reader = self.reader;
        let generator = generator(reader, closure)?;
        let continuation = reader.smi_field(generator, Field::JSGeneratorObjectContinuation)?;
        if continuation != reader.constant(Constant::GeneratorExecuting)? {
            return Err(format!(
                "the generator the await's closure resumed does not run (its continuation is {continuation})"
            )
            .into());
        }
        let reject = matches!(
            resumed,
            Builtin::AsyncFunctionAwaitRejectClosure | Builtin::AsyncGeneratorAwaitRejectClosure
        );
        self.check_reused(generator, closure, reject)?;
        let mut promise = promise_of(reader, generator)?;
        let mut seen = BTreeSet::new();
        for _ in 0..MAX_STEPS {
            let Some(current) = promise else {
                return Ok(());
            };
            if !seen.insert(current) {
                return Err("the await chain is a cycle".into());
            }
            promise = self.step(current)?;
        }
        if promise.is_some() {
            return Err(format!("the await chain is longer than {MAX_STEPS} promises").into());
        }
        Ok(())
    }

    /// From 14.6 an async function makes its await closures once, and
    /// keeps them: a closure that resumes one must be the one it keeps.
    fn check_reused(
        &self,
        generator: Object,
        closure: Object,
        reject: bool,
    ) -> Result<(), Problem> {
        let reader = self.reader;
        let field = if reject {
            Field::JSAsyncFunctionObjectAwaitRejectClosure
        } else {
            Field::JSAsyncFunctionObjectAwaitResolveClosure
        };
        if !reader.layout.has(field)
            || !reader.is(generator, Constant::JSAsyncFunctionObjectType)?
        {
            return Ok(());
        }
        if reader.field(generator, field)? != closure.0 {
            return Err(
                "the async function keeps other await closures than the one that resumed it".into(),
            );
        }
        Ok(())
    }

    /// One step of `CaptureAsyncStackTrace`'s loop from the pending
    /// promise `promise`: the frame its one reaction names, if any, and
    /// the next promise, or `None` where the chain ends.
    fn step(&mut self, promise: Object) -> Step {
        let reader = self.reader;
        let flags = reader
            .smi(reader.field(promise, Field::JSPromiseFlags)?)?
            .ok_or("a promise's flags are not a Smi")?;
        let status = super::properties::part(
            reader,
            flags,
            Constant::JSPromiseStatusMask,
            Some(Constant::JSPromiseStatusShift),
        )?;
        // A settled promise has run its reactions, or queued them.
        if status != reader.constant(Constant::PromisePending)? {
            return Ok(None);
        }
        // A pending promise with no reaction keeps a Smi; nothing waits.
        let Some(reaction) =
            reader.object(reader.field(promise, Field::JSPromiseReactionsOrResult)?)?
        else {
            return Ok(None);
        };
        if !reader.is(reaction, Constant::PromiseReactionType)? {
            return Err("a pending promise's reactions are no PromiseReaction".into());
        }
        if reader
            .smi(reader.field(reaction, Field::PromiseReactionNext)?)?
            .is_none()
        {
            return Err("several reactions wait for the promise, and V8 follows only one".into());
        }
        self.reaction(reaction)
    }

    /// What the one reaction `reaction` of a pending promise names: the
    /// frame of what waits, if any, and the next promise, or `None` where
    /// the chain ends.
    fn reaction(&mut self, reaction: Object) -> Step {
        let reader = self.reader;
        let fulfill = reader.field(reaction, Field::PromiseReactionFulfillHandler)?;
        let reject = reader.field(reaction, Field::PromiseReactionRejectHandler)?;
        let (fulfill, fulfill_id) = handler(reader, fulfill)?;
        let (reject, reject_id) = handler(reader, reject)?;

        if let Some(fulfill) = fulfill
            && FULFILLS
                .into_iter()
                .any(|builtin| self.is(builtin, fulfill_id))
        {
            let generator = generator(reader, fulfill)?;
            let continuation = reader.smi_field(generator, Field::JSGeneratorObjectContinuation)?;
            if continuation < 0 {
                return Err(format!(
                    "an await's generator is not suspended (its continuation is {continuation})"
                )
                .into());
            }
            self.check_reused(generator, fulfill, false)?;
            self.suspended(generator)?;
            return promise_of(reader, generator);
        }

        let combinator = [
            (
                Builtin::PromiseAllResolveElementClosure,
                fulfill,
                fulfill_id,
                "all",
            ),
            (
                Builtin::PromiseAllSettledResolveElementClosure,
                fulfill,
                fulfill_id,
                "allSettled",
            ),
            (
                Builtin::PromiseAnyRejectElementClosure,
                reject,
                reject_id,
                "any",
            ),
        ]
        .into_iter()
        .find_map(|(builtin, function, id, name)| {
            self.is(builtin, id).then_some((function?, name, builtin))
        });
        if let Some((element, name, builtin)) = combinator {
            self.combinator(element, name)?;
            let context = function_context(reader, element)?;
            // V8 replaces a called element's context by the native one.
            if reader.is(context, Constant::NativeContextType)? {
                return Ok(None);
            }
            let slot = if builtin == Builtin::PromiseAnyRejectElementClosure {
                Constant::PromiseAnyRejectElementCapabilitySlot
            } else {
                Constant::PromiseAllResolveElementCapabilitySlot
            };
            let capability = object_where(
                reader,
                super::access::slot(reader, context, slot)?,
                |capability| reader.is(capability, Constant::PromiseCapabilityType),
            )?
            .ok_or("a combinator's element keeps no PromiseCapability")?;
            return capability_promise(reader, capability);
        }

        if let Some(fulfill) = fulfill
            && self.is(Builtin::PromiseCapabilityDefaultResolve, fulfill_id)
        {
            let context = function_context(reader, fulfill)?;
            let held = super::access::slot(reader, context, Constant::PromiseIfNotResolvedSlot)?;
            // Undefined once the capability resolved.
            return js_promise(reader, held);
        }

        // Any other reaction: the promise it settles, where one does.
        let settles = reader.field(reaction, Field::PromiseReactionPromiseOrCapability)?;
        match reader.object(settles)? {
            Some(capability) if reader.is(capability, Constant::PromiseCapabilityType)? => {
                capability_promise(reader, capability)
            }
            _ => js_promise(reader, settles),
        }
    }

    /// Adds the frame of the suspended generator `generator`, unless V8's
    /// stack traces hide its function.
    fn suspended(&mut self, generator: Object) -> Result<(), Problem> {
        let reader = self.reader;
        let function = reader.field(generator, Field::JSGeneratorObjectFunction)?;
        let function = object_where(reader, function, |function| is_function(reader, function))?
            .ok_or("a generator's function is no JSFunction")?;
        let shared = reader.typed_field(
            function,
            Field::JSFunctionSharedFunctionInfo,
            Constant::SharedFunctionInfoType,
        )?;
        if super::frames::script(reader, shared)?.is_none()
            && !super::names::is_native(reader, shared)?
        {
            return Ok(());
        }
        let offset = script_offset(reader, self.encoding, generator, shared);
        let recorded = super::frames::recorded(
            reader,
            self.table,
            super::frames::Named::Function((function, shared)),
            offset,
        )?;
        let role = if recorded
            .place
            .as_ref()
            .is_none_or(|place| place.path.starts_with("node:"))
        {
            CodeRole::RuntimeInternal
        } else {
            CodeRole::Ordinary
        };
        self.frames.push(InterpretedFrame {
            frame: VirtualAddress::new(generator.0),
            language: recorded.language,
            function: Some(recorded.function),
            place: recorded.place,
            problem: recorded.problem,
            first_line: recorded.first_line,
            outermost: false,
            role,
        });
        Ok(())
    }

    /// Adds the frame of the combinator `Promise.NAME` whose element
    /// `element` waits: `AppendPromiseCombinatorFrame`'s, at the element's
    /// index, which V8 keeps, plus one, as the element function's hash.
    fn combinator(&mut self, element: Object, name: &str) -> Result<(), Problem> {
        let reader = self.reader;
        let index = identity_hash(reader, element)? - 1;
        self.frames.push(InterpretedFrame {
            frame: VirtualAddress::new(element.0),
            language: crate::SourceLanguage::JavaScript,
            function: Some(format!("Promise.{name} (index {index})").into()),
            place: None,
            problem: None,
            first_line: None,
            outermost: false,
            role: CodeRole::RuntimeInternal,
        });
        Ok(())
    }
}

/// The identity hash of `object` (`GetIdentityHashHelper`): its
/// properties' word where that is a Smi, or the hash its property array
/// keeps above the array's length.
fn identity_hash(reader: &Reader<'_>, object: Object) -> Result<i64, Problem> {
    let properties = reader.field(object, Field::JSReceiverPropertiesOrHash)?;
    let hash = match reader.object(properties)? {
        None => reader
            .smi(properties)?
            .ok_or("an object's properties are no Smi")?,
        Some(array) if reader.is(array, Constant::PropertyArrayType)? => {
            reader.smi_field(array, Field::PropertyArrayLengthAndHash)?
                >> reader.constant(Constant::PropertyArrayLengthFieldSize)?
        }
        Some(_) => {
            return Err("a combinator's element keeps its index in a dictionary, which microscope does not read".into());
        }
    };
    // V8's kNoHashSentinel.
    if hash <= 0 {
        return Err("a combinator's element keeps no index".into());
    }
    Ok(hash)
}

/// Whether `object` is a `JSFunction` of any of its types.
fn is_function(reader: &Reader<'_>, object: Object) -> Result<bool, Problem> {
    let ty = reader.instance_type(object)?;
    Ok((reader.constant(Constant::FirstJSFunctionType)?
        ..=reader.constant(Constant::LastJSFunctionType)?)
        .contains(&ty))
}

/// Whether `object` is a generator object of any of its types.
pub(super) fn is_generator(reader: &Reader<'_>, object: Object) -> Result<bool, Problem> {
    let ty = reader.instance_type(object)?;
    Ok((reader.constant(Constant::FirstJSGeneratorObjectType)?
        ..=reader.constant(Constant::LastJSGeneratorObjectType)?)
        .contains(&ty))
}

/// A reaction's handler, the function it is if it is one, and that
/// function's builtin number if it runs one.
fn handler(reader: &Reader<'_>, word: u64) -> Result<(Option<Object>, Option<i64>), Problem> {
    let Some(function) = object_where(reader, word, |object| is_function(reader, object))? else {
        return Ok((None, None));
    };
    let shared = reader.typed_field(
        function,
        Field::JSFunctionSharedFunctionInfo,
        Constant::SharedFunctionInfoType,
    )?;
    Ok((
        Some(function),
        super::call_sites::builtin_id(reader, shared)?,
    ))
}

fn function_context(reader: &Reader<'_>, function: Object) -> Result<Object, Problem> {
    reader
        .object(reader.field(function, Field::JSFunctionContext)?)?
        .ok_or_else(|| "a function's context is no object".into())
}

/// The generator an await's closure `closure` resumes: its await
/// context's extension.
fn generator(reader: &Reader<'_>, closure: Object) -> Result<Object, Problem> {
    let context = function_context(reader, closure)?;
    if !reader.is(context, Constant::AwaitContextType)? {
        return Err("an await's closure has no await context".into());
    }
    object_where(
        reader,
        super::access::slot(reader, context, Constant::ContextExtensionIndex)?,
        |generator| is_generator(reader, generator),
    )?
    .ok_or_else(|| "an await's context holds no generator".into())
}

/// The promise the caller of the async function or generator
/// `generator` waits for: an async function's own, or the promise of the
/// request an async generator serves, if it serves one.
fn promise_of(reader: &Reader<'_>, generator: Object) -> Step {
    if reader.is(generator, Constant::JSAsyncFunctionObjectType)? {
        return reader
            .typed_field(
                generator,
                Field::JSAsyncFunctionObjectPromise,
                Constant::JSPromiseType,
            )
            .map(Some);
    }
    if !reader.is(generator, Constant::JSAsyncGeneratorObjectType)? {
        return Err(
            "an await's generator is neither an async function's nor an async generator's".into(),
        );
    }
    let queue = reader.field(generator, Field::JSAsyncGeneratorObjectQueue)?;
    let Some(request) = object_where(reader, queue, |request| {
        reader.is(request, Constant::AsyncGeneratorRequestType)
    })?
    else {
        return Ok(None);
    };
    reader
        .typed_field(
            request,
            Field::AsyncGeneratorRequestPromise,
            Constant::JSPromiseType,
        )
        .map(Some)
}

fn capability_promise(reader: &Reader<'_>, capability: Object) -> Step {
    js_promise(
        reader,
        reader.field(capability, Field::PromiseCapabilityPromise)?,
    )
}

/// The promise `word` is, or `None` where it is none, as a capability of
/// a thenable that is no native promise holds.
fn js_promise(reader: &Reader<'_>, word: u64) -> Step {
    object_where(reader, word, |promise| {
        reader.is(promise, Constant::JSPromiseType)
    })
}

/// The object `word` is when `is` holds of it, or `None` when it is no
/// object or `is` does not hold. A failure to tell is the failure, never
/// `None`, so that an unreadable object ends a chain with its reason
/// rather than as a chain that is complete.
fn object_where(
    reader: &Reader<'_>,
    word: u64,
    is: impl FnOnce(Object) -> Result<bool, Problem>,
) -> Result<Option<Object>, Problem> {
    match reader.object(word)? {
        Some(object) if is(object)? => Ok(Some(object)),
        _ => Ok(None),
    }
}

/// The bytecode of the suspended generator `generator`, whose function's
/// shared information is `shared`, and the offset of the instruction it
/// suspended at (`GetGeneratorBytecodeOffset`).
pub(super) fn suspended_at(
    reader: &Reader<'_>,
    generator: Object,
    shared: Object,
) -> Result<(Object, i64), Problem> {
    let bytecode = super::names::bytecode(reader, shared)?
        .ok_or("the suspended function keeps no bytecode")?;
    let raw = reader
        .smi(reader.field(generator, Field::JSGeneratorObjectInputOrDebugPos)?)?
        .ok_or("the suspended generator keeps no bytecode offset")?;
    Ok((
        bytecode,
        super::frames::bytecode_offset(reader, bytecode, raw)?,
    ))
}

/// The offset in its script of where the suspended generator `generator`
/// waits.
fn script_offset(
    reader: &Reader<'_>,
    encoding: Encoding,
    generator: Object,
    shared: Object,
) -> Result<u64, Problem> {
    let (bytecode, offset) = suspended_at(reader, generator, shared)?;
    super::frames::script_offset_at(reader, encoding, bytecode, offset)
}

/// How many registers the suspended generator `generator`, whose
/// function's shared information is `shared`, saved: the register count
/// operand of the `SuspendGenerator` it suspended at, whose operands are
/// its generator's register, the first register saved, the count, and
/// the suspend's number, each as wide as its prefix makes it. The offset
/// V8 keeps is the bytecode's own, past any prefix, so the bytecode is
/// read from its start to tell its prefix from an operand.
pub(super) fn saved_registers(
    reader: &Reader<'_>,
    generator: Object,
    shared: Object,
) -> Result<u32, Problem> {
    const WIDTHS: [usize; 3] = [1, 2, 4];
    let (bytecode, offset) = suspended_at(reader, generator, shared)?;
    let sizes = crate::image::v8::bytecode_sizes(reader.version)?;
    let suspend = crate::image::v8::suspend_generator(reader.version)?;
    let count = sizes.len() / 3;
    let offset = usize::try_from(offset).map_err(|_| "a negative bytecode offset")?;
    let length = usize::try_from(reader.smi_field(bytecode, Field::BytecodeArrayLength)?)
        .map_err(|_| "a bytecode array has a negative length")?;
    // Bounded before allocating, as a byte array is: the length is the
    // target's, and the read is charged only once the buffer exists.
    if length > MAX_BYTECODE {
        return Err("a bytecode array is implausibly long".into());
    }
    // The instruction's four operands follow it, at their widest.
    let mut bytes = vec![0; (offset + 1 + 4 * 4).min(length)];
    reader.bytes_into(
        reader.field_address(bytecode, Field::BytecodeArrayBytes)?,
        &mut bytes,
    )?;

    let mut at = 0;
    while at <= offset {
        let byte = usize::from(*bytes.get(at).ok_or("the bytecode ends before its offset")?);
        let (opcode, scale) = super::compiled::PREFIXES
            .get(byte)
            .map_or((at, 0), |scale| (at + 1, *scale));
        let code = usize::from(
            *bytes
                .get(opcode)
                .ok_or("a bytecode prefix ends its bytecode")?,
        );
        if code >= count {
            return Err("an unknown bytecode".into());
        }
        if opcode == offset {
            if code != usize::from(suspend) {
                return Err(format!(
                    "the generator suspended at bytecode {code}, which is no SuspendGenerator"
                )
                .into());
            }
            let width = WIDTHS[scale];
            let operand = opcode + 1 + 2 * width;
            let field = bytes
                .get(operand..operand + width)
                .ok_or("the SuspendGenerator's operands end its bytecode")?;
            let mut word = [0; 4];
            word[..width].copy_from_slice(field);
            return Ok(u32::from_le_bytes(word));
        }
        let size = usize::from(
            *sizes
                .get(scale * count + code)
                .ok_or("an unknown bytecode")?,
        );
        if size == 0 {
            return Err(format!("bytecode {code} has no size").into());
        }
        // A prefix is a byte of its own before the bytecode it widens.
        at = opcode + size;
    }
    Err("no bytecode begins at the generator's offset".into())
}

#[cfg(test)]
mod tests {
    //! The walk over a heap the tests write, on 14.6's generated layout:
    //! each object is a zeroed block whose first word is its map, and each
    //! map a zeroed block that keeps an instance type. Memory no test wrote
    //! cannot be read.

    use std::collections::HashMap;

    use super::super::objects::{Object, Reader};
    use super::super::positions::Encoding;
    use super::super::scripts::Table;
    use super::{Builtins, MAX_BYTECODE, MAX_STEPS};
    use crate::image::v8::{Builtin, Constant, Field, V8Layout, V8Version};
    use crate::runtime_model::{Awaiting, RuntimeStop};
    use crate::{LoadBias, ThreadId, VirtualAddress};

    /// A V8 heap the tests write: bytes by address.
    #[derive(Default)]
    struct Heap(HashMap<u64, u8>);

    impl Heap {
        fn bytes(&mut self, address: u64, bytes: &[u8]) {
            for (at, byte) in (address..).zip(bytes) {
                self.0.insert(at, *byte);
            }
        }

        fn word(&mut self, address: u64, word: u64) {
            self.bytes(address, &word.to_le_bytes());
        }
    }

    impl RuntimeStop for Heap {
        fn read(&self, address: VirtualAddress, bytes: &mut [u8]) -> bool {
            for (at, byte) in (address.get()..).zip(bytes.iter_mut()) {
                let Some(read) = self.0.get(&at) else {
                    return false;
                };
                *byte = *read;
            }
            true
        }

        fn thread_pointer(&self, _thread: ThreadId) -> Option<u64> {
            None
        }

        fn instruction(&self, _thread: ThreadId) -> Option<VirtualAddress> {
            None
        }

        fn load_bias(&self) -> LoadBias {
            LoadBias::default()
        }

        fn threads(&self) -> Vec<ThreadId> {
            Vec::new()
        }
    }

    const VERSION: V8Version = V8Version {
        major: 14,
        minor: 6,
        build: 0,
        patch: 0,
    };

    /// The numbers the tests give the builtins, chosen by hand: any
    /// distinct non-negative numbers serve.
    const NUMBERS: Builtins = &[
        (Builtin::AsyncFunctionAwaitResolveClosure, 11),
        (Builtin::AsyncFunctionAwaitRejectClosure, 12),
        (Builtin::AsyncGeneratorAwaitResolveClosure, 13),
        (Builtin::AsyncGeneratorAwaitRejectClosure, 14),
        (Builtin::AsyncGeneratorYieldWithAwaitResolveClosure, 15),
        (Builtin::PromiseAllResolveElementClosure, 16),
        (Builtin::PromiseAllSettledResolveElementClosure, 17),
        (Builtin::PromiseAllSettledRejectElementClosure, 18),
        (Builtin::PromiseAnyRejectElementClosure, 19),
        (Builtin::PromiseCapabilityDefaultResolve, 20),
    ];
    const AWAIT_RESOLVE: u64 = 11;
    const GENERATOR_AWAIT_RESOLVE: u64 = 13;

    /// The bytes each object and map takes, past every field the tests
    /// write.
    const BLOCK: u64 = 0x100;

    /// An address no test writes.
    const UNREADABLE: u64 = 0xdead_0000;

    /// 14.6's generated layout, with the constants each build exports.
    fn layout() -> V8Layout {
        let mut layout = crate::debug_info::v8::generated_layout(14, 6).expect("a layout");
        let function = layout.constant(Constant::JSFunctionType).expect("a type");
        for (constant, value) in [
            (Constant::SmiTag, 0),
            (Constant::SmiTagMask, 1),
            (Constant::SmiShiftSize, 31),
            (Constant::HeapObjectTag, 1),
            (Constant::HeapObjectTagMask, 3),
            (Constant::StringRepresentationMask, 7),
            (Constant::SeqStringTag, 0),
            (Constant::ExternalStringTag, 2),
            (Constant::StringEncodingMask, 8),
            (Constant::OneByteStringTag, 8),
            (Constant::FirstNonstringType, 128),
            // A map's bit field 3 as 14.6 lays it out; every map the
            // tests write keeps it zero, with no descriptors of its own.
            (Constant::BitField3NumberOfOwnDescriptorsMask, 0x3ff << 12),
            (Constant::BitField3NumberOfOwnDescriptorsShift, 12),
            (Constant::BitField3IsDictionaryMapShift, 22),
            (Constant::FirstJSFunctionType, function),
            (Constant::LastJSFunctionType, function),
        ] {
            layout.set_constant(constant, value);
        }
        layout
    }

    const fn smi(value: u64) -> u64 {
        value << 32
    }

    /// Writes the objects of a chain into a heap, each at the next free
    /// block.
    struct Writer {
        heap: Heap,
        layout: V8Layout,
        next: u64,
        maps: HashMap<Constant, u64>,
        string_map: u64,
    }

    impl Writer {
        fn new() -> Self {
            let mut writer = Self {
                heap: Heap::default(),
                layout: layout(),
                next: 0x10_0000,
                maps: HashMap::new(),
                string_map: 0,
            };
            writer.string_map = writer.map_of_type(8);
            writer
        }

        fn offset(&self, field: Field) -> u64 {
            let offset = self.layout.field(field).expect("a field");
            assert!(offset + 8 <= BLOCK, "{field:?} is past a block");
            offset
        }

        /// A zeroed block; its address.
        fn block(&mut self) -> u64 {
            let at = self.next;
            self.next += BLOCK;
            self.heap
                .bytes(at, &vec![0; usize::try_from(BLOCK).expect("a block")]);
            at
        }

        fn map_of_type(&mut self, ty: u16) -> u64 {
            let at = self.block();
            self.heap.word(at, at + 1);
            let instance_type = self.offset(Field::MapInstanceType);
            self.heap.bytes(at + instance_type, &ty.to_le_bytes());
            at + 1
        }

        /// The map of objects of the instance type `ty`, made once.
        fn map(&mut self, ty: Constant) -> u64 {
            if let Some(map) = self.maps.get(&ty) {
                return *map;
            }
            let value = self.layout.constant(ty).expect("a type");
            let map = self.map_of_type(u16::try_from(value).expect("a type"));
            self.maps.insert(ty, map);
            map
        }

        /// An object of the instance type `ty`; tagged.
        fn object(&mut self, ty: Constant) -> u64 {
            let map = self.map(ty);
            let at = self.block();
            self.heap.word(at, map);
            at + 1
        }

        fn field(&mut self, object: u64, field: Field, word: u64) {
            let at = object - 1 + self.offset(field);
            self.heap.word(at, word);
        }

        /// A context of the instance type `ty` whose slot `index` holds
        /// `word`; tagged.
        fn context(&mut self, ty: Constant, index: Constant, word: u64) -> u64 {
            let context = self.object(ty);
            let slot = u64::try_from(self.layout.constant(index).expect("a slot")).expect("u64");
            let at = context - 1 + self.offset(Field::ContextElements) + 8 * slot;
            assert!(at + 8 <= context - 1 + BLOCK, "{index:?} is past a block");
            self.heap.word(at, word);
            context
        }

        /// A sequential one-byte string; tagged.
        fn string(&mut self, text: &str) -> u64 {
            let at = self.block();
            self.heap.word(at, self.string_map);
            let length = u32::try_from(text.len()).expect("a short string");
            let length_at = at + self.offset(Field::StringLength);
            self.heap.bytes(length_at, &length.to_le_bytes());
            let chars = at + self.offset(Field::SeqOneByteStringChars);
            self.heap.bytes(chars, text.as_bytes());
            at + 1
        }

        /// A function whose shared information is `shared`, in `context`;
        /// tagged. Its map keeps no descriptors and no prototype, so its
        /// name is its shared information's.
        fn function(&mut self, shared: u64, context: u64) -> u64 {
            let function = self.object(Constant::JSFunctionType);
            self.field(function, Field::JSFunctionSharedFunctionInfo, shared);
            self.field(function, Field::JSFunctionContext, context);
            function
        }

        /// The closure that runs the builtin numbered `number`, in
        /// `context`; tagged.
        fn builtin(&mut self, number: u64, context: u64) -> u64 {
            let shared = self.object(Constant::SharedFunctionInfoType);
            self.field(
                shared,
                Field::SharedFunctionInfoUntrustedFunctionData,
                smi(number),
            );
            self.function(shared, context)
        }

        /// A function of no script named `name`: native, which V8's stack
        /// traces show, or hidden; tagged.
        fn native(&mut self, name: &str, shown: bool) -> u64 {
            let shared = self.object(Constant::SharedFunctionInfoType);
            let name = self.string(name);
            self.field(shared, Field::SharedFunctionInfoNameOrScopeInfo, name);
            if shown {
                let native = self
                    .layout
                    .constant(Constant::SfiIsNativeMask)
                    .expect("a mask");
                let flags = u32::try_from(native).expect("a mask");
                let at = shared - 1 + self.offset(Field::SharedFunctionInfoFlags);
                self.heap.bytes(at, &flags.to_le_bytes());
            }
            self.function(shared, smi(0))
        }

        /// A pending promise whose reactions are `reactions`; tagged.
        fn promise(&mut self, reactions: u64) -> u64 {
            let promise = self.object(Constant::JSPromiseType);
            self.field(promise, Field::JSPromiseFlags, smi(0));
            self.field(promise, Field::JSPromiseReactionsOrResult, reactions);
            promise
        }

        /// A reaction whose next one is `next`, that calls `fulfill` and
        /// settles `settles`; tagged.
        fn reaction(&mut self, next: u64, fulfill: u64, settles: u64) -> u64 {
            let reaction = self.object(Constant::PromiseReactionType);
            self.field(reaction, Field::PromiseReactionNext, next);
            self.field(reaction, Field::PromiseReactionFulfillHandler, fulfill);
            self.field(reaction, Field::PromiseReactionRejectHandler, smi(0));
            self.field(reaction, Field::PromiseReactionPromiseOrCapability, settles);
            reaction
        }

        /// An async function's generator at `continuation`, running
        /// `function`, whose promise is `promise`, and the closure, in an
        /// await context of the generator's, that resumes it; both tagged.
        fn async_function(&mut self, function: u64, continuation: i64, promise: u64) -> (u64, u64) {
            let generator = self.object(Constant::JSAsyncFunctionObjectType);
            self.field(generator, Field::JSGeneratorObjectFunction, function);
            self.field(
                generator,
                Field::JSGeneratorObjectContinuation,
                signed_smi(continuation),
            );
            self.field(generator, Field::JSAsyncFunctionObjectPromise, promise);
            let context = self.context(
                Constant::AwaitContextType,
                Constant::ContextExtensionIndex,
                generator,
            );
            let closure = self.builtin(AWAIT_RESOLVE, context);
            self.field(
                generator,
                Field::JSAsyncFunctionObjectAwaitResolveClosure,
                closure,
            );
            (generator, closure)
        }

        /// The chain of the function `closure` resumed.
        fn walk(&self, closure: u64) -> Awaiting {
            let reader = Reader {
                stop: &self.heap,
                layout: &self.layout,
                version: VERSION,
                charge: None,
                blocks: None,
            };
            super::awaiting(
                &reader,
                &mut Table::default(),
                Encoding::of(VERSION),
                &Ok(NUMBERS),
                Object(closure),
            )
            .expect("a closure that resumes an async function")
        }
    }

    /// The Smi word of the signed `value`.
    fn signed_smi(value: i64) -> u64 {
        value.cast_unsigned() << 32
    }

    /// The running async function, whose promise is `promise`, and the
    /// closure that resumed it.
    fn running(writer: &mut Writer, promise: u64) -> u64 {
        let function = writer.native("inner", true);
        let (_, closure) = writer.async_function(function, -2, promise);
        closure
    }

    /// The chain's functions and generators, and why it ends.
    fn shown(chain: &Awaiting) -> (Vec<(String, u64)>, Option<String>) {
        (
            chain
                .frames
                .iter()
                .map(|frame| {
                    (
                        frame.function.as_deref().unwrap_or("?").to_owned(),
                        frame.frame.get(),
                    )
                })
                .collect(),
            chain.end.as_deref().map(str::to_owned),
        )
    }

    /// Two async functions, and a hidden one between them, suspended in
    /// await: the chain names the two, innermost first, each at its
    /// generator, and ends complete at a promise nothing waits for.
    #[test]
    fn a_chain_of_two_suspended_functions_ends_where_nothing_waits() {
        let mut writer = Writer::new();
        let last = writer.promise(smi(0));
        let outer = writer.native("outer", true);
        let (outer, outer_closure) = writer.async_function(outer, 1, last);
        let to_outer = writer.reaction(smi(0), outer_closure, smi(0));
        let hidden_promise = writer.promise(to_outer);
        let hidden = writer.native("hidden", false);
        let (_, hidden_closure) = writer.async_function(hidden, 0, hidden_promise);
        let to_hidden = writer.reaction(smi(0), hidden_closure, smi(0));
        let middle_promise = writer.promise(to_hidden);
        let middle = writer.native("middle", true);
        let (middle, middle_closure) = writer.async_function(middle, 3, middle_promise);
        let to_middle = writer.reaction(smi(0), middle_closure, smi(0));
        let first = writer.promise(to_middle);
        let closure = running(&mut writer, first);

        let chain = writer.walk(closure);
        assert_eq!(
            shown(&chain),
            (
                vec![("middle".to_owned(), middle), ("outer".to_owned(), outer)],
                None
            )
        );
        assert!(chain.frames.iter().all(|frame| frame.problem.is_none()));
    }

    /// A chain of `count` promises, each settled by the reaction of the
    /// one before, the first the running function's; the closure.
    fn line_of(writer: &mut Writer, count: usize) -> u64 {
        let mut next = writer.promise(smi(0));
        for _ in 1..count {
            let reaction = writer.reaction(smi(0), smi(0), next);
            next = writer.promise(reaction);
        }
        running(writer, next)
    }

    /// The walk takes at most `MAX_STEPS` steps: a chain of that many
    /// promises ends complete, and one more promise is too long.
    #[test]
    fn a_chain_longer_than_its_steps_says_so() {
        assert_eq!(MAX_STEPS, 256);
        let mut writer = Writer::new();
        let closure = line_of(&mut writer, 256);
        assert_eq!(shown(&writer.walk(closure)), (Vec::new(), None));

        let mut writer = Writer::new();
        let closure = line_of(&mut writer, 257);
        assert_eq!(
            shown(&writer.walk(closure)),
            (
                Vec::new(),
                Some("the await chain is longer than 256 promises".to_owned())
            )
        );
    }

    /// Two promises each settled by the other's reaction are a cycle.
    #[test]
    fn a_cycle_ends_the_chain() {
        let mut writer = Writer::new();
        let first = writer.promise(smi(0));
        let back = writer.reaction(smi(0), smi(0), first);
        let second = writer.promise(back);
        let forth = writer.reaction(smi(0), smi(0), second);
        writer.field(first, Field::JSPromiseReactionsOrResult, forth);
        let closure = running(&mut writer, first);
        assert_eq!(
            shown(&writer.walk(closure)),
            (Vec::new(), Some("the await chain is a cycle".to_owned()))
        );
    }

    /// A promise several reactions wait for ends the chain, with why, as
    /// V8 follows none of them.
    #[test]
    fn several_reactions_end_the_chain() {
        let mut writer = Writer::new();
        let last = writer.promise(smi(0));
        let second = writer.reaction(smi(0), smi(0), last);
        let first = writer.reaction(second, smi(0), last);
        let promise = writer.promise(first);
        let closure = running(&mut writer, promise);
        assert_eq!(
            shown(&writer.walk(closure)),
            (
                Vec::new(),
                Some("several reactions wait for the promise, and V8 follows only one".to_owned())
            )
        );
    }

    /// A pending promise whose reactions are some other object ends the
    /// chain, with why.
    #[test]
    fn reactions_that_are_no_reaction_end_the_chain() {
        let mut writer = Writer::new();
        let other = writer.promise(smi(0));
        let promise = writer.promise(other);
        let closure = running(&mut writer, promise);
        assert_eq!(
            shown(&writer.walk(closure)),
            (
                Vec::new(),
                Some("a pending promise's reactions are no PromiseReaction".to_owned())
            )
        );
    }

    /// A reaction that settles a capability whose promise cannot be read
    /// ends the chain with the read's failure, never as complete.
    #[test]
    fn an_unreadable_next_promise_ends_the_chain_with_its_reason() {
        let mut writer = Writer::new();
        let capability = writer.object(Constant::PromiseCapabilityType);
        writer.field(capability, Field::PromiseCapabilityPromise, UNREADABLE + 1);
        let reaction = writer.reaction(smi(0), smi(0), capability);
        let promise = writer.promise(reaction);
        let closure = running(&mut writer, promise);
        let (frames, end) = shown(&writer.walk(closure));
        assert_eq!(frames, Vec::new());
        let end = end.expect("a chain that ends at what cannot be read");
        assert!(end.contains(&format!("{UNREADABLE:#x}")), "{end}");
    }

    /// An async generator whose request cannot be read ends the chain
    /// with the read's failure, never as complete.
    #[test]
    fn an_unreadable_generator_request_ends_the_chain_with_its_reason() {
        let mut writer = Writer::new();
        let function = writer.native("produce", true);
        let generator = writer.object(Constant::JSAsyncGeneratorObjectType);
        writer.field(generator, Field::JSGeneratorObjectFunction, function);
        writer.field(
            generator,
            Field::JSGeneratorObjectContinuation,
            signed_smi(-2),
        );
        writer.field(
            generator,
            Field::JSAsyncGeneratorObjectQueue,
            UNREADABLE + 1,
        );
        let context = writer.context(
            Constant::AwaitContextType,
            Constant::ContextExtensionIndex,
            generator,
        );
        let closure = writer.builtin(GENERATOR_AWAIT_RESOLVE, context);
        let (frames, end) = shown(&writer.walk(closure));
        assert_eq!(frames, Vec::new());
        let end = end.expect("a chain that ends at what cannot be read");
        assert!(end.contains(&format!("{UNREADABLE:#x}")), "{end}");
    }

    /// A bytecode array longer than any the reader reads is refused
    /// before its bytes are: none of them is in the heap, so a read would
    /// fail otherwise.
    #[test]
    fn an_implausibly_long_bytecode_array_is_refused() {
        let mut writer = Writer::new();
        let length = u64::try_from(MAX_BYTECODE).expect("u64") + 1;
        let bytecode = writer.object(Constant::BytecodeArrayType);
        writer.field(bytecode, Field::BytecodeArrayLength, smi(length));
        let shared = writer.object(Constant::SharedFunctionInfoType);
        writer.field(
            shared,
            Field::SharedFunctionInfoTrustedFunctionData,
            bytecode,
        );
        // V8 keeps the offset from the array's tagged address: the last
        // byte, past the header less the tag.
        let header = writer.offset(Field::BytecodeArrayBytes);
        let generator = writer.object(Constant::JSAsyncFunctionObjectType);
        writer.field(
            generator,
            Field::JSGeneratorObjectInputOrDebugPos,
            smi(length - 1 + header - 1),
        );
        let reader = Reader {
            stop: &writer.heap,
            layout: &writer.layout,
            version: VERSION,
            charge: None,
            blocks: None,
        };
        let problem = super::saved_registers(&reader, Object(generator), Object(shared))
            .expect_err("a bytecode array past the bound");
        assert_eq!(&*problem, "a bytecode array is implausibly long");
    }
}
