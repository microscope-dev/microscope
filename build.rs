//! Embeds the web UI's built files in release builds of microscope, names the
//! sources the loader was built from, and compiles the syscall tracer's BPF
//! programs with the flake's clang.
//!
//! Development builds read `build/web` at run time instead, so rebuilding
//! the page never rebuilds the crate. Node is never needed to build microscope:
//! a missing `build/web` embeds nothing, and the page says to run
//! `just web`.

use std::ffi::OsString;
use std::fmt::Write as _;
use std::hash::Hasher as _;
use std::path::{Path, PathBuf};

fn main() {
    println!("cargo::rerun-if-changed=build.rs");
    let manifest = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").expect("cargo sets it"));
    sources_digest(&manifest);
    let out = PathBuf::from(std::env::var_os("OUT_DIR").expect("cargo sets it"));
    bpf_programs(&manifest, &out);
    let root = manifest.join("build/web");
    let mut files = Vec::new();
    if std::env::var("PROFILE").as_deref() == Ok("release") {
        println!("cargo::rerun-if-changed={}", root.display());
        collect(&root, &root, &mut files);
        files.sort();
    }

    let mut table = String::from("&[\n");

    for (name, path) in files {
        writeln!(
            table,
            "    ({name:?}, include_bytes!({path:?}).as_slice()),"
        )
        .expect("writing to a string");
    }

    table.push(']');

    std::fs::write(out.join("web_assets.rs"), table).expect("write the asset table");
}

/// Digests the library's source files and the locked dependencies, so that
/// images a cache holds are never read by a loader built from other sources:
/// any change may change what a load produces. The binary's own modules are
/// left out, so editing them rebuilds neither the library nor its tests.
fn sources_digest(manifest: &Path) {
    let mut files = Vec::new();
    let binary_only = ["bin", "cli", "dap", "main.rs", "present", "web"];
    let mut roots = vec!["Cargo.lock".to_owned()];

    for entry in std::fs::read_dir(manifest.join("src"))
        .expect("read src")
        .flatten()
    {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !binary_only.contains(&name.as_str()) {
            roots.push(format!("src/{name}"));
        }
    }

    for root in &roots {
        println!("cargo::rerun-if-changed={root}");
        let path = manifest.join(root);
        if path.is_dir() {
            collect(manifest, &path, &mut files);
        } else {
            files.push((root.clone(), path.to_string_lossy().into_owned()));
        }
    }

    files.sort();

    // SipHash with fixed keys: stable for one toolchain, and a toolchain
    // that changes it only empties caches.
    let mut digest = std::hash::DefaultHasher::new();
    for (name, path) in files {
        let contents = std::fs::read(&path).expect("read a source file");
        digest.write(name.as_bytes());
        digest.write_usize(contents.len());
        digest.write(&contents);
    }

    println!(
        "cargo::rustc-env=MICROSCOPE_SOURCES_DIGEST={:016x}",
        digest.finish()
    );
}

/// Compiles `src/backend/linux/bpf/*.bpf.c` into `OUT_DIR`, with the
/// unwrapped clang and only the headers the flake names, so that the objects
/// depend on nothing of the host's. Outside the dev shell the build stops
/// and says which variable is missing; it never looks for another compiler.
///
/// Every edit to the library reruns this script, since the sources digest
/// covers it, so the objects are rebuilt only when what clang reads or how
/// it is run changed: the compiler, its flags, and the C files beside the
/// programs. The headers outside come from store paths the flags name,
/// which change with their contents.
fn bpf_programs(manifest: &Path, out: &Path) {
    let directory = manifest.join("src/backend/linux/bpf");
    println!("cargo::rerun-if-changed={}", directory.display());
    let variable = |name: &str| {
        println!("cargo::rerun-if-env-changed={name}");
        std::env::var_os(name).unwrap_or_else(|| {
            panic!(
                "{name} is not set: build microscope inside its dev shell (`just dev`), \
                 which names the clang and headers the BPF programs are built with"
            )
        })
    };

    let clang = variable("MICROSCOPE_BPF_CLANG");
    let include = variable("MICROSCOPE_BPF_INCLUDE");
    let mut flags: Vec<OsString> = [
        "-target",
        "bpf",
        "-O2",
        "-g",
        "-Wall",
        "-Werror",
        "-nostdinc",
    ]
    .map(OsString::from)
    .into();
    flags.push(format!("-fdebug-prefix-map={}=.", manifest.display()).into());

    for path in std::env::split_paths(&include) {
        flags.push("-I".into());
        flags.push(path.into());
    }

    let mut files: Vec<PathBuf> = std::fs::read_dir(&directory)
        .expect("read the BPF sources")
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_none_or(|extension| extension != "rs"))
        .collect();
    files.sort();

    let mut digest = std::hash::DefaultHasher::new();
    for part in std::iter::once(&clang).chain(&flags) {
        digest.write_usize(part.len());
        digest.write(part.as_encoded_bytes());
    }

    for path in &files {
        let contents = std::fs::read(path).expect("read a BPF source");
        let name = path.file_name().expect("a file");
        digest.write_usize(name.len());
        digest.write(name.as_encoded_bytes());
        digest.write_usize(contents.len());
        digest.write(&contents);
    }

    // The bpf-walk-timing feature builds a second sampler that times each
    // walk of JavaScript frames, which only a measurement loads.
    let timing = std::env::var_os("CARGO_FEATURE_BPF_WALK_TIMING").is_some();
    digest.write_u8(u8::from(timing));

    let inputs = format!("{:016x}", digest.finish());
    let mut sources: Vec<(&PathBuf, PathBuf, &[&str])> = files
        .iter()
        .filter(|path| path.to_string_lossy().ends_with(".bpf.c"))
        .map(|source| {
            let name = source.file_name().expect("a file").to_string_lossy();
            (source, out.join(name.replace(".bpf.c", ".bpf.o")), &[][..])
        })
        .collect();
    if timing && let Some(sampler) = files.iter().find(|path| path.ends_with("sampler.bpf.c")) {
        sources.push((
            sampler,
            out.join("sampler-timed.bpf.o"),
            &["-DMICROSCOPE_WALK_TIMING"][..],
        ));
    }
    let stamp = out.join("bpf-inputs");
    let built = std::fs::read_to_string(&stamp).is_ok_and(|built| built == inputs);
    if built && sources.iter().all(|(_, object, _)| object.exists()) {
        return;
    }

    // An interrupted build leaves no stamp, so the next one starts over.
    if stamp.exists() {
        std::fs::remove_file(&stamp).expect("remove the BPF stamp");
    }

    for (source, object, defines) in sources {
        let status = std::process::Command::new(&clang)
            .args(&flags)
            .args(defines)
            .arg("-c")
            .arg(source)
            .arg("-o")
            .arg(&object)
            .status()
            .unwrap_or_else(|error| panic!("run {}: {error}", Path::new(&clang).display()));
        assert!(status.success(), "compiling {} failed", source.display());
    }

    std::fs::write(&stamp, inputs).expect("write the BPF stamp");
}

fn collect(root: &Path, directory: &Path, files: &mut Vec<(String, String)>) {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect(root, &path, files);
        } else if let Ok(relative) = path.strip_prefix(root) {
            files.push((
                relative.to_string_lossy().into_owned(),
                path.to_string_lossy().into_owned(),
            ));
        }
    }
}
