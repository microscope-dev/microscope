//! Known `rustc` internal symbol and type spellings that vary across Rust
//! toolchain versions.

pub const DROP_GLUE: [&str; 2] = ["drop_glue", "drop_in_place"];

pub const RUST_PANIC: [&str; 2] = ["__rustc::rust_panic", "rust_panic"];

pub const THREAD_LOCAL_STORAGE: [&str; 2] = ["__RUST_STD_INTERNAL_VAL", "VAL"];

pub const ARC: [&str; 2] = ["alloc::sync::Arc", "alloc::rcs::arc::Arc"];

pub fn drop_glue_of(ty: &str) -> [String; 2] {
    DROP_GLUE.map(|glue| format!("{glue}<{ty}>"))
}

pub fn is_drop_glue(name: &str) -> bool {
    DROP_GLUE.iter().any(|glue| {
        name.strip_prefix(glue)
            .is_some_and(|rest| rest.starts_with('<'))
    })
}

pub fn with_each_arc(name: &str) -> Vec<String> {
    let [written, ..] = ARC;
    if !name.contains(written) {
        return vec![name.to_owned()];
    }

    ARC.iter().map(|path| name.replace(written, path)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drop_glue_is_known_by_either_name() {
        assert!(is_drop_glue("drop_glue<cancel::waiting::{async_fn_env#0}>"));
        assert!(is_drop_glue(
            "drop_in_place<cancel::waiting::{async_fn_env#0}>"
        ));
        assert!(!is_drop_glue("drop_glue_slow<u8>"));
        assert!(!is_drop_glue("drop_in_place"));
        assert!(!is_drop_glue("drop<u8>"));
        assert_eq!(
            drop_glue_of("a::b"),
            [
                "drop_glue<a::b>".to_owned(),
                "drop_in_place<a::b>".to_owned()
            ]
        );
    }
}
