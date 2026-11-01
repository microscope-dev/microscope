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

/// Line ranges in address order, with gaps no line describes between some.
fn lines() -> impl Strategy<Value = Vec<(AddressRange<ImageAddress>, SourceLocation)>> {
    proptest::collection::vec((0_u64..6, 1_u64..12, 0_u32..2, 1_u64..6), 0..8).prop_map(|rows| {
        let mut end = 0;
        rows.into_iter()
            .map(|(gap, length, file, line)| {
                let start = end + gap;
                end = start + length;
                (range(start, end), at(file, line))
            })
            .filter(|(row, _)| row.end.get() <= 64)
            .collect()
    })
}

/// A call inlined into instance 0, or into the first call, with its code
/// and where it was called from.
#[derive(Debug, Clone)]
struct Call {
    code: AddressRange<ImageAddress>,
    site: Option<SourceLocation>,
    nested: bool,
}

fn calls() -> impl Strategy<Value = Vec<Call>> {
    proptest::collection::vec(
        (0_u64..60, 1_u64..12, location(), prop::bool::weighted(0.2)),
        0..5,
    )
    .prop_map(|calls| {
        calls
            .into_iter()
            .enumerate()
            .map(|(index, (start, length, site, nested))| Call {
                code: range(start, (start + length).min(64)),
                site,
                nested: nested && index > 0,
            })
            .collect()
    })
}

/// An image whose line tables hold `lines`, and whose instance 0 covers
/// the 64 bytes of code with `calls` inlined into it: each call is
/// instance one past its index.
fn image(
    lines: &[(AddressRange<ImageAddress>, SourceLocation)],
    calls: &[Call],
) -> crate::image::Image {
    use crate::image::lines::{Files, LineTables, Row};
    use crate::{CodeInstanceInfo, CodeRole, FunctionId, FunctionInfo, SourceLanguage};

    let mut files = Files::default();
    for name in ["/a.go", "/b.go"] {
        files.intern(name.into());
    }

    let mut tables = LineTables::default();
    for (code, location) in lines {
        let row = |address, end_sequence: bool| Row {
            address,
            file: (!end_sequence).then_some(location.file),
            line: if end_sequence { 0 } else { location.line.get() },
            column: 0,
            operation_index: 0,
            discriminator: 0,
            isa: 0,
            statement: true,
            prologue_end: false,
            epilogue_begin: false,
            end_sequence,
        };
        tables.begin_sequence().expect("few rows");
        let at = tables
            .push_row(&row(code.start.get(), false))
            .expect("few rows");
        tables
            .push_row(&row(code.end.get(), true))
            .expect("few rows");
        tables.push_range(*code, at, true).expect("few rows");
    }

    let function = FunctionInfo {
        id: FunctionId::new(0),
        name: "main.f".into(),
        linkage_name: None,
        declaration: None,
        language: SourceLanguage::Go,
        role: CodeRole::Ordinary,
        enclosing: None,
        coroutine: None,
        generics: std::sync::Arc::from([]),
        main_subprogram: false,
    };
    let mut instances = vec![CodeInstanceInfo {
        id: CodeInstanceId::new(0),
        function: FunctionId::new(0),
        parent: None,
        kind: CodeInstanceKind::OutOfLine,
        ranges: [range(0, 64)].into(),
        breakpoint_entry: None,
    }];
    instances.extend((1_u32..).zip(calls).map(|(id, call)| CodeInstanceInfo {
        id: CodeInstanceId::new(id),
        function: FunctionId::new(0),
        parent: Some(CodeInstanceId::new(if call.nested { 1 } else { 0 })),
        kind: CodeInstanceKind::Inline {
            call_site: call.site.clone(),
        },
        ranges: [call.code].into(),
        breakpoint_entry: None,
    }));

    let mut builder = crate::image::Builder::new(crate::TargetDescription::X86_64);
    tables.add_to(&mut builder);
    let mut paths = crate::image::PathsBuilder::default();
    files
        .add_to(&mut builder, &mut paths)
        .expect("paths without NULs");
    let mut strings = crate::image::StringsBuilder::default();
    crate::image::functions::add_functions_to(&mut builder, &mut strings, &[function], &instances)
        .expect("one function");
    crate::image::functions::add_code_to(
        &mut builder,
        &crate::image::functions::Code {
            instances: &instances,
            prologue_ends: &[],
            instruction_starts: &[],
        },
    )
    .expect("few instances");
    builder.bytes(crate::image::TableKind::Paths, paths.into_bytes());
    builder.bytes(crate::image::TableKind::Strings, strings.into_bytes());
    builder
        .seal(crate::image::Limits::default())
        .expect("a valid image")
}

fn ranges() -> impl Strategy<Value = Vec<AddressRange<ImageAddress>>> {
    proptest::collection::vec(
        (0_u64..56, 1_u64..12).prop_map(|(start, length)| range(start, (start + length).min(64))),
        0..5,
    )
}

proptest! {
    /// The visible ranges a real image gives are exactly the addresses of
    /// the scope where, by the rows and calls it was built from, the
    /// local is visible: past its declaration's line in the line of the
    /// first call inlined directly into its scope there, or else of the
    /// code. Only boundaries the image reports cut the ranges, so a
    /// boundary it misses shows up as a wrong address.
    #[test]
    fn visible_ranges_hold_exactly_the_addresses_where_the_local_is_visible(
        lines in lines(),
        calls in calls(),
        scope in ranges(),
        line in 1_u64..6,
        in_instance in prop::bool::weighted(0.8),
    ) {
        let image = image(&lines, &calls);
        let scope = fused(scope);
        let declared = GoDeclaration {
            location: at(0, line),
            instance: in_instance.then_some(CodeInstanceId::new(0)),
        };
        let visible = visible_ranges(&image, &scope, &declared);
        let mut expected = Vec::<AddressRange<ImageAddress>>::new();

        for at in 0..64 {
            let address = ImageAddress::new(at);
            let call = calls
                .iter()
                .find(|call| in_instance && !call.nested && call.code.contains(address));
            let executing = match call {
                Some(call) => call.site.clone(),
                None => lines
                    .iter()
                    .find(|(row, _)| row.contains(address))
                    .map(|(_, location)| location.clone()),
            };

            if scope.iter().any(|range| range.contains(address))
                && shown(&declared.location, executing.as_ref())
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
