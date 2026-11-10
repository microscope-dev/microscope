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

/// Every error number prints a name of its own, so a printed result names
/// one error.
#[test]
fn every_error_prints_a_name_of_its_own() {
    let mut seen = std::collections::BTreeMap::new();
    for number in 1..=4095 {
        let (name, _) = errno(Errno(number));
        if let Some(other) = seen.insert(name.clone(), number) {
            panic!("{name} names both {other} and {number}");
        }
    }
}

/// Found by `constants_read_back_as_their_value`: futex's modifier bits
/// past the two it names were dropped.
#[test]
fn futex_shows_the_bits_it_does_not_name() {
    let mut out = Tokens::default();
    enumeration(&mut out, "futex", 0x8000_0000, &Style::default());
    assert_eq!(out.text(), "FUTEX_WAIT|0x80000000");

    let mut out = Tokens::default();
    enumeration(
        &mut out,
        "futex",
        0x200 | 0x100 | 0x80 | 9,
        &Style::default(),
    );
    assert_eq!(
        out.text(),
        "FUTEX_WAIT_BITSET_PRIVATE|FUTEX_CLOCK_REALTIME|0x200"
    );
}

mod properties {
    use proptest::prelude::*;

    use super::*;

    /// The names a printed constant may hold, with their values: the
    /// set's own and those `flags` adds for it.
    fn names_of(set: &str) -> Vec<(u64, &'static str)> {
        let mut names = sets::set(set).expect("a set").names.to_vec();
        match set {
            "open" => {
                names.extend(sets::ACCESS_MODES);
                names.extend([(0o4_010_000, "O_SYNC"), (0o20_200_000, "O_TMPFILE")]);
            }
            "mmap_flags" => names.extend(sets::MAP_TYPES),
            "sock_type" => names.extend(sets::SOCK_TYPES),
            "access" => names.push((0, "F_OK")),
            "mmap_prot" => names.push((0, "PROT_NONE")),
            _ => {}
        }

        names
    }

    /// The value one `|`-separated part of a constant stands for.
    fn read_part(set: &str, part: &str) -> u64 {
        if let Some(hex) = part.strip_prefix("0x") {
            return u64::from_str_radix(hex, 16).expect("hex");
        }

        if let Ok(number) = part.parse::<i32>() {
            return u64::from(number.cast_unsigned());
        }

        let known = names_of(set);
        let value_of = |name: &str| {
            known
                .iter()
                .find(|&&(_, n)| n == name)
                .map(|&(value, _)| value)
        };

        match set {
            "clone" if part.starts_with("SIG") => {
                u64::try_from(signal_named(part).expect("a signal")).expect("positive")
            }
            "futex" if part == "FUTEX_CLOCK_REALTIME" => 256,
            "futex" if part.ends_with("_PRIVATE") => {
                128 | value_of(part.strip_suffix("_PRIVATE").expect("a suffix")).expect(part)
            }
            _ => value_of(part).unwrap_or_else(|| panic!("{part} is not in {set}")),
        }
    }

    /// The value a printed constant stands for, in any `-X` style.
    fn read_constant(set: &str, text: &str) -> u64 {
        // -X verbose: the number, then the names in a comment.
        if let Some((number, names)) = text.split_once(" /* ") {
            let number = read_part(set, number);
            let names = names.strip_suffix(" */").expect("a comment");
            assert_eq!(read_constant(set, names), number, "{text}");
            return number;
        }

        text.split('|')
            .fold(0, |all, part| all | read_part(set, part))
    }

    fn sets_used() -> Vec<&'static str> {
        let mut sets: Vec<_> = microscope::syscalls::specs::sets().collect();
        sets.sort_unstable();
        sets.dedup();
        sets
    }

    fn u64_edges() -> impl Strategy<Value = u64> {
        prop_oneof![
            prop::sample::select(vec![
                0,
                1,
                u64::MAX,
                u64::MAX - 1,
                0x8000_0000,
                0xffff_ffff,
                1 << 32
            ]),
            0_u64..1024,
            any::<u64>(),
        ]
    }

    /// A set and a word of it: some of its names together, and sometimes
    /// any other bits.
    fn set_and_word() -> impl Strategy<Value = (&'static str, u64)> {
        prop::sample::select(sets_used()).prop_flat_map(|set| {
            let names: Vec<u64> = names_of(set).iter().map(|&(value, _)| value).collect();
            let chosen = prop::collection::vec(prop::sample::select(names), 0..4);
            let noise = prop_oneof![
                Just(0_u64),
                (0_u32..64).prop_map(|bit| 1 << bit),
                u64_edges(),
            ];
            (Just(set), chosen, noise).prop_map(|(set, chosen, noise)| {
                (set, chosen.iter().fold(noise, |all, value| all | value))
            })
        })
    }

    fn styles() -> [Style; 3] {
        [
            Style::default(),
            Style {
                constants: Constants::Raw,
                ..Style::default()
            },
            Style {
                constants: Constants::Verbose,
                ..Style::default()
            },
        ]
    }

    /// Bytes back from a string `quote` printed, and whether `...`
    /// followed it.
    fn unquote(text: &str) -> (Vec<u8>, bool) {
        let (text, cut) = text
            .strip_suffix("...")
            .map_or((text, false), |text| (text, true));
        let inner = text
            .strip_prefix('"')
            .and_then(|text| text.strip_suffix('"'))
            .expect("quotes");
        let mut bytes = Vec::new();
        let mut chars = inner.bytes().peekable();

        while let Some(byte) = chars.next() {
            if byte != b'\\' {
                assert!(
                    (0x20..0x7f).contains(&byte) && byte != b'"',
                    "{byte:#x} unescaped in {text}"
                );
                bytes.push(byte);
                continue;
            }

            let escaped = chars.next().expect("an escape");
            bytes.push(match escaped {
                b'"' | b'\\' => escaped,
                b'n' => b'\n',
                b't' => b'\t',
                b'r' => b'\r',
                b'f' => 0x0c,
                b'v' => 0x0b,
                b'x' => {
                    let digits: String = [chars.next(), chars.next()]
                        .into_iter()
                        .flatten()
                        .map(char::from)
                        .collect();
                    u8::from_str_radix(&digits, 16).expect("two hex digits")
                }
                b'0'..=b'7' => {
                    let mut value = u32::from(escaped - b'0');
                    for _ in 0..2 {
                        match chars.peek() {
                            Some(&digit @ b'0'..=b'7') => {
                                value = value * 8 + u32::from(digit - b'0');
                                chars.next();
                            }
                            _ => break,
                        }
                    }
                    u8::try_from(value).expect("a byte")
                }
                other => panic!("unknown escape \\{}", char::from(other)),
            });
        }

        (bytes, cut)
    }

    /// The raw value a printed result stands for, or the error it names.
    fn read_result(text: &str) -> Result<i64, Errno> {
        let text = text.strip_prefix(" = ").expect("a result");
        if let Some((sign, error)) = text.split_once(' ')
            && (sign == "-1" || sign == "?")
        {
            let (name, _) = error.split_once(" (").expect("a message");
            let number = (1..=4095)
                .find(|&number| errno(Errno(number)).0 == name)
                .expect(name);
            assert_eq!(sign == "?", restarts(Errno(number)), "{text}");
            return Err(Errno(number));
        }

        // A descriptor may carry what it refers to.
        let number = text.split_once('<').map_or(text, |(number, _)| number);
        if let Some(hex) = number.strip_prefix("0x") {
            return Ok(u64::from_str_radix(hex, 16).expect("hex").cast_signed());
        }

        if number.len() > 1 && number.starts_with('0') {
            return Ok(u64::from_str_radix(&number[1..], 8)
                .expect("octal")
                .cast_signed());
        }

        Ok(number.parse().expect("a decimal"))
    }

    /// Any number under any ABI, mostly ones the tables list, with x32's
    /// own calls.
    fn call_id() -> impl Strategy<Value = SyscallId> {
        prop_oneof![
            (0_usize..3, 0_u32..480)
                .prop_map(|(abi, number)| SyscallId::new(Abi::ALL[abi], number)),
            (512_u32..548).prop_map(|number| SyscallId::new(Abi::X32, number)),
            any::<u32>().prop_map(|number| SyscallId::new(Abi::X86_64, number)),
        ]
    }

    /// Results: errors, small descriptors and counts, and the edges.
    fn result() -> impl Strategy<Value = i64> {
        prop_oneof![
            -4095_i64..0,
            0_i64..64,
            prop::sample::select(vec![i64::MIN, i64::MAX, -4096, 0x7f00_0000_0000]),
            any::<i64>(),
        ]
    }

    proptest! {
        // Futex is one set of about fifty, so its bad bits need more cases.
        #![proptest_config(ProptestConfig::with_cases(1024))]

        /// A flag or enumeration constant reads back, in every `-X`
        /// style, as the argument's value, cut to the set's width.
        #[test]
        fn constants_read_back_as_their_value((set, word) in set_and_word()) {
            let definition = sets::set(set).expect("a set");
            let value = if definition.narrow { word & 0xffff_ffff } else { word };
            let call = if set == "clone" { "clone" } else { "getpid" };
            let event = event(call, &[], 0, &[]);

            for style in styles() {
                let mut out = Tokens::default();
                match definition.shape {
                    sets::Shape::Flags => flags(&mut out, &event, set, word, &style),
                    sets::Shape::Enum => enumeration(&mut out, set, word, &style),
                }

                let text = out.text();
                prop_assert_eq!(read_constant(set, &text), value, "{} {:?}: {}", set, style.constants, text);
            }
        }

        /// A quoted string unescapes to the bytes shown, and `...`
        /// follows it exactly when it was cut.
        #[test]
        fn quoted_strings_unescape_to_the_bytes_shown(
            bytes in prop::collection::vec(
                prop_oneof![any::<u8>(), prop::sample::select(b"0123456789\"\\\n\x01\x7f".to_vec())],
                0..48,
            ),
            limit in prop::option::of(0_usize..40),
            truncated in any::<bool>(),
            hex in prop::sample::select(vec![Hex::Never, Hex::NonAscii, Hex::All]),
        ) {
            let mut out = Tokens::default();
            quote(&mut out, &bytes, limit, truncated, &Style { hex, ..Style::default() });

            let shown = &bytes[..limit.map_or(bytes.len(), |limit| limit.min(bytes.len()))];
            let (read, cut) = unquote(&out.text());
            prop_assert_eq!(read, shown);
            prop_assert_eq!(cut, truncated || shown.len() < bytes.len());
        }

        /// Any call with any words and captured bytes prints a line that
        /// ends in its result, which reads back as the raw return value or
        /// the error.
        #[test]
        fn results_read_back_as_the_outcome(
            call in call_id(),
            args in proptest::array::uniform6(u64_edges()),
            result in result(),
            bytes in prop::collection::vec(any::<u8>(), 0..300),
            truncated in any::<bool>(),
        ) {
            let captured = (0..6u8)
                .map(|arg| Capture {
                    arg,
                    when: ReadAt::Exit,
                    state: if truncated { CaptureState::Truncated { wanted: 400 } } else { CaptureState::Complete },
                    bytes: Arc::from(&bytes[(usize::from(arg) * 7).min(bytes.len())..]),
                })
                .collect();
            let event = SyscallEvent { call, args, captured, outcome: Outcome::of_return(result), ..event("getpid", &[], 0, &[]) };
            let fds = FdTable::default();

            for style in [
                Style::default(),
                Style { verbose: true, hex: Hex::NonAscii, fds: FdStyle::All, arg_names: true, ..Style::default() },
                Style { constants: Constants::Verbose, string_limit: 0, raw: true, ..Style::default() },
            ] {
                let context = Context { fds: Some(&fds), before: false };
                let line = super::call(&event, &style, context).text();
                let tail = returned(&event, &style, context).text();

                prop_assert!(line.starts_with(&format!("{}(", call.display_name())), "{}", line);
                prop_assert!(line.ends_with(&tail), "{} does not end in {}", line, tail);
                let expected = match event.outcome {
                    Outcome::Failed(error) => Err(error),
                    _ => Ok(result),
                };
                prop_assert_eq!(read_result(&tail), expected, "{}", tail);
            }
        }
    }
}
