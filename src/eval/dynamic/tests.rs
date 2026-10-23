//! Each language's rules against the answers CPython 3.12 to 3.15 and
//! Node 26 gave for the same operations.

use super::{binary, compare, key, shown, truth, unary};
use crate::eval::error::ErrorKind;
use crate::eval::number::{Exact, Float};
use crate::eval::syntax::ast::{BinaryOp, UnaryOp};
use crate::eval::target::{Key, Loaded, LoadedValue, Rules, Stop};
use crate::{TextCompletion, TextSummary};

fn of(language: Option<Rules>, value: LoadedValue) -> Loaded {
    Loaded {
        type_name: "value".into(),
        language,
        value,
    }
}

fn py_int(value: i128) -> Loaded {
    of(Some(Rules::Python), LoadedValue::Int(Exact::from(value)))
}

fn py_float(value: f64) -> Loaded {
    of(
        Some(Rules::Python),
        LoadedValue::Float(Float::from_f64(value)),
    )
}

fn py_bool(value: bool) -> Loaded {
    of(Some(Rules::Python), LoadedValue::Bool(value))
}

fn js_number(value: f64) -> Loaded {
    of(
        Some(Rules::JavaScript),
        LoadedValue::Float(Float::from_f64(value)),
    )
}

fn js_bigint(value: i128) -> Loaded {
    of(
        Some(Rules::JavaScript),
        LoadedValue::Int(Exact::from(value)),
    )
}

fn js(value: LoadedValue) -> Loaded {
    of(Some(Rules::JavaScript), value)
}

/// An integer the expression writes, which has no language of its own.
fn literal(value: i128) -> Loaded {
    of(None, LoadedValue::Int(Exact::from(value)))
}

fn text(language: Rules, text: &str) -> Loaded {
    of(
        Some(language),
        LoadedValue::Text(TextSummary {
            bytes: text.as_bytes().into(),
            completion: TextCompletion::Complete,
        }),
    )
}

fn kind(stop: Stop) -> ErrorKind {
    match stop {
        Stop::Refused(refusal) => refusal.kind,
        other => panic!("not refused: {other:?}"),
    }
}

fn int(result: Result<Loaded, Stop>) -> i128 {
    match result.expect("a value").value {
        LoadedValue::Int(value) => value.to_i128().expect("in range"),
        other => panic!("not an integer: {other:?}"),
    }
}

fn float(result: Result<Loaded, Stop>) -> f64 {
    match result.expect("a value").value {
        LoadedValue::Float(value) => value.to_f64(),
        other => panic!("not a float: {other:?}"),
    }
}

/// The same double, NaN and the sign of zero included.
fn same(actual: f64, expected: f64) -> bool {
    actual.to_bits() == expected.to_bits() || (actual.is_nan() && expected.is_nan())
}

#[test]
fn python_floors_remainders_toward_the_divisors_sign() {
    for (left, right, expected) in [(-7, 3, 2), (7, -3, -2), (-7, -3, -1), (7, 3, 1)] {
        let actual = int(binary(BinaryOp::Rem, &py_int(left), &literal(right)));
        assert_eq!(actual, expected, "{left} % {right}");
    }

    for (left, right, expected) in [
        (-7.5, 2.0, 0.5),
        (7.5, -2.0, -0.5),
        (0.0, -2.0, -0.0),
        (-0.0, 2.0, 0.0),
        (-5.0, f64::INFINITY, f64::INFINITY),
        (5.0, f64::INFINITY, 5.0),
    ] {
        let actual = float(binary(BinaryOp::Rem, &py_float(left), &py_float(right)));
        assert!(same(actual, expected), "{left} % {right} is {actual}");
    }
}

#[test]
fn python_divides_integers_into_a_correctly_rounded_float() {
    let cases: [(i128, i128, f64); 7] = [
        (7, 2, 3.5),
        (-7, 2, -3.5),
        ((1 << 60) + 1, 3, 3.843_071_682_022_823e17),
        // Dividing the operands' nearest doubles gives 3346037147578390.0.
        (2_241_844_888_877_521_059, 670, 3_346_037_147_578_389.5),
        (1_157_777_724_344_136_025, 715, 1_619_269_544_537_253.2),
        ((1 << 64) + 3, (1 << 60) + 7, 16.0),
        (i128::MIN, 3, -5.671_372_782_015_641e37),
    ];
    for (left, right, expected) in cases {
        let actual = float(binary(BinaryOp::Div, &py_int(left), &py_int(right)));
        assert!(same(actual, expected), "{left} / {right} is {actual}");
    }

    let big = Exact::from(10_u128.pow(38));
    let big = of(Some(Rules::Python), LoadedValue::Int(big));
    let actual = float(binary(BinaryOp::Div, &big, &literal(7)));
    assert!(
        same(actual, 1.428_571_428_571_428_6e37),
        "10**38 / 7 is {actual}"
    );
}

#[test]
fn python_raises_on_division_by_zero_and_negative_shifts() {
    for (op, left, right) in [
        (BinaryOp::Div, py_int(1), literal(0)),
        (BinaryOp::Rem, py_int(1), literal(0)),
        (BinaryOp::Div, py_float(1.0), literal(0)),
        (BinaryOp::Rem, py_float(1.0), py_float(-0.0)),
        (BinaryOp::Div, py_bool(true), py_bool(false)),
        (BinaryOp::Shl, py_int(1), literal(-1)),
        (BinaryOp::Shr, py_int(1), literal(-1)),
    ] {
        let stop = binary(op, &left, &right).expect_err("raises");
        assert_eq!(kind(stop), ErrorKind::Arithmetic, "{op:?}");
    }
}

#[test]
fn python_bools_are_ints_that_stay_bools_under_bit_operations() {
    assert_eq!(
        int(binary(BinaryOp::Add, &py_bool(true), &py_bool(true))),
        2
    );
    assert_eq!(int(binary(BinaryOp::BitOr, &py_bool(true), &literal(2))), 3);
    assert_eq!(int(unary(UnaryOp::BitNot, &py_bool(true))), -2);
    let and = binary(BinaryOp::BitAnd, &py_bool(true), &py_bool(false)).expect("a value");
    assert_eq!(and.value, LoadedValue::Bool(false));
    assert_eq!(&*and.type_name, "bool");
    let divided = float(binary(BinaryOp::Div, &py_bool(true), &py_bool(true)));
    assert!(same(divided, 1.0));
}

#[test]
fn python_compares_ints_and_floats_exactly_and_text_by_code_point() {
    let above = py_int((1 << 53) + 1);
    let rounded = py_float(9_007_199_254_740_992.0);
    assert!(!compare(BinaryOp::Eq, &above, &rounded).expect("compared"));
    assert!(compare(BinaryOp::Gt, &above, &rounded).expect("compared"));
    let emoji = text(Rules::Python, "\u{1F600}");
    let last = text(Rules::Python, "\u{FFFF}");
    assert!(!compare(BinaryOp::Lt, &emoji, &last).expect("compared"));
}

#[test]
fn python_empty_collections_are_false() {
    let empty = of(Some(Rules::Python), LoadedValue::Collection { length: 0 });
    assert!(!truth(&empty).expect("a truth value"));
}

#[test]
fn javascript_numbers_are_doubles() {
    let smi = js_number(42.0);
    assert!(same(float(binary(BinaryOp::Div, &smi, &literal(5))), 8.4));
    for (op, left, right, expected) in [
        (BinaryOp::Rem, -7.0, 3.0, -1.0),
        (BinaryOp::Rem, 7.0, -3.0, 1.0),
        (BinaryOp::Rem, 42.0, -5.0, 2.0),
        (BinaryOp::Rem, -0.0, 5.0, -0.0),
        (BinaryOp::Rem, 5.0, 0.0, f64::NAN),
        (BinaryOp::Div, 5.0, 0.0, f64::INFINITY),
        (BinaryOp::Div, -5.0, 0.0, f64::NEG_INFINITY),
        (BinaryOp::Div, 0.0, 0.0, f64::NAN),
        (
            BinaryOp::Add,
            9_007_199_254_740_994.0,
            1.0,
            9_007_199_254_740_996.0,
        ),
        (
            BinaryOp::Mul,
            2_147_483_647.0,
            2_147_483_647.0,
            4_611_686_014_132_420_608.0,
        ),
    ] {
        let actual = float(binary(op, &js_number(left), &js_number(right)));
        assert!(same(actual, expected), "{left} {op:?} {right} is {actual}");
    }
}

#[test]
fn javascript_bit_operations_take_int32s_and_mask_shift_counts() {
    for (op, left, right, expected) in [
        (BinaryOp::BitOr, 2_147_483_648.0, 0.0, -2_147_483_648.0),
        (BinaryOp::BitOr, 4_294_967_296.5, 0.0, 0.0),
        (BinaryOp::BitOr, -1.5, 0.0, -1.0),
        (BinaryOp::Shl, 1.0, 33.0, 2.0),
        (BinaryOp::Shl, 1.0, -1.0, -2_147_483_648.0),
        (BinaryOp::Shr, -8.0, 1.0, -4.0),
        (BinaryOp::Shr, 2_147_483_648.0, 0.0, -2_147_483_648.0),
    ] {
        let actual = float(binary(op, &js_number(left), &js_number(right)));
        assert!(same(actual, expected), "{left} {op:?} {right} is {actual}");
    }

    assert!(same(float(unary(UnaryOp::BitNot, &js_number(5.7))), -6.0));
    assert!(same(float(unary(UnaryOp::Neg, &js_number(0.0))), -0.0));
}

#[test]
fn javascript_bigints_are_exact_and_truncate() {
    assert_eq!(
        int(binary(BinaryOp::Div, &js_bigint(-7), &js_bigint(2))),
        -3
    );
    assert_eq!(
        int(binary(BinaryOp::Rem, &js_bigint(-7), &js_bigint(2))),
        -1
    );
    assert_eq!(int(binary(BinaryOp::Rem, &js_bigint(7), &js_bigint(-2))), 1);
    assert_eq!(
        int(binary(BinaryOp::Shr, &js_bigint(-9), &js_bigint(1))),
        -5
    );
    assert_eq!(int(binary(BinaryOp::Shl, &js_bigint(1), &js_bigint(-1))), 0);
    assert_eq!(int(unary(UnaryOp::BitNot, &js_bigint(5))), -6);
    let product = binary(
        BinaryOp::Mul,
        &js_bigint(-12_345_678_901_234_567_890),
        &js_bigint(2),
    );
    let product = product.expect("a value");
    assert_eq!(&*product.type_name, "bigint");
    assert_eq!(
        product.value,
        LoadedValue::Int(Exact::from(-24_691_357_802_469_135_780_i128))
    );

    let stop = binary(BinaryOp::Div, &js_bigint(1), &js_bigint(0)).expect_err("RangeError");
    assert_eq!(kind(stop), ErrorKind::Arithmetic);
}

#[test]
fn javascript_refuses_to_mix_bigints_with_numbers_but_compares_them() {
    let one = js_bigint(1);
    for other in [literal(1), js_number(1.0), js(LoadedValue::Bool(true))] {
        let stop = binary(BinaryOp::Add, &one, &other).expect_err("TypeError");
        assert_eq!(kind(stop), ErrorKind::Type);
    }

    assert!(compare(BinaryOp::Lt, &one, &js_number(1.5)).expect("compared"));
    assert!(compare(BinaryOp::Eq, &one, &literal(1)).expect("compared"));
}

#[test]
fn javascript_converts_null_undefined_and_booleans_to_numbers() {
    let null = js(LoadedValue::Nothing);
    let undefined = js(LoadedValue::Undefined);
    assert!(same(float(binary(BinaryOp::Add, &null, &literal(1))), 1.0));
    assert!(float(binary(BinaryOp::Add, &undefined, &literal(1))).is_nan());
    let truth_value = js(LoadedValue::Bool(true));
    assert!(same(
        float(binary(BinaryOp::Add, &truth_value, &truth_value)),
        2.0
    ));
    assert!(compare(BinaryOp::Lt, &null, &literal(1)).expect("compared"));
    assert!(!compare(BinaryOp::Lt, &undefined, &literal(1)).expect("compared"));
}

#[test]
fn javascript_loose_equality_runs_no_code() {
    let null = js(LoadedValue::Nothing);
    let undefined = js(LoadedValue::Undefined);
    let nan = js_number(f64::NAN);
    assert!(compare(BinaryOp::Eq, &null, &undefined).expect("compared"));
    assert!(!compare(BinaryOp::Eq, &null, &literal(0)).expect("compared"));
    assert!(!compare(BinaryOp::Eq, &nan, &nan).expect("compared"));
    assert!(compare(BinaryOp::Ne, &nan, &nan).expect("compared"));
    let truth_value = js(LoadedValue::Bool(true));
    assert!(compare(BinaryOp::Eq, &truth_value, &literal(1)).expect("compared"));
    assert!(compare(BinaryOp::Eq, &js_bigint(1), &truth_value).expect("compared"));

    let one = text(Rules::JavaScript, "1");
    let stop = compare(BinaryOp::Eq, &one, &literal(1)).expect_err("needs StringToNumber");
    assert_eq!(kind(stop), ErrorKind::Unsupported);
    let array = js(LoadedValue::Collection { length: 0 });
    let stop = compare(BinaryOp::Eq, &array, &array).expect_err("by identity");
    assert_eq!(kind(stop), ErrorKind::Unsupported);
}

#[test]
fn javascript_orders_strings_by_utf16_code_units() {
    let emoji = text(Rules::JavaScript, "\u{1F600}");
    let last = text(Rules::JavaScript, "\u{FFFF}");
    assert!(compare(BinaryOp::Lt, &emoji, &last).expect("compared"));
}

#[test]
fn javascript_truth_makes_every_object_true() {
    for (value, expected) in [
        (js(LoadedValue::Collection { length: 0 }), true),
        (js(LoadedValue::Object), true),
        (js_number(f64::NAN), false),
        (js_number(-0.0), false),
        (js_bigint(0), false),
        (js(LoadedValue::Undefined), false),
        (text(Rules::JavaScript, ""), false),
    ] {
        assert_eq!(truth(&value).expect("a truth value"), expected, "{value:?}");
    }
}

#[test]
fn javascript_refuses_what_would_run_code_or_make_a_string() {
    let name = text(Rules::JavaScript, "a");
    let object = js(LoadedValue::Object);
    for (left, right) in [(&name, &literal(1)), (&object, &literal(1))] {
        let stop = binary(BinaryOp::Add, left, right).expect_err("refused");
        assert_eq!(kind(stop), ErrorKind::Unsupported);
    }
}

#[test]
fn javascript_keys_are_integer_indices() {
    assert_eq!(
        key(&js_number(2.0)).expect("an index"),
        Key::Integer(Exact::from(2_u128))
    );
    assert!(key(&js_number(1.5)).is_err());
    assert!(key(&js_number(9_007_199_254_740_992.0)).is_err());
}

#[test]
fn values_of_two_languages_are_not_combined() {
    let stop = binary(BinaryOp::Add, &py_int(1), &js_number(1.0)).expect_err("refused");
    assert_eq!(kind(stop), ErrorKind::Unsupported);
}

#[test]
fn computed_values_are_written_as_their_language_writes_them() {
    let python = [
        (1.764e23, "1.764e+23"),
        (f64::INFINITY, "inf"),
        (f64::NEG_INFINITY, "-inf"),
        (f64::NAN, "nan"),
        (3.0, "3.0"),
        (1e16, "1e+16"),
        (-0.0, "-0.0"),
        (1e-5, "1e-05"),
        (0.0001, "0.0001"),
        (1e15, "1000000000000000.0"),
        (123.456, "123.456"),
        (5e-324, "5e-324"),
        (f64::MAX, "1.7976931348623157e+308"),
    ];
    for (value, expected) in python {
        assert_eq!(
            shown(&py_float(value)).as_deref(),
            Some(expected),
            "{value}"
        );
    }

    let javascript = [
        (1.764e23, "1.764e+23"),
        (f64::INFINITY, "Infinity"),
        (f64::NEG_INFINITY, "-Infinity"),
        (f64::NAN, "NaN"),
        (-0.0, "-0"),
        (8.4, "8.4"),
        (1e21, "1e+21"),
        (1e-7, "1e-7"),
        (123e-20, "1.23e-18"),
    ];
    for (value, expected) in javascript {
        assert_eq!(
            shown(&js_number(value)).as_deref(),
            Some(expected),
            "{value}"
        );
    }

    let others = [
        (py_int(-7), "-7"),
        (py_bool(false), "False"),
        (
            js_bigint(-24_691_357_802_469_135_780),
            "-24691357802469135780n",
        ),
        (js(LoadedValue::Bool(true)), "true"),
    ];
    for (value, expected) in others {
        assert_eq!(shown(&value).as_deref(), Some(expected), "{value:?}");
    }

    assert_eq!(shown(&literal(1)), None);
}
