//! What Inspect shows of a profile, as the page reads it.

use microscope::profiler::{InspectedCommand, Profile};

use crate::web::protocol::{
    ProfileCommand, ProfileFile, ProfileInspection, ProfileModule, ProfileProcess, ProfileTable,
};

pub(super) fn inspection(profile: &Profile, file: Option<ProfileFile>) -> ProfileInspection {
    let seen = profile.inspect();
    let command = |command: InspectedCommand| ProfileCommand {
        arguments: command.arguments,
        text: command.text,
    };
    ProfileInspection {
        command: seen.command.map(command),
        processes: seen
            .processes
            .into_iter()
            .map(|process| ProfileProcess {
                pid: process.pid,
                name: process.name,
                command: process.command.map(command),
            })
            .collect(),
        modules: seen
            .modules
            .into_iter()
            .map(|module| ProfileModule {
                path: module.path,
                size: module.size,
                build_id: module.build_id,
                frames: module.frames,
            })
            .collect(),
        depths: seen.depths,
        over_time: seen.over_time,
        distinct_stacks: seen.distinct_stacks,
        frames: seen.frames,
        frames_with_lines: seen.frames_with_lines,
        counted: seen.counted,
        stack_bytes: seen.stack_bytes,
        shortened: seen.shortened,
        recorder_setup: seen.recorder_setup,
        recorder_finishing: seen.recorder_finishing,
        tables: seen
            .tables
            .into_iter()
            .map(|(name, bytes)| ProfileTable {
                name: name.to_owned(),
                bytes,
            })
            .collect(),
        file,
    }
}
