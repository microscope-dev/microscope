//! Type identities: what a type is, whatever a producer named it.

use super::*;
use microscope::{
    ArgumentOrigin, GoKind, IntegerValue, SliceWords, SourceLanguage, TypeArgument, TypeIdentity,
    TypeInfo, TypeKind, TypeReference,
};

fn info(image: &ModuleImage, reference: TypeReference) -> &TypeInfo {
    image
        .type_info(reference)
        .unwrap_or_else(|| panic!("{reference:?} is not a resolved type"))
}

fn identity(info: &TypeInfo) -> &TypeIdentity {
    info.identity
        .as_deref()
        .unwrap_or_else(|| panic!("{} has no identity: {info:?}", info.name))
}

/// The one type, however many units define it, with this identity.
fn only<'a>(
    image: &'a ModuleImage,
    language: SourceLanguage,
    path: &[&str],
    base: &str,
) -> &'a TypeInfo {
    let found = image.type_instances(language, path, base);
    let first = *found
        .first()
        .unwrap_or_else(|| panic!("no {path:?}::{base} in {}", image.path().display()));

    for other in &found {
        assert!(
            image.same_type(first, *other),
            "{path:?}::{base} has several instances: {:#?}",
            found
                .iter()
                .map(|found| info(image, *found))
                .collect::<Vec<_>>()
        );
    }

    info(image, first)
}

/// The one type, however many units define it, that a name means.
fn spelled<'a>(image: &'a ModuleImage, name: &str) -> &'a TypeInfo {
    let found = image.types_named(name);
    let first = *found
        .first()
        .unwrap_or_else(|| panic!("no type is named {name} in {}", image.path().display()));

    for other in &found {
        assert!(
            image.same_type(first, *other),
            "{name} means several types: {:#?}",
            found
                .iter()
                .map(|found| info(image, *found))
                .collect::<Vec<_>>()
        );
    }

    info(image, first)
}

fn argument_type<'a>(image: &'a ModuleImage, argument: &TypeArgument) -> &'a TypeInfo {
    match argument {
        TypeArgument::Type(reference) => info(image, *reference),
        other => panic!("argument {other:?} is not a type"),
    }
}

/// The type's arguments, as their names or values.
fn argument_names(image: &ModuleImage, identity: &TypeIdentity) -> Vec<String> {
    identity
        .arguments
        .iter()
        .map(|argument| match argument {
            TypeArgument::Type(reference) => info(image, *reference).name.to_string(),
            TypeArgument::Value(IntegerValue::Signed(value)) => value.to_string(),
            TypeArgument::Value(IntegerValue::Unsigned(value)) => value.to_string(),
            TypeArgument::Unknown(text) => format!("?{text}"),
            other => panic!("unexpected argument {other:?}"),
        })
        .collect()
}

fn named<'a>(image: &'a ModuleImage, name: &str) -> &'a TypeInfo {
    image
        .types()
        .find_map(|node| match node {
            microscope::TypeNode::Resolved(info) if info.name.as_ref() == name => Some(info),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no type is named {name}"))
}

/// Both C++ standard libraries keep their containers in inline namespaces
/// and name them differently, and GCC omits some templates' parameters. An
/// identity is the same whichever compiler and library built the program.
#[tokio::test]
async fn cpp_template_identities_do_not_depend_on_the_compiler_or_library() {
    use SourceLanguage::Cpp;
    for fixture in [
        "templates-cpp-gcc-o0",
        "templates-cpp-gcc-dwarf4",
        "templates-cpp-clang-o0",
        "templates-cpp-libcxx-o0",
    ] {
        let image = load_fixture_image(fixture).await;

        let vector = only(&image, Cpp, &["std"], "vector");

        // A type a class declares in its scope adds nothing to its layout.
        assert!(
            matches!(vector.kind, TypeKind::Record { .. }),
            "{fixture}: {vector:?}"
        );

        let vector = identity(vector);
        assert_eq!(vector.language, Cpp, "{fixture}");
        assert_eq!(vector.origin, ArgumentOrigin::Dwarf, "{fixture}");
        assert_eq!(vector.arguments.len(), 2, "{fixture}: {vector:?}");
        assert_eq!(
            argument_type(&image, &vector.arguments[0]).name.as_ref(),
            "int"
        );

        // GCC describes no parameters for allocator<int>, so its name gives
        // them, resolved to the type they spell.
        let allocator = identity(argument_type(&image, &vector.arguments[1]));
        assert_eq!(
            (allocator.path.as_ref(), allocator.base.as_ref()),
            (&[Arc::from("std")][..], "allocator"),
            "{fixture}"
        );

        assert_eq!(argument_names(&image, allocator), ["int"], "{fixture}");

        // Type units copy namespaces without their inline marks, which
        // the units that define them carry.
        only(&image, Cpp, &["std"], "basic_string");

        let map = identity(only(&image, Cpp, &["std"], "map"));
        assert_eq!(map.arguments.len(), 4, "{fixture}: {map:?}");
        assert_eq!(
            argument_type(&image, &map.arguments[0]).name.as_ref(),
            "int"
        );

        // A parameter pack is flattened, and a value parameter is a value.
        let pack = identity(only(&image, Cpp, &[], "Pack"));
        assert_eq!(
            argument_names(&image, pack),
            ["int", "char", "double"],
            "{fixture}"
        );

        let fixed = identity(only(&image, Cpp, &[], "Fixed"));
        assert!(
            matches!(
                fixed.arguments.as_ref(),
                [
                    TypeArgument::Value(IntegerValue::Signed(3) | IntegerValue::Unsigned(3)),
                    TypeArgument::Type(_)
                ]
            ),
            "{fixture}: {fixed:?}"
        );

        let array = identity(only(&image, Cpp, &["std"], "array"));
        assert!(
            matches!(
                array.arguments.as_ref(),
                [
                    TypeArgument::Type(_),
                    TypeArgument::Value(IntegerValue::Unsigned(4) | IntegerValue::Signed(4))
                ]
            ),
            "{fixture}: {array:?}"
        );

        // A user's inline namespace collapses too; a local class is scoped
        // by its function.
        let thing = identity(only(&image, Cpp, &["outer"], "Thing"));
        assert_eq!(thing.origin, ArgumentOrigin::None, "{fixture}");
        only(&image, Cpp, &["main"], "Local");

        // A self-referential template's member points at the same identity.
        let node = only(&image, Cpp, &[], "Node");
        assert!(
            matches!(argument_names(&image, identity(node)).as_slice(), [long] if long == "long" || long == "long int"),
            "{fixture}"
        );

        let TypeKind::Record { members, .. } = &node.kind else {
            panic!("{fixture}: {node:?}");
        };

        let next = members
            .iter()
            .find(|member| member.name.as_deref() == Some("next"))
            .expect("Node::next");

        let TypeKind::Pointer {
            target: Some(target),
            ..
        } = info(&image, next.type_ref).kind
        else {
            panic!("{fixture}: Node::next is not a pointer");
        };

        assert!(image.same_type(target, node.reference), "{fixture}");
    }
}

/// rustc names a slice `&[T]` unoptimized and `*const [T]` optimized, and
/// spells mutable slices and boxed slices differently again. Their shape
/// says what they are. A pointer to a type with an unsized tail has the
/// same shape but carries the tail's length, so it is not a slice.
#[tokio::test]
async fn rust_identities_and_fat_pointers_follow_structure_not_spelling() {
    use SourceLanguage::Rust;
    for fixture in ["generics-rust-o0", "generics-rust-o2"] {
        let image = load_fixture_image(fixture).await;

        let vector = identity(spelled(&image, "alloc::vec::Vec<i32>"));
        assert_eq!(vector.language, Rust);
        assert_eq!(vector.origin, ArgumentOrigin::Dwarf, "{fixture}");
        assert_eq!(
            argument_names(&image, vector),
            ["i32", "Global"],
            "{fixture}"
        );

        // rustc describes type parameters but not const ones, which the
        // name supplies.
        let wrapper = identity(only(&image, Rust, &["generics"], "Wrapper"));
        assert_eq!(argument_names(&image, wrapper), ["u8", "3"], "{fixture}");

        for (name, element) in [
            ("*const [i32]", "i32"),
            ("&mut [u8]", "u8"),
            ("alloc::boxed::Box<[u16], alloc::alloc::Global>", "u16"),
        ] {
            let slice = named(&image, name);
            let TypeKind::Slice {
                element: found,
                words: SliceWords::POINTER_LENGTH,
                text: false,
            } = slice.kind
            else {
                panic!("{fixture}: {name} is not a slice: {slice:?}");
            };

            assert_eq!(info(&image, found).name.as_ref(), element, "{fixture}");
        }

        for name in ["&str", "alloc::boxed::Box<str, alloc::alloc::Global>"] {
            assert!(
                matches!(named(&image, name).kind, TypeKind::Slice { text: true, .. }),
                "{fixture}: {name}"
            );
        }

        let tail = named(&image, "&generics::Tail");
        assert!(
            matches!(tail.kind, TypeKind::Record { .. }),
            "{fixture}: a pointer to an unsized type is not a slice: {tail:?}"
        );

        let boxed = identity(named(
            &image,
            "alloc::boxed::Box<[u16], alloc::alloc::Global>",
        ));

        assert_eq!(
            (boxed.path.len(), boxed.base.as_ref()),
            (2, "Box"),
            "{fixture}: {boxed:?}"
        );
    }
}

/// Go identifies maps, channels, and slices by kind, whatever they are
/// named, and names generic instances without describing their parameters.
#[tokio::test]
async fn go_identities_come_from_kinds_and_instance_names() {
    use SourceLanguage::Go;
    for fixture in ["generics-go-o0", "generics-go-o2"] {
        let image = load_fixture_image(fixture).await;
        for (path, base) in [(&[][..], "map[string]int"), (&["main"][..], "Counts")] {
            let map = identity(only(&image, Go, path, base));
            let go = map.go.expect("Go attributes");
            assert_eq!(go.kind, GoKind::Map, "{fixture}: {map:?}");
            assert!(go.runtime_type.is_some(), "{fixture}: {map:?}");
            assert_eq!(map.origin, ArgumentOrigin::Dwarf, "{fixture}");
            assert_eq!(argument_names(&image, map), ["string", "int"], "{fixture}");
        }

        let channel = identity(only(&image, Go, &[], "chan int"));
        assert_eq!(channel.go.map(|go| go.kind), Some(GoKind::Chan));
        assert_eq!(argument_names(&image, channel), ["int"], "{fixture}");

        let pair = identity(only(&image, Go, &["main"], "Pair"));
        assert_eq!(pair.origin, ArgumentOrigin::ParsedName, "{fixture}");
        assert_eq!(argument_names(&image, pair), ["string", "int"], "{fixture}");

        let string = identity(only(&image, Go, &[], "string"));
        assert_eq!(string.go.map(|go| go.kind), Some(GoKind::String));
        assert!(matches!(
            named(&image, "[]int").kind,
            TypeKind::Slice {
                words: SliceWords::POINTER_LENGTH_CAPACITY,
                text: false,
                ..
            }
        ));
    }
}

/// Zig names generic instances by their calls and spells slices and
/// sentinels in its own syntax. Its text slices and sentinel pointers read
/// as text, charged to the inspection's budget: text the budget cannot
/// afford is cut short and says why, without failing the inspection.
#[tokio::test]
async fn zig_identities_text_and_text_budgets_follow_zig_syntax() {
    use SourceLanguage::Zig;
    let scenario =
        stop_at_marker("generics-zig-o0", "zig/generics.zig", "generics stop here").await;
    let image = scenario.handle().module_image();
    let list = identity(spelled(&image, "array_list.Aligned(u32,null)"));
    assert_eq!(list.language, Zig);
    assert_eq!(list.origin, ArgumentOrigin::ParsedName);
    assert_eq!(argument_names(&image, list), ["u32", "?null"]);
    let pair = identity(only(&image, Zig, &["generics"], "Pair"));
    assert_eq!(argument_names(&image, pair), ["u32", "[]const u8"]);
    for (name, text) in [
        ("[]const u8", true),
        ("[:0]const u8", true),
        ("[]const i32", false),
    ] {
        let slice = named(&image, name);
        assert!(
            matches!(slice.kind, TypeKind::Slice { text: found, .. } if found == text),
            "{name}: {slice:?}"
        );
    }

    let variables = scenario
        .operation("variables", scenario.handle().variables())
        .await;

    for (name, text) in [
        ("text", Some(complete("hello"))),
        ("terminated", Some(complete("zero"))),
        ("c_text", Some(complete("cstr"))),
        ("ints", None),
    ] {
        assert_eq!(text_of(&variables.variables, name), text, "{name}");
    }

    let full = inspect_text(&scenario, microscope::InspectionLimits::default()).await;
    let VariableState::Available { text, .. } = &full.state else {
        panic!("{full:?}");
    };

    assert_eq!(text.as_deref().cloned(), Some(complete("hello")));

    // Three bytes short of the whole text.
    let limits = microscope::InspectionLimits {
        memory_bytes: full.usage.memory_bytes - 3,
        ..microscope::InspectionLimits::default()
    };

    let tight = inspect_text(&scenario, limits).await;
    assert_eq!(
        tight.completion,
        microscope::InspectionCompletion::Complete,
        "{tight:?}"
    );

    let VariableState::Available {
        text: Some(text), ..
    } = &tight.state
    else {
        panic!("{tight:?}");
    };

    assert_eq!(&*text.bytes, b"he", "{text:?}");
    assert!(
        matches!(
            text.completion,
            microscope::TextCompletion::Limited {
                length: Some(5),
                exhaustion: microscope::InspectionExhaustion {
                    resource: microscope::InspectionLimit::MemoryBytes,
                    ..
                },
            }
        ),
        "{text:?}"
    );

    scenario.shutdown().await;
}

/// Stops a fixture at the line holding `marker`, in its source under
/// `tests/fixtures`.
async fn stop_at_marker(fixture: &str, source: &str, marker: &str) -> Scenario {
    let line = source_line(&format!("tests/fixtures/{source}"), marker);
    let file = source.rsplit('/').next().expect("a file name");
    let mut scenario = Scenario::launch(fixture);
    scenario.add_source_breakpoint(file, line).await;
    assert!(matches!(
        scenario.run_to_stop().await,
        StopReason::Breakpoint { .. }
    ));

    scenario
}

async fn size_of(scenario: &Scenario, text: &str) -> i128 {
    let evaluation = scenario
        .handle()
        .evaluate(&parsed_value_expression(text))
        .await
        .unwrap_or_else(|error| panic!("{text}: {error}"));

    let microscope::Evaluation::Value { value, .. } = evaluation else {
        panic!("{text} is not a value: {evaluation:?}");
    };

    match available_value(&value.state) {
        microscope::VariableValue::Scalar(ScalarValue::Signed(size)) => *size,
        microscope::VariableValue::Scalar(ScalarValue::Unsigned(size)) => {
            i128::try_from(*size).expect("size fits")
        }
        other => panic!("{text} is not an integer: {other:?}"),
    }
}

/// An expression names a type as a person writes it: qualified or not,
/// with the arguments a template's defaults leave out omitted, whatever
/// the producer named it.
#[tokio::test]
async fn expressions_name_types_by_their_identities() {
    for (fixture, source, sizes) in [
        (
            "templates-cpp-gcc-o0",
            "cpp/templates.cpp",
            &[
                ("sizeof(std::`vector<int>`)", 24),
                ("sizeof(outer::Thing)", 4),
                ("sizeof(`Fixed<3, short>`)", 6),
                // A namespace no unit marks inline is not inline, even when
                // a library names its inline namespaces alike elsewhere.
                ("sizeof(std::__debug::`vector<int>`)", 56),
            ][..],
        ),
        (
            "templates-cpp-libcxx-o0",
            "cpp/templates.cpp",
            &[
                ("sizeof(std::`vector<int>`)", 24),
                ("sizeof(`vector<int>`)", 24),
                ("sizeof(outer::v1::Thing)", 4),
            ][..],
        ),
        (
            "generics-rust-o0",
            "rust/generics.rs",
            &[
                ("sizeof(alloc::vec::`Vec<i32>`)", 24),
                ("sizeof(`Wrapper<u8, 3>`)", 3),
            ][..],
        ),
        (
            "generics-go-o0",
            "go/generics/main.go",
            &[
                ("sizeof(`main.Point`)", 16),
                ("sizeof(`Pair[string,int]`)", 24),
            ][..],
        ),
    ] {
        let marker = if source.starts_with("cpp") {
            "templates stop here"
        } else {
            "generics stop here"
        };

        let scenario = stop_at_marker(fixture, source, marker).await;
        for (text, size) in sizes {
            assert_eq!(size_of(&scenario, text).await, *size, "{fixture}: {text}");
        }

        scenario.shutdown().await;
    }
}

fn complete(text: &str) -> microscope::TextSummary {
    microscope::TextSummary {
        bytes: text.as_bytes().into(),
        completion: microscope::TextCompletion::Complete,
    }
}

/// Rust's text slices read as text, and byte slices do not.
#[tokio::test]
async fn rust_text_slices_read_as_text() {
    let scenario =
        stop_at_marker("generics-rust-o0", "rust/generics.rs", "generics stop here").await;

    let variables = scenario
        .operation("variables", scenario.handle().variables())
        .await;

    for (name, text) in [
        ("text", Some(complete("héllo"))),
        ("boxed_text", Some(complete("boxed"))),
        ("owned", Some(complete("owned"))),
        ("bytes", None),
        ("slice", None),
    ] {
        assert_eq!(text_of(&variables.variables, name), text, "{name}");
    }

    scenario.shutdown().await;
}

async fn inspect_text(
    scenario: &Scenario,
    limits: microscope::InspectionLimits,
) -> microscope::InspectedValue {
    let evaluation = scenario
        .handle()
        .evaluate_with(
            &parsed_value_expression("text"),
            microscope::EvaluationMode::Read,
            limits,
        )
        .await
        .expect("evaluate text");

    let microscope::Evaluation::Value { value, .. } = evaluation else {
        panic!("text is not a value: {evaluation:?}");
    };

    value
}
