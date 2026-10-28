//! C entry points to run-time sites, for a language that loads the SDK as a
//! shared library, as microscope's Python package does. Build it so with
//! `cargo rustc --release --features c --crate-type cdylib`.
//!
//! A site is the pointer [`microscope_probes_define`] gives, which lives for
//! the program; a null one does nothing. Texts are bytes and their length.
//! A zone or label that began must end on the thread it began on.

use core::cell::RefCell;
use core::fmt;

use crate::abi;
use crate::probes::{Site, define, end, index, listening, reserve, runtime_site, sites};

/// The ABI the library writes.
#[unsafe(no_mangle)]
pub extern "C" fn microscope_probes_abi() -> u16 {
    abi::ABI
}

/// Whether microscope is recording the program's instrumentation.
#[unsafe(no_mangle)]
pub extern "C" fn microscope_probes_listening() -> bool {
    listening()
}

/// The run-time site of `kind`, as [`define`] gives it, or null.
///
/// # Safety
///
/// `name` and `file` point to `name_length` and `file_length` readable
/// bytes, which need be valid only during the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn microscope_probes_define(
    kind: u8,
    name: *const u8,
    name_length: usize,
    file: *const u8,
    file_length: usize,
    line: u32,
) -> *const Site {
    // SAFETY: the caller passes readable bytes of these lengths.
    let (name, file) = unsafe { (bytes(name, name_length), bytes(file, file_length)) };
    let (Some(Ok(name)), Some(Ok(file))) = (name.map(str::from_utf8), file.map(str::from_utf8))
    else {
        return core::ptr::null();
    };
    define(kind, name, file, line).map_or(core::ptr::null(), core::ptr::from_ref)
}

/// Whether microscope switched `site` on.
#[unsafe(no_mangle)]
pub extern "C" fn microscope_probes_on(site: Option<&Site>) -> bool {
    site.is_some_and(Site::on)
}

/// Begins a zone at `site`: whether it began, and so must end.
#[unsafe(no_mangle)]
pub extern "C" fn microscope_probes_zone_begin(site: Option<&Site>) -> bool {
    site.is_some_and(|site| site.zone().began())
}

/// Ends a zone that began at `site`.
#[unsafe(no_mangle)]
pub extern "C" fn microscope_probes_zone_end(site: Option<&Site>) {
    if let Some(site) = site {
        end(site.key(), abi::ZONE_END);
    }
}

/// Asks that each ring made from now on hold at least `2^size` records, as
/// [`reserve`] does.
#[unsafe(no_mangle)]
pub extern "C" fn microscope_probes_reserve(size: u32) {
    reserve(size);
}

thread_local! {
    /// The keys of the calls the thread began and has not ended, outermost
    /// first.
    static CALLS: RefCell<Vec<u32>> = const { RefCell::new(Vec::new()) };
}

/// Where the run-time sites' records begin, each [`abi::RUNTIME_SITE_SIZE`]
/// bytes and its state byte at offset 1, for a caller that reads many
/// sites' states.
#[unsafe(no_mangle)]
pub extern "C" fn microscope_probes_sites() -> *const Site {
    sites()
}

/// The index of `site` among the run-time sites, by which
/// [`microscope_probes_call_begin`] and [`microscope_probes_call_end`] take
/// it, or -1 for none.
#[unsafe(no_mangle)]
pub extern "C" fn microscope_probes_index(site: Option<&Site>) -> i32 {
    site.and_then(index).map_or(-1, |index| index.cast_signed())
}

/// Begins a zone for a call at the run-time site `index`, which the thread
/// keeps until [`microscope_probes_call_end`] ends it. For a caller that
/// sees each call's start and end, as a tracer of calls does, but not
/// whether it began a zone at the start: the thread keeps that. A site is
/// taken by its index, which a caller passes faster than a pointer.
#[unsafe(no_mangle)]
pub extern "C" fn microscope_probes_call_begin(index: u32) {
    if let Some(site) = runtime_site(index)
        && site.zone().began()
    {
        let _ = CALLS.try_with(|calls| calls.borrow_mut().push(index));
    }
}

/// Ends the zone of the call at the run-time site `index` the thread began
/// last, if it began one there and has not ended it. Calls it began inside
/// that one and did not end end with it, as microscope ends them, flagged.
/// A call that began before its site was on ends nothing.
#[unsafe(no_mangle)]
pub extern "C" fn microscope_probes_call_end(index: u32) {
    let Some(site) = runtime_site(index) else {
        return;
    };

    let began = CALLS.try_with(|calls| {
        let mut calls = calls.borrow_mut();
        let at = calls.iter().rposition(|&call| call == index);
        at.inspect(|&at| calls.truncate(at)).is_some()
    });
    if began == Ok(true) {
        end(site.key(), abi::ZONE_END);
    }
}

/// Labels the thread's time with `site`'s name and the text `value`:
/// whether it began, and so must end.
///
/// # Safety
///
/// `value` points to `length` readable bytes, valid during the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn microscope_probes_label_begin(
    site: Option<&Site>,
    value: *const u8,
    length: usize,
) -> bool {
    // SAFETY: the caller passes readable bytes of this length.
    let value = unsafe { bytes(value, length) };
    site.is_some_and(|site| site.label(&Text(value.unwrap_or_default())).began())
}

/// Ends a label that began at `site`.
#[unsafe(no_mangle)]
pub extern "C" fn microscope_probes_label_end(site: Option<&Site>) {
    if let Some(site) = site {
        end(site.key(), abi::LABEL_END);
    }
}

/// The end of a frame, or a trigger, at `site`.
#[unsafe(no_mangle)]
pub extern "C" fn microscope_probes_event(site: Option<&Site>) {
    if let Some(site) = site {
        site.event();
    }
}

/// A value of `site`'s series.
#[unsafe(no_mangle)]
pub extern "C" fn microscope_probes_plot(site: Option<&Site>, value: f64) {
    if let Some(site) = site {
        site.plot(value);
    }
}

/// A message at `site`, the text `message`, cut to 256 bytes.
///
/// # Safety
///
/// `message` points to `length` readable bytes, valid during the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn microscope_probes_message(
    site: Option<&Site>,
    message: *const u8,
    length: usize,
) {
    // SAFETY: the caller passes readable bytes of this length.
    let message = unsafe { bytes(message, length) };
    if let Some(site) = site {
        site.message(format_args!("{}", Text(message.unwrap_or_default())));
    }
}

/// The `length` bytes at `start`, or none at a null pointer.
///
/// # Safety
///
/// A non-null `start` points to `length` readable bytes that live for `'a`.
unsafe fn bytes<'a>(start: *const u8, length: usize) -> Option<&'a [u8]> {
    if start.is_null() {
        return (length == 0).then_some(&[]);
    }
    // SAFETY: the caller passes readable bytes of this length.
    Some(unsafe { core::slice::from_raw_parts(start, length) })
}

/// Bytes written as text, any that are not UTF-8 replaced.
struct Text<'a>(&'a [u8]);

impl fmt::Display for Text<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        for chunk in self.0.utf8_chunks() {
            formatter.write_str(chunk.valid())?;
            if !chunk.invalid().is_empty() {
                formatter.write_str("\u{fffd}")?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
