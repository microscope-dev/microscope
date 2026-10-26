use std::sync::Arc;

use microscope::syscalls::blobs;
use microscope::syscalls::fds::{Description, FdTable, Kind as FdKind};
use microscope::syscalls::{
    Abi, Capture, CaptureState, EventFlags, Outcome, Pid, ReadAt, SyscallEvent, SyscallId, Tid,
};

use super::*;

fn event(name: &str, args: &[u64], result: i64, captured: &[(u8, &[u8])]) -> SyscallEvent {
    let mut words = [0; 6];
    words[..args.len()].copy_from_slice(args);

    SyscallEvent {
        thread: Tid::new(42),
        process: Pid::new(42),
        call: SyscallId::named(Abi::X86_64, name).expect(name),
        args: words,
        captured: captured
            .iter()
            .map(|&(arg, bytes)| Capture {
                arg,
                when: ReadAt::Exit,
                state: CaptureState::Complete,
                bytes: Arc::from(bytes),
            })
            .collect(),
        outcome: Outcome::of_return(result),
        entered: Some(1_000),
        exited: Some(12_000),
        stack: None,
        flags: EventFlags::default(),
    }
}

fn line(event: &SyscallEvent, style: &Style) -> String {
    call(
        event,
        style,
        Context {
            fds: None,
            before: false,
        },
    )
    .text()
}

const AT_FDCWD: u64 = (-100_i64).cast_unsigned();

#[test]
fn file_calls_print_as_strace_prints_them() {
    let style = Style::default();
    let open = event(
        "openat",
        &[AT_FDCWD, 0x7ff0, 0o2_000_000],
        3,
        &[(1, b"/etc/hostname\0")],
    );
    assert_eq!(
        line(&open, &style),
        r#"openat(AT_FDCWD, "/etc/hostname", O_RDONLY|O_CLOEXEC) = 3"#
    );

    let missing = event(
        "openat",
        &[AT_FDCWD, 0x7ff0, 0o101, 0o644],
        -2,
        &[(1, b"/nope\0")],
    );
    assert_eq!(
        line(&missing, &style),
        r#"openat(AT_FDCWD, "/nope", O_WRONLY|O_CREAT, 0644) = -1 ENOENT (No such file or directory)"#
    );

    let read = event("read", &[3, 0x5000, 4096], 13, &[(1, b"hello,\nworld\x01")]);
    assert_eq!(
        line(&read, &style),
        r#"read(3, "hello,\nworld\1", 4096) = 13"#
    );

    let long = event("write", &[1, 0x5000, 40], 40, &[(1, &[b'a'; 40])]);
    assert_eq!(
        line(&long, &style),
        format!(r#"write(1, "{}"..., 40) = 40"#, "a".repeat(32))
    );

    let mut unreadable = event("openat", &[AT_FDCWD, 0x7ffd_1000, 0], -14, &[]);
    unreadable.captured.push(Capture {
        arg: 1,
        when: ReadAt::Exit,
        state: CaptureState::Unreadable,
        bytes: Arc::from(&[][..]),
    });
    assert_eq!(
        line(&unreadable, &style),
        "openat(AT_FDCWD, 0x7ffd1000 <unreadable>, O_RDONLY) = -1 EFAULT (Bad address)"
    );

    let mut stat = vec![0u8; 144];
    stat[24..28].copy_from_slice(&0o100_644u32.to_le_bytes());
    stat[48..56].copy_from_slice(&412u64.to_le_bytes());
    let fstat = event("fstat", &[3, 0x6000], 0, &[(1, &stat)]);
    assert_eq!(
        line(&fstat, &style),
        "fstat(3, {st_mode=S_IFREG|0644, st_size=412, ...}) = 0"
    );
    let failed = event("fstat", &[9, 0x6000], -9, &[(1, &stat)]);
    assert_eq!(
        line(&failed, &style),
        "fstat(9, 0x6000) = -1 EBADF (Bad file descriptor)"
    );
}

#[test]
fn memory_process_and_network_calls_decode_their_constants() {
    let style = Style::default();
    let mmap = event(
        "mmap",
        &[0, 8192, 3, 0x22, (-1_i64).cast_unsigned(), 0],
        0x7f3a_2c00_0000,
        &[],
    );
    assert_eq!(
        line(&mmap, &style),
        "mmap(NULL, 8192, PROT_READ|PROT_WRITE, MAP_PRIVATE|MAP_ANONYMOUS, -1, 0) = 0x7f3a2c000000"
    );
    let clone = event("clone", &[0x0120_0011, 0, 0, 0x7f00, 0], 4243, &[]);
    assert_eq!(
        line(&clone, &style),
        "clone(child_stack=NULL, flags=CLONE_CHILD_CLEARTID|CLONE_CHILD_SETTID|SIGCHLD, child_tidptr=0x7f00) = 4243"
    );
    let futex = event("futex", &[0x7f3a_2c00_09d0, 128, 0, 0, 0, 0], -110, &[]);
    assert_eq!(
        line(&futex, &style),
        "futex(0x7f3a2c0009d0, FUTEX_WAIT_PRIVATE, 0, NULL) = -1 ETIMEDOUT (Connection timed out)"
    );
    let address = [2, 0, 0x15, 0x38, 10, 0, 3, 9, 0, 0, 0, 0, 0, 0, 0, 0];
    let connect = event("connect", &[7, 0x7000, 16], -115, &[(1, &address)]);
    assert_eq!(
        line(&connect, &style),
        "connect(7, {sa_family=AF_INET, sin_port=htons(5432), sin_addr=inet_addr(\"10.0.3.9\")}, 16) = -1 EINPROGRESS (Operation now in progress)"
    );
    let socket = event("socket", &[2, 0x80801, 0], 7, &[]);
    assert_eq!(
        line(&socket, &style),
        "socket(AF_INET, SOCK_STREAM|SOCK_CLOEXEC|SOCK_NONBLOCK, IPPROTO_IP) = 7"
    );

    let mut argv = Vec::new();
    blobs::start_strings(&mut argv, Some(2));
    blobs::push_string(&mut argv, b"ls");
    blobs::push_string(&mut argv, b"-l");
    let mut envp = Vec::new();
    blobs::start_strings(&mut envp, Some(23));
    let exec = event(
        "execve",
        &[0x1000, 0x2000, 0x3000],
        0,
        &[(0, b"/bin/ls\0"), (1, &argv), (2, &envp)],
    );
    assert_eq!(
        line(&exec, &style),
        r#"execve("/bin/ls", ["ls", "-l"], 0x3000 /* 23 vars */) = 0"#
    );
    let mut exit = event("exit_group", &[0], 0, &[]);
    exit.outcome = Outcome::ProcessExited;
    assert_eq!(line(&exit, &style), "exit_group(0) = ?");
    let restart = event("read", &[0, 0x5000, 10], -512, &[]);
    assert_eq!(
        line(&restart, &style),
        "read(0, 0x5000, 10) = ? ERESTARTSYS (To be restarted if SA_RESTART is set)"
    );
}

#[test]
fn styles_change_descriptors_constants_and_names() {
    let mut table = FdTable::default();
    table.set(
        Pid::new(42),
        3,
        Description {
            text: "/etc/hostname".into(),
            kind: FdKind::File,
            cloexec: false,
        },
    );
    let read = event("read", &[3, 0x5000, 64], 5, &[(1, b"host\n")]);
    let paths = Style {
        fds: FdStyle::Paths,
        ..Style::default()
    };

    assert_eq!(
        call(
            &read,
            &paths,
            Context {
                fds: Some(&table),
                before: false
            }
        )
        .text(),
        r#"read(3</etc/hostname>, "host\n", 64) = 5"#
    );
    let unknown = event("read", &[4, 0x5000, 64], 5, &[(1, b"host\n")]);
    assert_eq!(
        call(
            &unknown,
            &paths,
            Context {
                fds: Some(&table),
                before: false
            }
        )
        .text(),
        r#"read(4<?>, "host\n", 64) = 5"#
    );
    let open = event(
        "openat",
        &[AT_FDCWD, 0x7ff0, 0o2_000_000],
        3,
        &[(1, b"/etc/hostname\0")],
    );
    let raw = Style {
        constants: Constants::Raw,
        ..Style::default()
    };

    assert_eq!(
        line(&open, &raw),
        r#"openat(-100, "/etc/hostname", 0x80000) = 3"#
    );
    let verbose = Style {
        constants: Constants::Verbose,
        ..Style::default()
    };

    assert_eq!(
        line(&open, &verbose),
        r#"openat(-100 /* AT_FDCWD */, "/etc/hostname", 0x80000 /* O_RDONLY|O_CLOEXEC */) = 3"#
    );
    let named = Style {
        arg_names: true,
        ..Style::default()
    };

    assert_eq!(
        line(&open, &named),
        r#"openat(dfd=AT_FDCWD, filename="/etc/hostname", flags=O_RDONLY|O_CLOEXEC) = 3"#
    );
    let hex = Style {
        hex: Hex::All,
        ..Style::default()
    };

    assert_eq!(
        line(&read, &hex),
        r#"read(3, "\x68\x6f\x73\x74\x0a", 64) = 5"#
    );
    let i386 = SyscallEvent {
        call: SyscallId::named(Abi::X86_32, "getpid").expect("getpid"),
        ..event("getpid", &[], 42, &[])
    };

    assert_eq!(line(&i386, &Style::default()), "getpid() = 42");
    let unknown_call = SyscallEvent {
        call: SyscallId::new(Abi::X86_64, 999),
        ..event("getpid", &[1, 2], -38, &[])
    };

    assert_eq!(
        line(&unknown_call, &Style::default()),
        "syscall_0x3e7(0x1, 0x2, 0, 0, 0, 0) = -1 ENOSYS (Function not implemented)"
    );
}

#[test]
fn names_round_trip() {
    assert_eq!(signal(2), "SIGINT");
    assert_eq!(signal(34), "SIGRT_2");
    assert_eq!(signal_named("TERM"), Some(15));
    assert_eq!(signal_named("SIGRT_2"), Some(34));
    assert_eq!(errno_named("EAGAIN"), Some(Errno(11)));
}

#[test]
fn times_print_to_each_precision_cut_as_strace_cuts_them() {
    let time = 1_500_012_345;
    for (name, text) in [
        ("s", "1"),
        ("ms", "1.500"),
        ("us", "1.500012"),
        ("NS", "1.500012345"),
    ] {
        let precision = Precision::named(name).expect(name);
        assert_eq!(seconds(time, precision), text);
    }

    assert_eq!(seconds(987, Precision::Nanoseconds), "0.000000987");
    assert_eq!(seconds(987, Precision::default()), "0.000000");
    assert_eq!(Precision::named("usec"), None);
}

proptest::proptest! {
    #![proptest_config(proptest::prelude::ProptestConfig::with_cases(256))]

    /// Every call decodes any words and any captured bytes without
    /// panicking, under every style, as a fuzzer would check.
    #[test]
    fn any_capture_decodes(
        number in 0u32..480,
        abi in 0usize..3,
        args in proptest::array::uniform6(proptest::prelude::any::<u64>()),
        result in proptest::prelude::any::<i64>(),
        bytes in proptest::collection::vec(proptest::prelude::any::<u8>(), 0..300),
        truncated in proptest::prelude::any::<bool>(),
    ) {
        let call = SyscallId::new(Abi::ALL[abi], number);
        let captured = (0..6u8)
            .map(|arg| Capture {
                arg,
                when: ReadAt::Exit,
                state: if truncated { CaptureState::Truncated { wanted: 400 } } else { CaptureState::Complete },
                bytes: Arc::from(&bytes[(usize::from(arg) * 7).min(bytes.len())..]),
            })
            .collect();
        let event = SyscallEvent { call, args, captured, outcome: Outcome::of_return(result), ..event("getpid", &[], 0, &[]) };

        for style in [
            Style::default(),
            Style { verbose: true, hex: Hex::NonAscii, fds: FdStyle::All, arg_names: true, ..Style::default() },
            Style { constants: Constants::Verbose, string_limit: 0, ..Style::default() },
        ] {
            let text = super::call(&event, &style, Context { fds: Some(&FdTable::default()), before: false }).text();
            proptest::prop_assert!(text.contains('('));
        }
    }
}
