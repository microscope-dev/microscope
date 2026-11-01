//! Static array offsets, which a fixed layout gives, against the offsets a
//! layout bounded at run time gives.

use std::collections::BTreeSet;

use proptest::prelude::*;

use super::super::inspect::{array_source_indices, element_byte_offset};
use super::super::runtime_array::{adjacent_strides, element_offset};
use super::*;

/// An array's dimensions, its order and element size, and indices into it,
/// each within its dimension or a little outside.
fn array() -> impl Strategy<Value = (Vec<ArrayDimension>, ArrayOrdering, u64, Vec<i128>)> {
    let dimension = (
        prop_oneof![4 => -3_i128..=3, 1 => Just(i128::MIN), 1 => Just(i128::MAX - 3)],
        0_u64..=4,
    )
        .prop_map(|(lower_bound, count)| ArrayDimension { lower_bound, count });

    (
        proptest::collection::vec(dimension, 1..=3),
        prop::sample::select(vec![ArrayOrdering::RowMajor, ArrayOrdering::ColumnMajor]),
        1_u64..=16,
    )
        .prop_flat_map(|(dimensions, ordering, element_size)| {
            let indices = dimensions
                .iter()
                .map(|dimension| {
                    let first = dimension.lower_bound.saturating_sub(2);
                    let last = dimension
                        .lower_bound
                        .saturating_add(i128::from(dimension.count) + 1);
                    first..=last
                })
                .collect::<Vec<_>>();
            (
                Just(dimensions),
                Just(ordering),
                Just(element_size),
                indices,
            )
        })
}

/// Every source index of an array, in row-major order.
fn every_index(dimensions: &[ArrayDimension]) -> Vec<Vec<i128>> {
    dimensions
        .iter()
        .fold(vec![Vec::new()], |prefixes, dimension| {
            prefixes
                .iter()
                .flat_map(|prefix| {
                    (0..dimension.count).map(move |relative| {
                        let mut longer = prefix.clone();
                        longer.push(dimension.lower_bound + i128::from(relative));
                        longer
                    })
                })
                .collect()
        })
}

/// What finding an element's offset gives, from either layout.
#[derive(Debug, PartialEq, Eq)]
enum Offset {
    At(i64),
    OutOfBounds {
        index: i128,
        lower_bound: i128,
        count: u64,
    },
    Refused,
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    /// An element's offset is the same whether its array's layout is known
    /// statically or at run time, and so is the index refused as out of
    /// bounds.
    #[test]
    fn static_and_runtime_array_offsets_agree(
        (dimensions, ordering, element_size, indices) in array(),
    ) {
        let fixed = match element_byte_offset(&dimensions, ordering, &indices, element_size) {
            Ok(offset) => Offset::At(offset),
            Err(Error::ValueIndexOutOfBounds { index, lower_bound, count }) => {
                Offset::OutOfBounds { index, lower_bound, count }
            }
            Err(_) => Offset::Refused,
        };

        let bounds = dimensions
            .iter()
            .map(|dimension| (dimension.lower_bound, Some(dimension.count)))
            .collect::<Vec<_>>();
        let strides = adjacent_strides(&bounds, &vec![None; bounds.len()], ordering, element_size)
            .expect("small arrays have strides");
        let runtime = match element_offset(&bounds, &strides, &indices) {
            Ok(offset) => Offset::At(offset),
            Err(EvaluateError::Unavailable(VariableUnavailableReason::IndexOutOfBounds {
                index,
                lower_bound,
                count,
            })) => Offset::OutOfBounds { index, lower_bound, count },
            Err(_) => Offset::Refused,
        };

        prop_assert_eq!(&fixed, &runtime);

        // An index is out of bounds exactly when one of its parts is, and
        // the first such part is the one named.
        let outside = dimensions.iter().zip(&indices).find(|(dimension, index)| {
            **index < dimension.lower_bound
                || **index - dimension.lower_bound >= i128::from(dimension.count)
        });
        let named = outside.map(|(dimension, index)| (*index, dimension.lower_bound, dimension.count));
        let refused = match fixed {
            Offset::OutOfBounds { index, lower_bound, count } => Some((index, lower_bound, count)),
            _ => None,
        };
        prop_assert_eq!(refused, named);
    }

    /// Row-major element `k`'s source indices are at offset `k` elements,
    /// and in either order the elements' offsets are each multiple of the
    /// element size below the array's size exactly once.
    #[test]
    fn array_elements_fill_their_array_once(
        (dimensions, ordering, element_size, _) in array(),
    ) {
        let every = every_index(&dimensions);
        let elements = every.len() as u64;
        let offset = |ordering, indices: &[i128]| {
            element_byte_offset(&dimensions, ordering, indices, element_size)
                .expect("an index within bounds")
        };

        for (element, indices) in (0..elements).zip(&every) {
            let source = array_source_indices(&dimensions, element).expect("a non-empty array");
            prop_assert_eq!(&source, indices);
            prop_assert_eq!(offset(ArrayOrdering::RowMajor, &source), (element * element_size) as i64);
        }

        let offsets = every.iter().map(|indices| offset(ordering, indices)).collect::<BTreeSet<_>>();
        let expected = (0..elements).map(|element| (element * element_size) as i64).collect::<BTreeSet<_>>();
        prop_assert_eq!(offsets, expected);
    }
}
