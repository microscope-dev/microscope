//! Pointers, aggregates, child pages, dereferences, and inspection budgets.

use super::*;
use microscope::ValueChildRelationship;

#[tokio::test]
async fn pointer_variables_are_available_and_explicitly_dereferenceable() {
    for fixture in ["variables-gcc-o0", "variables-clang-o0"] {
        let mut partial = Scenario::launch(fixture);
        partial.add_source_breakpoint("variables.c", 48).await;
        assert!(matches!(
            partial.run_to_stop().await,
            StopReason::Breakpoint { .. }
        ));
        let variables = partial
            .operation("partial variables", partial.handle().variables())
            .await;
        assert_eq!(variables.variables.len(), 2);
        assert_variable_value(&variables.variables[0], ScalarValue::Signed(42));
        let pointer = &variables.variables[1];
        assert!(matches!(
            pointer.type_info.as_ref().map(|info| &info.kind),
            Some(microscope::TypeKind::Pointer { .. })
        ));
        let reference = match &pointer.state {
            VariableState::Available {
                value,
                dereference: microscope::DereferenceState::Available(reference),
                ..
            } => {
                let microscope::VariableValue::Address(value) = value else {
                    panic!("pointer value was not an address: {value:?}");
                };

                assert_ne!(value.address.get(), 0);
                reference.clone()
            }
            state => panic!("pointer was not available for dereference: {state:?}"),
        };

        let dereferenced = partial
            .operation(
                "dereference pointer",
                partial.handle().dereference(reference.clone()),
            )
            .await;
        assert!(matches!(
            dereferenced.state,
            VariableState::Available {
                dereference: microscope::DereferenceState::NotApplicable,
                ..
            }
        ));
        assert_eq!(
            available_value(&dereferenced.state),
            &microscope::VariableValue::Scalar(ScalarValue::Signed(42))
        );
        let constrained = partial
            .operation(
                "bound scalar dereference bytes",
                partial.handle().dereference_with_limits(
                    reference,
                    microscope::InspectionLimits {
                        memory_bytes: 3,
                        ..microscope::InspectionLimits::default()
                    },
                ),
            )
            .await;
        assert!(matches!(
            constrained.state,
            VariableState::Unavailable(VariableUnavailableReason::InspectionLimit(
                microscope::InspectionExhaustion {
                    resource: microscope::InspectionLimit::MemoryBytes,
                    limit: 3,
                    used: 0,
                    requested: 4,
                }
            ))
        ));
        assert!(matches!(
            constrained.completion,
            microscope::InspectionCompletion::Truncated(microscope::InspectionExhaustion {
                resource: microscope::InspectionLimit::MemoryBytes,
                limit: 3,
                used: 0,
                requested: 4,
            })
        ));
        partial.shutdown().await;
    }
}

#[tokio::test]
async fn structural_inspection_dereferences_each_intermediate_pointer_only_when_needed() {
    for fixture in [
        "variables-gcc-o0",
        "variables-clang-o0",
        "variables-gcc-o2",
        "variables-clang-o2",
    ] {
        let mut scenario = Scenario::launch(fixture);
        scenario.add_source_breakpoint("variables.c", 68).await;
        assert!(matches!(
            scenario.run_to_stop().await,
            StopReason::Breakpoint { .. }
        ));

        let terminal_pointer = scenario
            .operation(
                "inspect terminal pointer member",
                scenario
                    .handle()
                    .inspect(&value_expression(&["recursive_pointer", "next"])),
            )
            .await;
        assert!(
            matches!(
                terminal_pointer.type_info.as_ref().map(|info| &info.kind),
                Some(microscope::TypeKind::Pointer { .. })
            ) && matches!(
                available_value(&terminal_pointer.state),
                microscope::VariableValue::Address(microscope::AddressValue { address, .. })
                    if address.get() != 0
            ),
            "{fixture}: terminal pointer was implicitly dereferenced: {terminal_pointer:?}"
        );

        for (components, expected) in [
            (&["pair", "first"][..], 20),
            (&["pair", "second"][..], 22),
            (&["structure_pointer", "first"][..], 20),
            (&["structure_pointer", "second"][..], 22),
            (&["recursive_pointer", "value"][..], 40),
            (&["recursive_pointer", "next", "value"][..], 41),
            (&["recursive_pointer", "next", "next", "value"][..], 42),
        ] {
            let value = scenario
                .operation(
                    "inspect pointer member chain",
                    scenario.handle().inspect(&value_expression(components)),
                )
                .await;
            assert_signed(&value.state, expected, fixture);
        }

        let unavailable = scenario
            .operation(
                "inspect through null intermediate pointer",
                scenario.handle().inspect(&value_expression(&[
                    "recursive_pointer",
                    "next",
                    "next",
                    "next",
                    "value",
                ])),
            )
            .await;
        assert_eq!(
            unavailable
                .type_info
                .as_ref()
                .map(|type_info| type_info.name.as_ref()),
            Some("int"),
            "{fixture}: terminal type was lost after the null hop: {unavailable:?}"
        );
        assert!(
            matches!(
                unavailable.state,
                VariableState::Unavailable(VariableUnavailableReason::ValueAccess(
                    microscope::ValueAccessUnavailableReason::NullPointer
                ))
            ),
            "{fixture}: {unavailable:?}"
        );

        scenario.shutdown().await;
    }
}

#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "one table-driven scenario keeps the cross-language pointer contract identical"
)]
async fn thin_pointers_and_references_dereference_across_the_language_matrix() {
    for (fixture, source, line, pointer, nested, null) in [
        (
            "variables-gcc-o0",
            "variables.c",
            68,
            "pointer",
            "pointer_pointer",
            "null_pointer",
        ),
        (
            "variables-clang-o0",
            "variables.c",
            68,
            "pointer",
            "pointer_pointer",
            "null_pointer",
        ),
        (
            "variables-gcc-o2",
            "variables.c",
            68,
            "pointer",
            "pointer_pointer",
            "null_pointer",
        ),
        (
            "variables-clang-o2",
            "variables.c",
            68,
            "pointer",
            "pointer_pointer",
            "null_pointer",
        ),
        (
            "variables-cpp-gcc-o0",
            "variables.cpp",
            50,
            "lvalue_reference",
            "pointer_pointer",
            "null_pointer",
        ),
        (
            "variables-cpp-clang-o0",
            "variables.cpp",
            50,
            "lvalue_reference",
            "pointer_pointer",
            "null_pointer",
        ),
        (
            "variables-cpp-gcc-o2",
            "variables.cpp",
            50,
            "lvalue_reference",
            "pointer_pointer",
            "null_pointer",
        ),
        (
            "variables-cpp-clang-o2",
            "variables.cpp",
            50,
            "lvalue_reference",
            "pointer_pointer",
            "null_pointer",
        ),
        (
            "variables-rust-o0",
            "variables.rs",
            67,
            "shared",
            "raw_pointer",
            "null_pointer",
        ),
        (
            "variables-rust-o2",
            "variables.rs",
            80,
            "shared",
            "raw_pointer",
            "null_pointer",
        ),
        (
            "variables-zig-o0",
            "variables.zig",
            73,
            "pointer",
            "pointer_pointer",
            "null_pointer",
        ),
        (
            "variables-zig-o2",
            "variables.zig",
            85,
            "pointer",
            "pointer_pointer",
            "null_pointer",
        ),
    ] {
        let mut scenario = Scenario::launch(fixture);
        scenario.add_source_breakpoint(source, line).await;
        assert!(matches!(
            scenario.run_to_stop().await,
            StopReason::Breakpoint { .. }
        ));

        let pointee = dereference_named(&scenario, pointer, 1).await;
        assert_signed(&pointee.state, 42, fixture);
        let nested_pointee = dereference_named(&scenario, nested, 2).await;
        assert_signed(&nested_pointee.state, 42, fixture);

        let null = scenario
            .operation("inspect null pointer", scenario.handle().variable(null))
            .await;
        assert!(
            matches!(
                null.state,
                VariableState::Available {
                    dereference: microscope::DereferenceState::Unavailable {
                        reason: microscope::DereferenceUnavailableReason::Null,
                        ..
                    },
                    ..
                }
            ) && matches!(
                available_value(&null.state),
                microscope::VariableValue::Address(microscope::AddressValue { address, .. })
                    if address.get() == 0
            ),
            "{fixture}: {null:?}"
        );
        if source == "variables.c" {
            assert_signed(
                &dereference_named(&scenario, "pointer_parameter", 1)
                    .await
                    .state,
                42,
                fixture,
            );
            assert_signed(
                &dereference_named(&scenario, "alias_pointer", 1).await.state,
                42,
                fixture,
            );
            assert_signed(
                &dereference_named(&scenario, "const_pointee", 1).await.state,
                42,
                fixture,
            );
            assert_signed(
                &dereference_named(&scenario, "const_pointer", 1).await.state,
                42,
                fixture,
            );
            let void_pointer = scenario
                .operation(
                    "inspect void pointer",
                    scenario.handle().variable("void_pointer"),
                )
                .await;
            assert!(
                matches!(
                    void_pointer.state,
                    VariableState::Available {
                        dereference: microscope::DereferenceState::Unavailable {
                            reason: microscope::DereferenceUnavailableReason::UnspecifiedPointee,
                            ..
                        },
                        ..
                    }
                ),
                "{fixture}: {void_pointer:?}"
            );
            let invalid = dereference_named(&scenario, "invalid_pointer", 1).await;
            assert!(
                matches!(
                    invalid.state,
                    VariableState::Unavailable(VariableUnavailableReason::MemoryInaccessible {
                        completed: 0,
                        ..
                    })
                ),
                "{fixture}: {invalid:?}"
            );
        }

        if source == "variables.cpp" {
            assert_signed(
                &dereference_named(&scenario, "pointer_parameter", 1)
                    .await
                    .state,
                42,
                fixture,
            );
            assert_signed(
                &dereference_named(&scenario, "reference_parameter", 1)
                    .await
                    .state,
                42,
                fixture,
            );
            assert_signed(
                &dereference_named(&scenario, "const_reference", 1)
                    .await
                    .state,
                42,
                fixture,
            );
            assert_signed(
                &dereference_named(&scenario, "alias_pointer", 1).await.state,
                42,
                fixture,
            );
            for (name, kind) in [
                ("lvalue_reference", microscope::ReferenceKind::Lvalue),
                ("rvalue_reference", microscope::ReferenceKind::Rvalue),
            ] {
                let variable = scenario
                    .operation(
                        "inspect C++ reference kind",
                        scenario.handle().variable(name),
                    )
                    .await;
                assert!(
                    matches!(
                        variable.type_info.as_ref().map(|info| &info.kind),
                        Some(microscope::TypeKind::Reference { kind: actual, .. }) if *actual == kind
                    ),
                    "{fixture}: {variable:?}"
                );
            }

            assert_signed(
                &dereference_named(&scenario, "rvalue_reference", 1)
                    .await
                    .state,
                42,
                fixture,
            );
            let member = scenario
                .operation(
                    "member through a reference",
                    scenario
                        .handle()
                        .inspect(&value_expression(&["structure_reference", "second"])),
                )
                .await;
            assert_signed(&member.state, 22, fixture);
            assert_signed(
                &dereference_named(&scenario, "reference_to_pointer", 2)
                    .await
                    .state,
                42,
                fixture,
            );
        }

        if source == "variables.rs" {
            for name in [
                "shared_parameter",
                "raw_parameter",
                "raw_const",
                "alias_pointer",
            ] {
                assert_signed(
                    &dereference_named(&scenario, name, 1).await.state,
                    42,
                    fixture,
                );
            }

            let slice = scenario
                .operation("inspect Rust slice", scenario.handle().variable("slice"))
                .await;

            // Optimized code keeps the slice's pointer and length in two
            // registers.
            assert_slice_values(&scenario, &slice, None, &[20, 22], fixture).await;
            if fixture == "variables-rust-o0" {
                let indexed = scenario
                    .operation(
                        "inspect one Rust slice element directly",
                        scenario
                            .handle()
                            .inspect(&parsed_value_expression("slice[1]")),
                    )
                    .await;
                assert_signed(&indexed.state, 22, fixture);
                let range_page = scenario
                    .operation(
                        "inspect one bounded Rust slice range",
                        evaluate_range(
                            scenario.handle(),
                            "slice[0..2]",
                            microscope::InspectionLimits::default(),
                        ),
                    )
                    .await;
                assert_eq!(range_page.children.len(), 2, "{fixture}: {range_page:?}");
                let out_of_bounds = scenario
                    .handle()
                    .inspect(&parsed_value_expression("slice[2]"))
                    .await;
                assert!(
                    matches!(
                        out_of_bounds,
                        Ok(microscope::InspectedValue {
                            state: VariableState::Unavailable(
                                microscope::VariableUnavailableReason::IndexOutOfBounds {
                                    index: 2,
                                    lower_bound: 0,
                                    count: 2,
                                }
                            ),
                            ..
                        })
                    ),
                    "{fixture}: {out_of_bounds:?}"
                );
            } else {
                // Optimized code keeps its pointer and length in registers,
                // which a location in pieces assembles.
                assert!(
                    matches!(
                        slice.state,
                        VariableState::Available {
                            source: microscope::VariableValueSource::Composite,
                            ..
                        }
                    ),
                    "{fixture}: {slice:?}"
                );
                assert_slice_values(&scenario, &slice, None, &[20, 22], fixture).await;
            }
        }

        if source == "variables.zig" {
            if fixture != "variables-zig-o2" {
                assert_signed(
                    &dereference_named(&scenario, "pointer_parameter", 1)
                        .await
                        .state,
                    42,
                    fixture,
                );
            }

            assert_signed(
                &dereference_named(&scenario, "const_pointer", 1).await.state,
                42,
                fixture,
            );
            if fixture != "variables-zig-o2" {
                assert_signed(
                    &dereference_named(&scenario, "alias_pointer", 1).await.state,
                    42,
                    fixture,
                );
            }

            assert_signed(
                &dereference_named(&scenario, "many_pointer", 1).await.state,
                42,
                fixture,
            );
        }

        scenario.shutdown().await;
    }

    let fixture = "variables-zig-o2";
    let mut parameter = Scenario::new(
        "optimized Zig pointer parameter",
        Scenario::fixture(fixture),
    );
    parameter.add_source_breakpoint("variables.zig", 58).await;
    assert!(matches!(
        parameter.run_to_stop().await,
        StopReason::Breakpoint { .. }
    ));
    assert_signed(
        &dereference_named(&parameter, "pointer_parameter", 1)
            .await
            .state,
        42,
        fixture,
    );
    parameter.shutdown().await;
}

#[tokio::test]
async fn record_pointees_are_bounded_values_and_unsupported_pointees_remain_printable() {
    for (fixture, source, line, record_pointers, unsupported_pointers) in [
        (
            "variables-gcc-o0",
            "variables.c",
            68,
            &["structure_pointer", "recursive_pointer"][..],
            &["function_pointer"][..],
        ),
        (
            "variables-cpp-gcc-o0",
            "variables.cpp",
            50,
            &["structure_pointer", "recursive_pointer"][..],
            &[][..],
        ),
        (
            "variables-rust-o0",
            "variables.rs",
            67,
            &["structure_pointer", "recursive_pointer"][..],
            &[][..],
        ),
        (
            "variables-zig-o0",
            "variables.zig",
            73,
            &["structure_pointer", "recursive_pointer"][..],
            &[][..],
        ),
    ] {
        let mut scenario = Scenario::launch(fixture);
        scenario.add_source_breakpoint(source, line).await;
        assert!(matches!(
            scenario.run_to_stop().await,
            StopReason::Breakpoint { .. }
        ));
        if fixture != "variables-zig-o0" {
            let pair = scenario
                .operation("inspect direct record", scenario.handle().variable("pair"))
                .await;
            record_page(&scenario, &pair.state, 2, &format!("{fixture} pair")).await;
        }

        for name in record_pointers {
            let value = dereference_named(&scenario, name, 1).await;
            record_page(&scenario, &value.state, 2, &format!("{fixture} {name}")).await;
        }

        for name in unsupported_pointers {
            let variable = scenario
                .operation(
                    "inspect unsupported pointee",
                    scenario.handle().variable(*name),
                )
                .await;
            assert!(
                matches!(
                    variable.state,
                    VariableState::Available {
                        dereference: microscope::DereferenceState::Unavailable {
                            reason: microscope::DereferenceUnavailableReason::UnsupportedPointee(_),
                            ..
                        },
                        ..
                    }
                ) && matches!(
                    available_value(&variable.state),
                    microscope::VariableValue::Address(_)
                ),
                "{fixture} {name}: {variable:?}"
            );
        }

        let array = scenario
            .operation(
                "dereference bounded array",
                scenario.handle().variable("array_pointer"),
            )
            .await;
        let microscope::VariableState::Available {
            dereference: microscope::DereferenceState::Available(reference),
            ..
        } = array.state
        else {
            panic!("{fixture} array pointer did not expose a dereference: {array:?}");
        };

        let value = scenario
            .operation(
                "read bounded array",
                scenario.handle().dereference(reference),
            )
            .await;
        assert_array_values(&scenario, &value, fixture).await;
        scenario.shutdown().await;
    }
}

#[tokio::test]
async fn c_records_cover_nesting_arrays_bit_fields_globals_and_optimization() {
    for (fixture, inspect_parameters) in [
        ("records-c-gcc-o0", true),
        ("records-c-clang-o0", true),
        ("records-c-gcc-o2", false),
        ("records-c-clang-o2", false),
    ] {
        let mut scenario = Scenario::launch(fixture);
        scenario.add_breakpoint("inspect_records").await;
        assert!(matches!(
            scenario.run_to_stop().await,
            StopReason::Breakpoint { .. }
        ));

        let global = scenario
            .operation(
                "inspect global record",
                scenario.handle().variable("global_record"),
            )
            .await;
        let global_page = record_page(&scenario, &global.state, 2, fixture).await;
        assert!(
            global_page
                .children
                .iter()
                .filter_map(|child| match &child.relationship {
                    ValueChildRelationship::Member(member) => Some(member),
                    _ => None,
                })
                .all(|member| member.declaration.is_some()),
            "{fixture}: record member declarations were not preserved: {global:?}"
        );
        let inner = named_child(&global_page, "inner");
        let inner_page = record_page(&scenario, &inner.state, 2, fixture).await;
        assert_signed(&named_child(&inner_page, "signed_value").state, -7, fixture);

        if inspect_parameters {
            let record = dereference_named(&scenario, "record", 1).await;
            record_page(&scenario, &record.state, 2, fixture).await;

            let bits = dereference_named(&scenario, "bits", 1).await;
            let bits_page = record_page(&scenario, &bits.state, 3, fixture).await;
            assert_signed(&named_child(&bits_page, "negative").state, -3, fixture);
            assert!(matches!(
                available_value(&named_child(&bits_page, "first").state),
                microscope::VariableValue::Scalar(ScalarValue::Unsigned(5))
            ));
            assert!(matches!(
                available_value(&named_child(&bits_page, "second").state),
                microscope::VariableValue::Scalar(ScalarValue::Unsigned(42))
            ));

            let records = dereference_named(&scenario, "records", 1).await;
            let microscope::VariableValue::Array { .. } = available_value(&records.state) else {
                panic!("{fixture}: pointer-to-array did not decode: {records:?}");
            };

            let records_page = child_page(&scenario, &records.state, 0, 2).await;
            assert_eq!(records_page.children.len(), 2, "{fixture}: {records:?}");
            let second_page =
                record_page(&scenario, &records_page.children[1].state, 2, fixture).await;
            let values = named_child(&second_page, "values");
            let microscope::VariableValue::Array { .. } = available_value(&values.state) else {
                panic!("{fixture}: nested array was not decoded: {records:?}");
            };

            let values_page = child_page(&scenario, &values.state, 0, 2).await;
            assert_signed(&values_page.children[1].state, 44, fixture);

            let flexible = dereference_named(&scenario, "flexible", 1).await;
            let flexible_page = record_page(&scenario, &flexible.state, 2, fixture).await;
            assert_signed(&named_child(&flexible_page, "count").state, 2, fixture);

            // A flexible array member's elements are reached by index.
            assert_eq!(
                named_child(&flexible_page, "values").state,
                VariableState::Unavailable(VariableUnavailableReason::ValueAccess(
                    microscope::ValueAccessUnavailableReason::UnknownLength
                )),
                "{fixture}"
            );

            let incomplete = scenario
                .operation(
                    "inspect incomplete record pointer",
                    scenario.handle().variable("incomplete"),
                )
                .await;
            assert!(matches!(
                incomplete.state,
                VariableState::Available {
                    dereference: microscope::DereferenceState::Unavailable {
                        reason: microscope::DereferenceUnavailableReason::UnsupportedPointee(_),
                        ..
                    },
                    ..
                }
            ));
        }

        let mut reason = scenario.resume_to_stop().await;
        for _ in 0..8 {
            if matches!(reason, StopReason::Breakpoint { .. }) {
                reason = scenario.resume_to_stop().await;
            } else {
                break;
            }
        }

        assert_eq!(reason, StopReason::Exited(ExitStatus::Code(0)));
        scenario.shutdown().await;
    }
}

#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "one scenario proves lazy record and array access plus the existing structural path across both C compilers"
)]
async fn structural_inspection_reads_a_small_field_without_materializing_a_large_record() {
    let fixture = "records-c-gcc-o0";
    let mut scenario = Scenario::launch(fixture);
    scenario.add_breakpoint("inspect_records").await;
    assert!(matches!(
        scenario.run_to_stop().await,
        StopReason::Breakpoint { .. }
    ));

    let whole_record = dereference_named(&scenario, "large", 1).await;
    assert!(
        matches!(
            whole_record.state,
            VariableState::Available {
                raw: None,
                value: microscope::VariableValue::Record,
                ..
            }
        ) && available_children(&whole_record.state).total() == 2,
        "{fixture}: the large record was eagerly read: {whole_record:?}"
    );
    let tail = child_page(&scenario, &whole_record.state, 1, 1).await;
    assert_eq!(tail.children.len(), 1, "{fixture}: {tail:?}");
    assert_signed(&tail.children[0].state, 73, fixture);
    let header = child_page(&scenario, &whole_record.state, 0, 1).await;
    let padding = &header.children[0];
    assert!(
        matches!(
            available_value(&padding.state),
            microscope::VariableValue::Array { .. }
        ) && available_children(&padding.state).total() == 2048,
        "{fixture}: large member did not remain lazy: {padding:?}"
    );
    let middle = child_page(&scenario, &padding.state, 1024, 256).await;
    assert_eq!(middle.children.len(), 256, "{fixture}: {middle:?}");
    assert!(middle.children.iter().all(|child| matches!(
        available_value(&child.state),
        microscope::VariableValue::Scalar(ScalarValue::Unsigned(0))
    )));
    assert!(matches!(
        &middle.children[255].relationship,
        ValueChildRelationship::ArrayElement {
            index: 1279,
            indices,
        } if indices.as_ref() == [1279]
    ));
    let huge = scenario
        .operation(
            "inspect a large array",
            scenario.handle().variable("huge_array"),
        )
        .await;
    assert!(
        matches!(
            huge.state,
            VariableState::Available {
                raw: None,
                value: microscope::VariableValue::Array { .. },
                ..
            }
        ) && available_children(&huge.state).total() == 1024 * 1024 + 1,
        "{fixture}: large array was rejected or read eagerly: {huge:?}"
    );
    let huge_tail = child_page(&scenario, &huge.state, 1024 * 1024, 1).await;
    assert!(matches!(
        available_value(&huge_tail.children[0].state),
        microscope::VariableValue::Scalar(ScalarValue::Unsigned(0))
    ));
    for expression in ["huge_array[0]", "huge_array[1048576]"] {
        let indexed = scenario
            .operation(
                "inspect one large-array element directly",
                scenario
                    .handle()
                    .inspect(&parsed_value_expression(expression)),
            )
            .await;
        assert!(
            matches!(
                available_value(&indexed.state),
                microscope::VariableValue::Scalar(ScalarValue::Unsigned(0))
            ),
            "{fixture}: {expression}: {indexed:?}"
        );
    }

    let nested_array_member = scenario
        .operation(
            "inspect through an explicitly dereferenced array",
            scenario
                .handle()
                .inspect(&parsed_value_expression("(*records)[1].values[1]")),
        )
        .await;
    assert_signed(&nested_array_member.state, 44, fixture);
    let matrix_element = scenario
        .operation(
            "inspect one multidimensional array element",
            scenario
                .handle()
                .inspect(&parsed_value_expression("matrix[1][2]")),
        )
        .await;
    assert_signed(&matrix_element.state, 6, fixture);
    let range_page = scenario
        .operation(
            "inspect one bounded array range",
            evaluate_range(
                scenario.handle(),
                "huge_array[3..7]",
                microscope::InspectionLimits::default(),
            ),
        )
        .await;
    assert_eq!(range_page.offset, 3, "{fixture}: {range_page:?}");
    assert_eq!(range_page.children.len(), 4, "{fixture}: {range_page:?}");
    assert!(range_page.children.iter().enumerate().all(|(relative, child)| {
        matches!(
            &child.relationship,
            ValueChildRelationship::ArrayElement { index, indices }
                if *index == 3 + u64::try_from(relative).expect("small index")
                    && indices.as_ref() == [i128::try_from(3 + relative).expect("small index")]
        )
    }));

    // The page is read at once, but each element reports its own address.
    let sources = range_page
        .children
        .iter()
        .map(|child| match &child.state {
            VariableState::Available {
                source: microscope::VariableValueSource::Memory(address),
                ..
            } => address.get(),
            other => panic!("{fixture}: element is not in memory: {other:?}"),
        })
        .collect::<Vec<_>>();
    assert!(
        sources.windows(2).all(|pair| pair[1] == pair[0] + 1),
        "{fixture}: element sources {sources:#x?}"
    );
    let empty_page = scenario
        .operation(
            "inspect one empty in-bounds range",
            evaluate_range(
                scenario.handle(),
                "huge_array[7..7]",
                microscope::InspectionLimits::default(),
            ),
        )
        .await;
    assert_eq!(empty_page.offset, 7, "{fixture}: {empty_page:?}");
    assert!(empty_page.children.is_empty(), "{fixture}: {empty_page:?}");
    for expression in [
        "huge_array[7..3]",
        "huge_array[0..257]",
        "huge_array[1048576..1048578]",
        "matrix[0..1]",
        "global_record[0..1]",
    ] {
        let result = evaluate_range(
            scenario.handle(),
            expression,
            microscope::InspectionLimits::default(),
        )
        .await;
        let expected = match expression {
            // A range is checked against the array's bounds as it runs.
            "huge_array[7..3]" | "huge_array[1048576..1048578]" => matches!(
                &result,
                Err(Error::Expression(error)) if error.kind == microscope::ExpressionErrorKind::Bounds
            ),
            "global_record[0..1]" => matches!(
                &result,
                Err(Error::Expression(error)) if error.kind == microscope::ExpressionErrorKind::Type
            ),
            _ => matches!(&result, Err(Error::InvalidValueRange(_))),
        };

        assert!(expected, "{fixture}: {expression}: {result:?}");
    }

    let out_of_bounds = scenario
        .handle()
        .inspect(&parsed_value_expression("huge_array[1048577]"))
        .await;
    assert!(
        matches!(
            &out_of_bounds,
            Err(microscope::Error::Expression(error))
                if error.kind == microscope::ExpressionErrorKind::Bounds
        ),
        "{fixture}: {out_of_bounds:?}"
    );

    // A pointer indexes as C indexes it: `records[0]` is `*records`.
    let pointer_index = scenario
        .handle()
        .inspect(&parsed_value_expression("records[0]"))
        .await;
    assert_eq!(
        pointer_index
            .as_ref()
            .ok()
            .and_then(|value| value.type_info.as_ref())
            .map(|info| info.name.as_ref()),
        Some("outer_record[2]"),
        "{fixture}: {pointer_index:?}"
    );
    for (components, expected) in [
        (&["record", "inner", "signed_value"][..], -7),
        (&["bits", "negative"][..], -3),
    ] {
        let value = scenario
            .operation(
                "inspect nested or bit-field member",
                scenario.handle().inspect(&value_expression(components)),
            )
            .await;
        assert_signed(&value.state, expected, fixture);
    }

    let unsigned_bit_field = scenario
        .operation(
            "inspect unsigned bit-field member",
            scenario
                .handle()
                .inspect(&value_expression(&["bits", "second"])),
        )
        .await;
    assert!(
        matches!(
            available_value(&unsigned_bit_field.state),
            microscope::VariableValue::Scalar(ScalarValue::Unsigned(42))
        ),
        "{fixture}: {unsigned_bit_field:?}"
    );

    let selected = scenario
        .operation(
            "inspect small field in large record",
            scenario
                .handle()
                .inspect(&value_expression(&["large", "small"])),
        )
        .await;
    assert_signed(&selected.state, 73, fixture);

    scenario.shutdown().await;
}

#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "one public scenario proves every budget resource and resumable partial pages"
)]
async fn inspection_budgets_report_typed_partial_results_at_each_public_boundary() {
    let fixture = "records-c-gcc-o0";
    let mut scenario = Scenario::launch(fixture);
    scenario.add_breakpoint("inspect_records").await;
    assert!(matches!(
        scenario.run_to_stop().await,
        StopReason::Breakpoint { .. }
    ));

    let limits = microscope::InspectionLimits {
        variables: 1,
        ..microscope::InspectionLimits::default()
    };

    let variables = scenario
        .operation(
            "truncate visible variables at the exact variable boundary",
            scenario.handle().variables_with_limits(limits),
        )
        .await;
    assert_eq!(variables.variables.len(), 1, "{variables:?}");
    assert!(matches!(
        variables.completion,
        microscope::InspectionCompletion::Truncated(microscope::InspectionExhaustion {
            resource: microscope::InspectionLimit::Variables,
            limit: 1,
            used: 1,
            requested: 1,
        })
    ));

    let node_limited_variables = scenario
        .operation(
            "reserve each visible variable and value node atomically",
            scenario
                .handle()
                .variables_with_limits(microscope::InspectionLimits {
                    value_nodes: 1,
                    ..microscope::InspectionLimits::default()
                }),
        )
        .await;
    assert_eq!(
        node_limited_variables.variables.len(),
        1,
        "{node_limited_variables:?}"
    );
    assert_eq!(
        node_limited_variables.usage.variables, 1,
        "{node_limited_variables:?}"
    );
    assert!(matches!(
        node_limited_variables.completion,
        microscope::InspectionCompletion::Truncated(microscope::InspectionExhaustion {
            resource: microscope::InspectionLimit::ValueNodes,
            limit: 1,
            used: 1,
            requested: 1,
        })
    ));

    let huge = scenario
        .operation(
            "obtain a stop-scoped large-array capability",
            scenario
                .handle()
                .inspect(&parsed_value_expression("huge_array")),
        )
        .await;
    let reference = available_children(&huge.state).clone();

    let range = scenario
        .operation(
            "share one value-node budget across range selection and its child page",
            evaluate_range(
                scenario.handle(),
                "huge_array[0..4]",
                microscope::InspectionLimits {
                    value_nodes: 3,
                    ..microscope::InspectionLimits::default()
                },
            ),
        )
        .await;

    // The selection and the page share the limit, which the page's
    // elements reach.
    assert_eq!(range.children.len(), 3, "{range:?}");
    assert!(matches!(
        range.completion,
        microscope::InspectionCompletion::Truncated(microscope::InspectionExhaustion {
            resource: microscope::InspectionLimit::ValueNodes,
            limit: 3,
            used: 3,
            requested: 1,
        })
    ));

    let node_limits = microscope::InspectionLimits {
        value_nodes: 2,
        ..microscope::InspectionLimits::default()
    };

    let nodes = scenario
        .operation(
            "truncate a child page by value nodes",
            scenario.handle().value_children_with_limits(
                reference.clone(),
                microscope::ValueChildQuery {
                    offset: 0,
                    limit: 4,
                },
                node_limits,
            ),
        )
        .await;
    assert_eq!(nodes.children.len(), 2, "{nodes:?}");
    assert!(matches!(
        nodes.completion,
        microscope::InspectionCompletion::Truncated(microscope::InspectionExhaustion {
            resource: microscope::InspectionLimit::ValueNodes,
            limit: 2,
            used: 2,
            requested: 1,
        })
    ));
    let resumed = scenario
        .operation(
            "resume after a truncated child prefix with a fresh budget",
            scenario.handle().value_children_with_limits(
                reference.clone(),
                microscope::ValueChildQuery {
                    offset: 2,
                    limit: 2,
                },
                node_limits,
            ),
        )
        .await;
    assert!(matches!(
        resumed.children.as_ref(),
        [first, second]
            if matches!(
                &first.relationship,
                ValueChildRelationship::ArrayElement { index: 2, .. }
            ) && matches!(
                &second.relationship,
                ValueChildRelationship::ArrayElement { index: 3, .. }
            )
    ));

    let byte_limits = microscope::InspectionLimits {
        memory_bytes: 2,
        ..microscope::InspectionLimits::default()
    };

    let bytes = scenario
        .operation(
            "truncate a child page by requested memory bytes",
            scenario.handle().value_children_with_limits(
                reference.clone(),
                microscope::ValueChildQuery {
                    offset: 0,
                    limit: 4,
                },
                byte_limits,
            ),
        )
        .await;
    assert_eq!(bytes.children.len(), 2, "{bytes:?}");
    assert_eq!(bytes.usage.memory_bytes, 2, "{bytes:?}");
    assert!(matches!(
        bytes.completion,
        microscope::InspectionCompletion::Truncated(microscope::InspectionExhaustion {
            resource: microscope::InspectionLimit::MemoryBytes,
            limit: 2,
            used: 2,
            requested: 1,
        })
    ));

    let read_limits = microscope::InspectionLimits {
        memory_reads: 1,
        memory_bytes: 3,
        ..microscope::InspectionLimits::default()
    };

    let reads = scenario
        .operation(
            "truncate a child page by logical memory reads",
            scenario.handle().value_children_with_limits(
                reference,
                microscope::ValueChildQuery {
                    offset: 0,
                    limit: 4,
                },
                read_limits,
            ),
        )
        .await;
    assert_eq!(reads.children.len(), 1, "{reads:?}");
    assert_eq!(reads.usage.memory_reads, 1, "{reads:?}");
    assert!(matches!(
        reads.completion,
        microscope::InspectionCompletion::Truncated(microscope::InspectionExhaustion {
            resource: microscope::InspectionLimit::MemoryReads,
            limit: 1,
            used: 1,
            requested: 1,
        })
    ));

    let work_limits = microscope::InspectionLimits {
        expression_work: 1,
        ..microscope::InspectionLimits::default()
    };

    let work = scenario
        .operation(
            "truncate evaluation by expression work",
            scenario
                .handle()
                .inspect_with_limits(&parsed_value_expression("huge_array[0]"), work_limits),
        )
        .await;
    assert!(
        matches!(
            work.completion,
            microscope::InspectionCompletion::Truncated(microscope::InspectionExhaustion {
                resource: microscope::InspectionLimit::ExpressionWork,
                limit: 1,
                ..
            })
        ),
        "{work:?}"
    );

    let invalid = microscope::InspectionLimits {
        memory_reads: 0,
        ..microscope::InspectionLimits::default()
    };

    assert!(matches!(
        scenario
            .handle()
            .inspect_with_limits(&parsed_value_expression("huge_array"), invalid)
            .await,
        Err(microscope::Error::InvalidInspectionLimit {
            resource: microscope::InspectionLimit::MemoryReads,
            value: 0,
            maximum: 4_096,
        })
    ));

    scenario.shutdown().await;
}

#[tokio::test]
async fn cpp_records_cover_multiple_and_virtual_base_metadata() {
    for fixture in [
        "records-cpp-gcc-o0",
        "records-cpp-clang-o0",
        "records-cpp-gcc-o2",
        "records-cpp-clang-o2",
    ] {
        let mut scenario = Scenario::launch(fixture);
        scenario.add_breakpoint("inspect_records").await;
        assert!(matches!(
            scenario.run_to_stop().await,
            StopReason::Breakpoint { .. }
        ));
        let derived = dereference_named(&scenario, "derived", 1).await;
        let derived_page = record_page(&scenario, &derived.state, 3, fixture).await;
        let bases = derived_page
            .children
            .iter()
            .filter(|child| matches!(child.relationship, ValueChildRelationship::Base(_)))
            .collect::<Vec<_>>();
        assert_eq!(bases.len(), 2, "{fixture}: {derived:?}");
        assert!(
            derived_page
                .children
                .iter()
                .filter_map(|child| match &child.relationship {
                    ValueChildRelationship::Member(member) => Some(member),
                    _ => None,
                })
                .all(|member| member.name.as_deref() != Some("static_value")),
            "{fixture}: static member appeared in instance: {derived:?}"
        );
        assert_signed(&named_child(&derived_page, "own").state, 22, fixture);
        let left_page = record_page(&scenario, &bases[0].state, 1, fixture).await;
        assert_signed(&named_child(&left_page, "left").state, 9, fixture);
        let right_page = record_page(&scenario, &bases[1].state, 1, fixture).await;
        assert_signed(&named_child(&right_page, "right").state, 11, fixture);

        let virtual_derived = dereference_named(&scenario, "virtual_derived", 1).await;
        let virtual_page = record_page(&scenario, &virtual_derived.state, 1, fixture).await;
        let bases = virtual_page
            .children
            .iter()
            .filter(|child| matches!(child.relationship, ValueChildRelationship::Base(_)))
            .collect::<Vec<_>>();
        assert_eq!(bases.len(), 1, "{fixture}: {virtual_derived:?}");
        assert!(matches!(
            bases[0].relationship,
            ValueChildRelationship::Base(microscope::BaseClass {
                virtuality: microscope::BaseClassVirtuality::Virtual,
                ..
            })
        ));
        let virtual_base_page = record_page(&scenario, &bases[0].state, 1, fixture).await;
        assert_signed(
            &named_child(&virtual_base_page, "virtual_value").state,
            22,
            fixture,
        );

        let diamond = dereference_named(&scenario, "diamond", 1).await;
        let diamond_page = record_page(&scenario, &diamond.state, 2, fixture).await;
        let bases = diamond_page
            .children
            .iter()
            .filter(|child| matches!(child.relationship, ValueChildRelationship::Base(_)))
            .collect::<Vec<_>>();
        assert_eq!(bases.len(), 2, "{fixture}: {diamond:?}");
        for branch in &bases {
            let branch_page = record_page(&scenario, &branch.state, 1, fixture).await;
            let root = branch_page
                .children
                .iter()
                .find(|child| matches!(child.relationship, ValueChildRelationship::Base(_)))
                .unwrap_or_else(|| {
                    panic!("{fixture}: diamond branch had no base: {branch_page:?}")
                });
            assert!(
                matches!(
                    available_value(&root.state),
                    microscope::VariableValue::Record
                ),
                "{fixture}: virtual root was not lazy-expandable: {root:?}"
            );
        }

        assert_eq!(
            scenario.resume_to_stop().await,
            StopReason::Exited(ExitStatus::Code(0))
        );
        scenario.shutdown().await;
    }
}

#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "one matrix verifies equivalent record, array, slice, and optimized behavior across three producers"
)]
async fn rust_zig_and_go_records_cover_nested_arrays_slices_and_optimized_metadata() {
    for (fixture, function, inspect_values) in [
        ("records-rust-o0", "inspect_records", true),
        ("records-rust-o2", "inspect_records", false),
        ("records-zig-o0", "records.inspectRecords", true),
        ("records-zig-o2", "records.inspectRecords", false),
    ] {
        let mut scenario = Scenario::launch(fixture);
        if fixture.contains("zig") {
            scenario.add_source_breakpoint("records.zig", 28).await;
        } else {
            scenario.add_breakpoint(function).await;
        }

        assert!(matches!(
            scenario.run_to_stop().await,
            StopReason::Breakpoint { .. }
        ));
        for name in ["record", "records"] {
            if !inspect_values {
                let variable = scenario
                    .operation(
                        "inspect optimized record metadata",
                        scenario.handle().variable(name),
                    )
                    .await;
                assert!(
                    variable.type_info.is_some(),
                    "{fixture} {name}: {variable:?}"
                );
                assert!(!matches!(variable.state, VariableState::Malformed(_)));
                continue;
            }

            let value = dereference_named(&scenario, name, 1).await;
            match available_value(&value.state) {
                microscope::VariableValue::Record => {
                    record_page(&scenario, &value.state, 2, &format!("{fixture} {name}")).await;
                }
                microscope::VariableValue::Array { .. } => {
                    let page = child_page(&scenario, &value.state, 0, 2).await;
                    assert_eq!(page.children.len(), 2, "{fixture}: {value:?}");
                    assert!(matches!(
                        available_value(&page.children[0].state),
                        microscope::VariableValue::Record
                    ));
                }
                other => panic!("{fixture} {name}: unexpected value {other:?}"),
            }
        }

        if inspect_values {
            let slice = scenario
                .operation("inspect record slice", scenario.handle().variable("slice"))
                .await;
            let microscope::VariableValue::Slice { length: 2, .. } = available_value(&slice.state)
            else {
                panic!("{fixture}: slice did not decode: {slice:?}");
            };

            let slice_page = child_page(&scenario, &slice.state, 0, 2).await;
            assert_eq!(slice_page.children.len(), 2, "{fixture}: {slice:?}");
            assert!(matches!(
                available_value(&slice_page.children[0].state),
                microscope::VariableValue::Record
            ));
            assert_record_members(&scenario, "inner.signed_value", fixture).await;
            if fixture.starts_with("records-zig-") {
                let packed = dereference_named(&scenario, "packed_record", 1).await;
                let packed_page = record_page(&scenario, &packed.state, 1, fixture).await;
                assert_eq!(packed_page.children.len(), 1, "{packed:?}");
                assert_eq!(
                    match &packed_page.children[0].relationship {
                        ValueChildRelationship::Member(member) => member.name.as_deref(),
                        _ => None,
                    },
                    Some("bits"),
                    "{packed:?}"
                );
            }
        }

        let mut reason = scenario.resume_to_stop().await;
        for _ in 0..8 {
            if matches!(reason, StopReason::Breakpoint { .. }) {
                reason = scenario.resume_to_stop().await;
            } else {
                break;
            }
        }

        assert_eq!(reason, StopReason::Exited(ExitStatus::Code(0)));
        scenario.shutdown().await;
    }

    for (fixture, inspect_values) in [("records-go-o0", true), ("records-go-o2", false)] {
        let mut scenario = Scenario::launch(fixture);
        scenario.add_source_breakpoint("main.go", 22).await;
        run_go_to_breakpoint(&mut scenario, fixture).await;
        if inspect_values {
            let record = dereference_named(&scenario, "record", 1).await;
            record_page(&scenario, &record.state, 2, fixture).await;
            let record_page = record_page(&scenario, &record.state, 2, fixture).await;
            assert!(
                record_page.children.iter().any(|child| matches!(
                    &child.relationship,
                    ValueChildRelationship::Member(member) if member.embedded
                )),
                "{fixture}: Go embedded-field metadata was lost: {record:?}"
            );
            let records = dereference_named(&scenario, "records", 1).await;
            assert!(matches!(
                available_value(&records.state),
                microscope::VariableValue::Array { .. }
            ));
            assert_eq!(available_children(&records.state).total(), 2);
            let slice = scenario
                .operation(
                    "inspect Go record slice",
                    scenario.handle().variable("slice"),
                )
                .await;
            assert!(matches!(
                available_value(&slice.state),
                microscope::VariableValue::Slice { length: 2, .. }
            ));
            assert_record_members(&scenario, "inner.signedValue", fixture).await;
        } else {
            let variable = scenario
                .operation(
                    "inspect optimized Go global record",
                    scenario.handle().variable("main.globalRecord"),
                )
                .await;
            record_page(&scenario, &variable.state, 2, fixture).await;
        }

        resume_go_to_exit(&mut scenario, fixture).await;
        assert_eq!(scenario.shutdown().await, Some(ExitStatus::Code(0)));
    }
}

/// Reads one member through each of the record fixtures' parameters: the
/// record, the array of records, and the slice of it.
async fn assert_record_members(scenario: &Scenario, signed_member: &str, fixture: &str) {
    for (expression, expected) in [
        (format!("(*record).{signed_member}"), -7),
        ("(*records)[1].values[1]".to_owned(), 44),
        ("slice[0].values[0]".to_owned(), 20),
    ] {
        let value = scenario
            .operation(
                &expression,
                scenario
                    .handle()
                    .inspect(&parsed_value_expression(&expression)),
            )
            .await;
        assert_signed(&value.state, expected, &format!("{fixture}: {expression}"));
    }
}

#[tokio::test]
async fn c_enums_and_raw_unions_preserve_values_names_aliases_and_interpretations() {
    for fixture in [
        "enums-c-gcc-o0",
        "enums-c-clang-o0",
        "enums-c-gcc-o2",
        "enums-c-clang-o2",
    ] {
        let mut scenario = Scenario::launch(fixture);
        scenario.add_breakpoint("inspect_enums").await;
        assert!(matches!(
            scenario.run_to_stop().await,
            StopReason::Breakpoint { .. }
        ));

        for (name, expected, expected_names) in [
            (
                "signed_value",
                microscope::IntegerValue::Signed(-3),
                &["SIGNED_NEGATIVE"][..],
            ),
            (
                "zero_alias",
                microscope::IntegerValue::Signed(0),
                &["SIGNED_ZERO", "SIGNED_ZERO_ALIAS"][..],
            ),
            // Single bits are flags, and a value is the ones it sets.
            (
                "flags",
                microscope::IntegerValue::Unsigned(3),
                &["FLAG_READ", "FLAG_WRITE"][..],
            ),
            (
                "byte_value",
                microscope::IntegerValue::Unsigned(255),
                &["BYTE_MAX"][..],
            ),
            ("sequence", microscope::IntegerValue::Unsigned(3), &[][..]),
        ] {
            let inspected = dereference_named(&scenario, name, 1).await;
            let microscope::VariableValue::Enumeration { value, matches } =
                available_value(&inspected.state)
            else {
                panic!("{fixture} {name}: value was not an enumeration: {inspected:?}");
            };

            assert_eq!(*value, expected, "{fixture} {name}: {inspected:?}");
            assert_eq!(
                matches
                    .iter()
                    .map(|enumerator| enumerator.name.as_ref())
                    .collect::<Vec<_>>(),
                expected_names,
                "{fixture} {name}: {inspected:?}"
            );
        }

        let raw = dereference_named(&scenario, "raw", 1).await;
        let microscope::VariableValue::Union = available_value(&raw.state) else {
            panic!("{fixture}: raw value was not a union: {raw:?}");
        };

        let page = record_page(&scenario, &raw.state, 2, fixture).await;
        assert_eq!(
            page.children
                .iter()
                .filter_map(|child| match &child.relationship {
                    ValueChildRelationship::Member(member) => member.name.as_deref(),
                    _ => None,
                })
                .collect::<Vec<_>>(),
            ["integer", "floating"],
            "{fixture}: {raw:?}"
        );
        assert_signed(&named_child(&page, "integer").state, 42, fixture);

        let integer = scenario
            .operation(
                "inspect a union interpretation",
                scenario
                    .handle()
                    .inspect(&value_expression(&["raw", "integer"])),
            )
            .await;
        assert_signed(&integer.state, 42, fixture);

        assert_eq!(
            scenario.resume_to_stop().await,
            StopReason::Exited(ExitStatus::Code(0))
        );
        scenario.shutdown().await;
    }
}

/// Rust's enums decode as enumerations or as variants with an active
/// member, never as empty records, and the unit type, a base type of no
/// bytes, decodes as an empty structure does.
#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "one stop per build checks every kind of Rust enum the fixture holds"
)]
async fn rust_enums_decode_their_variants_and_unit_payloads() {
    for fixture in ["enums-rust-o0", "enums-rust-o2"] {
        let mut scenario = Scenario::launch(fixture);
        scenario.add_breakpoint("inspect_enum").await;
        assert!(matches!(
            scenario.run_to_stop().await,
            StopReason::Breakpoint { .. }
        ));
        let fieldless = dereference_named(&scenario, "fieldless", 1).await;
        assert!(
            matches!(
                available_value(&fieldless.state),
                microscope::VariableValue::Enumeration {
                    value: microscope::IntegerValue::Signed(-3),
                    matches,
                } if matches.len() == 1 && matches[0].name.as_ref() == "Negative"
            ),
            "{fieldless:?}"
        );
        let value = dereference_named(&scenario, "value", 1).await;
        assert!(
            matches!(
                available_value(&value.state),
                microscope::VariableValue::Variant {
                    active: Some(active),
                    ..
                } if active.members.first().and_then(|member| member.name.as_deref())
                    == Some("Integer")
            ),
            "{fixture}: {value:?}"
        );
        let wide = dereference_named(&scenario, "wide", 1).await;
        assert!(
            matches!(
                available_value(&wide.state),
                microscope::VariableValue::Enumeration {
                    value: microscope::IntegerValue::Unsigned(value),
                    matches,
                } if *value == (1_u128 << 100) + 9
                    && matches.len() == 1
                    && matches[0].name.as_ref() == "Huge"
            ),
            "{fixture}: {wide:?}"
        );
        let optional = dereference_named(&scenario, "optional", 1).await;
        assert!(
            matches!(
                available_value(&optional.state),
                microscope::VariableValue::Variant {
                    active: Some(active),
                    ..
                } if active.members.first().and_then(|member| member.name.as_deref())
                    == Some("Some")
            ),
            "{fixture}: {optional:?}"
        );
        let empty = dereference_named(&scenario, "empty", 1).await;
        assert!(
            matches!(
                available_value(&empty.state),
                microscope::VariableValue::Variant {
                    active: Some(active),
                    ..
                } if active.members.first().and_then(|member| member.name.as_deref())
                    == Some("None")
            ),
            "{fixture}: {empty:?}"
        );
        for (name, variant) in [("done", "Ok"), ("failed", "Err")] {
            let value = dereference_named(&scenario, name, 1).await;
            let page = record_page(&scenario, &value.state, 1, fixture).await;
            let active = named_child(&page, variant);
            let payload_page = record_page(&scenario, &active.state, 1, fixture).await;
            let payload = named_child(&payload_page, "__0");
            let decoded = match (name, available_value(&payload.state)) {
                ("done", microscope::VariableValue::Record) => {
                    record_page(&scenario, &payload.state, 0, fixture)
                        .await
                        .children
                        .is_empty()
                }
                ("failed", microscope::VariableValue::Scalar(ScalarValue::Unsigned(5))) => true,
                _ => false,
            };

            assert!(decoded, "{fixture} {name}: {payload:?}");
        }

        if fixture == "enums-rust-o0" {
            let microscope::VariableValue::Variant {
                discriminant,
                active: Some(active),
            } = available_value(&value.state)
            else {
                panic!("payload enum did not decode as an active variant: {value:?}");
            };

            assert_eq!(
                *discriminant,
                Some(microscope::IntegerValue::Unsigned(1)),
                "{value:?}"
            );
            assert_eq!(active.members.len(), 1, "{value:?}");
            assert_eq!(
                active.members[0].name.as_deref(),
                Some("Integer"),
                "{value:?}"
            );
            let variant_page = record_page(&scenario, &value.state, 1, fixture).await;
            let integer = named_child(&variant_page, "Integer");
            let payload_page = record_page(&scenario, &integer.state, 1, fixture).await;
            let payload = named_child(&payload_page, "__0");
            assert!(
                matches!(
                    available_value(&payload.state),
                    microscope::VariableValue::Scalar(ScalarValue::Unsigned(42))
                ),
                "{value:?}"
            );

            let selected_payload = scenario
                .operation(
                    "select active Rust enum payload",
                    scenario
                        .handle()
                        .inspect(&value_expression(&["value", "Integer", "__0"])),
                )
                .await;
            assert!(
                matches!(
                    available_value(&selected_payload.state),
                    microscope::VariableValue::Scalar(ScalarValue::Unsigned(42))
                ),
                "{selected_payload:?}"
            );
            let inactive_payload = scenario
                .operation(
                    "reject inactive Rust enum payload",
                    scenario
                        .handle()
                        .inspect(&value_expression(&["value", "Unit"])),
                )
                .await;
            assert!(
                matches!(
                    inactive_payload.state,
                    VariableState::Unavailable(microscope::VariableUnavailableReason::ValueAccess(
                        microscope::ValueAccessUnavailableReason::InactiveVariant(Some(_))
                    ))
                ),
                "{inactive_payload:?}"
            );
        }

        assert_eq!(
            scenario.resume_to_stop().await,
            StopReason::Exited(ExitStatus::Code(0))
        );
        scenario.shutdown().await;
    }
}

#[tokio::test]
async fn go_named_integer_constants_reconstruct_symbolic_values() {
    for fixture in ["enums-go-o0", "enums-go-o2"] {
        let mut scenario = Scenario::launch(fixture);
        scenario.add_breakpoint("main.inspectEnums").await;
        run_go_to_breakpoint(&mut scenario, fixture).await;

        for (name, expected, expected_names) in [
            (
                "negative",
                microscope::IntegerValue::Signed(-3),
                &["main.StateNegative"][..],
            ),
            (
                "alias",
                microscope::IntegerValue::Signed(0),
                &["main.StateZero", "main.StateAlias"][..],
            ),
        ] {
            let inspected = dereference_named(&scenario, name, 1).await;
            let microscope::VariableValue::Enumeration { value, matches } =
                available_value(&inspected.state)
            else {
                panic!("{fixture} {name}: value was not symbolic: {inspected:?}");
            };

            assert_eq!(*value, expected, "{fixture} {name}: {inspected:?}");
            assert_eq!(
                matches
                    .iter()
                    .map(|enumerator| enumerator.name.as_ref())
                    .collect::<Vec<_>>(),
                expected_names,
                "{fixture} {name}: {inspected:?}"
            );
        }

        // A value no constant names is a number, not a nameless symbol.
        let unknown = dereference_named(&scenario, "unknown", 1).await;
        assert_eq!(
            available_value(&unknown.state),
            &microscope::VariableValue::Scalar(ScalarValue::Signed(5)),
            "{fixture}"
        );

        resume_go_to_exit(&mut scenario, fixture).await;
        assert_eq!(scenario.shutdown().await, Some(ExitStatus::Code(0)));
    }
}

#[tokio::test]
async fn cpp_scoped_enums_and_unions_preserve_language_semantics() {
    for fixture in [
        "enums-cpp-gcc-o0",
        "enums-cpp-clang-o0",
        "enums-cpp-gcc-o2",
        "enums-cpp-clang-o2",
    ] {
        let mut scenario = Scenario::launch(fixture);
        scenario.add_breakpoint("inspect_enums").await;
        assert!(matches!(
            scenario.run_to_stop().await,
            StopReason::Breakpoint { .. }
        ));

        for (name, expected, expected_names) in [
            (
                "state",
                microscope::IntegerValue::Signed(-3),
                &["Negative"][..],
            ),
            (
                "alias",
                microscope::IntegerValue::Signed(0),
                &["Zero", "Alias"][..],
            ),
        ] {
            let inspected = dereference_named(&scenario, name, 1).await;
            let microscope::VariableValue::Enumeration { value, matches } =
                available_value(&inspected.state)
            else {
                panic!("{fixture} {name}: value was not an enumeration: {inspected:?}");
            };

            assert_eq!(*value, expected, "{fixture} {name}: {inspected:?}");
            assert_eq!(
                matches
                    .iter()
                    .map(|enumerator| enumerator.name.as_ref())
                    .collect::<Vec<_>>(),
                expected_names,
                "{fixture} {name}: {inspected:?}"
            );
        }

        let raw = dereference_named(&scenario, "raw", 1).await;
        assert!(
            matches!(
                available_value(&raw.state),
                microscope::VariableValue::Union
            ) && available_children(&raw.state).total() == 2,
            "{fixture}: {raw:?}"
        );

        assert_eq!(
            scenario.resume_to_stop().await,
            StopReason::Exited(ExitStatus::Code(0))
        );
        scenario.shutdown().await;
    }
}

#[tokio::test]
async fn zig_enums_tagged_unions_and_bare_unions_decode_without_guessing() {
    for fixture in ["enums-zig-o0", "enums-zig-o2"] {
        let mut scenario = Scenario::launch(fixture);
        scenario
            .add_source_breakpoint("tests/fixtures/zig/enums.zig", 27)
            .await;
        assert!(matches!(
            scenario.run_to_stop().await,
            StopReason::Breakpoint { .. }
        ));

        let state = dereference_named(&scenario, "state", 1).await;
        let VariableState::Available {
            value: state_graph, ..
        } = &state.state
        else {
            panic!("{fixture}: state was unavailable: {state:?}");
        };

        assert!(
            matches!(
                state_graph,
                microscope::VariableValue::Enumeration {
                    value: microscope::IntegerValue::Signed(-3),
                    matches,
                } if matches.len() == 1 && matches[0].name.as_ref() == "negative"
            ),
            "{fixture}: {state:?}"
        );
        let tagged = dereference_named(&scenario, "tagged", 1).await;
        let VariableState::Available { value: graph, .. } = &tagged.state else {
            panic!("{fixture}: tagged union was unavailable: {tagged:?}");
        };

        let microscope::VariableValue::Variant {
            active: Some(active),
            ..
        } = graph
        else {
            panic!("{fixture}: tagged union did not select an arm: {tagged:?}");
        };

        let integer = active
            .members
            .iter()
            .find(|member| member.name.as_deref() == Some("integer"))
            .unwrap_or_else(|| panic!("{fixture}: integer arm missing: {tagged:?}"));
        assert_eq!(integer.name.as_deref(), Some("integer"));
        let tagged_page = record_page(&scenario, &tagged.state, 1, fixture).await;
        assert!(
            matches!(
                available_value(&named_child(&tagged_page, "integer").state),
                microscope::VariableValue::Scalar(ScalarValue::Unsigned(42))
            ),
            "{fixture}: {tagged:?}"
        );
        let raw = dereference_named(&scenario, "raw", 1).await;
        assert!(
            matches!(
                available_value(&raw.state),
                microscope::VariableValue::Union
            ) && available_children(&raw.state).total() == 2,
            "{fixture}: {raw:?}"
        );
        for (name, arm, expected) in [("optional", "some", 43), ("failure", "success", 44)] {
            let inspected = dereference_named(&scenario, name, 1).await;
            let microscope::VariableValue::Variant {
                active: Some(active),
                ..
            } = available_value(&inspected.state)
            else {
                panic!("{fixture}: {name} did not select an arm: {inspected:?}");
            };

            assert_eq!(active.name.as_deref(), Some(arm), "{inspected:?}");
            assert_eq!(active.members.len(), 1, "{inspected:?}");
            let page = record_page(&scenario, &inspected.state, 1, fixture).await;
            assert!(
                matches!(
                    available_value(&page.children[0].state),
                    microscope::VariableValue::Scalar(ScalarValue::Unsigned(value))
                        if *value == expected
                ),
                "{fixture}: {inspected:?}"
            );
        }

        assert_eq!(
            scenario.resume_to_stop().await,
            StopReason::Exited(ExitStatus::Code(0))
        );
        scenario.shutdown().await;
    }
}

#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "one scenario proves scalar, child-page, indexed, and ranged boundary reads"
)]
async fn dereference_reads_are_all_or_unavailable_across_an_unmapped_boundary() {
    let fixture = "pointer-memory-gcc-o0";
    let mut scenario = Scenario::launch(fixture);
    scenario.add_breakpoint("inspect_boundaries").await;
    assert!(matches!(
        scenario.run_to_stop().await,
        StopReason::Breakpoint { .. }
    ));

    let valid = dereference_named(&scenario, "valid_pointer", 1).await;
    assert_signed(&valid.state, 42, fixture);
    let boundary = dereference_named(&scenario, "boundary_pointer", 1).await;
    assert!(
        matches!(
            boundary.state,
            VariableState::Unavailable(
                VariableUnavailableReason::MemoryInaccessible {
                    address,
                    requested: 4,
                    completed: 2,
                    next_address,
                }
            ) if next_address.get() == address.get() + 2
        ),
        "{boundary:?}"
    );
    let valid_after_failure = dereference_named(&scenario, "valid_pointer", 1).await;
    assert_signed(&valid_after_failure.state, 42, fixture);

    let array = dereference_named(&scenario, "boundary_array", 1).await;
    assert!(
        matches!(
            array.state,
            VariableState::Available {
                raw: None,
                value: microscope::VariableValue::Array { .. },
                ..
            }
        ) && available_children(&array.state).total() == 4,
        "{fixture}: array summary performed an eager boundary read: {array:?}"
    );
    let readable = child_page(&scenario, &array.state, 0, 2).await;
    assert_signed(&readable.children[0].state, 41, fixture);
    assert_signed(&readable.children[1].state, 42, fixture);
    let unreadable = child_page(&scenario, &array.state, 2, 2).await;
    assert_eq!(unreadable.children.len(), 2, "{fixture}: {unreadable:?}");
    assert!(
        unreadable.children.iter().all(|child| matches!(
            child.state,
            VariableState::Unavailable(VariableUnavailableReason::MemoryInaccessible {
                completed: 0,
                ..
            })
        )),
        "{fixture}: an unmapped child was reported as readable: {unreadable:?}"
    );
    assert_eq!(
        child_page(&scenario, &array.state, 2, 2).await,
        unreadable,
        "{fixture}: repeated page evaluation changed at one stop"
    );
    let indexed_readable = scenario
        .operation(
            "inspect a readable element at a mapping boundary",
            scenario
                .handle()
                .inspect(&parsed_value_expression("(*boundary_array)[1]")),
        )
        .await;
    assert_signed(&indexed_readable.state, 42, fixture);
    let indexed_unreadable = scenario
        .operation(
            "inspect an unreadable element at a mapping boundary",
            scenario
                .handle()
                .inspect(&parsed_value_expression("(*boundary_array)[2]")),
        )
        .await;
    assert!(
        matches!(
            indexed_unreadable.state,
            VariableState::Unavailable(VariableUnavailableReason::MemoryInaccessible {
                requested: 4,
                completed: 0,
                ..
            })
        ),
        "{fixture}: {indexed_unreadable:?}"
    );
    let range = scenario
        .operation(
            "inspect a range crossing an unmapped boundary",
            evaluate_range(
                scenario.handle(),
                "(*boundary_array)[0..4]",
                microscope::InspectionLimits::default(),
            ),
        )
        .await;
    assert_eq!(range.children.len(), 4, "{fixture}: {range:?}");
    assert_signed(&range.children[0].state, 41, fixture);
    assert_signed(&range.children[1].state, 42, fixture);
    assert!(
        range.children[2..].iter().all(|child| matches!(
            child.state,
            VariableState::Unavailable(VariableUnavailableReason::MemoryInaccessible {
                completed: 0,
                ..
            })
        )),
        "{fixture}: {range:?}"
    );

    // A raw read returns the readable prefix and where it stopped.
    let pointer = scenario
        .operation(
            "boundary array",
            scenario.handle().variable("boundary_array"),
        )
        .await;
    let microscope::VariableValue::Address(boundary) = available_value(&pointer.state) else {
        panic!("{fixture}: boundary array was not an address: {pointer:?}");
    };

    let end = VirtualAddress::new(boundary.address.get() + 8);
    let prefix = scenario
        .operation(
            "read across the boundary",
            scenario.handle().read_memory(boundary.address, 16),
        )
        .await;
    assert_eq!(
        prefix.bytes.as_ref(),
        [41_i32.to_le_bytes(), 42_i32.to_le_bytes()].concat()
    );
    let inaccessible = microscope::MemoryReadCompletion::Incomplete {
        next_address: end,
        reason: microscope::MemoryReadUnavailableReason::Inaccessible,
    };

    assert_eq!(prefix.completion, inaccessible);
    let none = scenario
        .operation(
            "read past the boundary",
            scenario.handle().read_memory(end, 8),
        )
        .await;
    assert!(none.bytes.is_empty(), "{none:?}");
    assert_eq!(none.completion, inaccessible);

    assert_eq!(
        scenario.resume_to_stop().await,
        StopReason::Exited(ExitStatus::Code(0))
    );
    scenario.shutdown().await;
}

#[tokio::test]
async fn value_child_pages_are_arbitrary_repeatable_bounded_and_stop_scoped() {
    let fixture = "variables-gcc-o0";
    let mut scenario = Scenario::launch(fixture);
    scenario.add_source_breakpoint("variables.c", 68).await;
    assert!(matches!(
        scenario.run_to_stop().await,
        StopReason::Breakpoint { .. }
    ));
    let array = dereference_named(&scenario, "array_pointer", 1).await;
    let reference = available_children(&array.state).clone();
    assert_eq!(reference.total(), 2);
    let pointer = scenario
        .operation("pointer", scenario.handle().variable("pointer"))
        .await;
    let VariableState::Available {
        dereference: microscope::DereferenceState::Available(target),
        ..
    } = pointer.state
    else {
        panic!("pointer was not dereferenceable: {pointer:?}");
    };

    let first = scenario
        .operation("dereference", scenario.handle().dereference(target.clone()))
        .await;
    assert_eq!(
        scenario
            .operation(
                "dereference again",
                scenario.handle().dereference(target.clone())
            )
            .await,
        first
    );

    let tail = child_page(&scenario, &array.state, 1, 1).await;
    assert_eq!(tail.offset, 1);
    assert_eq!(tail.total, 2);
    assert_eq!(tail.children.len(), 1);
    assert_signed(&tail.children[0].state, 22, fixture);
    assert!(matches!(
        &tail.children[0].relationship,
        ValueChildRelationship::ArrayElement { index: 1, indices }
            if indices.as_ref() == [1]
    ));
    assert_eq!(
        child_page(&scenario, &array.state, 1, 1).await,
        tail,
        "{fixture}: a repeated page changed at the same stop"
    );
    let beyond = child_page(&scenario, &array.state, u64::MAX, 1).await;
    assert_eq!(beyond.offset, u64::MAX);
    assert!(beyond.children.is_empty());

    for limit in [0, 257] {
        assert!(matches!(
            scenario
                .handle()
                .value_children(
                    reference.clone(),
                    microscope::ValueChildQuery { offset: 0, limit },
                )
                .await,
            Err(Error::InvalidValueChildPageLimit(actual)) if actual == limit
        ));
    }

    assert!(matches!(
        scenario.resume_to_stop().await,
        StopReason::Breakpoint { .. }
    ));
    assert!(matches!(
        scenario
            .handle()
            .value_children(
                reference,
                microscope::ValueChildQuery {
                    offset: 0,
                    limit: 1,
                },
            )
            .await,
        Err(Error::StaleStop)
    ));
    assert!(matches!(
        scenario.handle().dereference(target).await,
        Err(Error::StaleStop)
    ));
    let fresh = dereference_named(&scenario, "array_pointer", 1).await;
    let head = child_page(&scenario, &fresh.state, 0, 1).await;
    assert_signed(&head.children[0].state, 20, fixture);
    scenario.shutdown().await;
}

#[tokio::test]
async fn stop_scoped_capabilities_reject_running_state_before_becoming_stale() {
    let mut scenario = Scenario::launch("spin");
    scenario.add_breakpoint("main").await;
    assert!(matches!(
        scenario.run_to_stop().await,
        StopReason::Breakpoint { .. }
    ));
    let array = scenario
        .operation("spin array", scenario.handle().variable("spin_values"))
        .await;
    let children = available_children(&array.state).clone();
    let pointer = scenario
        .operation("spin pointer", scenario.handle().variable("spin_pointer"))
        .await;
    let VariableState::Available {
        dereference: microscope::DereferenceState::Available(target),
        ..
    } = pointer.state
    else {
        panic!("spin pointer was not dereferenceable: {pointer:?}");
    };

    let page = microscope::ValueChildQuery {
        offset: 0,
        limit: 1,
    };

    scenario.remove_all_breakpoints().await;
    let running = scenario.start_resuming().await;
    assert!(matches!(
        scenario
            .handle()
            .value_children(children.clone(), page.clone())
            .await,
        Err(Error::NotStopped)
    ));
    assert!(matches!(
        scenario.handle().dereference(target.clone()).await,
        Err(Error::NotStopped)
    ));
    assert_eq!(scenario.handle().pause().await.unwrap(), StopReason::Pause);
    assert_eq!(running.await.unwrap().unwrap(), StopReason::Pause);
    assert!(matches!(
        scenario.handle().value_children(children, page).await,
        Err(Error::StaleStop)
    ));
    assert!(matches!(
        scenario.handle().dereference(target).await,
        Err(Error::StaleStop)
    ));
    scenario.shutdown().await;
}

#[tokio::test]
async fn optimized_implicit_pointer_chains_reconstruct_the_referent_without_an_address() {
    let fixture = "variables-gcc-o2";
    let mut scenario = Scenario::launch(fixture);
    scenario.add_source_breakpoint("variables.c", 80).await;
    assert!(matches!(
        scenario.run_to_stop().await,
        StopReason::Breakpoint { .. }
    ));

    let pointer_pointer = scenario
        .operation(
            "inspect implicit pointer",
            scenario.handle().variable("pointer_pointer"),
        )
        .await;
    assert!(
        matches!(
            pointer_pointer.state,
            VariableState::Available {
                source: microscope::VariableValueSource::ImplicitPointer,
                raw: None,
                dereference: microscope::DereferenceState::Available(_),
                ..
            }
        ) && matches!(
            available_value(&pointer_pointer.state),
            microscope::VariableValue::ImplicitPointer
        ),
        "{pointer_pointer:?}"
    );

    let pointee = dereference_named(&scenario, "pointer_pointer", 2).await;
    assert_signed(&pointee.state, 42, fixture);
    let atomic_pointee = scenario
        .operation(
            "inspect through implicit pointer chain atomically",
            scenario
                .handle()
                .inspect(&parsed_value_expression("**pointer_pointer")),
        )
        .await;
    assert_signed(&atomic_pointee.state, 42, fixture);
    scenario.shutdown().await;

    let mut offset = Scenario::new(
        "nonzero implicit pointer offset",
        Scenario::fixture(fixture),
    );
    offset.add_source_breakpoint("variables.c", 87).await;
    assert!(matches!(
        offset.run_to_stop().await,
        StopReason::Breakpoint { .. }
    ));
    let byte_pointer = offset
        .operation(
            "inspect offset implicit pointer",
            offset.handle().variable("byte_pointer"),
        )
        .await;
    assert!(
        matches!(
            byte_pointer.state,
            VariableState::Available {
                source: microscope::VariableValueSource::ImplicitPointer,
                raw: None,
                dereference: microscope::DereferenceState::Available(_),
                ..
            }
        ) && matches!(
            available_value(&byte_pointer.state),
            microscope::VariableValue::ImplicitPointer
        ),
        "{byte_pointer:?}"
    );
    let byte = dereference_named(&offset, "byte_pointer", 1).await;
    assert_eq!(
        available_value(&byte.state),
        &microscope::VariableValue::Scalar(ScalarValue::Unsigned(42)),
        "{byte:?}"
    );
    let atomic_byte = offset
        .operation(
            "inspect through offset implicit pointer atomically",
            offset
                .handle()
                .inspect(&parsed_value_expression("*byte_pointer")),
        )
        .await;
    assert!(
        matches!(
            available_value(&atomic_byte.state),
            microscope::VariableValue::Scalar(ScalarValue::Unsigned(42))
        ),
        "{atomic_byte:?}"
    );
    offset.shutdown().await;
}

#[tokio::test]
async fn go_slices_decode_subranges_empty_and_nil_descriptors() {
    let fixture = "variables-go-o0";
    let mut scenario = Scenario::new("Go slice descriptors", Scenario::fixture(fixture));
    scenario.add_source_breakpoint("main.go", 88).await;
    run_go_to_breakpoint(&mut scenario, fixture).await;

    for (name, capacity, expected) in [
        ("values", Some(3), &[20_i128, 22][..]),
        ("empty", Some(4), &[][..]),
        ("nilSlice", Some(0), &[][..]),
    ] {
        let variable = scenario
            .operation(
                "inspect Go slice descriptor",
                scenario.handle().variable(name),
            )
            .await;
        assert_slice_values(&scenario, &variable, capacity, expected, fixture).await;
    }

    let mut reason = scenario.resume_to_stop().await;
    for _ in 0..32 {
        match reason {
            StopReason::Exited(ExitStatus::Code(0)) => break,
            StopReason::Breakpoint { .. } => reason = scenario.resume_to_stop().await,
            StopReason::Exception(ref exception) if exception.code == 23 => {
                reason = scenario.resume_to_stop().await;
            }
            _ => panic!("{fixture} stopped unexpectedly while exiting: {reason:?}"),
        }
    }

    assert_eq!(reason, StopReason::Exited(ExitStatus::Code(0)));
    assert_eq!(scenario.shutdown().await, Some(ExitStatus::Code(0)));
}

#[tokio::test]
async fn readable_invalid_boolean_bytes_are_not_reported_as_unavailable_or_malformed() {
    let fixture = "variables-gcc-o0";
    let mut scenario = Scenario::launch(fixture);
    scenario.add_breakpoint("inspect_invalid_boolean").await;
    assert!(matches!(
        scenario.run_to_stop().await,
        StopReason::Breakpoint { .. }
    ));

    let value = scenario
        .operation(
            "inspect invalid boolean representation",
            scenario
                .handle()
                .inspect(&parsed_value_expression("*invalid")),
        )
        .await;
    assert!(
        matches!(
            value.state,
            VariableState::Invalid {
                source: microscope::VariableValueSource::Memory(_),
                ref raw,
                reason: microscope::VariableInvalidReason::BooleanRepresentation(2),
            } if raw.as_ref() == [2]
        ),
        "{fixture}: {value:?}"
    );

    assert_eq!(
        scenario.resume_to_stop().await,
        StopReason::Exited(ExitStatus::Code(0))
    );
    scenario.shutdown().await;
}

fn text(bytes: &[u8], completion: microscope::TextCompletion) -> microscope::TextSummary {
    microscope::TextSummary {
        bytes: bytes.into(),
        completion,
    }
}

/// Text longer than a summary holds, of one repeated byte.
fn truncated(byte: u8, length: Option<u64>) -> microscope::TextSummary {
    text(
        &[byte; microscope::TextSummary::MAX_BYTES],
        microscope::TextCompletion::Truncated { length },
    )
}

/// Stops a fixture at its "strings stop here" line, or in `function`, and
/// checks each variable's text.
async fn assert_strings(
    fixture: &str,
    source: &str,
    function: Option<&str>,
    expected: &[(&str, Option<microscope::TextSummary>)],
) {
    let mut scenario = Scenario::launch(fixture);
    if let Some(function) = function {
        scenario.add_breakpoint(function).await;
    } else {
        let line = source_line(&format!("tests/fixtures/{source}"), "strings stop here");
        let file = source.rsplit('/').next().expect("a file name");
        scenario.add_source_breakpoint(file, line).await;
    }

    assert!(matches!(
        scenario.run_to_stop().await,
        StopReason::Breakpoint { .. }
    ));
    let variables = scenario
        .operation("variables", scenario.handle().variables())
        .await;
    for (name, expected) in expected {
        assert_eq!(
            &text_of(&variables.variables, name),
            expected,
            "{fixture} {name}"
        );
    }

    scenario.shutdown().await;
}

#[tokio::test]
async fn c_strings_read_up_to_their_terminator_limit_or_unreadable_memory() {
    use microscope::TextCompletion::{Complete, Unreadable};

    let unreadable = Unreadable {
        address: VirtualAddress::new(1),
    };

    assert_strings(
        "strings-c-gcc-o0",
        "c/strings.c",
        None,
        &[
            ("greeting", Some(text(b"hello, world", Complete))),
            (
                "escaped",
                Some(text(b"tab\there \"quoted\" \\ \xc3\xa9\x80", Complete)),
            ),
            ("long_text", Some(truncated(b'x', None))),
            ("buffer", Some(text(b"abc", Complete))),
            // An array without a terminator is all text.
            ("unterminated", Some(text(b"wxyz", Complete))),
            ("null_text", None),
            ("invalid", Some(text(b"", unreadable))),
            ("bytes", Some(text(b"A\xff", Complete))),
            // Wide text is decoded from its units: UTF-32 for `wchar_t`
            // and `char32_t`, UTF-16 for `char16_t`.
            ("wide", Some(text("wide é".as_bytes(), Complete))),
            ("sixteen", Some(text("sixteen λ 🦀".as_bytes(), Complete))),
            (
                "thirty_two",
                Some(text("thirty-two 🦀".as_bytes(), Complete)),
            ),
            ("wide_buffer", Some(text(b"buf", Complete))),
            ("lone", Some(text("a\u{fffd}".as_bytes(), Complete))),
        ],
    )
    .await;
}

#[tokio::test]
async fn cpp_rust_and_go_strings_read_their_recorded_length() {
    use microscope::TextCompletion::Complete;

    assert_strings(
        "strings-cpp-clang-o0",
        "cpp/strings.cpp",
        None,
        &[
            ("short_text", Some(text(b"short", Complete))),
            ("long_text", Some(truncated(b'y', Some(300)))),
            ("empty", Some(text(b"", Complete))),
            ("with_nul", Some(text(b"a\0b", Complete))),
        ],
    )
    .await;
    assert_strings(
        "strings-rust-o0",
        "rust/strings.rs",
        Some("strings_target"),
        &[
            ("borrowed", Some(text("héllo".as_bytes(), Complete))),
            ("owned", Some(text(b"owned text", Complete))),
            ("empty", Some(text(b"", Complete))),
            ("long", Some(truncated(b'z', Some(300)))),
        ],
    )
    .await;
    assert_strings(
        "strings-go-o0",
        "go/strings/main.go",
        None,
        &[
            ("name", Some(text(b"gopher", Complete))),
            ("long", Some(truncated(b'g', Some(300)))),
            ("empty", Some(text(b"", Complete))),
        ],
    )
    .await;
}

#[tokio::test]
async fn text_running_into_an_unmapped_page_stops_at_the_page() {
    let mut scenario = Scenario::launch("strings-c-gcc-o0");
    scenario.add_breakpoint("strings_target").await;
    assert!(matches!(
        scenario.run_to_stop().await,
        StopReason::Breakpoint { .. }
    ));
    let variables = scenario
        .operation("variables", scenario.handle().variables())
        .await;
    let edge = text_of(&variables.variables, "edge").expect("text");
    assert_eq!(&*edge.bytes, b"eeeee");
    let microscope::TextCompletion::Unreadable { address } = edge.completion else {
        panic!("the text continues into an unmapped page: {edge:?}");
    };

    assert_eq!(address.get() % 4096, 0);
    scenario.shutdown().await;
}

/// Before a function's prologue its frame base may be meaningless: clang's
/// unoptimized code bases locations on `rbp`, which still holds the zero
/// a program's entry point left in it. A location computed from it wraps
/// the address space, so the variable is unavailable, and the others are
/// still shown.
#[tokio::test]
async fn a_variable_whose_location_wraps_the_address_space_is_unavailable() {
    let program = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("build/golden/straight/straight-clang-O0");
    let mut scenario = Scenario::new("variables before a prologue", program);
    let entry = scenario
        .run_with_to_stop(LaunchOptions {
            stop_at_entry: true,
            ..LaunchOptions::default()
        })
        .await;
    assert_eq!(entry, StopReason::Entry);

    // The entry point clears rbp, aligns the stack, and calls rt_start.
    for _ in 0..4 {
        scenario.step_to_stop(StepKind::Instruction).await;
    }

    let location = scenario
        .operation("location", scenario.handle().current_location())
        .await;
    assert_eq!(
        location
            .image
            .symbol
            .as_ref()
            .map(|symbol| (symbol.name.as_ref(), symbol.offset)),
        Some(("rt_start", 0)),
        "the golden binary changed"
    );
    let variables = scenario
        .operation("variables", scenario.handle().variables())
        .await;
    let stack = variables
        .variables
        .iter()
        .find(|variable| variable.name.as_ref() == "stack")
        .expect("rt_start's parameter");
    assert!(
        matches!(stack.state, microscope::VariableState::Unavailable(_)),
        "{stack:?}"
    );
    scenario.shutdown().await;
}

async fn assert_array_values(
    scenario: &Scenario,
    value: &microscope::DereferencedValue,
    fixture: &str,
) {
    let microscope::VariableValue::Array { .. } = available_value(&value.state) else {
        panic!("{fixture}: expected decoded array, got {value:?}");
    };

    let page = child_page(scenario, &value.state, 0, 2).await;
    let values: Vec<i128> = page
        .children
        .iter()
        .map(|child| match available_value(&child.state) {
            microscope::VariableValue::Scalar(microscope::ScalarValue::Signed(value)) => *value,
            other => panic!("{fixture}: expected scalar array element, got {other:?}"),
        })
        .collect();
    assert_eq!(values, [20, 22], "{fixture}");
}

/// The page of elements a range expression selects.
async fn evaluate_range(
    handle: &microscope::DebuggerHandle,
    text: &str,
    limits: microscope::InspectionLimits,
) -> microscope::Result<microscope::ValueChildPage> {
    match handle
        .evaluate_with(
            &parsed_value_expression(text),
            microscope::EvaluationMode::Read,
            limits,
        )
        .await?
    {
        microscope::Evaluation::Range(page) => Ok(page),
        other => panic!("`{text}` is not a range: {other:?}"),
    }
}
