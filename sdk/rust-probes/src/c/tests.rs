//! The C entry points as a caller across a language boundary uses them,
//! which Miri runs too: `just miri`.

use core::sync::atomic::Ordering;

use super::*;

fn define_site(kind: u8, name: &str) -> *const Site {
    let file = "app.py";
    // SAFETY: both texts are readable for their lengths.
    unsafe {
        microscope_probes_define(
            kind,
            name.as_ptr(),
            name.len(),
            file.as_ptr(),
            file.len(),
            1,
        )
    }
}

/// A site is defined from bytes, refused as null when they are not text;
/// a null site does nothing; a zone begun ends by its own call.
#[test]
fn sites_are_defined_from_bytes_and_null_does_nothing() {
    assert_eq!(microscope_probes_abi(), abi::ABI);
    let zone = define_site(abi::SITE_ZONE, "c.zone");
    assert!(!zone.is_null());
    assert_eq!(zone, define_site(abi::SITE_ZONE, "c.zone"));
    // SAFETY: the bytes are readable; they are not UTF-8.
    let invalid = unsafe {
        microscope_probes_define(abi::SITE_ZONE, [0xff].as_ptr(), 1, core::ptr::null(), 0, 1)
    };
    assert!(invalid.is_null());
    // SAFETY: a null name of a nonzero length is refused before it is read.
    let unnamed = unsafe {
        microscope_probes_define(
            abi::SITE_ZONE,
            core::ptr::null(),
            3,
            core::ptr::null(),
            0,
            1,
        )
    };
    assert!(unnamed.is_null());

    // SAFETY: a non-null site from define lives for the program.
    let site = unsafe { zone.as_ref() };
    assert!(!microscope_probes_on(site));
    assert!(
        !microscope_probes_zone_begin(site),
        "an off site begins nothing"
    );
    assert!(!microscope_probes_zone_begin(None));
    microscope_probes_zone_end(None);
    microscope_probes_event(None);
    microscope_probes_plot(None, 1.0);
    // SAFETY: the text is readable for its length.
    unsafe {
        microscope_probes_message(None, b"x".as_ptr(), 1);
        assert!(!microscope_probes_label_begin(None, b"x".as_ptr(), 1));
    }
    microscope_probes_label_end(None);
    let _ = site.map(|site| site.state.load(Ordering::Relaxed));
}

/// Text that is not UTF-8 is written with its bad bytes replaced.
#[test]
fn text_replaces_what_is_not_utf8() {
    assert_eq!(Text(b"a\xffb").to_string(), "a\u{fffd}b");
    assert_eq!(Text("naïve".as_bytes()).to_string(), "naïve");
}
