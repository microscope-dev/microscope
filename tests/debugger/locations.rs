//! Values optimized code splits across places, recovers from its callers'
//! call sites, or points at without an address (`locations/main.c`). Every stage
//! of the program is on the stack when it stops in `locations_stop`.

use microscope::{
    EntryValueUnavailableReason, Evaluation, Expression, FloatValue, InspectedValue,
    OptimizedOutReason, VariableValue, VariableValueSource,
};

use super::*;

const VARIANTS: [&str; 3] = ["gcc-o2", "gcc-o2-nopie", "clang-o2"];

/// Launches a build of the fixture with `arguments` and stops in
/// `locations_stop`.
async fn stopped(variant: &str, arguments: &[&str]) -> Scenario {
    let mut scenario = Scenario::launch(&format!("locations-{variant}"));
    scenario.add_breakpoint("locations_stop").await;
    let reason = scenario
        .run_with_to_stop(LaunchOptions {
            arguments: arguments.iter().map(Into::into).collect(),
            ..LaunchOptions::default()
        })
        .await;

    assert!(
        matches!(reason, StopReason::Breakpoint { .. }),
        "{variant}: {reason:?}"
    );

    scenario
}

/// Selects the innermost frame that runs `function`.
async fn select(scenario: &Scenario, function: &str) {
    let trace = scenario
        .operation("backtrace", scenario.handle().backtrace())
        .await;

    let frame = trace
        .frames
        .iter()
        .find(|frame| frame.function.as_ref().map(|function| &*function.name) == Some(function))
        .unwrap_or_else(|| panic!("no frame runs {function}: {trace:#?}"));

    scenario
        .operation(function, scenario.handle().select_frame(frame.id))
        .await;
}

async fn inspect(scenario: &Scenario, text: &str) -> InspectedValue {
    scenario
        .operation(
            text,
            scenario
                .handle()
                .inspect(&Expression::parse(text).expect("expression")),
        )
        .await
}

async fn integer(scenario: &Scenario, text: &str) -> i128 {
    match inspect(scenario, text).await.state {
        VariableState::Available {
            value: VariableValue::Scalar(ScalarValue::Signed(value)),
            ..
        } => value,
        VariableState::Available {
            value: VariableValue::Scalar(ScalarValue::Unsigned(value)),
            ..
        } => value.try_into().expect("integer fits"),
        state => panic!("{text} is {state:?}"),
    }
}

async fn binary64(scenario: &Scenario, text: &str) -> f64 {
    match inspect(scenario, text).await.state {
        VariableState::Available {
            value: VariableValue::Scalar(ScalarValue::Floating(FloatValue::Binary64(bits))),
            ..
        } => f64::from_bits(bits),
        state => panic!("{text} is {state:?}"),
    }
}

async fn source(scenario: &Scenario, text: &str) -> VariableValueSource {
    match inspect(scenario, text).await.state {
        VariableState::Available { source, .. } => source,
        state => panic!("{text} is {state:?}"),
    }
}

async fn unavailable(scenario: &Scenario, text: &str) -> VariableUnavailableReason {
    match inspect(scenario, text).await.state {
        VariableState::Unavailable(reason) => reason,
        state => panic!("{text} is {state:?}"),
    }
}

/// The address an expression evaluates to, or its refusal's message.
async fn address(scenario: &Scenario, text: &str) -> Result<u64, String> {
    match scenario
        .handle()
        .evaluate(&Expression::parse(text).expect("expression"))
        .await
    {
        Ok(Evaluation::Value { value, .. }) => match value.state {
            VariableState::Available {
                value: VariableValue::Address(address),
                ..
            } => Ok(address.address.get()),
            state => panic!("{text} is {state:?}"),
        },
        Err(Error::Expression(error))
            if error.kind == microscope::ExpressionErrorKind::NotAnLvalue =>
        {
            Err(error.message)
        }
        other => panic!("{text}: {other:?}"),
    }
}

const fn is_register(source: &VariableValueSource) -> bool {
    matches!(source, VariableValueSource::Register(_))
}

#[tokio::test]
async fn a_record_split_across_registers_reads_and_places_each_member() {
    for variant in VARIANTS {
        let scenario = stopped(variant, &[]).await;
        select(&scenario, "split_record").await;
        assert_eq!(integer(&scenario, "pair.first").await, 11, "{variant}");
        assert_eq!(integer(&scenario, "pair.second").await, 21, "{variant}");
        assert_eq!(
            source(&scenario, "pair").await,
            VariableValueSource::Composite,
            "{variant}"
        );

        assert!(
            is_register(&source(&scenario, "pair.first").await),
            "{variant}"
        );

        assert!(
            is_register(&source(&scenario, "pair.second").await),
            "{variant}"
        );

        let whole = address(&scenario, "&pair").await.unwrap_err();
        assert!(whole.contains("several places"), "{variant}: {whole}");
        let member = address(&scenario, "&pair.first").await.unwrap_err();
        assert!(member.contains("register"), "{variant}: {member}");
        scenario.shutdown().await;
    }
}

#[tokio::test]
async fn a_wide_integer_in_two_registers_reads_whole() {
    for variant in ["gcc-o2", "gcc-o2-nopie"] {
        let scenario = stopped(variant, &[]).await;
        select(&scenario, "wide_value").await;
        assert_eq!(integer(&scenario, "seed").await, 21, "{variant}");
        assert_eq!(
            integer(&scenario, "value").await,
            (21_i128 << 64) | 63,
            "{variant}"
        );

        assert_eq!(
            source(&scenario, "value").await,
            VariableValueSource::Composite,
            "{variant}"
        );

        scenario.shutdown().await;
    }
}

#[tokio::test]
async fn members_of_a_partly_kept_record_are_judged_by_their_own_bits() {
    let scenario = stopped("clang-o2", &[]).await;

    // Both halves are on the stack, one after the other: the record is
    // simply in memory.
    select(&scenario, "split_point").await;
    assert!((binary64(&scenario, "point.x").await - 21.5).abs() < f64::EPSILON);
    assert!((binary64(&scenario, "point.y").await - 22.5).abs() < f64::EPSILON);
    let record = address(&scenario, "&point").await.expect("in memory");
    assert_eq!(address(&scenario, "&point.x").await, Ok(record));
    assert_eq!(address(&scenario, "&point.y").await, Ok(record + 8));

    // Clang keeps only `y`, in a register this frame's callee may change.
    select(&scenario, "wide_value").await;
    assert!(matches!(
        inspect(&scenario, "point").await.state,
        VariableState::Available {
            source: VariableValueSource::Composite,
            ..
        }
    ));

    assert_eq!(
        unavailable(&scenario, "point.x").await,
        VariableUnavailableReason::OptimizedOut(OptimizedOutReason::EmptyLocation)
    );

    assert!(matches!(
        unavailable(&scenario, "point.y").await,
        VariableUnavailableReason::RegisterNotSaved(register) if &*register == "xmm1"
    ));

    scenario.shutdown().await;
}

#[tokio::test]
async fn a_member_pointing_at_a_value_with_no_address_is_an_implicit_pointer() {
    for variant in VARIANTS {
        let scenario = stopped(variant, &[]).await;
        select(&scenario, "implicit_member").await;
        assert_eq!(integer(&scenario, "ref.count").await, 402, "{variant}");
        if variant.starts_with("gcc") {
            // `ref.pointer` points at `local`, which has no address, and GCC
            // keeps no value for `local` here either.
            assert!(matches!(
                inspect(&scenario, "ref.pointer").await.state,
                VariableState::Available {
                    value: VariableValue::ImplicitPointer,
                    source: VariableValueSource::ImplicitPointer,
                    ..
                }
            ));

            assert_eq!(
                unavailable(&scenario, "*ref.pointer").await,
                VariableUnavailableReason::OptimizedOut(OptimizedOutReason::NoLocation),
                "{variant}"
            );

            let refused = address(&scenario, "&ref.pointer").await.unwrap_err();
            assert!(refused.contains("referent"), "{variant}: {refused}");
        } else {
            assert_eq!(integer(&scenario, "local").await, 403);
            assert_eq!(
                unavailable(&scenario, "ref.pointer").await,
                VariableUnavailableReason::OptimizedOut(OptimizedOutReason::EmptyLocation)
            );
        }

        scenario.shutdown().await;
    }
}

#[tokio::test]
async fn parameters_held_only_on_entry_are_recovered_from_their_callers() {
    for variant in VARIANTS {
        let scenario = stopped(variant, &[]).await;
        let gcc = variant.starts_with("gcc");
        let expected: &[(&str, &str, i128)] = if gcc {
            &[
                ("entry_values", "first", 101),
                ("entry_values", "second", 102),
                ("chain_outer", "value", 203),
                ("chain_inner", "value", 203),
                // GCC's clone drops `unused`; the caller still computes it.
                ("removed_parameter", "unused", 201),
            ]
        } else {
            &[
                ("entry_values", "first", 101),
                ("entry_values", "second", 102),
                ("chain_inner", "value", 203),
                ("removed_parameter", "used", 100),
            ]
        };

        for &(function, name, value) in expected {
            select(&scenario, function).await;
            assert_eq!(
                integer(&scenario, name).await,
                value,
                "{variant}: {function} {name}"
            );

            assert_eq!(
                source(&scenario, name).await,
                VariableValueSource::Computed,
                "{variant}: {function} {name}"
            );
        }

        if gcc {
            // `main`'s caller is the C library's, which describes no calls.
            select(&scenario, "main").await;
            assert_eq!(
                unavailable(&scenario, "argv").await,
                VariableUnavailableReason::EntryValue(EntryValueUnavailableReason::NoCallSite),
                "{variant}"
            );
        }

        scenario.shutdown().await;
    }
}

#[tokio::test]
async fn entry_values_follow_the_one_possible_chain_of_tail_calls() {
    for variant in ["gcc-o2", "gcc-o2-nopie"] {
        let mut scenario = stopped(variant, &["tail"]).await;

        // `main` called `relay`, which jumped to `tail_target` passing its
        // own argument plus one: the only way `relay` reaches it.
        select(&scenario, "tail_target").await;
        assert_eq!(integer(&scenario, "value").await, 43, "{variant}");

        // `main` called `ping`, but `pong` may have jumped back into it
        // since, with another argument.
        assert!(matches!(
            scenario.resume_to_stop().await,
            StopReason::Breakpoint { .. }
        ));

        select(&scenario, "ping").await;
        assert_eq!(
            unavailable(&scenario, "count").await,
            VariableUnavailableReason::EntryValue(EntryValueUnavailableReason::TailCalls),
            "{variant}"
        );

        scenario.shutdown().await;
    }
}

#[tokio::test]
async fn an_entry_value_lost_further_out_says_whose_caller_lost_it() {
    use EntryValueUnavailableReason::{Caller, NoCallSite, UnknownTarget};
    use VariableUnavailableReason::EntryValue;

    for variant in VARIANTS {
        let scenario = stopped(variant, &["a", "b", "c"]).await;

        // `forwarded` was called by `forwarding`, which passed on what its
        // own caller, `main`, passed it through a pointer.
        let lost = if variant.starts_with("gcc") {
            // GCC describes no call through the pointer.
            select(&scenario, "forwarding").await;
            assert_eq!(
                unavailable(&scenario, "value").await,
                EntryValue(NoCallSite),
                "{variant}"
            );
            NoCallSite
        } else {
            // Clang's says the target was in a register the call clobbered.
            UnknownTarget
        };

        select(&scenario, "forwarded").await;
        assert_eq!(
            unavailable(&scenario, "value").await,
            EntryValue(Caller(Box::new(EntryValue(lost)))),
            "{variant}"
        );

        scenario.shutdown().await;
    }
}

#[tokio::test]
async fn a_library_function_recovers_what_the_program_passed_it() {
    for variant in VARIANTS {
        let scenario = stopped(variant, &["library"]).await;

        // `main` called `locations_in_library` through the procedure linkage
        // table: its call site names the library function only by symbol.
        select(&scenario, "locations_in_library").await;
        assert_eq!(integer(&scenario, "value").await, 52, "{variant}");
        assert_eq!(
            source(&scenario, "value").await,
            VariableValueSource::Computed,
            "{variant}"
        );

        if variant.starts_with("gcc") {
            let callback = scenario
                .operation(
                    "address",
                    scenario.handle().runtime_address("locations_stop"),
                )
                .await;

            let VariableState::Available {
                value: VariableValue::Address(stop),
                ..
            } = inspect(&scenario, "stop").await.state
            else {
                panic!("{variant}: stop has no value");
            };

            assert_eq!(stop.address, callback, "{variant}");
        } else {
            // Clang's call site does not say what it passed.
            assert_eq!(
                unavailable(&scenario, "stop").await,
                VariableUnavailableReason::EntryValue(EntryValueUnavailableReason::NoParameter),
                "{variant}"
            );
        }

        scenario.shutdown().await;
    }
}

#[tokio::test]
async fn tail_calls_another_module_may_have_taken_are_not_followed() {
    for variant in VARIANTS {
        let scenario = stopped(variant, &["interposed"]).await;

        // `main` called the library's `locations_interposing`, which jumped
        // to `locations_interposed` through its procedure linkage table. The
        // program defines one too, which took the jump and passed on 114
        // where the library's own would have passed 15.
        select(&scenario, "locations_interposed_target").await;
        assert_eq!(
            integer(&scenario, "locations_library_sink").await,
            114,
            "{variant}"
        );

        assert_eq!(
            unavailable(&scenario, "value").await,
            VariableUnavailableReason::EntryValue(EntryValueUnavailableReason::TailCalls),
            "{variant}"
        );

        scenario.shutdown().await;
    }
}

#[tokio::test]
async fn a_call_through_a_kept_pointer_says_where_it_went() {
    for variant in VARIANTS {
        let scenario = stopped(variant, &["pointer"]).await;

        // `main` keeps the pointer it calls `forwarding` through in a
        // register its callees preserve, which its call site names.
        select(&scenario, "forwarded").await;
        assert_eq!(integer(&scenario, "value").await, 2, "{variant}");
        if variant.starts_with("gcc") {
            select(&scenario, "forwarding").await;
            assert_eq!(integer(&scenario, "value").await, 2, "{variant}");
        }

        scenario.shutdown().await;
    }
}

/// Assigns `value` to `target` in the selected frame.
async fn assign(scenario: &Scenario, target: &str, value: &str) -> microscope::Result<Evaluation> {
    scenario
        .handle()
        .evaluate_with(
            &Expression::parse(&format!("{target} = {value}")).expect("expression"),
            microscope::EvaluationMode::Assign,
            microscope::InspectionLimits::default(),
        )
        .await
}

async fn watch(scenario: &Scenario, text: &str) -> microscope::Result<microscope::Watchpoint> {
    scenario
        .handle()
        .watch(
            &Expression::parse(text).expect("expression"),
            microscope::WatchAccess::Write,
        )
        .await
}

#[tokio::test]
async fn a_value_in_several_places_is_neither_changed_nor_watched() {
    for variant in ["gcc-o2", "gcc-o2-nopie"] {
        let scenario = stopped(variant, &[]).await;
        select(&scenario, "wide_value").await;
        let refused = assign(&scenario, "value", "1").await.unwrap_err();
        assert!(
            refused.to_string().contains("split across several places"),
            "{variant}: {refused}"
        );

        assert!(
            matches!(
                watch(&scenario, "value").await,
                Err(Error::WatchTargetNotInMemory(reason)) if reason.contains("several places")
            ),
            "{variant}"
        );

        scenario.shutdown().await;
    }

    // Clang describes `point` in two pieces of memory, one after the other:
    // one place, which can be changed.
    let scenario = stopped("clang-o2", &[]).await;
    select(&scenario, "split_point").await;
    assign(&scenario, "point.y", "30.5")
        .await
        .expect("assign to memory");

    assert!((binary64(&scenario, "point.y").await - 30.5).abs() < f64::EPSILON);
    scenario.shutdown().await;
}
