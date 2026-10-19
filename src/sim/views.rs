//! The views oracle: what the debugger presents of the `containers`
//! program's vector, list, and table, and the pages of their elements,
//! against what simulated memory holds.
//!
//! The oracle knows the program's containers from its C source, never from
//! microscope's reading of the debug information: their layouts are fixed by
//! the x86-64 C ABI whichever compiler built them. It walks memory as the
//! program's own views say (`tests/golden/containers/containers.views`),
//! exactly, so it can say which elements the debugger must show, where each
//! is, and which problem a broken container must end in.

use crate::{
    InspectedValue, InspectionLimits, PresentedCount, PresentedShape, ValueChildPage,
    ValueChildRelationship, VariableState, VariableValueSource, ViewProblem,
};

use super::marks::Mark;

/// One element or entry: where its value, and an entry's key, are.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub key: Option<(u64, Vec<u8>)>,
    pub value: (u64, Vec<u8>),
}

/// What a container holds, by its views' rules.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Truth {
    Items(Vec<Item>),

    /// The view refuses the container, for this reason.
    Refused(Refusal),
}

/// Why a view refuses a container.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// A check fails: more elements than room.
    Check,

    /// A node leads back to one visited before this element.
    Cycle { at: u64 },

    /// The generators end before the declared count.
    Short { declared: u64, generated: u64 },
}

/// The containers the program defines, by the name of the global holding
/// each.
pub const CONTAINERS: [&str; 3] = ["vec", "list", "table"];

/// How the most nodes a list walk follows, beyond any the program links.
const MAX_WALK: u64 = 64;

/// What the container in global `name` at `address` holds, reading memory
/// through `read`; `None` when memory cannot say.
pub fn truth(
    name: &str,
    address: u64,
    read: &impl Fn(u64, u64) -> Option<Vec<u8>>,
) -> Option<Truth> {
    let word = |at: u64| Some(u64::from_le_bytes(read(at, 8)?.try_into().ok()?));
    let item = |at: u64, size: u64| Some((at, read(at, size)?));

    match name {
        // {i32 *data; u64 n; u64 cap}
        "vec" => {
            let (data, n, cap) = (word(address)?, word(address + 8)?, word(address + 16)?);
            if n > cap {
                return Some(Truth::Refused(Refusal::Check));
            }

            let mut items = Vec::new();
            for index in 0..n {
                items.push(Item {
                    key: None,
                    value: item(data.checked_add(index * 4)?, 4)?,
                });
            }

            Some(Truth::Items(items))
        }

        // {node *head; u64 count}, nodes {i32 value; node *next}
        "list" => {
            let (head, count) = (word(address)?, word(address + 8)?);
            let mut items = Vec::new();
            let mut visited = Vec::new();
            let mut node = head;

            while (items.len() as u64) < count {
                if node == 0 || items.len() as u64 > MAX_WALK {
                    return Some(Truth::Refused(Refusal::Short {
                        declared: count,
                        generated: items.len() as u64,
                    }));
                }

                if visited.contains(&node) {
                    return Some(Truth::Refused(Refusal::Cycle {
                        at: items.len() as u64,
                    }));
                }

                visited.push(node);
                items.push(Item {
                    key: None,
                    value: item(node, 4)?,
                });
                let next = word(node + 8)?;

                // A ring ends where it began.
                node = if next == head { 0 } else { next };
            }

            Some(Truth::Items(items))
        }

        // {slot *slots; u64 cap; u64 n}, slots {i32 used; i32 key; i32 value}
        "table" => {
            let (slots, cap, n) = (word(address)?, word(address + 8)?, word(address + 16)?);
            if n > cap {
                return Some(Truth::Refused(Refusal::Check));
            }

            let mut items = Vec::new();

            for index in 0..cap.min(MAX_WALK) {
                if items.len() as u64 == n {
                    break;
                }

                let slot = slots.checked_add(index * 12)?;
                if read(slot, 4)? != [0; 4] {
                    items.push(Item {
                        key: Some(item(slot + 4, 4)?),
                        value: item(slot + 8, 4)?,
                    });
                }
            }

            if (items.len() as u64) < n {
                return Some(Truth::Refused(Refusal::Short {
                    declared: n,
                    generated: items.len() as u64,
                }));
            }

            Some(Truth::Items(items))
        }
        _ => None,
    }
}

/// What the debugger showed of one container: its presentation, and its
/// elements in one page and in pages of another size.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Shown {
    pub shape: PresentedShape,
    pub count: Option<PresentedCount>,
    pub problem: Option<ViewProblem>,

    /// Elements from one page, then from pages of the client's size.
    pub whole: Vec<Item>,
    pub paged: Vec<Item>,

    /// Whether a page of each said its budget ran out, which alone ends
    /// the elements early.
    pub truncated: [bool; 2],
}

/// Where a child's value is and its bytes, when it was read from memory.
fn place(state: &VariableState) -> Option<(u64, Vec<u8>)> {
    match state {
        VariableState::Available {
            source: VariableValueSource::Memory(address),
            raw: Some(raw),
            ..
        } => Some((address.get(), raw.to_vec())),
        _ => None,
    }
}

/// The elements or entries of pages of a presentation's children.
pub fn items(pages: &[ValueChildPage]) -> Result<Vec<Item>, String> {
    let mut items = Vec::new();

    for page in pages {
        for child in page.children.iter() {
            let (index, key) = match &child.relationship {
                ValueChildRelationship::Element { index } => (*index, None),
                ValueChildRelationship::Entry { index, key } => (*index, Some(&key.state)),
                _ => continue,
            };

            if index != items.len() as u64 {
                return Err(format!(
                    "child {index} came where child {} belongs",
                    items.len()
                ));
            }

            let value = place(&child.state)
                .ok_or_else(|| format!("element {index} was read from no memory: {child:?}"))?;
            let key = match key {
                Some(key) => Some(
                    place(key)
                        .ok_or_else(|| format!("key {index} was read from no memory: {key:?}"))?,
                ),
                None => None,
            };

            items.push(Item { key, value });
        }
    }

    Ok(items)
}

/// Views: a container's presentation is what its view makes of memory: its
/// elements or entries, each read from where the walk finds it and holding
/// what memory holds there, the same in pages of any size; or, for a
/// broken one, the typed problem the walk ends in, and the value as
/// stored. Returns the marks the judgment reached.
pub fn judge(truth: &Truth, shown: &Shown) -> Result<Vec<Mark>, String> {
    let mut marks = Vec::new();

    match truth {
        Truth::Refused(refusal) => {
            let matches = match (refusal, &shown.problem) {
                (Refusal::Check, Some(ViewProblem::CheckFailed { .. })) => true,
                (Refusal::Cycle { at }, Some(ViewProblem::Cycle { at: shown })) => at == shown,
                (
                    Refusal::Short {
                        declared,
                        generated,
                    },
                    Some(ViewProblem::CountMismatch {
                        declared: shown_declared,
                        generated: shown_generated,
                    }),
                ) => declared == shown_declared && generated == shown_generated,
                _ => false,
            };

            if shown.shape != PresentedShape::Raw || !matches {
                return Err(format!(
                    "the view must refuse it, {refusal:?}, but presented it as {:?} with {:?}",
                    shown.shape, shown.problem
                ));
            }

            if matches!(refusal, Refusal::Cycle { .. }) {
                marks.push(Mark::ViewCycleRefused);
            }
        }
        Truth::Items(items) => {
            if !matches!(shown.shape, PresentedShape::Sequence | PresentedShape::Map) {
                return Err(format!(
                    "it holds {} elements, but was presented as {:?} with {:?}",
                    items.len(),
                    shown.shape,
                    shown.problem
                ));
            }

            if shown.count != Some(PresentedCount::Exact(items.len() as u64)) {
                return Err(format!(
                    "it holds {} elements, but its count is {:?}",
                    items.len(),
                    shown.count
                ));
            }

            for ((pages, truncated), which) in [&shown.whole, &shown.paged]
                .into_iter()
                .zip(shown.truncated)
                .zip(["one page", "small pages"])
            {
                if pages.len() > items.len()
                    || pages[..] != items[..pages.len()]
                    || pages.len() < items.len() && !truncated
                {
                    return Err(format!(
                        "in {which}, it shows {pages:?}, but holds {items:?}"
                    ));
                }
            }

            marks.push(Mark::ViewPresented);
            if shown.whole.len() == items.len() && shown.paged.len() == items.len() {
                marks.push(Mark::ViewPaged);
            }
        }
    }

    Ok(marks)
}

/// Whether a page used no more than its limits.
pub fn within(page: &ValueChildPage, limits: InspectionLimits) -> Result<(), String> {
    let usage = page.usage;
    if usage.memory_reads > limits.memory_reads
        || usage.memory_bytes > limits.memory_bytes
        || usage.expression_work > limits.expression_work
        || usage.value_nodes > limits.value_nodes
    {
        return Err(format!(
            "a page used {usage:?}, beyond its limits {limits:?}"
        ));
    }

    Ok(())
}

/// What one presentation's value showed, from the client's observation.
#[must_use]
pub fn shown(
    value: &InspectedValue,
    whole: Vec<Item>,
    paged: Vec<Item>,
    truncated: [bool; 2],
) -> Option<Shown> {
    let VariableState::Available {
        presentation: Some(presentation),
        ..
    } = &value.state
    else {
        return None;
    };

    Some(Shown {
        shape: presentation.shape,
        count: presentation.count,
        problem: presentation.problem.clone(),
        whole,
        paged,
        truncated,
    })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    /// Memory of a list of three nodes at 0x100, 0x110, and 0x120, holding
    /// 1, 2, and 3, whose head is at 0x10.
    fn memory() -> BTreeMap<u64, u8> {
        let mut memory = BTreeMap::new();
        let mut put = |address: u64, bytes: &[u8]| {
            for (offset, byte) in bytes.iter().enumerate() {
                memory.insert(address + offset as u64, *byte);
            }
        };

        put(0x10, &0x100_u64.to_le_bytes());
        put(0x18, &3_u64.to_le_bytes());
        for (index, next) in [(0_u64, 0x110_u64), (1, 0x120), (2, 0)] {
            let node = 0x100 + 0x10 * index;
            put(
                node,
                &(i32::try_from(index).expect("small") + 1).to_le_bytes(),
            );
            put(node + 8, &next.to_le_bytes());
        }

        memory
    }

    fn reader(memory: &BTreeMap<u64, u8>) -> impl Fn(u64, u64) -> Option<Vec<u8>> + '_ {
        |address, size| {
            (address..address + size)
                .map(|at| memory.get(&at).copied())
                .collect()
        }
    }

    fn list_shown(items: Vec<Item>) -> Shown {
        Shown {
            shape: PresentedShape::Sequence,
            count: Some(PresentedCount::Exact(3)),
            problem: None,
            whole: items.clone(),
            paged: items,
            truncated: [false; 2],
        }
    }

    /// Sabotage: a debugger that drops one element, or shows an element
    /// from somewhere else, fails the oracle.
    #[test]
    fn an_engine_that_drops_or_moves_an_element_fails() {
        let memory = memory();
        let truth = truth("list", 0x10, &reader(&memory)).expect("memory says");
        let Truth::Items(items) = &truth else {
            panic!("{truth:?}");
        };

        let mut dropped = list_shown(items.clone());
        dropped.whole.remove(1);
        assert!(judge(&truth, &dropped).is_err());

        // Only a page whose budget ran out may end the elements early.
        let mut ended = list_shown(items.clone());
        ended.paged.pop();
        assert!(judge(&truth, &ended).is_err());
        ended.truncated[1] = true;
        assert!(judge(&truth, &ended).is_ok());
        let mut moved = list_shown(items.clone());
        moved.paged[2].value.0 += 4;
        assert!(judge(&truth, &moved).is_err());
        let mut miscounted = list_shown(items.clone());
        miscounted.count = Some(PresentedCount::Exact(2));
        assert!(judge(&truth, &miscounted).is_err());
    }

    /// A cyclic list must end in the typed cycle problem, at the element
    /// that would repeat, and nothing else will do.
    #[test]
    fn a_cyclic_list_must_be_refused_as_a_cycle() {
        let mut memory = memory();
        for (offset, byte) in 0x110_u64.to_le_bytes().iter().enumerate() {
            memory.insert(0x128 + offset as u64, *byte);
        }

        for (offset, byte) in 5_u64.to_le_bytes().iter().enumerate() {
            memory.insert(0x18 + offset as u64, *byte);
        }

        let truth = truth("list", 0x10, &reader(&memory)).expect("memory says");
        assert_eq!(truth, Truth::Refused(Refusal::Cycle { at: 3 }));
        let refused = Shown {
            shape: PresentedShape::Raw,
            count: None,
            problem: Some(ViewProblem::Cycle { at: 3 }),
            whole: Vec::new(),
            paged: Vec::new(),
            truncated: [false; 2],
        };

        assert_eq!(judge(&truth, &refused), Ok(vec![Mark::ViewCycleRefused]));
        let elsewhere = Shown {
            problem: Some(ViewProblem::Cycle { at: 2 }),
            ..refused.clone()
        };

        assert!(judge(&truth, &elsewhere).is_err());
        let presented = Shown {
            shape: PresentedShape::Sequence,
            count: Some(PresentedCount::Exact(5)),
            problem: None,
            ..refused
        };

        assert!(judge(&truth, &presented).is_err());
    }
}
