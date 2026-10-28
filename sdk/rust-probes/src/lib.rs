//! Instrumentation that microscope records: zones, frames, plots, messages,
//! labels, and triggers.
//!
//! Macro sites are static data in `microscope_sites`, read from the program's
//! file rather than sent by the program. Sites start off until microscope
//! writes their state byte; an off site costs a load and a branch. An on
//! site writes one or more 16-byte records to its thread's in-process ring,
//! which microscope reads externally. Writers never wait: old records are
//! overwritten if microscope falls behind, and microscope counts the loss.
//! Sites switch themselves off when microscope's heartbeat stops, so the
//! program keeps running if microscope goes away.
//!
//! Without the `enabled` feature every macro expands to nothing, and its
//! arguments are checked but never evaluated.
//!
//! A thread's first event allocates a ring; do not record it in a signal handler.
//!
//! ```ignore
//! fn handle(request: &Request) -> Response {
//!     let _zone = microscope_probes::zone!("handle");
//!     let _route = microscope_probes::label!("route", request.route());
//!     let rows = {
//!         let _zone = microscope_probes::zone!("query");
//!         query(request)
//!     };
//!     microscope_probes::plot!("rows", rows.len() as f64);
//!     if rows.is_empty() {
//!         microscope_probes::message!("no rows for {}", request.id());
//!     }
//!     render(rows)
//! }
//! ```
//!
//! The records in [`abi`] are language-independent.

// Production code returns errors instead of panicking.
// Tests may unwrap.
#![cfg_attr(
    not(test),
    deny(
        clippy::expect_used,
        clippy::panic,
        clippy::todo,
        clippy::unimplemented,
        clippy::unreachable,
        clippy::unwrap_used
    )
)]
#![cfg_attr(not(feature = "enabled"), no_std)]

/// The records microscope reads, by their byte layout. Every field is little
/// endian.
pub mod abi {
    /// The control block, one per program, in `microscope_probe_control`:
    ///
    /// | Offset | Field |
    /// | --- | --- |
    /// | 0 | magic `USCOPEPR` |
    /// | 8 | ABI version, `u16` |
    /// | 10 | clock, `u16`: a `CLOCK_` constant, set by microscope |
    /// | 12 | enable, `u32`: set by microscope: [`ENABLED`], with [`ENABLE_DEFINED`]; or by the program, as [`RECORDER`] says |
    /// | 16 | ring size, `u32`, as a power of two of records: set by microscope |
    /// | 20 | rings, `u32`: how many of the registry's slots threads took |
    /// | 24 | heartbeat, `u64`: microscope's `CLOCK_MONOTONIC`, written as it reads |
    /// | 32 | events no ring could take, `u64` |
    /// | 40 | the registry's length, `u32`: [`MOST_RINGS`] |
    /// | 44 | exiting, `u32`: set as the program exits |
    /// | 48 | flushed, `u32`: set by microscope once it has read every ring since |
    /// | 52 | timeout, `u32`: milliseconds without a heartbeat before every site switches itself off, set by microscope; zero is [`STALE_NANOSECONDS`] |
    /// | 56 | the run-time sites' address, `u64`, or zero before the program defines one (ABI 2) |
    /// | 64 | the registry: each ring's address, `u64`, or zero |
    ///
    /// A thread sets a ring's slot after it counts it in `rings`, so a slot
    /// below `rings` may still be zero. A program that exits while microscope
    /// reads it waits, up to [`EXIT_WAIT_NANOSECONDS`], for microscope to say
    /// it has read every ring, so that the last records are not lost with it.
    pub const MAGIC: [u8; 8] = *b"USCOPEPR";

    /// ABI 2 adds run-time sites; ABI 1 is the same without them.
    pub const ABI: u16 = 2;

    /// The enable word's bit that microscope listens.
    pub const ENABLED: u32 = 1;

    /// The enable word's bit that every run-time site is on as it is
    /// defined, as when microscope records every site (ABI 2).
    pub const ENABLE_DEFINED: u32 = 2;

    /// The environment variable a recording that starts a program and
    /// records every site sets to its own process id. A program whose
    /// parent that is switches every static site on, and sets its enable
    /// word to [`ENABLED`] and [`ENABLE_DEFINED`] and its heartbeat to now,
    /// as it first defines a run-time site or asks whether microscope
    /// listens, so that it records from then on, before microscope has
    /// found it. Its timeout stays zero, which microscope never
    /// writes, and says so. A program microscope never reads switches
    /// itself off once [`STALE_NANOSECONDS`] pass, as any program does.
    /// The program's own children have another parent, and are not on.
    pub const RECORDER: &str = "MICROSCOPE_PROBES_RECORDER";

    /// Record times are nanoseconds of `CLOCK_MONOTONIC`.
    pub const CLOCK_MONOTONIC: u16 = 1;

    /// Record times are the time-stamp counter's.
    pub const CLOCK_TSC: u16 = 2;

    pub const CONTROL_SIZE: usize = 64;

    /// A site, 32 bytes, in `microscope_sites`; its offsets are from the
    /// field that holds them:
    ///
    /// | Offset | Field |
    /// | --- | --- |
    /// | 0 | version, `u8`: [`SITE_VERSION`] |
    /// | 1 | kind, `u8`: a `SITE_` constant |
    /// | 2 | flags, `u16` |
    /// | 4 | line, `u32` |
    /// | 8 | name, `i32` offset, and 12 its length, `u32` |
    /// | 16 | file, `i32` offset, and 20 its length, `u32` |
    /// | 24 | state byte, `i32` offset: zero is off |
    pub const SITE_VERSION: u8 = 1;

    pub const SITE_SIZE: usize = 32;

    /// The SDK's own site, which keeps the section present.
    pub const SITE_NONE: u8 = 0;

    pub const SITE_ZONE: u8 = 1;

    pub const SITE_FRAME: u8 = 2;

    pub const SITE_PLOT: u8 = 3;

    pub const SITE_MESSAGE: u8 = 4;

    pub const SITE_LABEL: u8 = 5;

    pub const SITE_TRIGGER: u8 = 6;

    /// The longest name or file a site may have.
    pub const MOST_SITE_TEXT: u32 = 4096;

    /// Run-time sites, which a program defines as it runs, as a language
    /// that names its zones at run time does: a 16-byte header, then
    /// records of [`RUNTIME_SITE_SIZE`] bytes:
    ///
    /// | Offset | Field |
    /// | --- | --- |
    /// | 0 | magic, `u32`: [`SITES_MAGIC`] |
    /// | 4 | version, `u8`: [`SITES_VERSION`] |
    /// | 8 | capacity, `u32`: how many records follow |
    /// | 12 | count, `u32`: how many are defined, set after each is whole |
    ///
    /// The records are append-only and never change once counted, but for
    /// their state bytes.
    pub const SITES_MAGIC: u32 = u32::from_le_bytes(*b"USST");

    pub const SITES_VERSION: u8 = 1;

    pub const SITES_HEADER: usize = 16;

    /// A run-time site's record:
    ///
    /// | Offset | Field |
    /// | --- | --- |
    /// | 0 | kind, `u8`: a `SITE_` constant |
    /// | 1 | state byte, `u8`: zero is off |
    /// | 2 | flags, `u16` |
    /// | 4 | line, `u32` |
    /// | 8 | name's address, `u64` |
    /// | 16 | name's length, `u32` |
    /// | 20 | file's length, `u32` |
    /// | 24 | file's address, `u64` |
    ///
    /// Its records name it [`RUNTIME_KEY`] plus its index. A static site's
    /// key, its state byte's offset from the control block, lies within
    /// 2^30 of it either way, so the two never meet.
    pub const RUNTIME_SITE_SIZE: usize = 48;

    pub const MOST_RUNTIME_SITES: usize = 4096;

    pub const RUNTIME_KEY: u32 = 1 << 31;

    /// The farthest a static site's state byte may be from the control
    /// block in ABI 2.
    pub const MOST_STATIC_OFFSET: i64 = 1 << 30;

    /// A ring: a 128-byte header, then `2^size` records of 16 bytes:
    ///
    /// | Offset | Field |
    /// | --- | --- |
    /// | 0 | magic, `u32`: [`RING_MAGIC`] |
    /// | 4 | version, `u8` |
    /// | 5 | size, `u8`, as a power of two of records |
    /// | 6 | clock, `u8`: the control block's when the thread took the ring |
    /// | 8 | thread id, `u32` |
    /// | 12 | state, `u32`: a `RING_` constant |
    /// | 16 | events dropped because a signal handler interrupted a write, `u64` |
    /// | 64 | head, `u64`: the position the next record takes |
    pub const RING_MAGIC: u32 = u32::from_le_bytes(*b"USRG");

    pub const RING_VERSION: u8 = 1;

    pub const RING_HEADER: usize = 128;

    pub const RING_FREE: u32 = 0;

    pub const RING_ACTIVE: u32 = 1;

    /// Its thread exited: once microscope has read it, microscope frees it.
    pub const RING_RETIRED: u32 = 2;

    /// A thread is taking it.
    pub const RING_CLAIMED: u32 = 3;

    pub const DEFAULT_RING_SIZE: u32 = 12;

    pub const LEAST_RING_SIZE: u32 = 6;

    pub const MOST_RING_SIZE: u32 = 20;

    pub const MOST_RINGS: usize = 1024;

    /// A record, 16 bytes: time `u64`, site `u32` (a static site's state
    /// byte's offset from the control block, or a run-time site's key),
    /// kind `u8`, and an argument, `u24`. A plot's
    /// value, and a message's or label's bytes, take the records after it,
    /// each of kind [`CONTINUED`] and fifteen bytes of the rest: bytes 0 to
    /// 12, then 13 to 16.
    pub const RECORD: usize = 16;

    pub const ZONE_BEGIN: u8 = 1;

    pub const ZONE_END: u8 = 2;

    pub const FRAME: u8 = 3;

    /// The next record holds the value, an `f64`.
    pub const PLOT: u8 = 4;

    /// The argument is the text's length; its bytes follow.
    pub const MESSAGE: u8 = 5;

    /// The argument is the value's length; its bytes follow.
    pub const LABEL_BEGIN: u8 = 6;

    pub const LABEL_END: u8 = 7;

    pub const TRIGGER: u8 = 8;

    /// More of the record before it.
    pub const CONTINUED: u8 = 9;

    /// The longest message or label value.
    pub const MOST_TEXT: usize = 256;

    /// How long microscope may go without a heartbeat before every site
    /// switches itself off, unless it says otherwise.
    pub const STALE_NANOSECONDS: u64 = 10_000_000_000;

    /// How old a heartbeat may be for an exiting program to wait for
    /// microscope, and how long it waits at most.
    pub const FRESH_NANOSECONDS: u64 = 1_000_000_000;

    pub const EXIT_WAIT_NANOSECONDS: u64 = 1_000_000_000;
}

#[cfg(feature = "enabled")]
mod probes;

#[cfg(feature = "c")]
mod c;

/// Whether microscope is recording the program's instrumentation: every site
/// it chose is on.
#[cfg(not(feature = "enabled"))]
#[must_use]
pub const fn listening() -> bool {
    false
}

#[cfg(feature = "enabled")]
#[doc(hidden)]
pub use probes::*;

/// A span of the thread's time, from here until the guard drops.
#[cfg(feature = "enabled")]
#[macro_export]
macro_rules! zone {
    ($name:literal) => {
        $crate::Zone::begin($crate::__site!($crate::abi::SITE_ZONE, $name))
    };
}

/// A span of the thread's time, from here until the guard drops.
#[cfg(not(feature = "enabled"))]
#[macro_export]
macro_rules! zone {
    ($name:literal) => {
        $crate::__off()
    };
}

/// What [`zone!`] and [`label!`] give when built off: nothing, bound to
/// its thread as their guards are, so a program builds alike either way.
#[cfg(not(feature = "enabled"))]
#[doc(hidden)]
#[must_use]
pub struct Off(core::marker::PhantomData<*const ()>);

#[cfg(not(feature = "enabled"))]
#[doc(hidden)]
#[inline(always)]
pub const fn __off() -> Off {
    Off(core::marker::PhantomData)
}

/// A zone or label is a span of one thread, so its guard cannot move to
/// another, as across an `.await` that resumes elsewhere: this fails to
/// compile if one could, as two impls would then apply.
const _: fn() = || {
    trait NotSend<Which> {
        fn check() {}
    }

    impl<T: ?Sized> NotSend<()> for T {}
    impl<T: ?Sized + Send> NotSend<u8> for T {}

    #[cfg(feature = "enabled")]
    {
        <Zone as NotSend<_>>::check();
        <Label as NotSend<_>>::check();
    }

    #[cfg(not(feature = "enabled"))]
    <Off as NotSend<_>>::check();
};

/// The end of a frame, for a program that works in frames.
#[cfg(feature = "enabled")]
#[macro_export]
macro_rules! frame {
    () => {
        $crate::frame!("frame")
    };
    ($name:literal) => {
        $crate::__event(
            $crate::__site!($crate::abi::SITE_FRAME, $name),
            $crate::abi::FRAME,
        )
    };
}

/// The end of a frame, for a program that works in frames.
#[cfg(not(feature = "enabled"))]
#[macro_export]
macro_rules! frame {
    ($($name:literal)?) => {
        ()
    };
}

/// A value of the series `name`, as an `f64`.
#[cfg(feature = "enabled")]
#[macro_export]
macro_rules! plot {
    ($name:literal, $value:expr) => {{
        let state = $crate::__site!($crate::abi::SITE_PLOT, $name);
        if $crate::__on(state) {
            $crate::__plot(state, $value);
        }
    }};
}

/// A value of the series `name`, as an `f64`.
#[cfg(not(feature = "enabled"))]
#[macro_export]
macro_rules! plot {
    ($name:literal, $value:expr) => {
        if false {
            let _: f64 = $value;
        }
    };
}

/// A message on the thread's timeline, formatted only while its site is
/// on, and cut to 256 bytes.
#[cfg(feature = "enabled")]
#[macro_export]
macro_rules! message {
    ($format:literal $($arguments:tt)*) => {{
        let state = $crate::__site!($crate::abi::SITE_MESSAGE, $format);
        if $crate::__on(state) {
            $crate::__message(state, ::core::format_args!($format $($arguments)*));
        }
    }};
}

/// A message on the thread's timeline, formatted only while its site is
/// on, and cut to 256 bytes.
#[cfg(not(feature = "enabled"))]
#[macro_export]
macro_rules! message {
    ($format:literal $($arguments:tt)*) => {
        if false {
            let _ = ::core::format_args!($format $($arguments)*);
        }
    };
}

/// Labels the thread's time `key=value` until the guard drops: samples
/// and zones inside it carry the label. The value is formatted only while
/// the site is on, and cut to 256 bytes.
#[cfg(feature = "enabled")]
#[macro_export]
macro_rules! label {
    ($key:literal, $value:expr) => {
        $crate::Label::begin($crate::__site!($crate::abi::SITE_LABEL, $key), &$value)
    };
}

/// Labels the thread's time `key=value` until the guard drops.
#[cfg(not(feature = "enabled"))]
#[macro_export]
macro_rules! label {
    ($key:literal, $value:expr) => {{
        if false {
            let _ = &$value;
        }
        $crate::__off()
    }};
}

/// Asks microscope to save the recording's window around this moment, when it
/// keeps one.
#[cfg(feature = "enabled")]
#[macro_export]
macro_rules! trigger {
    ($why:literal) => {
        $crate::__event(
            $crate::__site!($crate::abi::SITE_TRIGGER, $why),
            $crate::abi::TRIGGER,
        )
    };
}

/// Asks microscope to save the recording's window around this moment.
#[cfg(not(feature = "enabled"))]
#[macro_export]
macro_rules! trigger {
    ($why:literal) => {
        ()
    };
}

/// A site of `kind` named `name`, here: its record goes into the
/// `microscope_sites` section, and the expression is its state byte.
#[cfg(feature = "enabled")]
#[doc(hidden)]
#[macro_export]
macro_rules! __site {
    ($kind:expr, $name:expr) => {{
        // A block holds no assembly, but a module in it does.
        mod site {
            pub static STATE: ::core::sync::atomic::AtomicU8 =
                ::core::sync::atomic::AtomicU8::new(0);
            static NAME: [u8; $name.len()] = $crate::__bytes($name);
            static FILE: [u8; ::core::file!().len()] = $crate::__bytes(::core::file!());
            ::core::arch::global_asm!(
                ".pushsection microscope_sites,\"aR\",@progbits",
                ".balign 8",
                ".byte {version}, {kind}",
                ".short 0",
                ".long {line}",
                ".long {name} - .",
                ".long {name_length}",
                ".long {file} - .",
                ".long {file_length}",
                ".long {state} - .",
                ".long 0",
                ".popsection",
                version = const $crate::abi::SITE_VERSION,
                kind = const $kind,
                line = const ::core::line!(),
                name = sym NAME,
                name_length = const $name.len(),
                file = sym FILE,
                file_length = const ::core::file!().len(),
                state = sym STATE,
            );
        }
        &site::STATE
    }};
}

/// The bytes of `text`, as a site's name or file.
#[doc(hidden)]
#[must_use]
pub const fn __bytes<const N: usize>(text: &str) -> [u8; N] {
    let mut bytes = [0; N];
    let mut index = 0;
    while index < N {
        bytes[index] = text.as_bytes()[index];
        index += 1;
    }

    bytes
}
