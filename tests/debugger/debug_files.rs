use std::io::{BufRead as _, BufReader, Write as _};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use microscope::DebugFileOptions;

use super::libraries::pending_function;
use super::*;
use crate::support::ScratchDir;

fn split(name: &str) -> PathBuf {
    Scenario::fixture(&format!("split/{name}"))
}

fn debug_root() -> PathBuf {
    split("debug-root")
}

fn build_id_debug_file() -> (PathBuf, String) {
    let ids = debug_root().join(".build-id");
    let directory = fs::read_dir(&ids)
        .expect("the build-id directory")
        .map(|entry| entry.expect("an entry").path())
        .find(|path| path.is_dir())
        .expect("a build-id directory");
    let file = fs::read_dir(&directory)
        .expect("the build-id files")
        .map(|entry| entry.expect("an entry").path())
        .find(|path| {
            path.extension()
                .is_some_and(|extension| extension == "debug")
        })
        .expect("a debug file");
    let prefix = directory.file_name().expect("a name").to_string_lossy();
    let rest = file.file_stem().expect("a name").to_string_lossy();

    (file.clone(), format!("{prefix}{rest}"))
}

async fn stops_with_debug_information(mut scenario: Scenario, debug_file: &Path) {
    let image = scenario.handle().module_image();
    assert_eq!(
        image.debug_file().map(|path| path.as_path()),
        Some(debug_file),
        "the debug file"
    );
    assert!(image.functions().len() != 0);
    let breakpoint = scenario
        .operation(
            "add breakpoint",
            scenario
                .handle()
                .add_breakpoint(BreakpointSpec::Function("breakpoint_target".to_owned())),
        )
        .await;
    let reason = scenario.run_to_stop().await;
    let StopReason::Breakpoint { hits, .. } = &reason else {
        panic!("stopped for {reason:?}");
    };

    assert_eq!(hits[0].breakpoint, breakpoint.id);
    let location = scenario
        .operation("location", scenario.handle().current_location())
        .await;
    assert!(location_line(&location).is_some(), "{location:?}");

    assert!(
        image
            .symbols()
            .any(|symbol| symbol.name() == "breakpoint_target"),
        "the debug file's symbols"
    );
    scenario.shutdown().await;
}

#[tokio::test]
async fn a_debug_link_beside_the_program_describes_it() {
    let fixture = split("basic-debuglink");
    let scenario = Scenario::new("debuglink", &fixture);
    let debug_file = split(".debug/basic-debuglink.debug")
        .canonicalize()
        .expect("the debug file");
    stops_with_debug_information(scenario, &debug_file).await;
}

#[tokio::test]
async fn a_debug_link_to_a_file_from_another_build_is_refused() {
    let scratch = ScratchDir::new("stale-debuglink");
    let program = scratch.path().join("basic-debuglink");
    fs::copy(split("basic-debuglink"), &program).expect("copy the program");
    fs::create_dir(scratch.path().join(".debug")).expect("the .debug directory");

    fs::copy(
        build_id_debug_file().0,
        scratch.path().join(".debug/basic-debuglink.debug"),
    )
    .expect("copy a debug file");
    let scenario = Scenario::new("stale-debuglink", &program);
    let image = scenario.handle().module_image();
    assert_eq!(image.debug_file(), None);
    assert_eq!(image.functions().len(), 0);
    let Some(microscope::DebugFile::Unusable { path, reason }) = image.separate_debug_file() else {
        panic!("{:?}", image.separate_debug_file());
    };

    assert_eq!(
        path.as_path(),
        scratch.path().join(".debug/basic-debuglink.debug")
    );
    assert!(
        reason.starts_with("its CRC-32 is 0x")
            && reason.ends_with(
                "the module's debug link records: it is from another build, or has changed since"
            ),
        "{reason}"
    );
    assert_eq!(
        image.debug_information(),
        microscope::DebugInformation::Absent
    );
    scenario.shutdown().await;
}

#[tokio::test]
async fn a_debug_directory_holds_the_debug_file_a_build_id_names() {
    let fixture = split("basic-build-id");

    let scenario = Scenario::new("build-id-missing", &fixture);
    assert_eq!(scenario.handle().module_image().functions().len(), 0);
    scenario.shutdown().await;

    let options = DebugFileOptions {
        directories: vec![debug_root()],
        ..DebugFileOptions::default()
    };

    let scenario = Scenario::with_debug_files("build-id", &fixture, &options);
    stops_with_debug_information(scenario, &build_id_debug_file().0).await;
}

#[tokio::test]
async fn a_library_loads_its_debug_information_from_its_debug_link() {
    let mut scenario = Scenario::new("library-debuglink", split("module-frames"));
    let breakpoint = pending_function(&scenario, "dso_apply").await;
    let reason = scenario.run_to_stop().await;
    let StopReason::Breakpoint { hits, .. } = &reason else {
        panic!("stopped for {reason:?}");
    };

    assert_eq!(hits[0].breakpoint, breakpoint.id);
    let trace = scenario
        .operation("backtrace", scenario.handle().backtrace())
        .await;
    let frame = &trace.frames[0];
    assert_eq!(
        frame.function.as_ref().map(|function| &*function.name),
        Some("dso_apply")
    );
    assert!(frame.source.is_some(), "{frame:?}");
    let image = scenario
        .operation(
            "library image",
            scenario
                .handle()
                .loaded_module_image(frame.module.expect("a module")),
        )
        .await;
    let debug_file = split(".debug/libmodule-frames.so.debug")
        .canonicalize()
        .expect("the debug file");
    assert_eq!(
        image.debug_file().map(|path| path.as_path()),
        Some(debug_file.as_path())
    );
    scenario.shutdown().await;
}

struct Server {
    url: String,
    requests: Arc<AtomicUsize>,
}

impl Server {
    fn start(id: String, body: Vec<u8>) -> Self {
        Self::serving(vec![(id, body)])
    }

    fn serving(files: Vec<(String, Vec<u8>)>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind a port");
        let url = format!("http://{}", listener.local_addr().expect("an address"));
        let requests = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&requests);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else {
                    continue;
                };

                let mut request = String::new();
                let mut reader = BufReader::new(&stream);
                if reader.read_line(&mut request).is_err() {
                    continue;
                }

                let mut line = String::new();
                while reader.read_line(&mut line).is_ok_and(|read| read > 2) {
                    line.clear();
                }

                let body = files.iter().find_map(|(id, body)| {
                    request
                        .starts_with(&format!("GET /buildid/{id}/debuginfo "))
                        .then_some(body)
                });
                let response = body.map_or_else(
                    || {
                        b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                            .to_vec()
                    },
                    |body| {
                        counted.fetch_add(1, Ordering::SeqCst);
                        let mut response = format!(
                            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            body.len()
                        )
                        .into_bytes();
                        response.extend_from_slice(body);
                        response
                    },
                );

                let _ = stream.write_all(&response);
            }
        });

        Self { url, requests }
    }
}

#[tokio::test]
async fn debuginfod_downloads_debug_files_into_its_cache() {
    let fixture = split("basic-build-id");
    let (debug_file, id) = build_id_debug_file();
    let cache = ScratchDir::new("debuginfod-cache");
    let options = |url: &str| DebugFileOptions {
        debuginfod: true,
        debuginfod_urls: Some(vec![url.to_owned()]),
        debuginfod_cache: Some(cache.path().to_path_buf()),
        ..DebugFileOptions::default()
    };

    let wrong = Server::start(
        id.clone(),
        fs::read(split(".debug/basic-debuglink.debug")).expect("a debug file"),
    );
    let scenario = Scenario::with_debug_files("debuginfod-wrong", &fixture, &options(&wrong.url));
    assert_eq!(scenario.handle().module_image().functions().len(), 0);
    assert_eq!(wrong.requests.load(Ordering::SeqCst), 1);
    scenario.shutdown().await;

    let server = Server::start(id.clone(), fs::read(&debug_file).expect("the debug file"));
    let cached = cache.path().join(&id).join("debuginfo");
    let scenario = Scenario::with_debug_files("debuginfod", &fixture, &options(&server.url));
    assert_eq!(server.requests.load(Ordering::SeqCst), 1);
    assert_eq!(
        fs::read(&cached).expect("the cached file"),
        fs::read(&debug_file).expect("the debug file")
    );
    stops_with_debug_information(scenario, &cached).await;

    let scenario = Scenario::with_debug_files("debuginfod-cached", &fixture, &options(&wrong.url));
    stops_with_debug_information(scenario, &cached).await;
    assert_eq!(wrong.requests.load(Ordering::SeqCst), 1);
}

fn dwz(name: &str) -> PathBuf {
    Scenario::fixture(&format!("dwz/{name}"))
}

fn dwz_debug_root() -> PathBuf {
    dwz("gcc-o2/debug-root")
}

fn build_id(path: &Path) -> String {
    let data = fs::read(path).expect("read the file");
    let object = object::File::parse(data.as_slice()).expect("an object file");
    object::Object::build_id(&object)
        .expect("readable notes")
        .expect("a build-id")
        .iter()
        .fold(String::new(), |mut text, byte| {
            use std::fmt::Write as _;
            write!(text, "{byte:02x}").expect("writing to a String cannot fail");
            text
        })
}

fn filed(root: &Path, id: &str) -> PathBuf {
    root.join(".build-id")
        .join(&id[..2])
        .join(format!("{}.debug", &id[2..]))
}

fn described(image: &ModuleImage) -> BTreeSet<String> {
    let place = |location: Option<SourceLocation>| {
        location.map_or_else(String::new, |location| {
            let file = image.source_file(location.file).expect("a source file");
            format!(
                "{}:{:?}:{:?}",
                file.path.display(),
                location.line,
                location.column
            )
        })
    };

    let mut described = BTreeSet::new();
    for function in image.functions() {
        described.insert(format!(
            "function {} {:?} {:?} at {}",
            function.name(),
            function.linkage_name(),
            function.language(),
            place(function.declaration())
        ));
    }

    for global in image.globals() {
        let ty = match &global.type_info {
            microscope::GlobalVariableType::Resolved(info) => info.name.to_string(),
            other => format!("{other:?}"),
        };

        let name = if global.qualified_name.starts_with("<anonymous global at ") {
            "<anonymous global>"
        } else {
            &global.qualified_name
        };

        described.insert(format!(
            "global {name} {:?} {ty} at {}",
            global.linkage_name,
            place(global.declaration.clone())
        ));
    }

    for node in image.types() {
        described.insert(match node {
            microscope::TypeNode::Resolved(info) => {
                let kind = format!("{:?}", info.kind);
                let kind = kind.split([' ', '(', '{']).next().unwrap_or_default();
                format!("type {} {:?} {kind}", info.name, info.byte_size)
            }
            malformed => format!("{malformed:?}"),
        });
    }

    for row in image.statement_rows() {
        described.insert(format!(
            "row {:#x} {} {} {:?}",
            row.address.get(),
            place(row.location),
            row.discriminator,
            row.flags
        ));
    }

    described
}

#[tokio::test]
async fn sharing_through_a_supplementary_file_changes_nothing_the_debug_information_says() {
    for variant in ["gcc-o0", "gcc-o2", "clang-o2"] {
        for (module, layout) in [
            ("shapes", "dwz"),
            ("libshapes.so", "dwz"),
            ("shapes", "dwarf5"),
            ("libshapes.so", "dwarf5"),
        ] {
            let load = async |layout: &str| {
                let path = dwz(&format!("{variant}/{layout}/{module}"));
                let scenario = Scenario::new(format!("{variant}-{layout}-{module}"), path);
                let described = described(&scenario.handle().module_image());
                scenario.shutdown().await;
                described
            };

            let expected = load("plain").await;
            let found = load(layout).await;
            assert!(
                expected.iter().any(|line| line.contains("/shapes.h:")),
                "{variant} {module} describes what its header shares"
            );
            assert!(
                expected == found,
                "{variant} {layout} {module} differs\nonly without dwz: {:#?}\nonly with dwz: {:#?}",
                expected.difference(&found).collect::<Vec<_>>(),
                found.difference(&expected).collect::<Vec<_>>()
            );
        }
    }
}

async fn frame_places(scenario: &Scenario) -> Vec<(String, String)> {
    let trace = scenario
        .operation("backtrace", scenario.handle().backtrace())
        .await;
    let mut places = Vec::new();
    for frame in trace.frames.iter() {
        let (Some(function), Some(source), Some(module)) =
            (&frame.function, &frame.source, frame.module)
        else {
            continue;
        };

        let image = scenario
            .operation(
                "module image",
                scenario.handle().loaded_module_image(module),
            )
            .await;
        let file = image.source_file(source.file).expect("a source file");
        places.push((
            function.name.to_string(),
            file.path
                .file_name()
                .expect("a file name")
                .to_string_lossy()
                .into_owned(),
        ));
    }

    places
}

async fn evaluated(scenario: &Scenario, text: &str) -> microscope::VariableValue {
    let expression =
        microscope::Expression::parse(text).unwrap_or_else(|error| panic!("`{text}`: {error}"));
    match scenario
        .operation(text, scenario.handle().evaluate(&expression))
        .await
    {
        microscope::Evaluation::Value { value, .. } => available_value(&value.state).clone(),
        other => panic!("`{text}` is not a value: {other:?}"),
    }
}

#[tokio::test]
async fn debug_files_read_what_they_share_from_their_supplementary_file() {
    let program = dwz("gcc-o2/split/shapes");
    let library = dwz("gcc-o2/split/libshapes.so");
    let options = DebugFileOptions {
        directories: vec![dwz_debug_root()],
        ..DebugFileOptions::default()
    };

    let mut scenario = Scenario::with_debug_files("dwz", &program, &options);
    let image = scenario.handle().module_image();
    assert_eq!(
        image.debug_file().map(|path| path.as_path()),
        Some(filed(&dwz_debug_root(), &build_id(&program)).as_path())
    );
    let breakpoint = pending_function(&scenario, "shapes::measure").await;
    let reason = scenario.run_to_stop().await;
    let StopReason::Breakpoint { hits, .. } = &reason else {
        panic!("stopped for {reason:?}");
    };

    assert_eq!(hits[0].breakpoint, breakpoint.id);
    let trace = scenario
        .operation("backtrace", scenario.handle().backtrace())
        .await;
    let library_image = scenario
        .operation(
            "library image",
            scenario
                .handle()
                .loaded_module_image(trace.frames[0].module.expect("a module")),
        )
        .await;
    assert_eq!(
        library_image.debug_file().map(|path| path.as_path()),
        Some(filed(&dwz_debug_root(), &build_id(&library)).as_path())
    );

    assert!(matches!(
        evaluated(&scenario, "shape.opposite_.y").await,
        microscope::VariableValue::Scalar(ScalarValue::Signed(4))
    ));
    assert!(matches!(
        evaluated(&scenario, "shapes::shapes_measured").await,
        microscope::VariableValue::Scalar(ScalarValue::Signed(0))
    ));

    let width = source_line("tests/fixtures/cpp/shapes/shapes.h", "shapes: width");
    scenario
        .add_source_breakpoint("tests/fixtures/cpp/shapes/shapes.h", width)
        .await;
    scenario.resume_to_stop().await;
    scenario.step_to_stop(StepKind::IntoSource).await;
    let places = frame_places(&scenario).await;
    let named = |function: &str, file: &str| (function.to_owned(), file.to_owned());
    assert_eq!(
        places[..4],
        [
            named("length", "shapes.h"),
            named("width", "shapes.h"),
            named("area", "geometry.cpp"),
            named("measure", "library.cpp"),
        ],
        "{places:?}"
    );
    let trace = scenario
        .operation("backtrace", scenario.handle().backtrace())
        .await;
    scenario
        .operation(
            "select width",
            scenario.handle().select_frame(trace.frames[1].id),
        )
        .await;
    let variables = scenario
        .operation("variables", scenario.handle().variables())
        .await;
    let across = variables
        .variables
        .iter()
        .find(|variable| &*variable.name == "across")
        .unwrap_or_else(|| panic!("no `across` in {:?}", variables.variables));
    assert_eq!(
        across.type_info.as_ref().map(|info| &*info.name),
        Some("Span<int>")
    );
    let module_image = scenario
        .operation(
            "module image",
            scenario
                .handle()
                .loaded_module_image(trace.frames[1].module.expect("a module")),
        )
        .await;
    let declaration = across.declaration.as_ref().expect("a declaration");
    assert_eq!(
        module_image
            .source_file(declaration.file)
            .map(|file| file.path.as_path()),
        Some(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/cpp/shapes/shapes.h")
                .as_path()
        )
    );
    scenario.shutdown().await;
}

#[tokio::test]
async fn a_supplementary_file_must_be_the_one_its_debug_files_name() {
    let program = dwz("gcc-o2/split/shapes");
    let supplementary = dwz_debug_root().join(".dwz/shapes");
    let program_id = build_id(&program);
    let debug_file = filed(&dwz_debug_root(), &program_id);
    let scratch = ScratchDir::new("dwz-supplementary");

    let root = scratch.path().join("root");
    let copied = filed(&root, &program_id);
    fs::create_dir_all(copied.parent().expect("a directory")).expect("the build-id directory");
    fs::copy(&debug_file, &copied).expect("copy the debug file");
    fs::create_dir_all(root.join(".dwz")).expect("the .dwz directory");
    fs::copy(dwz("clang-o2/dwz/.dwz/shapes"), root.join(".dwz/shapes"))
        .expect("copy another supplementary file");
    let refused = |scenario: &Scenario| {
        let image = scenario.handle().module_image();
        assert_eq!(image.functions().len(), 0);
        let Some(microscope::DebugFile::Unusable { path, reason }) = image.separate_debug_file()
        else {
            panic!("{:?}", image.separate_debug_file());
        };

        assert_eq!(path.as_path(), copied.as_path());
        assert!(
            reason.starts_with("its dwz supplementary file ../../.dwz/shapes (build-id ")
                && reason.ends_with(") was not found"),
            "{reason}"
        );
    };

    let options = |directories: Vec<PathBuf>| DebugFileOptions {
        directories,
        ..DebugFileOptions::default()
    };

    let scenario =
        Scenario::with_debug_files("dwz-misnamed", &program, &options(vec![root.clone()]));
    refused(&scenario);
    scenario.shutdown().await;

    let other = scratch.path().join("other");
    let filed_supplementary = filed(&other, &build_id(&supplementary));
    fs::create_dir_all(filed_supplementary.parent().expect("a directory"))
        .expect("the build-id directory");
    fs::copy(&supplementary, &filed_supplementary).expect("copy the supplementary file");
    let scenario = Scenario::with_debug_files(
        "dwz-by-build-id",
        &program,
        &options(vec![root.clone(), other]),
    );
    let image = scenario.handle().module_image();
    assert_eq!(
        image.debug_file().map(|path| path.as_path()),
        Some(copied.as_path())
    );
    assert!(image.functions().any(|function| function.name() == "area"));
    scenario.shutdown().await;

    let cache = ScratchDir::new("dwz-debuginfod-cache");
    let server = Server::serving(vec![
        (
            program_id.clone(),
            fs::read(&debug_file).expect("the debug file"),
        ),
        (
            build_id(&supplementary),
            fs::read(&supplementary).expect("the supplementary file"),
        ),
    ]);
    let downloading = DebugFileOptions {
        debuginfod: true,
        debuginfod_urls: Some(vec![server.url.clone()]),
        debuginfod_cache: Some(cache.path().to_path_buf()),
        ..DebugFileOptions::default()
    };

    let scenario = Scenario::with_debug_files("dwz-debuginfod", &program, &downloading);
    assert_eq!(server.requests.load(Ordering::SeqCst), 2);
    let image = scenario.handle().module_image();
    assert!(image.functions().any(|function| function.name() == "area"));
    assert_eq!(
        fs::read(
            cache
                .path()
                .join(build_id(&supplementary))
                .join("debuginfo")
        )
        .expect("the cached supplementary file"),
        fs::read(&supplementary).expect("the supplementary file")
    );
    scenario.shutdown().await;
}

#[tokio::test]
async fn a_debug_file_whose_supplementary_file_is_missing_is_refused_with_its_reason() {
    let options = DebugFileOptions {
        directories: vec![split("altlink-root")],
        ..DebugFileOptions::default()
    };

    let mut scenario = Scenario::with_debug_files("altlink", split("basic-build-id"), &options);
    let image = scenario.handle().module_image();
    assert_eq!(image.functions().len(), 0);
    assert_eq!(image.debug_file(), None);
    let Some(microscope::DebugFile::Unusable { path, reason }) = image.separate_debug_file() else {
        panic!("{:?}", image.separate_debug_file());
    };

    assert!(path.starts_with(split("altlink-root")), "{path:?}");
    assert_eq!(
        &**reason,
        "its dwz supplementary file ../../.dwz/microscope-fixture (build-id 01020304) was \
         not found"
    );
    assert!(matches!(
        scenario.run_to_stop().await,
        StopReason::Exited(_)
    ));
    scenario.shutdown().await;

    let scratch = ScratchDir::new("dwz-own-missing");
    let program = scratch.path().join("shapes");
    fs::copy(dwz("gcc-o0/dwz/shapes"), &program).expect("copy the program");
    let scenario = Scenario::new("dwz-own-missing", &program);
    let image = scenario.handle().module_image();
    let microscope::DebugInformation::Unusable { reason } = image.debug_information() else {
        panic!("{:?}", image.debug_information());
    };

    assert!(
        reason.starts_with("its dwz supplementary file .dwz/shapes (build-id ")
            && reason.ends_with(") was not found"),
        "{reason}"
    );
    assert_eq!(image.functions().len(), 0);
    assert!(
        image
            .symbols()
            .any(|symbol| symbol.name() == "_ZN6shapes4areaERKNS_5ShapeE")
    );
    scenario.shutdown().await;
}

#[tokio::test]
async fn a_supplementary_file_that_is_no_regular_file_is_not_read() {
    let scratch = ScratchDir::new("dwz-fifo");
    let program = scratch.path().join("shapes");
    fs::copy(dwz("gcc-o0/dwz/shapes"), &program).expect("copy the program");
    fs::create_dir(scratch.path().join(".dwz")).expect("the .dwz directory");
    nix::unistd::mkfifo(
        &scratch.path().join(".dwz/shapes"),
        nix::sys::stat::Mode::S_IRWXU,
    )
    .expect("a FIFO");

    let scenario = Scenario::new("dwz-fifo", &program);
    let microscope::DebugInformation::Unusable { reason } =
        scenario.handle().module_image().debug_information()
    else {
        panic!("{:?}", scenario.handle().module_image().debug_information());
    };
    assert!(reason.ends_with(") was not found"), "{reason}");
    scenario.shutdown().await;
}

fn debug_sup_checksum(path: &Path) -> String {
    let data = fs::read(path).expect("read the file");
    let object = object::File::parse(data.as_slice()).expect("an object file");
    let section = object::Object::section_by_name(&object, ".debug_sup").expect("a .debug_sup");
    let bytes = object::ObjectSection::data(&section).expect("its bytes");
    let name = bytes[3..]
        .iter()
        .position(|&byte| byte == 0)
        .expect("a name")
        + 3;
    let length = usize::from(bytes[name + 1]);
    assert!(length < 0x80, "a one-byte length");

    bytes[name + 2..name + 2 + length]
        .iter()
        .fold(String::new(), |mut text, byte| {
            use std::fmt::Write as _;
            write!(text, "{byte:02x}").expect("writing to a String cannot fail");
            text
        })
}

#[tokio::test]
async fn a_debug_sup_names_its_supplementary_file_by_checksum() {
    let scratch = ScratchDir::new("debug-sup");
    let program = scratch.path().join("shapes");
    fs::copy(dwz("gcc-o2/dwarf5/shapes"), &program).expect("copy the program");
    fs::create_dir(scratch.path().join(".dwz")).expect("the .dwz directory");
    fs::copy(
        dwz("clang-o2/dwarf5/.dwz/shapes"),
        scratch.path().join(".dwz/shapes"),
    )
    .expect("copy another build's supplementary file");
    let checksum = debug_sup_checksum(&program);
    let scenario = Scenario::new("debug-sup-missing", &program);
    assert_eq!(
        scenario.handle().module_image().debug_information(),
        microscope::DebugInformation::Unusable {
            reason: format!(
                "its supplementary file .dwz/shapes (checksum {checksum}) was not found"
            )
            .into()
        }
    );
    scenario.shutdown().await;

    let root = scratch.path().join("root");
    let filed = filed(&root, &checksum);
    fs::create_dir_all(filed.parent().expect("a directory")).expect("the build-id directory");
    fs::copy(dwz("gcc-o2/dwarf5/.dwz/shapes"), &filed).expect("file the supplementary file");
    let options = DebugFileOptions {
        directories: vec![root],
        ..DebugFileOptions::default()
    };

    let scenario = Scenario::with_debug_files("debug-sup", &program, &options);
    let image = scenario.handle().module_image();
    assert!(image.functions().any(|function| function.name() == "area"));
    scenario.shutdown().await;
}
