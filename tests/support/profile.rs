use microscope::profiler::{GapReason, Profile, SampledStack};

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Snapshotting {
    Stopped(std::time::Duration),

    Live(std::time::Duration),
}

impl Snapshotting {
    pub fn options(self) -> microscope::profiler::RecordOptions {
        use microscope::profiler::{RecordOptions, Stopping, Walk};

        let (every, stopping) = match self {
            Self::Stopped(every) => (every, Stopping::Allowed),
            Self::Live(every) => (every, Stopping::Never),
        };
        RecordOptions {
            snapshots: Some(every),
            stopping,
            walk: Walk::Never,
            ..RecordOptions::default()
        }
    }
}
