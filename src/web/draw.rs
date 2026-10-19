//! The data a drawing's renderer draws: each input, walked through the
//! values' presentations as the value tree reads them, into plain values
//! whose kind its type alone decides (`docs/visualizers.md`). Numbers in
//! sequences and the bytes `bytes(PTR, LEN)` reads travel beside the answer
//! in one binary frame.
//!
//! A drawing is never partial: when any part of any input cannot be read,
//! the renderer gets nothing, and the page says which part and why.

use std::sync::Arc;

use microscope::{
    BaseTypeEncoding, DebuggerHandle, Evaluation, EvaluationMode, Expression, FloatValue,
    InspectionLimits, Presentation, PresentedCount, PresentedShape, ScalarValue, TextCompletion,
    TypeInfo, TypeKind, ValueChildQuery, ValueChildRelationship, ValueChildren,
    ValueChildrenReference, VariableState, VariableValue,
};

use super::inspect;
use super::protocol::{
    self, Datum, DrawInput, Drawing, ErrorKind, NumberKind, RendererInfo, RendererSource,
};

use super::session::Failure;
use super::values::expression_failure;

/// The most values one drawing's inputs may hold.
const MOST_VALUES: u64 = 65_536;

/// How deeply one drawing's inputs may nest.
const MOST_DEPTH: usize = 16;

/// The largest integer a double holds exactly, `Number.MAX_SAFE_INTEGER`.
const MOST_EXACT: u128 = (1 << 53) - 1;

/// How many children one request reads.
const PAGE: u32 = 256;

/// What reading a drawing's inputs takes: the most any request may, as a
/// drawing shows more than a row.
const fn input_limits() -> InspectionLimits {
    microscope::MAX_INSPECTION_LIMITS
}

/// A drawing and the bytes its inputs' numbers and bytes lie in.
pub struct Drawn {
    pub drawing: Drawing,
    pub bytes: Vec<u8>,
}

/// Reads the inputs of the drawing `request` names.
pub async fn draw(handle: &DebuggerHandle, request: &protocol::Draw) -> Result<Drawn, Failure> {
    let at = request.at;
    let context = inspect::context(handle, at.stop, at.execution(), at.frame).await?;
    let expression = Expression::parse(&request.path).map_err(expression_failure)?;
    let evaluation = handle
        .at(context)
        .evaluate_with(&expression, EvaluationMode::Read, input_limits())
        .await?;
    let Evaluation::Value { value, .. } = evaluation else {
        return Err(Failure::new(
            ErrorKind::Invalid,
            "a range of elements is drawn one element at a time",
        ));
    };

    let value = pointee_with_drawings(handle, context, &request.path, value).await;
    let offered = match &value.state {
        VariableState::Available {
            presentation: Some(presentation),
            ..
        } => presentation
            .visualizers
            .iter()
            .find(|visualizer| *visualizer.renderer.name == *request.renderer)
            .cloned(),
        _ => None,
    };

    let is_offered = offered.is_some();
    let mut walk = Walk {
        handle,
        bytes: Vec::new(),
        values: 0,
    };

    let (renderer, inputs) = if let Some(visualizer) = offered {
        let inputs = handle
            .visualizer_inputs(Arc::clone(&visualizer.inputs), input_limits())
            .await?;
        (Arc::clone(&visualizer.renderer), walk.inputs(&inputs).await)
    } else {
        let renderer = handle
            .renderers()
            .await?
            .iter()
            .find(|renderer| *renderer.name == *request.renderer)
            .cloned()
            .ok_or_else(|| {
                Failure::new(
                    ErrorKind::Invalid,
                    format!("no renderer is named `{}`", request.renderer),
                )
            })?;
        let datum = walk
            .value(value.type_info.as_ref(), &value.state, "values", 0)
            .await;

        // As `visualize "NAME" { values = self }`.
        (
            renderer,
            vec![("values".to_owned(), datum, Some(String::new()))],
        )
    };

    let mut problem = None;
    let mut drawn = Vec::new();
    for (name, datum, path) in inputs {
        match datum {
            Ok(value) => drawn.push(DrawInput { name, value, path }),
            Err(reason) => {
                problem = Some(reason);
                break;
            }
        }
    }

    let bytes = if problem.is_some() {
        Vec::new()
    } else {
        walk.bytes
    };

    Ok(Drawn {
        drawing: Drawing {
            renderer: info(&renderer),
            offered: is_offered,
            inputs: problem.is_none().then_some(drawn),
            problem,
            bytes: bytes.len() as u64,
        },
        bytes,
    })
}

/// What a pointer at a value with drawings points to, as its row offers
/// the target's drawings; any other value is drawn as it is.
async fn pointee_with_drawings(
    handle: &DebuggerHandle,
    context: microscope::StopContext,
    path: &str,
    value: microscope::InspectedValue,
) -> microscope::InspectedValue {
    if let VariableState::Available {
        value: VariableValue::Address(_),
        ..
    } = &value.state
        && let Ok(pointee) = Expression::parse(&format!("*({path})"))
        && let Ok(Evaluation::Value { value: pointee, .. }) = handle
            .at(context)
            .evaluate_with(&pointee, EvaluationMode::Read, input_limits())
            .await
        && let VariableState::Available {
            presentation: Some(presentation),
            ..
        } = &pointee.state
        && !presentation.visualizers.is_empty()
    {
        pointee
    } else {
        value
    }
}

fn info(renderer: &microscope::Renderer) -> RendererInfo {
    RendererInfo {
        name: renderer.name.to_string(),
        origin: renderer.origin.to_string(),
        digest: renderer.digest.to_string(),
    }
}

/// Every renderer, each name once, as a drawing would find it.
pub async fn renderers(handle: &DebuggerHandle) -> Result<protocol::RendererList, Failure> {
    let mut list = Vec::<RendererInfo>::new();
    for renderer in handle.renderers().await?.iter() {
        if !list.iter().any(|seen| *seen.name == *renderer.name) {
            list.push(info(renderer));
        }
    }

    Ok(protocol::RendererList { renderers: list })
}

/// A renderer's JavaScript, by its digest.
pub async fn renderer(handle: &DebuggerHandle, digest: &str) -> Result<RendererSource, Failure> {
    handle
        .renderers()
        .await?
        .iter()
        .find(|renderer| *renderer.digest == *digest)
        .map(|renderer| RendererSource {
            info: info(renderer),
            source: renderer.source.to_string(),
        })
        .ok_or_else(|| {
            Failure::new(
                ErrorKind::StaleStop,
                "no renderer has that digest any longer; the views were reloaded",
            )
        })
}

/// One drawing's walk through its inputs.
struct Walk<'a> {
    handle: &'a DebuggerHandle,

    /// The bytes the inputs' numbers and bytes lie in.
    bytes: Vec<u8>,

    /// How many values the walk has read.
    values: u64,
}

type Walked = Result<Datum, String>;

impl Walk<'_> {
    /// Appends bytes, each run of numbers aligned to 8 so the page can view
    /// it as an array of its kind.
    fn append(&mut self, bytes: &[u8]) -> u64 {
        while !self.bytes.len().is_multiple_of(8) {
            self.bytes.push(0);
        }

        let offset = self.bytes.len() as u64;
        self.bytes.extend_from_slice(bytes);
        offset
    }

    fn bytes(&mut self, bytes: &[u8]) -> Datum {
        Datum::Bytes {
            offset: self.append(bytes),
            length: bytes.len() as u64,
        }
    }

    /// Counts one value against the drawing's limit.
    fn count(&mut self, path: &str, values: u64) -> Result<(), String> {
        self.values = self.values.saturating_add(values);
        if self.values > MOST_VALUES {
            return Err(format!(
                "`{path}`: the inputs hold more than {MOST_VALUES} values; hand over bulk data with `bytes(PTR, LEN)` or name less"
            ));
        }

        Ok(())
    }

    /// A view's inputs, each by name, as read or why not, with the part of
    /// the drawn value it is.
    async fn inputs(
        &mut self,
        inputs: &microscope::VisualizerInputs,
    ) -> Vec<(String, Walked, Option<String>)> {
        let mut read = Vec::new();
        for input in inputs.inputs.iter() {
            let datum = match &input.value {
                microscope::VisualizerValue::Value(value) => {
                    self.value(value.type_info.as_ref(), &value.state, &input.name, 0)
                        .await
                }
                microscope::VisualizerValue::Bytes(bytes) => Ok(self.bytes(bytes)),
                microscope::VisualizerValue::Text(text) => Ok(Datum::Text {
                    s: text.to_string(),
                }),
                microscope::VisualizerValue::Problem(problem) => {
                    Err(format!("`{}`: {problem}", input.name))
                }
            };

            read.push((
                input.name.to_string(),
                datum,
                input.path.as_deref().map(str::to_owned),
            ));
        }

        read
    }

    /// One value, as its type decides.
    fn value<'b>(
        &'b mut self,
        type_info: Option<&'b TypeInfo>,
        state: &'b VariableState,
        path: &'b str,
        depth: usize,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Walked> + Send + 'b>> {
        Box::pin(async move {
            if depth > MOST_DEPTH {
                return Err(format!(
                    "`{path}`: the inputs nest more than {MOST_DEPTH} deep"
                ));
            }

            self.count(path, 1)?;
            let VariableState::Available {
                value: raw,
                text,
                presentation,
                children,
                ..
            } = state
            else {
                return Err(format!("`{path}`: {}", unreadable(state)));
            };

            if let Some(text) = text {
                return match &text.completion {
                    TextCompletion::Complete => Ok(Datum::Text {
                        s: String::from_utf8_lossy(&text.bytes).into_owned(),
                    }),
                    TextCompletion::Truncated { .. } | TextCompletion::Limited { .. } => {
                        Err(format!(
                            "`{path}`: its text is longer than a drawing reads, {} bytes",
                            microscope::TextSummary::MAX_BYTES
                        ))
                    }
                    TextCompletion::Unreadable { address } => {
                        Err(format!("`{path}`: its text at {address} cannot be read"))
                    }
                };
            }

            if let VariableValue::Variant { active, .. } = raw {
                return self.sum(active.as_deref(), children, path, depth).await;
            }

            // A view decides first: a Go map or a smart pointer is stored as an
            // address but presents what it holds.
            if let Some(presentation) = presentation {
                match presentation.shape {
                    PresentedShape::Raw => {
                        return Err(format!(
                            "`{path}`: its view failed: {}",
                            presentation.summary
                        ));
                    }
                    PresentedShape::Formatted => {}
                    PresentedShape::Text => {
                        return Err(format!("`{path}`: its text cannot be read"));
                    }
                    PresentedShape::Empty => return Ok(Datum::Null),
                    PresentedShape::Value => {
                        if let Some(inner) = &presentation.presented {
                            return self
                                .value(inner.type_info.as_ref(), &inner.state, path, depth + 1)
                                .await;
                        }

                        return self.record(&presentation.children, path, depth).await;
                    }
                    PresentedShape::Dynamic | PresentedShape::Record => {
                        return self.record(&presentation.children, path, depth).await;
                    }
                    PresentedShape::Sequence => {
                        let count = exact(presentation.count, path)?;
                        return self
                            .sequence(&presentation.children, count, None, path, depth)
                            .await;
                    }
                    PresentedShape::Map => {
                        let count = exact(presentation.count, path)?;
                        return self.map(&presentation.children, count, path, depth).await;
                    }
                    _ => return Err(format!("`{path}`: a value of this kind cannot be drawn")),
                }
            }

            // Pointers no view presents stay addresses: the walk follows none.
            match raw {
                VariableValue::Address(address) => {
                    return Ok(Datum::Big {
                        big: address.address.get().to_string(),
                    });
                }
                VariableValue::Function { code, .. } => {
                    return Ok(Datum::Big {
                        big: code.map_or(0, microscope::VirtualAddress::get).to_string(),
                    });
                }
                _ => {}
            }

            self.stored(type_info, raw, children, path, depth).await
        })
    }

    /// A value as stored, which no view presents.
    async fn stored(
        &mut self,
        type_info: Option<&TypeInfo>,
        raw: &VariableValue,
        children: &ValueChildren,
        path: &str,
        depth: usize,
    ) -> Walked {
        match raw {
            VariableValue::Scalar(scalar) => Ok(scalar_datum(scalar, type_info)),
            VariableValue::Enumeration {
                value: number,
                matches,
            } => {
                let name = matches
                    .iter()
                    .find(|enumerator| enumerator.value == *number)
                    .map(|enumerator| enumerator.name.to_string());
                let size = type_info.and_then(|info| info.byte_size).unwrap_or(8);
                let datum = integer_datum(*number, size);
                Ok(Datum::Enum {
                    name,
                    value: Box::new(datum),
                })
            }
            VariableValue::Array { dimensions } => {
                let counts = dimensions
                    .iter()
                    .map(|dimension| dimension.count)
                    .collect::<Vec<_>>();
                let total = counts
                    .iter()
                    .try_fold(1_u64, |total, count| total.checked_mul(*count));
                let total = total.ok_or_else(|| format!("`{path}`: the array is too large"))?;
                self.sequence(children, total, Some(&counts), path, depth)
                    .await
            }
            VariableValue::Slice { length, .. } => {
                self.sequence(children, *length, None, path, depth).await
            }
            VariableValue::Record | VariableValue::Union => {
                self.record(children, path, depth).await
            }
            VariableValue::ImplicitPointer => Err(format!(
                "`{path}`: the compiler left out the pointer itself"
            )),
            VariableValue::NotAllocated | VariableValue::NotAssociated => Ok(Datum::Null),
            _ => Err(format!("`{path}`: a value of this kind cannot be drawn")),
        }
    }

    /// The variant a sum type holds and its payload: its one member, or a
    /// record of several, or nothing.
    async fn sum(
        &mut self,
        active: Option<&microscope::Variant>,
        children: &ValueChildren,
        path: &str,
        depth: usize,
    ) -> Walked {
        let Some(active) = active else {
            return Err(format!("`{path}`: it holds no variant its type describes"));
        };

        let members = active.members.len();
        let variant = active
            .name
            .clone()
            .or_else(|| match &*active.members {
                [member] => member.name.clone(),
                _ => None,
            })
            .map_or_else(|| "<unnamed variant>".to_owned(), |name| name.to_string());
        if members == 0 {
            return Ok(Datum::Sum {
                variant,
                value: None,
            });
        }

        let children = self.children(children, path).await?;
        let mut payload = children[children.len().saturating_sub(members)..].to_vec();

        // A Rust variant's one member is a structure named for the variant,
        // which holds the variant's fields as stored.
        if let [only] = &payload[..]
            && child_name(only).as_deref() == Some(variant.as_str())
            && let VariableState::Available {
                presentation,
                children,
                ..
            } = &only.state
            && presentation
                .as_deref()
                .is_none_or(Presentation::is_rust_tuple)
        {
            payload = self.children(children, path).await?;
        }

        if payload.is_empty() {
            return Ok(Datum::Sum {
                variant,
                value: None,
            });
        }

        let payload = if let [only] = &payload[..] {
            self.value(
                Some(&only.type_info),
                &only.state,
                &format!("{path}.{variant}"),
                depth + 1,
            )
            .await?
        } else {
            let mut fields = Vec::new();
            for child in &payload {
                let name = child_name(child).unwrap_or_default();
                let datum = self
                    .value(
                        Some(&child.type_info),
                        &child.state,
                        &format!("{path}.{name}"),
                        depth + 1,
                    )
                    .await?;
                fields.push((name, datum));
            }

            tuple_or_record(fields)
        };

        Ok(Datum::Sum {
            variant,
            value: Some(Box::new(payload)),
        })
    }

    /// A record's members and a view's fields, by name; `[raw]` never.
    async fn record(&mut self, children: &ValueChildren, path: &str, depth: usize) -> Walked {
        let children = self.children(children, path).await?;
        let mut members = Vec::new();
        for child in &children {
            let Some(name) = child_name(child) else {
                continue;
            };

            let datum = self
                .value(
                    Some(&child.type_info),
                    &child.state,
                    &format!("{path}.{name}"),
                    depth + 1,
                )
                .await?;
            members.push((name, datum));
        }

        Ok(tuple_or_record(members))
    }

    /// A map's entries, each its key and value.
    async fn map(
        &mut self,
        children: &ValueChildren,
        count: u64,
        path: &str,
        depth: usize,
    ) -> Walked {
        let ValueChildren::Available(reference) = children else {
            return Err(format!("`{path}`: its entries cannot be read"));
        };

        self.count(path, count.saturating_mul(2))?;
        let mut entries = Vec::new();
        for child in self.page(reference, 0, count, path).await? {
            let ValueChildRelationship::Entry { index, key } = &child.relationship else {
                continue;
            };

            let at = format!("{path}[{index}]");
            let key = self
                .value(Some(&key.type_info), &key.state, &at, depth + 1)
                .await?;
            let value = self
                .value(Some(&child.type_info), &child.state, &at, depth + 1)
                .await?;
            entries.push((key, value));
        }

        Ok(Datum::Entries { entries })
    }

    /// A sequence's `count` elements: numbers in bulk when they lie next
    /// to each other, otherwise each in turn. An array of several
    /// dimensions is rows of rows.
    async fn sequence(
        &mut self,
        children: &ValueChildren,
        count: u64,
        dimensions: Option<&[u64]>,
        path: &str,
        depth: usize,
    ) -> Walked {
        let ValueChildren::Available(reference) = children else {
            return if count == 0 {
                Ok(Datum::List { items: Vec::new() })
            } else {
                Err(format!("`{path}`: its elements cannot be read"))
            };
        };

        let room = microscope::MAX_DRAWING_BYTES.saturating_sub(self.bytes.len() as u64);
        let numbers = self
            .handle
            .numbers(Arc::clone(reference), room)
            .await
            .map_err(|error| format!("`{path}`: {error}"))?;
        if let Some(numbers) = numbers.filter(|numbers| numbers.count == count) {
            self.count(path, count / 64)?;
            let offset = self.append(&numbers.bytes);
            return Ok(rows(
                kind(numbers.kind),
                offset,
                count,
                dimensions.unwrap_or(&[count]),
            ));
        }

        self.count(path, count)?;
        let children = self.page(reference, 0, count, path).await?;

        // Numbers of one kind are an array of that kind.
        let kinds = children
            .iter()
            .map(|child| number_kind(&child.type_info, &child.state))
            .collect::<Vec<_>>();
        if let Some(Some(first)) = kinds.first()
            && kinds.iter().all(|kind| *kind == Some(*first))
        {
            let mut bytes = Vec::with_capacity(children.len() * 8);
            for child in &children {
                let VariableState::Available {
                    value: VariableValue::Scalar(scalar),
                    ..
                } = &child.state
                else {
                    return Err(format!("`{path}`: its numbers are not all scalars"));
                };

                number_bytes(*first, scalar, &mut bytes);
            }

            let offset = self.append(&bytes);
            return Ok(rows(*first, offset, count, dimensions.unwrap_or(&[count])));
        }

        let mut items = Vec::with_capacity(children.len());
        for (index, child) in children.iter().enumerate() {
            items.push(
                self.value(
                    Some(&child.type_info),
                    &child.state,
                    &format!("{path}[{index}]"),
                    depth + 1,
                )
                .await?,
            );
        }

        Ok(nest(items, dimensions.unwrap_or(&[count])))
    }

    /// Every child of a value.
    async fn children(
        &self,
        children: &ValueChildren,
        path: &str,
    ) -> Result<Vec<microscope::ValueChild>, String> {
        match children {
            ValueChildren::NotApplicable => Ok(Vec::new()),
            ValueChildren::Unavailable(reason) => Err(format!("`{path}`: {reason}")),
            ValueChildren::Available(reference) => {
                self.page(reference, 0, reference.total(), path).await
            }
            _ => Err(format!("`{path}`: its parts cannot be read")),
        }
    }

    /// Children `start..start + count` of a value, a page at a time.
    async fn page(
        &self,
        reference: &Arc<ValueChildrenReference>,
        start: u64,
        count: u64,
        path: &str,
    ) -> Result<Vec<microscope::ValueChild>, String> {
        let mut children = Vec::new();
        let end = start.saturating_add(count);
        let mut next = start;
        while next < end {
            let limit = u32::try_from(end - next).unwrap_or(u32::MAX).min(PAGE);
            let page = self
                .handle
                .value_children_with_limits(
                    Arc::clone(reference),
                    ValueChildQuery {
                        offset: next,
                        limit,
                    },
                    input_limits(),
                )
                .await
                .map_err(|error| format!("`{path}`: {error}"))?;
            if page.children.is_empty() {
                return Err(format!(
                    "`{path}`: reading its children stopped at {next}: {:?}",
                    page.completion
                ));
            }

            next += page.children.len() as u64;
            children.extend(page.children.iter().cloned());
        }

        Ok(children)
    }
}

/// A count a view declared exactly, or why a drawing cannot use it.
fn exact(count: Option<PresentedCount>, path: &str) -> Result<u64, String> {
    match count {
        Some(PresentedCount::Exact(count)) => Ok(count),
        Some(PresentedCount::AtLeast(count)) => Err(format!(
            "`{path}`: it holds at least {count} elements, and counting them all ran out of budget"
        )),
        None => Ok(0),
        Some(_) => Err(format!("`{path}`: its count is not known")),
    }
}

/// A child's name in a record: a member's or field's, or a base's type.
fn child_name(child: &microscope::ValueChild) -> Option<String> {
    match &child.relationship {
        ValueChildRelationship::Member(member) => member.name.as_deref().map(str::to_owned),
        ValueChildRelationship::Field { name } => Some(name.to_string()),
        ValueChildRelationship::Base(_) => Some(child.type_info.name.to_string()),
        _ => None,
    }
}

/// An integer: a number when its type is at most 32 bits wide, otherwise
/// a bigint.
fn integer_datum(value: microscope::IntegerValue, size: u64) -> Datum {
    let (wide, text) = match value {
        microscope::IntegerValue::Signed(value) => (i64::try_from(value).ok(), value.to_string()),
        microscope::IntegerValue::Unsigned(value) => (i64::try_from(value).ok(), value.to_string()),
        _ => (None, String::new()),
    };

    match wide {
        Some(i) if size <= 4 => Datum::Int { i },
        _ => Datum::Big { big: text },
    }
}

/// Members named `__0`, `__1`, … in order, as a Rust tuple's are, are a
/// tuple, which is an array.
fn tuple_or_record(members: Vec<(String, Datum)>) -> Datum {
    let tuple = !members.is_empty()
        && members
            .iter()
            .enumerate()
            .all(|(index, (name, _))| *name == format!("__{index}"));
    if tuple {
        Datum::List {
            items: members.into_iter().map(|(_, datum)| datum).collect(),
        }
    } else {
        Datum::Record { members }
    }
}

/// A run of numbers as rows of `dimensions`: one array for one dimension.
fn rows(kind: NumberKind, offset: u64, count: u64, dimensions: &[u64]) -> Datum {
    match dimensions {
        [] | [_] => Datum::Numbers {
            kind,
            offset,
            count,
        },
        [outer, inner @ ..] => {
            let each = count / (*outer).max(1);
            let size = number_size(kind) as u64;
            Datum::List {
                items: (0..*outer)
                    .map(|row| rows(kind, offset + row * each * size, each, inner))
                    .collect(),
            }
        }
    }
}

/// Elements as rows of `dimensions`.
fn nest(items: Vec<Datum>, dimensions: &[u64]) -> Datum {
    match dimensions {
        [] | [_] => Datum::List { items },
        [outer, inner @ ..] => {
            let each = items.len() / usize::try_from(*outer).unwrap_or(1).max(1);
            let mut items = items.into_iter();
            Datum::List {
                items: (0..*outer)
                    .map(|_| nest(items.by_ref().take(each).collect(), inner))
                    .collect(),
            }
        }
    }
}

const fn kind(kind: microscope::NumberKind) -> NumberKind {
    match kind {
        microscope::NumberKind::I8 => NumberKind::I8,
        microscope::NumberKind::U8 => NumberKind::U8,
        microscope::NumberKind::I16 => NumberKind::I16,
        microscope::NumberKind::U16 => NumberKind::U16,
        microscope::NumberKind::I32 => NumberKind::I32,
        microscope::NumberKind::U32 => NumberKind::U32,
        microscope::NumberKind::I64 => NumberKind::I64,
        microscope::NumberKind::U64 => NumberKind::U64,
        microscope::NumberKind::F32 => NumberKind::F32,
        microscope::NumberKind::F64 => NumberKind::F64,
    }
}

const fn number_size(kind: NumberKind) -> usize {
    match kind {
        NumberKind::I8 | NumberKind::U8 => 1,
        NumberKind::I16 | NumberKind::U16 => 2,
        NumberKind::I32 | NumberKind::U32 | NumberKind::F32 => 4,
        NumberKind::I64 | NumberKind::U64 | NumberKind::F64 => 8,
    }
}

/// The kind of number a scalar child is, by its type: an integer of at
/// most 64 bits, or a 32- or 64-bit float. A character wider than a byte
/// is a character, and a truth value is not a number.
fn number_kind(type_info: &TypeInfo, state: &VariableState) -> Option<NumberKind> {
    let VariableState::Available {
        value: VariableValue::Scalar(scalar),
        text: None,
        ..
    } = state
    else {
        return None;
    };

    if wide_character(type_info) {
        return None;
    }

    Some(match (scalar, type_info.byte_size?) {
        (ScalarValue::Signed(_), 1) => NumberKind::I8,
        (ScalarValue::Unsigned(_), 1) => NumberKind::U8,
        (ScalarValue::Signed(_), 2) => NumberKind::I16,
        (ScalarValue::Unsigned(_), 2) => NumberKind::U16,
        (ScalarValue::Signed(_), 4) => NumberKind::I32,
        (ScalarValue::Unsigned(_), 4) => NumberKind::U32,
        (ScalarValue::Signed(_), 8) => NumberKind::I64,
        (ScalarValue::Unsigned(_), 8) => NumberKind::U64,
        (ScalarValue::Floating(FloatValue::Binary32(_)), _) => NumberKind::F32,
        (ScalarValue::Floating(FloatValue::Binary64(_)), _) => NumberKind::F64,
        _ => return None,
    })
}

/// Writes a scalar as a number of `kind`, little-endian.
fn number_bytes(kind: NumberKind, scalar: &ScalarValue, bytes: &mut Vec<u8>) {
    let integer = match scalar {
        ScalarValue::Signed(value) => u128::from_ne_bytes(value.to_ne_bytes()),
        ScalarValue::Unsigned(value) => *value,
        ScalarValue::Floating(FloatValue::Binary32(bits)) => u128::from(*bits),
        ScalarValue::Floating(FloatValue::Binary64(bits)) => u128::from(*bits),
        _ => 0,
    };

    let size = number_size(kind);
    bytes.extend_from_slice(&integer.to_le_bytes()[..size]);
}

/// Whether a type is a character wider than a byte, which reaches a
/// renderer as a string of one character.
const fn wide_character(type_info: &TypeInfo) -> bool {
    matches!(
        &type_info.kind,
        TypeKind::Base(base)
            if matches!(base.encoding, BaseTypeEncoding::SignedCharacter | BaseTypeEncoding::UnsignedCharacter)
                && base.byte_size > 1
    )
}

/// A scalar: a truth value, a number of at most 32 bits, a bigint for a
/// wider integer, a float, or a character.
fn scalar_datum(scalar: &ScalarValue, type_info: Option<&TypeInfo>) -> Datum {
    // An integer a view writes, such as `64`, has no width: it is a number
    // when a double holds it exactly.
    let size = type_info
        .and_then(|info| info.byte_size)
        .unwrap_or_else(|| {
            let exact = match scalar {
                ScalarValue::Signed(value) => value.unsigned_abs() <= MOST_EXACT,
                ScalarValue::Unsigned(value) => *value <= MOST_EXACT,
                _ => false,
            };

            if exact { 4 } else { 8 }
        });
    let character = type_info.is_some_and(wide_character);

    match scalar {
        ScalarValue::Boolean(value) => Datum::Bool { b: *value },
        ScalarValue::Signed(value) if character => {
            character_datum(u128::from_ne_bytes(value.to_ne_bytes()))
        }
        ScalarValue::Unsigned(value) if character => character_datum(*value),
        ScalarValue::Signed(value) if size <= 4 => Datum::Int {
            i: i64::try_from(*value).unwrap_or_default(),
        },
        ScalarValue::Unsigned(value) if size <= 4 => Datum::Int {
            i: i64::try_from(*value).unwrap_or_default(),
        },
        ScalarValue::Signed(value) => Datum::Big {
            big: value.to_string(),
        },
        ScalarValue::Unsigned(value) => Datum::Big {
            big: value.to_string(),
        },
        ScalarValue::Floating(value) => Datum::Float { f: float(*value) },
        ScalarValue::Complex { real, imaginary } => Datum::Record {
            members: vec![
                ("real".to_owned(), Datum::Float { f: float(*real) }),
                (
                    "imag".to_owned(),
                    Datum::Float {
                        f: float(*imaginary),
                    },
                ),
            ],
        },
        _ => Datum::Null,
    }
}

fn character_datum(code: u128) -> Datum {
    let character = u32::try_from(code)
        .ok()
        .and_then(char::from_u32)
        .unwrap_or(char::REPLACEMENT_CHARACTER);

    Datum::Text {
        s: character.to_string(),
    }
}

/// A float as the shortest text that reads back as its nearest double, as
/// JavaScript's `Number` reads it. A 32-bit float is its exact double, as
/// it is in a `Float32Array`.
fn float(value: FloatValue) -> String {
    let double = match value {
        FloatValue::Binary32(bits) => Some(f64::from(f32::from_bits(bits))),
        FloatValue::Binary64(bits) => Some(f64::from_bits(bits)),
        _ => None,
    };

    let text = double.map_or_else(
        || microscope::float_text(value),
        |double| double.to_string(),
    );

    match text.as_str() {
        "inf" => "Infinity".to_owned(),
        "-inf" => "-Infinity".to_owned(),
        _ => text,
    }
}

/// Why a value that is not available cannot be drawn.
fn unreadable(state: &VariableState) -> String {
    match state {
        VariableState::Unavailable(reason) => reason.to_string(),
        VariableState::Invalid { reason, .. } => format!("{reason:?}"),
        VariableState::Malformed(reason) => reason.description.to_string(),
        VariableState::Available { .. } => "available".to_owned(),
    }
}
