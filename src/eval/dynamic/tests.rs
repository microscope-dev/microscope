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
            extent: None,
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

mod properties {
    use std::cmp::Ordering;

    use proptest::prelude::*;

    use super::*;

    /// A nonnegative integer as 64-bit limbs, low first: just enough
    /// arithmetic to compare a quotient with a double exactly.
    fn wide(value: u128) -> Vec<u64> {
        #[expect(clippy::cast_possible_truncation, reason = "the low limb")]
        let low = value as u64;
        let high = (value >> 64) as u64;
        vec![low, high]
    }

    fn times(limbs: &[u64], factor: u64) -> Vec<u64> {
        let mut carry = 0_u128;
        let mut product: Vec<u64> = limbs
            .iter()
            .map(|&limb| {
                let sum = u128::from(limb) * u128::from(factor) + carry;
                carry = sum >> 64;
                #[expect(clippy::cast_possible_truncation, reason = "the low limb")]
                let low = sum as u64;
                low
            })
            .collect();
        product.extend(wide(carry));
        product
    }

    fn shifted(limbs: &[u64], bits: u32) -> Vec<u64> {
        let mut shifted = vec![0; (bits / 64) as usize];
        let (by, mut carry) = (bits % 64, 0);
        for &limb in limbs {
            shifted.push(if by == 0 { limb } else { (limb << by) | carry });
            carry = if by == 0 { 0 } else { limb >> (64 - by) };
        }

        shifted.push(carry);
        shifted
    }

    fn compare(left: &[u64], right: &[u64]) -> Ordering {
        let length = left.len().max(right.len());
        let limb = |limbs: &[u64], at: usize| limbs.get(at).copied().unwrap_or(0);
        (0..length)
            .rev()
            .map(|at| limb(left, at).cmp(&limb(right, at)))
            .find(|order| order.is_ne())
            .unwrap_or(Ordering::Equal)
    }

    /// A positive normal double as `mantissa · 2^exponent`.
    fn parts(value: f64) -> (u128, i32) {
        let bits = value.to_bits();
        let exponent = i32::try_from(bits >> 52).expect("positive") - 1075;
        (u128::from(bits & ((1 << 52) - 1) | 1 << 52), exponent)
    }

    /// The point halfway between two neighboring positive doubles.
    fn midpoint(low: f64, high: f64) -> (u128, i32) {
        let ((low, low_exponent), (high, high_exponent)) = (parts(low), parts(high));
        let exponent = low_exponent.min(high_exponent);
        let sum = (low << (low_exponent - exponent)) + (high << (high_exponent - exponent));
        (sum, exponent - 1)
    }

    /// `dividend / divisor` against `mantissa · 2^exponent`, exactly.
    fn compare_quotient(
        dividend: u128,
        divisor: u128,
        (mantissa, exponent): (u128, i32),
    ) -> Ordering {
        let mantissa = u64::try_from(mantissa).expect("a midpoint has 55 bits");
        let left = shifted(&wide(dividend), (-exponent).max(0).unsigned_abs());
        let right = shifted(
            &times(&wide(divisor), mantissa),
            exponent.max(0).unsigned_abs(),
        );
        compare(&left, &right)
    }

    /// An exact integer: small ones, powers of two and their neighbors,
    /// and any in range, of either sign.
    fn exact() -> impl Strategy<Value = Exact> {
        let magnitude = prop_oneof![
            0_u128..1000,
            (0_u32..128, 0_u128..3)
                .prop_map(|(shift, add)| (1_u128 << shift).saturating_add(add).saturating_sub(1)),
            0_u128..(1 << 60),
            any::<u128>(),
        ];

        (any::<bool>(), magnitude).prop_map(|(negative, magnitude)| {
            if negative && magnitude <= 1 << 127 {
                Exact::from(magnitude.cast_signed().wrapping_neg())
            } else {
                Exact::from(magnitude)
            }
        })
    }

    fn nonzero() -> impl Strategy<Value = Exact> {
        exact().prop_map(|value| {
            if value.is_zero() {
                Exact::from(1_u128)
            } else {
                value
            }
        })
    }

    fn py_exact(value: Exact) -> Loaded {
        of(Some(Rules::Python), LoadedValue::Int(value))
    }

    /// `x mod modulus`, in `0..modulus`.
    fn modulo(value: Exact, modulus: u128) -> u128 {
        let rest = value.magnitude() % modulus;
        if value.is_negative() && rest != 0 {
            modulus - rest
        } else {
            rest
        }
    }

    const TWO_53: i64 = 1 << 53;

    proptest! {
        /// Operands a double holds exactly divide as their doubles do,
        /// which IEEE division rounds once.
        #[test]
        fn python_divides_small_integers_as_doubles(
            left in -TWO_53..=TWO_53,
            right in (1..=TWO_53, any::<bool>()).prop_map(|(right, negative)| if negative { -right } else { right }),
        ) {
            let actual = float(binary(BinaryOp::Div, &py_int(left.into()), &py_int(right.into())));
            #[expect(clippy::cast_precision_loss, reason = "both fit in 53 bits")]
            let expected = left as f64 / right as f64;
            prop_assert!(same(actual, expected), "{} / {} is {}, not {}", left, right, actual, expected);
        }

        /// Any quotient is the double nearest the exact one, ties to
        /// even, with the operands' sign; by a power of two it is the
        /// dividend's nearest double, scaled.
        #[test]
        fn python_divides_integers_rounding_once(left in exact(), right in nonzero()) {
            let actual = float(binary(BinaryOp::Div, &py_exact(left), &py_exact(right)));
            let negative = left.is_negative() != right.is_negative();
            prop_assert_eq!(actual.is_sign_negative(), negative, "{:?} / {:?} is {}", left, right, actual);

            let magnitude = actual.abs();
            let (dividend, divisor) = (left.magnitude(), right.magnitude());
            if dividend == 0 {
                prop_assert_eq!(magnitude.to_bits(), 0);
            } else {
                let below = compare_quotient(dividend, divisor, midpoint(magnitude.next_down(), magnitude));
                let above = compare_quotient(dividend, divisor, midpoint(magnitude, magnitude.next_up()));
                let even = parts(magnitude).0.is_multiple_of(2);
                prop_assert!(below.is_ge() && above.is_le(), "{:?} / {:?} is {}", left, right, actual);
                prop_assert!(even || (below.is_gt() && above.is_lt()), "{:?} / {:?}: a tie to odd {}", left, right, actual);
            }

            if divisor.is_power_of_two() {
                #[expect(clippy::cast_precision_loss, reason = "the one rounding")]
                let expected = dividend as f64 / divisor as f64;
                prop_assert_eq!(magnitude.to_bits(), expected.to_bits());
            }
        }

        /// `a % b` has `b`'s sign, is smaller than `b`, and differs from
        /// `a` by a multiple of `b`: `a == floor(a / b) · b + r`.
        #[test]
        fn python_remainders_take_the_divisors_sign(left in exact(), right in nonzero()) {
            let result = binary(BinaryOp::Rem, &py_exact(left), &py_exact(right)).expect("a remainder");
            let LoadedValue::Int(rest) = result.value else {
                panic!("not an integer: {result:?}");
            };

            prop_assert!(rest.is_zero() || rest.is_negative() == right.is_negative(), "{:?} % {:?} is {:?}", left, right, rest);
            prop_assert!(rest.magnitude() < right.magnitude(), "{:?} % {:?} is {:?}", left, right, rest);
            prop_assert_eq!(modulo(left, right.magnitude()), modulo(rest, right.magnitude()));
        }

        /// `x | 0` is `ToInt32(x)`: an integer-valued double below 2^63
        /// wraps as an `i64` cut to 32 bits, and any other drops its
        /// fraction first.
        #[test]
        fn javascript_bit_operators_take_32_bits(
            mantissa in -TWO_53..TWO_53,
            shift in -60_i32..=10,
        ) {
            #[expect(clippy::cast_precision_loss, reason = "the mantissa has 53 bits")]
            let value = mantissa as f64 * 2_f64.powi(shift);
            let actual = float(binary(BinaryOp::BitOr, &js_number(value), &js_number(0.0)));

            #[expect(clippy::cast_possible_truncation, reason = "the rule under test")]
            let expected = f64::from((value as i64) as i32);
            prop_assert!(same(actual, expected), "{} | 0 is {}, not {}", value, actual, expected);
        }
    }
}
