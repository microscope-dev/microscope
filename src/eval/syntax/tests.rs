use proptest::prelude::*;

use super::ast::{BinaryOp, CastForm, NodeKind, Suffix, UnaryOp};
use super::lexer::{MAX_TEXT_BYTES, TokenKind, lex};
use super::parser::{MAX_DEPTH, MAX_NODES};
use super::*;
use crate::eval::error::ErrorKind;
use crate::eval::number::Float;

fn parse(text: &str) -> Expression {
    Expression::parse(text).unwrap_or_else(|error| panic!("`{text}`: {error}"))
}

fn tree(text: &str) -> Tree {
    parse(text)
        .tree()
        .unwrap_or_else(|| panic!("`{text}` has several readings"))
        .clone()
}

fn error(text: &str) -> ExpressionError {
    match Expression::parse(text) {
        Ok(expression) => panic!("`{text}` parsed as `{expression}`"),
        Err(error) => error,
    }
}

/// The error's kind and the text it points at.
fn points_at(text: &str) -> (ErrorKind, &str) {
    let error = error(text);
    (error.kind, error.span.text(text))
}

/// The text's normal form.
fn normal(text: &str) -> String {
    parse(text).to_string()
}

fn single_token(text: &str) -> TokenKind {
    let tokens = lex(text).unwrap_or_else(|error| panic!("`{text}`: {error}"));
    assert_eq!(tokens.len(), 2, "`{text}` is one token: {tokens:?}");
    tokens[0].kind.clone()
}

#[test]
fn literals_read_as_written() {
    let int = |value, suffix| TokenKind::Integer { value, suffix };
    let typed = |width, signed| Some(Suffix::Int { width, signed });
    for (text, expected) in [
        ("0", int(0, None)),
        ("42", int(42, None)),
        ("1_000", int(1000, None)),
        ("0x2a", int(42, None)),
        ("0XFF", int(255, None)),
        ("0o17", int(15, None)),
        ("0b1010_1010", int(170, None)),
        ("255u8", int(255, typed(8, false))),
        ("1_i64", int(1, typed(64, true))),
        ("0xffu8", int(255, typed(8, false))),
        ("0xff_i7", int(255, typed(7, true))),
        ("0x1f32", int(0x1f32, None)),
        ("7u128", int(7, typed(128, false))),
        ("3usize", int(3, Some(Suffix::Size { signed: false }))),
        (
            "340282366920938463463374607431768211455",
            int(u128::MAX, None),
        ),
        ("1.5", TokenKind::Float(Float::from_f64(1.5))),
        ("1e3", TokenKind::Float(Float::from_f64(1000.0))),
        ("2.5e-3", TokenKind::Float(Float::from_f64(0.0025))),
        ("1_0.2_5", TokenKind::Float(Float::from_f64(10.25))),
        ("2.5f32", TokenKind::Float(Float::from_f32(2.5))),
        ("1f32", TokenKind::Float(Float::from_f32(1.0))),
        ("3f64", TokenKind::Float(Float::from_f64(3.0))),
        ("'a'", TokenKind::Char(97)),
        ("'\\n'", TokenKind::Char(10)),
        ("'\\''", TokenKind::Char(39)),
        ("'\\u{e9}'", TokenKind::Char(0xe9)),
        ("'é'", TokenKind::Char(0xe9)),
        ("\"a\\\"b\"", TokenKind::Text(b"a\"b".to_vec())),
        ("\"\\xff\\u{e9}\"", TokenKind::Text(vec![0xff, 0xc3, 0xa9])),
        ("\"\"", TokenKind::Text(Vec::new())),
        (
            "`my-pkg.global`",
            TokenKind::Quoted("my-pkg.global".to_owned()),
        ),
        ("$rax", TokenKind::Register("rax".to_owned())),
        ("_name9", TokenKind::Ident("_name9".to_owned())),
    ] {
        assert_eq!(single_token(text), expected, "`{text}`");
    }
}

#[test]
fn malformed_literals_point_at_themselves_and_suggest_fixes() {
    for (text, pointed) in [
        ("1 + 2q8", "2q8"),
        ("0x", "0x"),
        ("3.5e39f32", "3.5e39f32"),
        ("0b2", "0b2"),
        ("0x1p3", "0x1p3"),
        ("1u0", "1u0"),
        ("1u129", "1u129"),
        ("1u08", "1u08"),
        ("''", "''"),
        ("'\\xff'", "'\\xff"),
        ("\"open", "\"open"),
        ("\"\\q\"", "\"\\q"),
        ("\"\\u{d800}\"", "\"\\u"),
        ("``", "``"),
        ("`open", "`open"),
        ("$", "$"),
        ("a @ b", "@"),
        ("naïve", "ï"),
    ] {
        assert_eq!(points_at(text), (ErrorKind::Syntax, pointed), "`{text}`");
    }

    assert_eq!(
        error("017").hint.as_deref(),
        Some("write 0o17 for octal or 17 for decimal")
    );
    assert!(error("17UL").hint.unwrap().contains("17 as unsigned long"));
    assert!(error("naïve").hint.unwrap().contains("backticks"));
    assert_eq!(error("nullptr").hint.as_deref(), Some("write `null`"));
    // Text in single quotes, as Python and JavaScript write it.
    assert_eq!(
        error("d['nut']").hint.as_deref(),
        Some("text goes in double quotes, as in `\"nut\"`")
    );
    assert_eq!(error("'ab").hint, None);
}

/// Precedence levels from loosest to tightest, as the reference lists them,
/// written independently of the parser's table.
const LEVELS: &[&[&str]] = &[
    &["||"],
    &["&&"],
    &["==", "!=", "<", "<=", ">", ">="],
    &["|"],
    &["^"],
    &["&"],
    &["<<", ">>"],
    &["+", "-"],
    &["*", "/", "%"],
];

fn level(op: &str) -> usize {
    LEVELS
        .iter()
        .position(|level| level.contains(&op))
        .expect("a listed operator")
}

/// Every ordered pair of binary operators groups as the table says, which
/// the fully parenthesized text pins down.
#[test]
fn every_pair_of_binary_operators_groups_by_the_table() {
    let operators: Vec<&str> = LEVELS
        .iter()
        .flat_map(|level| level.iter().copied())
        .collect();
    for &first in &operators {
        for &second in &operators {
            let text = format!("a {first} b {second} c");
            let (first_level, second_level) = (level(first), level(second));
            if first_level == second_level && LEVELS[first_level].contains(&"==") {
                assert_eq!(points_at(&text), (ErrorKind::Syntax, second), "`{text}`");
                continue;
            }

            let grouped = if first_level >= second_level {
                format!("((a {first} b) {second} c)")
            } else {
                format!("(a {first} (b {second} c))")
            };

            assert!(tree(&text).same_shape(&tree(&grouped)), "`{text}`");
            assert_eq!(normal(&grouped), text, "`{grouped}` needs no parentheses");
        }

        // Prefix operators bind tighter than every binary operator, and `as`
        // tighter than every binary operator but looser than prefixes.
        for prefix in ["-", "!", "~", "*", "&"] {
            let text = format!("{prefix}a {first} b");
            assert!(
                tree(&text).same_shape(&tree(&format!("({prefix}a) {first} b"))),
                "`{text}`"
            );
        }

        let text = format!("a {first} b as u8");
        assert!(
            tree(&text).same_shape(&tree(&format!("a {first} (b as u8)"))),
            "`{text}`"
        );
        let text = format!("a {first} b ? c : d");
        assert!(
            tree(&text).same_shape(&tree(&format!("(a {first} b) ? c : d"))),
            "`{text}`"
        );
        let text = format!("a = b {first} c");
        assert!(
            tree(&text).same_shape(&tree(&format!("a = (b {first} c)"))),
            "`{text}`"
        );
    }

    for (text, grouped) in [
        ("-a as u8", "(-a) as u8"),
        ("a as u8 as i16", "(a as u8) as i16"),
        ("*p.x", "*(p.x)"),
        ("-a[0]", "-(a[0])"),
        ("&a->b", "&(a->b)"),
        ("(int)a.b", "(int)(a.b)"),
        ("(int)a as u8", "((int)a) as u8"),
        ("a ? b : c ? d : e", "a ? b : (c ? d : e)"),
        ("a = b = c", "a = (b = c)"),
        ("a += b -= c", "a += (b -= c)"),
        ("x & 1 == 0", "(x & 1) == 0"),
        ("1 << 70 >> 68", "(1 << 70) >> 68"),
        ("a - b - c", "(a - b) - c"),
        ("a ? b : c = d", "(a ? b : c) = d"),
    ] {
        assert!(
            tree(text).same_shape(&tree(grouped)),
            "`{text}` is `{grouped}`"
        );
    }
}

#[test]
fn casts_are_read_from_their_spelling() {
    let cast = |text: &str| {
        let t = tree(text);
        let NodeKind::Cast { ty, form, .. } = t.kind(t.root()).clone() else {
            panic!("`{text}` is a cast");
        };

        (super::print::type_text(&ty), form)
    };

    for (text, ty, form) in [
        ("(int)x", "int", CastForm::Prefix),
        ("(T)x", "T", CastForm::Prefix),
        ("(T)(x)", "T", CastForm::Prefix),
        ("(T)!x", "T", CastForm::Prefix),
        ("(T)1", "T", CastForm::Prefix),
        ("(const T)-1", "T", CastForm::Prefix),
        ("(struct S*)&x", "struct S*", CastForm::Prefix),
        ("(T**)p", "T**", CastForm::Prefix),
        ("(int)-1", "int", CastForm::Prefix),
        ("(ns::T)x", "ns::T", CastForm::Prefix),
        ("(main.point)x", "main.point", CastForm::Prefix),
        ("(`Option<i32>`)x", "`Option<i32>`", CastForm::Prefix),
        ("x as u8", "u8", CastForm::As),
        ("x as *T", "T*", CastForm::As),
        (
            "x as long unsigned long",
            "unsigned long long",
            CastForm::As,
        ),
        ("x as struct S", "struct S", CastForm::As),
    ] {
        assert_eq!(cast(text), (ty.to_owned(), form), "`{text}`");
    }

    // A trailing `*` after `as` multiplies; `(*p)` dereferences.
    let product = tree("x as T * 2");
    assert!(matches!(
        product.kind(product.root()),
        NodeKind::Binary {
            op: BinaryOp::Mul,
            ..
        }
    ));
    let member = tree("(*p).x");
    let NodeKind::Member { base, .. } = member.kind(member.root()) else {
        panic!("a member");
    };

    assert!(matches!(
        member.kind(*base),
        NodeKind::Unary {
            op: UnaryOp::Deref,
            ..
        }
    ));

    // A parenthesized name before anything that cannot begin an operand
    // only groups.
    for text in [
        "(n) + 1",
        "(n)[0]",
        "(p).x",
        "(p)->x",
        "(n) == 1",
        "(n)",
        "(n) as u8",
    ] {
        assert!(parse(text).ambiguities().is_empty(), "`{text}`");
        assert!(
            !matches!(
                tree(text).kind(tree(text).root()),
                NodeKind::Cast {
                    form: CastForm::Prefix,
                    ..
                }
            ),
            "`{text}`"
        );
    }

    assert_eq!(points_at("(int) + 1"), (ErrorKind::Syntax, "+"));
}

/// A parenthesized name before `-`, `*`, or `&` has a reading for each of
/// what the name may be, and the readings group what follows differently.
#[test]
fn a_parenthesized_name_before_an_operator_has_both_readings() {
    for (text, as_value, as_type) in [
        ("(n) - 1", "n - 1", "(n)(-1)"),
        ("(n) - a * b", "n - a * b", "((n)(-a)) * b"),
        ("(n) * p + 1", "n * p + 1", "(n)(*p) + 1"),
        ("(n) & x", "n & x", "(n)(&x)"),
        ("x * (n) - 1", "x * n - 1", "x * (n)(-1)"),
        ("(a.b) - 1", "a.b - 1", "(a.b)(-1)"),
        ("(::g) - 1", "::g - 1", "(::g)(-1)"),
        ("-(n) - 1", "-n - 1", "-(n)(-1)"),
    ] {
        let expression = parse(text);
        assert_eq!(expression.ambiguities().len(), 1, "`{text}`");
        let value = expression.reading(0).expect("the value reading parses");
        let cast = expression.reading(1).expect("the cast reading parses");
        assert!(value.same_shape(&tree(as_value)), "`{text}` as a value");
        assert!(cast.same_shape(&tree(as_type)), "`{text}` as a type");
        assert_eq!(
            expression.to_string(),
            text,
            "the normal form keeps the parentheses"
        );
    }

    let expression = parse("(a) - (b) - c");
    assert_eq!(expression.ambiguities().len(), 2);
    let names: Vec<_> = expression
        .ambiguities()
        .iter()
        .map(|ambiguity| ambiguity.span.text("(a) - (b) - c"))
        .collect();
    assert_eq!(names, ["(a)", "(b)"]);
    assert!(
        expression
            .reading(0b11)
            .unwrap()
            .same_shape(&tree("(a)(-(b)(-c))"))
    );
    assert!(
        expression
            .reading(0b01)
            .unwrap()
            .same_shape(&tree("(a)(-b) - c"))
    );
    assert!(
        expression
            .reading(0b10)
            .unwrap()
            .same_shape(&tree("a - (b)(-c)"))
    );

    // Not a parenthesized name: no ambiguity.
    for text in [
        "(int) - 1",
        "(n.0) - 1",
        "sizeof(n) - 1",
        "len(n) - 1",
        "(1) - 1",
        "(true) - 1",
    ] {
        assert!(parse(text).ambiguities().is_empty(), "`{text}`");
    }

    assert_eq!(points_at("(a)-(b)-(c)-(d)-(e)-f"), (ErrorKind::Limit, "("));
    assert_eq!(error("(a)-(b)-(c)-(d)-(e)-f").span.start, 16);
}

/// A view's expressions may call the views' built-in functions; the
/// console's may not, and in neither is a built-in's name reserved.
#[test]
fn only_views_call_built_in_functions() {
    let view = |text: &str| {
        Expression::parse_view(text).unwrap_or_else(|error| panic!("`{text}`: {error}"))
    };

    let expression = view("inner(inner(buf) . ptr) as *T");
    assert_eq!(expression.to_string(), "inner(inner(buf).ptr) as *T");
    assert_eq!(
        Expression::parse_view(&expression.to_string())
            .expect("the normal form parses")
            .to_string(),
        expression.to_string()
    );

    // A parenthesized name after a built-in is its operand, not a cast.
    let expression = view("inner(x) - 1");
    assert!(expression.ambiguities().is_empty());
    let tree = expression.tree().expect("one reading");
    assert!(matches!(
        tree.kind(tree.root()),
        NodeKind::Binary {
            op: BinaryOp::Sub,
            ..
        }
    ));
    assert_eq!(view("inner.inner").to_string(), "inner.inner");
    assert_eq!(points_at("inner(x)"), (ErrorKind::Syntax, "inner("));
    assert_eq!(normal("inner + 1"), "inner + 1");
    let unclosed = Expression::parse_view("inner(x").expect_err("unclosed");
    assert_eq!(unclosed.kind, ErrorKind::Syntax);
}

/// A tuple is written in parentheses with commas, or in brackets with
/// commas, and a single parenthesized expression still only groups.
#[test]
fn tuples_are_read_in_parentheses_and_in_brackets() {
    let items = |text: &str| {
        let t = tree(text);
        let NodeKind::Tuple(items) = t.kind(t.root()).clone() else {
            panic!("`{text}` is a tuple");
        };

        items
            .iter()
            .map(|item| t.span(*item).text(text).to_owned())
            .collect::<Vec<_>>()
    };

    assert_eq!(items("(1, 2)"), ["1", "2"]);
    assert_eq!(items("(a,)"), ["a"]);
    assert!(items("()").is_empty());
    assert_eq!(
        items("((1, 2), x + 1, \"s\")"),
        ["(1, 2)", "x + 1", "\"s\""]
    );
    assert_eq!(items("(1, 2,)"), ["1", "2"]);

    for (text, index) in [
        ("d[1, 2]", "1, 2"),
        ("d[1,]", "1,"),
        ("d[(1, 2)]", "(1, 2)"),
        ("d[a, (b, c)]", "a, (b, c)"),
    ] {
        let t = tree(text);
        let NodeKind::Index { index: key, .. } = t.kind(t.root()) else {
            panic!("`{text}` is an index");
        };

        assert!(
            matches!(t.kind(*key), NodeKind::Tuple(_)),
            "`{text}` indexes by a tuple"
        );
        assert_eq!(t.span(*key).text(text), index, "`{text}`");
    }

    assert!(parse("d[1, 2]").same_shape(&parse("d[(1, 2)]")));
    assert!(!parse("d[(1)]").same_shape(&parse("d[(1,)]")));
    assert!(!parse("(1, 2)").same_shape(&parse("(1, 2, 3)")));

    // One parenthesized expression groups, and a cast is still a cast.
    assert!(matches!(
        tree("(a)").kind(tree("(a)").root()),
        NodeKind::Name(_)
    ));
    for text in ["(int)x", "(py)x", "(T*)p", "(n) - 1"] {
        let expression = parse(text);
        let reading = expression
            .reading(expression.ambiguities().len())
            .expect("a reading");
        assert!(
            !matches!(reading.kind(reading.root()), NodeKind::Tuple(_)),
            "`{text}`"
        );
    }

    for (text, expected) in [
        ("( 1 ,2 )", "(1, 2)"),
        ("(a ,)", "(a,)"),
        ("( )", "()"),
        ("d [ 1 , 2 ]", "d[(1, 2)]"),
        ("d[1,]", "d[(1,)]"),
        ("d[((1, 2))]", "d[(1, 2)]"),
        ("((1, 2), 3)", "((1, 2), 3)"),
        ("(a = 1, b)", "(a = 1, b)"),
        ("(c ? 1 : 2, 3)", "(c ? 1 : 2, 3)"),
    ] {
        assert_eq!(normal(text), expected, "`{text}`");
        assert!(
            parse(expected).same_shape(&parse(text)),
            "`{expected}` reads as `{text}`"
        );
    }

    for (text, pointed) in [
        ("d[1, 2:3]", ":"),
        ("d[1, 2..3]", ".."),
        ("d[:1, 2]", ","),
        ("(1, 2", ""),
        ("(1 2)", "2"),
        ("(,)", ","),
        ("d[,]", ","),
        ("(1,,)", ","),
        ("f(1, 2)", "f("),
    ] {
        assert_eq!(points_at(text), (ErrorKind::Syntax, pointed), "`{text}`");
    }

    // A view's expressions never hold tuples.
    for text in ["(1, 2)", "m[1, 2]", "()", "(a,)"] {
        let error = Expression::parse_view(text).expect_err(text);
        assert_eq!(error.kind, ErrorKind::Syntax, "`{text}`");
        assert!(
            error.message.contains("tuple"),
            "`{text}`: {}",
            error.message
        );
    }
}

#[test]
fn syntax_errors_point_at_the_offending_text() {
    for (text, kind, pointed) in [
        ("a == b != c", ErrorKind::Syntax, "!="),
        ("ns::f(x)", ErrorKind::Syntax, "ns::f("),
        ("", ErrorKind::Syntax, ""),
        (")", ErrorKind::Syntax, ")"),
        ("a b", ErrorKind::Syntax, "b"),
        ("a ? b", ErrorKind::Syntax, ""),
        ("a[1", ErrorKind::Syntax, ""),
        ("(a", ErrorKind::Syntax, ""),
        ("a.", ErrorKind::Syntax, ""),
        ("a->1x", ErrorKind::Syntax, "x"),
        ("x as", ErrorKind::Syntax, ""),
        ("x as 1", ErrorKind::Syntax, "1"),
        ("as", ErrorKind::Syntax, "as"),
        ("a::", ErrorKind::Syntax, ""),
        ("&&x", ErrorKind::Syntax, "&&"),
        ("sizeof x", ErrorKind::Syntax, "x"),
    ] {
        assert_eq!(points_at(text), (kind, pointed), "`{text}`");
    }
}

#[test]
fn limits_bound_text_depth_and_size() {
    let long = "a".repeat(MAX_TEXT_BYTES + 1);
    assert_eq!(error(&long).kind, ErrorKind::Limit);
    let at_limit = format!(
        "{}a{}",
        "(".repeat(MAX_DEPTH / 2 - 1),
        ")".repeat(MAX_DEPTH / 2 - 1)
    );
    parse(&at_limit);
    let deep = format!("{}a{}", "(".repeat(MAX_DEPTH), ")".repeat(MAX_DEPTH));
    assert_eq!(error(&deep).kind, ErrorKind::Limit);
    let negations = format!("{}a", "-".repeat(MAX_DEPTH + 1));
    assert_eq!(error(&negations).kind, ErrorKind::Limit);
    let wide = vec!["a"; MAX_NODES / 2 + 1].join(" + ");
    assert_eq!(error(&wide).kind, ErrorKind::Limit);
}

#[test]
fn normal_form_spaces_operators_and_keeps_needed_parentheses() {
    for (text, expected) in [
        ("a+b*c", "a + b * c"),
        ("(a+b)*c", "(a + b) * c"),
        ("((a))", "a"),
        ("- ( - x)", "--x"),
        ("&(&x)", "& &x"),
        ("(n)  -  1", "(n)  -  1"),
        ("( T )( - 1 )", "(T)(-1)"),
        ("(int)-1", "(int)-1"),
        ("(a).b", "a.b"),
        ("a.b", "a.b"),
        ("(a.b).c", "a.b.c"),
        ("one.c::duplicate + 1", "one.c::duplicate + 1"),
        ("t.0.1", "t.0.1"),
        ("0x1.5", "(1).5"),
        ("1u8.5", "1u8.5"),
        ("p -> x", "p->x"),
        ("x as * T", "x as *T"),
        ("( T * ) p", "(T*)p"),
        ("a [ 1 .. 4 ]", "a[1..4]"),
        ("`a b` + `c`", "`a b` + c"),
        ("`as`", "`as`"),
        ("`int` + 1", "`int` + 1"),
        ("'\\u{1}' + '\\''", "'\\u{1}' + '\\''"),
        ("\"a\\x00\\xff\\\"\"", "\"a\\0\\xff\\\"\""),
        (
            "1.0 + 2.5f32 + 1e300 + 0x10_u8",
            "1.0 + 2.5f32 + 1e300 + 16u8",
        ),
        ("3isize", "3isize"),
        ("(a ? b : c) ? d : e", "(a ? b : c) ? d : e"),
        ("a ? (b = 1) : c", "a ? (b = 1) : c"),
        ("(a = b) + 1", "(a = b) + 1"),
        ("-(a as u8)", "-(a as u8)"),
        ("(-a) as u8", "-a as u8"),
        ("sizeof ( T )", "sizeof(T)"),
        ("sizeof(unsigned int)", "sizeof(unsigned int)"),
        ("sizeof(T mut)", "sizeof(const T)"),
        ("len( a )", "len(a)"),
        ("len + 1", "len + 1"),
        ("$pc", "$pc"),
        ("::g", "::g"),
        ("true && null == p", "true && null == p"),
        ("(a < b) == c", "(a < b) == c"),
    ] {
        assert_eq!(normal(text), expected, "`{text}`");
        assert!(
            parse(expected).same_shape(&parse(text)),
            "`{expected}` reads as `{text}`"
        );
    }
}

/// Text from a small grammar of the language, with random spacing and
/// parentheses.
fn expression_text() -> impl Strategy<Value = String> {
    let leaf = prop_oneof![
        Just("a".to_owned()),
        Just("b.c".to_owned()),
        Just("ns::d".to_owned()),
        Just("`q x`".to_owned()),
        Just("$rax".to_owned()),
        Just("null".to_owned()),
        Just("true".to_owned()),
        Just("'z'".to_owned()),
        Just("\"s\\n\"".to_owned()),
        Just("1.5".to_owned()),
        Just("2.5f32".to_owned()),
        (0..300_u32).prop_map(|value| value.to_string()),
        (0..300_u32).prop_map(|value| format!("{value}u8")),
        Just("0xff".to_owned()),
        Just("(T)".to_owned()),
    ];
    leaf.prop_recursive(6, 48, 3, |inner| {
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
            Just("<"),
            Just("="),
            Just("+="),
        ];
        let prefix = prop_oneof![Just("-"), Just("!"), Just("~"), Just("*"), Just("&")];
        let types = prop_oneof![
            Just("int"),
            Just("T"),
            Just("unsigned long"),
            Just("T*"),
            Just("struct S"),
        ];
        prop_oneof![
            (inner.clone(), binary, inner.clone())
                .prop_map(|(left, op, right)| format!("{left} {op} {right}")),
            (prefix, inner.clone()).prop_map(|(op, operand)| format!("{op}{operand}")),
            inner.clone().prop_map(|operand| format!("({operand})")),
            inner.clone().prop_map(|operand| format!("({operand}).f")),
            inner
                .clone()
                .prop_map(|operand| format!("({operand})->g.0")),
            (inner.clone(), inner.clone()).prop_map(|(base, index)| format!("({base})[{index}]")),
            (inner.clone(), inner.clone())
                .prop_map(|(base, index)| format!("({base})[{index}, 1]")),
            (inner.clone(), inner.clone()).prop_map(|(a, b)| format!("({a}, {b})")),
            inner.clone().prop_map(|item| format!("({item},)")),
            (inner.clone(), inner.clone(), inner.clone())
                .prop_map(|(a, b, c)| format!("{a} ? {b} : {c}")),
            (types.clone(), inner.clone()).prop_map(|(ty, operand)| format!("({ty}){operand}")),
            (inner.clone(), types)
                .prop_map(|(operand, ty)| format!("({operand}) as {}", ty.trim_end_matches('*'))),
            (
                prop_oneof![Just("n"), Just("ns::T"), Just("a.b")],
                prop_oneof![Just("-"), Just("*"), Just("&")],
                inner.clone()
            )
                .prop_map(|(name, op, rest)| format!("({name}) {op} {rest}")),
            inner
                .clone()
                .prop_map(|operand| format!("sizeof({operand})")),
            inner.clone().prop_map(|operand| format!("len({operand})")),
            inner.clone().prop_map(|operand| format!("cap({operand})")),
            (
                inner.clone(),
                proptest::option::of(inner.clone()),
                proptest::option::of(inner)
            )
                .prop_map(|(base, start, end)| format!(
                    "({base})[{}:{}]",
                    start.unwrap_or_default(),
                    end.unwrap_or_default()
                )),
        ]
    })
}

proptest! {
    #[test]
    fn the_normal_form_reads_back_the_same(text in expression_text()) {
        check_invariants(&text).map_err(TestCaseError::fail)?;
    }

    #[test]
    fn arbitrary_text_holds_the_invariants(text in "[-a-z0-9 ()\\[\\],.*&!~<>=+?:`$'\"\\\\]{0,40}") {
        check_invariants(&text).map_err(TestCaseError::fail)?;
    }
}

#[test]
fn children_are_named_by_expressions_that_parse_back() {
    let pair = Expression::name("pair").expect("a name");
    assert_eq!(
        pair.member("first").expect("a member").to_string(),
        "pair.first"
    );
    assert_eq!(
        pair.member("a.b").expect("a member").to_string(),
        "pair.`a.b`"
    );
    let target = Expression::name("p")
        .and_then(|name| name.dereferenced())
        .expect("a pointee");
    assert_eq!(target.to_string(), "*p");
    assert_eq!(target.member("x").expect("a member").to_string(), "(*p).x");
    assert_eq!(
        pair.indexed(&[-2, 3]).expect("an element").to_string(),
        "pair[-2][3]"
    );
    assert_eq!(pair.range(1, 4).expect("a range").to_string(), "pair[1..4]");
    assert_eq!(
        Expression::name("has space").expect("a name").to_string(),
        "`has space`"
    );
    let sum = Expression::parse("a + b").expect("a sum");
    assert_eq!(sum.member("c").expect("a member").to_string(), "(a + b).c");
}
