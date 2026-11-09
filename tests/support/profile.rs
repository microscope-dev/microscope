//! What a test of a real recording may assert of its samples. The kernel
//! promises to count every call, not to keep every sample: a ring that other
//! recordings shrank, or a reader starved of CPU, loses some, and the
//! profile says so. So a test checks the accounting here, then that each
//! sample it kept is right (dev/testing.md).

use microscope::profiler::{GapReason, Profile, SampledStack};

/// Checks that the recording counted `calls`, that each is a sample it kept
/// or one the kernel dropped, and that it dropped any only with its rings
/// shrunk or sampling paused while it caught up. The samples it kept, for
/// the caller to check each of.
pub fn accounted(profile: &Profile, calls: u64) -> Vec<SampledStack> {
    let stacks = profile.stacks();
    let context = context(profile);
    assert_eq!(profile.counted(), calls, "every call is counted: {context}");
    assert_eq!(
        stacks.len() as u64 + profile.lost_samples(),
        calls,
        "every call is a sample or counted lost: {context}"
    );
    assert_loss_explained(profile);

    stacks
}

/// Checks that the kernel dropped samples or other records only where the
/// profile says why: its rings shrank, or sampling paused while microscope
/// caught up.
pub fn assert_loss_explained(profile: &Profile) {
    let lost = profile.lost_samples() + profile.lost_records();
    assert!(
        lost == 0
            || profile.setting("ring warning").is_some()
            || !profile.paused(GapReason::Behind).is_empty(),
        "records were lost with full rings and the reader keeping up: {}",
        context(profile)
    );
}

/// What a failure says of a recording's accounting.
fn context(profile: &Profile) -> String {
    format!(
        "counted {}, kept {}, lost {} samples and {} other records; \
         rings of {} pages, {} asked{}; {} spans behind",
        profile.counted(),
        profile.samples(),
        profile.lost_samples(),
        profile.lost_records(),
        profile.setting("ring pages").unwrap_or("?"),
        profile.setting("requested ring pages").unwrap_or("?"),
        profile
            .setting("ring warning")
            .map_or_else(String::new, |warning| format!(" ({warning})")),
        profile.paused(GapReason::Behind).len(),
    )
}
