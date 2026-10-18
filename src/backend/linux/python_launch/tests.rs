use super::*;

/// A request waits in the FIFO for the bootstrap; one no thread took is
/// read back out, and the directory goes with its owner.
#[test]
fn requests_wait_until_taken_or_withdrawn_and_the_directory_is_private() {
    let launch = PythonLaunch::create().expect("create the bootstrap");
    let directory = launch.directory.clone();
    let stat = std::fs::metadata(&directory).expect("the directory");
    assert_eq!(
        std::os::unix::fs::PermissionsExt::mode(&stat.permissions()) & 0o777,
        0o700
    );
    assert_eq!(
        std::fs::read_to_string(directory.join("sitecustomize.py")).expect("the bootstrap"),
        SITECUSTOMIZE
    );

    launch
        .launched(Pid::from_raw(42))
        .expect("name the process");
    assert_eq!(
        std::fs::read_to_string(directory.join("pid")).expect("the pid"),
        "42"
    );
    launch.request("/tmp/a.py").expect("request");
    launch.request("/tmp/b.py").expect("request");
    assert!(launch.withdraw("/tmp/b.py").expect("withdraw"));
    assert!(
        !launch.withdraw("/tmp/b.py").expect("withdraw again"),
        "a withdrawn request is gone"
    );
    assert!(launch.request("relative.py").is_err());
    assert!(launch.request("/tmp/two\nlines.py").is_err());

    // A full FIFO refuses rather than blocking the controller.
    let long = format!("/{}", "x".repeat(4000));
    let refused = (0..64).find_map(|_| launch.request(&long).err());
    assert!(
        refused.is_some_and(|reason| reason.contains("has not read")),
        "a full FIFO is refused"
    );

    drop(launch);
    assert!(!directory.exists(), "the directory goes with its owner");
}

/// Only CPython 3.12 and 3.13 are bootstrapped: by the libpython they
/// need, or by their own version. Other programs, and files that are not
/// programs, are launched as they are.
#[test]
fn only_cpython_3_12_and_3_13_are_bootstrapped() {
    let root = std::env::var_os("MICROSCOPE_CPYTHON")
        .expect("MICROSCOPE_CPYTHON names the interpreters; run tests in the dev shell");
    let root = Path::new(&root);
    for (build, bootstrapped) in [
        ("3.12", true),
        ("3.13", true),
        ("3.13t", true),
        ("ubuntu-3.12", true),
        ("debian-3.13", true),
        ("3.14", false),
        ("standalone-3.14", false),
        ("3.15", false),
    ] {
        let executable = root.join(build).join("python");
        assert_eq!(
            launches_bootstrapped_python(&executable),
            bootstrapped,
            "{build}: {}",
            executable.display()
        );
    }
    assert!(!launches_bootstrapped_python(Path::new("/bin/sh")));
    assert!(!launches_bootstrapped_python(Path::new("/nonexistent")));

    let mut environment = vec![("PYTHONPATH".into(), Some("/mine".into()))];
    let launch = PythonLaunch::prepare(&root.join("3.12").join("python"), &mut environment)
        .expect("prepare")
        .expect("a bootstrap for 3.12");
    let value = |name: &str| {
        environment
            .iter()
            .rev()
            .find(|(set, _)| set == name)
            .and_then(|(_, value)| value.clone())
    };
    let mut path = launch.directory.as_os_str().to_owned();
    path.push(":/mine");
    assert_eq!(value("PYTHONPATH"), Some(path));
    assert_eq!(value(ORIGINAL_PATH), Some("1/mine".into()));
    assert_eq!(
        value(WAKE),
        Some(launch.directory.join("wake").into_os_string())
    );
}
