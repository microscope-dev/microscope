//! Node's JavaScript timers. Every timer of an `Environment` shares one
//! libuv timer, which calls the function its principal realm keeps as its
//! timers callback, `processTimers` (`lib/internal/timers.js`). That
//! module keeps the timers themselves: `timerListMap` holds a list of
//! `Timeout` objects for each duration, linked through `_idleNext` and
//! `_idlePrev`. The walk reaches the module's variables through the
//! context `processTimers` closes over.

use std::sync::Arc;

use super::objects::{Object, Problem, Reader};
use super::own::{self, Held};
use super::tasks::{Environment, Walk};
use crate::TaskState;
use crate::image::v8::{Constant, Field};
use crate::runtime_model::RuntimeTask;

/// The function Node calls for its timers, and the script that defines it.
const PROCESS_TIMERS: &str = "processTimers";
const TIMERS_SCRIPT: &str = "node:internal/timers";

/// How many contexts out the module's is from `processTimers`'s at most.
const MOST_CONTEXTS: usize = 8;

/// The most timers read: a process keeps far fewer.
const MOST_TIMERS: usize = 1 << 16;

/// The variables of `internal/timers` the walk reads.
struct Module {
    lists: Object,
    async_id: u64,
    trigger: u64,
    refed: u64,
}

/// The tasks of `environment`'s timers, appended to `tasks`. An
/// `Environment` that has not yet set up its timers has none.
pub(super) fn listed(
    walk: &mut Walk<'_>,
    environment: Environment,
    tasks: &mut Vec<RuntimeTask>,
) -> Result<(), Problem> {
    let reader = walk.reader;
    let realm = reader.word(environment.address + walk.layout.principal_realm)?;
    if realm == 0 {
        return Err("the environment has no principal realm".into());
    }
    let handle = reader.word(realm + walk.layout.timers_callback)?;
    if handle == 0 {
        return Ok(());
    }
    let callback = super::tasks::function(&reader, reader.word(handle)?)?
        .ok_or("the realm's timers callback is not a function")?;
    let shared = super::tasks::shared(&reader, callback)?;
    let path = super::tasks::script_path(&reader, walk.table, callback)?;
    if super::names::function_name(&reader, shared)? != PROCESS_TIMERS
        || path.as_ref().map(|(_, path)| &**path) != Some(TIMERS_SCRIPT)
    {
        return Err(format!(
            "the realm's timers callback is not {TIMERS_SCRIPT}'s {PROCESS_TIMERS}"
        )
        .into());
    }
    let module = module(&reader, callback)?;

    let mut count = 0;
    for list in own::elements(&reader, module.lists, 0..u32::MAX)? {
        let Held::Data(list) = list.held else {
            return Err("a timer list is not an object".into());
        };
        let list = reader
            .object(list)?
            .ok_or("a timer list is not an object")?;
        let mut previous = list;
        let mut timer = linked(&reader, list, "_idleNext")?;
        while timer != list {
            count += 1;
            if count > MOST_TIMERS {
                return Err(format!("there are more than {MOST_TIMERS} timers").into());
            }
            if linked(&reader, timer, "_idlePrev")? != previous {
                return Err(
                    format!("the timer at {:#x} is not linked to its neighbour", timer.0).into(),
                );
            }
            tasks.push(task(walk, environment, &module, timer)?);
            previous = timer;
            timer = linked(&reader, timer, "_idleNext")?;
        }
    }
    Ok(())
}

/// The variables of the module whose context is the first out from
/// `callback`'s that holds `timerListMap`.
fn module(reader: &Reader<'_>, callback: Object) -> Result<Module, Problem> {
    let mut context = reader
        .object(reader.field(callback, Field::JSFunctionContext)?)?
        .ok_or("processTimers' context is not an object")?;
    for _ in 0..MOST_CONTEXTS {
        let locals = super::variables::locals(reader, context)?;
        let local = |name: &str| -> Result<u64, Problem> {
            match locals.iter().find(|(local, _)| local == name) {
                Some((_, Ok(word))) => Ok(*word),
                Some((_, Err(problem))) => Err(problem.clone()),
                None => Err(format!("{TIMERS_SCRIPT} has no variable {name}").into()),
            }
        };
        if locals.iter().any(|(name, _)| name == "timerListMap") {
            return Ok(Module {
                lists: reader
                    .object(local("timerListMap")?)?
                    .ok_or("timerListMap is not an object")?,
                async_id: local("async_id_symbol")?,
                trigger: local("trigger_async_id_symbol")?,
                refed: local("kRefed")?,
            });
        }
        let previous = super::access::slot(reader, context, Constant::ContextPreviousIndex)?;
        context = reader
            .object(previous)?
            .ok_or("no context out from processTimers' holds timerListMap")?;
    }
    Err("no context out from processTimers' holds timerListMap".into())
}

/// The object the own property `name` of `object` links to.
fn linked(reader: &Reader<'_>, object: Object, name: &str) -> Result<Object, Problem> {
    super::tasks::own_data(reader, object, name)?
        .map(|word| reader.object(word))
        .transpose()?
        .flatten()
        .ok_or_else(|| format!("the timer at {:#x} has no {name}", object.0).into())
}

/// What a property of a timer holds that the walk reads.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Primitive {
    Number(f64),
    Boolean(bool),
    Null,
    Other,
}

fn primitive(reader: &Reader<'_>, held: Held) -> Result<Primitive, Problem> {
    let word = match held {
        Held::Double { bits, .. } => return Ok(Primitive::Number(f64::from_bits(bits))),
        Held::Accessor(_) => return Err("a timer's property is an accessor".into()),
        Held::Data(word) => word,
    };
    if let Some(value) = reader.smi(word)? {
        #[allow(clippy::cast_precision_loss, reason = "a Smi is at most 32 bits here")]
        return Ok(Primitive::Number(value as f64));
    }
    let Some(object) = reader.object(word)? else {
        return Ok(Primitive::Other);
    };
    if reader.is(object, Constant::HeapNumberType)? {
        return Ok(Primitive::Number(f64::from_bits(
            reader.word(reader.field_address(object, Field::HeapNumberValue)?)?,
        )));
    }
    if reader.is(object, Constant::OddballType)? {
        let kind = reader.smi_field(object, Field::OddballKind)?;
        let is = |constant| reader.constant(constant).ok() == Some(kind);
        return Ok(if is(Constant::OddballTrue) {
            Primitive::Boolean(true)
        } else if is(Constant::OddballFalse) {
            Primitive::Boolean(false)
        } else if is(Constant::OddballNull) || is(Constant::OddballUndefined) {
            Primitive::Null
        } else {
            Primitive::Other
        });
    }
    Ok(Primitive::Other)
}

/// The task of the `Timeout` object `timer`.
fn task(
    walk: &mut Walk<'_>,
    environment: Environment,
    module: &Module,
    timer: Object,
) -> Result<RuntimeTask, Problem> {
    let reader = walk.reader;
    let properties = own::named(&reader, timer)?;
    let named = |name: &str| -> Result<Option<Held>, Problem> {
        for property in &properties {
            if !property.symbol && super::properties::is_named(&reader, property.key.0, name)? {
                return Ok(Some(property.held));
            }
        }
        Ok(None)
    };
    let keyed = |symbol: u64| {
        properties
            .iter()
            .find(|property| property.symbol && property.key.0 == symbol)
            .map(|property| property.held)
    };
    let number = |held: Option<Held>, what: &str| -> Result<f64, Problem> {
        match held.map(|held| primitive(&reader, held)).transpose()? {
            Some(Primitive::Number(value)) => Ok(value),
            _ => Err(format!("the timer at {:#x} has no {what}", timer.0).into()),
        }
    };

    let async_id = number(keyed(module.async_id), "async id")?;
    let trigger = number(keyed(module.trigger), "trigger id")?;
    let duration = number(named("_idleTimeout")?, "_idleTimeout")?;
    let repeat = match named("_repeat")?
        .map(|held| primitive(&reader, held))
        .transpose()?
    {
        Some(Primitive::Number(_)) => true,
        Some(Primitive::Null) => false,
        _ => return Err(format!("the timer at {:#x} has no _repeat", timer.0).into()),
    };
    let destroyed = named("_destroyed")?
        .map(|held| primitive(&reader, held))
        .transpose()?
        == Some(Primitive::Boolean(true));
    let Some(Primitive::Boolean(refed)) = keyed(module.refed)
        .map(|held| primitive(&reader, held))
        .transpose()?
    else {
        return Err(format!(
            "the timer at {:#x} does not say whether it is ref'd",
            timer.0
        )
        .into());
    };

    let mut problems = Vec::new();
    let callback = match named("_onTimeout")? {
        Some(Held::Data(word)) => {
            super::tasks::callback(&reader, walk.table, word).unwrap_or_else(|problem| {
                problems.push(format!("its callback cannot be read: {problem}"));
                None
            })
        }
        _ => None,
    };

    let kind = if repeat { "interval" } else { "timeout" };
    let unreferenced = if refed { "" } else { ", unref'd" };
    let mut detail = format!("{kind} {duration} ms{unreferenced}");
    if !problems.is_empty() {
        detail = format!("{detail} ({})", problems.join("; "));
    }
    Ok(RuntimeTask {
        number: environment.owner.number(async_id)?,
        locator: timer.0,
        state: if destroyed {
            TaskState::Exited
        } else {
            TaskState::Blocked
        },
        detail: Some(detail.into()),
        thread: None,
        resume: None,
        creation: None,
        spawned: None,
        entry: None,
        parent: environment.owner.number(trigger).ok(),
        internal: false,
        labels: vec![(Arc::from("type"), Arc::from("Timeout"))],
        callback,
        stackless: true,
    })
}
