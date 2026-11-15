use std::collections::BTreeSet;

use std::sync::Arc;

use super::objects::{Object, Problem, Reader, address_at};
use crate::image::v8::{Constant, Field, ScopeInfoSlice, scope_info_slice};
use crate::runtime_model::{InterpretedFrameRef, ReportedLocal, ReportedValue};

pub(super) const MAX_CONTEXTS: usize = 256;

const MAX_SLOTS: i64 = 1 << 16;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Kind {
    Receiver,
    Argument(u32),
    Local,
    Captured,
    Module,
    Register {
        index: u32,
        unnamed: Option<Problem>,
    },
    Named(u32),
    Optimized,
    Global,
}

impl Kind {
    pub(super) const fn shown(&self) -> crate::VariableKind {
        match self {
            Self::Receiver | Self::Argument(_) => crate::VariableKind::Parameter,
            Self::Local | Self::Named(_) | Self::Optimized => crate::VariableKind::Local,
            Self::Captured => crate::VariableKind::Captured,
            Self::Module => crate::VariableKind::Module,
            Self::Register { .. } => crate::VariableKind::Unnamed,
            Self::Global => crate::VariableKind::Global,
        }
    }
}

pub(super) const UNNAMED: &str =
    "V8 only records names for variables that a closure uses, so these have none";

const HEADER_LEVEL: u32 = 3;

const OPTIMIZED: &str =
    "V8 optimized this frame and keeps its locals where microscope cannot read them";

#[derive(Debug, Default)]
struct Binding {
    parameters: Vec<Option<Arc<str>>>,

    registers: Vec<Option<Arc<str>>>,

    uninitialized: BTreeSet<u32>,
}

pub(super) const UNINITIALIZED: &str = "the variable is not initialized yet";

pub(super) const WITH_CONTEXT: &str = "lookup crosses a with context, whose object and Symbol.unscopables microscope does not evaluate";

fn bind(
    reader: &Reader<'_>,
    reported: &[ReportedLocal],
    own: &BTreeSet<&str>,
    (parameter_count, first): (usize, u32),
    register_count: u32,
    argument: &dyn Fn(u32) -> Result<u64, Problem>,
    register: &dyn Fn(u32) -> Result<u64, Problem>,
) -> Result<Binding, Problem> {
    let mismatch = |name: &str| -> Problem {
        format!(
            "V8's debugger reported a local `{name}` whose value is not where microscope expects it, so the frame's locals keep their numbers"
        )
        .into()
    };
    let mut binding = Binding::default();
    let (parameters, locals) = reported.split_at(parameter_count.min(reported.len()));
    for (index, local) in parameters.iter().enumerate() {
        if own.contains(&*local.name) {
            binding.parameters.push(None);
            continue;
        }
        let index = u32::try_from(index).map_err(|_| "too many parameters")?;
        if !holds(reader, argument(index)?, &local.value)? {
            return Err(mismatch(&local.name));
        }
        binding.parameters.push(Some(Arc::clone(&local.name)));
    }
    let locals = locals
        .iter()
        .filter(|local| !own.contains(&*local.name))
        .collect::<Vec<_>>();
    let room = register_count.saturating_sub(first) as usize;
    if let Some(extra) = locals.get(room) {
        return Err(mismatch(&extra.name));
    }
    binding.registers.resize(first as usize, None);
    for (index, local) in locals.iter().enumerate() {
        let index = u32::try_from(index).map_err(|_| "too many locals")? + first;
        let word = register(index)?;
        if !holds(reader, word, &local.value)? {
            return Err(mismatch(&local.name));
        }
        if local.value == ReportedValue::Unavailable && !super::own::is_hole(reader, word)? {
            binding.uninitialized.insert(index);
        }
        for earlier in &mut binding.registers {
            if earlier.as_deref() == Some(&*local.name) {
                *earlier = None;
            }
        }
        binding.registers.push(Some(Arc::clone(&local.name)));
    }
    debug_assert_eq!(
        binding.parameters.len(),
        parameters.len(),
        "the binding names {} parameters of {}",
        binding.parameters.len(),
        parameters.len(),
    );
    debug_assert_eq!(
        binding.registers.len(),
        first as usize + locals.len(),
        "the binding from register {first} names {} registers for {} locals",
        binding.registers.len(),
        locals.len(),
    );
    debug_assert!(
        binding
            .uninitialized
            .iter()
            .all(|index| (*index as usize) < binding.registers.len()),
        "the binding marks a register past its {} as uninitialized",
        binding.registers.len(),
    );
    Ok(binding)
}

pub(super) fn parameters(reader: &Reader<'_>, function: Object) -> Result<usize, Problem> {
    let shared = reader.typed_field(
        function,
        Field::JSFunctionSharedFunctionInfo,
        Constant::SharedFunctionInfoType,
    )?;
    let mut count = [0; 2];
    reader.bytes_into(
        reader.field_address(shared, Field::SharedFunctionInfoFormalParameterCount)?,
        &mut count,
    )?;
    let count =
        i64::from(u16::from_le_bytes(count)) - reader.constant(Constant::JSArgcReceiverSlots)?;
    usize::try_from(count).map_err(|_| format!("the function declares {count} parameters").into())
}

fn holds(reader: &Reader<'_>, word: u64, reported: &ReportedValue) -> Result<bool, Problem> {
    use super::inspect::Value;

    if super::own::is_hole(reader, word)? {
        return Ok(matches!(reported, ReportedValue::Unavailable));
    }
    let value = super::values::Builder::new(*reader).value(word, HEADER_LEVEL);
    Ok(match (reported, value) {
        (ReportedValue::Undefined | ReportedValue::Unavailable, Value::Undefined)
        | (ReportedValue::Null, Value::Null)
        | (ReportedValue::Symbol, Value::Symbol(_)) => true,
        (ReportedValue::Boolean(expected), Value::Boolean(held)) => *expected == held,
        (ReportedValue::Number(expected), Value::Number(held)) => {
            expected.to_bits() == held.to_bits() || (expected.is_nan() && held.is_nan())
        }
        (ReportedValue::String(expected), Value::String(held)) => {
            expected.encode_utf16().count() == held.length() as usize
                && expected
                    .encode_utf16()
                    .zip(held.units())
                    .all(|(expected, held)| expected == *held)
        }
        (ReportedValue::BigInt(expected), Value::BigInt(held)) => {
            **expected == *super::inspect::bigint_to_string(&held)
        }
        (ReportedValue::Object(class), Value::Object(_)) => {
            let object = reader.object(word)?.ok_or("an object is not an object")?;
            class
                .encode_utf16()
                .eq(super::constructors::constructor_name(reader, object)?)
        }
        _ => false,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Variable {
    pub(super) name: String,
    pub(super) kind: Kind,

    pub(super) word: Result<u64, Problem>,

    pub(super) at: Option<u64>,
}

pub(super) fn variables(
    reader: &Reader<'_>,
    frame: InterpretedFrameRef,
) -> Result<Vec<Variable>, Problem> {
    scoped_variables(reader, frame).map(|(variables, _)| variables)
}

pub(super) fn scoped_variables(
    reader: &Reader<'_>,
    frame: InterpretedFrameRef,
) -> Result<(Vec<Variable>, bool), Problem> {
    if let Some(object) = reader.object(frame.address.get())? {
        return suspended_variables(reader, object);
    }
    stack_variables(reader, frame)
}

fn stack_variables(
    reader: &Reader<'_>,
    frame: InterpretedFrameRef,
) -> Result<(Vec<Variable>, bool), Problem> {
    let fp = frame.address.get();
    let function = super::frames::javascript_function(reader, fp)?
        .ok_or("the frame runs no JavaScript function")?;
    let at = |constant| -> Result<u64, Problem> {
        Ok(fp.wrapping_add_signed(reader.constant(constant)?))
    };
    let mut found = Vec::new();

    let word = reader.word(at(Constant::StandardFrameArgCOffset)?)?;
    let argc = word
        .cast_signed()
        .checked_sub(reader.constant(Constant::JSArgcReceiverSlots)?)
        .filter(|argc| (0..MAX_SLOTS).contains(argc))
        .ok_or_else(|| format!("the frame's argument count word is {word:#x}"))?;
    let first = at(Constant::UnoptimizedFrameFirstParamFromFp)?;
    let context = reader.word(at(Constant::OffFpContext)?)?;
    let context = reader
        .object(context)?
        .ok_or("the frame's context is not an object")?;
    let this = if is_arrow(reader, function)? {
        lexical_this(reader, context)?.unwrap_or_else(|| {
            Err("V8 did not keep `this` because the arrow function does not use it".into())
        })
    } else {
        reader.word(first)
    };
    found.push(Variable {
        name: "this".to_owned(),
        kind: Kind::Receiver,
        word: this,
        at: None,
    });
    let mut seen = BTreeSet::new();
    let closure = reader.field(function, Field::JSFunctionContext)?;
    let (mut captured, crossed_with) = context_variables(reader, context, closure, &mut seen)?;
    let file = at(Constant::UnoptimizedFrameRegisterFileFromFp)?;
    let count = if frame.live_registers {
        registers(reader, fp, function)?
    } else {
        None
    };
    let register_at = |index: u32| file.checked_sub(8 * u64::from(index));
    let read_register = |index: u32| reader.word(register_at(index).ok_or("no such register")?);

    let argument = |index: u32| reader.word(address_at(first, 8 * (u64::from(index) + 1))?);
    let Names {
        parameters,
        registers: names,
        uninitialized,
        unnamed,
        optimized,
    } = stack_names(
        reader,
        (fp, function),
        &captured,
        count,
        &argument,
        &read_register,
    );
    for variable in optimized {
        seen.insert(variable.name.clone());
        found.push(variable);
    }

    found.extend(arguments(argc, &parameters, &argument, first));

    for (index, name) in names.iter().enumerate() {
        if let (Some(name), Ok(index)) = (name, u32::try_from(index)) {
            seen.insert(name.to_string());
            found.push(Variable {
                name: name.to_string(),
                kind: Kind::Named(index),
                word: if uninitialized.contains(&index) {
                    Err(UNINITIALIZED.into())
                } else {
                    read_register(index)
                },
                at: register_at(index),
            });
        }
    }
    captured.retain(|variable| !names.iter().flatten().any(|name| **name == *variable.name));
    found.extend(captured);

    if let Some(count) = count {
        for index in 0..count {
            if names.get(index as usize).is_some_and(Option::is_some) {
                continue;
            }
            found.push(Variable {
                name: format!("r{index}"),
                kind: Kind::Register {
                    index,
                    unnamed: unnamed.clone(),
                },
                word: read_register(index),
                at: register_at(index),
            });
        }
    }

    found.extend(script_bindings(reader, context, &mut seen, crossed_with)?);
    Ok((found, crossed_with))
}

fn suspended_variables(
    reader: &Reader<'_>,
    object: Object,
) -> Result<(Vec<Variable>, bool), Problem> {
    if !super::awaits::is_generator(reader, object)? {
        return Ok((Vec::new(), false));
    }
    let generator = object;
    let function = reader
        .object(reader.field(generator, Field::JSGeneratorObjectFunction)?)?
        .ok_or("the generator's function is not an object")?;
    let context = suspended_context(reader, generator)?;
    let mut found = Vec::new();
    let this = if is_arrow(reader, function)? {
        lexical_this(reader, context)?.unwrap_or_else(|| {
            Err("V8 did not keep `this` because the arrow function does not use it".into())
        })
    } else {
        reader.field(generator, Field::JSGeneratorObjectReceiver)
    };
    found.push(Variable {
        name: "this".to_owned(),
        kind: Kind::Receiver,
        word: this,
        at: None,
    });

    let mut seen = BTreeSet::new();
    let closure = reader.field(function, Field::JSFunctionContext)?;
    let (captured, crossed_with) = context_variables(reader, context, closure, &mut seen)?;
    let saved = reader
        .object(reader.field(generator, Field::JSGeneratorObjectParametersAndRegisters)?)?
        .ok_or("the generator's saved registers are not an array")?;
    let parameter_count = u32::try_from(parameters(reader, function)?)
        .map_err(|_| "the function declares too many parameters")?;
    for index in 0..parameter_count {
        found.push(Variable {
            name: format!("arguments[{index}]"),
            kind: Kind::Argument(index),
            word: reader.element(saved, i64::from(index)),
            at: None,
        });
    }
    found.extend(captured);
    let shared = reader.typed_field(
        function,
        Field::JSFunctionSharedFunctionInfo,
        Constant::SharedFunctionInfoType,
    )?;
    let count = super::awaits::saved_registers(reader, generator, shared)?;
    if i64::from(count) >= MAX_SLOTS {
        return Err(format!("the generator saved {count} registers").into());
    }
    for index in 0..count {
        found.push(Variable {
            name: format!("r{index}"),
            kind: Kind::Register {
                index,
                unnamed: None,
            },
            word: reader.element(saved, i64::from(parameter_count + index)),
            at: None,
        });
    }
    found.extend(script_bindings(reader, context, &mut seen, crossed_with)?);
    Ok((found, crossed_with))
}

fn suspended_context(reader: &Reader<'_>, generator: Object) -> Result<Object, Problem> {
    reader
        .object(reader.field(generator, Field::JSGeneratorObjectContext)?)?
        .ok_or_else(|| "the generator's context is not an object".into())
}

pub(super) fn frame_context(
    reader: &Reader<'_>,
    frame: InterpretedFrameRef,
) -> Result<Object, Problem> {
    let address = frame.address.get();
    if let Some(object) = reader.object(address)? {
        if !super::awaits::is_generator(reader, object)? {
            return Err("a promise combinator's frame runs no function of the program".into());
        }
        return suspended_context(reader, object);
    }
    let context =
        reader.word(address.wrapping_add_signed(reader.constant(Constant::OffFpContext)?))?;
    reader
        .object(context)?
        .ok_or_else(|| "the frame's context is not an object".into())
}

fn script_bindings(
    reader: &Reader<'_>,
    context: Object,
    seen: &mut BTreeSet<String>,
    crossed_with: bool,
) -> Result<Vec<Variable>, Problem> {
    let mut found = Vec::new();
    let native = super::access::native_context(reader, context)?;
    for context in super::access::script_contexts(reader, native)? {
        for (name, word) in locals(reader, context)? {
            if seen.insert(name.clone()) {
                found.push(Variable {
                    name,
                    kind: Kind::Module,
                    word: if crossed_with {
                        Err(WITH_CONTEXT.into())
                    } else {
                        word
                    },
                    at: None,
                });
            }
        }
    }
    found.push(Variable {
        name: "globalThis".to_owned(),
        kind: Kind::Global,
        at: None,
        word: if crossed_with {
            Err(WITH_CONTEXT.into())
        } else {
            super::access::slot(reader, native, Constant::ContextGlobalProxyIndex)
        },
    });
    Ok(found)
}

#[derive(Debug, Default)]
struct Names {
    parameters: Vec<Option<Arc<str>>>,
    registers: Vec<Option<Arc<str>>>,
    uninitialized: BTreeSet<u32>,

    unnamed: Option<Problem>,

    optimized: Vec<Variable>,
}

fn stack_names(
    reader: &Reader<'_>,
    (fp, function): (u64, Object),
    captured: &[Variable],
    count: Option<u32>,
    argument: &dyn Fn(u32) -> Result<u64, Problem>,
    register: &dyn Fn(u32) -> Result<u64, Problem>,
) -> Names {
    let Some(reported) = reader.stop.reported_locals(crate::VirtualAddress::new(fp)) else {
        return Names::default();
    };
    let reported = match reported {
        Ok(reported) => reported,
        Err(problem) => {
            return Names {
                unnamed: Some(problem),
                ..Names::default()
            };
        }
    };
    let Some(count) = count else {
        let optimized = reported
            .iter()
            .filter(|local| {
                !captured
                    .iter()
                    .any(|variable| *variable.name == *local.name)
            })
            .map(|local| Variable {
                name: local.name.to_string(),
                kind: Kind::Optimized,
                word: Err(OPTIMIZED.into()),
                at: None,
            })
            .collect();
        return Names {
            optimized,
            ..Names::default()
        };
    };
    let own = captured
        .iter()
        .filter(|variable| variable.kind == Kind::Local)
        .map(|variable| variable.name.as_str())
        .collect::<BTreeSet<_>>();
    let bound = parameters(reader, function).and_then(|parameter_count| {
        let bind = |first| {
            bind(
                reader,
                &reported,
                &own,
                (parameter_count, first),
                count,
                argument,
                register,
            )
        };
        bind(0).or_else(|problem| match is_resumable(reader, function) {
            Ok(true) => bind(1).map_err(|_| problem),
            _ => Err(problem),
        })
    });
    match bound {
        Ok(binding) => Names {
            parameters: binding.parameters,
            registers: binding.registers,
            uninitialized: binding.uninitialized,
            ..Names::default()
        },
        Err(problem) => Names {
            unnamed: Some(problem),
            ..Names::default()
        },
    }
}

fn arguments(
    argc: i64,
    parameters: &[Option<Arc<str>>],
    argument: &dyn Fn(u32) -> Result<u64, Problem>,
    first: u64,
) -> Vec<Variable> {
    let at = |index: u32| first.checked_add(8 * (u64::from(index) + 1));
    let passed = u32::try_from(argc).unwrap_or(0);
    let mut found = Vec::new();
    for index in 0..passed {
        let name = parameters.get(index as usize).cloned().flatten();
        found.push(Variable {
            name: name.map_or_else(|| format!("arguments[{index}]"), |name| name.to_string()),
            kind: Kind::Argument(index),
            word: argument(index),
            at: at(index),
        });
    }
    for (index, name) in parameters.iter().enumerate().skip(passed as usize) {
        if let (Some(name), Ok(index)) = (name, u32::try_from(index)) {
            found.push(Variable {
                name: name.to_string(),
                kind: Kind::Argument(index),
                word: argument(index),
                at: at(index),
            });
        }
    }
    found
}

pub(super) fn registers(
    reader: &Reader<'_>,
    fp: u64,
    function: Object,
) -> Result<Option<u32>, Problem> {
    let slot = fp.wrapping_add_signed(reader.constant(Constant::OffFpBytecodeArray)?);
    let Some(bytecode) = reader.object(reader.word(slot)?)? else {
        return Ok(None);
    };
    if !reader.is(bytecode, Constant::BytecodeArrayType)? {
        return Ok(None);
    }
    let shared = reader.typed_field(
        function,
        Field::JSFunctionSharedFunctionInfo,
        Constant::SharedFunctionInfoType,
    )?;
    if super::names::bytecode(reader, shared)? != Some(bytecode) {
        return Ok(None);
    }
    let size = reader.int32_field(bytecode, Field::BytecodeArrayFrameSize)?;
    let count = i64::from(size) / 8;
    if !(0..MAX_SLOTS).contains(&count) {
        return Err(format!("the frame keeps {count} registers").into());
    }
    Ok(u32::try_from(count).ok())
}

fn context_variables(
    reader: &Reader<'_>,
    mut context: Object,
    closure: u64,
    seen: &mut BTreeSet<String>,
) -> Result<(Vec<Variable>, bool), Problem> {
    let mut found = Vec::new();
    let mut own = true;
    let mut crossed_with = false;
    for _ in 0..MAX_CONTEXTS {
        own &= context.0 != closure;
        let ty = reader.instance_type(context)?;
        if ty == reader.constant(Constant::NativeContextType)? {
            return Ok((found, crossed_with));
        }
        crossed_with |= ty == reader.constant(Constant::WithContextType)?;
        let first = reader.constant(Constant::FirstContextType)?;
        let last = reader.constant(Constant::LastContextType)?;
        if !(first..=last).contains(&ty) {
            return Err(
                format!("{:#x}, of instance type {ty}, is not a context", context.0).into(),
            );
        }
        let kind = if ty == reader.constant(Constant::ScriptContextType)?
            || ty == reader.constant(Constant::ModuleContextType)?
        {
            Kind::Module
        } else if own {
            Kind::Local
        } else {
            Kind::Captured
        };
        for (name, at) in located_locals(reader, context)? {
            if name != "this" && seen.insert(name.clone()) {
                found.push(Variable {
                    name,
                    kind: kind.clone(),
                    word: if crossed_with {
                        Err(WITH_CONTEXT.into())
                    } else {
                        reader.word(at)
                    },
                    at: Some(at),
                });
            }
        }
        let previous = slot(reader, context, Constant::ContextPreviousIndex)?;
        context = reader
            .object(previous)?
            .ok_or("a context's previous context is not an object")?;
    }
    Err("the frame has too many contexts".into())
}

fn is_arrow(reader: &Reader<'_>, function: Object) -> Result<bool, Problem> {
    let shared = reader.typed_field(
        function,
        Field::JSFunctionSharedFunctionInfo,
        Constant::SharedFunctionInfoType,
    )?;
    let flags = i64::from(reader.int32_field(shared, Field::SharedFunctionInfoFlags)?);
    let kind = (flags & reader.constant(Constant::SfiFunctionKindMask)?)
        >> reader.constant(Constant::SfiFunctionKindShift)?;
    Ok((reader.constant(Constant::FunctionKindArrowFunction)?
        ..=reader.constant(Constant::FunctionKindAsyncArrowFunction)?)
        .contains(&kind))
}

pub(super) fn is_resumable(reader: &Reader<'_>, function: Object) -> Result<bool, Problem> {
    let shared = reader.typed_field(
        function,
        Field::JSFunctionSharedFunctionInfo,
        Constant::SharedFunctionInfoType,
    )?;
    let flags = i64::from(reader.int32_field(shared, Field::SharedFunctionInfoFlags)?);
    let kind = (flags & reader.constant(Constant::SfiFunctionKindMask)?)
        >> reader.constant(Constant::SfiFunctionKindShift)?;
    Ok([
        Constant::FunctionKindAsyncArrowFunction,
        Constant::FunctionKindAsyncFunction,
        Constant::FunctionKindAsyncConciseMethod,
        Constant::FunctionKindStaticAsyncConciseMethod,
        Constant::FunctionKindAsyncConciseGeneratorMethod,
        Constant::FunctionKindStaticAsyncConciseGeneratorMethod,
        Constant::FunctionKindAsyncGeneratorFunction,
        Constant::FunctionKindGeneratorFunction,
        Constant::FunctionKindConciseGeneratorMethod,
        Constant::FunctionKindStaticConciseGeneratorMethod,
    ]
    .iter()
    .any(|constant| reader.constant(*constant).ok() == Some(kind)))
}

fn lexical_this(
    reader: &Reader<'_>,
    mut context: Object,
) -> Result<Option<Result<u64, Problem>>, Problem> {
    for _ in 0..MAX_CONTEXTS {
        let ty = reader.instance_type(context)?;
        if ty == reader.constant(Constant::NativeContextType)? {
            return Ok(None);
        }
        if let Some((_, word)) = all_locals(reader, context)?
            .into_iter()
            .find(|(name, _)| name == "this")
        {
            return Ok(Some(word));
        }
        let previous = slot(reader, context, Constant::ContextPreviousIndex)?;
        context = reader
            .object(previous)?
            .ok_or("a context's previous context is not an object")?;
    }
    Err("the frame has too many contexts".into())
}

pub(super) fn slot(reader: &Reader<'_>, context: Object, index: Constant) -> Result<u64, Problem> {
    let index = u64::try_from(reader.constant(index)?).map_err(|_| "a negative context slot")?;
    reader.word(address_at(
        reader.field_address(context, Field::ContextElements)?,
        8 * index,
    )?)
}

pub(super) type Local = (String, Result<u64, Problem>);

pub(super) fn locals(reader: &Reader<'_>, context: Object) -> Result<Vec<Local>, Problem> {
    let mut found = all_locals(reader, context)?;
    found.retain(|(name, _)| name != "this");
    Ok(found)
}

fn all_locals(reader: &Reader<'_>, context: Object) -> Result<Vec<Local>, Problem> {
    Ok(located_locals(reader, context)?
        .into_iter()
        .map(|(name, at)| (name, reader.word(at)))
        .collect())
}

fn located_locals(reader: &Reader<'_>, context: Object) -> Result<Vec<(String, u64)>, Problem> {
    let scope = slot(reader, context, Constant::ContextScopeInfoIndex)?;
    let scope = reader
        .object(scope)?
        .filter(|scope| reader.is(*scope, Constant::ScopeInfoType).unwrap_or(false))
        .ok_or("a context's scope info is not a ScopeInfo")?;
    let count = reader.smi_field(scope, Field::ScopeInfoContextLocalCount)?;
    if count == 0 {
        return Ok(Vec::new());
    }
    let start = scope.0 - reader.constant(Constant::HeapObjectTag)?.cast_unsigned();
    let names = local_names(reader, scope, start, count)?;
    debug_assert!(
        names
            .iter()
            .all(|(index, _)| i64::try_from(*index).is_ok_and(|index| index < count)),
        "the scope {:#x} names a context local past its {count}",
        scope.0,
    );
    debug_assert!(
        names.is_sorted_by_key(|(index, _)| *index),
        "the context locals of the scope {:#x} are out of index order",
        scope.0,
    );

    let flags = super::names::scope_flags(reader, scope)?;
    let extension = flags & reader.constant(Constant::ScopeHasContextExtensionSlotMask)? != 0;
    let first = reader.constant(if extension {
        Constant::ContextMinContextExtendedSlots
    } else {
        Constant::ContextMinContextSlots
    })?;
    let first = u64::try_from(first).map_err(|_| "a negative context slot")?;
    let elements = reader.field_address(context, Field::ContextElements)?;

    let mut found = Vec::new();
    for (index, name) in names {
        let name = reader
            .object(name)?
            .ok_or("a context local's name is not a string")?;
        let name = reader.string(name)?;
        if name.is_empty() || name.starts_with('.') {
            continue;
        }
        found.push((name, address_at(elements, 8 * (first + index))?));
    }
    Ok(found)
}

fn local_names(
    reader: &Reader<'_>,
    scope: Object,
    start: u64,
    count: i64,
) -> Result<Vec<(u64, u64)>, Problem> {
    let mut read = |offset| reader.word(address_at(start, offset).ok()?).ok();
    if let Some(names) =
        scope_info_slice(reader.version, ScopeInfoSlice::ContextLocalNames, &mut read)?
            .filter(|names| i64::try_from(names.length).is_ok_and(|length| length == count))
    {
        return (0..names.length)
            .map(|index| {
                let at = address_at(address_at(start, names.offset)?, 8 * index)?;
                Ok((index, reader.word(at)?))
            })
            .collect();
    }

    let missing = || {
        format!(
            "the scope {:#x} has {count} context locals but no names",
            scope.0
        )
    };
    let table = scope_info_slice(
        reader.version,
        ScopeInfoSlice::ContextLocalNamesHashtable,
        &mut read,
    )?
    .filter(|table| table.length == 1)
    .ok_or_else(missing)?;
    let table = reader
        .object(reader.word(address_at(start, table.offset)?)?)?
        .filter(|table| {
            reader
                .is(*table, Constant::NameToIndexHashTableType)
                .unwrap_or(false)
        })
        .ok_or_else(missing)?;
    let length = reader.smi_field(table, Field::FixedArrayBaseLength)?;
    let objects = reader.field_address(table, Field::FixedArrayObjects)?;
    let word = |index: i64| -> Result<u64, Problem> {
        if !(0..length).contains(&index) {
            return Err(format!("an entry past the hash table {:#x}", table.0).into());
        }
        reader.word(address_at(objects, 8 * index.cast_unsigned())?)
    };
    let capacity = reader
        .smi(word(reader.constant(Constant::HashTableCapacityIndex)?)?)?
        .filter(|capacity| (0..=MAX_SLOTS).contains(capacity))
        .ok_or_else(|| format!("the hash table {:#x} has no capacity", table.0))?;
    let first = reader.constant(Constant::HashTablePrefixStartIndex)?
        + reader.constant(Constant::NameToIndexPrefixSize)?;
    let size = reader.constant(Constant::NameToIndexEntrySize)?;
    let key = reader.constant(Constant::HashTableEntryKeyIndex)?;
    let value = reader.constant(Constant::NameToIndexEntryValueIndex)?;

    let mut names = Vec::new();
    for entry in 0..capacity {
        let at = first + entry * size;
        let name = word(at + key)?;
        let Some(object) = reader.object(name)? else {
            continue;
        };
        if reader.instance_type(object)? >= reader.constant(Constant::FirstNonstringType)? {
            continue;
        }
        let index = reader
            .smi(word(at + value)?)?
            .filter(|index| (0..count).contains(index))
            .ok_or_else(|| format!("a local of the hash table {:#x} has no index", table.0))?;
        names.push((index.cast_unsigned(), name));
    }
    names.sort_unstable();
    if i64::try_from(names.len()).ok() != Some(count) {
        return Err(format!(
            "the hash table {:#x} names {} of {count} context locals",
            table.0,
            names.len()
        )
        .into());
    }
    Ok(names)
}

#[cfg(test)]
mod tests {
    use super::super::objects::tests::{Heap, layout, reader};
    use crate::VirtualAddress;
    use crate::image::v8::{Constant, Field};
    use crate::runtime_model::InterpretedFrameRef;

    #[test]
    fn a_frame_whose_argument_count_overflows_is_a_problem() {
        let mut layout = layout();
        let function = layout.constant(Constant::JSFunctionType).unwrap();
        for (constant, value) in [
            (Constant::OffFpFunction, -16),
            (Constant::StandardFrameArgCOffset, -24),
            (Constant::JSArgcReceiverSlots, 1),
            (Constant::FirstJSFunctionType, function),
            (Constant::LastJSFunctionType, function),
        ] {
            layout.set_constant(constant, value);
        }
        let mut heap = Heap::default();
        heap.word(0x1000, 0x1001);
        heap.word(
            0x1000 + layout.field(Field::MapInstanceType).unwrap(),
            function.cast_unsigned(),
        );
        heap.word(0x2000, 0x1001);
        let fp = 0x9000;
        heap.word(fp - 16, 0x2001);
        heap.word(fp - 24, 1 << 63);

        let frame = InterpretedFrameRef {
            address: VirtualAddress::new(fp),
            live_registers: true,
        };
        let problem = super::variables(&reader(&heap, &layout), frame)
            .expect_err("the argument count is out of range");
        assert!(problem.contains("argument count"), "{problem}");
    }
}
