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

pub mod abi {
    pub const MAGIC: [u8; 8] = *b"USCOPEPR";

    pub const ABI: u16 = 2;

    pub const ENABLED: u32 = 1;

    pub const ENABLE_DEFINED: u32 = 2;

    pub const RECORDER: &str = "MICROSCOPE_PROBES_RECORDER";

    pub const CLOCK_MONOTONIC: u16 = 1;

    pub const CLOCK_TSC: u16 = 2;

    pub const CONTROL_SIZE: usize = 64;

    pub const SITE_VERSION: u8 = 1;

    pub const SITE_SIZE: usize = 32;

    pub const SITE_SECTION: &str = "microscope_sites";

    const _: () = assert!(
        SITE_VERSION == 1 && SITE_SIZE == 32,
        "a new site layout takes a section of its own: see SITE_SECTION"
    );

    pub const SITE_NONE: u8 = 0;

    pub const SITE_ZONE: u8 = 1;

    pub const SITE_FRAME: u8 = 2;

    pub const SITE_PLOT: u8 = 3;

    pub const SITE_MESSAGE: u8 = 4;

    pub const SITE_LABEL: u8 = 5;

    pub const SITE_TRIGGER: u8 = 6;

    pub const MOST_SITE_TEXT: u32 = 4096;

    pub const SITES_MAGIC: u32 = u32::from_le_bytes(*b"USST");

    pub const SITES_VERSION: u8 = 1;

    pub const SITES_HEADER: usize = 16;

    pub const RUNTIME_SITE_SIZE: usize = 48;

    pub const MOST_RUNTIME_SITES: usize = 4096;

    pub const RUNTIME_KEY: u32 = 1 << 31;

    pub const MOST_STATIC_OFFSET: i64 = 1 << 30;

    pub const RING_MAGIC: u32 = u32::from_le_bytes(*b"USRG");

    pub const RING_VERSION: u8 = 1;

    pub const RING_HEADER: usize = 128;

    pub const RING_FREE: u32 = 0;

    pub const RING_ACTIVE: u32 = 1;

    pub const RING_RETIRED: u32 = 2;

    pub const RING_CLAIMED: u32 = 3;

    pub const DEFAULT_RING_SIZE: u32 = 12;

    pub const LEAST_RING_SIZE: u32 = 6;

    pub const MOST_RING_SIZE: u32 = 20;

    pub const MOST_RINGS: usize = 1024;

    pub const RECORD: usize = 16;

    pub const ZONE_BEGIN: u8 = 1;

    pub const ZONE_END: u8 = 2;

    pub const FRAME: u8 = 3;

    pub const PLOT: u8 = 4;

    pub const MESSAGE: u8 = 5;

    pub const LABEL_BEGIN: u8 = 6;

    pub const LABEL_END: u8 = 7;

    pub const TRIGGER: u8 = 8;

    pub const CONTINUED: u8 = 9;

    pub const MOST_TEXT: usize = 256;

    pub const STALE_NANOSECONDS: u64 = 10_000_000_000;

    pub const FRESH_NANOSECONDS: u64 = 1_000_000_000;

    pub const EXIT_WAIT_NANOSECONDS: u64 = 1_000_000_000;
}

#[cfg(feature = "enabled")]
mod probes;

#[cfg(feature = "c")]
mod c;

#[cfg(not(feature = "enabled"))]
#[must_use]
pub const fn listening() -> bool {
    false
}

#[cfg(not(feature = "enabled"))]
#[must_use]
pub const fn found() -> bool {
    false
}

#[cfg(feature = "enabled")]
#[doc(hidden)]
pub use probes::*;

#[cfg(feature = "enabled")]
#[macro_export]
macro_rules! zone {
    ($name:literal) => {
        $crate::Zone::begin($crate::__site!($crate::abi::SITE_ZONE, $name))
    };
}

#[cfg(not(feature = "enabled"))]
#[macro_export]
macro_rules! zone {
    ($name:literal) => {
        $crate::__off()
    };
}

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

#[cfg(not(feature = "enabled"))]
#[macro_export]
macro_rules! frame {
    ($($name:literal)?) => {
        ()
    };
}

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

#[cfg(not(feature = "enabled"))]
#[macro_export]
macro_rules! plot {
    ($name:literal, $value:expr) => {
        if false {
            let _: f64 = $value;
        }
    };
}

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

#[cfg(not(feature = "enabled"))]
#[macro_export]
macro_rules! message {
    ($format:literal $($arguments:tt)*) => {
        if false {
            let _ = ::core::format_args!($format $($arguments)*);
        }
    };
}

#[cfg(feature = "enabled")]
#[macro_export]
macro_rules! label {
    ($key:literal, $value:expr) => {
        $crate::Label::begin($crate::__site!($crate::abi::SITE_LABEL, $key), &$value)
    };
}

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

#[cfg(not(feature = "enabled"))]
#[macro_export]
macro_rules! trigger {
    ($why:literal) => {
        ()
    };
}

#[cfg(feature = "enabled")]
#[doc(hidden)]
#[macro_export]
macro_rules! __site {
    ($kind:expr, $name:expr) => {{
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
