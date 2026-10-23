//! An error's stack formatted from the raw call sites V8 captured, as Node
//! formats them when `stack` is first read (Q8).
//!
//! V8's getter hands the call sites to Node's `prepareStackTraceCallback`
//! (`lib/internal/errors.js`), which calls the realm's
//! `Error.prepareStackTrace`, by default Node's `ErrorPrepareStackTrace`,
//! which calls the module's `internalPrepareStackTrace`, by default
//! `defaultPrepareStackTrace`: the error's own line, then each call site
//! as V8's `CallSite.prototype.toString` writes it (`SerializeCallSiteInfo`
//! in `objects/call-site-info.cc`), each after `\n    at `. Every step is
//! proven to be the default one before its text is written; any other is
//! refused by name, as are call sites of WebAssembly and of V8's builtins.
//! A call site that cannot be read says so in its own line.

use std::cell::RefCell;
use std::collections::BTreeMap;

use super::access;
use super::compiled;
use super::constructors;
use super::frames;
use super::inspect::{JsString, Value};
use super::lookup::{self, Found, Key};
use super::names;
use super::objects::{MAX_STRING, Object, Problem, Reader};
use super::own::{self, Held};
use super::positions::{self, Encoding};
use super::properties;
use super::values::Builder;
use super::variables;
use crate::image::v8::{Constant, Field};
use crate::runtime_model::StopMemory;

/// The most prototypes followed: chains are a few objects long.
const MAX_PROTOTYPES: usize = 64;

/// The most contexts a closure's chain is followed through.
const MAX_CONTEXTS: usize = 256;

/// The most scripts an `eval` origin names, each an `eval` of the next.
const MAX_EVALS: usize = 64;

/// The most call sites read of one error; `Error.stackTraceLimit` keeps
/// ten by default.
const MAX_SITES: i64 = 1 << 16;

/// The script Node's own error formatting is compiled from.
const NODE_ERRORS: &str = "node:internal/errors";

/// The UTF-16 units of a string.
type Units = Vec<u16>;

/// The text Node would give `error.stack`, from its call sites `sites`.
pub(super) fn format(
    builder: &Builder<'_>,
    error: Object,
    sites: Object,
) -> Result<Value, Problem> {
    let reader = builder.reader;
    let realm = Realm::of(&reader, error)?;
    let node = Node::find(&reader, &realm)?;
    realm.check_call_site_to_string(&reader)?;
    let sites = read_sites(&reader, sites)?;
    let mut text = node.header(&reader, error)?;
    let sources = Sources::default();
    for site in sites {
        push(&mut text, "\n    at ");
        let frame = site.and_then(|site| Frame::new(&reader, &sources, site)?.serialize());
        match frame {
            Ok(line) => text.extend(line),
            Err(problem) => push(
                &mut text,
                &format!("<microscope cannot write this call site: {problem}>"),
            ),
        }
    }
    Ok(Value::String(JsString::complete(text)))
}

fn push(text: &mut Units, more: &str) {
    text.extend(more.encode_utf16());
}

/// The realm an error was made in: its native context and global object.
struct Realm {
    native: Object,
    global: Object,
}

impl Realm {
    /// The realm of `error`, whose map's meta map keeps its native context,
    /// as V8's `GetCreationContext` finds it for an error.
    fn of(reader: &Reader<'_>, error: Object) -> Result<Self, Problem> {
        let native = access::native_context(reader, reader.map(error)?)?;
        let global = reader
            .object(access::slot(
                reader,
                native,
                Constant::ContextExtensionIndex,
            )?)?
            .filter(|global| {
                reader
                    .is(*global, Constant::JSGlobalObjectType)
                    .unwrap_or(false)
            })
            .ok_or("the error's realm has no global object")?;
        Ok(Self { native, global })
    }

    /// Refuses a realm whose `CallSite.prototype.toString`, which Node's
    /// `ArrayPrototypeJoin` calls on each call site, is not V8's builtin.
    fn check_call_site_to_string(&self, reader: &Reader<'_>) -> Result<(), Problem> {
        let constructor = reader
            .object(access::slot(
                reader,
                self.native,
                Constant::ContextCallSiteFunctionIndex,
            )?)?
            .ok_or("the realm has no CallSite constructor")?;
        let prototype = match lookup::get(reader, constructor, Key::Name("prototype"))? {
            Some(lookup::Value::Word(word)) => reader.object(word)?,
            _ => None,
        }
        .ok_or("the realm's CallSite constructor has no prototype")?;
        if let Some((Held::Data(word), _)) = lookup::own(reader, prototype, Key::Name("toString"))?
            && let Some(function) = reader.object(word)?
            && lookup::is_builtin(reader, function, "toString")?
        {
            return Ok(());
        }
        Err(
            "the realm's CallSite.prototype.toString is not V8's, and microscope does not call it"
                .into(),
        )
    }
}

/// `globalThis.Error`: found as V8's `HashTable::FindEntry` finds it in
/// the global object's dictionary, by the hash of the name `%Error%` keeps,
/// which is the internalized "Error"; and else by [`lookup::find`], which
/// reads every property the global object keeps on the way.
fn global_error(reader: &Reader<'_>, realm: &Realm) -> Result<Found, Problem> {
    if let Some(found) = probe_global(reader, realm)? {
        return Ok(found);
    }
    lookup::find(reader, realm.global, Key::Name("Error"))
}

/// The global object's own `Error`, where a probe of its dictionary by
/// the name's hash finds it; `None` where it does not, which proves no
/// absence, as no probe of a table read without its hash function does.
fn probe_global(reader: &Reader<'_>, realm: &Realm) -> Result<Option<Found>, Problem> {
    let Some(function) = reader
        .object(access::slot(
            reader,
            realm.native,
            Constant::ContextErrorFunctionIndex,
        )?)?
        .filter(|function| {
            reader
                .is(*function, Constant::JSFunctionType)
                .unwrap_or(false)
        })
    else {
        return Ok(None);
    };
    let shared = shared(reader, function)?;
    let Some(name) = reader
        .object(reader.field(shared, Field::SharedFunctionInfoNameOrScopeInfo)?)?
        .filter(|name| is_string(reader, *name).unwrap_or(false))
    else {
        return Ok(None);
    };
    if whole_string(reader, name)? != "Error".encode_utf16().collect::<Units>() {
        return Ok(None);
    }
    let raw = i64::from(
        reader
            .int32_field(name, Field::NameRawHashField)?
            .cast_unsigned(),
    );
    if raw & reader.constant(Constant::NameHashFieldTypeMask)?
        != reader.constant(Constant::NameHashFieldTypeHash)?
    {
        return Ok(None);
    }
    let hash = raw >> reader.constant(Constant::NameHashShift)?;

    let Some(table) = reader
        .object(reader.field(realm.global, Field::JSReceiverPropertiesOrHash)?)?
        .filter(|table| {
            reader
                .is(*table, Constant::GlobalDictionaryType)
                .unwrap_or(false)
        })
    else {
        return Ok(None);
    };
    let capacity = reader
        .smi(reader.element(table, reader.constant(Constant::HashTableCapacityIndex)?)?)?
        .filter(|capacity| *capacity > 0 && capacity.count_ones() == 1 && *capacity <= MAX_PROBES)
        .ok_or_else(|| format!("the global dictionary {:#x} has no capacity", table.0))?;
    let first = reader.constant(Constant::HashTablePrefixStartIndex)?
        + reader.constant(Constant::GlobalDictionaryShapePrefixSize)?;
    let size = reader.constant(Constant::GlobalDictionaryShapeEntrySize)?;
    let key = reader.constant(Constant::HashTableEntryKeyIndex)?;
    // `FirstProbe` and `NextProbe`; a global dictionary spreads no hash.
    let mut entry = hash & (capacity - 1);
    for count in 1..=capacity {
        let word = reader.element(table, first + entry * size + key)?;
        if is_undefined(reader, word)? {
            return Ok(None);
        }
        if let Some(cell) = reader.object(word)?
            && reader.is(cell, Constant::PropertyCellType)?
            && reader.field(cell, Field::PropertyCellName)? == name.0
        {
            let value = reader.field(cell, Field::PropertyCellValue)?;
            if own::is_hole(reader, value)? {
                return Ok(None);
            }
            let details = reader
                .smi(reader.field(cell, Field::PropertyCellPropertyDetailsRaw)?)?
                .ok_or("a property cell's details are no Smi")?;
            if properties::part(reader, details, Constant::PropKindMask, None)?
                == reader.constant(Constant::PropKindAccessor)?
            {
                return Err(
                    "the realm's global Error is a getter, which microscope does not call".into(),
                );
            }
            return Ok(Some(Found::Data(value)));
        }
        entry = (entry + count) & (capacity - 1);
    }
    Ok(None)
}

/// The most entries a global dictionary is probed through.
const MAX_PROBES: i64 = 1 << 20;

/// What Node's default formatting needs of its `internal/errors` module.
struct Node {
    /// The module's `kIsNodeError`, the symbol a Node error has.
    is_node_error: Object,
}

impl Node {
    /// Node's formatting of the errors of `realm`, proven to be its
    /// default: `globalThis.Error.prepareStackTrace` is Node's own
    /// `ErrorPrepareStackTrace`, whose module formats with
    /// `defaultPrepareStackTrace` and overrides no error's stack.
    fn find(reader: &Reader<'_>, realm: &Realm) -> Result<Self, Problem> {
        // `globalThis.Error?.prepareStackTrace`, in `prepareStackTraceCallback`.
        let constructor = match global_error(reader, realm)? {
            Found::Data(word) => reader
                .object(word)?
                .filter(|object| is_receiver(reader, *object).unwrap_or(false)),
            Found::Accessor { .. } => {
                return Err(
                    "the realm's global Error is a getter, which microscope does not call".into(),
                );
            }
            Found::Absent => None,
        }
        .ok_or(
            "the realm's global Error is no object, so Node formats the stack with the main \
             realm's Error.prepareStackTrace, which microscope does not follow",
        )?;
        let prepare = match lookup::find(reader, constructor, Key::Name("prepareStackTrace"))? {
            Found::Data(word) => reader.object(word)?,
            Found::Accessor { .. } => {
                return Err(
                    "Error.prepareStackTrace is a getter, which microscope does not call".into(),
                );
            }
            Found::Absent => None,
        }
        .filter(|function| lookup::is_callable(reader, *function).unwrap_or(false))
        .ok_or(
            "the realm's Error.prepareStackTrace is no function, so Node formats the stack with \
             the main realm's, which microscope does not follow",
        )?;
        if !is_node_function(reader, prepare, "ErrorPrepareStackTrace")? {
            return Err(
                "a custom Error.prepareStackTrace formats the stack, which microscope does not run"
                    .into(),
            );
        }

        let locals = closure_locals(
            reader,
            prepare,
            &[
                "internalPrepareStackTrace",
                "kIsNodeError",
                "overrideStackTrace",
            ],
        )?;
        let local = |name: &str| -> Result<Object, Problem> {
            locals
                .get(name)
                .copied()
                .map(|word| reader.object(word))
                .transpose()?
                .flatten()
                .ok_or_else(|| format!("Node's {name} is no object").into())
        };

        let internal = local("internalPrepareStackTrace")?;
        if !is_node_function(reader, internal, "defaultPrepareStackTrace")? {
            return Err(
                if is_node_function(reader, internal, "prepareStackTraceWithSourceMaps")? {
                    "source maps are enabled, and microscope does not map a stack's positions \
                     through them"
                } else {
                    "Node formats stacks with an internal prepareStackTrace other than its \
                     default, which microscope does not run"
                }
                .into(),
            );
        }

        // `prepareStackTraceCallback` first asks whether Node overrides
        // this error's stack; one that overrides any is refused.
        let overrides = local("overrideStackTrace")?;
        if !reader.is(overrides, Constant::JSWeakMapType)? {
            return Err("Node's overrideStackTrace is no WeakMap".into());
        }
        let table = reader.typed_field(
            overrides,
            Field::JSWeakCollectionTable,
            Constant::EphemeronHashTableType,
        )?;
        let length = reader.smi_field(table, Field::FixedArrayBaseLength)?;
        let index = reader.constant(Constant::HashTableNumberOfElementsIndex)?;
        if !(0..length).contains(&index) {
            return Err("Node's overrideStackTrace has no count of its entries".into());
        }
        let count = reader.word(
            reader.field_address(table, Field::FixedArrayObjects)? + index.cast_unsigned() * 8,
        )?;
        if reader.smi(count)? != Some(0) {
            return Err(
                "Node overrides the stack of some error, which microscope does not follow".into(),
            );
        }

        let is_node_error = local("kIsNodeError")?;
        if !reader.is(is_node_error, Constant::SymbolType)? {
            return Err("Node's kIsNodeError is no symbol".into());
        }
        Ok(Self { is_node_error })
    }

    /// The first line `defaultPrepareStackTrace` writes of `error`:
    /// ``${name} [${code}]: ${message}`` for a Node error, otherwise
    /// `Error.prototype.toString`'s.
    fn header(&self, reader: &Reader<'_>, error: Object) -> Result<Units, Problem> {
        if has_symbol(reader, error, self.is_node_error)? {
            let mut text = Units::new();
            let part = |name| -> Result<Units, Problem> {
                Ok(get(reader, error, name)?
                    .unwrap_or_else(|| "undefined".encode_utf16().collect()))
            };
            text.extend(part("name")?);
            push(&mut text, " [");
            text.extend(part("code")?);
            push(&mut text, "]: ");
            text.extend(part("message")?);
            return Ok(text);
        }

        // V8's `ErrorUtils::ToString`, as `Error.prototype.toString` calls it.
        let name = get(reader, error, "name")?.unwrap_or_else(|| "Error".encode_utf16().collect());
        let message = get(reader, error, "message")?.unwrap_or_default();
        if name.is_empty() {
            return Ok(message);
        }
        if message.is_empty() {
            return Ok(name);
        }
        let mut text = name;
        push(&mut text, ": ");
        text.extend(message);
        Ok(text)
    }
}

/// `ToString(object[name])`, with no getter called; `None` for undefined.
fn get(reader: &Reader<'_>, object: Object, name: &str) -> Result<Option<Units>, Problem> {
    match lookup::get(reader, object, Key::Name(name))? {
        None => Ok(None),
        Some(lookup::Value::Text(units)) => Ok(Some(units)),
        Some(lookup::Value::Word(word)) => to_string(reader, word),
    }
}

/// `ToString` of the value `word`, where no program code runs for it;
/// `None` for undefined.
fn to_string(reader: &Reader<'_>, word: u64) -> Result<Option<Units>, Problem> {
    if let Some(integer) = reader.smi(word)? {
        return Ok(Some(integer.to_string().encode_utf16().collect()));
    }
    let object = reader
        .object(word)?
        .ok_or("a value is neither a Smi nor an object")?;
    if is_string(reader, object)? {
        return whole_string(reader, object).map(Some);
    }
    let text = if reader.is(object, Constant::OddballType)? {
        let kind = reader.smi_field(object, Field::OddballKind)?;
        let is = |constant| reader.constant(constant).ok() == Some(kind);
        if is(Constant::OddballUndefined) {
            return Ok(None);
        }
        if is(Constant::OddballNull) {
            "null"
        } else if is(Constant::OddballTrue) {
            "true"
        } else if is(Constant::OddballFalse) {
            "false"
        } else {
            return Err(format!("an oddball of kind {kind}, which is no value").into());
        }
        .to_owned()
    } else if reader.is(object, Constant::HeapNumberType)? {
        let value =
            f64::from_bits(reader.word(reader.field_address(object, Field::HeapNumberValue)?)?);
        number_to_string(value)?
    } else if reader.is(object, Constant::SymbolType)? {
        return Err("a symbol, which ToString throws on".into());
    } else {
        return Err(
            "a value whose string V8 computes by calling its methods, which microscope does not"
                .into(),
        );
    };
    Ok(Some(text.encode_utf16().collect()))
}

/// `Number::toString` of `value` where it is a whole number below 10^21, or
/// NaN or an infinity; any other is refused.
fn number_to_string(value: f64) -> Result<String, Problem> {
    if value.is_nan() {
        return Ok("NaN".to_owned());
    }
    if value.is_infinite() {
        return Ok(if value > 0.0 { "Infinity" } else { "-Infinity" }.to_owned());
    }
    if value.fract() == 0.0 && value.abs() < 1e21 {
        #[expect(
            clippy::cast_possible_truncation,
            reason = "a whole number below 10^21 fits an i128"
        )]
        return Ok((value as i128).to_string());
    }
    Err("a fractional number, whose shortest form microscope does not write".into())
}

/// Whether `symbol` is a property of `object` or its prototypes, as `in`
/// asks; a proxy's `has` trap is refused.
fn has_symbol(reader: &Reader<'_>, object: Object, symbol: Object) -> Result<bool, Problem> {
    let mut holder = object;
    for _ in 0..MAX_PROTOTYPES {
        if reader.is(holder, Constant::JSProxyType)? {
            return Err("a proxy, whose has trap microscope does not call".into());
        }
        if own::all_named(reader, holder)?
            .iter()
            .any(|property| property.key == symbol)
        {
            return Ok(true);
        }
        match lookup::prototype(reader, holder)? {
            Some(next) => holder = next,
            None => return Ok(false),
        }
    }
    Err(format!("the prototypes of {:#x} are too many", object.0).into())
}

/// Whether `function` is the function `name` of Node's `internal/errors`.
fn is_node_function(reader: &Reader<'_>, function: Object, name: &str) -> Result<bool, Problem> {
    if !is_function(reader, function)? {
        return Ok(false);
    }
    let shared = shared(reader, function)?;
    let named = match names::shared_name(reader, shared)? {
        (Some(own), _) => properties::is_named(reader, own.0, name)?,
        (None, _) => false,
    };
    let Some(script) = frames::script(reader, shared)? else {
        return Ok(false);
    };
    Ok(named
        && properties::is_named(
            reader,
            reader.field(script, Field::ScriptName)?,
            NODE_ERRORS,
        )?)
}

/// The words of the variables `wanted` that `function`'s closure sees, each
/// as the innermost context that binds it holds it.
fn closure_locals(
    reader: &Reader<'_>,
    function: Object,
    wanted: &[&str],
) -> Result<BTreeMap<String, u64>, Problem> {
    let mut context = reader
        .object(reader.field(function, Field::JSFunctionContext)?)?
        .ok_or("a function's context is no object")?;
    let mut found = BTreeMap::new();
    for _ in 0..MAX_CONTEXTS {
        if reader.is(context, Constant::NativeContextType)? {
            break;
        }
        for (name, word) in variables::locals(reader, context)? {
            if wanted.contains(&name.as_str()) && !found.contains_key(&name) {
                found.insert(name, word?);
            }
        }
        context = reader
            .object(access::slot(
                reader,
                context,
                Constant::ContextPreviousIndex,
            )?)?
            .ok_or("a context's previous context is no object")?;
    }
    if let Some(missing) = wanted.iter().find(|name| !found.contains_key(**name)) {
        return Err(format!("Node's {NODE_ERRORS} has no {missing}").into());
    }
    Ok(found)
}

/// What V8 captured of one call site.
#[derive(Debug, Clone, Copy)]
struct Site {
    receiver: u64,
    function: u64,

    /// The code its offset is in: a `BytecodeArray`, a `Code`, or neither.
    code: Option<Object>,

    /// The offset in `code`, or the source position, or a promise's index.
    offset: i64,
    flags: i64,
}

/// The call sites `sites` holds, each read or with why it cannot be: from
/// 14.6 the raw data `CallSiteInfo::ConstructFromRawData` reads, with its
/// baseline frames resolved as `ExpandDeferredFrames` resolves them;
/// before, `CallSiteInfo` objects.
fn read_sites(reader: &Reader<'_>, sites: Object) -> Result<Vec<Result<Site, Problem>>, Problem> {
    let length = reader.smi_field(sites, Field::FixedArrayBaseLength)?;
    if !(0..=MAX_SITES).contains(&length) {
        return Err(format!("an error keeps {length} call sites").into());
    }
    // Node's patch that keeps raw data came after V8 14.6 did: Node 26.5,
    // of the same V8, keeps `CallSiteInfo` objects, as V8 does before.
    let objects = length == 0
        || reader
            .object(reader.element(sites, 0)?)?
            .map(|first| reader.is(first, Constant::CallSiteInfoType))
            .transpose()?
            .unwrap_or(false);
    if objects
        || !reader
            .layout
            .has(Field::ErrorStackDataRawDataForCallSiteInfosOrFormattedStack)
    {
        return Ok((0..length)
            .map(|index| call_site_info(reader, reader.element(sites, index)?))
            .collect());
    }
    let count = reader.constant(Constant::CallSiteInfoRawCount)?;
    if count <= 0 {
        return Err("a raw call site has no fields".into());
    }
    Ok((0..length / count)
        .map(|index| raw_site(reader, sites, index * count))
        .collect())
}

/// The call site the `CallSiteInfo` `word` holds.
fn call_site_info(reader: &Reader<'_>, word: u64) -> Result<Site, Problem> {
    let info = reader
        .object(word)?
        .filter(|info| {
            reader
                .is(*info, Constant::CallSiteInfoType)
                .unwrap_or(false)
        })
        .ok_or("a call site is no CallSiteInfo")?;
    let code = reader.field(info, Field::CallSiteInfoCodeObject)?;
    Ok(Site {
        receiver: reader.field(info, Field::CallSiteInfoReceiverOrInstance)?,
        function: reader.field(info, Field::CallSiteInfoFunction)?,
        code: reader
            .object(code)?
            .filter(|code| !reader.is(*code, Constant::OddballType).unwrap_or(true)),
        offset: reader.smi_field(info, Field::CallSiteInfoCodeOffsetOrSourcePosition)?,
        flags: reader.smi_field(info, Field::CallSiteInfoFlags)?,
    })
}

/// The call site whose raw data begins at `base` of `sites`.
fn raw_site(reader: &Reader<'_>, sites: Object, base: i64) -> Result<Site, Problem> {
    let at =
        |slot| -> Result<u64, Problem> { reader.element(sites, base + reader.constant(slot)?) };
    let smi = |slot| -> Result<i64, Problem> {
        reader
            .smi(at(slot)?)?
            .ok_or_else(|| format!("a call site's {} is no Smi", Constant::name(slot)).into())
    };
    let wrapper = reader
        .object(at(Constant::CallSiteInfoRawCode)?)?
        .ok_or("a call site's code is no object")?;
    let code = if reader.is(wrapper, Constant::CodeWrapperType)? {
        Some(reader.typed_field(wrapper, Field::CodeWrapperCode, Constant::CodeType)?)
    } else if reader.is(wrapper, Constant::BytecodeWrapperType)? {
        Some(reader.typed_field(
            wrapper,
            Field::BytecodeWrapperBytecode,
            Constant::BytecodeArrayType,
        )?)
    } else if reader.is(wrapper, Constant::OddballType)? {
        None
    } else {
        return Err(format!(
            "a call site's code is no code but of instance type {}",
            reader.instance_type(wrapper)?
        )
        .into());
    };
    let mut site = Site {
        receiver: at(Constant::CallSiteInfoRawReceiver)?,
        function: at(Constant::CallSiteInfoRawFunction)?,
        code,
        offset: smi(Constant::CallSiteInfoRawOffset)?,
        flags: smi(Constant::CallSiteInfoRawFlags)?,
    };

    let deferred = reader.constant(Constant::CallSiteInfoIsDeferredBaselineFrameMask)?;
    if site.flags & deferred != 0 {
        // `ExpandDeferredFrames`: the baseline code's pc offset becomes the
        // offset of its bytecode, and the code that bytecode.
        let code = compiled::code(reader, code.ok_or("a deferred call site has no code")?)?;
        if code.kind != reader.constant(Constant::CodeKindBaseline)? {
            return Err("a deferred call site's code is not baseline code".into());
        }
        let function = function_of(reader, site.function)?;
        let own = names::bytecode(reader, shared(reader, function)?)?;
        let pc = u64::try_from(site.offset).map_err(|_| "a negative pc offset")?;
        let (bytecode, offset) = compiled::baseline_offset(reader, &code, pc)?;
        if own != Some(bytecode) {
            return Err("a baseline call site's bytecode is not its function's".into());
        }
        site.code = Some(bytecode);
        site.offset = offset;
        site.flags &= !deferred;
    }
    Ok(site)
}

/// The `JSFunction` the word `word` points to.
fn function_of(reader: &Reader<'_>, word: u64) -> Result<Object, Problem> {
    reader
        .object(word)?
        .filter(|function| is_function(reader, *function).unwrap_or(false))
        .ok_or_else(|| "a call site's function is no JSFunction".into())
}

/// The flags of a call site that serializing it reads.
#[derive(Debug, Clone, Copy)]
struct Flags {
    constructor: bool,
    is_async: bool,
    position_computed: bool,
}

impl Flags {
    fn of(reader: &Reader<'_>, flags: i64) -> Result<Self, Problem> {
        let bit =
            |mask: Constant| -> Result<bool, Problem> { Ok(flags & reader.constant(mask)? != 0) };
        if bit(Constant::CallSiteInfoIsWasmMask)? || bit(Constant::CallSiteInfoIsAsmJsWasmMask)? {
            return Err("a WebAssembly call site, which microscope does not write".into());
        }
        if bit(Constant::CallSiteInfoIsBuiltinMask)? {
            return Err("a call site in V8's own builtins, which microscope does not write".into());
        }
        Ok(Self {
            constructor: bit(Constant::CallSiteInfoIsConstructorMask)?,
            is_async: bit(Constant::CallSiteInfoIsAsyncMask)?,
            position_computed: bit(Constant::CallSiteInfoIsSourcePositionComputedMask)?,
        })
    }
}

/// A line and a column, each from 0, as `Script::PositionInfo` has them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Position {
    line: i64,
    column: i64,
}

/// One JavaScript call site, as V8's `CallSiteInfo` methods read it.
struct Frame<'r> {
    reader: &'r Reader<'r>,
    sources: &'r Sources,
    site: Site,
    flags: Flags,
    function: Object,
    shared: Object,
    script: Option<Object>,
}

impl<'r> Frame<'r> {
    fn new(reader: &'r Reader<'r>, sources: &'r Sources, site: Site) -> Result<Self, Problem> {
        let flags = Flags::of(reader, site.flags)?;
        let function = function_of(reader, site.function)?;
        let shared = shared(reader, function)?;
        Ok(Self {
            reader,
            sources,
            site,
            flags,
            function,
            shared,
            script: frames::script(reader, shared)?,
        })
    }

    /// `SerializeJSStackFrame`.
    fn serialize(&self) -> Result<Units, Problem> {
        let function_name = self.function_name()?;
        let mut text = Units::new();
        if self.flags.is_async {
            push(&mut text, "async ");
            if self.is_promise_combinator()? {
                if !function_name.is_empty() {
                    push(&mut text, "Promise.");
                    text.extend(&function_name);
                } else if self.reader.version.major >= 14 {
                    push(&mut text, "<anonymous>");
                } else {
                    return Err("a promise combinator with no name, which V8 cannot write".into());
                }
                push(&mut text, &format!(" (index {})", self.source_position()?));
                return Ok(text);
            }
        }
        if self.is_method_call()? {
            self.append_method_call(&mut text, &function_name)?;
        } else if self.flags.constructor {
            push(&mut text, "new ");
            if function_name.is_empty() {
                push(&mut text, "<anonymous>");
            } else {
                text.extend(&function_name);
            }
        } else if !function_name.is_empty() {
            text.extend(&function_name);
        } else {
            self.append_file_location(&mut text)?;
            return Ok(text);
        }
        push(&mut text, " (");
        self.append_file_location(&mut text)?;
        push(&mut text, ")");
        Ok(text)
    }

    /// `AppendMethodCall`.
    fn append_method_call(&self, text: &mut Units, function_name: &[u16]) -> Result<(), Problem> {
        if function_name.is_empty() {
            let type_name = self.type_name()?;
            if !type_name.is_empty() {
                text.extend(type_name);
                push(text, ".");
            }
            match self.method_name()? {
                Some(method) if !method.is_empty() => text.extend(method),
                _ => push(text, "<anonymous>"),
            }
            return Ok(());
        }
        if is_identifier(function_name)? {
            let type_name = self.type_name()?;
            if !type_name.is_empty() && type_name != function_name {
                text.extend(type_name);
                push(text, ".");
            }
        }
        text.extend(function_name);
        if let Some(method) = self.method_name()?
            && !method.is_empty()
            && !ends_with_method_name(function_name, &method)
        {
            push(text, " [as ");
            text.extend(method);
            push(text, "]");
        }
        Ok(())
    }

    /// `AppendFileLocation`.
    fn append_file_location(&self, text: &mut Units) -> Result<(), Problem> {
        let name = match self.script {
            Some(script) => name_or_source_url(self.reader, script)?,
            None => None,
        };
        if name.is_none()
            && let Some(script) = self.script
            && is_eval(self.reader, script)?
        {
            text.extend(eval_origin(self.reader, self.sources, script, 0)?);
            push(text, ", ");
        }
        match name {
            Some(name) if !name.is_empty() => text.extend(name),
            _ => push(text, "<anonymous>"),
        }
        let Some(script) = self.script else {
            return Ok(());
        };
        let position = self.source_position()?;
        let info = position_info(self.reader, self.sources, script, position, true)?;
        let url = has_source_url_comment(self.reader, script)?;
        let line_offset = self.reader.smi_field(script, Field::ScriptLineOffset)?;
        // `GetLineNumber` and `GetColumnNumber`: 0 is no line, or no column.
        let mut line = info.line + 1;
        if url {
            line -= line_offset;
        }
        if line == 0 {
            return Ok(());
        }
        push(text, &format!(":{line}"));
        let mut column = info.column + 1;
        if url && info.line == line_offset {
            column -= self.reader.smi_field(script, Field::ScriptColumnOffset)?;
        }
        if column != 0 {
            push(text, &format!(":{column}"));
        }
        Ok(())
    }

    /// `GetFunctionName`: a builtin's name for stack traces, else the
    /// function's debug name, else `eval` for an eval's code; empty for
    /// none.
    fn function_name(&self) -> Result<Units, Problem> {
        if let Some(id) = builtin_id(self.reader, self.shared)?
            && let Some(name) = name_for_stack_trace(self.reader, self.function, self.shared, id)?
        {
            return Ok(name.encode_utf16().collect());
        }
        let name = debug_name(self.reader, self.function)?;
        if !name.is_empty() {
            return Ok(name);
        }
        if let Some(script) = self.script
            && is_eval(self.reader, script)?
        {
            return Ok("eval".encode_utf16().collect());
        }
        Ok(Units::new())
    }

    /// `GetMethodName`: the name of the receiver's property that holds the
    /// function; `None` for none.
    fn method_name(&self) -> Result<Option<Units>, Problem> {
        let reader = self.reader;
        if is_null_or_undefined(reader, self.site.receiver)? {
            return Ok(None);
        }
        if is_class_initializer(reader, self.shared)? {
            return Ok(None);
        }
        let receiver = self.receiver_object()?;

        let (own, scope) = names::shared_name(reader, self.shared)?;
        let mut name = own
            .map(|own| whole_string(reader, own))
            .transpose()?
            .unwrap_or_default();
        let prefixed = ["get ", "set "]
            .iter()
            .any(|prefix| name.starts_with(&prefix.encode_utf16().collect::<Units>()));
        if prefixed {
            name.drain(..4);
        } else if name.is_empty() {
            // The name V8 inferred, as `o.foo` for `o.foo = function () {}`,
            // past its last dot.
            let inferred: Units = names::inferred_name(reader, self.shared, scope)?
                .encode_utf16()
                .collect();
            name = match inferred.iter().rposition(|unit| *unit == u16::from(b'.')) {
                Some(dot) => inferred[dot + 1..].to_vec(),
                None => inferred,
            };
        }
        if !name.is_empty() && self.holds_function(receiver, &name)? {
            return Ok(Some(name));
        }
        infer_method_name(reader, receiver, self.function)
    }

    /// Whether `receiver[name]`, found as a `LookupIterator` that skips
    /// interceptors finds it, is the function, or an accessor pair whose
    /// getter or setter is.
    fn holds_function(&self, receiver: Object, name: &[u16]) -> Result<bool, Problem> {
        let reader = self.reader;
        let text = String::from_utf16(name).map_err(|_| "a method name that is no text")?;
        if text
            .chars()
            .next()
            .is_some_and(|first| first.is_ascii_digit() || first == '-')
            || ["NaN", "Infinity"].contains(&text.as_str())
        {
            return Err("a method name that is a number, which V8 looks up as an index".into());
        }
        let mut holder = receiver;
        for _ in 0..MAX_PROTOTYPES {
            if reader.is(holder, Constant::JSProxyType)? {
                return Ok(false);
            }
            refuse_access_checks(reader, holder)?;
            match lookup::own(reader, holder, Key::Name(&text))? {
                Some((Held::Data(word), _)) => return Ok(word == self.function.0),
                Some((Held::Accessor(accessor), _)) => {
                    return pair_holds(reader, accessor, self.function);
                }
                Some((Held::Double { .. }, _)) => return Ok(false),
                None => {}
            }
            match lookup::prototype(reader, holder)? {
                Some(next) => holder = next,
                None => return Ok(false),
            }
        }
        Err("the receiver's prototypes are too many".into())
    }

    /// `GetTypeName`, for a method call.
    fn type_name(&self) -> Result<Units, Problem> {
        let reader = self.reader;
        let receiver = self.receiver_object()?;
        if reader.is(receiver, Constant::JSProxyType)? {
            return Ok("Proxy".encode_utf16().collect());
        }
        // From 13.6 `GetTypeName` names a function receiver by its debug
        // name; 12.4's `AppendMethodCall` names a class constructor so.
        let named = if (reader.version.major, reader.version.minor) >= (13, 6) {
            is_function(reader, receiver)?
        } else {
            reader.is(receiver, Constant::JSClassConstructorType)?
        };
        if named {
            let name = debug_name(reader, receiver)?;
            if !name.is_empty() {
                return Ok(name);
            }
        }
        constructors::constructor_name(reader, receiver)
    }

    /// The receiver, which `Object::ToObject` would wrap were it a
    /// primitive; a primitive is refused.
    fn receiver_object(&self) -> Result<Object, Problem> {
        let reader = self.reader;
        reader
            .object(self.site.receiver)?
            .filter(|receiver| is_receiver(reader, *receiver).unwrap_or(false))
            .ok_or_else(|| {
                "a method called on a primitive, whose wrapper microscope does not make".into()
            })
    }

    /// `IsMethodCall`: a call neither at the top level nor a constructor's.
    fn is_method_call(&self) -> Result<bool, Problem> {
        let receiver = self.site.receiver;
        let top_level = is_null_or_undefined(self.reader, receiver)?
            || match self.reader.object(receiver)? {
                Some(object) => self.reader.is(object, Constant::JSGlobalProxyType)?,
                None => false,
            };
        Ok(!top_level && !self.flags.constructor)
    }

    /// `IsPromiseAll`, `IsPromiseAllSettled`, or `IsPromiseAny`.
    fn is_promise_combinator(&self) -> Result<bool, Problem> {
        let reader = self.reader;
        let native = access::native_context(reader, reader.map(self.function)?)?;
        for slot in [
            Constant::ContextPromiseAllIndex,
            Constant::ContextPromiseAllSettledIndex,
            Constant::ContextPromiseAnyIndex,
        ] {
            if access::slot(reader, native, slot)? == self.function.0 {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// `GetSourcePosition`: the position V8 computed, or else the one the
    /// code's position table gives its offset.
    fn source_position(&self) -> Result<i64, Problem> {
        if self.flags.position_computed {
            return Ok(self.site.offset);
        }
        let code = self.site.code.ok_or("the call site has no code")?;
        if self.reader.is(code, Constant::BytecodeArrayType)? {
            return bytecode_position(self.reader, code, self.site.offset);
        }
        Err("a call site in compiled code, whose positions microscope does not read".into())
    }
}

/// `SourcePosition` of the bytecode `bytecode` at `offset`: the script
/// offset of the last position at or before it, 0 for none.
fn bytecode_position(reader: &Reader<'_>, bytecode: Object, offset: i64) -> Result<i64, Problem> {
    let table = reader
        .object(reader.field(bytecode, Field::BytecodeArraySourcePositionTable)?)?
        .filter(|table| {
            [Constant::TrustedByteArrayType, Constant::ByteArrayType]
                .iter()
                .any(|kind| reader.is(*table, *kind).unwrap_or(false))
        })
        .ok_or(
            "V8 has not collected the function's source positions, which it would by parsing it \
             again",
        )?;
    let entries = positions::decode(
        &positions::byte_array(reader, table)?,
        Encoding::of(reader.version),
    )?;
    let external = reader.constant(Constant::SourcePositionIsExternalMask)?;
    let mask = reader.constant(Constant::SourcePositionScriptOffsetMask)?;
    let shift = reader.constant(Constant::SourcePositionScriptOffsetShift)?;
    let mut position = 0;
    // `SourcePositionTableIterator`, `kJavaScriptOnly`.
    for entry in entries
        .iter()
        .filter(|entry| entry.position & external == 0)
    {
        if entry.code_offset > offset {
            break;
        }
        position = ((entry.position & mask) >> shift) - 1;
    }
    Ok(position)
}

/// `Script::GetPositionInfo` of `position` in `script`, with its line and
/// column offsets where `with_offset`; a line and column of -1 where the
/// script has no such position.
fn position_info(
    reader: &Reader<'_>,
    sources: &Sources,
    script: Object,
    position: i64,
    with_offset: bool,
) -> Result<Position, Problem> {
    let position = position.max(0);
    let Some((line, start)) = line_start(reader, sources, script, position)? else {
        return Ok(Position {
            line: -1,
            column: -1,
        });
    };
    let mut info = Position {
        line,
        column: position - start,
    };
    if with_offset {
        if info.line == 0 {
            info.column += reader.smi_field(script, Field::ScriptColumnOffset)?;
        }
        info.line += reader.smi_field(script, Field::ScriptLineOffset)?;
    }
    Ok(info)
}

/// The line, from 0, that `position` of `script` is on, and the offset its
/// line starts at, by the line ends V8's `String::CalculateLineEnds`
/// gives: those the script keeps, or else those of its source, the last
/// one past its end. `None` for a position past the last line's end, or a
/// script with no source.
fn line_start(
    reader: &Reader<'_>,
    sources: &Sources,
    script: Object,
    position: i64,
) -> Result<Option<(i64, i64)>, Problem> {
    let kept = reader.field(script, Field::ScriptLineEnds)?;
    if let Some(ends) = reader.object(kept)?
        && reader.is(ends, Constant::FixedArrayType)?
    {
        let end = |index| -> Result<i64, Problem> {
            reader
                .smi(reader.element(ends, index)?)?
                .ok_or_else(|| "a script's line end is no Smi".into())
        };
        let count = reader.smi_field(ends, Field::FixedArrayBaseLength)?;
        if count == 0 || end(count - 1)? < position {
            return Ok(None);
        }
        // The first line whose end is at or past the position.
        let (mut low, mut high) = (0, count - 1);
        while low < high {
            let middle = low + (high - low) / 2;
            if end(middle)? >= position {
                high = middle;
            } else {
                low = middle + 1;
            }
        }
        let start = if low == 0 { 0 } else { end(low - 1)? + 1 };
        return Ok(Some((low, start)));
    }

    let Some(source) = reader
        .object(reader.field(script, Field::ScriptSource)?)?
        .filter(|source| is_string(reader, *source).unwrap_or(false))
    else {
        return Ok(None);
    };
    let length = i64::from(reader.int32_field(source, Field::StringLength)?);
    if position > length {
        return Ok(None);
    }
    // The units before the position and the one at it, which tells a "\r"
    // that ends a line from one that begins "\r\n".
    let wanted = (position + 1).min(length);
    let units = sources.prefix(reader, source, wanted.cast_unsigned())?;
    let (mut line, mut start) = (0, 0);
    for (at, pair) in (0_i64..).zip(
        units.windows(2).map(|pair| (pair[0], pair[1])).chain(
            // The source's last unit ends a line as if a NUL followed it.
            units
                .last()
                .filter(|_| wanted == length)
                .map(|last| (*last, 0)),
        ),
    ) {
        if at >= position {
            break;
        }
        if is_line_terminator_sequence(pair.0, pair.1) {
            line += 1;
            start = at + 1;
        }
    }
    Ok(Some((line, start)))
}

/// The units of each script source read while writing one stack, from
/// its start, so that its call sites in one script read the source once.
#[derive(Default)]
struct Sources(RefCell<BTreeMap<u64, Units>>);

impl Sources {
    /// At most the units read past this many in one read: a source's
    /// longer prefix is read whole, so as not to take a read of each of
    /// its blocks; a shorter one by blocks, which the stop keeps for the
    /// stacks of the other errors read with it.
    const BY_BLOCKS: u64 = 16 * 1024;

    /// The first `wanted` units of the string `source`, or all it has.
    fn prefix(
        &self,
        reader: &Reader<'_>,
        source: Object,
        wanted: u64,
    ) -> Result<std::cell::Ref<'_, [u16]>, Problem> {
        {
            let mut kept = self.0.borrow_mut();
            let units = kept.entry(source.0).or_default();
            let have = units.len() as u64;
            if have < wanted {
                let piece = if wanted - have > Self::BY_BLOCKS {
                    wanted - have
                } else {
                    // Two-byte units fill a block at half as many.
                    (StopMemory::BLOCK / 2) as u64
                };
                let mut at = have;
                while at < wanted {
                    let limit = piece.min(wanted - at);
                    let before = units.len();
                    reader.string_into(source, at, limit, 0, units)?;
                    if units.len() == before {
                        break;
                    }
                    at = units.len() as u64;
                }
            }
        }
        let wanted = usize::try_from(wanted).map_err(|_| "a source is too long")?;
        Ok(std::cell::Ref::map(self.0.borrow(), |kept| {
            kept.get(&source.0)
                .map_or(&[][..], |units| &units[..wanted.min(units.len())])
        }))
    }
}

/// V8's `IsLineTerminatorSequence`: a line feed, a carriage return not
/// before a line feed, or a line or paragraph separator.
const fn is_line_terminator_sequence(unit: u16, next: u16) -> bool {
    match unit {
        0x0a | 0x2028 | 0x2029 => true,
        0x0d => next != 0x0a,
        _ => false,
    }
}

/// `Script::GetNameOrSourceURL`: its source URL where it has one, else its
/// name; `None` where that is no string.
fn name_or_source_url(reader: &Reader<'_>, script: Object) -> Result<Option<Units>, Problem> {
    let url = reader.field(script, Field::ScriptSourceUrl)?;
    let word = if is_undefined(reader, url)? {
        reader.field(script, Field::ScriptName)?
    } else {
        url
    };
    match reader.object(word)? {
        Some(name) if is_string(reader, name)? => whole_string(reader, name).map(Some),
        _ => Ok(None),
    }
}

/// `Script::HasSourceURLComment`: its source URL is a string, not empty.
fn has_source_url_comment(reader: &Reader<'_>, script: Object) -> Result<bool, Problem> {
    match reader.object(reader.field(script, Field::ScriptSourceUrl)?)? {
        Some(url) if is_string(reader, url)? => {
            Ok(reader.int32_field(url, Field::StringLength)? != 0)
        }
        _ => Ok(false),
    }
}

/// Whether `script` is an `eval`'s.
fn is_eval(reader: &Reader<'_>, script: Object) -> Result<bool, Problem> {
    let flags = reader.smi_field(script, Field::ScriptFlags)?;
    let kind = properties::part(
        reader,
        flags,
        Constant::ScriptCompilationTypeMask,
        Some(Constant::ScriptCompilationTypeShift),
    )?;
    Ok(kind == reader.constant(Constant::ScriptCompilationTypeEval)?)
}

/// `FormatEvalOrigin` of the eval's script `script`, `depth` evals deep.
fn eval_origin(
    reader: &Reader<'_>,
    sources: &Sources,
    script: Object,
    depth: usize,
) -> Result<Units, Problem> {
    if depth > MAX_EVALS {
        return Err("evals nest too deeply".into());
    }
    if let Some(name) = name_or_source_url(reader, script)? {
        return Ok(name);
    }
    let mut text = Units::new();
    push(&mut text, "eval at ");
    let Some(shared) = eval_from_shared(reader, script)? else {
        push(&mut text, "<anonymous>");
        return Ok(text);
    };
    let name = shared_debug_name(reader, shared)?;
    if name.is_empty() {
        push(&mut text, "<anonymous>");
    } else {
        text.extend(name);
    }
    let Some(outer) = frames::script(reader, shared)? else {
        return Ok(text);
    };
    push(&mut text, " (");
    if is_eval(reader, outer)? {
        text.extend(eval_origin(reader, sources, outer, depth + 1)?);
    } else if let Some(name) = reader
        .object(reader.field(outer, Field::ScriptName)?)?
        .filter(|name| is_string(reader, *name).unwrap_or(false))
    {
        text.extend(whole_string(reader, name)?);
        let at = position_info(
            reader,
            sources,
            outer,
            eval_position(reader, script)?,
            false,
        )?;
        if at.line >= 0 {
            push(&mut text, &format!(":{}:{}", at.line + 1, at.column + 1));
        }
    } else {
        push(&mut text, "unknown source");
    }
    push(&mut text, ")");
    Ok(text)
}

/// The function whose code made the eval's script `script`, if known.
fn eval_from_shared(reader: &Reader<'_>, script: Object) -> Result<Option<Object>, Problem> {
    let word = reader.field(script, Field::ScriptEvalFromSharedOrWrappedArguments)?;
    Ok(reader.object(word)?.filter(|shared| {
        reader
            .is(*shared, Constant::SharedFunctionInfoType)
            .unwrap_or(false)
    }))
}

/// `Script::GetEvalPosition`: where in its caller's script the eval was
/// made; a negative position is the caller's bytecode offset, not yet
/// translated.
fn eval_position(reader: &Reader<'_>, script: Object) -> Result<i64, Problem> {
    let position = reader.smi_field(script, Field::ScriptEvalFromPosition)?;
    if position >= 0 {
        return Ok(position);
    }
    let Some(shared) = eval_from_shared(reader, script)? else {
        return Ok(0);
    };
    let bytecode = names::bytecode(reader, shared)?
        .ok_or("the eval's caller has no bytecode to find the eval's position in")?;
    bytecode_position(reader, bytecode, -position)
}

/// `JSFunction::GetDebugName` of `function`.
fn debug_name(reader: &Reader<'_>, function: Object) -> Result<Units, Problem> {
    let shared = shared(reader, function)?;
    if let Some(name) = initializer_name(reader, shared)? {
        // No program reaches an initializer, so its map keeps the `name`
        // accessor, and V8 names it by its shared information.
        return Ok(name.encode_utf16().collect());
    }
    whole(&names::debug_name(reader, function, shared)?)
}

/// `SharedFunctionInfo::DebugName` of `shared`.
fn shared_debug_name(reader: &Reader<'_>, shared: Object) -> Result<Units, Problem> {
    if let Some(name) = initializer_name(reader, shared)? {
        return Ok(name.encode_utf16().collect());
    }
    whole(&names::function_name(reader, shared)?)
}

/// `name`, refused where it may be cut short: names are read to at most
/// [`MAX_STRING`] characters.
fn whole(name: &str) -> Result<Units, Problem> {
    let units: Units = name.encode_utf16().collect();
    if u64::try_from(units.len()).is_ok_and(|length| length < MAX_STRING) {
        return Ok(units);
    }
    Err("a function name too long to read whole".into())
}

/// What `DebugName` names a class's field initializer.
fn initializer_name(reader: &Reader<'_>, shared: Object) -> Result<Option<&'static str>, Problem> {
    let kind = function_kind(reader, shared)?;
    let first = reader.constant(Constant::FunctionKindClassMembersInitializerFunction)?;
    // From 14.6 each initializer has a kind preceded by the other's.
    let instance = reader
        .constant(Constant::FunctionKindClassMembersInitializerFunctionPrecededByStatic)
        .unwrap_or(first);
    if !is_class_initializer(reader, shared)? {
        return Ok(None);
    }
    Ok(Some(if (first..=instance).contains(&kind) {
        "<instance_members_initializer>"
    } else {
        "<static_initializer>"
    }))
}

/// `IsClassMembersInitializerFunction` through 13.6, and from 14.6
/// `IsClassInitializerFunction`: a class's field initializer.
fn is_class_initializer(reader: &Reader<'_>, shared: Object) -> Result<bool, Problem> {
    let kind = function_kind(reader, shared)?;
    let first = reader.constant(Constant::FunctionKindClassMembersInitializerFunction)?;
    let last = reader
        .constant(Constant::FunctionKindClassStaticInitializerFunctionPrecededByMember)
        .or_else(|_| reader.constant(Constant::FunctionKindClassStaticInitializerFunction))?;
    Ok((first..=last).contains(&kind))
}

fn function_kind(reader: &Reader<'_>, shared: Object) -> Result<i64, Problem> {
    let flags = i64::from(reader.int32_field(shared, Field::SharedFunctionInfoFlags)?);
    properties::part(
        reader,
        flags,
        Constant::SfiFunctionKindMask,
        Some(Constant::SfiFunctionKindShift),
    )
}

/// The builtin `shared` runs, by its number, if it is one of V8's
/// builtins (`HasBuiltinId`): its function data is that number, a Smi.
fn builtin_id(reader: &Reader<'_>, shared: Object) -> Result<Option<i64>, Problem> {
    let field = if reader
        .layout
        .has(Field::SharedFunctionInfoUntrustedFunctionData)
    {
        Field::SharedFunctionInfoUntrustedFunctionData
    } else {
        Field::SharedFunctionInfoFunctionData
    };
    Ok(reader
        .smi(reader.field(shared, field)?)?
        .filter(|id| *id >= 0))
}

/// The `DataView.prototype` methods `Builtins::NameForStackTrace` names,
/// the same in 12.4, 13.6, and 14.6.
const DATA_VIEW_METHODS: [&str; 22] = [
    "getBigInt64",
    "getBigUint64",
    "getFloat16",
    "getFloat32",
    "getFloat64",
    "getInt8",
    "getInt16",
    "getInt32",
    "getUint8",
    "getUint16",
    "getUint32",
    "setBigInt64",
    "setBigUint64",
    "setFloat16",
    "setFloat32",
    "setFloat64",
    "setInt8",
    "setInt16",
    "setInt32",
    "setUint8",
    "setUint16",
    "setUint32",
];

/// `Builtins::NameForStackTrace` of the builtin numbered `id` that
/// `function` runs, `None` for one it does not name. The builtins it names
/// are each installed once, on `DataView.prototype` or
/// `String.prototype`, under the name their shared information keeps, so
/// the builtin of that property of `function`'s realm tells whether `id`
/// is the one named.
fn name_for_stack_trace(
    reader: &Reader<'_>,
    function: Object,
    shared: Object,
    id: i64,
) -> Result<Option<String>, Problem> {
    let (Some(own), _) = names::shared_name(reader, shared)? else {
        return Ok(None);
    };
    let own = reader.string(own)?;
    let native = access::native_context(reader, reader.map(function)?)?;
    let slot = |index| -> Result<Object, Problem> {
        reader
            .object(access::slot(reader, native, index)?)?
            .ok_or_else(|| format!("the realm's {} is no object", Constant::name(index)).into())
    };
    let data_view = || -> Result<Object, Problem> {
        match lookup::get(
            reader,
            slot(Constant::ContextDataViewFunctionIndex)?,
            Key::Name("prototype"),
        )? {
            Some(lookup::Value::Word(word)) => reader
                .object(word)?
                .ok_or_else(|| "DataView.prototype is no object".into()),
            _ => Err("DataView has no prototype".into()),
        }
    };

    let (holder, key, getter, name) = if DATA_VIEW_METHODS.contains(&own.as_str()) {
        let name = format!("DataView.prototype.{own}");
        (data_view()?, own, false, name)
    } else if own == "get byteLength" {
        let name = "get DataView.prototype.byteLength".to_owned();
        (data_view()?, "byteLength".to_owned(), true, name)
    } else if ["toLocaleLowerCase", "indexOf", "toLowerCase"].contains(&own.as_str()) {
        let name = format!("String.{own}");
        (
            slot(Constant::ContextInitialStringPrototypeIndex)?,
            own,
            false,
            name,
        )
    } else {
        return Ok(None);
    };

    let installed = match lookup::own(reader, holder, Key::Name(&key))? {
        Some((Held::Data(word), _)) if !getter => reader.object(word)?,
        Some((Held::Accessor(pair), _))
            if getter && reader.is(pair, Constant::AccessorPairType)? =>
        {
            reader.object(reader.field(pair, Field::AccessorPairGetter)?)?
        }
        _ => None,
    };
    let installed = match installed {
        Some(installed) if is_function(reader, installed)? => {
            builtin_id(reader, self::shared(reader, installed)?)?
        }
        _ => None,
    };
    match installed {
        Some(installed) => Ok((installed == id).then_some(name)),
        None => Err(format!(
            "the realm's {} is not V8's, so microscope cannot tell whether V8 names this builtin \
             {name}",
            name.trim_start_matches("get ")
        )
        .into()),
    }
}

/// `InferMethodName`: the one enumerable string key of the receiver or its
/// prototypes whose value, or accessor, is `function`; `None` for none or
/// more than one.
fn infer_method_name(
    reader: &Reader<'_>,
    receiver: Object,
    function: Object,
) -> Result<Option<Units>, Problem> {
    /// What the walk has found so far.
    enum Inferred {
        Undefined,
        Null,
        Key(Units),
    }
    let mut name = Inferred::Undefined;
    let objects = reader.constant(Constant::FirstJSObjectType)?
        ..=reader.constant(Constant::LastJSObjectType)?;
    let mut current = receiver;
    for _ in 0..MAX_PROTOTYPES {
        if !objects.contains(&reader.instance_type(current)?) {
            break;
        }
        if reader.is(current, Constant::JSGlobalProxyType)? {
            return Err(
                "a global proxy among the receiver's prototypes, whose access check microscope \
                 does not make"
                    .into(),
            );
        }
        if access_check_needed(reader, current)? {
            break;
        }
        let properties = reader.field(current, Field::JSReceiverPropertiesOrHash)?;
        if let Some(table) = reader.object(properties)?
            && reader.is(table, Constant::SwissNameDictionaryType)?
        {
            return Err(
                "an object kept in a Swiss name dictionary, which microscope does not read".into(),
            );
        }
        for property in own::all_named(reader, current)? {
            if property.symbol || !property.enumerable {
                continue;
            }
            let holds = match property.held {
                Held::Data(word) => word == function.0,
                Held::Accessor(accessor) => pair_holds(reader, accessor, function)?,
                Held::Double { .. } => false,
            };
            if !holds {
                continue;
            }
            let key = whole_string(reader, property.key)?;
            name = match name {
                Inferred::Undefined => Inferred::Key(key),
                Inferred::Key(found) if found == key => Inferred::Key(found),
                Inferred::Key(_) | Inferred::Null => Inferred::Null,
            };
        }
        // `PrototypeIterator::Advance`: the map's prototype, to null.
        match reader.object(reader.field(reader.map(current)?, Field::MapPrototype)?)? {
            Some(next) if !reader.is(next, Constant::OddballType)? => current = next,
            _ => break,
        }
    }
    Ok(match name {
        Inferred::Key(key) => Some(key),
        Inferred::Undefined | Inferred::Null => None,
    })
}

/// Whether the accessor `accessor` is a pair whose getter or setter is
/// `function`.
fn pair_holds(reader: &Reader<'_>, accessor: Object, function: Object) -> Result<bool, Problem> {
    if !reader.is(accessor, Constant::AccessorPairType)? {
        return Ok(false);
    }
    Ok(
        reader.field(accessor, Field::AccessorPairGetter)? == function.0
            || reader.field(accessor, Field::AccessorPairSetter)? == function.0,
    )
}

/// Whether `object`'s map asks for an access check.
fn access_check_needed(reader: &Reader<'_>, object: Object) -> Result<bool, Problem> {
    let bits = i64::from(reader.byte_field(reader.map(object)?, Field::MapBitField)?);
    Ok(properties::part(
        reader,
        bits,
        Constant::MapIsAccessCheckNeededMask,
        Some(Constant::MapIsAccessCheckNeededShift),
    )? != 0)
}

/// Refuses an object a lookup would stop at for an access check.
fn refuse_access_checks(reader: &Reader<'_>, object: Object) -> Result<(), Problem> {
    if reader.is(object, Constant::JSGlobalProxyType)? || access_check_needed(reader, object)? {
        return Err("an object behind an access check, which microscope does not make".into());
    }
    Ok(())
}

/// `StringEndsWithMethodName`: whether `subject` is `pattern`, or ends with
/// it after a `.` or a space.
fn ends_with_method_name(subject: &[u16], pattern: &[u16]) -> bool {
    if subject == pattern {
        return true;
    }
    subject.len() > pattern.len()
        && subject.ends_with(pattern)
        && [u16::from(b'.'), u16::from(b' ')].contains(&subject[subject.len() - pattern.len() - 1])
}

/// `String::IsIdentifier`, unit by unit: an identifier's start, then its
/// parts, with `$`, `_`, and `\` among both. Past Latin-1 V8 asks ICU's
/// tables, which microscope does not have, so such a name is refused.
fn is_identifier(name: &[u16]) -> Result<bool, Problem> {
    let Some((first, rest)) = name.split_first() else {
        return Ok(false);
    };
    if name.iter().any(|unit| *unit > 0xff) {
        return Err(
            "a function name past Latin-1, whose identifier test needs ICU's tables".into(),
        );
    }
    let start = |unit: u16| {
        let Ok(byte) = u8::try_from(unit) else {
            return false;
        };
        byte.is_ascii_alphabetic()
            || [b'$', b'_', b'\\', 0xaa, 0xb5, 0xba].contains(&byte)
            || (0xc0..=0xff).contains(&byte) && byte != 0xd7 && byte != 0xf7
    };
    let part = |unit: u16| {
        start(unit) || u8::try_from(unit).is_ok_and(|byte| byte.is_ascii_digit() || byte == 0xb7)
    };
    Ok(start(*first) && rest.iter().all(|unit| part(*unit)))
}

/// Every unit of the string `string`.
fn whole_string(reader: &Reader<'_>, string: Object) -> Result<Units, Problem> {
    let mut units = Units::new();
    reader.string_into(string, 0, u64::from(u32::MAX), 0, &mut units)?;
    Ok(units)
}

fn shared(reader: &Reader<'_>, function: Object) -> Result<Object, Problem> {
    reader.typed_field(
        function,
        Field::JSFunctionSharedFunctionInfo,
        Constant::SharedFunctionInfoType,
    )
}

/// Whether `object` is a `JSFunction`.
fn is_function(reader: &Reader<'_>, object: Object) -> Result<bool, Problem> {
    let ty = reader.instance_type(object)?;
    Ok((reader.constant(Constant::FirstJSFunctionType)?
        ..=reader.constant(Constant::LastJSFunctionType)?)
        .contains(&ty))
}

fn is_string(reader: &Reader<'_>, object: Object) -> Result<bool, Problem> {
    Ok(reader.instance_type(object)? < reader.constant(Constant::FirstNonstringType)?)
}

/// Whether `object` is a `JSReceiver`, an object as JavaScript has them.
fn is_receiver(reader: &Reader<'_>, object: Object) -> Result<bool, Problem> {
    let ty = reader.instance_type(object)?;
    Ok((reader.constant(Constant::FirstJSReceiverType)?
        ..=reader.constant(Constant::LastJSReceiverType)?)
        .contains(&ty))
}

fn is_undefined(reader: &Reader<'_>, word: u64) -> Result<bool, Problem> {
    oddball_is(reader, word, &[Constant::OddballUndefined])
}

fn is_null_or_undefined(reader: &Reader<'_>, word: u64) -> Result<bool, Problem> {
    oddball_is(
        reader,
        word,
        &[Constant::OddballUndefined, Constant::OddballNull],
    )
}

/// Whether `word` is an oddball of one of the kinds `kinds`.
fn oddball_is(reader: &Reader<'_>, word: u64, kinds: &[Constant]) -> Result<bool, Problem> {
    let Some(object) = reader.object(word)? else {
        return Ok(false);
    };
    if !reader.is(object, Constant::OddballType)? {
        return Ok(false);
    }
    let kind = reader.smi_field(object, Field::OddballKind)?;
    Ok(kinds
        .iter()
        .any(|constant| reader.constant(*constant).ok() == Some(kind)))
}

#[cfg(test)]
mod tests {
    use super::{ends_with_method_name, is_identifier, is_line_terminator_sequence};

    fn units(text: &str) -> Vec<u16> {
        text.encode_utf16().collect()
    }

    #[test]
    fn a_method_name_ends_a_function_name_after_a_dot_or_a_space() {
        let ends = |subject, pattern| ends_with_method_name(&units(subject), &units(pattern));
        assert!(ends("m", "m"));
        assert!(ends("Module._compile", "_compile"));
        assert!(ends("get broken", "broken"));
        assert!(!ends("describeWidget", "describe"));
        assert!(!ends("xm", "m"));
        assert!(!ends("m", "xm"));
    }

    #[test]
    fn identifiers_are_told_by_v8s_latin1_table() {
        let identifier = |text: &str| is_identifier(&units(text)).unwrap();
        assert!(identifier("describeWidget"));
        assert!(identifier("$_\\x1"));
        assert!(identifier("caf\u{e9}"));
        assert!(identifier("a\u{b7}b"));
        assert!(!identifier("\u{b7}a"));
        assert!(!identifier("1a"));
        assert!(!identifier("get broken"));
        assert!(!identifier("Module._compile"));
        assert!(!identifier("<instance_members_initializer>"));
        assert!(!identifier("a\u{d7}"));
        assert!(!identifier(""));
        assert!(is_identifier(&units("\u{3c0}")).is_err());
    }

    #[test]
    fn a_carriage_return_ends_a_line_unless_a_line_feed_follows() {
        assert!(is_line_terminator_sequence(0x0a, 0));
        assert!(is_line_terminator_sequence(0x0d, 0x41));
        assert!(!is_line_terminator_sequence(0x0d, 0x0a));
        assert!(is_line_terminator_sequence(0x2028, 0));
        assert!(!is_line_terminator_sequence(0x41, 0x0a));
    }
}
