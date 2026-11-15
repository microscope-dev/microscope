#![no_std]

#[macro_export]
macro_rules! microscope_views_file {
    ($path:expr) => {
        ::core::arch::global_asm!(
            ".pushsection .debug_microscope_views,\"\",@progbits",
            ".byte 1, 1",
            ".long 8f - 7f",
            "7:",
            concat!(".incbin \"", $path, "\""),
            "8:",
            ".popsection",
        );
    };
}

#[macro_export]
macro_rules! microscope_kernel {
    ($name:expr, $source:expr, $module:expr) => {
        ::core::arch::global_asm!(
            ".pushsection .debug_microscope_views,\"\",@progbits",
            ".byte 2, 1",
            ".long 8f - 7f",
            "7:",
            ".short 4f - 3f",
            "3:",
            concat!(".ascii \"", $name, "\""),
            "4:",
            ".long 6f - 5f",
            "5:",
            concat!(".incbin \"", $source, "\""),
            "6:",
            concat!(".incbin \"", $module, "\""),
            "8:",
            ".popsection",
        );
    };
}

#[macro_export]
macro_rules! microscope_visualizer {
    ($name:expr, $path:expr) => {
        ::core::arch::global_asm!(
            ".pushsection .debug_microscope_views,\"\",@progbits",
            ".byte 3, 1",
            ".long 8f - 7f",
            "7:",
            ".short 4f - 3f",
            "3:",
            concat!(".ascii \"", $name, "\""),
            "4:",
            concat!(".incbin \"", $path, "\""),
            "8:",
            ".popsection",
        );
    };
}

/// Writing a kernel: a WebAssembly function a view calls to walk a
/// structure that is an algorithm more than a layout, as microscope's
/// `docs/views.md` describes. It exports `run`, which takes the view's
/// arguments as 64-bit words, and yields items of words, such as
/// addresses, which the view presents:
///
/// ```ignore
/// #[unsafe(no_mangle)]
/// pub unsafe extern "C" fn run(pointer: *const u64, count: i32) -> i32 {
///     // SAFETY: microscope passes `count` words.
///     let arguments =
///         unsafe { microscope_views::kernel::arguments(pointer, count) };
///     let &[head] = arguments else {
///         return 1;
///     };
///     let mut node = head;
///     while node != 0 && microscope_views::kernel::emit(&[node]) {
///         node = microscope_views::kernel::load(node);
///     }
///     0
/// }
/// ```
///
/// Build it as a `cdylib` for `wasm32-unknown-unknown`, `no_std`, with
/// `panic = "abort"`. `run` returns 0 when it is done, and anything else
/// says it failed.
#[cfg(target_family = "wasm")]
pub mod kernel {
    #[link(wasm_import_module = "microscope_kernel_v1")]
    unsafe extern "C" {
        #[link_name = "read"]
        fn host_read(address: u64, buffer: *mut u8, length: i32) -> i32;

        #[link_name = "yield"]
        fn host_yield(words: *const u64, count: i32) -> i32;
    }

    /// The view's arguments.
    ///
    /// # Safety
    ///
    /// `arguments` and `count` are those `run` was called with.
    #[must_use]
    pub unsafe fn arguments<'a>(arguments: *const u64, count: i32) -> &'a [u64] {
        let count = usize::try_from(count).unwrap_or(0);

        // SAFETY: microscope passes `count` words at `arguments`.
        unsafe { core::slice::from_raw_parts(arguments, count) }
    }

    pub fn read(address: u64, buffer: &mut [u8]) {
        let length = i32::try_from(buffer.len()).unwrap_or(i32::MAX);

        // SAFETY: the host writes at most `length` bytes into `buffer`.
        unsafe { host_read(address, buffer.as_mut_ptr(), length) };
    }

    pub trait Integer: Sized {
        fn from_le(bytes: &[u8]) -> Self;
    }

    macro_rules! integers {
        ($($ty:ty),*) => {$(
            impl Integer for $ty {
                fn from_le(bytes: &[u8]) -> Self {
                    let mut le = [0; core::mem::size_of::<$ty>()];
                    le.copy_from_slice(bytes);
                    <$ty>::from_le_bytes(le)
                }
            }
        )*};
    }

    integers!(u8, u16, u32, u64, i8, i16, i32, i64);

    #[must_use]
    pub fn load<T: Integer>(address: u64) -> T {
        let mut bytes = [0; 8];
        let bytes = &mut bytes[..core::mem::size_of::<T>()];
        read(address, bytes);
        T::from_le(bytes)
    }

    #[must_use]
    pub fn emit(words: &[u64]) -> bool {
        let count = i32::try_from(words.len()).unwrap_or(i32::MAX);

        // SAFETY: the host reads `count` words at `words`.
        unsafe { host_yield(words.as_ptr(), count) != 0 }
    }
}
