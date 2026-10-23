//! What expressions read of Python's objects: what a name means in a frame,
//! the item a key or an index finds, and an object as an operand.
//!
//! A name is a variable of the frame's code, then a global of the frame's
//! module, then a built-in, as Python's own lookup finds it. An attribute
//! is found as [`super::instances`] finds it. A `dict`'s key
//! is found as Python's lookup finds it, without running any code: by the
//! key's hash, through the dict's table, comparing only the keys of equal
//! hash it passes. A key of `None`, `bool`, `int`, `float`, `str`, `bytes`,
//! or `tuple` is compared by value, a `tuple` with a tuple key item by
//! item, in order, as `tuple_richcompare` does; a key of any other type
//! only its own `__eq__` could compare, so meeting one refuses the lookup.
//!
//! A key the expression takes from the program is that object, which a
//! lookup, and a tuple's comparison of its items, find by identity before
//! comparing values, as CPython's do: a NaN finds itself.
//!
//! A number's hash is CPython's, a `tuple`'s is `tuplehash` of its items'
//! hashes, and a `str`'s is SipHash-1-3 keyed by the process's secret, once
//! a key of the dict shows that hashing gives the hashes its strings keep,
//! or the hash a `str` of the program keeps. A NaN of the program hashes
//! by its address. Where no hash is known, as for a NaN the expression
//! computed or where the secret cannot be read, every entry is compared,
//! and a key of another type is ruled out only by the hash its entry
//! keeps.

use std::sync::Arc;

use super::code::{CodeId, CodeTable};
use super::instances::Class;
use super::library::Library;
use super::objects::{Problem, Record, malformed};
use super::text;
use super::values::{Dict, Object, Values, exact};
use super::{address, handle};
use crate::eval::error::ErrorKind;
use crate::eval::target::{MAX_TUPLE_DEPTH, MAX_TUPLE_KEYS};
use crate::image::python::{Constant, Field};
use crate::runtime_model::{
    InterpretedFailure, InterpretedKey, InterpretedKind, InterpretedName, InterpretedOperand,
    InterpretedStep,
};
use crate::{TextCompletion, TextSummary, VariableUnavailableReason, VirtualAddress};

/// The most characters of a `str`, or bytes of a `bytes`, an operand reads.
const OPERAND_TEXT: u64 = 1 << 16;

/// How a `dict`'s table marks a slot no entry has filled, and how far a
/// probe's perturbation shifts on each step.
const EMPTY_SLOT: i64 = -1;
const PERTURB_SHIFT: u32 = 5;

/// How many of a `dict`'s first entries are tried for a `str` key that
/// shows whether hashing gives the hashes its strings keep.
const HASH_WITNESSES: u64 = 8;

/// CPython's hashes of numbers are their values modulo this prime,
/// `2**61 - 1`, and an infinity's is this.
const HASH_MODULUS: u64 = (1 << 61) - 1;
const HASH_BITS: u32 = 61;
const HASH_INFINITY: i64 = 314_159;

/// The primes of xxHash64, by which CPython's `tuplehash` mixes its items'
/// hashes, and the hash it gives where the mix comes to -1.
const XXPRIME_1: u64 = 11_400_714_785_074_694_791;
const XXPRIME_2: u64 = 14_029_467_366_897_019_727;
const XXPRIME_5: u64 = 2_870_177_450_012_600_261;
const TUPLE_HASH_ERROR: i64 = 1_546_275_796;

/// How a key of a `dict` compares with a key an expression wrote.
enum Compared {
    Equal,
    Unequal,

    /// Only the key's own type's code could say: the type's name.
    Unknown(Arc<str>),
}

fn refused(kind: ErrorKind, message: impl Into<Arc<str>>) -> InterpretedFailure {
    InterpretedFailure::Refused(kind, message.into())
}

impl Values<'_, '_> {
    /// Why a read failed, as the value's state says.
    pub(super) fn unavailable(&self, problem: Problem) -> InterpretedFailure {
        InterpretedFailure::Unavailable(match self.failed(problem) {
            crate::runtime_model::InterpretedSlot::Limit(exhaustion) => {
                VariableUnavailableReason::InspectionLimit(exhaustion)
            }
            crate::runtime_model::InterpretedSlot::Unreadable(problem) => {
                VariableUnavailableReason::Interpreted(problem)
            }
            _ => VariableUnavailableReason::EvaluationLimit,
        })
    }

    fn read(&self, address: VirtualAddress) -> Result<Object, InterpretedFailure> {
        self.object(address)
            .map_err(|problem| self.unavailable(problem))
    }

    /// What `name` means in the frame `frame`, which runs the code in row
    /// `code` of `table`.
    pub(super) fn name(
        &self,
        table: &CodeTable,
        code: CodeId,
        frame: &Record<'_>,
        name: &str,
    ) -> Result<Option<InterpretedName>, InterpretedFailure> {
        let hidden = self.reader.layout.constant(Constant::FastHidden);
        let variable = table
            .locals(code)
            .iter()
            .position(|local| local.kind & hidden == 0 && **table.name(local.name) == *name);
        if let Some(slot) = variable {
            let slot = u32::try_from(slot)
                .map_err(|_| self.unavailable(malformed(frame.address, "frame")))?;
            return Ok(Some(InterpretedName::Variable(slot)));
        }

        let key = InterpretedKey::Text(name.as_bytes().to_vec());

        for field in [Field::FrameGlobals, Field::FrameBuiltins] {
            let namespace = frame
                .pointer(field)
                .map_err(|problem| self.unavailable(problem))?;
            let Some(namespace) = namespace else {
                continue;
            };

            let Object::Dict(dict) = self.read(namespace)? else {
                return Err(self.unavailable(malformed(namespace, "frame's namespace")));
            };

            if let Some(object) = self.lookup(dict, &key)? {
                return Ok(Some(InterpretedName::Object(handle(object))));
            }
        }

        Ok(None)
    }

    /// The object the variable in slot `slot` of the frame `frame` holds,
    /// which runs the code in row `code` of `table`.
    pub(super) fn variable(
        &self,
        table: &CodeTable,
        code: CodeId,
        frame: &Record<'_>,
        slot: u32,
    ) -> Result<VirtualAddress, InterpretedFailure> {
        let layout = self.reader.layout;
        let local = usize::try_from(slot)
            .ok()
            .and_then(|slot| table.locals(code).get(slot))
            .ok_or_else(|| self.unavailable(malformed(frame.address, "frame")))?;
        let shared = local.kind
            & (layout.constant(Constant::FastCell) | layout.constant(Constant::FastFree));
        let at = self
            .reader
            .offset(Field::FrameLocalsplus)
            .map(|start| {
                frame
                    .address
                    .wrapping_add(start)
                    .wrapping_add(u64::from(slot) * 8)
            })
            .map_err(|problem| self.unavailable(problem))?;
        let bits = self
            .reader
            .word(at)
            .map_err(|problem| self.unavailable(problem))?;
        let held = self.reference(bits).and_then(|object| {
            if shared == 0 {
                Ok(object)
            } else {
                self.cell(object)
            }
        });
        match held {
            Ok(Some(object)) => Ok(object),
            Ok(None) => Err(InterpretedFailure::Unavailable(
                VariableUnavailableReason::Unbound,
            )),
            Err(problem) => Err(InterpretedFailure::Unavailable(
                VariableUnavailableReason::Interpreted(problem),
            )),
        }
    }

    /// Where the variable in slot `slot` of the frame `frame` keeps its
    /// object, to watch: a variable of a thread's frame that holds its
    /// object itself, not in a cell a closure shares.
    pub(super) fn watched(
        &self,
        table: &CodeTable,
        code: CodeId,
        frame: &Record<'_>,
        slot: u32,
    ) -> Result<VirtualAddress, InterpretedFailure> {
        let layout = self.reader.layout;
        let local = usize::try_from(slot)
            .ok()
            .and_then(|slot| table.locals(code).get(slot))
            .ok_or_else(|| self.unavailable(malformed(frame.address, "frame")))?;
        if local.kind & (layout.constant(Constant::FastCell) | layout.constant(Constant::FastFree))
            != 0
        {
            return Err(refused(
                ErrorKind::Unsupported,
                "the variable is kept in a cell its closures share, which is not watched",
            ));
        }
        let owner = frame
            .number(Field::FrameOwner, 1)
            .map_err(|problem| self.unavailable(problem))?;
        if owner != u64::from(layout.constant(Constant::OwnerThread)) {
            return Err(refused(
                ErrorKind::Unsupported,
                "a generator's or coroutine's variables live in its object, which outlives \
                 each run of its frame, and are not watched",
            ));
        }
        self.reader
            .offset(Field::FrameLocalsplus)
            .map(|start| {
                frame
                    .address
                    .wrapping_add(start)
                    .wrapping_add(u64::from(slot) * 8)
            })
            .map_err(|problem| self.unavailable(problem))
    }

    /// The object a step from the object at `object` reaches.
    pub(super) fn step(
        &self,
        object: VirtualAddress,
        step: InterpretedStep<'_>,
    ) -> Result<VirtualAddress, InterpretedFailure> {
        let key = match step {
            InterpretedStep::Item(key) => key,
            InterpretedStep::Attribute(name) => return self.attribute(object, name),
        };

        let (read, class) = match self.read(object)? {
            Object::Instance { class, base } => (
                self.special(&class, base, "__getitem__", "finds its items")?,
                Some(class),
            ),
            Object::Class { this, meta } => return Err(self.class_items(&this, &meta)?),
            read => (read, None),
        };

        // A library type's dict finds its items as a dict does, but for
        // the keys it holds none for, which its type may make values of.
        let (read, class) = match read {
            Object::Library {
                class: library,
                value,
                ..
            } if value.dict().is_some() => (
                value.dict().map_or(Object::Nothing, Object::Dict),
                class.or(Some(library)),
            ),
            read => (read, class),
        };

        match read {
            Object::List { items, length } | Object::Tuple { items, length } => {
                let position = position(&self.resolved(key)?, length, &read)?;
                let item = self
                    .items(items, position, position + 1)
                    .map_err(|problem| self.unavailable(problem))?;
                match item.first() {
                    Some(item) if *item != VirtualAddress::new(0) => Ok(*item),
                    _ => Err(self.unavailable(malformed(object, &type_text(&read)))),
                }
            }
            Object::Library {
                value:
                    Library::Deque {
                        block,
                        index,
                        length,
                        ..
                    },
                ..
            } => {
                let position = position(&self.resolved(key)?, length, &read)?;
                let item = self
                    .deque_items((block, index, length), position..position + 1)
                    .map_err(|problem| self.unavailable(problem))?;
                item.first()
                    .copied()
                    .ok_or_else(|| self.unavailable(malformed(object, "deque")))
            }
            Object::Dict(dict) => match self.lookup(dict, key)? {
                Some(value) => Ok(value),
                None => match class {
                    Some(class) if self.type_attribute(&class, "__missing__")?.is_some() => {
                        Err(refused(
                            ErrorKind::Unsupported,
                            format!(
                                "a `{}` gives a key it holds no value for by its own \
                                 `__missing__`, which microscope does not run",
                                class.qualified()
                            ),
                        ))
                    }
                    _ => Err(refused(
                        ErrorKind::MissingKey,
                        format!("the dict holds no key {}", self.key_text(key, 0)),
                    )),
                },
            },
            Object::Library {
                value: Library::Array(_),
                ..
            }
            | Object::Str { .. }
            | Object::Bytes { .. } => Err(refused(
                ErrorKind::Unsupported,
                format!(
                    "an item of a `{}` is a new object, which microscope does not make",
                    type_text(&read)
                ),
            )),
            _ => Err(refused(
                ErrorKind::Type,
                format!(
                    "microscope finds the items of a list, tuple, dict, or deque, not of a `{}`",
                    type_text(&read)
                ),
            )),
        }
    }

    /// The value `dict` holds for the very object at `key`, whose hash is
    /// `hash`, or `None` where it holds no value for that object: found as
    /// CPython's lookup finds a key it holds, by identity, which compares
    /// no keys and so runs no code.
    pub(super) fn identical(
        &self,
        dict: Dict,
        key: VirtualAddress,
        hash: i64,
    ) -> Result<Option<VirtualAddress>, Problem> {
        let mask = (1_u64 << dict.log2_size) - 1;
        let mut slot = hash.cast_unsigned() & mask;
        let mut perturb = hash.cast_unsigned();

        for _ in 0..=mask.saturating_mul(2).saturating_add(64) {
            let bytes = self.reader.bytes(
                dict.indices.wrapping_add(slot * dict.index_width),
                dict.index_width,
            )?;
            let index = signed(&bytes).ok_or_else(|| malformed(dict.object, "dict"))?;
            if index == EMPTY_SLOT {
                return Ok(None);
            }

            if let Ok(index) = u64::try_from(index) {
                let entry = self.entry(dict, index)?;
                if entry.key == key && entry.value != VirtualAddress::new(0) {
                    return Ok(Some(entry.value));
                }
            }

            perturb >>= PERTURB_SHIFT;
            slot = slot.wrapping_mul(5).wrapping_add(perturb).wrapping_add(1) & mask;
        }

        Err(malformed(dict.object, "dict"))
    }

    /// The value `dict` holds for `key`, or `None` when it holds none.
    pub(super) fn lookup(
        &self,
        dict: Dict,
        key: &InterpretedKey,
    ) -> Result<Option<VirtualAddress>, InterpretedFailure> {
        let hash = self.key_hash(dict, key, 0)?;
        hash.map_or_else(|| self.scan(dict, key), |hash| self.probe(dict, key, hash))
    }

    /// CPython's hash of `key`, where it is known: text's needs the
    /// process's secret, which `dict`'s keys may show hashing gives, and a
    /// tuple's needs each of its items'.
    fn key_hash(
        &self,
        dict: Dict,
        key: &InterpretedKey,
        depth: usize,
    ) -> Result<Option<i64>, InterpretedFailure> {
        let items = match key {
            InterpretedKey::Text(text) => return self.text_hash(dict, text),
            InterpretedKey::Tuple(items) => items,
            InterpretedKey::Object(object) => {
                let object = address(*object);
                match self.read(object)? {
                    // A NaN equals nothing, itself included, so its hash is
                    // its object's identity.
                    Object::Float(value) if value.is_nan() => {
                        return Ok(Some(hash_pointer(object)));
                    }

                    // A str keeps its hash once one is asked of it.
                    Object::Str { .. } => {
                        let kept = self
                            .reader
                            .head(object)
                            .and_then(|head| head.field_word(Field::UnicodeHash))
                            .map(u64::cast_signed)
                            .map_err(|problem| self.unavailable(problem))?;
                        if kept != -1 {
                            return Ok(Some(kept));
                        }
                    }
                    _ => {}
                }

                return self.key_hash(dict, &self.held(object, depth)?, depth);
            }
            key => return Ok(hash(key)),
        };

        if depth >= MAX_TUPLE_DEPTH {
            return Err(too_deep());
        }

        let mut hashes = Vec::with_capacity(items.len());
        for item in items {
            let Some(hash) = self.key_hash(dict, item, depth + 1)? else {
                return Ok(None);
            };

            hashes.push(hash);
        }

        Ok(Some(hash_tuple(&hashes)))
    }

    /// The value of the program's object at `object` as a key, one level
    /// deep: `None`, a `bool`, an `int`, a `float`, or a `str` is its
    /// value, and a `tuple` holds its items, each the program's object.
    /// One of any other type only its own code could hash or compare, and
    /// is refused, as an `int` beyond 128 bits is. `depth` is how deep in
    /// tuples the object is.
    fn held(
        &self,
        object: VirtualAddress,
        depth: usize,
    ) -> Result<InterpretedKey, InterpretedFailure> {
        let read = self.read(object)?;
        Ok(match &read {
            Object::Nothing => InterpretedKey::Nothing,
            Object::Bool(value) => InterpretedKey::Bool(*value),
            Object::Int { negative, digits } => match exact(*negative, digits, self.shift()) {
                Some(value) => InterpretedKey::Int(value),
                None => return Err(beyond_128_bits()),
            },
            Object::LargeInt { .. } => return Err(beyond_128_bits()),
            Object::Float(value) => InterpretedKey::Float(*value),
            Object::Str { .. } => {
                let (points, length) = self.code_points(object)?;
                if points.len() as u64 != length {
                    return Err(refused(
                        ErrorKind::Unsupported,
                        format!(
                            "a key's str holds {length} characters, more than the \
                             {OPERAND_TEXT} microscope compares"
                        ),
                    ));
                }

                // A lone surrogate is no character of UTF-8 text, which
                // keys hold.
                if points.iter().any(|point| char::from_u32(*point).is_none()) {
                    return Err(refused(
                        ErrorKind::Unsupported,
                        "a key's str holds a lone surrogate, which microscope does not compare",
                    ));
                }

                InterpretedKey::Text(text::utf8(&points))
            }
            Object::Tuple { items, length } => {
                if depth >= MAX_TUPLE_DEPTH {
                    return Err(too_deep());
                }

                if *length > MAX_TUPLE_KEYS as u64 {
                    return Err(refused(
                        ErrorKind::Limit,
                        format!(
                            "the key is a tuple of {length} items, and microscope finds a tuple \
                             key of at most {MAX_TUPLE_KEYS}"
                        ),
                    ));
                }

                let items = self
                    .items(*items, 0, *length)
                    .map_err(|problem| self.unavailable(problem))?;
                InterpretedKey::Tuple(
                    items
                        .into_iter()
                        .map(|item| InterpretedKey::Object(handle(item)))
                        .collect(),
                )
            }
            read => {
                return Err(refused(
                    ErrorKind::Unsupported,
                    format!(
                        "a `{}` is no key microscope compares: it finds keys of None, bool, int, \
                         float, str, and tuples of them, and only a type's own code could hash or \
                         compare another",
                        type_text(read)
                    ),
                ));
            }
        })
    }

    /// `key` with the program's object it names read as its value, for an
    /// index of a sequence, which takes no identity.
    fn resolved(&self, key: &InterpretedKey) -> Result<InterpretedKey, InterpretedFailure> {
        match key {
            InterpretedKey::Object(object) => self.held(address(*object), 0),
            key => Ok(key.clone()),
        }
    }

    /// A key as Python's `repr` writes it, for messages; an object of the
    /// program's that cannot be read is written by its address.
    fn key_text(&self, key: &InterpretedKey, depth: usize) -> String {
        match key {
            InterpretedKey::Object(object) => self.held(address(*object), depth).map_or_else(
                |_| format!("<object at {:#x}>", object.get()),
                |held| self.key_text(&held, depth),
            ),
            InterpretedKey::Tuple(items) => {
                let items: Vec<String> = items
                    .iter()
                    .map(|item| self.key_text(item, depth + 1))
                    .collect();
                match items.as_slice() {
                    [item] => format!("({item},)"),
                    items => format!("({})", items.join(", ")),
                }
            }
            key => scalar_text(key),
        }
    }

    /// The value `dict` holds for `key`, whose hash is `hash`, found by
    /// following the table's probe sequence for the hash, as CPython's
    /// lookup does.
    fn probe(
        &self,
        dict: Dict,
        key: &InterpretedKey,
        hash: i64,
    ) -> Result<Option<VirtualAddress>, InterpretedFailure> {
        let unavailable = |problem| self.unavailable(problem);
        let mask = (1_u64 << dict.log2_size) - 1;
        let mut slot = hash.cast_unsigned() & mask;
        let mut perturb = hash.cast_unsigned();

        // A table always has empty slots, which end a probe in fewer
        // steps than it has slots, past which the table is malformed.
        for _ in 0..=mask.saturating_mul(2).saturating_add(64) {
            let bytes = self
                .reader
                .bytes(
                    dict.indices.wrapping_add(slot * dict.index_width),
                    dict.index_width,
                )
                .map_err(unavailable)?;
            let index =
                signed(&bytes).ok_or_else(|| unavailable(malformed(dict.object, "dict")))?;
            if index == EMPTY_SLOT {
                return Ok(None);
            }

            // Below empty are the slots of deleted entries, which a probe
            // passes over.
            if let Ok(index) = u64::try_from(index) {
                let entry = self.entry(dict, index).map_err(unavailable)?;

                // The very object is found before any hash is compared.
                if entry.value != VirtualAddress::new(0)
                    && matches!(key, InterpretedKey::Object(object) if address(*object) == entry.key)
                {
                    return Ok(Some(entry.value));
                }

                if entry.value != VirtualAddress::new(0)
                    && self.entry_hash(entry.key, entry.hash)? == hash
                {
                    match self.compare(entry.key, key, 0)? {
                        Compared::Equal => return Ok(Some(entry.value)),
                        Compared::Unequal => {}
                        Compared::Unknown(name) => return Err(self.doubtful(&name, key)),
                    }
                }
            }

            perturb >>= PERTURB_SHIFT;
            slot = slot.wrapping_mul(5).wrapping_add(perturb).wrapping_add(1) & mask;
        }

        Err(unavailable(malformed(dict.object, "dict")))
    }

    /// The hash of a `dict`'s key: what its entry keeps, or, for a table
    /// of `str` keys, which keeps none, what the key keeps of itself.
    fn entry_hash(
        &self,
        key: VirtualAddress,
        kept: Option<i64>,
    ) -> Result<i64, InterpretedFailure> {
        if let Some(kept) = kept {
            return Ok(kept);
        }

        self.reader
            .head(key)
            .and_then(|head| head.field_word(Field::UnicodeHash))
            .map(u64::cast_signed)
            .map_err(|problem| self.unavailable(problem))
    }

    /// The hash of text as a `str` of this process, where it is known:
    /// once a `str` key of `dict` shows that hashing with the process's
    /// secret gives the hashes its strings keep.
    fn text_hash(&self, dict: Dict, text: &[u8]) -> Result<Option<i64>, InterpretedFailure> {
        let (Some(secret), Ok(text)) = (self.secret, std::str::from_utf8(text)) else {
            return Ok(None);
        };

        let unavailable = |problem| self.unavailable(problem);
        let bytes = self.reader.bytes(secret, 16).map_err(unavailable)?;
        let word = |offset| super::objects::read_le(&bytes, offset, 8).unwrap_or(0);
        let key = (word(0), word(8));
        if self.hashing.get().is_none() {
            for index in 0..dict.count.min(HASH_WITNESSES) {
                let entry = self.entry(dict, index).map_err(unavailable)?;
                if entry.value == VirtualAddress::new(0) {
                    continue;
                }

                let Object::Str { length, .. } = self.read(entry.key)? else {
                    continue;
                };

                let kept = self.entry_hash(entry.key, entry.hash)?;
                let (points, _) = self.code_points(entry.key)?;
                if kept != -1 && points.len() as u64 == length {
                    self.hashing.set(Some(hash_text(key, &points) == kept));
                    break;
                }
            }
        }

        let points: Vec<u32> = text.chars().map(u32::from).collect();
        Ok((self.hashing.get() == Some(true)).then(|| hash_text(key, &points)))
    }

    /// The value `dict` holds for `key`, whose hash is not known, found by
    /// comparing every entry.
    fn scan(
        &self,
        dict: Dict,
        key: &InterpretedKey,
    ) -> Result<Option<VirtualAddress>, InterpretedFailure> {
        let mut found = None;
        let mut uncompared = Vec::new();
        let mut failure = None;
        let scanned = self.entries(dict, 0..u64::MAX, |entry| {
            match self.compare(entry.key, key, 0) {
                Ok(Compared::Equal) => {
                    found.get_or_insert((entry.value, entry.hash));
                }
                Ok(Compared::Unequal) => {}
                Ok(Compared::Unknown(name)) => uncompared.push((name, entry.hash)),
                Err(error) => {
                    failure = Some(error);
                    return Err("the comparison failed".into());
                }
            }

            Ok(())
        });

        if let Some(failure) = failure {
            return Err(failure);
        }

        scanned.map_err(|problem| self.unavailable(problem))?;

        // A key is scanned for only where its hash is not known, as a
        // str's salted for the process, or a NaN's: the matching key's
        // entry gives it.
        let hash = found.and_then(|(_, hash)| hash);
        for (name, entry) in uncompared {
            if let (Some(hash), Some(entry)) = (hash, entry)
                && hash != entry
            {
                continue;
            }

            return Err(self.doubtful(&name, key));
        }

        Ok(found.map(|(value, _)| value))
    }

    /// How the key of a `dict`, or an item of one, at `address` compares
    /// with `wanted`, `depth` tuples deep in the key: the program's own
    /// object is equal to itself before any value is compared.
    fn compare(
        &self,
        address: VirtualAddress,
        wanted: &InterpretedKey,
        depth: usize,
    ) -> Result<Compared, InterpretedFailure> {
        let held;
        let wanted = match wanted {
            InterpretedKey::Object(object) if *object == handle(address) => {
                return Ok(Compared::Equal);
            }
            InterpretedKey::Object(object) => {
                held = self.held(super::address(*object), depth)?;
                &held
            }
            wanted => wanted,
        };

        let object = self.read(address)?;
        let number = match &object {
            Object::Bool(value) => Number::Int(i128::from(*value)),
            Object::Int { negative, digits } => match exact(*negative, digits, self.shift()) {
                Some(value) => Number::Int(value),
                None => return Ok(Compared::Unknown("int".into())),
            },
            Object::Float(value) => Number::Float(*value),
            Object::Str { length, points } => {
                let InterpretedKey::Text(bytes) = wanted else {
                    return Ok(Compared::Unequal);
                };

                let Ok(text) = std::str::from_utf8(bytes) else {
                    return Ok(Compared::Unequal);
                };

                if text.chars().count() as u64 != *length {
                    return Ok(Compared::Unequal);
                }

                let points = if points.len() as u64 == *length {
                    points.clone()
                } else {
                    self.code_points(address)?.0
                };

                let equal = text.chars().map(u32::from).eq(points.iter().copied());
                return Ok(if equal {
                    Compared::Equal
                } else {
                    Compared::Unequal
                });
            }
            Object::Nothing => {
                return Ok(if *wanted == InterpretedKey::Nothing {
                    Compared::Equal
                } else {
                    Compared::Unequal
                });
            }
            Object::Tuple { items, length } => {
                return match wanted {
                    InterpretedKey::Tuple(wanted) => {
                        self.compare_items(*items, *length, wanted, depth)
                    }
                    _ => Ok(Compared::Unequal),
                };
            }
            Object::Bytes { .. } | Object::Set { frozen: true, .. } => {
                return Ok(Compared::Unequal);
            }
            object => return Ok(Compared::Unknown(type_text(object).into())),
        };

        let wanted = match wanted {
            InterpretedKey::Int(value) => Number::Int(*value),
            InterpretedKey::Bool(value) => Number::Int(i128::from(*value)),
            InterpretedKey::Float(value) => Number::Float(*value),
            InterpretedKey::Text(_)
            | InterpretedKey::Nothing
            | InterpretedKey::Tuple(_)
            | InterpretedKey::Object(_) => return Ok(Compared::Unequal),
        };

        Ok(if number.equals(wanted) {
            Compared::Equal
        } else {
            Compared::Unequal
        })
    }

    /// How a `tuple` of `length` items at `items` compares with `wanted`,
    /// as `tuple_richcompare` compares for `==`: item by item, in order, to
    /// the shorter's end, the first item that is not equal deciding, and
    /// then the lengths. An item only its own type's code could compare
    /// leaves the answer unknown, unless an item before it was unequal.
    fn compare_items(
        &self,
        items: VirtualAddress,
        length: u64,
        wanted: &[InterpretedKey],
        depth: usize,
    ) -> Result<Compared, InterpretedFailure> {
        if depth >= MAX_TUPLE_DEPTH {
            return Err(too_deep());
        }

        let shared = length.min(wanted.len() as u64);
        let stored = self
            .items(items, 0, shared)
            .map_err(|problem| self.unavailable(problem))?;
        for (item, wanted) in stored.into_iter().zip(wanted) {
            match self.compare(item, wanted, depth + 1)? {
                Compared::Equal => {}
                unequal_or_unknown => return Ok(unequal_or_unknown),
            }
        }

        Ok(if length == wanted.len() as u64 {
            Compared::Equal
        } else {
            Compared::Unequal
        })
    }

    /// Why a key of the type named `name` refuses a lookup of `key`.
    fn doubtful(&self, name: &str, key: &InterpretedKey) -> InterpretedFailure {
        refused(
            ErrorKind::Unsupported,
            format!(
                "the dict holds a key of type `{name}`, which only its type's `__eq__` could \
                 compare with {}, and microscope runs no code",
                self.key_text(key, 0)
            ),
        )
    }

    /// The code points of the `str` at `object`, up to [`OPERAND_TEXT`],
    /// and how many it holds.
    fn code_points(&self, object: VirtualAddress) -> Result<(Vec<u32>, u64), InterpretedFailure> {
        self.reader
            .head(object)
            .and_then(|head| head.code_points(OPERAND_TEXT))
            .map_err(|problem| self.unavailable(problem))
    }

    /// The object at `object`, as an operation computes with it.
    pub(super) fn operand(
        &self,
        object: VirtualAddress,
    ) -> Result<InterpretedOperand, InterpretedFailure> {
        let read = self.read(object)?;
        let (kind, text) = match &read {
            Object::Nothing => (InterpretedKind::Nothing, None),
            Object::Bool(value) => (InterpretedKind::Bool(*value), None),
            Object::Int { negative, digits } => (
                InterpretedKind::Int(exact(*negative, digits, self.shift())),
                None,
            ),
            Object::LargeInt { .. } => (InterpretedKind::Int(None), None),
            Object::Float(value) => (InterpretedKind::Float(*value), None),
            Object::Str { .. } => {
                let (points, length) = self.code_points(object)?;
                let complete = points.len() as u64 == length;
                (
                    InterpretedKind::Text,
                    Some(TextSummary {
                        bytes: text::utf8(&points).into(),
                        completion: if complete {
                            TextCompletion::Complete
                        } else {
                            TextCompletion::Truncated { length: None }
                        },
                    }),
                )
            }
            Object::Bytes { length, .. } => {
                let shown = (*length).min(OPERAND_TEXT);
                let bytes = self
                    .reader
                    .field_bytes(object, Field::BytesData, shown)
                    .map_err(|problem| self.unavailable(problem))?;
                (
                    InterpretedKind::Bytes,
                    Some(TextSummary {
                        bytes: bytes.into(),
                        completion: if shown == *length {
                            TextCompletion::Complete
                        } else {
                            TextCompletion::Truncated {
                                length: Some(*length),
                            }
                        },
                    }),
                )
            }
            Object::List { length, .. }
            | Object::Tuple { length, .. }
            | Object::Set { used: length, .. } => {
                (InterpretedKind::Sequence { length: *length }, None)
            }
            Object::Dict(Dict { length, .. }) => (InterpretedKind::Map { length: *length }, None),
            Object::Library { value, .. } => (value.kind(), None),
            Object::Instance { .. }
            | Object::Class { .. }
            | Object::Module { .. }
            | Object::Function { .. }
            | Object::Builtin { .. }
            | Object::Code { .. } => (InterpretedKind::Other, None),
        };

        Ok(InterpretedOperand {
            type_name: type_text(&read).into(),
            kind,
            text,
        })
    }

    /// Why the class `this`, of the metaclass `meta`, gives no item:
    /// Python finds one by the metaclass's `__getitem__`, or else by the
    /// class's own `__class_getitem__`, and either runs code.
    fn class_items(
        &self,
        this: &Class,
        meta: &Class,
    ) -> Result<InterpretedFailure, InterpretedFailure> {
        Ok(if self.type_attribute(meta, "__getitem__")?.is_some() {
            refused(
                ErrorKind::Unsupported,
                format!(
                    "the class `{}` finds its items by its metaclass's `__getitem__`, which \
                     microscope does not run",
                    this.qualified()
                ),
            )
        } else if self.type_attribute(this, "__class_getitem__")?.is_some() {
            refused(
                ErrorKind::Unsupported,
                format!(
                    "the class `{}` makes its items by its own `__class_getitem__`, which \
                     microscope does not run",
                    this.qualified()
                ),
            )
        } else {
            refused(
                ErrorKind::Type,
                format!("the class `{}` has no items", this.qualified()),
            )
        })
    }

    /// The length of the object at `object`, as Python's `len` gives it.
    pub(super) fn length(&self, object: VirtualAddress) -> Result<u64, InterpretedFailure> {
        let read = match self.read(object)? {
            Object::Instance { class, base } => {
                self.special(&class, base, "__len__", "gives its length")?
            }
            Object::Class { this, meta } => {
                return Err(match self.type_attribute(&meta, "__len__")? {
                    Some(_) => refused(
                        ErrorKind::Unsupported,
                        format!(
                            "the class `{}` gives its length by its metaclass's `__len__`, which \
                             microscope does not run",
                            this.qualified()
                        ),
                    ),
                    None => refused(
                        ErrorKind::Type,
                        format!("the class `{}` has no length", this.qualified()),
                    ),
                });
            }
            read => read,
        };

        match read {
            Object::Str { length, .. }
            | Object::Bytes { length, .. }
            | Object::List { length, .. }
            | Object::Tuple { length, .. }
            | Object::Dict(Dict { length, .. })
            | Object::Set { used: length, .. } => Ok(length),
            Object::Library { ref value, .. } => match value.kind() {
                InterpretedKind::Sequence { length } | InterpretedKind::Map { length } => {
                    Ok(length)
                }
                _ => Err(refused(
                    ErrorKind::Type,
                    format!("a `{}` has no length", type_text(&read)),
                )),
            },
            read => Err(refused(
                ErrorKind::Type,
                format!(
                    "microscope reads the length of a str, bytes, list, tuple, dict, set, or \
                     deque, not of a `{}`",
                    type_text(&read)
                ),
            )),
        }
    }
}

/// The position in a sequence of `length` items, `read`, that `key`
/// indexes, as Python counts a negative index from its end.
fn position(key: &InterpretedKey, length: u64, read: &Object) -> Result<u64, InterpretedFailure> {
    let index = match key {
        InterpretedKey::Int(index) => *index,
        InterpretedKey::Bool(value) => i128::from(*value),
        _ => {
            return Err(refused(
                ErrorKind::Type,
                format!(
                    "a `{}` is indexed by integers, not by {}",
                    type_text(read),
                    key_kind(key)
                ),
            ));
        }
    };

    if index < 0 {
        index.checked_add(i128::from(length))
    } else {
        Some(index)
    }
    .and_then(|position| u64::try_from(position).ok())
    .filter(|position| *position < length)
    .ok_or_else(|| {
        refused(
            ErrorKind::Bounds,
            format!(
                "the `{}` holds {length} items, and {index} indexes none of them",
                type_text(read)
            ),
        )
    })
}

/// Why a tuple key that nests too deep is refused.
fn too_deep() -> InterpretedFailure {
    refused(
        ErrorKind::Limit,
        format!("microscope compares tuples in a key nested at most {MAX_TUPLE_DEPTH} deep"),
    )
}

/// Why an `int` of the program's beyond 128 bits is no key.
fn beyond_128_bits() -> InterpretedFailure {
    refused(
        ErrorKind::Unsupported,
        "the key is an int beyond the 128 bits microscope computes with",
    )
}

/// A `dict` table's slot, a little-endian signed number of its width.
fn signed(bytes: &[u8]) -> Option<i64> {
    Some(match bytes.len() {
        1 => i64::from(i8::from_le_bytes(bytes.try_into().ok()?)),
        2 => i64::from(i16::from_le_bytes(bytes.try_into().ok()?)),
        4 => i64::from(i32::from_le_bytes(bytes.try_into().ok()?)),
        8 => i64::from_le_bytes(bytes.try_into().ok()?),
        _ => return None,
    })
}

/// CPython's hash of a `str` of `points`, keyed by `key`: SipHash-1-3 of
/// the code points in the narrowest of the widths CPython keeps them in,
/// one, two, or four bytes each, and 0 for the empty `str`.
pub(super) fn hash_text(key: (u64, u64), points: &[u32]) -> i64 {
    if points.is_empty() {
        return 0;
    }

    let widest = points.iter().copied().max().unwrap_or(0);
    let bytes: Vec<u8> = if widest < 0x100 {
        points
            .iter()
            .map(|point| u8::try_from(*point).unwrap_or(0))
            .collect()
    } else if widest < 0x1_0000 {
        points
            .iter()
            .flat_map(|point| u16::try_from(*point).unwrap_or(0).to_le_bytes())
            .collect()
    } else {
        points
            .iter()
            .flat_map(|point| point.to_le_bytes())
            .collect()
    };

    never_error(siphash13(key, &bytes).cast_signed())
}

/// SipHash-1-3 of `bytes` keyed by `(k0, k1)`, as CPython's `pyhash.c`
/// computes it.
fn siphash13((k0, k1): (u64, u64), bytes: &[u8]) -> u64 {
    const fn round(v: &mut [u64; 4]) {
        v[0] = v[0].wrapping_add(v[1]);
        v[1] = v[1].rotate_left(13) ^ v[0];
        v[0] = v[0].rotate_left(32);
        v[2] = v[2].wrapping_add(v[3]);
        v[3] = v[3].rotate_left(16) ^ v[2];
        v[0] = v[0].wrapping_add(v[3]);
        v[3] = v[3].rotate_left(21) ^ v[0];
        v[2] = v[2].wrapping_add(v[1]);
        v[1] = v[1].rotate_left(17) ^ v[2];
        v[2] = v[2].rotate_left(32);
    }

    let mut v = [
        k0 ^ 0x736f_6d65_7073_6575,
        k1 ^ 0x646f_7261_6e64_6f6d,
        k0 ^ 0x6c79_6765_6e65_7261,
        k1 ^ 0x7465_6462_7974_6573,
    ];
    let (words, tail) = bytes.as_chunks::<8>();
    for word in words {
        let word = u64::from_le_bytes(*word);
        v[3] ^= word;
        round(&mut v);
        v[0] ^= word;
    }

    let mut last = [0; 8];
    last[..tail.len()].copy_from_slice(tail);
    let last = u64::from_le_bytes(last) | ((bytes.len() as u64) << 56);
    v[3] ^= last;
    round(&mut v);
    v[0] ^= last;
    v[2] ^= 0xff;
    for _ in 0..3 {
        round(&mut v);
    }

    v[0] ^ v[1] ^ v[2] ^ v[3]
}

/// A number a key holds or an expression wrote.
#[derive(Clone, Copy)]
enum Number {
    Int(i128),
    Float(f64),
}

impl Number {
    /// Whether two numbers are equal, exactly, as Python compares them.
    #[expect(clippy::float_cmp, reason = "Python compares numbers exactly")]
    fn equals(self, other: Self) -> bool {
        match (self, other) {
            (Self::Int(left), Self::Int(right)) => left == right,
            (Self::Float(left), Self::Float(right)) => left == right,
            (Self::Int(int), Self::Float(float)) | (Self::Float(float), Self::Int(int)) => {
                integral(float) == Some(int)
            }
        }
    }
}

/// A float's value as an integer, where it is one that fits in 128 bits.
#[expect(
    clippy::cast_possible_truncation,
    clippy::float_cmp,
    reason = "an integral float from -2**127 to below 2**127 converts exactly"
)]
fn integral(value: f64) -> Option<i128> {
    const LIMIT: f64 = 170_141_183_460_469_231_731_687_303_715_884_105_728.0;
    (value.is_finite() && value.trunc() == value && (-LIMIT..LIMIT).contains(&value))
        .then_some(value as i128)
}

/// CPython's hash of a key that is one value, where it does not depend on
/// the process: `None` for text, whose hash is salted, and for NaN, whose
/// hash is its object's identity.
fn hash(key: &InterpretedKey) -> Option<i64> {
    match key {
        InterpretedKey::Int(value) => Some(hash_int(*value)),
        InterpretedKey::Bool(value) => Some(i64::from(*value)),
        InterpretedKey::Float(value) => hash_float(*value),
        // A constant since CPython 3.12, the oldest microscope reads.
        InterpretedKey::Nothing => Some(0xfca8_6420),
        InterpretedKey::Text(_) | InterpretedKey::Tuple(_) | InterpretedKey::Object(_) => None,
    }
}

/// CPython's `tuplehash` of a tuple whose items hash to `hashes`: xxHash64's
/// rounds over them, unchanged from CPython 3.8 to 3.15.
fn hash_tuple(hashes: &[i64]) -> i64 {
    let mut accumulator = XXPRIME_5;
    for hash in hashes {
        accumulator = accumulator.wrapping_add(hash.cast_unsigned().wrapping_mul(XXPRIME_2));
        accumulator = accumulator.rotate_left(31);
        accumulator = accumulator.wrapping_mul(XXPRIME_1);
    }

    accumulator = accumulator.wrapping_add(hashes.len() as u64 ^ (XXPRIME_5 ^ 0x0035_d373));
    if accumulator == u64::MAX {
        TUPLE_HASH_ERROR
    } else {
        accumulator.cast_signed()
    }
}

/// CPython's `Py_HashPointer`: an object's address, rotated right by
/// four bits, as an object that hashes by identity hashes.
const fn hash_pointer(object: VirtualAddress) -> i64 {
    never_error(object.get().rotate_right(4).cast_signed())
}

/// `-1` is CPython's hash for an error, so a value that would hash to it
/// hashes to `-2`.
const fn never_error(hash: i64) -> i64 {
    if hash == -1 { -2 } else { hash }
}

/// CPython's `long_hash`: the value modulo [`HASH_MODULUS`], with the
/// value's sign.
pub(super) fn hash_int(value: i128) -> i64 {
    let magnitude = value.unsigned_abs() % u128::from(HASH_MODULUS);
    let magnitude = i64::try_from(magnitude).unwrap_or(i64::MAX);
    never_error(if value < 0 { -magnitude } else { magnitude })
}

/// CPython's `_Py_HashDouble`: an integral float hashes as the int it
/// equals, and any other as the rational it is, modulo [`HASH_MODULUS`].
pub(super) fn hash_float(value: f64) -> Option<i64> {
    if value.is_nan() {
        return None;
    }

    if value.is_infinite() {
        return Some(if value > 0.0 {
            HASH_INFINITY
        } else {
            -HASH_INFINITY
        });
    }

    let (mut mantissa, mut exponent) = frexp(value.abs());
    let mut hash: u64 = 0;
    #[expect(
        clippy::while_float,
        reason = "CPython's algorithm runs until the mantissa is spent, in two rounds at most"
    )]
    while mantissa != 0.0 {
        hash = ((hash << 28) & HASH_MODULUS) | hash >> (HASH_BITS - 28);
        mantissa *= 268_435_456.0;
        exponent -= 28;
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "the mantissa's integer part is below 2**28"
        )]
        let digit = mantissa as u64;
        #[expect(clippy::cast_precision_loss, reason = "the digit is below 2**28")]
        let whole = digit as f64;
        mantissa -= whole;
        hash += digit;
        if hash >= HASH_MODULUS {
            hash -= HASH_MODULUS;
        }
    }

    let bits = i32::try_from(HASH_BITS).unwrap_or(61);
    let shift = if exponent >= 0 {
        exponent % bits
    } else {
        bits - 1 - ((-1 - exponent) % bits)
    }
    .cast_unsigned();

    hash = ((hash << shift) & HASH_MODULUS) | hash >> (HASH_BITS - shift);
    let signed = i64::try_from(hash).unwrap_or(i64::MAX);
    Some(never_error(if value < 0.0 { -signed } else { signed }))
}

/// C's `frexp` of a finite, non-negative value: a mantissa in [0.5, 1),
/// or 0, and the power of two that scales it to the value.
fn frexp(value: f64) -> (f64, i32) {
    if value == 0.0 {
        return (0.0, 0);
    }

    let bits = value.to_bits();
    let biased = i32::try_from((bits >> 52) & 0x7ff).unwrap_or(0);
    if biased == 0 {
        // A subnormal: scale it into the normal range first.
        let (mantissa, exponent) = frexp(value * f64::from_bits(0x43f0_0000_0000_0000));
        return (mantissa, exponent - 64);
    }

    let mantissa = f64::from_bits((bits & !(0x7ff << 52)) | (1022 << 52));
    (mantissa, biased - 1022)
}

/// The name of an object's type, as messages show it: a built-in type's
/// alone, and another's qualified by its module.
fn type_text(object: &Object) -> String {
    match Values::type_name(object) {
        (Some(module), name) if &*module != "builtins" => format!("{module}.{name}"),
        (_, name) => name.to_string(),
    }
}

/// What kind of key an expression wrote, as a message says it.
const fn key_kind(key: &InterpretedKey) -> &'static str {
    match key {
        InterpretedKey::Int(_) => "an integer",
        InterpretedKey::Float(_) => "a float",
        InterpretedKey::Bool(_) => "a truth value",
        InterpretedKey::Text(_) => "text",
        InterpretedKey::Nothing => "`None`",
        InterpretedKey::Tuple(_) => "a tuple",
        InterpretedKey::Object(_) => "an object",
    }
}

/// A key that is one value, as Python writes it.
fn scalar_text(key: &InterpretedKey) -> String {
    match key {
        InterpretedKey::Int(value) => value.to_string(),
        InterpretedKey::Float(value) => text::float(*value),
        InterpretedKey::Bool(true) => "True".to_owned(),
        InterpretedKey::Bool(false) => "False".to_owned(),
        InterpretedKey::Nothing => "None".to_owned(),
        InterpretedKey::Text(bytes) => std::str::from_utf8(bytes).map_or_else(
            |_| text::bytes(bytes),
            |text| text::string(&text.chars().map(u32::from).collect::<Vec<_>>()),
        ),
        InterpretedKey::Tuple(_) | InterpretedKey::Object(_) => key_kind(key).to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::{frexp, hash_float, hash_int, hash_text, hash_tuple};

    #[test]
    fn text_hashes_as_cpython_hashes_it_with_a_zero_secret() {
        // Each value is `hash(s)` in CPython 3.12 to 3.14 run with
        // PYTHONHASHSEED=0, which zeroes the secret.
        let hash = |text: &str| hash_text((0, 0), &text.chars().map(u32::from).collect::<Vec<_>>());
        assert_eq!(hash(""), 0);
        assert_eq!(hash("a"), 4_644_417_185_603_328_019);
        assert_eq!(hash("abc"), -4_594_863_902_769_663_758);
        assert_eq!(hash("len"), 1_135_259_017_144_773_752);
        assert_eq!(hash("abcdefgh"), 4_574_395_652_268_504_554);
        assert_eq!(hash("abcdefghijklmnopq"), 7_044_894_726_457_044_172);
        assert_eq!(hash("café"), 137_524_001_917_817_222);
        assert_eq!(hash("日本語"), 7_116_994_712_188_270);
        assert_eq!(hash("snake \u{1f40d}"), -6_697_009_101_786_501_615);
        assert_eq!(hash_text((0, 0), &[0xdc80]), 8_931_334_759_769_479_107);
    }

    #[test]
    fn numbers_hash_as_cpython_hashes_them() {
        // Each value is `hash(x)` in CPython 3.12 to 3.15.
        assert_eq!(hash_int(0), 0);
        assert_eq!(hash_int(7), 7);
        assert_eq!(hash_int(-1), -2);
        assert_eq!(hash_int(-7), -7);
        assert_eq!(hash_int((1 << 61) - 1), 0);
        assert_eq!(hash_int(1 << 61), 1);
        assert_eq!(hash_int(-(1 << 61)), -2);
        assert_eq!(hash_int(1 << 100), 1 << 39);
        assert_eq!(hash_float(1.0), Some(1));
        assert_eq!(hash_float(-1.0), Some(-2));
        assert_eq!(hash_float(0.5), Some(1_152_921_504_606_846_976));
        assert_eq!(hash_float(1.5), Some(1_152_921_504_606_846_977));
        assert_eq!(hash_float(-0.5), Some(-1_152_921_504_606_846_976));
        assert_eq!(hash_float(0.1), Some(230_584_300_921_369_408));
        assert_eq!(hash_float(f64::INFINITY), Some(314_159));
        assert_eq!(hash_float(f64::NEG_INFINITY), Some(-314_159));
        assert_eq!(hash_float(f64::NAN), None);
        assert_eq!(hash_float(1e100), Some(1_822_893_315_824_342_674));
        assert_eq!(hash_float(1e-300), Some(482_449_582_752_280_463));
        assert_eq!(hash_float(5e-324), Some(16_777_216));
        assert_eq!(hash_float(2.0_f64.powi(70)), Some(hash_int(1 << 70)));
    }

    #[test]
    fn tuples_hash_as_cpython_hashes_them() {
        // Each value is `hash(t)` in CPython 3.12 to 3.15, free-threaded
        // builds included; an item hashes as the tests above check.
        let empty = hash_tuple(&[]);
        assert_eq!(empty, 5_740_354_900_026_072_187);
        assert_eq!(hash_tuple(&[1]), -6_644_214_454_873_602_895);
        assert_eq!(hash_tuple(&[1, 2]), -3_550_055_125_485_641_917);
        assert_eq!(
            hash_tuple(&[1, hash_tuple(&[2, 3])]),
            7_267_574_591_690_527_098
        );
        assert_eq!(
            hash_tuple(&[0xfca8_6420, 1, hash_int(-1)]),
            1_423_733_903_558_920_243
        );
        assert_eq!(
            hash_tuple(&[hash_int((1 << 61) - 1), hash_int(-2)]),
            8_374_038_142_897_728_572
        );
        assert_eq!(hash_tuple(&[empty]), -5_486_347_211_504_344_842);
        let half = hash_float(0.5).unwrap_or_default();
        assert_eq!(hash_tuple(&[half]), -408_149_959_306_781_352);
    }

    #[test]
    fn frexp_splits_a_float_as_c_does() {
        assert_eq!(frexp(8.0), (0.5, 4));
        assert_eq!(frexp(0.75), (0.75, 0));
        assert_eq!(frexp(5e-324), (0.5, -1073));
    }
}
