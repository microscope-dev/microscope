use proptest::prelude::*;

use super::*;
use crate::{LineNumber, SourceFileId};

fn range(start: u64, end: u64) -> AddressRange<ImageAddress> {
    AddressRange {
        start: ImageAddress::new(start),
        end: ImageAddress::new(end),
    }
}

fn at(file: u32, line: u64) -> SourceLocation {
    SourceLocation {
        file: SourceFileId::new(file),
        line: LineNumber::new(line).expect("lines are one-based"),
        column: None,
    }
}

/// Line rows in line-program order, and calls inlined into instance 0 in
/// instance order.
struct Fake {
    rows: Vec<(AddressRange<ImageAddress>, SourceLocation)>,
    calls: Vec<(AddressRange<ImageAddress>, Option<SourceLocation>)>,
}

impl Code for Fake {
    fn call_at(&self, parent: CodeInstanceId, address: ImageAddress) -> Option<InlinedCall> {
        (parent == CodeInstanceId::new(0))
            .then(|| self.calls.iter().find(|(call, _)| call.contains(address)))
            .flatten()
            .map(|(_, site)| InlinedCall { site: site.clone() })
    }

    fn line_at(&self, address: ImageAddress) -> Option<SourceLocation> {
        self.rows
            .iter()
            .find(|(row, _)| row.contains(address))
            .map(|(_, location)| location.clone())
    }

    fn boundaries(
        &self,
        _: Option<CodeInstanceId>,
        _: AddressRange<ImageAddress>,
    ) -> Vec<ImageAddress> {
        self.rows
            .iter()
            .map(|(row, _)| *row)
            .chain(self.calls.iter().map(|(call, _)| *call))
            .flat_map(|range| [range.start, range.end])
            .collect()
    }
}

fn declared(location: SourceLocation) -> GoDeclaration {
    GoDeclaration {
        location,
        instance: Some(CodeInstanceId::new(0)),
    }
}

#[test]
fn a_go_local_is_visible_only_past_its_declarations_line() {
    // Lines 10, 11 (the declaration), 12, a call inlined at line 9, a row
    // of another file, line 13, and code with no line.
    let code = Fake {
        rows: vec![
            (range(0x10, 0x20), at(0, 10)),
            (range(0x20, 0x30), at(0, 11)),
            (range(0x30, 0x40), at(0, 12)),
            (range(0x40, 0x50), at(1, 70)),
            (range(0x50, 0x60), at(0, 3)),
            (range(0x60, 0x70), at(0, 13)),
        ],
        calls: vec![
            (range(0x40, 0x48), Some(at(0, 9))),
            (range(0x50, 0x60), Some(at(0, 14))),
        ],
    };

    let visible = visible_ranges(&code, &[range(0x10, 0x80)], &declared(at(0, 11)));
    assert_eq!(
        visible,
        [range(0x30, 0x40), range(0x48, 0x80)],
        "line 12, the other file's row past the call, the call at line 14 \
         though its own code is at line 3, line 13, and code with no line"
    );
}

fn location() -> impl Strategy<Value = Option<SourceLocation>> {
    proptest::option::of((0_u32..2, 1_u64..6).prop_map(|(file, line)| at(file, line)))
}

fn ranges() -> impl Strategy<Value = Vec<AddressRange<ImageAddress>>> {
    proptest::collection::vec(
        (0_u64..40, 1_u64..12).prop_map(|(start, length)| range(start, start + length)),
        0..5,
    )
}

proptest! {
    /// The visible ranges are exactly the addresses of the scope where a
    /// query there finds the local visible, however rows and calls overlap.
    #[test]
    fn visible_ranges_hold_exactly_the_addresses_a_query_finds_visible(
        rows in ranges(),
        row_locations in proptest::collection::vec((0_u32..2, 1_u64..6), 5),
        calls in ranges(),
        sites in proptest::collection::vec(location(), 5),
        scope in ranges(),
        line in 1_u64..6,
    ) {
        let code = Fake {
            rows: rows
                .into_iter()
                .zip(row_locations)
                .map(|(row, (file, line))| (row, at(file, line)))
                .collect(),
            calls: calls.into_iter().zip(sites).collect(),
        };

        let scope = fused(scope);
        let declared = declared(at(0, line));
        let visible = visible_ranges(&code, &scope, &declared);
        let mut expected = Vec::<AddressRange<ImageAddress>>::new();

        for at in 0..64 {
            let address = ImageAddress::new(at);
            if scope.iter().any(|range| range.contains(address))
                && visible_at(&code, &declared, address)
            {
                match expected.last_mut() {
                    Some(last) if last.end == address => last.end = ImageAddress::new(at + 1),
                    _ => expected.push(range(at, at + 1)),
                }
            }
        }

        prop_assert_eq!(visible, expected);
    }
}
