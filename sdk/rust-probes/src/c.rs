use core::cell::RefCell;
use core::fmt;

use crate::abi;
use crate::probes::{
    Site, define, end, found, index, interrupted, listening, reserve, runtime_site, sites,
};

#[unsafe(no_mangle)]
pub extern "C" fn microscope_probes_abi() -> u16 {
    abi::ABI
}

#[unsafe(no_mangle)]
pub extern "C" fn microscope_probes_listening() -> bool {
    listening()
}

#[unsafe(no_mangle)]
pub extern "C" fn microscope_probes_found() -> bool {
    found()
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

#[unsafe(no_mangle)]
pub extern "C" fn microscope_probes_on(site: Option<&Site>) -> bool {
    site.is_some_and(Site::on)
}

#[unsafe(no_mangle)]
pub extern "C" fn microscope_probes_zone_begin(site: Option<&Site>) -> bool {
    site.is_some_and(|site| site.zone().began())
}

#[unsafe(no_mangle)]
pub extern "C" fn microscope_probes_zone_end(site: Option<&Site>) {
    if let Some(site) = site {
        end(site.key(), abi::ZONE_END);
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn microscope_probes_reserve(size: u32) {
    reserve(size);
}

thread_local! {
    /// The keys of the calls the thread began and has not ended, outermost
    /// first.
    pub(crate) static CALLS: RefCell<Vec<u32>> = const { RefCell::new(Vec::new()) };
}

#[unsafe(no_mangle)]
pub extern "C" fn microscope_probes_sites() -> *const Site {
    sites()
}

#[unsafe(no_mangle)]
pub extern "C" fn microscope_probes_index(site: Option<&Site>) -> i32 {
    site.and_then(index).map_or(-1, |index| index.cast_signed())
}

#[unsafe(no_mangle)]
pub extern "C" fn microscope_probes_call_begin(index: u32) {
    let Some(site) = runtime_site(index) else {
        return;
    };

    let kept = CALLS.try_with(|calls| {
        let Ok(mut calls) = calls.try_borrow_mut() else {
            return false;
        };
        if site.zone().began() {
            calls.push(index);
        }
        true
    });
    if kept != Ok(true) && site.on() {
        interrupted();
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn microscope_probes_call_end(index: u32) {
    let Some(site) = runtime_site(index) else {
        return;
    };

    let began = CALLS.try_with(|calls| {
        let Ok(mut calls) = calls.try_borrow_mut() else {
            return false;
        };
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

#[unsafe(no_mangle)]
pub extern "C" fn microscope_probes_label_end(site: Option<&Site>) {
    if let Some(site) = site {
        end(site.key(), abi::LABEL_END);
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn microscope_probes_event(site: Option<&Site>) {
    if let Some(site) = site {
        site.event();
    }
}

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
