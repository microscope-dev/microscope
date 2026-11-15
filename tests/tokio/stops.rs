use microscope::{
    Backtrace, Evaluation, Expression, InspectedValue, ScalarValue, VariableState, VariableValue,
};

use crate::support::Scenario;

pub fn source(path: &str) -> String {
    format!("tests/fixtures/rust/tokio/{path}")
}

pub fn line(path: &str, marker: &str) -> u64 {
    crate::support::source_line(&source(path), marker)
}

pub async fn integer(scenario: &Scenario, text: &str) -> Option<i128> {
    let expression = Expression::parse(text).expect("an expression");
    let Evaluation::Value { value, .. } = scenario
        .operation(text, scenario.handle().evaluate(&expression))
        .await
    else {
        panic!("{text}: not a value");
    };

    match value.state {
        VariableState::Available {
            value: VariableValue::Scalar(ScalarValue::Signed(value)),
            ..
        } => Some(value),
        VariableState::Available {
            value: VariableValue::Scalar(ScalarValue::Unsigned(value)),
            ..
        } => Some(i128::try_from(value).expect("a small integer")),
        VariableState::Unavailable(_) => None,
        other => panic!("{text}: {other:?}"),
    }
}

pub async fn place(scenario: &Scenario) -> (String, u64) {
    let trace = backtrace(scenario).await;
    let frame = trace.frames.first().expect("a frame");

    (
        frame
            .function
            .as_ref()
            .map(|function| function.name.to_string())
            .unwrap_or_default(),
        frame.source.as_ref().map_or(0, |source| source.line.get()),
    )
}

pub async fn backtrace(scenario: &Scenario) -> Backtrace {
    scenario
        .operation("backtrace", scenario.handle().backtrace())
        .await
}

pub fn frames_to(trace: &Backtrace, last: &str) -> Vec<(String, u64)> {
    let mut frames = Vec::new();

    for frame in trace.frames.iter() {
        let name = frame
            .function
            .as_ref()
            .map(|function| function.name.to_string())
            .unwrap_or_default();
        let line = frame.source.as_ref().map_or(0, |source| source.line.get());
        let done = name == last;
        frames.push((name, line));
        if done {
            break;
        }
    }

    frames
}

pub async fn locals(scenario: &Scenario) -> Vec<(String, Option<i128>, VariableState)> {
    let snapshot = scenario
        .operation("variables", scenario.handle().variables())
        .await;
    snapshot
        .variables
        .iter()
        .map(|variable| {
            let number = match &variable.state {
                VariableState::Available {
                    value: VariableValue::Scalar(ScalarValue::Signed(value)),
                    ..
                } => Some(*value),
                VariableState::Available {
                    value: VariableValue::Scalar(ScalarValue::Unsigned(value)),
                    ..
                } => i128::try_from(*value).ok(),
                _ => None,
            };

            (variable.name.to_string(), number, variable.state.clone())
        })
        .collect()
}

pub async fn evaluated(scenario: &Scenario, text: &str) -> InspectedValue {
    let expression = Expression::parse(text).expect("an expression");
    match scenario
        .operation(text, scenario.handle().evaluate(&expression))
        .await
    {
        Evaluation::Value { value, .. } => value,
        other => panic!("{text}: {other:?}"),
    }
}
