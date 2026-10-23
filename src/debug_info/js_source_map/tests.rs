use std::path::PathBuf;

use proptest::prelude::*;

use super::*;

fn base() -> Location {
    Location::Path(PathBuf::from("/work/dist/app.js.map"))
}

fn parse(json: &str) -> SourceMap {
    SourceMap::parse(json.as_bytes(), &base()).expect("a valid map")
}

fn parse_error(json: &str) -> SourceMapError {
    SourceMap::parse(json.as_bytes(), &base()).expect_err("a malformed map")
}

fn at(line: u32, column: u32) -> Position {
    Position::new(Line::from_zero_based(line), Column::from_zero_based(column))
}

fn place(source: u32, line: u32, column: u32, name: Option<u32>) -> OriginalPlace {
    OriginalPlace {
        source: SourceId::new(source),
        line: Line::from_zero_based(line),
        column: Column::from_zero_based(column),
        name: name.map(NameId::new),
    }
}

fn simple(sources: &str, names: &str, mappings: &str) -> String {
    format!(r#"{{"version":3,"sources":{sources},"names":{names},"mappings":"{mappings}"}}"#)
}

/// Line 0: `AAAA` maps column 0 to a.ts 0:0; `IAAI` moves 4 columns in
/// both, to 0:4; `EAAEC` moves 2 more and takes name +1, `bar`; `G` at
/// column 9 has no source. Line 1 is empty. Line 2 starts again at column
/// 0: `KCCK` is column 5 to b.ts (+1) 1:11 (+1, +5); `I` at column 9 has no
/// source; `EDDF` is column 11 back to a.ts (-1) 0:9 (-1, -2); `CAAAD` is
/// column 12 at the same place, with name -1, `foo`.
const WORKED: &str = "AAAA,IAAI,EAAEC,G;;KCCK,I,EDDF,CAAAD";

fn worked() -> SourceMap {
    parse(&simple(r#"["a.ts","b.ts"]"#, r#"["foo","bar"]"#, WORKED))
}

#[test]
fn segments_decode_to_their_worked_out_places() {
    let map = worked();
    assert_eq!(
        map.generated,
        [
            at(0, 0),
            at(0, 4),
            at(0, 6),
            at(0, 9),
            at(2, 5),
            at(2, 9),
            at(2, 11),
            at(2, 12)
        ]
    );
    assert_eq!(
        map.originals,
        [
            Some(place(0, 0, 0, None)),
            Some(place(0, 0, 4, None)),
            Some(place(0, 0, 6, Some(1))),
            None,
            Some(place(1, 1, 11, None)),
            None,
            Some(place(0, 0, 9, None)),
            Some(place(0, 0, 9, Some(0))),
        ]
    );
    assert_eq!(map.name(NameId::new(0)), Some("foo"));
    assert_eq!(map.name(NameId::new(1)), Some("bar"));
    assert_eq!(map.name(NameId::new(2)), None);
}

#[test]
fn a_position_takes_the_segment_that_covers_it_on_its_own_line() {
    let map = worked();
    let cases = [
        (at(0, 0), Some(place(0, 0, 0, None))),
        (at(0, 3), Some(place(0, 0, 0, None))),
        (at(0, 4), Some(place(0, 0, 4, None))),
        (at(0, 8), Some(place(0, 0, 6, Some(1)))),
        // The segment at column 9 has no source, through the line's end.
        (at(0, 9), None),
        (at(0, 500), None),
        // An empty line, after a line whose last segment had a place.
        (at(1, 0), None),
        // Before line 2's first segment, which starts at column 5.
        (at(2, 0), None),
        (at(2, 4), None),
        (at(2, 5), Some(place(1, 1, 11, None))),
        (at(2, 10), None),
        (at(2, 11), Some(place(0, 0, 9, None))),
        (at(2, 12), Some(place(0, 0, 9, Some(0)))),
        (at(3, 0), None),
    ];
    for (position, expected) in cases {
        assert_eq!(map.original_at(position), expected, "{position:?}");
    }

    assert_eq!(
        map.covering(at(0, 9)),
        Some(Covering {
            start: Column::from_zero_based(9),
            original: None
        })
    );
    assert_eq!(map.covering(at(2, 4)), None);
}

#[test]
fn an_original_line_leads_to_every_generated_position_mapped_to_it() {
    let map = worked();
    let a = SourceId::new(0);
    let b = SourceId::new(1);
    let line = |n| Line::from_zero_based(n);
    let found: Vec<(Position, u32)> = map
        .generated_for_line(a, line(0))
        .map(|found| (found.generated, found.original_column.zero_based()))
        .collect();
    assert_eq!(
        found,
        [
            (at(0, 0), 0),
            (at(0, 4), 4),
            (at(0, 6), 6),
            (at(2, 11), 9),
            (at(2, 12), 9)
        ]
    );
    assert_eq!(map.generated_for_line(a, line(1)).count(), 0);

    let column = |n| ColumnChoice::Covering(Column::from_zero_based(n));
    assert_eq!(
        map.breakpoint_positions(a, line(0), ColumnChoice::LineStart),
        [at(0, 0)]
    );
    assert_eq!(map.breakpoint_positions(a, line(0), column(5)), [at(0, 4)]);
    assert_eq!(
        map.breakpoint_positions(a, line(0), column(9)),
        [at(2, 11), at(2, 12)]
    );
    assert_eq!(
        map.breakpoint_positions(a, line(0), column(99)),
        [at(2, 11), at(2, 12)]
    );
    assert_eq!(
        map.breakpoint_positions(b, line(1), ColumnChoice::LineStart),
        [at(2, 5)]
    );
    assert_eq!(map.breakpoint_positions(b, line(1), column(10)), []);

    // Never the nearest line: nothing maps to a.ts line 1 or b.ts line 0.
    assert_eq!(
        map.breakpoint_positions(a, line(1), ColumnChoice::LineStart),
        []
    );
    assert_eq!(
        map.breakpoint_positions(b, line(0), ColumnChoice::LineStart),
        []
    );
}

/// Two segments at one position: the first is empty and covers nothing,
/// so neither direction leads through it.
#[test]
fn a_segment_another_starts_at_covers_nothing() {
    let map = parse(&simple(r#"["a.ts"]"#, "[]", "AAAA,AACA"));
    assert_eq!(map.original_at(at(0, 0)), Some(place(0, 1, 0, None)));
    let a = SourceId::new(0);
    assert_eq!(
        map.generated_for_line(a, Line::from_zero_based(0)).count(),
        0
    );
    assert_eq!(
        map.breakpoint_positions(a, Line::from_zero_based(1), ColumnChoice::LineStart),
        [at(0, 0)]
    );
}

/// Segments of a line may be listed out of column order; they are sorted
/// as ECMA-426 sorts them.
#[test]
fn segments_out_of_column_order_are_sorted() {
    // Column 4 to 0:4, then column 0 (-4) to 0:0 (-4).
    let map = parse(&simple(r#"["a.ts"]"#, "[]", "IAAI,JAAJ"));
    assert_eq!(map.generated, [at(0, 0), at(0, 4)]);
    assert_eq!(map.original_at(at(0, 2)), Some(place(0, 0, 0, None)));
}

#[test]
fn bases_convert_only_through_named_constructors() {
    assert_eq!(Line::from_one_based(1), Some(Line::from_zero_based(0)));
    assert_eq!(Line::from_one_based(0), None);
    assert_eq!(Column::from_one_based(7), Some(Column::from_zero_based(6)));
    assert_eq!(Column::from_zero_based(6).one_based(), Some(7));
    assert_eq!(Line::from_zero_based(u32::MAX).one_based(), None);
}

/// Section 0 starts at 0:0, with names `["unused"]`: `AAAA` 0:0 to one.ts
/// 0:0, `EAAE` 0:2 to 0:2, then `AACA` 1:0 to 1:2. Section 1 starts at
/// 1:10 with names `["x"]`: `AAAAA` local 0:0 is 1:10, to two.ts 0:0 named
/// `x`; `EAAE` local 0:2 is 1:12, to 0:2; `CAAC` local 1:1 is 2:1, since
/// the column offset moves the first line only, to 0:3.
const INDEX: &str = r#"{
    "version": 3,
    "file": "bundle.js",
    "sections": [
        {"offset": {"line": 0, "column": 0},
         "map": {"version": 3, "sources": ["one.ts"], "names": ["unused"], "mappings": "AAAA,EAAE;AACA"}},
        {"offset": {"line": 1, "column": 10},
         "map": {"version": 3, "sources": ["two.ts", "one.ts"], "names": ["x"], "mappings": "AAAAA,EAAE;CAAC,CCAA"}}
    ]
}"#;

#[test]
fn an_index_map_moves_each_section_by_its_offset() {
    let map = parse(INDEX);
    assert_eq!(map.file(), Some("bundle.js"));
    assert_eq!(
        map.generated,
        [
            at(0, 0),
            at(0, 2),
            at(1, 0),
            at(1, 10),
            at(1, 12),
            at(2, 1),
            at(2, 2)
        ]
    );

    let cases = [
        (at(1, 5), Some(place(0, 1, 2, None))),
        (at(1, 10), Some(place(1, 0, 0, Some(1)))),
        (at(1, 13), Some(place(1, 0, 2, None))),
        (at(2, 0), None),
        (at(2, 1), Some(place(1, 0, 3, None))),
        // Section 1's one.ts is section 0's: `CCAA` is index 1, 0:3.
        (at(2, 2), Some(place(0, 0, 3, None))),
    ];
    for (position, expected) in cases {
        assert_eq!(map.original_at(position), expected, "{position:?}");
    }

    assert_eq!(map.name(NameId::new(1)), Some("x"));
    assert_eq!(map.sources().len(), 2);
    let one = map.source(SourceId::new(0)).expect("one.ts");
    assert_eq!(
        one.location,
        Some(Location::Path(PathBuf::from("/work/dist/one.ts")))
    );
}

#[test]
fn index_maps_reject_sections_out_of_order_overlapping_or_nested() {
    let section = |line: u32, column: u32, mappings: &str| {
        format!(
            r#"{{"offset":{{"line":{line},"column":{column}}},"map":{}}}"#,
            simple(r#"["a.ts"]"#, "[]", mappings)
        )
    };
    let index =
        |sections: &[String]| format!(r#"{{"version":3,"sections":[{}]}}"#, sections.join(","));

    let unsorted = index(&[section(5, 0, "AAAA"), section(4, 0, "AAAA")]);
    assert!(matches!(
        parse_error(&unsorted),
        SourceMapError::Sections { section: 1, .. }
    ));

    // Section 0's last segment is 1:0, after section 1's start at 0:20.
    let overlapping = index(&[section(0, 0, "AAAA;AAAA"), section(0, 20, "AAAA")]);
    assert!(matches!(
        parse_error(&overlapping),
        SourceMapError::Sections { section: 1, .. }
    ));

    // A section may start where the one before it ends.
    let touching = index(&[section(0, 0, "AAAA;AAAA"), section(1, 0, "AAAA")]);
    assert_eq!(parse(&touching).segment_count(), 3);

    let nested = format!(
        r#"{{"version":3,"sections":[{{"offset":{{"line":0,"column":0}},"map":{}}}]}}"#,
        index(&[section(0, 0, "AAAA")])
    );
    assert!(matches!(
        parse_error(&nested),
        SourceMapError::Sections { section: 0, .. }
    ));

    let by_url = r#"{"version":3,"sections":[{"offset":{"line":0,"column":0},"url":"a.map"}]}"#;
    assert!(matches!(
        parse_error(by_url),
        SourceMapError::Sections { .. }
    ));

    let bad_inner = index(&[section(0, 0, "AAAA"), section(3, 0, "ACAA")]);
    assert!(matches!(
        parse_error(&bad_inner),
        SourceMapError::Section { section: 1, .. }
    ));

    let overflowing = index(&[section(u32::MAX, 0, "AAAA;AAAA")]);
    assert!(matches!(
        parse_error(&overflowing),
        SourceMapError::Section { section: 0, .. }
    ));

    let empty =
        r#"{"offset":{"line":0,"column":0},"map":{"version":3,"sources":[],"mappings":""}}"#;
    let too_many = format!(
        r#"{{"version":3,"sections":[{}]}}"#,
        vec![empty; MAX_SECTIONS + 1].join(",")
    );
    assert!(matches!(
        parse_error(&too_many),
        SourceMapError::TooLarge { .. }
    ));
}

#[test]
fn sources_resolve_through_the_root_and_the_map_location() {
    let json = r#"{"version":3,"sourceRoot":"../src","sources":
        ["a.ts","/abs/b.ts","webpack:///c.ts",null,"sub/%20d.ts","file:///home/u/e.ts"],
        "sourcesContent":["let a = 1;",null],
        "ignoreList":[2],
        "mappings":""}"#;
    let map = parse(json);
    let locations: Vec<Option<Location>> = map
        .sources()
        .map(|(_, source)| source.location.clone())
        .collect();
    let path = |p: &str| Some(Location::Path(PathBuf::from(p)));
    assert_eq!(
        locations,
        [
            path("/work/src/a.ts"),
            path("/work/src/abs/b.ts"),
            Some(Location::Url("webpack:///c.ts".into())),
            None,
            path("/work/src/sub/ d.ts"),
            path("/home/u/e.ts"),
        ]
    );

    let a = map.source(SourceId::new(0)).expect("a.ts");
    assert_eq!(a.name.as_deref(), Some("a.ts"));
    assert_eq!(a.content.as_deref(), Some("let a = 1;"));
    assert!(!a.ignored);
    let c = map.source(SourceId::new(2)).expect("c.ts");
    assert!(c.ignored);
    assert_eq!(c.content, None);
    assert_eq!(map.source(SourceId::new(3)).expect("null").name, None);

    // An empty root adds nothing, and a root of URL joins as a URL.
    let empty_root = parse(r#"{"version":3,"sourceRoot":"","sources":["a.ts"],"mappings":""}"#);
    assert_eq!(
        empty_root.source(SourceId::new(0)).expect("a.ts").location,
        path("/work/dist/a.ts")
    );
    let remote = SourceMap::parse(
        br#"{"version":3,"sourceRoot":"/static/","sources":["../a.ts","b.ts"],"mappings":""}"#,
        &Location::Url("https://example.com/js/app.js.map".into()),
    )
    .expect("a valid map");
    let remote: Vec<_> = remote.sources().map(|(_, s)| s.location.clone()).collect();
    assert_eq!(
        remote,
        [
            Some(Location::Url("https://example.com/a.ts".into())),
            Some(Location::Url("https://example.com/static/b.ts".into())),
        ]
    );
}

#[test]
fn the_last_source_mapping_comment_names_the_map() {
    let cases: [(&str, Option<&str>); 13] = [
        (
            "code();\n//# sourceMappingURL=app.js.map\n",
            Some("app.js.map"),
        ),
        ("code();\n//@ sourceMappingURL=old.map", Some("old.map")),
        (
            "//# sourceMappingURL=first.map\n//# sourceMappingURL=second.map",
            Some("second.map"),
        ),
        ("//# sourceMappingURL=a.map\r\n  \t\r\n", Some("a.map")),
        (
            "//# sourceMappingURL=a.map\u{2028}// later comment",
            Some("a.map"),
        ),
        ("//#   sourceMappingURL=spaced.map   ", Some("spaced.map")),
        ("//# sourceMappingURL=a.map\nfoo();", None),
        ("foo(); //# sourceMappingURL=a.map", None),
        ("let a = `\n//# sourceMappingURL=foo.js.map\n// `", None),
        ("//# sourceMappingURL=a.map */", None),
        (
            "//# sourceMappingURL=a.map\n//# sourceMappingURL=a b",
            Some("a.map"),
        ),
        ("// # sourceMappingURL=a.map", None),
        ("", None),
    ];
    for (script, expected) in cases {
        assert_eq!(source_mapping_url(script), expected, "{script:?}");
    }
}

#[test]
fn a_map_reference_resolves_from_the_script() {
    let script = Location::Path(PathBuf::from("/srv/dist/app.js"));
    let document = |path: &str| MapReference::Document(Location::Path(PathBuf::from(path)));
    let cases = [
        ("app.js.map", document("/srv/dist/app.js.map")),
        ("../maps/app.js.map?v=1#x", document("/srv/maps/app.js.map")),
        ("/abs/app.map", document("/abs/app.map")),
        ("file:///abs/x.map", document("/abs/x.map")),
        ("file://localhost/abs/y.map", document("/abs/y.map")),
        (
            "https://cdn.example/app.map",
            MapReference::Document(Location::Url("https://cdn.example/app.map".into())),
        ),
    ];
    for (url, expected) in cases {
        assert_eq!(MapReference::resolve(&script, url), Ok(expected), "{url}");
    }
    assert!(MapReference::resolve(&script, "file://elsewhere/x.map").is_err());
    assert!(MapReference::resolve(&script, "").is_err());

    let json = r#"{"version":3,"sources":["../src/a.ts"],"mappings":"AAAA"}"#;
    let encoded = base64_encode(json.as_bytes());
    for header in [
        "application/json;base64",
        "application/json;charset=utf-8;base64",
        "application/json;charset=UTF-8;base64",
        "Application/JSON;charset=utf8;base64",
    ] {
        let text = format!("x();\n//# sourceMappingURL=data:{header},{encoded}\n");
        let reference = locate(&script, &text)
            .expect("a valid data URL")
            .expect("a comment");
        assert_eq!(
            reference,
            MapReference::Inline(json.as_bytes().to_vec()),
            "{header}"
        );

        let MapReference::Inline(bytes) = &reference else {
            unreachable!()
        };
        let map = SourceMap::parse(bytes, reference.base(&script)).expect("the inline map");
        assert_eq!(
            map.source(SourceId::new(0)).expect("a.ts").location,
            Some(Location::Path(PathBuf::from("/srv/src/a.ts")))
        );
    }

    let percent = "data:application/json,%7B%22version%22:3%7D";
    assert_eq!(
        MapReference::resolve(&script, percent),
        Ok(MapReference::Inline(br#"{"version":3}"#.to_vec()))
    );
    for bad in [
        "data:text/plain;base64,e30=",
        "data:application/json;charset=latin1;base64,e30=",
        "data:application/json;base64,e30!",
        "data:application/json;base64,e",
        "data:application/json;base64",
    ] {
        assert!(
            matches!(
                MapReference::resolve(&script, bad),
                Err(SourceMapError::Url(_))
            ),
            "{bad}"
        );
    }
    assert_eq!(locate(&script, "no comment"), Ok(None));
}

#[test]
fn malformed_maps_are_typed_errors() {
    let one = r#"["a.ts"]"#;
    let mappings = |m: &str| parse_error(&simple(one, r#"["n"]"#, m));
    let reason = |error: SourceMapError| match error {
        SourceMapError::Mappings { offset, reason } => (offset, reason),
        other => panic!("not a mappings error: {other}"),
    };

    assert_eq!(
        reason(mappings("AA!A")),
        (2, "a character is not a base64 digit")
    );
    assert_eq!(
        reason(mappings("g")),
        (0, "a number ends without its last digit")
    );
    assert_eq!(
        reason(mappings("AAAg,A")),
        (3, "a number ends without its last digit")
    );
    assert_eq!(
        reason(mappings("AA")),
        (0, "a segment has neither 1, 4, nor 5 fields")
    );
    assert_eq!(
        reason(mappings("AAA")),
        (0, "a segment has neither 1, 4, nor 5 fields")
    );
    assert_eq!(
        reason(mappings("AAAAAA")),
        (0, "a segment has more than 5 fields")
    );
    assert_eq!(reason(mappings("AAAA,,AAAA")), (5, "a segment is empty"));
    assert_eq!(reason(mappings("AAAA,")), (5, "a segment is empty"));
    assert_eq!(reason(mappings(",AAAA")), (0, "a segment is empty"));
    assert_eq!(
        reason(mappings("ACAA")),
        (0, "a source index is out of range")
    );
    assert_eq!(
        reason(mappings("AAAAC")),
        (0, "a name index is out of range")
    );
    assert_eq!(
        reason(mappings("AADA")),
        (0, "an original line is out of range")
    );
    assert_eq!(
        reason(mappings("AAAD")),
        (0, "an original column is out of range")
    );
    assert_eq!(
        reason(mappings("D")),
        (0, "a generated column is out of range")
    );
    assert_eq!(
        reason(mappings("gggggggB")),
        (0, "a number exceeds 32 bits")
    );
    assert_eq!(reason(mappings("//////H")), (0, "a number exceeds 32 bits"));
    assert_eq!(
        reason(mappings("A;//////D")),
        (2, "a generated column is out of range")
    );
    // A name index needs names: a map without any has none to give.
    assert_eq!(
        reason(parse_error(&simple(one, "[]", "AAAAA"))),
        (0, "a name index is out of range")
    );
}

#[test]
fn malformed_json_and_fields_are_typed_errors() {
    assert!(matches!(parse_error("not json"), SourceMapError::Json(_)));
    assert!(matches!(parse_error("[]"), SourceMapError::Json(_)));
    assert!(matches!(
        SourceMap::parse(
            b"{\"version\":3,\"sources\":[\"\xff\"],\"mappings\":\"\"}",
            &base()
        ),
        Err(SourceMapError::Json(_))
    ));
    assert!(matches!(
        parse_error(r#"{"version":3,"sources":["a"],"names":[1],"mappings":""}"#),
        SourceMapError::Json(_)
    ));
    assert!(matches!(
        parse_error(r#"{"version":3,"sources":[],"mappings":"","mappings":""}"#),
        SourceMapError::Json(_)
    ));
    assert!(matches!(
        parse_error(r#"{"version":3,"sections":[{"offset":{"line":1e20,"column":0},"map":{}}]}"#),
        SourceMapError::Json(_)
    ));
    assert_eq!(
        parse_error(r#"{"version":2,"sources":[],"mappings":""}"#),
        SourceMapError::Version
    );
    assert_eq!(
        parse_error(r#"{"sources":[],"mappings":""}"#),
        SourceMapError::Version
    );
    assert_eq!(
        parse_error(r#"{"version":3,"sources":[]}"#),
        SourceMapError::MissingField("mappings")
    );
    assert_eq!(
        parse_error(r#"{"version":3,"mappings":""}"#),
        SourceMapError::MissingField("sources")
    );

    let deep = format!("{}{}", "[".repeat(100_000), "]".repeat(100_000));
    assert!(matches!(parse_error(&deep), SourceMapError::Json(_)));
}

#[test]
fn vlq_values_span_32_bits_with_the_sign() {
    let decode = |text: &str| {
        let mut at = 0;
        let value = decode_vlq(text.as_bytes(), &mut at);
        value.map(|value| (value, at))
    };
    assert_eq!(decode("A"), Ok((0, 1)));
    assert_eq!(decode("C"), Ok((1, 1)));
    assert_eq!(decode("D"), Ok((-1, 1)));
    // The spec's examples.
    assert_eq!(decode("iB"), Ok((17, 2)));
    assert_eq!(decode("V"), Ok((-10, 1)));
    // "-0" is -2^31.
    assert_eq!(decode("B"), Ok((-(1 << 31), 1)));
    assert_eq!(decode("+/////D"), Ok(((1 << 31) - 1, 7)));
    assert_eq!(decode("//////D"), Ok((-((1 << 31) - 1), 7)));
    // Zero digits past 32 bits add nothing, so they are allowed.
    assert_eq!(decode("ggggggggA"), Ok((0, 9)));
    assert!(decode("+/////H").is_err());
}

#[test]
#[should_panic(expected = "out of generated order")]
fn the_invariant_check_catches_unsorted_segments() {
    let mut map = worked();
    map.generated.swap(0, 1);
    map.check_invariants();
}

#[test]
#[should_panic(expected = "covers nothing")]
fn the_invariant_check_catches_an_empty_segment_in_the_original_index() {
    let mut map = parse(&simple(r#"["a.ts"]"#, "[]", "AAAA,AACA"));
    map.by_original.insert(0, 0);
    map.check_invariants();
}

fn base64_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let mut buffer = [0u8; 3];
        buffer[..chunk.len()].copy_from_slice(chunk);
        let n = (u32::from(buffer[0]) << 16) | (u32::from(buffer[1]) << 8) | u32::from(buffer[2]);
        for index in 0..4 {
            if index <= chunk.len() {
                out.push(char::from(
                    ALPHABET[((n >> (18 - 6 * index)) & 63) as usize],
                ));
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// One segment as a generator writes it: generated column, and original
/// source, line, column, and name index.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Written {
    pub column: u32,
    pub original: Option<(u32, u32, u32, Option<u32>)>,
}

pub(super) fn encode_vlq(value: i64, out: &mut String) {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut unsigned: u64 = if value == -(1 << 31) {
        1
    } else if value < 0 {
        (value.unsigned_abs() << 1) | 1
    } else {
        value.unsigned_abs() << 1
    };
    loop {
        let mut digit = unsigned & 31;
        unsigned >>= 5;
        if unsigned > 0 {
            digit |= 32;
        }
        out.push(char::from(
            ALPHABET[usize::try_from(digit).expect("a digit")],
        ));
        if unsigned == 0 {
            break;
        }
    }
}

/// Encodes lines of segments as a `mappings` string, each field relative
/// to the one before as ECMA-426 defines.
pub(super) fn encode_mappings(lines: &[Vec<Written>]) -> String {
    let mut out = String::new();
    let (mut source, mut line, mut column, mut name) = (0i64, 0i64, 0i64, 0i64);
    for (index, segments) in lines.iter().enumerate() {
        if index > 0 {
            out.push(';');
        }
        let mut generated = 0i64;
        for (index, segment) in segments.iter().enumerate() {
            if index > 0 {
                out.push(',');
            }
            encode_vlq(i64::from(segment.column) - generated, &mut out);
            generated = i64::from(segment.column);
            if let Some((s, l, c, n)) = segment.original {
                encode_vlq(i64::from(s) - source, &mut out);
                encode_vlq(i64::from(l) - line, &mut out);
                encode_vlq(i64::from(c) - column, &mut out);
                (source, line, column) = (i64::from(s), i64::from(l), i64::from(c));
                if let Some(n) = n {
                    encode_vlq(i64::from(n) - name, &mut out);
                    name = i64::from(n);
                }
            }
        }
    }
    out
}

pub(super) const SOURCES: u32 = 3;
pub(super) const NAMES: u32 = 4;

pub(super) fn written_strategy() -> impl Strategy<Value = Vec<Vec<Written>>> {
    let original = prop::option::weighted(
        0.8,
        (0..SOURCES, 0..20u32, 0..30u32, prop::option::of(0..NAMES)),
    );
    let segment = (0..40u32, original).prop_map(|(column, original)| Written { column, original });
    prop::collection::vec(prop::collection::vec(segment, 0..8), 0..8)
}

pub(super) fn written_json(lines: &[Vec<Written>]) -> String {
    simple(
        r#"["s0.ts","s1.ts","s2.ts"]"#,
        r#"["n0","n1","n2","n3"]"#,
        &encode_mappings(lines),
    )
}

/// The segments in sorted order: by position, and at one position in the
/// order they were written.
fn expected_segments(lines: &[Vec<Written>]) -> Vec<(Position, Option<OriginalPlace>)> {
    let mut expected: Vec<(Position, Option<OriginalPlace>)> = lines
        .iter()
        .enumerate()
        .flat_map(|(line, segments)| {
            segments.iter().map(move |segment| {
                let line = u32::try_from(line).expect("a small line");
                (
                    at(line, segment.column),
                    segment
                        .original
                        .map(|(source, line, column, name)| place(source, line, column, name)),
                )
            })
        })
        .collect();
    expected.sort_by_key(|(position, _)| *position);
    expected
}

proptest! {
    #[test]
    fn vlq_round_trips(value in i64::from(i32::MIN)..=i64::from(i32::MAX)) {
        let mut text = String::new();
        encode_vlq(value, &mut text);
        let mut at = 0;
        prop_assert_eq!(decode_vlq(text.as_bytes(), &mut at), Ok(value));
        prop_assert_eq!(at, text.len());
    }

    #[test]
    fn encoded_segments_decode_back(lines in written_strategy()) {
        let map = parse(&written_json(&lines));
        let expected = expected_segments(&lines);
        let decoded: Vec<_> = map.generated.iter().copied().zip(map.originals.iter().copied()).collect();
        prop_assert_eq!(decoded, expected);
    }

    /// A lookup agrees with a scan of every segment: the last one listed
    /// of those that start at the greatest column at or before the
    /// position on its line.
    #[test]
    fn lookups_agree_with_a_linear_scan(
        lines in written_strategy(),
        queries in prop::collection::vec((0..9u32, 0..45u32), 1..40),
    ) {
        let map = parse(&written_json(&lines));
        let segments = expected_segments(&lines);
        for (line, column) in queries {
            let mut oracle = None;
            for (position, original) in &segments {
                if position.line.zero_based() == line && position.column.zero_based() <= column {
                    let start = position.column;
                    if oracle.is_none_or(|(best, _)| start >= best) {
                        oracle = Some((start, *original));
                    }
                }
            }
            let expected = oracle.map(|(start, original)| Covering { start, original });
            prop_assert_eq!(map.covering(at(line, column)), expected, "{}:{}", line, column);
        }
    }

    /// Every generated position an original line leads to leads back to
    /// it, and every position that leads to it is among them.
    #[test]
    fn original_lines_and_lookups_agree(lines in written_strategy()) {
        let map = parse(&written_json(&lines));
        for source in 0..SOURCES {
            let source = SourceId::new(source);
            for line in 0..20 {
                let line = Line::from_zero_based(line);
                let found: Vec<Position> =
                    map.generated_for_line(source, line).map(|found| found.generated).collect();
                for position in &found {
                    let back = map.original_at(*position).expect("a mapped position");
                    prop_assert_eq!((back.source, back.line), (source, line));
                }

                let mut starts: Vec<Position> = map
                    .generated
                    .iter()
                    .copied()
                    .filter(|start| {
                        map.original_at(*start)
                            .is_some_and(|back| (back.source, back.line) == (source, line))
                    })
                    .collect();
                starts.dedup();
                let mut sorted = found.clone();
                sorted.sort_unstable();
                prop_assert_eq!(sorted, starts);

                let chosen = map.breakpoint_positions(source, line, ColumnChoice::LineStart);
                prop_assert!(chosen.iter().all(|position| found.contains(position)));
                prop_assert_eq!(chosen.is_empty(), found.is_empty());
            }
        }
    }
}
