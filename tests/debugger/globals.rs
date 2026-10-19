//! Global variables, the global catalog, and thread-local storage.

use super::*;

#[tokio::test]
async fn structural_inspection_preserves_dots_in_global_roots_before_selecting_members() {
    let fixture = "records-go-o0";
    let mut scenario = Scenario::launch(fixture);
    scenario.add_source_breakpoint("main.go", 22).await;
    run_go_to_breakpoint(&mut scenario, fixture).await;

    let root = scenario
        .operation(
            "inspect dotted global root",
            scenario
                .handle()
                .inspect(&value_expression(&["main", "globalRecord"])),
        )
        .await;

    record_page(&scenario, &root.state, 2, fixture).await;

    let member = scenario
        .operation(
            "inspect member below dotted global root",
            scenario.handle().inspect(&value_expression(&[
                "main",
                "globalRecord",
                "inner",
                "signedValue",
            ])),
        )
        .await;

    assert_signed(&member.state, -7, fixture);

    resume_go_to_exit(&mut scenario, fixture).await;
    assert_eq!(scenario.shutdown().await, Some(ExitStatus::Code(0)));
}

const CPP_GLOBALS: &[&str] = &[
    "fixture::alpha::duplicate",
    "fixture::Holder::member",
    "fixture::Holder::constexpr_member",
];

#[tokio::test]
async fn global_catalogs_normalize_qualification_and_optimized_storage_and_list_in_pages() {
    for (fixture, expected) in [
        ("globals-c-gcc-o0", &["external_value", "duplicate"][..]),
        ("globals-cpp-gcc-o0", CPP_GLOBALS),
        ("globals-cpp-clang-o0", CPP_GLOBALS),
        (
            "globals-rust-o0",
            &[
                "globals::ROOT_IMMUTABLE",
                "globals::alpha::DUPLICATE",
                "globals::beta::DUPLICATE",
            ][..],
        ),
        (
            "globals-go-o0",
            &["main.packageValue", "main.packageMutable"][..],
        ),
        (
            "globals-zig-o0",
            &[
                "globals.root_value",
                "globals.Alpha.duplicate",
                "globals.Beta.duplicate",
            ][..],
        ),
    ] {
        let debugger = Debugger::new(Scenario::fixture(fixture)).expect("load global catalog");
        let handle = debugger.handle();
        let image = handle.module_image();
        for qualified in expected {
            catalog_global(image, qualified);
        }

        for global in image.globals() {
            if let microscope::GlobalVariableType::Resolved(type_info) = &global.type_info {
                assert_eq!(
                    image.type_info(type_info.reference),
                    Some(type_info),
                    "{fixture}: global {} published type metadata that disagrees with its type graph node",
                    global.qualified_name
                );
            }
        }

        // Listing is filtered, paged, and ordered by name.
        if fixture == "globals-go-o0" {
            let page = |offset, limit| {
                handle.globals(microscope::GlobalVariableQuery {
                    filter: Some("main.package".to_owned()),
                    offset,
                    limit,
                })
            };

            let first = page(0, 1).await.expect("first global page");
            let second = page(1, 1).await.expect("second global page");
            assert_eq!(
                (first.offset, first.total, first.variables.len()),
                (0, 7, 1)
            );

            assert_eq!((second.total, second.variables.len()), (7, 1));
            assert!(first.variables[0].module.is_none());
            assert!(
                first.variables[0].variable.qualified_name
                    < second.variables[0].variable.qualified_name
            );

            assert!(matches!(
                page(0, 0).await,
                Err(Error::InvalidGlobalPageLimit(0))
            ));
        }

        debugger
            .shutdown()
            .await
            .expect("shut down catalog debugger");
    }

    for fixture in ["globals-rust-o2", "globals-zig-o2"] {
        let debugger = Debugger::new(Scenario::fixture(fixture)).expect("load optimized catalog");
        let handle = debugger.handle();
        let global = handle
            .module_image()
            .globals()
            .find(|global| matches!(global.name.as_ref(), "OPTIMIZED_AWAY" | "root_constant"))
            .unwrap_or_else(|| panic!("{fixture} missing optimized global"));

        assert!(matches!(
            global.type_info,
            microscope::GlobalVariableType::Resolved(_)
        ));

        debugger
            .shutdown()
            .await
            .expect("shut down catalog debugger");
    }
}

#[tokio::test]
async fn c_globals_cover_local_shadowing_collisions_relocation_and_optimization() {
    for fixture in [
        "globals-c-gcc-o0",
        "globals-c-clang-o0",
        "globals-c-gcc-o2",
        "globals-c-clang-o2",
        "globals-c-gcc-nopie",
    ] {
        let mut scenario = Scenario::launch(fixture);
        scenario.add_source_breakpoint("main.c", 9).await;
        assert!(matches!(
            scenario.run_to_stop().await,
            StopReason::Breakpoint { .. }
        ));

        let shadow = scenario
            .operation(
                "inspect local shadow",
                scenario.handle().variable("external_value"),
            )
            .await;

        assert_eq!(shadow.kind, VariableKind::Local);
        assert_variable_value(&shadow, ScalarValue::Signed(999));

        let external = catalog_global(scenario.handle().module_image(), "external_value");
        let external = scenario
            .operation(
                "inspect exact external global",
                scenario.handle().main_global(external.id),
            )
            .await;

        assert_eq!(external.kind, VariableKind::Global);
        assert!(external.global.is_some());
        assert_variable_value(&external, ScalarValue::Signed(101));

        let pointer = scenario
            .operation(
                "inspect global pointer",
                scenario.handle().variable("external_pointer"),
            )
            .await;

        let reference = match pointer.state {
            VariableState::Available {
                dereference: microscope::DereferenceState::Available(reference),
                ..
            } => reference,
            state => panic!("{fixture} global pointer was unavailable: {state:?}"),
        };

        let dereferenced = scenario
            .operation(
                "dereference global pointer",
                scenario.handle().dereference(reference),
            )
            .await;

        assert_signed(&dereferenced.state, 101, fixture);

        let one = scenario
            .operation(
                "inspect first file static",
                scenario.handle().variable("one.c::duplicate"),
            )
            .await;

        let two = scenario
            .operation(
                "inspect second file static",
                scenario.handle().variable("two.c::duplicate"),
            )
            .await;

        assert_variable_value(&one, ScalarValue::Signed(201));
        assert_variable_value(&two, ScalarValue::Signed(202));
        assert!(matches!(
            scenario.handle().variable("duplicate").await,
            Err(Error::AmbiguousGlobalVariable { .. })
        ));

        assert_eq!(
            scenario.resume_to_stop().await,
            StopReason::Exited(ExitStatus::Code(0))
        );

        scenario.shutdown().await;
    }
}

#[tokio::test]
async fn cpp_globals_resolve_namespaces_static_members_specifications_and_constants() {
    for fixture in [
        "globals-cpp-gcc-o0",
        "globals-cpp-clang-o0",
        "globals-cpp-gcc-o2",
        "globals-cpp-clang-o2",
    ] {
        let mut scenario = Scenario::launch(fixture);
        scenario.add_breakpoint("inspect_globals").await;
        assert!(matches!(
            scenario.run_to_stop().await,
            StopReason::Breakpoint { .. }
        ));

        for (name, expected) in [
            ("fixture::alpha::duplicate", 121),
            ("fixture::beta::duplicate", 122),
            ("fixture::Holder::member", 131),
            ("fixture::Holder::inline_member", 132),
            ("fixture::Holder::constexpr_member", 133),
            ("fixture::Holder::negative_constexpr_member", -123),
            ("fixture::{anonymous}::anonymous_value", 123),
        ] {
            let variable = scenario
                .operation(
                    "inspect qualified C++ global",
                    scenario.handle().variable(name),
                )
                .await;

            assert_variable_value(&variable, ScalarValue::Signed(expected));
        }

        assert!(matches!(
            scenario.handle().variable("duplicate").await,
            Err(Error::AmbiguousGlobalVariable { .. })
        ));

        assert_eq!(
            scenario.resume_to_stop().await,
            StopReason::Exited(ExitStatus::Code(0))
        );

        scenario.shutdown().await;
    }
}

#[tokio::test]
async fn rust_globals_preserve_module_qualification_and_honest_optimized_unavailability() {
    let mut scenario = Scenario::new("Rust globals O0", Scenario::fixture("globals-rust-o0"));
    scenario.add_breakpoint("inspect_globals").await;
    assert!(matches!(
        scenario.run_to_stop().await,
        StopReason::Breakpoint { .. }
    ));

    for (name, expected) in [
        ("globals::ROOT_IMMUTABLE", 141),
        ("globals::ROOT_MUTABLE", 142),
        ("globals::alpha::DUPLICATE", 151),
        ("globals::beta::DUPLICATE", 152),
    ] {
        let variable = scenario
            .operation("inspect Rust global", scenario.handle().variable(name))
            .await;

        assert_variable_value(&variable, ScalarValue::Signed(expected));
    }

    assert!(matches!(
        scenario.handle().variable("DUPLICATE").await,
        Err(Error::AmbiguousGlobalVariable { .. })
    ));

    assert_signed(
        &dereference_named(&scenario, "globals::ROOT_POINTER", 1)
            .await
            .state,
        157,
        "globals-rust-o0",
    );

    assert_eq!(
        scenario.resume_to_stop().await,
        StopReason::Exited(ExitStatus::Code(0))
    );

    scenario.shutdown().await;

    let mut optimized = Scenario::new("Rust globals O2", Scenario::fixture("globals-rust-o2"));
    optimized.add_breakpoint("inspect_globals").await;
    assert!(matches!(
        optimized.run_to_stop().await,
        StopReason::Breakpoint { .. }
    ));

    let root = optimized
        .operation(
            "inspect optimized Rust global",
            optimized.handle().variable("globals::ROOT_IMMUTABLE"),
        )
        .await;

    assert!(matches!(
        root.state,
        VariableState::Unavailable(
            microscope::VariableUnavailableReason::OptimizedOut(_)
                | microscope::VariableUnavailableReason::UnavailableAtInstruction,
        )
    ));

    assert_signed(
        &dereference_named(&optimized, "globals::ROOT_POINTER", 1)
            .await
            .state,
        157,
        "globals-rust-o2",
    );

    assert_eq!(
        optimized.resume_to_stop().await,
        StopReason::Exited(ExitStatus::Code(0))
    );

    optimized.shutdown().await;
}

#[tokio::test]
async fn go_package_globals_are_printable_without_source_stepping() {
    let fixture = "globals-go-o0";
    let mut scenario = Scenario::launch(fixture);
    scenario.add_breakpoint("main.inspectGlobals").await;
    run_go_to_breakpoint(&mut scenario, fixture).await;
    for (name, expected) in [
        ("main.packageValue", ScalarValue::Signed(161)),
        ("main.packageMutable", ScalarValue::Signed(162)),
    ] {
        let variable = scenario
            .operation(
                "inspect Go package global",
                scenario.handle().variable(name),
            )
            .await;

        assert_variable_value(&variable, expected);
    }

    assert_signed(
        &dereference_named(&scenario, "main.packagePointer", 1)
            .await
            .state,
        162,
        fixture,
    );

    assert_signed(
        &dereference_named(&scenario, "main.packagePointerPointer", 2)
            .await
            .state,
        162,
        fixture,
    );

    let nil = scenario
        .operation(
            "inspect Go package nil pointer",
            scenario.handle().variable("main.packageNil"),
        )
        .await;

    assert!(matches!(
        nil.state,
        VariableState::Available {
            dereference: microscope::DereferenceState::Unavailable {
                reason: microscope::DereferenceUnavailableReason::Null,
                ..
            },
            ..
        }
    ));

    let pair = dereference_named(&scenario, "main.packagePairPointer", 1).await;
    record_page(&scenario, &pair.state, 2, fixture).await;
    resume_go_to_exit(&mut scenario, fixture).await;
    assert_eq!(scenario.shutdown().await, Some(ExitStatus::Code(0)));

    let debugger =
        Debugger::new(Scenario::fixture("globals-go-o2")).expect("load optimized Go globals");

    let handle = debugger.handle();
    catalog_global(handle.module_image(), "main.packageValue");
    let pointer = catalog_global(handle.module_image(), "main.packagePointer");
    assert!(matches!(
        &pointer.type_info,
        microscope::GlobalVariableType::Resolved(microscope::TypeInfo {
            kind: microscope::TypeKind::Pointer { .. },
            ..
        })
    ));

    debugger
        .shutdown()
        .await
        .expect("shut down optimized Go debugger");
}

#[tokio::test]
async fn zig_globals_cover_containers_constants_pie_and_optimized_storage() {
    for fixture in ["globals-zig-o0", "globals-zig-nopie"] {
        let mut scenario = Scenario::launch(fixture);
        scenario.add_breakpoint("inspectGlobals").await;
        assert!(matches!(
            scenario.run_to_stop().await,
            StopReason::Breakpoint { .. }
        ));

        for (name, expected) in [
            ("globals.root_value", 171),
            ("globals.root_constant", 172),
            ("globals.Alpha.duplicate", 181),
            ("globals.Alpha.constant", 182),
            ("globals.Beta.duplicate", 183),
        ] {
            let variable = scenario
                .operation("inspect Zig global", scenario.handle().variable(name))
                .await;
            assert_variable_value(&variable, ScalarValue::Signed(expected));
        }

        assert!(matches!(
            scenario.handle().variable("duplicate").await,
            Err(Error::AmbiguousGlobalVariable { .. })
        ));

        assert_signed(
            &dereference_named(&scenario, "globals.root_pointer", 1)
                .await
                .state,
            184,
            fixture,
        );

        assert_eq!(
            scenario.resume_to_stop().await,
            StopReason::Exited(ExitStatus::Code(0))
        );

        scenario.shutdown().await;
    }

    let mut optimized = Scenario::new("Zig globals O2", Scenario::fixture("globals-zig-o2"));
    optimized.add_breakpoint("inspectGlobals").await;
    assert!(matches!(
        optimized.run_to_stop().await,
        StopReason::Breakpoint { .. }
    ));

    let constant = optimized
        .operation(
            "inspect optimized Zig global",
            optimized.handle().variable("globals.root_constant"),
        )
        .await;

    assert!(
        matches!(
            constant.state,
            VariableState::Unavailable(microscope::VariableUnavailableReason::OptimizedOut(_))
        ),
        "{constant:?}"
    );

    let pointer = optimized
        .operation(
            "inspect optimized Zig pointer global",
            optimized.handle().variable("globals.root_pointer"),
        )
        .await;

    assert!(
        matches!(
            pointer.state,
            VariableState::Unavailable(microscope::VariableUnavailableReason::OptimizedOut(_))
        ),
        "{pointer:?}"
    );

    assert_eq!(
        optimized.resume_to_stop().await,
        StopReason::Exited(ExitStatus::Code(0))
    );

    optimized.shutdown().await;
}

#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "one lifecycle scenario must retain identities across load, unload, and reload"
)]
async fn shared_library_globals_track_load_unload_reload_and_stale_identity() {
    let mut scenario = Scenario::new("shared globals", Scenario::fixture("globals-shared"));
    assert!(matches!(
        scenario.handle().loaded_modules().await,
        Err(Error::NotRunning)
    ));
    scenario.add_breakpoint("after_load").await;
    scenario.add_breakpoint("after_unload").await;
    scenario.add_breakpoint("after_reload").await;
    let mut events = scenario.handle().subscribe();

    assert!(matches!(
        scenario.run_to_stop().await,
        StopReason::Breakpoint { .. }
    ));

    let Err(Error::AmbiguousLoadedGlobalVariable {
        selector,
        candidates,
    }) = scenario.handle().variable("module_collision").await
    else {
        panic!("same-name globals across modules must be ambiguous");
    };

    assert_eq!(selector, "module_collision");
    assert_eq!(candidates.len(), 2);
    let loaded = scenario
        .operation(
            "list loaded DSO globals",
            scenario.handle().globals(microscope::GlobalVariableQuery {
                filter: Some("dso_".to_owned()),
                ..microscope::GlobalVariableQuery::default()
            }),
        )
        .await;

    let external = loaded
        .variables
        .iter()
        .find(|entry| entry.variable.name.as_ref() == "dso_external")
        .expect("DSO external global");

    let first_module = external.module.expect("DSO is loaded");
    let first_reference = microscope::GlobalVariableReference {
        module: first_module.id,
        image: external.image,
        variable: external.variable.id,
    };

    let value = scenario
        .operation(
            "inspect DSO external global",
            scenario.handle().loaded_global(first_reference),
        )
        .await;

    assert_variable_value(&value, ScalarValue::Signed(211));
    let cross_module_pointer =
        catalog_global(scenario.handle().module_image(), "cross_module_pointer");

    let cross_module_pointer = scenario
        .operation(
            "inspect main-image pointer into DSO",
            scenario.handle().main_global(cross_module_pointer.id),
        )
        .await;

    let cross_module_reference = match cross_module_pointer.state {
        VariableState::Available {
            dereference: microscope::DereferenceState::Available(reference),
            ..
        } => reference,
        state => panic!("cross-module pointer was unavailable: {state:?}"),
    };

    let cross_module_referent = scenario
        .operation(
            "dereference main-image pointer into DSO",
            scenario.handle().dereference(cross_module_reference),
        )
        .await;

    assert_signed(&cross_module_referent.state, 211, "globals-shared");
    let dso_pointer = loaded
        .variables
        .iter()
        .find(|entry| entry.variable.name.as_ref() == "dso_pointer")
        .expect("DSO pointer global");

    let dso_pointer_module = dso_pointer.module.expect("pointer DSO is loaded");
    let dso_pointer = scenario
        .operation(
            "inspect DSO pointer global",
            scenario
                .handle()
                .loaded_global(microscope::GlobalVariableReference {
                    module: dso_pointer_module.id,
                    image: dso_pointer.image,
                    variable: dso_pointer.variable.id,
                }),
        )
        .await;

    let reference = match dso_pointer.state {
        VariableState::Available {
            dereference: microscope::DereferenceState::Available(reference),
            ..
        } => reference,
        state => panic!("DSO pointer global was unavailable: {state:?}"),
    };

    let dso_referent = scenario
        .operation(
            "dereference DSO pointer global",
            scenario.handle().dereference(reference),
        )
        .await;

    assert_signed(&dso_referent.state, 211, "globals-shared");
    let dso_tls = loaded
        .variables
        .iter()
        .find(|entry| entry.variable.name.as_ref() == "dso_tls")
        .expect("DSO TLS global");

    let dso_tls_module = dso_tls.module.expect("TLS DSO is loaded");
    let tls_value = scenario
        .operation(
            "inspect dynamically loaded TLS global",
            scenario
                .handle()
                .loaded_global(microscope::GlobalVariableReference {
                    module: dso_tls_module.id,
                    image: dso_tls.image,
                    variable: dso_tls.variable.id,
                }),
        )
        .await;

    assert_variable_value(&tls_value, ScalarValue::Signed(213));

    // glibc's descriptors find a loaded library's TLS block in the DTV or
    // in static TLS space the loader set aside.
    assert_eq!(tls_location_by_name(&scenario, "dso_tls").await.1, 213);
    let mut saw_load = false;

    while let Ok(event) = events.try_recv() {
        saw_load |= matches!(
            event,
            microscope::DebuggerEvent::ModuleLoaded { module, .. }
                if module.path.ends_with("libglobals.so")
        );
    }

    assert!(saw_load);

    assert!(matches!(
        scenario.resume_to_stop().await,
        StopReason::Breakpoint { .. }
    ));

    assert!(matches!(
        scenario.handle().loaded_global(first_reference).await,
        Err(Error::ModuleNotLoaded(id)) if id == first_module.id
    ));

    let mut saw_unload = false;
    while let Ok(event) = events.try_recv() {
        saw_unload |= matches!(
            event,
            microscope::DebuggerEvent::ModuleUnloaded { module, .. }
                if module.module.id == first_module.id
        );
    }

    assert!(saw_unload);

    assert!(matches!(
        scenario.resume_to_stop().await,
        StopReason::Breakpoint { .. }
    ));

    let reloaded = scenario
        .operation(
            "list reloaded DSO globals",
            scenario.handle().globals(microscope::GlobalVariableQuery {
                filter: Some("dso_external".to_owned()),
                ..microscope::GlobalVariableQuery::default()
            }),
        )
        .await;

    let reloaded = reloaded.variables.first().expect("reloaded DSO global");
    let reloaded_module = reloaded.module.expect("DSO was reloaded");
    assert_ne!(reloaded_module.id, first_module.id);
    assert_ne!(reloaded.image, first_reference.image);
    let reloaded_value = scenario
        .operation(
            "inspect reloaded DSO global",
            scenario
                .handle()
                .loaded_global(microscope::GlobalVariableReference {
                    module: reloaded_module.id,
                    image: reloaded.image,
                    variable: reloaded.variable.id,
                }),
        )
        .await;

    assert_variable_value(&reloaded_value, ScalarValue::Signed(211));

    // Reloading reuses the module's TLS slot under a newer generation.
    assert_eq!(tls_location_by_name(&scenario, "dso_tls").await.1, 213);

    assert_eq!(
        scenario.resume_to_stop().await,
        StopReason::Exited(ExitStatus::Code(0))
    );

    assert!(matches!(
        scenario.handle().loaded_modules().await,
        Err(Error::NotRunning)
    ));

    scenario.remove_all_breakpoints().await;
    scenario.add_breakpoint("after_load").await;
    assert!(matches!(
        scenario.run_to_stop().await,
        StopReason::Breakpoint { .. }
    ));

    let second_run = scenario
        .operation(
            "list second-run modules",
            scenario.handle().loaded_modules(),
        )
        .await;

    let second_run_dso = second_run
        .modules
        .iter()
        .filter(|module| {
            module
                .path
                .file_name()
                .is_some_and(|name| name == "libglobals.so")
        })
        .collect::<Vec<_>>();

    assert_eq!(second_run_dso.len(), 1, "{second_run:?}");
    assert_ne!(second_run_dso[0].module.id, reloaded_module.id);
    scenario.remove_all_breakpoints().await;
    assert_eq!(
        scenario.resume_to_stop().await,
        StopReason::Exited(ExitStatus::Code(0))
    );

    assert_eq!(scenario.shutdown().await, Some(ExitStatus::Code(0)));
}

#[tokio::test]
async fn tls_globals_resolve_per_selected_thread_for_gcc_and_clang() {
    for fixture in ["globals-tls-gcc", "globals-tls-clang"] {
        let mut scenario = Scenario::launch(fixture);
        scenario.add_breakpoint("tls_stop").await;
        scenario.add_breakpoint("tls_after_join").await;
        assert!(matches!(
            scenario.run_to_stop().await,
            StopReason::Breakpoint { .. }
        ));

        let global = catalog_global(scenario.handle().module_image(), "tls_value").id;
        let pointer = catalog_global(scenario.handle().module_image(), "tls_pointer").id;
        let snapshot = scenario.snapshot().await;
        assert_eq!(snapshot.threads.len(), 3, "{fixture}: {snapshot:?}");
        let mut values = Vec::new();
        let mut thread_references = Vec::new();
        for thread in snapshot.threads.iter() {
            scenario
                .operation(
                    "select TLS thread",
                    scenario.handle().select_context(thread.id),
                )
                .await;

            let variable = scenario
                .operation(
                    "inspect selected thread TLS",
                    scenario.handle().main_global(global),
                )
                .await;

            let microscope::VariableValue::Scalar(ScalarValue::Signed(value)) =
                available_value(&variable.state)
            else {
                panic!("{fixture}: unavailable TLS variable {variable:?}");
            };

            values.push(*value);

            // The variable found by name is the storage whose address the
            // thread stored in its own pointer.
            let (address, by_name) = tls_location_by_name(&scenario, "tls_value").await;
            assert_eq!(by_name, *value, "{fixture}");
            let pointer = scenario
                .operation(
                    "inspect selected thread TLS pointer",
                    scenario.handle().main_global(pointer),
                )
                .await;

            let microscope::VariableValue::Address(stored) = available_value(&pointer.state) else {
                panic!("{fixture}: TLS pointer is not an address: {pointer:?}");
            };

            assert_eq!(stored.address.get(), address, "{fixture}");
            let reference = match pointer.state {
                VariableState::Available {
                    dereference: microscope::DereferenceState::Available(reference),
                    ..
                } => reference,
                state => panic!("{fixture}: unavailable TLS pointer {state:?}"),
            };

            thread_references.push((thread.id, *value, reference.clone()));
            let tls_referent = scenario
                .operation(
                    "dereference selected thread TLS pointer",
                    scenario.handle().dereference(reference),
                )
                .await;

            assert_signed(&tls_referent.state, *value, fixture);
        }

        let selected = snapshot.threads.last().expect("TLS thread").id;
        scenario
            .operation(
                "change selection before reusing TLS capabilities",
                scenario.handle().select_context(selected),
            )
            .await;

        for (origin, value, reference) in thread_references {
            assert_eq!(reference.context(), origin.into());
            let tls_referent = scenario
                .operation(
                    "dereference TLS capability after changing selection",
                    scenario.handle().dereference(reference),
                )
                .await;

            assert_signed(&tls_referent.state, value, fixture);
        }

        values.sort_unstable();
        assert_eq!(values, [300, 301, 302], "{fixture}");
        assert!(matches!(
            scenario.resume_to_stop().await,
            StopReason::Breakpoint { .. }
        ));

        let after_exit = scenario.snapshot().await;
        assert_eq!(after_exit.threads.len(), 1, "{fixture}: {after_exit:?}");
        let surviving = scenario
            .operation(
                "inspect TLS after worker exit",
                scenario.handle().main_global(global),
            )
            .await;

        assert_variable_value(&surviving, ScalarValue::Signed(300));
        assert_eq!(
            scenario.resume_to_stop().await,
            StopReason::Exited(ExitStatus::Code(0))
        );

        assert_eq!(scenario.shutdown().await, Some(ExitStatus::Code(0)));
    }
}

#[tokio::test]
async fn tls_of_every_module_resolves_per_thread_on_glibc_and_musl() {
    // Each build, whether it loads a plugin, and its thread count. Builds
    // with a plugin are dynamically linked, with the library and the plugin
    // as separate TLS modules. Statically linked glibc builds without threads
    // lack glibc's thread library. The builds for Ubuntu's glibc run on it:
    // 2.27 and 2.31 describe their thread structures in libpthread, 2.35 in
    // libc.
    for (fixture, plugin, threads) in [
        ("tls-modules-gcc", true, 3),
        ("tls-modules-gcc-static", false, 3),
        ("tls-modules-clang-static-pie", false, 3),
        ("tls-modules-single-thread-gcc-static-pie", false, 1),
        ("tls-modules-single-thread-clang-static", false, 1),
        ("tls-modules-glibc2.27", true, 3),
        ("tls-modules-glibc2.31", true, 3),
        ("tls-modules-glibc2.35", true, 3),
        ("tls-modules-single-thread-glibc2.35", false, 1),
        ("tls-modules-musl-gcc-o0", true, 3),
        ("tls-modules-musl-clang-o2-nopie", true, 3),
        ("tls-modules-musl-gcc-static", false, 3),
        ("tls-modules-musl-clang-static-pie", false, 3),
    ] {
        let mut scenario = Scenario::launch(fixture);
        let entry = scenario
            .run_with_to_stop(LaunchOptions {
                stop_at_entry: true,
                ..LaunchOptions::default()
            })
            .await;

        assert_eq!(entry, StopReason::Entry, "{fixture}");

        // Before the C library sets the thread pointer up, no thread has TLS.
        // Only dynamically linked glibc needs its loader to identify the
        // executable's TLS, and its loader has not run yet.
        let dynamic_glibc = fixture == "tls-modules-gcc" || fixture.contains("-glibc");
        if !dynamic_glibc {
            let early = scenario
                .operation("TLS at entry", scenario.handle().variable("main_tls"))
                .await;

            assert_eq!(
                early.state,
                VariableState::Unavailable(VariableUnavailableReason::TlsUnavailable(
                    microscope::TlsUnavailableReason::LookupFailed(
                        "the thread has not allocated the module's TLS block".into()
                    )
                )),
                "{fixture}"
            );
        }

        scenario.add_breakpoint("tls_stop").await;
        assert!(
            matches!(
                scenario.resume_to_stop().await,
                StopReason::Breakpoint { .. }
            ),
            "{fixture}"
        );

        support::assert_tls_modules(&mut scenario, fixture, plugin, threads).await;
        assert_eq!(
            scenario.resume_to_stop().await,
            StopReason::Exited(ExitStatus::Code(0)),
            "{fixture}"
        );

        assert_eq!(scenario.shutdown().await, Some(ExitStatus::Code(0)));
    }
}

/// A library with no code maps nothing executable, but it is a module all
/// the same, with globals and TLS of its own.
#[tokio::test]
async fn a_library_with_no_code_is_a_module_with_its_globals_and_tls() {
    let fixture = "data-library";
    let mut scenario = Scenario::launch(fixture);
    scenario.add_breakpoint("data_stop").await;
    assert!(
        matches!(scenario.run_to_stop().await, StopReason::Breakpoint { .. }),
        "{fixture}"
    );

    let modules = scenario
        .operation("modules", scenario.handle().loaded_modules())
        .await;

    assert!(
        modules
            .modules
            .iter()
            .any(|module| module.path.ends_with("libdata.so")),
        "{modules:#?}"
    );

    for (name, expected) in [("data_global", 7), ("data_tls", 42)] {
        let variable = scenario
            .operation(name, scenario.handle().variable(name))
            .await;

        assert_variable_value(&variable, ScalarValue::Signed(expected));
    }

    assert_eq!(
        scenario.resume_to_stop().await,
        StopReason::Exited(ExitStatus::Code(0))
    );

    assert_eq!(scenario.shutdown().await, Some(ExitStatus::Code(0)));
}

/// glibc before 2.34 describes its thread structures only in libpthread, so
/// the TLS of a program that never loads it cannot be found, which microscope
/// says rather than guessing at the C library's layout.
#[tokio::test]
async fn tls_on_glibc_before_2_34_without_libpthread_is_unavailable_with_its_reason() {
    let fixture = "tls-modules-single-thread-glibc2.31";
    let mut scenario = Scenario::launch(fixture);
    scenario.add_breakpoint("tls_stop").await;
    assert!(matches!(
        scenario.run_to_stop().await,
        StopReason::Breakpoint { .. }
    ));

    for name in ["main_tls", "library_tls"] {
        let variable = scenario
            .operation("unlocatable TLS", scenario.handle().variable(name))
            .await;

        assert_eq!(
            variable.state,
            VariableState::Unavailable(VariableUnavailableReason::TlsUnavailable(
                microscope::TlsUnavailableReason::LookupFailed(
                    "no loaded module defines _thread_db_link_map_l_tls_modid, which glibc \
                     before 2.34 defines only in libpthread"
                        .into()
                )
            )),
            "{name}"
        );
    }

    assert_eq!(
        scenario.resume_to_stop().await,
        StopReason::Exited(ExitStatus::Code(0))
    );

    assert_eq!(scenario.shutdown().await, Some(ExitStatus::Code(0)));
}

/// The address and value of a TLS variable in the selected thread.
fn tls_location(variable: &microscope::Variable) -> (u64, i128) {
    match &variable.state {
        VariableState::Available {
            value: microscope::VariableValue::Scalar(ScalarValue::Signed(value)),
            source: microscope::VariableValueSource::Memory(address),
            ..
        } => (address.get(), *value),
        state => panic!("{} was not available in memory: {state:?}", variable.name),
    }
}

/// The address and value of the TLS variable `name` in the selected thread.
pub async fn tls_location_by_name(scenario: &Scenario, name: &str) -> (u64, i128) {
    tls_location(
        &scenario
            .operation("TLS by name", scenario.handle().variable(name))
            .await,
    )
}

fn catalog_global(image: &ModuleImage, qualified_name: &str) -> microscope::GlobalVariableInfo {
    image
        .globals()
        .find(|global| global.qualified_name.as_ref() == qualified_name)
        .unwrap_or_else(|| {
            panic!(
                "missing global {qualified_name}; catalog: {:?}",
                image
                    .globals()
                    .map(|global| global.qualified_name)
                    .collect::<Vec<_>>()
            )
        })
}
