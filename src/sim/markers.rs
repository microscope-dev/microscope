//! Parser and evaluator for inline `// MARK:` and `// EXPECT:` assertions in
//! golden C source files, used to verify variable inspection and expression
//! evaluation at stopped source lines.

use std::collections::BTreeMap;
use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Marker {
    pub line: u64,
    pub condition: Condition,

    pub text: String,

    pub expect: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Condition(Vec<Comparison>);

#[derive(Debug, Clone, PartialEq, Eq)]
struct Comparison {
    left: Expression,
    operator: Operator,
    right: Expression,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Operator {
    Equal,
    NotEqual,
    Less,
    LessOrEqual,
    Greater,
    GreaterOrEqual,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Expression {
    Number(i128),
    Variable(String),
    Binary(Box<Self>, char, Box<Self>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Holds,

    Fails(String),

    Unknown(String),
}

impl Condition {
    #[must_use]
    pub fn variables(&self) -> Vec<&str> {
        let mut names = Vec::new();
        for comparison in &self.0 {
            comparison.left.variables(&mut names);
            comparison.right.variables(&mut names);
        }

        names.sort_unstable();
        names.dedup();
        names
    }

    #[must_use]
    pub fn evaluate(&self, values: &BTreeMap<String, i128>) -> Verdict {
        for comparison in &self.0 {
            let (left, right) = match (
                comparison.left.evaluate(values),
                comparison.right.evaluate(values),
            ) {
                (Ok(left), Ok(right)) => (left, right),
                (Err(missing), _) | (_, Err(missing)) => return Verdict::Unknown(missing),
            };

            let holds = match comparison.operator {
                Operator::Equal => left == right,
                Operator::NotEqual => left != right,
                Operator::Less => left < right,
                Operator::LessOrEqual => left <= right,
                Operator::Greater => left > right,
                Operator::GreaterOrEqual => left >= right,
            };

            if !holds {
                return Verdict::Fails(format!(
                    "{} is {left} and {} is {right}",
                    comparison.left, comparison.right
                ));
            }
        }

        Verdict::Holds
    }
}

impl Expression {
    fn variables<'a>(&'a self, names: &mut Vec<&'a str>) {
        match self {
            Self::Number(_) => {}
            Self::Variable(name) => names.push(name),
            Self::Binary(left, _, right) => {
                left.variables(names);
                right.variables(names);
            }
        }
    }

    fn evaluate(&self, values: &BTreeMap<String, i128>) -> Result<i128, String> {
        match self {
            Self::Number(value) => Ok(*value),
            Self::Variable(name) => values.get(name).copied().ok_or_else(|| name.clone()),
            Self::Binary(left_operand, operator, right_operand) => {
                let left = left_operand.evaluate(values)?;
                let right = right_operand.evaluate(values)?;
                let value = match operator {
                    '+' => left.checked_add(right),
                    '-' => left.checked_sub(right),
                    '*' => left.checked_mul(right),
                    '/' => left.checked_div(right),
                    _ => left.checked_rem(right),
                };

                value.ok_or_else(|| {
                    let why = if matches!(operator, '/' | '%') && right == 0 {
                        "divides by zero"
                    } else {
                        "overflows"
                    };

                    format!("{self} {why}")
                })
            }
        }
    }
}

impl fmt::Display for Expression {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Number(value) => write!(formatter, "{value}"),
            Self::Variable(name) => formatter.write_str(name),
            Self::Binary(left, operator, right) => {
                write!(formatter, "({left} {operator} {right})")
            }
        }
    }
}

pub fn parse(source: &str) -> Result<Vec<Marker>, String> {
    source
        .lines()
        .zip(1..)
        .filter_map(|(text, line)| Some((text.split_once("// MARK:")?.1, line)))
        .map(|(text, line)| {
            let (text, expect) = match text.split_once("// EXPECT:") {
                Some((text, expect)) => (text.trim(), Some(expect.trim().to_owned())),
                None => (text.trim(), None),
            };

            Ok(Marker {
                line,
                condition: Parser::new(text)
                    .condition()
                    .map_err(|error| format!("line {line}: {error} in {text:?}"))?,
                text: text.to_owned(),
                expect,
            })
        })
        .collect()
}

struct Parser<'a> {
    rest: &'a str,
}

impl<'a> Parser<'a> {
    const fn new(text: &'a str) -> Self {
        Self { rest: text }
    }

    fn skip_space(&mut self) {
        self.rest = self.rest.trim_start();
    }

    fn eat(&mut self, token: &str) -> bool {
        self.skip_space();
        match self.rest.strip_prefix(token) {
            Some(rest) => {
                self.rest = rest;
                true
            }
            None => false,
        }
    }

    fn condition(&mut self) -> Result<Condition, String> {
        let mut comparisons = vec![self.comparison()?];
        while self.eat("&&") {
            comparisons.push(self.comparison()?);
        }

        self.skip_space();
        if !self.rest.is_empty() {
            return Err(format!("unexpected {:?}", self.rest));
        }

        Ok(Condition(comparisons))
    }

    fn comparison(&mut self) -> Result<Comparison, String> {
        let left = self.sum()?;
        let operator = [
            ("==", Operator::Equal),
            ("!=", Operator::NotEqual),
            ("<=", Operator::LessOrEqual),
            (">=", Operator::GreaterOrEqual),
            ("<", Operator::Less),
            (">", Operator::Greater),
        ]
        .into_iter()
        .find_map(|(token, operator)| self.eat(token).then_some(operator))
        .ok_or("a comparison needs an operator")?;

        Ok(Comparison {
            left,
            operator,
            right: self.sum()?,
        })
    }

    fn sum(&mut self) -> Result<Expression, String> {
        let mut left = self.product()?;
        loop {
            let operator = if self.eat("+") {
                '+'
            } else if self.eat("-") {
                '-'
            } else {
                return Ok(left);
            };

            left = Expression::Binary(Box::new(left), operator, Box::new(self.product()?));
        }
    }

    fn product(&mut self) -> Result<Expression, String> {
        let mut left = self.atom()?;
        loop {
            let operator = if self.eat("*") {
                '*'
            } else if self.eat("/") {
                '/'
            } else if self.eat("%") {
                '%'
            } else {
                return Ok(left);
            };

            left = Expression::Binary(Box::new(left), operator, Box::new(self.atom()?));
        }
    }

    fn atom(&mut self) -> Result<Expression, String> {
        if self.eat("(") {
            let inner = self.sum()?;
            if !self.eat(")") {
                return Err("a parenthesis is not closed".into());
            }

            return Ok(inner);
        }

        self.skip_space();
        let length = self
            .rest
            .find(|character: char| !character.is_ascii_alphanumeric() && character != '_')
            .unwrap_or(self.rest.len());
        let (token, rest) = self.rest.split_at(length);
        self.rest = rest;

        if token.is_empty() {
            return Err("an operand is missing".into());
        }

        if token.starts_with(|character: char| character.is_ascii_digit()) {
            return token
                .parse()
                .map(Expression::Number)
                .map_err(|_| format!("{token} is not a number"));
        }

        Ok(Expression::Variable(token.to_owned()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn evaluate(condition: &str, values: &[(&str, i128)]) -> Verdict {
        let markers = parse(&format!("x; // MARK: {condition}")).expect("a marker");
        let values = values
            .iter()
            .map(|&(name, value)| (name.to_owned(), value))
            .collect();

        markers[0].condition.evaluate(&values)
    }

    #[test]
    fn a_condition_without_a_value_is_unknown() {
        let unknown = |text: &str| Verdict::Unknown(text.to_owned());
        let (a, b) = ("a", "b");
        assert_eq!(
            evaluate("a / b == 0", &[(a, 1), (b, 0)]),
            unknown("(a / b) divides by zero")
        );
        assert_eq!(
            evaluate("a % b == 0", &[(a, 1), (b, 0)]),
            unknown("(a % b) divides by zero")
        );
        assert_eq!(
            evaluate("a * a * a == 0", &[(a, 1 << 50)]),
            unknown("((a * a) * a) overflows")
        );
        assert_eq!(evaluate("a == 1", &[]), unknown("a"));
        assert_eq!(
            evaluate("a / b == 2 && a % b == 1", &[(a, 7), (b, 3)]),
            Verdict::Holds
        );
        assert!(matches!(
            evaluate("a / b == 3", &[(a, 7), (b, 3)]),
            Verdict::Fails(_)
        ));
    }
}
