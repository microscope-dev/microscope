//! The built-in views present the standard libraries' containers as the
//! `containers` fixtures' `VIEW:` markers say, in every build of the
//! matrix, and every built-in view binds in some build.
//!
//! A marker reads `VIEW: <expression> => <summary>`, where `{c*N}` stands
//! for N of the character c, `{duration A..B}` for a duration from A to B
//! as Go writes them, and a trailing `(any order)` lets a hash table's
//! entries come in any order, `VIEW: <expression> => problem:
//! <words>` when the view must refuse the value, or `VIEW: <expression> =>
//! stored` when no view presents it. Each expression is evaluated in the
//! frame that calls `barrier`.

use microscope::{
    Evaluation, ExceptionStops, Expression, InspectedValue, PresentedCount, PresentedShape,
    StackFrameId, ValueChildQuery, ValueChildRelationship, ValueChildren,
};

use super::*;

/// What a marker says its expression shows.
enum Expected {
    Summary(String),

    /// A summary whose items may come in any order.
    Unordered(String),

    /// Only how many elements or entries, which come in an order that
    /// changes between runs.
    Count(u64),

    /// The view refuses the value, saying this.
    Problem(String),

    /// The presented value's children, each `name = summary` (a field or
    /// a member), `[i] = summary`, or `key: summary`, and `[raw]`.
    Children(String),

    /// No view presents the value, and it holds no text.
    Stored,
}

struct Marker {
    line: usize,
    expression: String,
    expected: Expected,
}

/// `{c*N}` as N of the character c.
fn expand(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;

    while let Some(start) = rest.find('{') {
        let Some((character, count)) = rest[start + 1..]
            .split_once('}')
            .and_then(|(inner, _)| inner.split_once('*'))
            .and_then(|(character, count)| {
                Some((character.chars().next()?, count.parse::<usize>().ok()?))
            })
        else {
            out.push_str(&rest[..=start]);
            rest = &rest[start + 1..];
            continue;
        };

        out.push_str(&rest[..start]);
        out.extend(std::iter::repeat_n(character, count));
        rest = &rest[start + 1 + rest[start + 1..].find('}').expect("closed") + 1..];
    }

    out.push_str(rest);
    out
}

/// What a marker's text after ` => ` says.
fn expectation(text: &str) -> Expected {
    if text == "stored" {
        return Expected::Stored;
    }

    if let Some(problem) = text.strip_prefix("problem: ") {
        return Expected::Problem(problem.to_owned());
    }

    if let Some(children) = text.strip_prefix("children: ") {
        return Expected::Children(children.to_owned());
    }

    if let Some(count) = text.strip_prefix("count: ") {
        return Expected::Count(count.parse().expect("a count"));
    }

    text.strip_suffix(" (any order)").map_or_else(
        || Expected::Summary(expand(text)),
        |summary| Expected::Unordered(expand(summary)),
    )
}

fn markers(source: &str) -> Vec<Marker> {
    let path = format!("{}/tests/fixtures/{source}", env!("CARGO_MANIFEST_DIR"));
    markers_in(&fs::read_to_string(&path).expect("read the fixture's source"))
}

/// The markers in a fixture's source, or in what it printed, each with its
/// line.
fn markers_in(text: &str) -> Vec<Marker> {
    text.lines()
        .enumerate()
        .filter_map(|(index, line)| {
            let marker = line.split_once("VIEW: ")?.1;
            let (expression, expected) = marker.split_once(" => ")?;
            Some(Marker {
                line: index + 1,
                expression: expression.trim().to_owned(),
                expected: expectation(expected.trim()),
            })
        })
        .collect()
}

async fn evaluate(scenario: &Scenario, text: &str) -> InspectedValue {
    let expression = Expression::parse(text).unwrap_or_else(|error| panic!("`{text}`: {error}"));
    match scenario
        .operation(text, scenario.handle().evaluate(&expression))
        .await
    {
        Evaluation::Value { value, .. } => value,
        other => panic!("`{text}` is not a value: {other:?}"),
    }
}

/// A duration as Go writes one, such as `1h2m3.5s` or `250ms`, in
/// nanoseconds.
fn go_duration(text: &str) -> Option<f64> {
    let mut total = 0.0;
    let mut rest = text;

    while !rest.is_empty() {
        let digits = rest.find(|c: char| !c.is_ascii_digit() && c != '.')?;
        let number = rest[..digits].parse::<f64>().ok()?;
        rest = &rest[digits..];

        let (scale, unit) = [
            (1e6, "ms"),
            (1e3, "µs"),
            (1.0, "ns"),
            (3.6e12, "h"),
            (6e10, "m"),
            (1e9, "s"),
        ]
        .into_iter()
        .find(|(_, unit)| rest.starts_with(unit))?;

        total = number.mul_add(scale, total);
        rest = &rest[unit.len()..];
    }

    (!text.is_empty()).then_some(total)
}

/// Whether a summary is what a marker expects, each `{duration A..B}` in
/// it any duration from A to B.
fn summary_matches(expected: &str, actual: &str) -> bool {
    let Some((head, tail)) = expected.split_once("{duration ") else {
        return expected == actual;
    };

    let Some(rest) = actual.strip_prefix(head) else {
        return false;
    };

    let (range, tail) = tail.split_once('}').expect("a closed duration");
    let (low, high) = range.split_once("..").expect("a duration range");

    let end = rest
        .find(|c: char| !c.is_ascii_alphanumeric() && c != '.' && c != 'µ')
        .unwrap_or(rest.len());

    let bounds = (go_duration(low), go_duration(high));
    go_duration(&rest[..end]).is_some_and(|duration| {
        bounds.0.is_some_and(|low| low <= duration) && bounds.1.is_some_and(|high| duration <= high)
    }) && summary_matches(tail, &rest[end..])
}

/// A summary with its items sorted, so that items in any order compare
/// equal: `len=2 {2: 20, 1: 10}` is `len=2 {1: 10, 2: 20}`.
fn sorted_items(summary: &str) -> String {
    let Some(open) = summary.find(['[', '{']) else {
        return summary.to_owned();
    };

    let (head, body) = summary.split_at(open);
    let (inner, close) = body[1..].split_at(body.len() - 2);
    let mut items = Vec::new();
    let mut depth = 0_i32;
    let mut quoted = false;
    let mut start = 0;
    let bytes = inner.as_bytes();

    for (index, byte) in bytes.iter().enumerate() {
        match byte {
            b'"' if index == 0 || bytes[index - 1] != b'\\' => quoted = !quoted,
            b'[' | b'{' if !quoted => depth += 1,
            b']' | b'}' if !quoted => depth -= 1,
            b',' if !quoted && depth == 0 => {
                items.push(inner[start..index].trim());
                start = index + 1;
            }
            _ => {}
        }
    }

    if !inner.trim().is_empty() {
        items.push(inner[start..].trim());
    }

    items.sort_unstable();
    format!("{head}{}{}{close}", &body[..1], items.join(", "))
}

fn presentation(value: &InspectedValue) -> Option<&microscope::Presentation> {
    match &value.state {
        VariableState::Available { presentation, .. } => presentation.as_deref(),
        _ => None,
    }
}

fn summary(value: &InspectedValue) -> String {
    match &value.state {
        VariableState::Available {
            presentation: Some(presentation),
            ..
        } => presentation.summary.to_string(),
        state => format!("{state:?}"),
    }
}

/// Whether a child is an element or an entry.
const fn is_item(relationship: &ValueChildRelationship) -> bool {
    matches!(
        relationship,
        ValueChildRelationship::Element { .. } | ValueChildRelationship::Entry { .. }
    )
}

/// The first `wanted` children, in as few pages as the largest budget
/// allows: a scan resumes where the page before it ran out.
async fn first_children(
    scenario: &Scenario,
    marker: &Marker,
    reference: &Arc<microscope::ValueChildrenReference>,
    wanted: u64,
    failures: &mut Vec<String>,
) -> Option<Vec<microscope::ValueChild>> {
    let mut children = Vec::new();

    while (children.len() as u64) < wanted {
        let page = scenario
            .operation(
                "children",
                scenario.handle().value_children_with_limits(
                    Arc::clone(reference),
                    ValueChildQuery {
                        offset: children.len() as u64,
                        limit: u32::try_from(wanted - children.len() as u64).expect("small"),
                    },
                    microscope::InspectionLimits {
                        memory_reads: 1024,
                        ..microscope::InspectionLimits::default()
                    },
                ),
            )
            .await;
        if page.children.is_empty() {
            failures.push(format!(
                "line {}: a page at {} is empty: {:?}",
                marker.line,
                children.len(),
                page.completion
            ));
            return None;
        }

        if page.children.len() as u64 != wanted - children.len() as u64
            && page.completion.exhaustion().is_none()
        {
            failures.push(format!(
                "line {}: a short page at {} is complete",
                marker.line,
                children.len()
            ));
        }

        children.extend(page.children.iter().cloned());
    }

    Some(children)
}

/// Checks a presented sequence's or map's children: its elements or
/// entries, which evaluate back, the same in pages of any size; its fields;
/// and `[raw]`, the value as stored.
async fn check_children(
    scenario: &Scenario,
    marker: &Marker,
    presentation: &microscope::Presentation,
    failures: &mut Vec<String>,
) {
    let ValueChildren::Available(reference) = &presentation.children else {
        failures.push(format!("line {}: no children", marker.line));
        return;
    };

    let count = presentation.count.map_or(0, PresentedCount::known);
    let wanted = reference.total().min(256);
    let Some(whole) = first_children(scenario, marker, reference, wanted, failures).await else {
        return;
    };

    let elements = whole
        .iter()
        .filter(|child| is_item(&child.relationship))
        .count() as u64;
    if elements != count.min(256) {
        failures.push(format!(
            "line {}: {elements} elements of {count}",
            marker.line
        ));
    }

    let raw = whole.last();
    if !matches!(
        raw,
        Some(microscope::ValueChild {
            relationship: ValueChildRelationship::Raw,
            state: VariableState::Available {
                presentation: None,
                ..
            },
            ..
        })
    ) && reference.total() <= 256
    {
        failures.push(format!("line {}: no [raw] child: {raw:?}", marker.line));
    }

    // Small pages read the same elements as one large page.
    let mut paged = Vec::new();
    let mut offset = 0;

    while offset < count.min(32) {
        let small = scenario
            .operation(
                "a small page",
                scenario
                    .handle()
                    .value_children(Arc::clone(reference), ValueChildQuery { offset, limit: 7 }),
            )
            .await;
        paged.extend(small.children.iter().map(|child| child.state.clone()));
        offset += 7;
    }

    let expected = whole
        .iter()
        .take(paged.len())
        .map(|child| child.state.clone())
        .collect::<Vec<_>>();
    if paged.len() > expected.len() || paged[..expected.len()] != expected[..] {
        failures.push(format!(
            "line {}: pages of 7 read other elements",
            marker.line
        ));
    }

    for index in [0, count.saturating_sub(1)] {
        if index < count.min(256) {
            let child = &whole[usize::try_from(index).expect("small")];
            check_name(scenario, marker, index, child, failures).await;
        }
    }
}

/// An element's name evaluates back to it, and so does where an entry's
/// value is.
async fn check_name(
    scenario: &Scenario,
    marker: &Marker,
    index: u64,
    child: &microscope::ValueChild,
    failures: &mut Vec<String>,
) {
    let name = match (&child.relationship, &child.state) {
        // An element a view computes, as a bitset's positions are, has no
        // place to index.
        (
            ValueChildRelationship::Element { .. },
            VariableState::Available {
                source: microscope::VariableValueSource::Computed,
                ..
            },
        ) => return,
        (ValueChildRelationship::Element { .. }, _) => {
            format!("({})[{index}]", marker.expression)
        }

        (
            ValueChildRelationship::Entry { .. },
            VariableState::Available {
                source: microscope::VariableValueSource::Memory(address),
                ..
            },
        ) => microscope::Expression::at(&child.type_info.name, address.get())
            .expect("an entry's place")
            .to_string(),
        other => {
            failures.push(format!("line {}: child {index} is {other:?}", marker.line));
            return;
        }
    };

    let again = evaluate(scenario, &name).await;
    let rendered = |state: &VariableState| match state {
        VariableState::Available {
            presentation: Some(presentation),
            ..
        } => presentation.summary.to_string(),
        VariableState::Available { value, text, .. } => format!("{value:?} {text:?}"),
        state => format!("{state:?}"),
    };

    if rendered(&again.state) != rendered(&child.state) {
        failures.push(format!(
            "line {}: `{name}` is {} but child {index} is {}",
            marker.line,
            rendered(&again.state),
            rendered(&child.state)
        ));
    }
}

/// Checks every child of a presented value, as a `children:` marker lists
/// them, noting the views that presented them.
async fn check_listed_children(
    scenario: &Scenario,
    marker: &Marker,
    presentation: &microscope::Presentation,
    expected: &str,
    seen: &mut BTreeSet<String>,
    failures: &mut Vec<String>,
) {
    let ValueChildren::Available(reference) = &presentation.children else {
        failures.push(format!("line {}: no children", marker.line));
        return;
    };

    let Some(children) =
        first_children(scenario, marker, reference, reference.total(), failures).await
    else {
        return;
    };

    seen.extend(children.iter().filter_map(|child| match &child.state {
        VariableState::Available {
            presentation: Some(presentation),
            ..
        } => Some(presentation.view.to_string()),
        _ => None,
    }));

    let rendered = children
        .iter()
        .map(|child| {
            let value = microscope::value_summary(Some(&child.type_info), &child.state);
            match &child.relationship {
                ValueChildRelationship::Element { index } => format!("[{index}] = {value}"),
                ValueChildRelationship::Entry { key, .. } => format!(
                    "{}: {value}",
                    microscope::value_summary(Some(&key.type_info), &key.state)
                ),
                ValueChildRelationship::Field { name } => format!("{name} = {value}"),
                ValueChildRelationship::Member(member) => format!(
                    "{} = {value}",
                    member.name.as_deref().unwrap_or("<anonymous>")
                ),
                ValueChildRelationship::Raw => "[raw]".to_owned(),
                other => format!("{other:?} = {value}"),
            }
        })
        .collect::<Vec<_>>()
        .join(", ");
    if rendered != expected {
        failures.push(format!(
            "line {}: `{}`'s children\n    expected {expected}\n    actual   {rendered}",
            marker.line, marker.expression
        ));
    }
}

/// Checks what one marker says its expression shows.
async fn check_marker(
    scenario: &Scenario,
    marker: &Marker,
    value: &InspectedValue,
    seen: &mut BTreeSet<String>,
    failures: &mut Vec<String>,
) {
    if matches!(marker.expected, Expected::Stored) {
        if !matches!(
            value.state,
            VariableState::Available {
                text: None,
                presentation: None,
                ..
            }
        ) {
            failures.push(format!(
                "line {}: `{}` is presented: {:?}",
                marker.line, marker.expression, value.state
            ));
        }

        return;
    }

    // Text a language's own types hold, such as Rust's `Box<str>`, needs
    // no view.
    if let (
        VariableState::Available {
            text: Some(text),
            presentation: None,
            ..
        },
        Expected::Summary(expected) | Expected::Unordered(expected),
    ) = (&value.state, &marker.expected)
    {
        if &microscope::quoted_text(text) != expected {
            failures.push(format!("line {}: the text is {text:?}", marker.line));
        }

        return;
    }

    let Some(presentation) = presentation(value) else {
        failures.push(format!(
            "line {}: `{}` has no presentation: {:?}",
            marker.line, marker.expression, value.state
        ));
        return;
    };

    seen.insert(presentation.view.to_string());
    match (&marker.expected, presentation.shape) {
        (Expected::Problem(words), PresentedShape::Raw) => {
            let problem = presentation
                .problem
                .as_ref()
                .map(ToString::to_string)
                .unwrap_or_default();
            if !problem.contains(words.as_str()) {
                failures.push(format!("line {}: the problem is `{problem}`", marker.line));
            }
        }
        (Expected::Problem(_), _) => failures.push(format!(
            "line {}: `{}` shows as {}, not a problem",
            marker.line,
            marker.expression,
            summary(value)
        )),
        (Expected::Stored, _) => unreachable!("checked above"),
        (Expected::Children(expected), _) => {
            check_listed_children(scenario, marker, presentation, expected, seen, failures).await;
        }
        (Expected::Count(expected), _) => {
            if presentation.count != Some(PresentedCount::Exact(*expected)) {
                failures.push(format!(
                    "line {}: `{}` counts {:?}",
                    marker.line, marker.expression, presentation.count
                ));
            }

            check_children(scenario, marker, presentation, failures).await;
        }
        (Expected::Summary(expected) | Expected::Unordered(expected), _) => {
            let mut actual = summary(value);
            if matches!(marker.expected, Expected::Unordered(_)) {
                actual = sorted_items(&actual);
            }

            if !summary_matches(expected, &actual) {
                failures.push(format!(
                    "line {}: `{}`\n    expected {expected}\n    actual   {actual}",
                    marker.line, marker.expression
                ));
            }

            if matches!(
                presentation.shape,
                PresentedShape::Sequence | PresentedShape::Map
            ) {
                check_children(scenario, marker, presentation, failures).await;
            }
        }
    }
}

/// Selects the frame that called the one stopped in.
async fn select_caller(scenario: &Scenario) {
    let trace = scenario
        .operation("backtrace", scenario.handle().backtrace())
        .await;
    let caller: StackFrameId = trace.frames[1].id;
    scenario
        .operation("select caller", scenario.handle().select_frame(caller))
        .await;
}

/// Stops a containers build at `barrier`, checks every marker in its
/// caller, and returns the views that presented values.
async fn check_containers(
    fixture: &str,
    source: &str,
    barrier: &str,
    optimized: bool,
) -> BTreeSet<String> {
    check_containers_but(fixture, source, barrier, optimized, &[]).await
}

/// As [`check_containers`], skipping the markers of `skipped` expressions,
/// which a build has no way to show.
async fn check_containers_but(
    fixture: &str,
    source: &str,
    barrier: &str,
    optimized: bool,
    skipped: &[&str],
) -> BTreeSet<String> {
    let mut scenario = Scenario::launch(fixture);
    scenario.add_breakpoint(barrier).await;
    let reason = scenario.run_to_stop().await;

    assert!(
        matches!(reason, StopReason::Breakpoint { .. }),
        "{fixture}: {reason:?}"
    );

    select_caller(&scenario).await;

    let markers = markers(source)
        .into_iter()
        .filter(|marker| !skipped.contains(&marker.expression.as_str()))
        .collect::<Vec<_>>();
    assert!(markers.len() >= 5, "{source} has its markers");

    let mut seen = BTreeSet::new();
    let mut failures = Vec::new();
    check_markers(&scenario, &markers, optimized, &mut seen, &mut failures).await;
    assert!(failures.is_empty(), "{fixture}:\n{}", failures.join("\n"));
    scenario.shutdown().await;
    seen
}

/// Checks every marker at the stop, noting the views that presented
/// values.
async fn check_markers(
    scenario: &Scenario,
    markers: &[Marker],
    optimized: bool,
    seen: &mut BTreeSet<String>,
    failures: &mut Vec<String>,
) {
    for marker in markers {
        // An optimized build may keep no value of a variable, or no
        // variable at all.
        let value = if optimized {
            let expression = Expression::parse(&marker.expression).expect("an expression");
            match scenario.handle().evaluate(&expression).await {
                Ok(Evaluation::Value { value, .. }) => value,
                Ok(other) => panic!("`{}` is not a value: {other:?}", marker.expression),
                Err(error) if error.to_string().contains("no variable is named") => continue,
                Err(error) => panic!("`{}`: {error}", marker.expression),
            }
        } else {
            evaluate(scenario, &marker.expression).await
        };

        // Nor need it keep everything a view reads, such as the parts of a
        // value it holds in pieces.
        let view_unavailable = matches!(
            &value.state,
            VariableState::Available { presentation: Some(presentation), .. }
                if matches!(presentation.problem, Some(microscope::ViewProblem::Unavailable(_)))
        );
        if optimized && (matches!(value.state, VariableState::Unavailable(_)) || view_unavailable) {
            continue;
        }

        // Or keep it where it has no address, which a view may need.
        if optimized
            && let VariableState::Available {
                presentation: Some(presentation),
                ..
            } = &value.state
            && matches!(
                presentation.problem,
                Some(microscope::ViewProblem::Refused(_))
            )
        {
            continue;
        }

        check_marker(scenario, marker, &value, seen, failures).await;
    }
}

/// Where a fixture's markers are evaluated.
#[derive(Clone, Copy)]
enum MarkedFrame {
    /// In the frame of `barrier`, which the fixture passes its values.
    Barrier,

    /// In the frame that calls `barrier`, whose locals they are.
    Caller,
}

/// Runs a fixture that prints its markers before each call to `barrier`,
/// and checks the markers printed before each stop there, at that stop.
/// The fixture's exceptions do not stop it. Returns the views that
/// presented values.
async fn check_printed_markers(
    fixture: &str,
    barrier: &str,
    frame: MarkedFrame,
    optimized: bool,
) -> BTreeSet<String> {
    let scratch = crate::support::ScratchDir::new("views");
    let output_path = scratch.path().join("stdout");
    let output = fs::File::create(&output_path).expect("create the fixture's output");

    let mut scenario = Scenario::launch(fixture);
    scenario
        .operation(
            "exception stops",
            scenario.handle().set_exception_stops(ExceptionStops::NONE),
        )
        .await;

    scenario.add_breakpoint(barrier).await;
    let mut reason = scenario
        .run_with_to_stop(LaunchOptions {
            stdout: Some(std::process::Stdio::from(output)),
            ..LaunchOptions::default()
        })
        .await;

    let mut seen = BTreeSet::new();
    let mut failures = Vec::new();
    let mut read = 0;
    let mut stops = 0;
    while matches!(reason, StopReason::Breakpoint { .. }) {
        stops += 1;

        // The fixture writes its markers before it calls barrier.
        let printed = fs::read_to_string(&output_path).expect("read the fixture's output");
        let markers = markers_in(&printed[read..]);
        read = printed.len();
        assert!(
            !markers.is_empty(),
            "{fixture} printed markers before stop {stops}"
        );

        if matches!(frame, MarkedFrame::Caller) {
            select_caller(&scenario).await;
        }

        let before = failures.len();
        check_markers(&scenario, &markers, optimized, &mut seen, &mut failures).await;
        for failure in &mut failures[before..] {
            *failure = format!("stop {stops}, printed {failure}");
        }

        reason = scenario.resume_to_stop().await;
    }

    assert_eq!(reason, StopReason::Exited(ExitStatus::Code(0)), "{fixture}");
    assert!(stops >= 2, "{fixture} stopped at each barrier");
    assert!(failures.is_empty(), "{fixture}:\n{}", failures.join("\n"));
    scenario.shutdown().await;
    seen
}

/// Every built-in view of `library` presented some marked value.
fn assert_every_view_binds(library: &str, seen: &BTreeSet<String>) {
    let missing = microscope::built_in_views()
        .into_iter()
        .filter(|view| &*view.source == library)
        .map(|view| view.to_string())
        .filter(|view| !seen.contains(view))
        .collect::<Vec<_>>();
    assert!(
        missing.is_empty(),
        "{library}: no fixture value binds {missing:?}"
    );
}

#[tokio::test]
async fn cpp_containers_present_as_their_views_say_across_the_library_matrix() {
    let mut seen = BTreeSet::new();

    for (fixture, optimized, skipped) in [
        ("containers-cpp-gcc-o0", false, &[][..]),
        ("containers-cpp-gcc-o2", true, &[]),
        ("containers-cpp-clang-o0", false, &[]),
        ("containers-cpp-clang-o2", true, &[]),
        // The old ABI's list keeps no count to be wrong.
        ("containers-cpp-gcc-oldabi", false, &["overcounted"]),
        // Without -fstandalone-debug, libc++'s control blocks are
        // undescribed, so a shared_ptr shows no counts and a weak_ptr cannot
        // say whether its object exists; recursive_mutex, defined in the
        // library, is only declared.
        (
            "containers-cpp-libcxx-o0",
            false,
            &["shared_too", "weak", "expired", "reentered"],
        ),
        (
            "containers-cpp-libcxx-o2",
            true,
            &["shared_too", "weak", "expired", "reentered"],
        ),
        ("containers-cpp-libcxx-standalone", false, &[]),
        ("containers-cpp-gcc-debug", false, &[]),
        ("containers-cpp-gcc-static", false, &[]),
        ("containers-cpp-clang-simple", false, &[]),
    ] {
        seen.extend(
            check_containers_but(fixture, "cpp/containers.cpp", "barrier", optimized, skipped)
                .await,
        );
    }

    assert_every_view_binds("libstdc++.views", &seen);
    assert_every_view_binds("libc++.views", &seen);
}

#[tokio::test]
async fn c_library_values_present_as_their_views_say() {
    let mut seen = BTreeSet::new();

    for (fixture, optimized) in [
        ("glibc-c-gcc-o0", false),
        ("glibc-c-gcc-o2", true),
        ("glibc-c-clang-o0", false),
    ] {
        seen.extend(check_containers(fixture, "c/glibc.c", "barrier", optimized).await);
    }

    assert_every_view_binds("glibc.views", &seen);
}

#[tokio::test]
async fn rust_containers_present_as_their_views_say() {
    let mut seen = BTreeSet::new();
    for (fixture, optimized) in [("containers-rust-o0", false), ("containers-rust-o2", true)] {
        seen.extend(check_containers(fixture, "rust/containers.rs", "barrier", optimized).await);
    }

    assert_every_view_binds("rust-std.views", &seen);
}

#[tokio::test]
async fn zig_containers_present_as_their_views_say() {
    let mut seen = BTreeSet::new();

    for (fixture, optimized, skipped) in [
        ("containers-zig-o0", false, &[][..]),
        // ReleaseSafe emits no entry type for an array hash map, so nothing
        // says how its entries are laid out, and it shows as stored.
        (
            "containers-zig-o2",
            true,
            &["ordered", "strings", "no_ordered"],
        ),
        // Zig's own backend emits no entry type for an array hash map
        // either.
        (
            "containers-zig-self-hosted",
            false,
            &["ordered", "strings", "no_ordered"],
        ),
    ] {
        seen.extend(
            check_containers_but(fixture, "zig/containers.zig", "barrier", optimized, skipped)
                .await,
        );
    }

    assert_every_view_binds("zig-std.views", &seen);
}

#[tokio::test]
async fn odin_containers_present_as_their_views_say() {
    let mut seen = BTreeSet::new();

    for (fixture, optimized) in [("containers-odin-o0", false), ("containers-odin-o2", true)] {
        seen.extend(
            check_containers(
                fixture,
                "odin/containers.odin",
                "containers::barrier",
                optimized,
            )
            .await,
        );
    }

    assert_every_view_binds("odin-core.views", &seen);
}

#[tokio::test]
async fn d_containers_present_as_their_views_say() {
    let mut seen = BTreeSet::new();
    for (fixture, optimized) in [("containers-d-o0", false), ("containers-d-o2", true)] {
        seen.extend(check_containers(fixture, "d/containers.d", "barrier", optimized).await);
    }

    assert_every_view_binds("d-std.views", &seen);
}

#[tokio::test]
async fn go_containers_present_as_their_views_say() {
    let mut seen = BTreeSet::new();

    for (fixture, optimized) in [("containers-go-o0", false), ("containers-go-o2", true)] {
        seen.extend(
            check_containers(fixture, "go/containers/main.go", "main.barrier", optimized).await,
        );
    }

    assert_every_view_binds("go-runtime.views", &seen);
}

/// The standard library's values present as Go itself shows them: the
/// fixture prints each value's marker from its own String and Error
/// methods, so the markers stay true when the toolchain moves.
#[tokio::test]
async fn go_library_values_present_as_go_shows_them() {
    let mut seen = BTreeSet::new();

    for (fixture, optimized) in [("stdlib-go-o0", false), ("stdlib-go-o2", true)] {
        seen.extend(
            check_printed_markers(fixture, "main.barrier", MarkedFrame::Barrier, optimized).await,
        );
    }

    for library in [
        "go-time.views",
        "go-sync.views",
        "go-text.views",
        "go-errors.views",
        "go-containers.views",
    ] {
        assert_every_view_binds(library, &seen);
    }
}

/// tokio's values present as the program says, in each state each view
/// tells apart, and every one of tokio's views binds. Some present values
/// no expression reaches: a task's cell, which a join handle presents as
/// the type its task's code says, and the futures tasks wait on for
/// permits and messages, which the workers tests check, and for lines and
/// sockets, which the attach tests check. Each tokio release the fixtures
/// are built with binds every view.
#[tokio::test]
async fn tokio_values_present_as_their_views_say() {
    for fixture in ["tokio-values-o0", "tokio-values-1.52-o0"] {
        let mut seen = check_printed_markers(fixture, "barrier", MarkedFrame::Caller, false).await;
        seen.extend(
            microscope::built_in_views()
                .into_iter()
                .filter(|view| {
                    [
                        "rust tokio::runtime::task::core::Cell<_, _>",
                        "rust tokio::sync::batch_semaphore::Acquire",
                        "rust core::future::poll_fn::PollFn<tokio::sync::mpsc::**::recv::`{async_fn#0}`::`{closure_env#0}`<_>>",
                        "rust core::future::poll_fn::PollFn<tokio::io::util::lines::**::next_line::`{async_fn#0}`::`{closure_env#0}`<_>>",
                        "rust tokio::runtime::io::scheduled_io::Readiness",
                    ]
                    .contains(&&*view.header)
                })
                .map(|view| view.to_string()),
        );

        assert_every_view_binds("tokio.views", &seen);
    }
}

#[tokio::test]
async fn optimized_tokio_values_present_as_their_views_say() {
    for fixture in ["tokio-values-o3", "tokio-values-1.52-o3"] {
        check_printed_markers(fixture, "barrier", MarkedFrame::Caller, true).await;
    }
}

/// A core of the program's first stop presents its values as the program
/// said before it, which the core's log keeps.
#[tokio::test]
async fn tokio_values_in_a_core_present_as_their_views_say() {
    for (variant, optimized) in [
        ("o0", false),
        ("o3", true),
        ("1.52-o0", false),
        ("1.52-o3", true),
    ] {
        let core = format!("tokio-values-{variant}.core");
        let log =
            fs::read_to_string(Scenario::fixture(&format!("{core}.log"))).expect("the core's log");
        let scenario = Scenario::open_core(
            &core,
            &microscope::CoreDumpOptions::new(Scenario::fixture(&core)),
        );
        let trace = scenario
            .operation("backtrace", scenario.handle().backtrace())
            .await;
        let main = trace
            .frames
            .iter()
            .find(|frame| {
                frame
                    .function
                    .as_ref()
                    .is_some_and(|function| &*function.name == "main")
            })
            .unwrap_or_else(|| panic!("{core}: no frame of main"));

        scenario
            .operation("select main", scenario.handle().select_frame(main.id))
            .await;

        let markers = markers_in(&log);
        assert!(markers.len() >= 50, "{core} keeps its markers");

        let (mut seen, mut failures) = (BTreeSet::new(), Vec::new());
        check_markers(&scenario, &markers, optimized, &mut seen, &mut failures).await;
        assert!(failures.is_empty(), "{core}:\n{}", failures.join("\n"));
        scenario.shutdown().await;
    }
}

/// Inspection sent beside run control never holds it up or answers
/// wrongly: whichever the controller serves first, run control is
/// acknowledged, and each inspection is either its whole answer at the stop
/// or a failure for an old stop, never part of one or a different one.
#[tokio::test]
async fn inspection_sent_beside_run_control_is_whole_or_stale() {
    for round in 0..4 {
        let mut scenario = Scenario::launch("containers-rust-o0");
        scenario.add_breakpoint("barrier").await;
        scenario.run_to_stop().await;

        let trace = scenario
            .operation("backtrace", scenario.handle().backtrace())
            .await;
        let caller = trace.frames[1].id;
        let snapshot = scenario.snapshot().await;
        let InferiorState::Stopped {
            process_id,
            stop_id,
            thread_id,
            ..
        } = snapshot.inferior
        else {
            panic!("round {round}: not stopped: {:?}", snapshot.inferior);
        };

        let context = microscope::StopContext {
            stop: stop_id,
            execution: thread_id.into(),
            frame: caller,
        };

        let handle = scenario.handle().clone();
        let words = Expression::parse("words").expect("an expression");
        let many = Expression::parse("many[299] + len(many)").expect("an expression");
        let view = handle.at(context);
        let (words, many, resumed) = tokio::join!(
            view.evaluate(&words),
            view.evaluate(&many),
            handle.continue_execution(
                stop_id,
                microscope::ResumeScope::Process(process_id),
                microscope::ExceptionDisposition::Pass,
            ),
        );

        resumed.unwrap_or_else(|error| panic!("round {round}: continue failed: {error}"));

        for (result, expected) in [(words, "len=2 [\"one\", \"two\"]"), (many, "599")] {
            match result {
                Ok(Evaluation::Value { value, .. }) => {
                    let shown = match &value.state {
                        VariableState::Available {
                            presentation: Some(presentation),
                            ..
                        } => presentation.summary.to_string(),
                        VariableState::Available {
                            value: microscope::VariableValue::Scalar(ScalarValue::Signed(number)),
                            ..
                        } => number.to_string(),
                        VariableState::Available {
                            value: microscope::VariableValue::Scalar(ScalarValue::Unsigned(number)),
                            ..
                        } => number.to_string(),
                        state => format!("{state:?}"),
                    };

                    assert_eq!(shown, expected, "round {round}");
                }
                Err(Error::StaleStop | Error::NotStopped | Error::NotRunning) => {}
                other => panic!("round {round}: {other:?}"),
            }
        }

        scenario.shutdown().await;
    }
}

/// An assignment sent before run control is made at its stop and answered
/// with its target read again, even when reading it again runs a view long
/// enough to notice the run control waiting: the write cannot be undone or
/// made again, so nothing interrupts that read.
#[tokio::test]
async fn an_assignment_sent_before_run_control_answers_with_its_value() {
    let mut scenario = Scenario::launch("containers-rust-o0");
    scenario.add_breakpoint("barrier").await;
    scenario.run_to_stop().await;
    let trace = scenario
        .operation("backtrace", scenario.handle().backtrace())
        .await;
    let caller = trace.frames[1].id;

    // Each check costs more than the work between looks for run control,
    // and finding an element's place runs them all.
    let sum = vec!["n"; 16].join(" + ");
    let checks = format!("    check {sum} >= 0\n").repeat(5);
    let views = format!(
        "microscope-views 1
view rust alloc::vec::Vec<T, _> {{
    let data = inner(inner(buf).ptr) as *T
    let n = len
{checks}    show sequence(len) for i in range(len) => data[i]
}}
"
    );

    let errors = scenario
        .operation(
            "load views",
            scenario
                .handle()
                .load_views(&[("costly.views", &views)], &[], &[]),
        )
        .await;
    assert!(errors.is_empty(), "{errors:?}");

    let snapshot = scenario.snapshot().await;
    let InferiorState::Stopped {
        process_id,
        stop_id,
        thread_id,
        ..
    } = snapshot.inferior
    else {
        panic!("not stopped: {:?}", snapshot.inferior);
    };

    let handle = scenario.handle().clone();
    let frame = handle.at(microscope::StopContext {
        stop: stop_id,
        execution: thread_id.into(),
        frame: caller,
    });
    let assignment = Expression::parse("ints[0] = 7").expect("an expression");

    // Polled in order, so the assignment is queued first.
    let (assigned, resumed) = tokio::join!(
        frame.evaluate_with(
            &assignment,
            microscope::EvaluationMode::Assign,
            microscope::InspectionLimits::default(),
        ),
        handle.continue_execution(
            stop_id,
            microscope::ResumeScope::Process(process_id),
            microscope::ExceptionDisposition::Pass,
        ),
    );
    resumed.expect("continue");

    match assigned {
        Ok(Evaluation::Value { value, .. }) => assert!(
            matches!(
                value.state,
                VariableState::Available {
                    value: microscope::VariableValue::Scalar(ScalarValue::Signed(7)),
                    ..
                }
            ),
            "{value:?}"
        ),
        other => panic!("{other:?}"),
    }

    scenario.shutdown().await;
}

/// Views loaded for a session come before the built-in ones; a view that
/// presents a value as another lends it that value's children; and what
/// keeps a view out of a file is reported with where it is.
#[tokio::test]
async fn session_views_come_first_and_present_values_as_others() {
    let mut scenario = Scenario::launch("containers-rust-o0");
    scenario.add_breakpoint("barrier").await;
    scenario.run_to_stop().await;
    let trace = scenario
        .operation("backtrace", scenario.handle().backtrace())
        .await;
    scenario
        .operation(
            "select caller",
            scenario.handle().select_frame(trace.frames[1].id),
        )
        .await;

    let errors = scenario
        .operation(
            "load views",
            scenario.handle().load_views(
                &[(
                    "session.views",
                    "microscope-views 1
view rust std::**::PathBuf {
    show value(inner(self))
    field length = inner(self).len
}
view rust alloc::vec::Vec<T, _> {
    show empty(\"a vector\")
}
view rust nowhere {
    show nothing
}
",
                )],
                &[],
                &[],
            ),
        )
        .await;
    assert_eq!(
        errors.iter().map(ToString::to_string).collect::<Vec<_>>(),
        [
            "session.views:10:10: expected a shape: `text`, `value`, `empty`, `sequence`, `map`, `record`, `tuple`, `dynamic`, or `if`, found `nothing`"
        ]
    );

    let ints = evaluate(&scenario, "ints").await;
    assert_eq!(summary(&ints), "a vector");

    // The path presents as its bytes, a Vec<u8> the session's view now
    // presents, whose children it borrows.
    let path = evaluate(&scenario, "path").await;
    let presentation = presentation(&path).expect("presented");
    assert_eq!(
        (presentation.shape, presentation.summary.as_ref()),
        (PresentedShape::Value, "a vector")
    );
    let ValueChildren::Available(reference) = &presentation.children else {
        panic!("{presentation:?}");
    };

    let page = scenario
        .operation(
            "children",
            scenario.handle().value_children(
                Arc::clone(reference),
                ValueChildQuery {
                    offset: 0,
                    limit: 8,
                },
            ),
        )
        .await;

    let names = page
        .children
        .iter()
        .map(|child| match &child.relationship {
            ValueChildRelationship::Field { name } => name.to_string(),
            ValueChildRelationship::Raw => "[raw]".to_owned(),
            other => format!("{other:?}"),
        })
        .collect::<Vec<_>>();

    // The vector's own presentation, `empty`, has no elements, and its
    // `[raw]` is its own: the path's children are its field and `[raw]`.
    assert_eq!(names, ["length", "[raw]"]);

    let length = &page.children[0];
    assert!(
        matches!(
            length.state,
            VariableState::Available {
                value: microscope::VariableValue::Scalar(ScalarValue::Unsigned(15)),
                ..
            }
        ),
        "{length:?}"
    );

    // Restoring the built-in views presents the vector again.
    scenario
        .operation("unload views", scenario.handle().load_views(&[], &[], &[]))
        .await;

    assert_eq!(
        summary(&evaluate(&scenario, "ints").await),
        "len=3 [1, 2, 3]"
    );
    scenario.shutdown().await;
}

/// Views a module carries in its `.debug_microscope_views` section present
/// that module's own types, and never another module's type of the same
/// name.
#[tokio::test]
async fn embedded_views_present_only_their_own_modules_types() {
    let mut scenario = Scenario::launch("embedded-views");
    scenario.add_breakpoint("barrier").await;
    scenario.run_to_stop().await;
    let trace = scenario
        .operation("backtrace", scenario.handle().backtrace())
        .await;
    scenario
        .operation(
            "select caller",
            scenario.handle().select_frame(trace.frames[1].id),
        )
        .await;

    let numbers = evaluate(&scenario, "numbers").await;
    let presented = presentation(&numbers).expect("the program's view presents it");
    assert_eq!(
        (presented.summary.as_ref(), presented.view.source.as_ref()),
        ("len=3 [1, 2, 3]", "embedded-views.views[0]")
    );

    let origin = evaluate(&scenario, "library_origin").await;
    let presented = presentation(&origin).expect("the library's view presents it");
    assert_eq!(
        (presented.summary.as_ref(), presented.view.source.as_ref()),
        ("{x: 0x3, y: 4}", "libembedded-views.so.views[0]")
    );

    // The program's own point has the library's point's name, and no view.
    let here = evaluate(&scenario, "here").await;
    assert!(presentation(&here).is_none(), "{:?}", here.state);
    scenario.shutdown().await;
}

/// The Rust SDK's macros embed a program's views, and a kernel one calls,
/// as the C header does; the kernel is written with the SDK.
#[tokio::test]
async fn the_rust_sdk_embeds_a_programs_views_and_kernels() {
    let mut scenario = Scenario::launch("embedded-views-rust");
    scenario.add_breakpoint("barrier").await;
    scenario.run_to_stop().await;
    let trace = scenario
        .operation("backtrace", scenario.handle().backtrace())
        .await;
    scenario
        .operation(
            "select caller",
            scenario.handle().select_frame(trace.frames[1].id),
        )
        .await;

    for (expression, expected) in [
        ("tags", r#"len=2 ["red", "green"]"#),
        ("temperature", "21.5°C"),
        ("family", "len=5 [1, 2, 3, 4, 5]"),
        ("nobody", "len=0 []"),
    ] {
        let value = evaluate(&scenario, expression).await;
        let presented = presentation(&value).expect("the program's views present it");
        assert_eq!(
            (presented.summary.as_ref(), presented.view.source.as_ref()),
            (expected, "embedded-views-rust.views[0]"),
            "{expression}"
        );
    }

    scenario.shutdown().await;
}

/// The program `docs/writing-views.md` writes views for presents as its
/// markers say, with the views it carries.
#[tokio::test]
async fn the_tutorials_program_presents_as_its_markers_say() {
    check_containers("tutorial", "c/tutorial/tutorial.c", "barrier", false).await;
}

/// Every block of `docs/writing-views.md` that names a file quotes that
/// file as it is, so the tutorial and the fixtures it is built from never
/// drift apart.
#[test]
fn the_tutorial_quotes_its_fixtures_as_they_are() {
    let root = env!("CARGO_MANIFEST_DIR");
    let tutorial = fs::read_to_string(format!("{root}/docs/writing-views.md"))
        .expect("read docs/writing-views.md");
    let mut quoted = 0;
    let mut lines = tutorial.lines();

    while let Some(line) = lines.next() {
        let Some(path) = line
            .strip_prefix("```")
            .and_then(|info| info.split_once(' '))
            .map(|(_, path)| path.trim())
        else {
            continue;
        };

        let block = lines
            .by_ref()
            .take_while(|line| *line != "```")
            .collect::<Vec<_>>()
            .join("\n");

        let file = fs::read_to_string(format!("{root}/{path}"))
            .unwrap_or_else(|error| panic!("{path}: {error}"));
        assert!(
            file.contains(&block),
            "docs/writing-views.md quotes {path} as it is not:\n{block}"
        );
        quoted += 1;
    }

    assert!(quoted >= 5, "the tutorial quotes its fixtures");
}
