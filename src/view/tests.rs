use std::sync::Arc;

use super::bind::BoundView;
use super::run::{Child, Failure, Presented, children, present};
use super::scan::Checkpoints;
use super::syntax::{self, Language};
use super::{ViewSet, choose};
use crate::eval::fake::{Place, Step, World};
use crate::{
    BaseTypeEncoding as E, IntegerValue, SourceLanguage, TextCompletion, TypeArgument,
    TypeReference, VariableValue,
};

fn bytes(values: &[u64]) -> Vec<u8> {
    values
        .iter()
        .flat_map(|value| value.to_le_bytes())
        .collect()
}

fn ints(values: impl IntoIterator<Item = i32>) -> Vec<u8> {
    values.into_iter().flat_map(i32::to_le_bytes).collect()
}

/// The world `docs/views.md` describes, which most tests here share.
fn world() -> World {
    let mut world = World::new();
    add_containers(&mut world);
    add_others(&mut world);
    add_linked(&mut world);
    add_python(&mut world);
    world
}

/// Python objects, whose types only their values know: `point`, an
/// `app.Point` whose attributes `x` and `y` hold 3 and 4; `red`, a member of
/// the enum `app.Color`, named `RED` and holding 1; `gone`, an `app.Point`
/// with no attributes; and `number`, the int 7.
fn add_python(world: &mut World) {
    use crate::eval::fake::Dynamic;

    let x = world.dynamic(Dynamic::Int(3));
    let y = world.dynamic(Dynamic::Int(4));
    let name = world.dynamic(Dynamic::Str("RED"));
    let value = world.dynamic(Dynamic::Int(1));
    let instance = |type_name, bases, attributes| Dynamic::Instance {
        type_name,
        bases,
        attributes,
    };

    for (variable, value) in [
        (
            "point",
            instance("app.Point", Vec::new(), vec![("x", x), ("y", y)]),
        ),
        (
            "red",
            instance(
                "app.Color",
                vec!["enum.Enum"],
                vec![("_name_", name), ("_value_", value)],
            ),
        ),
        ("gone", instance("app.Point", Vec::new(), Vec::new())),
        ("number", Dynamic::Int(7)),
    ] {
        let index = world.dynamic(value);
        world.dynamic_variable(variable, index);
    }
}

/// Hand-rolled C vectors and text.
fn add_containers(world: &mut World) {
    let int = world.base("int", E::Signed, 4);
    let size = world.base("unsigned long", E::Unsigned, 8);
    let char = world.base("char", E::SignedCharacter, 1);
    let int_pointer = world.pointer(Some(int));
    let char_pointer = world.pointer(Some(char));
    let intvec = world.record(
        "intvec",
        24,
        &[("data", int_pointer, 0), ("n", size, 8), ("cap", size, 16)],
    );
    world.identify(intvec, SourceLanguage::C, &[], "intvec", Vec::new());
    let text = world.record("str_t", 16, &[("p", char_pointer, 0), ("len", size, 8)]);
    world.identify(text, SourceLanguage::C, &[], "str_t", Vec::new());

    let elements = world.allocate(&ints([10, 20, 30, 0]));
    world.variable("v", intvec, &bytes(&[elements, 3, 4]));
    world.variable("bad", intvec, &bytes(&[elements, 9, 4]));
    world.variable("none", intvec, &bytes(&[0, 0, 0]));
    let many = world.allocate(&ints(0..300));
    world.variable("big", intvec, &bytes(&[many, 300, 300]));
    world.variable("dangling", intvec, &bytes(&[0xdead_0000, 2, 2]));

    // Text without a length is read a page at a time.
    let mut page = b"hello\0".to_vec();
    page.resize(4096, b'?');
    world.map(0x9_0000, &page);
    world.variable("s", text, &bytes(&[0x9_0000, 5]));

    // Text that runs to the end of the last mapped page.
    world.map(0x7_fffd, b"abc");
    world.variable("cut", text, &bytes(&[0x7_fffd, 300]));
}

const VIEWS: &str = "microscope-views 1
view c intvec {
    check n <= cap
    show sequence(n) for i in range(n) => data[i]
    field capacity = cap
}
view c str_t {
    show text(p, len)
}
";

/// A built-in view that does not parse is left out, and so escapes the
/// fixtures' check that every built-in view binds.
#[test]
fn every_built_in_view_parses() {
    let set = ViewSet::built_in();
    assert!(set.errors().is_empty(), "{:?}", set.errors());
}

fn set(text: &str) -> ViewSet {
    let set = ViewSet::new([("test.views", text)]);
    assert!(set.errors().is_empty(), "{:?}", set.errors());
    set
}

fn place(world: &World, name: &str) -> Place {
    Place::Memory {
        address: world.address_of(name),
        ty: world.type_of(name),
    }
}

/// The view `views` chooses for the variable `name`.
fn bound(world: &World, views: &ViewSet, name: &str) -> Arc<BoundView<Step>> {
    choose(views, world.type_of(name), world)
        .bound
        .expect("a view binds")
}

fn failed(failure: Failure) -> String {
    match failure {
        Failure::Problem(problem) => format!("problem: {problem}"),
        Failure::Debugger(error) => format!("debugger: {error}"),
    }
}

fn presented(world: &mut World, views: &ViewSet, name: &str) -> Result<Presented, String> {
    let choice = choose(views, world.type_of(name), world);
    let Some(bound) = choice.bound else {
        return Err(format!("problem: no view binds: {:?}", choice.candidates));
    };

    let this = place(world, name);
    present(&bound, world, this, &mut Checkpoints::default()).map_err(failed)
}

fn summary(world: &mut World, views: &ViewSet, name: &str) -> String {
    presented(world, views, name).map_or_else(|problem| problem, |presented| presented.summary)
}

/// A child as the reference's examples write it.
fn rendered(child: &Child) -> String {
    let value = |value: &crate::InspectedValue| {
        super::summary::value(value.type_info.as_ref(), &value.state)
    };

    match child {
        Child::Element(index, element) => format!("[{index}] = {}", value(element)),
        Child::Entry(_, key, entry) => format!("{}: {}", value(key), value(entry)),
        Child::Field(name, field) => format!("{name} = {}", value(field)),
        Child::Raw => "[raw]".to_owned(),
    }
}

/// Child `k` costs one evaluation of the element, wherever it is.
#[test]
fn random_access_children_cost_the_same_at_any_index() {
    let mut world = world();
    let bound = bound(&world, &set(VIEWS), "big");
    let mut costs = Vec::new();

    for index in [0, 150, 299] {
        world.reads.clear();
        let this = place(&world, "big");
        let page = children(
            &bound,
            &mut world,
            this,
            300,
            index,
            1,
            &mut Checkpoints::default(),
        )
        .expect("children");
        assert!(matches!(&page[..], [Child::Element(found, _)] if *found == index));
        costs.push(world.reads.len());
    }

    assert!(costs.windows(2).all(|pair| pair[0] == pair[1]), "{costs:?}");

    // The check's two reads, the count's and the range's, and the element's
    // pointer and value.
    assert_eq!(costs[0], 6, "{costs:?}");
}

/// A count as large as a word holds, as garbage often is, pages without
/// overflowing: its last children are still its fields and `[raw]`.
#[test]
fn the_largest_counts_page_without_overflowing() {
    let mut world = world();
    let intvec = world.type_of("v");
    world.variable("huge", intvec, &bytes(&[0x10, u64::MAX, u64::MAX]));
    let bound = bound(&world, &set(VIEWS), "huge");
    let this = place(&world, "huge");
    let page = children(
        &bound,
        &mut world,
        this,
        u64::MAX,
        u64::MAX - 1,
        4,
        &mut Checkpoints::default(),
    )
    .expect("children");
    assert!(
        matches!(&page[..], [Child::Element(index, _)] if *index == u64::MAX - 1),
        "{page:?}"
    );
}

#[test]
fn text_reads_its_length_or_to_its_terminator_and_says_how_it_ended() {
    let mut world = world();
    let cut = presented(&mut world, &set(VIEWS), "cut").expect("presents");
    let read = cut.text.expect("text");
    assert_eq!(&*read.bytes, b"abc");
    assert!(
        matches!(read.completion, TextCompletion::Unreadable { address } if address.get() == 0x8_0000),
        "{read:?}"
    );
    assert_eq!(cut.summary, "\"abc\"... <unreadable at 0x80000>");

    let terminated = set("microscope-views 1\nview c str_t { show text(p) }\n");
    assert_eq!(summary(&mut world, &terminated, "s"), "\"hello\"");
}

#[test]
fn an_alternative_that_does_not_bind_falls_through_and_says_why() {
    let views = set("microscope-views 1
view c intvec {
    let size = length
    show sequence(size) for i in range(size) => data[i]
}
view c intvec {
    let size = length or n
    show sequence(size) for i in range(size) => data[i]
}
");
    let world = world();
    let choice = choose(&views, world.type_of("v"), &world);
    assert!(choice.bound.is_some());
    let [rejected, chosen] = choice.candidates.as_slice() else {
        panic!("two candidates: {:?}", choice.candidates);
    };

    assert_eq!(rejected.name.line, 2);
    let reason = rejected.rejection.as_ref().expect("rejected").to_string();
    assert!(
        reason.contains("`length` is neither a member of `intvec` nor a name the view declares"),
        "{reason}"
    );
    assert_eq!(chosen.name.line, 6);
    assert!(chosen.rejection.is_none());
}

#[test]
fn patterns_capture_arguments_and_anchor_at_the_root() {
    let mut world = World::new();
    let int = world.base("int", E::Signed, 4);
    let array = world.array(int, &[3]);
    let pair = world.record(
        "Pair<int, 3>",
        16,
        &[("first", int, 0), ("items", array, 4)],
    );
    world.identify(
        pair,
        SourceLanguage::Cpp,
        &["app", "detail"],
        "Pair",
        vec![
            TypeArgument::Type(int),
            TypeArgument::Value(IntegerValue::Unsigned(3)),
        ],
    );
    world.variable("p", pair, &ints([1, 2, 3, 4]));
    let shown = |world: &mut World, header: &str| {
        // A pattern that names the count gives the body none to use.
        let count = if header.contains("N>") { "N" } else { "3" };
        let views = ViewSet::new([(
            "test.views",
            format!(
                "microscope-views 1\nview {header} {{\n    show sequence({count}) for i in range({count}) => *((T*)&items[0] + i)\n}}\n"
            )
            .as_str(),
        )]);
        assert!(views.errors().is_empty(), "{:?}", views.errors());
        summary(world, &views, "p")
    };

    assert_eq!(
        shown(&mut world, "c++ app::detail::Pair<T, N>"),
        "len=3 [2, 3, 4]"
    );
    assert_eq!(shown(&mut world, "any **::Pair<T, 3>"), "len=3 [2, 3, 4]");

    for header in [
        "c app::detail::Pair<T, N>",
        "c++ app::detail::Pair<T, N, M>",
        "c++ app::detail::Pair<long, N>",
    ] {
        assert!(
            shown(&mut world, header).starts_with("problem: no view binds"),
            "{header}"
        );
    }

    // A pattern that reaches a parameter pack spells all of it, so `Pair<T>`
    // does not name a pair whose pack holds two arguments.
    world.pack(pair, 0);
    assert_eq!(
        shown(&mut world, "c++ app::detail::Pair<T, N>"),
        "len=3 [2, 3, 4]"
    );
    let views = ViewSet::new([(
        "test.views",
        "microscope-views 1\nview c++ app::detail::Pair<T> {\n    show empty(\"one\")\n}\nview c++ app::detail::Pair {\n    show empty(\"any\")\n}\n",
    )]);
    assert_eq!(summary(&mut world, &views, "p"), "any");

    // Segments rustc writes in braces, as it names closures, are quoted.
    let closure = world.record("{closure_env#0}", 4, &[("x", int, 0)]);
    world.identify(
        closure,
        SourceLanguage::Rust,
        &["app", "run", "{async_fn#0}"],
        "{closure_env#0}",
        Vec::new(),
    );
    world.variable("f", closure, &ints([7]));
    let views = ViewSet::new([(
        "test.views",
        "microscope-views 1\nview rust app::**::`{async_fn#0}`::`{closure_env#0}` {\n    show value(x)\n}\n",
    )]);
    assert!(views.errors().is_empty(), "{:?}", views.errors());
    assert_eq!(summary(&mut world, &views, "f"), "7");
}

#[test]
fn the_built_in_views_parse() {
    let views = ViewSet::built_in();
    assert!(views.errors().is_empty(), "{:?}", views.errors());
    assert!(views.views().len() >= 16, "{}", views.views().len());
}

fn errors(text: &str) -> Vec<String> {
    syntax::parse("bad.views", text)
        .errors
        .iter()
        .map(ToString::to_string)
        .collect()
}

#[test]
fn view_file_errors_point_at_their_line_and_column_and_skip_one_view() {
    assert_eq!(
        errors("view c x { show empty(\"\") }"),
        ["bad.views:1:1: a view file begins with `microscope-views 1`"]
    );
    assert_eq!(
        errors("microscope-views 2\n"),
        ["bad.views:1:1: this microscope reads views of version 1"]
    );

    let file = syntax::parse(
        "bad.views",
        "microscope-views 1
view c first {
    let x = n +
    show empty(\"\")
}
view c second {
    show empty(\"ok\")
}
extend c third {
    show empty(\"\")
}
view c fourth {
    show text(p)
    show text(p)
}
view c fifth {
    format x as octal
}
view c sixth {
    show sequence(n) for i in list(head) => i
}
view kotlin seventh {
    show empty(\"\")
}
",
    );
    let messages = file
        .errors
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>();

    assert_eq!(file.views.len(), 1, "{messages:?}");
    assert_eq!(file.views[0].pattern.base, "second");
    assert_eq!(file.views[0].language, Language::C);
    assert_eq!(messages.len(), 6, "{messages:?}");
    assert!(
        messages[0].starts_with("bad.views:3:16: "),
        "{}",
        messages[0]
    );
    assert!(
        messages[1].starts_with(
            "bad.views:10:5: an `extend` adds to the view that shows the value; it does not `show`"
        ),
        "{}",
        messages[1]
    );
    assert!(
        messages[2].starts_with("bad.views:14:5: a view shows its value once"),
        "{}",
        messages[2]
    );
    assert!(
        messages[3].contains("`octal` is no format"),
        "{}",
        messages[3]
    );
    assert!(
        messages[4].contains("expected `,` before the next argument"),
        "{}",
        messages[4]
    );
    assert!(
        messages[5].contains("expected a language"),
        "{}",
        messages[5]
    );
}

/// Recovering from an error never splits a character, wherever it is.
#[test]
fn recovery_from_an_error_steps_over_whole_characters() {
    let file = syntax::parse(
        "bad.views",
        "microscope-views 1\n\u{2603}é view c x {\nview c second {\n    show empty(\"ok\")\n}\n",
    );
    assert_eq!(file.views.len(), 1, "{:?}", file.errors);
    assert_eq!(file.views[0].pattern.base, "second");
}

#[test]
fn expressions_end_where_their_statement_or_shape_does() {
    let file = syntax::parse(
        "ok.views",
        "microscope-views 1 # the version
view c++ app::Thing<T, _> {   # a comment
    let begin = first
        or `or`.second       # an alternative on the next line
    check begin <= end &&
        end <= limit
    show if len(x) == 0 { empty(\"empty\") }
         else { sequence(end - begin) for i in range(end - begin)
                => begin[i] }
    field total = (a
        + b)
    summary \"{size} \\{braces\\}\"
}
",
    );
    assert!(file.errors.is_empty(), "{:?}", file.errors);
    let statements = &file.views[0].statements;
    let syntax::Statement::Let { alternatives, .. } = &statements[0] else {
        panic!("a let");
    };

    let texts = alternatives
        .iter()
        .map(syntax::Expr::text)
        .collect::<Vec<_>>();
    assert_eq!(texts, ["first", "`or`.second"]);
    let syntax::Statement::Check(check) = &statements[1] else {
        panic!("a check");
    };

    assert_eq!(check.expression.to_string(), "begin <= end && end <= limit");
    let syntax::Statement::Show(syntax::Shape::If { otherwise, .. }) = &statements[2] else {
        panic!("an if");
    };

    let syntax::Shape::Sequence {
        element, clauses, ..
    } = otherwise.as_ref()
    else {
        panic!("a sequence");
    };

    let [
        syntax::Clause {
            generator: syntax::Generator::Range(length),
            ..
        },
    ] = clauses.as_slice()
    else {
        panic!("one range: {clauses:?}");
    };

    assert_eq!((length.text(), element.text()), ("end - begin", "begin[i]"));
    let syntax::Statement::Summary(pieces) = &statements[4] else {
        panic!("a summary");
    };

    assert!(
        matches!(&pieces[..], [syntax::Piece::Hole { .. }, syntax::Piece::Literal(text)] if text == " {braces}")
    );
}

#[test]
fn bound_views_reject_what_does_not_type_check() {
    let world = world();
    for (body, reason) in [
        (
            "show sequence(n) for i in range(n) => data[i].x",
            "`data[i]` has no members",
        ),
        ("show text(n)", "`text` takes a pointer to characters"),
        (
            "show sequence(data) for i in range(n) => i",
            "is not an integer",
        ),
        ("check n\n    show empty(\"\")", ""),
        (
            "type T = int*\n    show empty(\"\")",
            "write the pointer where it is used",
        ),
        ("type T = typeof(n)\n    show empty(\"\")", ""),
        ("show value(self.missing)", "has no member"),
    ] {
        let views = set(&format!(
            "microscope-views 1\nview c intvec {{\n    {body}\n}}\n"
        ));
        let choice = choose(&views, world.type_of("v"), &world);

        match (&choice.bound, reason) {
            (Some(_), "") => {}
            (Some(_), _) => panic!("`{body}` bound"),
            (None, _) => {
                let rejection = choice.candidates[0].rejection.as_ref().expect("rejected");
                assert!(
                    rejection.to_string().contains(reason),
                    "`{body}`: {rejection}"
                );
            }
        }
    }
}

/// Views are pure, as the evaluator is: they reach a program only through
/// the evaluator's traits, so none of their code may reach for process
/// control, debug information, I/O, clocks, or threads. Tests are exempt.
#[test]
fn views_stay_pure() {
    const FORBIDDEN: [&str; 11] = [
        "crate::backend",
        "crate::debug_info",
        "crate::sim",
        "nix::",
        "gimli",
        "tokio",
        "std::fs",
        "std::env",
        "std::process",
        "std::thread",
        "std::time",
    ];
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/view");
    let mut checked = 0;

    for entry in std::fs::read_dir(&root).expect("read the views' sources") {
        let path = entry.expect("a directory entry").path();
        if path.file_name().is_some_and(|name| name == "tests.rs")
            || path.extension().is_none_or(|extension| extension != "rs")
        {
            continue;
        }

        let source = std::fs::read_to_string(&path).expect("read a source file");
        let source = source.split("#[cfg(test)]").next().unwrap_or_default();

        for forbidden in FORBIDDEN {
            assert!(
                !source.contains(forbidden),
                "{} uses `{forbidden}`",
                path.display()
            );
        }

        checked += 1;
    }

    assert!(checked >= 6, "the views' sources were found");
}

/// The hostile harness's containers are each presented by the built-in
/// view meant for them, so the fuzz target exercises every library's views.
#[test]
fn the_hostile_harness_exercises_every_library() {
    let choices = super::fuzz::built_in_choices();
    let unbound = choices
        .iter()
        .filter(|(_, view)| view.is_none())
        .collect::<Vec<_>>();
    assert!(unbound.is_empty(), "{unbound:?}");
}

proptest::proptest! {
    #![proptest_config(proptest::prelude::ProptestConfig::with_cases(256))]

    /// Whatever memory holds, and whatever views a file says, presenting
    /// ends in a value or a typed problem within its budget.
    #[test]
    fn views_over_arbitrary_memory_end_in_values_or_problems(
        data in proptest::collection::vec(proptest::prelude::any::<u8>(), 0..6000),
    ) {
        super::fuzz::hostile(&data);
    }
}

/// A tagged union, a Rust box, C++ template instances, and the values
/// `formatted_and_linked` adds.
fn add_others(world: &mut World) {
    let int = world.base("int", E::Signed, 4);
    let tagged = world.record("tagged", 8, &[("kind", int, 0), ("value", int, 4)]);
    world.identify(tagged, SourceLanguage::C, &[], "tagged", Vec::new());
    world.variable("nothing", tagged, &ints([0, 0]));
    world.variable("something", tagged, &ints([1, 7]));
    let marker = world.record("PhantomData", 0, &[]);
    let pointer = world.pointer(Some(int));
    let boxed = world.record(
        "Box<i32>",
        8,
        &[("_marker", marker, 0), ("ptr", pointer, 0)],
    );
    world.identify(
        boxed,
        SourceLanguage::Rust,
        &["alloc", "boxed"],
        "Box",
        vec![TypeArgument::Type(int)],
    );
    let target = world.allocate(&ints([42]));
    world.variable("b", boxed, &bytes(&[target]));
    let array = world.array(int, &[3]);
    let pair = world.record(
        "Pair<int, 3>",
        16,
        &[("first", int, 0), ("items", array, 4)],
    );
    world.identify(
        pair,
        SourceLanguage::Cpp,
        &["app", "detail"],
        "Pair",
        vec![
            TypeArgument::Type(int),
            TypeArgument::Value(IntegerValue::Unsigned(3)),
        ],
    );
    world.variable("p", pair, &ints([1, 2, 3, 4]));

    // `app::Handle<int>` holds a `void *` to an `app::Cell<int>`.
    let cell = world.record("Cell<int>", 4, &[("value", int, 0)]);
    world.identify(
        cell,
        SourceLanguage::Cpp,
        &["app"],
        "Cell",
        vec![TypeArgument::Type(int)],
    );
    let void = world.pointer(None);
    let handle = world.record("Handle<int>", 8, &[("cell", void, 0)]);
    world.identify(
        handle,
        SourceLanguage::Cpp,
        &["app"],
        "Handle",
        vec![TypeArgument::Type(int)],
    );
    let stored = world.allocate(&ints([5]));
    world.variable("handle", handle, &bytes(&[stored]));

    // `app::Either<int, char*>` holds one of its arguments, which `index`
    // chooses, in `storage`.
    let char = world.base("char", E::SignedCharacter, 1);
    let characters = world.pointer(Some(char));
    let either = world.record(
        "Either<int, char*>",
        16,
        &[("storage", characters, 0), ("index", int, 8)],
    );
    world.identify(
        either,
        SourceLanguage::Cpp,
        &["app"],
        "Either",
        vec![TypeArgument::Type(int), TypeArgument::Type(characters)],
    );

    // `app::Erased` holds an object only the function at `manage` knows
    // the type of.
    let erased = world.record("Erased", 16, &[("object", void, 0), ("manage", void, 8)]);
    world.identify(erased, SourceLanguage::Cpp, &["app"], "Erased", Vec::new());
    world.function(0x4_0000, &[("T", int)]);
    let held = world.allocate(&ints([5]));
    world.variable("erased", erased, &bytes(&[held, 0x4_0000]));
    world.variable("stray", erased, &bytes(&[held, 0x5_0000]));
    world.variable("number", either, &bytes(&[7, 0]));
    world.variable("pointer", either, &bytes(&[0x9_0000, 1]));
    world.variable("neither", either, &bytes(&[0, 5]));
    formatted_and_linked(world, int, tagged);
}

/// The examples' `entry`, whose members formats write, a tagged value of a
/// kind nothing names, an intrusive run queue, and an arena of handles.
fn formatted_and_linked(world: &mut World, int: TypeReference, tagged: TypeReference) {
    world.variable("strange", tagged, &ints([5, 0]));

    let short = world.base("unsigned short", E::Unsigned, 2);
    let long = world.base("long", E::Signed, 8);
    world.enumeration(
        "Access",
        int,
        &[("NONE", 0), ("READ", 1), ("WRITE", 2), ("EXEC", 4)],
    );
    world.enumeration("Color", int, &[("RED", 0), ("GREEN", 1), ("BLUE", 2)]);
    let label = world.array(short, &[4]);
    let entry = world.record(
        "entry",
        32,
        &[
            ("mode", int, 0),
            ("color", int, 4),
            ("elapsed", long, 8),
            ("label", label, 16),
            ("letter", int, 24),
        ],
    );
    world.identify(entry, SourceLanguage::C, &[], "entry", Vec::new());
    let mut item = ints([3, 2]);
    item.extend(1500_i64.to_le_bytes());
    item.extend([b'h', 0, b'i', 0, 0, 0, 0, 0]);
    item.extend(ints([65, 0]));
    world.variable("item", entry, &item);

    // Bytes, as text and otherwise: a `message {uint8_t text[8]; uint8_t
    // raw[4]}`, and `octets`, a `uint8_t[4]`. `uint8_t` names `unsigned
    // char`, as C's does.
    let unsigned_char = world.base("unsigned char", E::UnsignedCharacter, 1);
    let byte = world.typedef("uint8_t", unsigned_char);
    let text = world.array(byte, &[8]);
    let raw = world.array(byte, &[4]);
    let message = world.record("message", 12, &[("text", text, 0), ("raw", raw, 8)]);
    world.identify(message, SourceLanguage::C, &[], "message", Vec::new());
    let mut note = b"hi there".to_vec();
    note.extend([0xff, 0, 1, 2]);
    world.variable("note", message, &note);
    let octets = world.typedef("octets", raw);
    world.identify(octets, SourceLanguage::C, &[], "octets", Vec::new());
    world.variable("word", octets, b"ok!?");
    world.variable("blob", octets, &[0xff, 0, 1, 2]);

    // A run queue whose tasks link through the `node` each embeds, in a
    // ring through the queue's own `tasks`.
    let size = world.base("unsigned long", E::Unsigned, 8);
    let link = world.record("list_head", 16, &[]);
    let link_pointer = world.pointer(Some(link));
    world.set_members(
        link,
        &[("next", link_pointer, 0), ("prev", link_pointer, 8)],
    );
    let task = world.record("task", 24, &[("pid", int, 0), ("node", link, 8)]);
    world.identify(task, SourceLanguage::C, &[], "task", Vec::new());
    let queue = world.record("run_queue", 24, &[("tasks", link, 0), ("nr", size, 16)]);
    world.identify(queue, SourceLanguage::C, &[], "run_queue", Vec::new());
    let first = world.allocate(&[0; 24]);
    let second = world.allocate(&[0; 24]);
    let sentinel = world.variable("queue", queue, &[0; 24]);
    let mut words = |address, pid: i32, next: u64, prev: u64| {
        let mut object = i64::from(pid).to_le_bytes().to_vec();
        object.extend(next.to_le_bytes());
        object.extend(prev.to_le_bytes());
        world.write(&Place::Memory { address, ty: task }, &object);
    };

    words(first, 10, second + 8, sentinel);
    words(second, 20, sentinel, first + 8);
    world.write(
        &Place::Memory {
            address: sentinel,
            ty: queue,
        },
        &bytes(&[first + 8, second + 8, 2]),
    );

    // A handle is an index into the global `arena`.
    let arena = world.array(int, &[4]);
    world.variable("arena", arena, &ints([5, 6, 7, 8]));
    let handle = world.record("handle_t", 4, &[("index", int, 0)]);
    world.identify(handle, SourceLanguage::C, &[], "handle_t", Vec::new());
    world.variable("slot", handle, &ints([2]));
}

/// The identities of the classes of the world's object at `index`.
fn lineage(world: &World, index: usize) -> Vec<crate::TypeIdentity> {
    world
        .lineage(index)
        .iter()
        .map(|class| {
            let (module, name) = class.rsplit_once('.').expect("a qualified class");
            crate::runtime_model::interpreted_identity(SourceLanguage::Python, Some(module), name)
        })
        .collect()
}

/// What a value shows as an example writes it.
fn example_outcome(views: &ViewSet, name: &str, outcome: &str) -> String {
    let mut world = world();
    let (choice, this) = world.dynamic_index(name).map_or_else(
        || {
            (
                choose(views, world.type_of(name), &world),
                place(&world, name),
            )
        },
        |index| {
            (
                super::choose_dynamic(&[views], &lineage(&world, index), &world),
                Place::Dynamic(index),
            )
        },
    );
    let Some(bound) = choice.bound else {
        return choice.candidates.first().map_or_else(
            || "unbound: no view's pattern names the type".to_owned(),
            |candidate| {
                format!(
                    "unbound: {}",
                    candidate
                        .rejection
                        .as_ref()
                        .map(ToString::to_string)
                        .unwrap_or_default()
                )
            },
        );
    };

    let listed = super::run::visualizers(&bound);
    if outcome.starts_with("visualizers: ") {
        let drawn = listed
            .iter()
            .filter_map(|(_, visualizer)| {
                let drawing = visualizer.bound.as_ref().ok()?;
                let inputs = drawing
                    .inputs
                    .iter()
                    .map(|input| input.name.as_ref())
                    .collect::<Vec<_>>();
                Some(format!("{}({})", visualizer.name, inputs.join(", ")))
            })
            .collect::<Vec<_>>();
        return format!("visualizers: {}", drawn.join(", "));
    }

    if outcome.starts_with("unbound visualizers: ") {
        let unbound = listed
            .iter()
            .filter_map(|(_, visualizer)| {
                let rejection = visualizer.bound.as_ref().err()?;
                Some(format!("`{}`: {}", visualizer.name, rejection.reason))
            })
            .collect::<Vec<_>>();
        return format!("unbound visualizers: {}", unbound.join("; "));
    }

    let mut checkpoints = Checkpoints::default();
    let presented = match present(&bound, &mut world, this.clone(), &mut checkpoints) {
        Ok(presented) => presented,
        Err(failure) => return failed(failure),
    };

    if !outcome.starts_with("children: ") {
        return presented.summary;
    }

    let elements = presented.count.map_or(0, crate::PresentedCount::known);

    match children(&bound, &mut world, this, elements, 0, 64, &mut checkpoints) {
        Ok(children) => format!(
            "children: {}",
            children.iter().map(rendered).collect::<Vec<_>>().join(", ")
        ),
        Err(failure) => failed(failure),
    }
}

/// Every example in the views reference, `docs/views.md`, holds, so the
/// reference and the implementation cannot drift apart.
#[test]
fn every_example_in_the_views_reference_holds() {
    const REFERENCE: &str = include_str!("../../docs/views.md");
    let mut failures = Vec::new();
    let mut blocks = 0;
    let mut rows = 0;
    let mut lines = REFERENCE.lines().enumerate();
    while let Some((_, line)) = lines.next() {
        if line.trim() != "```microscope-view-example" {
            continue;
        }

        blocks += 1;
        let mut file = String::new();
        let mut examples = Vec::new();
        let mut in_rows = false;
        for (index, line) in lines.by_ref() {
            match line.trim() {
                "```" => break,
                "---" => in_rows = true,
                row if in_rows && !row.is_empty() => examples.push((index + 1, row.to_owned())),
                _ if !in_rows => {
                    file.push_str(line);
                    file.push('\n');
                }
                _ => {}
            }
        }

        let parsed = syntax::parse("example.views", &file);
        let views = ViewSet::new([("example.views", file.as_str())]);

        for (line, row) in examples {
            rows += 1;
            let (name, expected) = row
                .split_once(" => ")
                .unwrap_or_else(|| panic!("docs/views.md:{line}: `{row}` has no ` => `"));
            let actual = if name == "file" {
                let errors = parsed
                    .errors
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>();

                let wanted = expected.strip_prefix("error: ").unwrap_or(expected);
                match errors.iter().find(|error| error.contains(wanted)) {
                    Some(_) => expected.to_owned(),
                    None => format!("errors {errors:?}"),
                }
            } else {
                let actual = example_outcome(&views, name, expected);

                match expected.strip_prefix("unbound: ") {
                    Some(reason) if actual.contains(reason) => expected.to_owned(),
                    _ => actual,
                }
            };

            if actual != expected {
                failures.push(format!(
                    "docs/views.md:{line}: `{name}`\n    expected {expected}\n    actual   {actual}"
                ));
            }
        }
    }

    assert!(
        blocks >= 8 && rows >= 15,
        "the reference's examples were found"
    );
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// Lays out list nodes holding `values`, each linked to the node `links`
/// names (`None` for null), and returns their addresses.
fn nodes(world: &mut World, values: &[i32], links: &[Option<usize>]) -> Vec<u64> {
    let base = world.allocate(&vec![0; values.len() * 16]);
    let address = |index: usize| base + 16 * index as u64;
    let mut bytes = Vec::new();
    for (value, link) in values.iter().zip(links) {
        bytes.extend_from_slice(&i64::from(*value).to_le_bytes());
        bytes.extend_from_slice(&link.map_or(0, address).to_le_bytes());
    }

    world.map(base, &bytes);
    (0..values.len()).map(address).collect()
}

/// A chain of nodes holding `values`, the last linked to null.
fn chain(world: &mut World, values: &[i32]) -> Vec<u64> {
    let links = (1..=values.len())
        .map(|next| (next < values.len()).then_some(next))
        .collect::<Vec<_>>();

    nodes(world, values, &links)
}

/// Tree nodes `(key, value, left, right)`, children by index.
fn tree_nodes(world: &mut World, nodes: &[(i32, i32, Option<usize>, Option<usize>)]) -> Vec<u64> {
    let base = world.allocate(&vec![0; nodes.len() * 24]);
    let address = |index: usize| base + 24 * index as u64;
    let mut bytes = Vec::new();
    for (key, value, left, right) in nodes {
        bytes.extend_from_slice(&ints([*key, *value]));
        bytes.extend_from_slice(&left.map_or(0, address).to_le_bytes());
        bytes.extend_from_slice(&right.map_or(0, address).to_le_bytes());
    }

    world.map(base, &bytes);
    (0..nodes.len()).map(address).collect()
}

/// Hand-rolled C linked structures: lists of `node {int value; node
/// *next}`, binary trees of `tnode {int key; int value; tnode *left; tnode
/// *right}`, an open-addressed table, and chained buckets.
fn add_linked(world: &mut World) {
    let int = world.base("int", E::Signed, 4);
    let size = world.base("unsigned long", E::Unsigned, 8);
    let node = world.record("node", 16, &[]);
    let node_pointer = world.pointer(Some(node));
    world.set_members(node, &[("value", int, 0), ("next", node_pointer, 8)]);
    let list = world.record("list", 16, &[("head", node_pointer, 0), ("count", size, 8)]);
    world.identify(list, SourceLanguage::C, &[], "list", Vec::new());
    let tnode = world.record("tnode", 24, &[]);
    let tnode_pointer = world.pointer(Some(tnode));
    world.set_members(
        tnode,
        &[
            ("key", int, 0),
            ("value", int, 4),
            ("left", tnode_pointer, 8),
            ("right", tnode_pointer, 16),
        ],
    );
    let tree = world.record(
        "tree",
        16,
        &[("root", tnode_pointer, 0), ("count", size, 8)],
    );
    world.identify(tree, SourceLanguage::C, &[], "tree", Vec::new());
    let slot = world.record(
        "slot",
        12,
        &[("used", int, 0), ("key", int, 4), ("value", int, 8)],
    );
    let slot_pointer = world.pointer(Some(slot));
    let table = world.record(
        "table",
        24,
        &[
            ("slots", slot_pointer, 0),
            ("cap", size, 8),
            ("n", size, 16),
        ],
    );
    world.identify(table, SourceLanguage::C, &[], "table", Vec::new());
    let buckets = world.pointer(Some(node_pointer));
    let chained = world.record(
        "chained",
        24,
        &[
            ("buckets", buckets, 0),
            ("nbuckets", size, 8),
            ("n", size, 16),
        ],
    );
    world.identify(chained, SourceLanguage::C, &[], "chained", Vec::new());

    // 1 -> 2 -> 3 -> null
    let counted = chain(world, &[1, 2, 3]);
    world.variable("three", list, &bytes(&[counted[0], 3]));
    world.variable("empty_list", list, &bytes(&[0, 0]));

    // A circular list, whose last node leads back to its first.
    let circle = nodes(world, &[4, 5, 6], &[Some(1), Some(2), Some(0)]);
    world.variable("circle", list, &bytes(&[circle[0], 3]));

    // 7 -> 8 -> 9 -> 8 -> …: a cycle that does not pass the head.
    let looped = nodes(world, &[7, 8, 9], &[Some(1), Some(2), Some(1)]);
    world.variable("looped", list, &bytes(&[looped[0], 5]));

    // A list shorter than its count.
    world.variable("short", list, &bytes(&[counted[0], 5]));
    let long = chain(world, &(0..1000).collect::<Vec<_>>());
    world.variable("long", list, &bytes(&[long[0], 1000]));

    //     2
    //    / \
    //   1   3
    let balanced = tree_nodes(
        world,
        &[
            (2, 20, Some(1), Some(2)),
            (1, 10, None, None),
            (3, 30, None, None),
        ],
    );
    world.variable("balanced", tree, &bytes(&[balanced[0], 3]));

    // A tree whose rightmost node leads back to the root.
    let tangled = tree_nodes(
        world,
        &[
            (2, 20, Some(1), Some(2)),
            (1, 10, None, None),
            (3, 30, None, Some(0)),
        ],
    );
    world.variable("tangled", tree, &bytes(&[tangled[0], 5]));

    // A chain of left children deeper than any tree a library builds.
    let deep = (0..200_usize)
        .map(|index| {
            let key = i32::try_from(index).expect("small");
            (key, key, (index + 1 < 200).then_some(index + 1), None)
        })
        .collect::<Vec<_>>();
    let deep = tree_nodes(world, &deep);
    world.variable("deep", tree, &bytes(&[deep[0], 200]));

    // Slots 1 and 3 of 4 are used.
    let mut slots = Vec::new();
    for slot in [[0, 0, 0], [1, 5, 50], [0, 9, 90], [1, 6, 60]] {
        slots.extend_from_slice(&ints(slot));
    }

    let slots = world.allocate(&slots);
    world.variable("sparse", table, &bytes(&[slots, 4, 2]));

    // Bucket 0 holds 1 -> 2, bucket 1 nothing, bucket 2 holds 3.
    let first = chain(world, &[1, 2]);
    let second = chain(world, &[3]);
    let array = world.allocate(&bytes(&[first[0], 0, second[0]]));
    world.variable("buckets", chained, &bytes(&[array, 3, 3]));
}

const LINKED_VIEWS: &str = "microscope-views 1
view c list {
    show sequence(count) for x in list(head, n => n->next) => x->value
}
view c tree {
    show map(count) for x in inorder(root, n => n->left, n => n->right) => x->key : x->value
}
view c table {
    show map(n) for i in range(cap) if slots[i].used != 0 => slots[i].key : slots[i].value
}
view c chained {
    show sequence(n) for b in range(nbuckets) for x in list(buckets[b], p => p->next) => x->value
}
";

/// Every element or entry of a presented value, in pages of `size`.
fn paged(world: &mut World, views: &ViewSet, name: &str, size: u64) -> Vec<String> {
    let bound = bound(world, views, name);
    let this = place(world, name);
    let mut checkpoints = Checkpoints::default();
    let presented = present(&bound, world, this.clone(), &mut checkpoints).expect("presents");
    let count = presented.count.map_or(0, crate::PresentedCount::known);
    let mut elements = Vec::new();
    let mut offset = 0;

    while offset < count {
        let page = children(
            &bound,
            world,
            this.clone(),
            count,
            offset,
            size,
            &mut checkpoints,
        )
        .expect("a page");
        elements.extend(
            page.iter()
                .filter(|child| matches!(child, Child::Element(..) | Child::Entry(..)))
                .map(rendered),
        );
        offset += size;
    }

    elements
}

#[test]
fn a_list_without_a_count_is_counted_as_far_as_the_budget_allows() {
    let mut world = world();
    let views = set("microscope-views 1
view c list {
    show sequence(_) for x in list(head, n => n->next) => x->value
}
");
    assert_eq!(
        summary(&mut world, &views, "long"),
        "len=1000 [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, …]"
    );
    world.work = Some(400);
    let shown = presented(&mut world, &views, "long").expect("presents");
    let Some(crate::PresentedCount::AtLeast(count)) = shown.count else {
        panic!("{:?}", shown.count);
    };

    assert!((16..1000).contains(&count), "{count}");
    assert!(
        shown.summary.starts_with(&format!("len>={count} [0, 1, 2")),
        "{}",
        shown.summary
    );
}

/// A later page resumes from the nearest checkpoint rather than walking
/// from the start again, and pages of any size read the same elements.
#[test]
fn scans_resume_from_checkpoints_and_page_alike_in_any_size() {
    let mut world = world();
    let views = set(LINKED_VIEWS);
    let bound = bound(&world, &views, "long");
    let this = place(&world, "long");
    let mut fresh = Checkpoints::default();
    world.reads.clear();
    let page =
        children(&bound, &mut world, this.clone(), 1000, 900, 4, &mut fresh).expect("a page");
    assert!(
        matches!(page.first(), Some(Child::Element(900, _))),
        "{page:?}"
    );
    let from_start = world.reads.len();
    world.reads.clear();
    let page = children(&bound, &mut world, this, 1000, 904, 4, &mut fresh).expect("a page");
    assert!(
        matches!(page.first(), Some(Child::Element(904, _))),
        "{page:?}"
    );
    let resumed = world.reads.len();
    assert!(
        resumed * 4 < from_start,
        "{resumed} reads resumed, {from_start} from the start"
    );

    for (name, size) in [("long", 7), ("balanced", 1), ("sparse", 1), ("buckets", 2)] {
        assert_eq!(
            paged(&mut world, &views, name, size),
            paged(&mut world, &views, name, 300),
            "{name}"
        );
    }
}

/// However little budget there is, a presentation is the whole one or a
/// problem, never a different one.
#[test]
fn too_little_work_never_makes_a_presentation_wrong() {
    let views = ViewSet::new([("test.views", VIEWS), ("linked.views", LINKED_VIEWS)]);
    for name in ["v", "three", "balanced", "sparse", "buckets"] {
        let mut unlimited = world();
        unlimited.work = Some(100_000);
        let full = summary(&mut unlimited, &views, name);
        let used = 100_000 - unlimited.work.expect("limited");

        for budget in 0..used {
            let mut limited = world();
            limited.work = Some(budget);
            let limited = summary(&mut limited, &views, name);
            assert!(
                limited == full || limited.contains("limit") || limited.contains("<unavailable>"),
                "{name}: {budget} of {used} units gave `{limited}`"
            );
        }
    }
}

#[test]
fn an_element_a_scan_reaches_is_a_place() {
    let mut world = world();
    let bound = bound(&world, &set(LINKED_VIEWS), "three");
    let this = place(&world, "three");
    let mut checkpoints = Checkpoints::default();
    let second = super::run::element_place(&bound, &mut world, this.clone(), 1, &mut checkpoints)
        .expect("a place");
    assert!(
        matches!(
            crate::eval::target::Machine::load(&mut world, &second),
            Ok(VariableValue::Scalar(crate::ScalarValue::Signed(2)))
        ),
        "{second:?}"
    );
    let past = super::run::element_place(&bound, &mut world, this, 3, &mut checkpoints);
    assert!(
        matches!(&past, Err(Failure::Problem(problem)) if problem.to_string().contains("out of bounds") || problem.to_string().contains("index")),
        "{past:?}"
    );
}

/// A `type` with arguments is the one type whose identity it spells, found
/// by identity whatever its name; `offsetof` and `TYPE.Name` reach a
/// record's layout and the types declared inside it.
#[test]
fn types_are_constructed_from_arguments_and_layouts_named() {
    let mut world = World::new();
    let int = world.base("int", E::Signed, 4);
    let long = world.base("long", E::Signed, 8);
    let node = |world: &mut World, element, name: &str, size| {
        let node = world.record(name, size, &[("next", element, 0), ("value", element, 8)]);
        world.identify(
            node,
            SourceLanguage::Cpp,
            &["lib"],
            "node_of",
            vec![TypeArgument::Type(element)],
        );
        node
    };

    let int_node = node(&mut world, int, "node_of<int>", 12);
    let _long_node = node(&mut world, long, "node_of<long int>", 16);

    // A copy of `node_of<int>` from another unit, as DWARF has one per unit.
    let copy = node(&mut world, int, "node_of<int>", 12);
    let holder = world.record("holder<int>", 8, &[("first", int, 0), ("second", int, 4)]);
    world.identify(
        holder,
        SourceLanguage::Cpp,
        &["lib"],
        "holder",
        vec![TypeArgument::Type(int)],
    );
    let header = world.record("holder<int>.Header", 4, &[("size", int, 0)]);
    world.variable("h", holder, &ints([3, 4]));
    let _ = (int_node, copy, header);

    let views = |body: &str| {
        ViewSet::new([(
            "test.views",
            format!("microscope-views 1\nview c++ lib::holder<T> {{\n{body}\n}}\n").as_str(),
        )])
    };

    let shown = |world: &mut World, body: &str| {
        let views = views(body);
        assert!(views.errors().is_empty(), "{:?}", views.errors());
        summary(world, &views, "h")
    };

    assert_eq!(
        shown(
            &mut world,
            "    type Node = lib::node_of<T>\n    show value(sizeof(Node) + offsetof(Node, value))"
        ),
        "20"
    );
    assert_eq!(
        shown(
            &mut world,
            "    type Header = typeof(self).Header\n    show value(sizeof(Header))"
        ),
        "4"
    );

    // Same-named instances of one identity are one type, but different
    // identities are not.
    world.base("char", E::SignedCharacter, 1);

    for (body, reason) in [
        (
            "    type Node = lib::node_of<U>\n    show empty(\"\")",
            "`U` is neither an argument the pattern captured nor a type the view names",
        ),
        (
            "    type Node = lib::node_of<_>\n    show empty(\"\")",
            "names several types",
        ),
        (
            "    type Node = lib::node_of<char>\n    show empty(\"\")",
            "no type is `lib::node_of<char>`",
        ),
        (
            "    type Node = lib::node_of<T>\n    show value(offsetof(Node, third))",
            "has no member `third` of its own",
        ),
        (
            "    type Missing = typeof(self).Missing\n    show empty(\"\")",
            "declares no type `Missing`",
        ),
    ] {
        let views = views(body);
        let choice = choose(&views, holder, &world);
        let rejection = choice
            .candidates
            .first()
            .and_then(|candidate| candidate.rejection.as_ref())
            .unwrap_or_else(|| panic!("`{body}` bound"));
        assert!(
            rejection.to_string().contains(reason),
            "`{body}`: {rejection}"
        );
    }
}

/// A type a function's arguments complete is found as the program runs,
/// the function's parameters' names coming before the view's own.
#[test]
fn types_are_constructed_from_the_arguments_of_a_function_at_run_time() {
    let mut world = World::new();
    let int = world.base("int", E::Signed, 4);
    let long = world.base("long int", E::Signed, 8);
    for (element, name, size) in [(int, "node_of<int>", 12), (long, "node_of<long int>", 16)] {
        let node = world.record(name, size, &[("next", element, 0), ("value", element, 8)]);
        world.identify(
            node,
            SourceLanguage::Cpp,
            &["lib"],
            "node_of",
            vec![TypeArgument::Type(element)],
        );
    }

    let holder = world.record("holder<int>", 8, &[("first", int, 0), ("second", int, 4)]);
    world.identify(
        holder,
        SourceLanguage::Cpp,
        &["lib"],
        "holder",
        vec![TypeArgument::Type(int)],
    );
    world.variable("h", holder, &ints([3, 4]));
    world.function(0x7000, &[("T", long)]);
    world.function(0x7100, &[("T", holder)]);
    let found = |world: &mut World, code: &str| {
        let views = ViewSet::new([(
            "test.views",
            format!(
                "microscope-views 1\nview c++ lib::holder<T> {{\n    show dynamic(&first, lib::node_of<T> of {code})\n}}\n"
            )
            .as_str(),
        )]);
        assert!(views.errors().is_empty(), "{:?}", views.errors());

        presented(world, &views, "h").map(|presented| {
            presented
                .inner
                .and_then(|inner| inner.type_info)
                .map(|info| info.name.to_string())
                .unwrap_or_default()
        })
    };

    assert_eq!(
        found(&mut world, "0x7000"),
        Ok("node_of<long int>".to_owned())
    );
    for (code, problem) in [
        (
            "0x7100",
            "no type is `lib::node_of<T>` of the function at 0x7100",
        ),
        (
            "0x7200",
            "no function the debug information describes has its code at 0x7200",
        ),
    ] {
        let failure = found(&mut world, code).expect_err(code);
        assert!(failure.contains(problem), "{code}: {failure}");
    }
}

#[test]
fn generators_and_maps_parse_with_their_errors_where_they_are() {
    let file = syntax::parse(
        "ok.views",
        "microscope-views 1
view c a {
    show map(n)
        for x in inorder(root, n => n->left, n => n->right)
            if x->key != 0
        for y in list(x->chain, p => p->next) if y != 0
        => x->flag ? x->key : -1 : std::max::value
}
",
    );
    assert!(file.errors.is_empty(), "{:?}", file.errors);
    let syntax::Statement::Show(syntax::Shape::Map {
        clauses,
        key,
        value,
        ..
    }) = &file.views[0].statements[0]
    else {
        panic!("a map");
    };

    assert_eq!(clauses.len(), 2);
    assert!(matches!(
        clauses[0].items.as_slice(),
        [syntax::Item::Filter(_)]
    ));
    assert_eq!(key.text(), "x->flag ? x->key : -1");
    assert_eq!(value.text(), "std::max::value");

    for (body, error) in [
        (
            "show map(n) for i in range(n) => i",
            "expected `:` between the entry's key and value",
        ),
        (
            "show sequence(n) => i",
            "expected `for` and a generator after the count",
        ),
        (
            "show sequence(n) for i in tree(r) => i",
            "expected a generator",
        ),
        (
            "show sequence(n) for a in range(1) for b in range(1) for c in range(1) for d in range(1) for e in range(1) => a",
            "generators may nest at most 4 deep",
        ),
        (
            "show sequence(n) for x in list(h, => x) => x",
            "expected the node's name",
        ),
    ] {
        let messages = errors(&format!(
            "microscope-views 1\nview c a {{\n    {body}\n}}\n"
        ));
        assert!(
            messages.iter().any(|message| message.contains(error)),
            "`{body}`: {messages:?}"
        );
    }
}

/// A page whose budget runs out while the scan looks for its next element
/// keeps the elements it found: a sparse table's page is never emptied by
/// the empty slots after its last element.
#[test]
fn a_page_keeps_its_elements_when_the_budget_ends_between_them() {
    let mut world = world();
    let mut slots = Vec::new();
    for index in 0..200 {
        let used = i32::from(index % 50 == 0);
        slots.extend_from_slice(&ints([used, index, index * 10]));
    }

    let slots = world.allocate(&slots);
    world.variable("wide", world.type_of("sparse"), &bytes(&[slots, 200, 4]));
    let views = set(LINKED_VIEWS);
    let bound = bound(&world, &views, "wide");
    let this = place(&world, "wide");
    world.work = Some(300);
    let page = children(
        &bound,
        &mut world,
        this,
        4,
        0,
        4,
        &mut Checkpoints::default(),
    );
    let Ok(page) = page else {
        panic!("{page:?}");
    };

    assert!(
        matches!(page.as_slice(), [Child::Entry(0, ..), ..] if page.len() < 4),
        "{page:?}"
    );

    // So is a summary's preview.
    world.work = Some(150);
    assert_eq!(
        summary(&mut world, &views, "wide"),
        "len=4 {0: 0, <unavailable>, …}"
    );
}

/// A cycle that leads back past the checkpoint a page resumes from, where
/// the nodes visited before it are not remembered, is still found, by
/// Brent's algorithm, before the declared count is reached.
#[test]
fn a_cycle_behind_a_checkpoint_is_still_found() {
    let mut world = world();
    let mut links = (1..300).map(Some).collect::<Vec<_>>();
    links.push(Some(10));
    let values = (0..300).collect::<Vec<_>>();
    let looped = nodes(&mut world, &values, &links);
    world.variable(
        "long_loop",
        world.type_of("long"),
        &bytes(&[looped[0], 100_000]),
    );
    let bound = bound(&world, &set(LINKED_VIEWS), "long_loop");
    let this = place(&world, "long_loop");
    let mut checkpoints = Checkpoints::default();
    let first = children(
        &bound,
        &mut world,
        this.clone(),
        100_000,
        0,
        260,
        &mut checkpoints,
    );
    assert!(matches!(&first, Ok(page) if page.len() == 260), "{first:?}");
    let mut offset = 260;

    let found = loop {
        assert!(offset < 5_000, "no cycle found by element {offset}");
        match children(
            &bound,
            &mut world,
            this.clone(),
            100_000,
            offset,
            200,
            &mut checkpoints,
        ) {
            Ok(_) => offset += 200,
            Err(Failure::Problem(problem)) => break problem,
            Err(Failure::Debugger(error)) => panic!("{error}"),
        }
    };

    assert!(
        matches!(found, crate::ViewProblem::Cycle { at } if at > 300),
        "{found:?}"
    );
}
