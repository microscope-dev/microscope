use std::process::Command;

#[path = "../tests/support/memory_cap.rs"]
pub mod memory_cap;

const OVER_ALLOCATE: &str = "MICROSCOPE_TEST_OVER_ALLOCATE";

#[test]
fn an_allocation_past_the_cap_aborts_the_test_process() {
    if std::env::var_os(OVER_ALLOCATE).is_some() {
        let bytes = vec![1_u8; 2 << 30];
        std::hint::black_box(bytes);
        return;
    }

    let output = Command::new(std::env::current_exe().expect("test executable"))
        .args([
            "--exact",
            "test_memory::an_allocation_past_the_cap_aborts_the_test_process",
            "--nocapture",
        ])
        .env(OVER_ALLOCATE, "1")
        .output()
        .expect("run the test executable");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "the over-allocation succeeded");
    assert!(
        stderr.contains("exceeds the test memory cap"),
        "unexpected stderr: {stderr}"
    );
}

#[test]
fn threads_that_exit_give_back_what_they_reserved() {
    let before = memory_cap::reserved();
    let threads: Vec<_> = (0..1000)
        .map(|_| std::thread::spawn(|| vec![1_u8; 5000]))
        .collect();
    for thread in threads {
        std::hint::black_box(thread.join().expect("an allocating thread"));
    }

    let after = memory_cap::reserved();
    assert!(
        after.abs_diff(before) < 1 << 20,
        "{before} bytes reserved before 1000 threads came and went, {after} after"
    );
}
