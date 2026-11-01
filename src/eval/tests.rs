//! Runs every example in the language's reference, `docs/expressions.md`,
//! so that the reference and the implementation cannot drift apart.

use proptest::prelude::*;

use super::bind::{Mode, bind};
use super::error::ExpressionError;
use super::fake::world;
use super::interp::{Assigned, Failure, Outcome, run};
use super::number::{Exact, Float};
use super::syntax::Expression;
use crate::{FloatValue, IntegerValue, ScalarValue, VariableState, VariableValue};

const REFERENCE: &str = include_str!("../../docs/expressions.md");

/// One `expression => outcome` row of an example block.
struct Row<'text> {
    line: usize,
    world: Option<&'text str>,
    expression: &'text str,
    outcome: &'text str,
}

fn rows() -> Vec<Row<'static>> {
    let mut rows = Vec::new();
    let mut in_block = false;
    let mut world = None;
    for (index, line) in REFERENCE.lines().enumerate() {
        match line.trim() {
            "```microscope-example" => {
                in_block = true;
                world = None;
            }
            "```" => in_block = false,
            "" => {}
            row if in_block => {
                if let Some(name) = row.strip_prefix("world:") {
                    world = Some(name.trim());
                    continue;
                }

                let (expression, outcome) = row
                    .split_once(" => ")
                    .unwrap_or_else(|| panic!("line {}: `{row}` has no ` => `", index + 1));
                rows.push(Row {
                    line: index + 1,
                    world,
                    expression: expression.trim(),
                    outcome: outcome.trim(),
                });
            }
            _ => {}
        }
    }

    rows
}

/// The text of a Markdown code span, in single or double backticks.
fn code(span: &str) -> Option<&str> {
    span.strip_prefix("`` ")
        .and_then(|inner| inner.strip_suffix(" ``"))
        .or_else(|| span.strip_prefix('`')?.strip_suffix('`'))
}

/// A value as the reference writes it.
fn render(value: &VariableValue) -> String {
    match value {
        VariableValue::Scalar(ScalarValue::Signed(value)) => value.to_string(),
        VariableValue::Scalar(ScalarValue::Unsigned(value)) => value.to_string(),
        VariableValue::Scalar(ScalarValue::Boolean(value)) => value.to_string(),
        VariableValue::Scalar(ScalarValue::Floating(value)) => match value {
            FloatValue::Binary32(bits) => format!("{:?}", f32::from_bits(*bits)),
            FloatValue::Binary64(bits) => format!("{:?}", f64::from_bits(*bits)),
            other => Float::from_value(*other).to_string(),
        },
        VariableValue::Enumeration { value, matches } => {
            if matches.is_empty() {
                match value {
                    IntegerValue::Signed(value) => value.to_string(),
                    IntegerValue::Unsigned(value) => value.to_string(),
                }
            } else {
                matches
                    .iter()
                    .map(|enumerator| enumerator.name.as_ref())
                    .collect::<Vec<_>>()
                    .join(" | ")
            }
        }
        VariableValue::Address(address) => format!("{:#x}", address.address.get()),
        VariableValue::Array { .. } => "[…]".to_owned(),
        _ => "{…}".to_owned(),
    }
}

/// What evaluating a row's expression in its world gives, as the reference
/// writes it.
fn evaluate(world_name: &str, text: &str) -> String {
    let (mode, text) = text
        .strip_prefix("assign:")
        .map_or((Mode::Read, text), |rest| (Mode::Assign, rest.trim()));
    let mut world = world(world_name);
    let failure = |error: &ExpressionError| {
        format!("error {} at `{}`", error.kind.name(), error.span.text(text))
    };

    let expression = match Expression::parse(text) {
        Ok(expression) => expression,
        Err(error) => return failure(&error),
    };

    let program = match bind(&expression, &world, mode) {
        Ok(program) => program,
        Err(error) => return failure(&error),
    };

    let outcome = match run(&program, &mut world) {
        Ok(outcome @ (Outcome::Assign { .. } | Outcome::AssignRegister { .. })) => {
            // The value is the target read again.
            match outcome {
                Outcome::Assign { target, bytes, .. } => world.write(&target, &bytes),
                Outcome::AssignRegister {
                    register, bytes, ..
                } => world.write_register(&register, &bytes),
                _ => unreachable!("matched above"),
            }

            let target = text.split_once('=').map_or(text, |(target, _)| {
                target.trim_end_matches(['+', '-', '*', '/', '%', '&', '|', '^', '<', '>'])
            });
            let expression = Expression::parse(target.trim()).expect("the target parses");
            let program = bind(&expression, &world, Mode::Read).expect("the target binds");
            run(&program, &mut world)
        }
        outcome => outcome,
    };

    match outcome {
        Ok(Outcome::Value { value, cause }) => {
            let name = value
                .type_info
                .as_ref()
                .map_or("?", |info| info.name.as_ref())
                .to_owned();
            match (&value.state, cause) {
                // Text shows as text, and a pointer to it as its address.
                (
                    VariableState::Available {
                        value,
                        text: Some(text),
                        ..
                    },
                    _,
                ) if !matches!(value, VariableValue::Address(_)) => {
                    format!("{} : {name}", crate::quoted_text(text))
                }
                // A value a presentation summarizes shows as its summary,
                // as a Python object's does.
                (
                    VariableState::Available {
                        presentation: Some(presentation),
                        ..
                    },
                    _,
                ) => format!("{} : {name}", presentation.summary),
                (VariableState::Available { value, .. }, _) => {
                    format!("{} : {name}", render(value))
                }
                (_, Some(cause)) => format!("unavailable at `{}`", cause.text(text)),
                (state, None) => format!("{state:?} without a cause"),
            }
        }
        Ok(Outcome::Range { start, end, .. }) => format!("range {start}..{end}"),
        // The runtime stores what a dynamic variable is given.
        Ok(Outcome::AssignDynamic { value, .. }) => match value {
            Assigned::Integer(value) => format!("gives {value}"),
            Assigned::Float(value) => format!("gives {value:?}"),
            Assigned::Bool(value) => format!("gives {value}"),
            Assigned::Text(text) => format!("gives {text:?}"),
            Assigned::Nothing => "gives None".to_owned(),
            Assigned::Undefined => "gives undefined".to_owned(),
            Assigned::BigInt(value) => format!("gives {value}n"),
            Assigned::Object(_) => "gives an object".to_owned(),
        },
        Ok(Outcome::Assign { .. } | Outcome::AssignRegister { .. }) => {
            "an assignment the world did not make".to_owned()
        }
        Err(Failure::Expression(error)) => failure(&error),
        Err(Failure::Debugger(error)) => format!("debugger failure {error}"),
    }
}

#[test]
fn every_example_in_the_reference_holds() {
    let rows = rows();
    assert!(rows.len() > 50, "the reference's examples were found");
    let mut failures = Vec::new();
    for row in rows {
        let context = format!("docs/expressions.md:{}: `{}`", row.line, row.expression);
        if let Some(normal) = row.outcome.strip_prefix("reads as ") {
            let normal = code(normal).unwrap_or_else(|| panic!("{context}: malformed outcome"));
            match Expression::parse(row.expression) {
                Ok(expression) if expression.to_string() == normal => {}
                Ok(expression) => failures.push(format!("{context}: reads as `{expression}`")),
                Err(error) => failures.push(format!("{context}: {error}")),
            }

            continue;
        }

        let parsed = || match Expression::parse(row.expression) {
            Ok(_) => "parsed".to_owned(),
            Err(error) => format!(
                "error {} at `{}`",
                error.kind.name(),
                error.span.text(row.expression)
            ),
        };

        let actual = row
            .world
            .map_or_else(parsed, |world| evaluate(world, row.expression));
        let expected = match row.outcome.split_once(" at ") {
            Some((kind, pointed)) => {
                let pointed = code(pointed).unwrap_or_else(|| panic!("{context}: malformed span"));
                format!("{kind} at `{pointed}`")
            }
            None => row.outcome.to_owned(),
        };

        if actual != expected {
            failures.push(format!(
                "{context}\n    expected {expected}\n    actual   {actual}"
            ));
        }
    }

    assert!(
        failures.is_empty(),
        "{} rows disagree:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

/// Evaluates `text` in `world`, returning the outcome as the reference
/// writes it.
fn outcome(world: &mut super::fake::World, text: &str) -> String {
    let expression = Expression::parse(text).expect("parse");
    let program = bind(&expression, world, Mode::Read).expect("bind");
    match run(&program, world) {
        Ok(Outcome::Value { value, cause }) => match (&value.state, cause) {
            (VariableState::Available { value: decoded, .. }, _) => format!(
                "{} : {}",
                render(decoded),
                value
                    .type_info
                    .map_or_else(String::new, |info| info.name.to_string())
            ),
            (state, cause) => format!("{state:?} at {cause:?}"),
        },
        Ok(Outcome::Range { start, end, .. }) => format!("range {start}..{end}"),
        Ok(
            Outcome::Assign { .. } | Outcome::AssignRegister { .. } | Outcome::AssignDynamic { .. },
        ) => "an assignment".to_owned(),
        Err(Failure::Expression(error)) => format!("error {error}"),
        Err(Failure::Debugger(error)) => format!("failure {error}"),
    }
}

#[test]
fn short_circuits_read_nothing_they_skip() {
    let mut world = super::fake::memory();
    let s = world.address_of("s");
    world.poison(s, 16);
    for (text, expected) in [
        ("false && s.a > 0", "false : bool"),
        ("1 || s.b == 7", "true : bool"),
        ("0 ? s.a : 2", "2 : integer"),
        ("null_ptr != null && null_ptr->a > 0", "false : bool"),
        ("sizeof(s.b) + sizeof(s)", "24 : integer"),
        ("len(arr) + len(m)", "6 : integer"),
    ] {
        assert_eq!(outcome(&mut world, text), expected, "`{text}`");
    }
}

#[test]
fn evaluation_reads_exactly_the_bytes_it_needs() {
    let mut world = super::fake::memory();
    let s = world.address_of("s");
    let ptr = world.address_of("ptr");
    for (text, reads) in [
        ("s.b", vec![(s + 8, 8)]),
        ("s.a + 1", vec![(s, 4)]),
        ("ptr->b", vec![(ptr, 8), (s + 8, 8)]),
        ("&s.b", vec![]),
        ("*&s.a", vec![(s, 4)]),
    ] {
        world.reads.clear();
        outcome(&mut world, text);
        assert_eq!(world.reads, reads, "`{text}`");
    }
}

#[test]
fn a_program_bound_once_runs_on_other_data_as_one_bound_there() {
    let texts = [
        "uc + 10",
        "(u8)(uc + 10)",
        "~uc",
        "sc >> 1",
        "i32v * 2 > u32v",
        "f * 2",
        "uc > 3 ? uc : 0",
    ];
    let original = super::fake::scalars();
    let mut changed = super::fake::scalars();
    changed.set("uc", &[3]);
    changed.set("sc", &(-100_i8).to_le_bytes());
    changed.set("i32v", &i32::MAX.to_le_bytes());
    changed.set("f", &(-0.25_f32).to_le_bytes());
    for text in texts {
        let expression = Expression::parse(text).expect("parse");
        let program = bind(&expression, &original, Mode::Read).expect("bind");
        let reused = format!(
            "{:?}",
            run(&program, &mut changed).map(|outcome| format!("{outcome:?}"))
        );
        let fresh = bind(&expression, &changed, Mode::Read).expect("bind");
        let rebound = format!(
            "{:?}",
            run(&fresh, &mut changed).map(|outcome| format!("{outcome:?}"))
        );
        assert_eq!(reused, rebound, "`{text}`");
    }
}

#[test]
fn a_value_of_a_dynamic_type_finds_its_attributes_and_items_as_it_runs() {
    for (text, expected) in [
        ("point.x", "3 : int"),
        ("point.x * point.y", "12 : int"),
        ("point.z", "error missing-key at `point.z`"),
        ("count.real", "error unsupported at `count.real`"),
        ("point->x", "error type at `point`"),
        ("point.0", "error type at `0`"),
        ("(count + 1).real", "error type at `(count + 1)`"),
        ("numbers[0][1]", "error type at `numbers[0][1]`"),
        ("&count", "0x7f0000c0 : void*"),
        ("sizeof(count)", "error type at `sizeof(count)`"),
        ("(bool)empty", "false : bool"),
        ("(bool)numbers", "true : bool"),
        ("count > 5 ? numbers : pair", "[10, 20, 30] : list"),
        ("~count", "-8 : int"),
        ("-ratio", "-0.5 : float"),
        ("ratio % 1", "0.5 : float"),
        ("count % -2", "-1 : int"),
        ("-count % 3", "2 : int"),
        ("-ratio % -2", "-0.5 : float"),
        ("count / 2", "3.5 : float"),
        ("count / 0", "error arithmetic at `count / 0`"),
        ("count % 0", "error arithmetic at `count % 0`"),
        ("count << -1", "error arithmetic at `count << -1`"),
        ("True & False", "False : bool"),
        ("True + True", "2 : int"),
        ("count / 2 / 0", "error arithmetic at `count / 2 / 0`"),
        ("count / 2 > 3", "true : bool"),
        ("count + 1n", "error unsupported at `count + 1n`"),
        ("count << 2", "28 : int"),
        ("ratio & 1", "error type at `ratio & 1`"),
        ("name + 1", "error type at `name + 1`"),
        ("name + \"x\"", "error type at `\"x\"`"),
        ("nothing < 1", "error type at `nothing < 1`"),
        ("name < \"bob\"", "true : bool"),
        ("data < data", "false : bool"),
        ("ages[nothing]", "error missing-key at `ages[nothing]`"),
        ("ages[numbers]", "error unsupported at `ages[numbers]`"),
    ] {
        assert_eq!(evaluate("python", text), expected, "`{text}`");
    }
}

#[test]
fn a_program_bound_once_runs_on_whatever_types_its_dynamic_values_have() {
    use super::fake::Dynamic;
    let texts = ["count + 1", "count > 2", "!count", "count == None"];
    let original = super::fake::python();
    let mut changed = super::fake::python();
    changed.replace_dynamic("count", Dynamic::Float(1.5));
    let mut emptied = super::fake::python();
    emptied.replace_dynamic("count", Dynamic::Nothing);
    for (world, expected) in [
        (
            &mut changed,
            [
                "2.5 : float",
                "false : bool",
                "false : bool",
                "false : bool",
            ],
        ),
        (
            &mut emptied,
            [
                "error type at `count + 1`",
                "error type at `count > 2`",
                "true : bool",
                "true : bool",
            ],
        ),
    ] {
        for (text, expected) in texts.into_iter().zip(expected) {
            let expression = Expression::parse(text).expect("parse");
            let program = bind(&expression, &original, Mode::Read).expect("bind");
            let actual = match run(&program, world) {
                Ok(Outcome::Value { value, .. }) => match &value.state {
                    VariableState::Available { value: decoded, .. } => format!(
                        "{} : {}",
                        render(decoded),
                        value
                            .type_info
                            .map_or_else(String::new, |info| info.name.to_string())
                    ),
                    state => format!("{state:?}"),
                },
                Err(Failure::Expression(error)) => {
                    format!("error {} at `{}`", error.kind.name(), error.span.text(text))
                }
                other => format!("{other:?}"),
            };
            assert_eq!(actual, expected, "`{text}`");
        }
    }
}

#[test]
fn too_little_work_ends_in_an_unavailable_value_never_a_wrong_one() {
    for text in [
        "s.a + ptr->b * 2",
        "arr[1] + m[1][2]",
        "name == \"hello\" && color == BLUE",
    ] {
        let mut world = super::fake::memory();
        world.work = Some(1_000);
        let full = outcome(&mut world, text);
        let used = 1_000 - world.work.expect("limited");
        for budget in 0..used {
            let mut world = super::fake::memory();
            world.work = Some(budget);
            let limited = outcome(&mut world, text);
            assert!(
                limited.contains("ExpressionWork"),
                "`{text}` with {budget} of {used} units gave `{limited}`, not `{full}` or a limit"
            );
        }
    }
}

/// Expressions from the language's grammar over the memory world's names.
fn world_expression() -> impl Strategy<Value = String> {
    let leaf = prop_oneof![
        Just("s"),
        Just("s.a"),
        Just("s.b"),
        Just("ptr"),
        Just("null_ptr"),
        Just("arr"),
        Just("ip"),
        Just("m"),
        Just("name"),
        Just("buf"),
        Just("color"),
        Just("sign"),
        Just("vp"),
        Just("items"),
        Just("count"),
        Just("limit"),
        Just("first"),
        Just("r"),
        Just("gone"),
        Just("$rip"),
        Just("$rbp"),
        Just("$task"),
        Just("squares"),
        Just("ages"),
        Just("spare"),
        Just("RED"),
        Just("BLUE"),
        Just("1"),
        Just("0"),
        Just("-1"),
        Just("255u8"),
        Just("2.5"),
        Just("true"),
        Just("null"),
        Just("\"hello\""),
        Just("'a'"),
    ]
    .prop_map(str::to_owned);
    leaf.prop_recursive(5, 32, 3, |inner| {
        let binary = prop_oneof![
            Just("+"),
            Just("-"),
            Just("*"),
            Just("/"),
            Just("%"),
            Just("<<"),
            Just(">>"),
            Just("&"),
            Just("|"),
            Just("^"),
            Just("&&"),
            Just("||"),
            Just("=="),
            Just("!="),
            Just("<"),
            Just(">="),
        ];
        let prefix = prop_oneof![Just("-"), Just("!"), Just("~"), Just("*"), Just("&")];
        let ty = prop_oneof![
            Just("u8"),
            Just("i64"),
            Just("int"),
            Just("S*"),
            Just("Color"),
            Just("f32"),
            Just("bool")
        ];
        prop_oneof![
            (inner.clone(), binary, inner.clone())
                .prop_map(|(l, op, r)| format!("({l}) {op} ({r})")),
            (prefix, inner.clone()).prop_map(|(op, operand)| format!("{op}({operand})")),
            (ty, inner.clone()).prop_map(|(ty, operand)| format!("({ty})({operand})")),
            (inner.clone(), inner.clone()).prop_map(|(base, index)| format!("({base})[{index}]")),
            inner.clone().prop_map(|base| format!("({base}).a")),
            inner.clone().prop_map(|base| format!("({base})->b")),
            (inner.clone(), inner.clone(), inner.clone())
                .prop_map(|(c, t, e)| format!("({c}) ? ({t}) : ({e})")),
            inner.clone().prop_map(|operand| format!("len({operand})")),
            inner.clone().prop_map(|operand| format!("cap({operand})")),
            (inner.clone(), inner.clone(), inner.clone())
                .prop_map(|(base, start, end)| format!("({base})[{start}:{end}]")),
            inner.prop_map(|operand| format!("sizeof({operand})")),
        ]
    })
}

/// Whatever an expression is, evaluating it never panics, reads only
/// mapped memory it may, runs the same twice, and presents the type the
/// binder computed. Most generated expressions mix types the binder
/// refuses, so the cases that bind are counted and must not fall below a
/// floor: about 10% bind.
#[test]
fn generated_expressions_evaluate_consistently() {
    use proptest::test_runner::{Config, TestRunner};

    let config = Config {
        source_file: Some(file!()),
        ..Config::default()
    };
    let mut runner = TestRunner::new(config);
    let bound = std::cell::Cell::new(0_u32);

    let result = runner.run(&world_expression(), |text| {
        let expression = Expression::parse(&text);
        proptest::prop_assert!(expression.is_ok(), "`{}` does not parse", text);
        let expression = expression.expect("checked");

        let mut world = super::fake::memory();
        let Ok(program) = bind(&expression, &world, Mode::Read) else {
            return Ok(());
        };

        bound.set(bound.get() + 1);
        let expected = super::types::type_info(&world, program.result()).name;
        let first = run(&program, &mut world);
        let second = run(&program, &mut world);
        proptest::prop_assert_eq!(format!("{first:?}"), format!("{second:?}"));
        if let Ok(Outcome::Value { value, .. }) = first {
            let presented = value.type_info.map(|info| info.name);
            proptest::prop_assert_eq!(presented.as_ref(), Some(&expected), "`{}`", text);
        }

        Ok(())
    });

    if let Err(failure) = result {
        panic!("{failure}");
    }

    let cases = runner.config().cases;
    assert!(
        bound.get() * 30 >= cases,
        "only {} of {cases} expressions bound",
        bound.get()
    );
}

/// What an expression of exact integers is: an integer or a truth value.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Model {
    Int(Exact),
    Bool(bool),
}

/// A generated expression and what the reference says of it.
#[derive(Debug, Clone)]
struct Part {
    text: String,

    /// Its value, or an arithmetic error at the text of the part that
    /// failed, in the order evaluation reaches them.
    value: Result<Model, String>,

    /// A literal, or `-` of one: the binder folds those into one literal,
    /// as it does `-129i8`.
    literal: bool,

    /// The first folded literal out of range, which the binder refuses
    /// before anything runs, in a branch evaluation would skip too.
    refused: Option<String>,
}

impl Part {
    /// The part as an operand in parentheses, which its span then
    /// includes: an error at the part itself is at the parenthesized text.
    fn grouped(self) -> Self {
        let text = self.text.clone();
        let wrap = move |failed: String| {
            if failed == text {
                format!("({failed})")
            } else {
                failed
            }
        };

        Self {
            value: self.value.map_err(&wrap),
            refused: self.refused.map(&wrap),
            ..self
        }
    }

    fn int(&self) -> Result<Exact, String> {
        match &self.value {
            Ok(Model::Int(value)) => Ok(*value),
            Ok(Model::Bool(_)) => unreachable!("integers only"),
            Err(failed) => Err(failed.clone()),
        }
    }

    /// The part combining operands, refused at the first operand that is.
    fn of(text: String, value: Result<Model, String>, operands: &[&Self]) -> Self {
        let refused = operands.iter().find_map(|operand| operand.refused.clone());
        Self {
            text,
            value,
            literal: false,
            refused,
        }
    }
}

fn truth(model: &Model) -> bool {
    match model {
        Model::Int(value) => !value.is_zero(),
        Model::Bool(value) => *value,
    }
}

/// An exact literal as the language may write it.
fn literal() -> impl Strategy<Value = Part> {
    let value = prop_oneof![
        3 => 0_u128..300,
        2 => (0_u32..128, -1_i32..=1)
            .prop_map(|(shift, offset)| (1_u128 << shift).saturating_add_signed(offset.into())),
        1 => prop::sample::select(vec![u128::MAX, 1 << 127, i128::MAX.cast_unsigned()]),
        1 => any::<u128>(),
    ];

    (value, 0_usize..4).prop_map(|(value, spelling)| {
        let text = match spelling {
            0 => format!("{value:#x}"),
            1 => format!("{value:#b}"),
            2 => format!("{value:#o}").replacen("0o", "0o_", 1),
            _ => value.to_string(),
        };

        Part {
            text,
            value: Ok(Model::Int(Exact::from(value))),
            literal: true,
            refused: None,
        }
    })
}

/// `+ - * / % & | ^ << >>` of two operands, and `-` or `~` of one.
fn integer_parts(inner: BoxedStrategy<Part>) -> impl Strategy<Value = Part> {
    use super::number::BitOperator;

    let integer = |value: Result<Exact, super::number::NumberError>, text: &str| {
        value.map(Model::Int).map_err(|_| text.to_owned())
    };

    let arithmetic = (
        inner.clone(),
        prop::sample::select(vec!["+", "-", "*", "/", "%", "&", "|", "^", "<<", ">>"]),
        inner.clone(),
    )
        .prop_map(move |(left, op, right)| {
            let text = format!("({}) {op} ({})", left.text, right.text);
            let value = left.int().and_then(|a| {
                let b = right.int()?;
                integer(
                    match op {
                        "+" => a.add(b),
                        "-" => a.sub(b),
                        "*" => a.mul(b),
                        "/" => a.div(b),
                        "%" => a.rem(b),
                        "&" => a.bitwise(BitOperator::And, b),
                        "|" => a.bitwise(BitOperator::Or, b),
                        "^" => a.bitwise(BitOperator::Xor, b),
                        "<<" => a.shl(b),
                        _ => a.shr(b),
                    },
                    &text,
                )
            });
            Part::of(text, value, &[&left, &right])
        });

    let prefix = (prop::sample::select(vec!["-", "~"]), inner).prop_map(move |(op, operand)| {
        let text = format!("{op}({})", operand.text);
        let value = operand
            .int()
            .and_then(|value| integer(if op == "-" { value.neg() } else { value.not() }, &text));

        let folded = op == "-" && operand.literal;
        let mut part = Part::of(text.clone(), value.clone(), &[&operand]);
        part.literal = folded;
        if folded && part.refused.is_none() && value.is_err() {
            part.refused = Some(text);
        }

        part
    });

    prop_oneof![2 => arithmetic, 1 => prefix]
}

/// `?:` over a comparison, `&&`, `||`, `!`, or an integer's truth.
fn conditional_parts(inner: BoxedStrategy<Part>) -> impl Strategy<Value = Part> {
    let comparison = (
        inner.clone(),
        prop::sample::select(vec!["==", "!=", "<", "<=", ">", ">="]),
        inner.clone(),
    )
        .prop_map(move |(left, op, right)| {
            let text = format!("({}) {op} ({})", left.text, right.text);
            let value = left.int().and_then(|a| {
                let b = right.int()?;
                Ok(Model::Bool(match op {
                    "==" => a == b,
                    "!=" => a != b,
                    "<" => a < b,
                    "<=" => a <= b,
                    ">" => a > b,
                    _ => a >= b,
                }))
            });
            Part::of(text, value, &[&left, &right])
        });

    let truth_of = prop_oneof![comparison.prop_map(Part::grouped), inner.clone()];
    let logic = (
        truth_of.clone(),
        prop::sample::select(vec!["&&", "||"]),
        truth_of.clone(),
    )
        .prop_map(|(left, op, right)| {
            let text = format!("({}) {op} ({})", left.text, right.text);
            let value = left
                .value
                .clone()
                .and_then(|value| match (op, truth(&value)) {
                    ("&&", false) => Ok(Model::Bool(false)),
                    ("||", true) => Ok(Model::Bool(true)),
                    _ => right.value.clone().map(|right| Model::Bool(truth(&right))),
                });
            Part::of(text, value, &[&left, &right])
        });

    let not = truth_of.clone().prop_map(|operand| {
        let text = format!("!({})", operand.text);
        let value = operand
            .value
            .clone()
            .map(|value| Model::Bool(!truth(&value)));
        Part::of(text, value, &[&operand])
    });

    (
        prop_oneof![
            truth_of,
            logic.prop_map(Part::grouped),
            not.prop_map(Part::grouped)
        ],
        inner.clone(),
        inner,
    )
        .prop_map(|(condition, then, otherwise)| {
            let text = format!(
                "({}) ? ({}) : ({})",
                condition.text, then.text, otherwise.text
            );
            let value = condition.value.clone().and_then(|condition| {
                if truth(&condition) {
                    then.value.clone()
                } else {
                    otherwise.value.clone()
                }
            });
            Part::of(text, value, &[&condition, &then, &otherwise])
        })
}

/// Expressions of exact integer literals, with comparisons, truth
/// operators, and conditionals, each with the value the language's
/// reference gives it: exact arithmetic within -2^127..2^128, infinite
/// two's complement for bits, and `&&`, `||`, and `?:` evaluating only
/// what they need.
fn literal_expression() -> impl Strategy<Value = Part> {
    literal()
        .prop_recursive(4, 24, 3, |inner| {
            // Every operand below is written in parentheses.
            let inner = inner.prop_map(Part::grouped).boxed();
            prop_oneof![
                6 => integer_parts(inner.clone()),
                1 => conditional_parts(inner),
            ]
        })
        .prop_flat_map(|whole| {
            // A whole expression may also be a truth value.
            let comparison = (
                Just(whole.clone().grouped()),
                prop::sample::select(vec!["==", "<"]),
                literal(),
            )
                .prop_map(|(left, op, right)| {
                    let text = format!("({}) {op} ({})", left.text, right.text);
                    let value = left.int().and_then(|a| {
                        let b = right.int()?;
                        Ok(Model::Bool(if op == "==" { a == b } else { a < b }))
                    });
                    Part::of(text, value, &[&left, &right])
                });
            prop_oneof![3 => Just(whole), 1 => comparison]
        })
}

proptest::proptest! {
    // A negative operand of a bitwise operator with one above 2^127 is
    // rare, and took a bug to find; 1024 cases cost about 0.15 s.
    #![proptest_config(proptest::prelude::ProptestConfig::with_cases(1024))]

    /// Literal arithmetic, comparisons, and truth operators evaluate to
    /// the model's value, or to an arithmetic error at the part the model
    /// says failed.
    #[test]
    fn literal_expressions_evaluate_to_their_value(part in literal_expression()) {
        let expected = match (part.refused, part.value) {
            (Some(failed), _) | (None, Err(failed)) => format!("error arithmetic at `{failed}`"),
            (None, Ok(Model::Int(value))) => format!("{value} : integer"),
            (None, Ok(Model::Bool(value))) => format!("{value} : bool"),
        };

        proptest::prop_assert_eq!(evaluate("scalars", &part.text), expected, "`{}`", part.text);
    }
}

/// The evaluator is pure: it reaches a program only through the traits the
/// debugger implements for it, so none of its code may reach for process
/// control, debug information, I/O, clocks, or threads. It is one language
/// for every source language, so it tests for none: what a language's
/// types mean reaches it as capabilities of those traits. Tests are exempt.
#[test]
fn the_evaluator_stays_pure() {
    const FORBIDDEN: [&str; 14] = [
        "SourceLanguage::",
        "GoKind",
        "GoTypeAttributes",
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
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/eval");
    let mut pending = vec![root];
    let mut checked = 0;
    while let Some(path) = pending.pop() {
        if path.is_dir() {
            let entries = std::fs::read_dir(&path).expect("read the evaluator's sources");
            pending.extend(entries.map(|entry| entry.expect("a directory entry").path()));
            continue;
        }

        let test_file = path.file_name().is_some_and(|name| name == "tests.rs");
        if test_file || path.extension().is_none_or(|extension| extension != "rs") {
            continue;
        }

        let source = std::fs::read_to_string(&path).expect("read a source file");

        // Code compiled only for tests or the fuzzer may use anything.
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

    assert!(checked >= 6, "the evaluator's sources were found");
}

/// Found by `literal_expressions_evaluate_to_their_value`: a bitwise result
/// of two exact integers beyond the range blamed its left operand, which
/// is a fine value, rather than the operation.
#[test]
fn bitwise_results_out_of_range_point_at_the_operation() {
    let text = "(-(0x1)) ^ ((0o_103) | (0xb07d992beac56249596f30113b3d4011))";
    assert_eq!(
        evaluate("scalars", text),
        format!("error arithmetic at `{text}`")
    );

    let text = "0xb07d992beac56249596f30113b3d4011 ^ -1";
    assert_eq!(
        evaluate("scalars", text),
        format!("error arithmetic at `{text}`")
    );

    // An exact operand too wide for a typed one is still the culprit.
    assert_eq!(
        evaluate("scalars", "uc | 0x100"),
        "error arithmetic at `0x100`"
    );
}
