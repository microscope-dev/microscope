use std::collections::BTreeSet;

use microscope_data::{
    capture::{
        Capture, Frame, Function, Line, Mapping, Marker, Module, Plot, Process, Range, Sample,
        Setting, Site, Stack, Syscall, SyscallNote, SyscallStat, Task, TaskWait, Thread,
        ThreadRange, Zone, ZoneRange,
        records::{FrameKind, NONE},
    },
    image::StringsBuilder,
};

struct Kept(Vec<bool>);

impl Kept {
    fn new(len: usize) -> Self {
        Self(vec![false; len])
    }

    fn keep(&mut self, row: u32) {
        if let Some(kept) = self.0.get_mut(row as usize) {
            *kept = true;
        }
    }

    fn has(&self, row: u32) -> bool {
        self.0.get(row as usize).copied().unwrap_or(false)
    }

    fn rows(&self) -> impl Iterator<Item = u32> + '_ {
        (0..)
            .zip(&self.0)
            .filter(|(_, kept)| **kept)
            .map(|(row, _)| row)
    }

    fn map(&self) -> Vec<u32> {
        let mut next = 0_u32;
        self.0
            .iter()
            .map(|&kept| {
                if kept {
                    let out = next;
                    next = next.saturating_add(1);
                    out
                } else {
                    NONE
                }
            })
            .collect()
    }
}

fn kept<T: Copy>(rows: &[T], kept: &Kept) -> impl Iterator<Item = T> {
    rows.iter()
        .zip(&kept.0)
        .filter(|(_, kept)| **kept)
        .map(|(row, _)| *row)
}

fn map_id(map: &[u32], row: u32) -> u32 {
    if row == NONE {
        NONE
    } else {
        map.get(row as usize).copied().unwrap_or(NONE)
    }
}

fn copy_str(from: &Capture, strings: &mut StringsBuilder, id: u32) -> u32 {
    if id == NONE {
        return NONE;
    }
    strings.push(from.string(id)).map_or(NONE, |s| s.0)
}

enum SettingKeyAction {
    Keep,
    Drop,
    Rename(String),
}

fn remap_process_setting_key(key: &str, proc_map: &[u32]) -> SettingKeyAction {
    if matches!(key, "program" | "pid" | "arguments" | "argv" | "command") {
        return SettingKeyAction::Drop;
    }
    for prefix in ["arguments ", "argv ", "command "] {
        if let Some(rest) = key.strip_prefix(prefix) {
            if let Ok(old_idx) = rest.parse::<u32>() {
                let mapped = map_id(proc_map, old_idx);
                if mapped != NONE {
                    return SettingKeyAction::Rename(format!("{prefix}{mapped}"));
                }
            }
            return SettingKeyAction::Drop;
        }
    }
    SettingKeyAction::Keep
}

#[expect(
    clippy::too_many_lines,
    reason = "filters every Capture table by allowed PIDs and compacts all indices and strings with zero cross-process leakage"
)]
#[must_use]
pub fn filter_capture_by_pids(from: &Capture, allowed_pids: &BTreeSet<u32>) -> Capture {
    if allowed_pids.is_empty() {
        return Capture::default();
    }

    let mut kept_procs = Kept::new(from.processes.len());
    for (idx, proc_row) in from.processes.iter().enumerate() {
        if allowed_pids.contains(&proc_row.pid.get()) {
            kept_procs.keep(u32::try_from(idx).unwrap_or(NONE));
        }
    }

    let mut kept_threads = Kept::new(from.threads.len());
    for (idx, thread) in from.threads.iter().enumerate() {
        if kept_procs.has(thread.process.get()) {
            kept_threads.keep(u32::try_from(idx).unwrap_or(NONE));
        }
    }

    let mut kept_stacks = Kept::new(from.stacks.len());
    let mut kept_tasks = Kept::new(from.tasks.len());
    let mut kept_sites = Kept::new(from.sites.len());
    let mut kept_frames = Kept::new(from.frames.len());
    let mut kept_functions = Kept::new(from.functions.len());
    let mut kept_modules = Kept::new(from.modules.len());

    for range in &from.sample_ranges {
        if range.count.get() > 0 && kept_threads.has(range.thread.get()) {
            for sample in from.thread_samples(range) {
                kept_stacks.keep(sample.stack.get());
                kept_tasks.keep(sample.task.get());
            }
        }
    }
    for range in &from.zone_ranges {
        if range.count.get() > 0 && kept_threads.has(range.thread.get()) {
            for zone in from.range_zones(range) {
                kept_sites.keep(zone.site.get());
            }
        }
    }
    for plot in &from.plots {
        if kept_threads.has(plot.thread.get()) {
            kept_sites.keep(plot.site.get());
        }
    }
    for marker in &from.markers {
        let tid = marker.thread.get();
        if tid != NONE && kept_threads.has(tid) {
            kept_sites.keep(marker.site.get());
        }
    }
    for wait in &from.task_waits {
        if kept_tasks.has(wait.task.get()) {
            kept_stacks.keep(wait.stack.get());
        }
    }
    for call in &from.syscalls {
        if kept_threads.has(call.thread.get()) {
            kept_stacks.keep(call.stack.get());
        }
    }
    for note in &from.syscall_notes {
        let keep_proc = note.process.get() == NONE || kept_procs.has(note.process.get());
        let keep_thread = note.thread.get() == NONE || kept_threads.has(note.thread.get());
        if keep_proc && keep_thread && (note.process.get() != NONE || note.thread.get() != NONE) {
            kept_stacks.keep(note.stack.get());
        }
    }

    for (row, stack) in from.stacks.iter().enumerate().rev() {
        let row = u32::try_from(row).unwrap_or(NONE);
        if kept_stacks.has(row) {
            kept_stacks.keep(stack.parent.get());
            kept_frames.keep(stack.frame.get());
        }
    }
    for row in kept_tasks.rows() {
        if let Some(task) = from.tasks.get(row as usize) {
            kept_frames.keep(task.root.get());
        }
    }
    for row in kept_frames.rows() {
        for line in from.frame_lines(row) {
            kept_functions.keep(line.function.get());
        }
    }

    let modules: Vec<u32> = from
        .mappings
        .iter()
        .filter(|m| kept_procs.has(m.process.get()))
        .map(|m| m.module.get())
        .chain(kept(&from.frames, &kept_frames).map(|f| f.module.get()))
        .chain(kept(&from.functions, &kept_functions).map(|f| f.module.get()))
        .chain(kept(&from.sites, &kept_sites).map(|s| s.module.get()))
        .collect();
    for module in modules {
        kept_modules.keep(module);
    }

    let proc_map = kept_procs.map();
    let thread_map = kept_threads.map();
    let stack_map = kept_stacks.map();
    let task_map = kept_tasks.map();
    let site_map = kept_sites.map();
    let frame_map = kept_frames.map();
    let fn_map = kept_functions.map();
    let mod_map = kept_modules.map();

    let mut strings = StringsBuilder::default();
    let mut to = Capture::default();

    for setting in &from.settings {
        let key_str = from.string(setting.key.get());
        let key = match remap_process_setting_key(key_str, &proc_map) {
            SettingKeyAction::Drop => continue,
            SettingKeyAction::Rename(remapped) => {
                strings.push(&remapped).map_or(NONE, |s| s.0).into()
            }
            SettingKeyAction::Keep => copy_str(from, &mut strings, setting.key.get()).into(),
        };
        let value = copy_str(from, &mut strings, setting.value.get()).into();
        to.settings.push(Setting { key, value });
    }

    for module in kept(&from.modules, &kept_modules) {
        let path = copy_str(from, &mut strings, module.path.get()).into();
        let build_id = copy_str(from, &mut strings, module.build_id.get()).into();
        to.modules.push(Module {
            path,
            build_id,
            ..module
        });
    }
    for process in kept(&from.processes, &kept_procs) {
        let name = copy_str(from, &mut strings, process.name.get()).into();
        to.processes.push(Process { name, ..process });
    }
    for thread in kept(&from.threads, &kept_threads) {
        let name = copy_str(from, &mut strings, thread.name.get()).into();
        to.threads.push(Thread {
            process: map_id(&proc_map, thread.process.get()).into(),
            name,
            ..thread
        });
    }
    for mapping in &from.mappings {
        if kept_procs.has(mapping.process.get()) {
            to.mappings.push(Mapping {
                process: map_id(&proc_map, mapping.process.get()).into(),
                module: map_id(&mod_map, mapping.module.get()).into(),
                ..*mapping
            });
        }
    }

    for frame in kept(&from.frames, &kept_frames) {
        let named = FrameKind::from_number(frame.kind).is_some_and(FrameKind::named);
        let address = match u32::try_from(frame.address.get()) {
            Ok(name) if named => u64::from(copy_str(from, &mut strings, name)),
            _ => frame.address.get(),
        };
        to.frames.push(Frame {
            address: address.into(),
            module: map_id(&mod_map, frame.module.get()).into(),
            ..frame
        });
    }
    for stack in kept(&from.stacks, &kept_stacks) {
        to.stacks.push(Stack {
            parent: map_id(&stack_map, stack.parent.get()).into(),
            frame: map_id(&frame_map, stack.frame.get()).into(),
        });
    }
    if !from.frame_symbols.is_empty() {
        for frame in kept_frames.rows() {
            let lines = from.frame_lines(frame);
            to.frame_symbols.push(Range {
                start: u32::try_from(to.lines.len()).unwrap_or(NONE).into(),
                count: u32::try_from(lines.len()).unwrap_or(0).into(),
            });
            for line in lines {
                let file = copy_str(from, &mut strings, line.file.get()).into();
                to.lines.push(Line {
                    function: map_id(&fn_map, line.function.get()).into(),
                    file,
                    ..*line
                });
            }
        }
    }
    for function in kept(&from.functions, &kept_functions) {
        let name = copy_str(from, &mut strings, function.name.get()).into();
        let file = copy_str(from, &mut strings, function.file.get()).into();
        to.functions.push(Function {
            name,
            module: map_id(&mod_map, function.module.get()).into(),
            file,
            ..function
        });
    }
    for site in kept(&from.sites, &kept_sites) {
        let name = copy_str(from, &mut strings, site.name.get()).into();
        let file = copy_str(from, &mut strings, site.file.get()).into();
        to.sites.push(Site {
            name,
            file,
            module: map_id(&mod_map, site.module.get()).into(),
            ..site
        });
    }
    for task in kept(&from.tasks, &kept_tasks) {
        let runtime = copy_str(from, &mut strings, task.runtime.get()).into();
        let labels = copy_str(from, &mut strings, task.labels.get()).into();
        to.tasks.push(Task {
            runtime,
            root: map_id(&frame_map, task.root.get()).into(),
            labels,
            ..task
        });
    }

    for range in &from.sample_ranges {
        if range.count.get() == 0 || !kept_threads.has(range.thread.get()) {
            continue;
        }
        let start = u32::try_from(to.samples.len()).unwrap_or(NONE);
        let rows = from.thread_samples(range);
        for sample in rows {
            to.samples.push(Sample {
                stack: map_id(&stack_map, sample.stack.get()).into(),
                task: map_id(&task_map, sample.task.get()).into(),
                ..*sample
            });
        }
        let count = u32::try_from(rows.len()).unwrap_or(0);
        if count > 0 {
            to.sample_ranges.push(ThreadRange {
                thread: map_id(&thread_map, range.thread.get()).into(),
                start: start.into(),
                count: count.into(),
            });
        }
    }

    for range in &from.off_cpu_ranges {
        if range.count.get() == 0 || !kept_threads.has(range.thread.get()) {
            continue;
        }
        let start = u32::try_from(to.off_cpu.len()).unwrap_or(NONE);
        let rows = from.thread_off_cpu(range);
        to.off_cpu.extend_from_slice(rows);
        let count = u32::try_from(rows.len()).unwrap_or(0);
        if count > 0 {
            to.off_cpu_ranges.push(ThreadRange {
                thread: map_id(&thread_map, range.thread.get()).into(),
                start: start.into(),
                count: count.into(),
            });
        }
    }

    for range in &from.zone_ranges {
        if range.count.get() == 0 || !kept_threads.has(range.thread.get()) {
            continue;
        }
        let start = u32::try_from(to.zones.len()).unwrap_or(NONE);
        let rows = from.range_zones(range);
        for zone in rows {
            let value = copy_str(from, &mut strings, zone.value.get()).into();
            to.zones.push(Zone {
                site: map_id(&site_map, zone.site.get()).into(),
                value,
                ..*zone
            });
        }
        let count = u32::try_from(rows.len()).unwrap_or(0);
        if count > 0 {
            to.zone_ranges.push(ZoneRange {
                thread: map_id(&thread_map, range.thread.get()).into(),
                depth: range.depth,
                start: start.into(),
                count: count.into(),
            });
        }
    }

    for plot in &from.plots {
        if kept_threads.has(plot.thread.get()) {
            to.plots.push(Plot {
                site: map_id(&site_map, plot.site.get()).into(),
                thread: map_id(&thread_map, plot.thread.get()).into(),
                ..*plot
            });
        }
    }

    for marker in &from.markers {
        let tid = marker.thread.get();
        if tid != NONE && kept_threads.has(tid) {
            let text = copy_str(from, &mut strings, marker.text.get()).into();
            to.markers.push(Marker {
                thread: map_id(&thread_map, tid).into(),
                site: map_id(&site_map, marker.site.get()).into(),
                text,
                ..*marker
            });
        }
    }

    for wait in &from.task_waits {
        if kept_tasks.has(wait.task.get()) {
            let state = copy_str(from, &mut strings, wait.state.get()).into();
            to.task_waits.push(TaskWait {
                task: map_id(&task_map, wait.task.get()).into(),
                stack: map_id(&stack_map, wait.stack.get()).into(),
                state,
                ..*wait
            });
        }
    }

    to.accounting.clone_from(&from.accounting);
    to.gaps.clone_from(&from.gaps);

    let mut kept_calls_before = Vec::with_capacity(from.syscalls.len() + 1);
    kept_calls_before.push(0_u32);
    for call in &from.syscalls {
        if kept_threads.has(call.thread.get()) {
            let blob = from.blob(call.captures.get(), call.captures_len.get());
            let (captures, captures_len) = if blob.is_empty() {
                (NONE, 0)
            } else {
                let start = u32::try_from(to.syscall_blobs.len()).unwrap_or(NONE);
                let len = u32::try_from(blob.len()).unwrap_or(0);
                to.syscall_blobs.extend_from_slice(blob);
                (start, len)
            };
            to.syscalls.push(Syscall {
                thread: map_id(&thread_map, call.thread.get()).into(),
                stack: map_id(&stack_map, call.stack.get()).into(),
                captures: captures.into(),
                captures_len: captures_len.into(),
                ..*call
            });
        }
        kept_calls_before.push(u32::try_from(to.syscalls.len()).unwrap_or(u32::MAX));
    }

    for note in &from.syscall_notes {
        let keep_proc = note.process.get() == NONE || kept_procs.has(note.process.get());
        let keep_thread = note.thread.get() == NONE || kept_threads.has(note.thread.get());
        if !(keep_proc && keep_thread && (note.process.get() != NONE || note.thread.get() != NONE))
        {
            continue;
        }
        let text = copy_str(from, &mut strings, note.text.get()).into();
        let blob = from.blob(note.blob.get(), note.blob_len.get());
        let (blob_off, blob_len) = if blob.is_empty() {
            (NONE, 0)
        } else {
            let start = u32::try_from(to.syscall_blobs.len()).unwrap_or(NONE);
            let len = u32::try_from(blob.len()).unwrap_or(0);
            to.syscall_blobs.extend_from_slice(blob);
            (start, len)
        };
        let mapped_calls = kept_calls_before
            .get(note.calls.get() as usize)
            .copied()
            .unwrap_or_else(|| u32::try_from(to.syscalls.len()).unwrap_or(u32::MAX));
        to.syscall_notes.push(SyscallNote {
            thread: map_id(&thread_map, note.thread.get()).into(),
            process: map_id(&proc_map, note.process.get()).into(),
            stack: map_id(&stack_map, note.stack.get()).into(),
            text,
            blob: blob_off.into(),
            blob_len: blob_len.into(),
            calls: mapped_calls.into(),
            ..*note
        });
    }

    for stat in &from.syscall_stats {
        let proc_ok = stat.process.get() != NONE && kept_procs.has(stat.process.get());
        let thread_ok = stat.thread.get() == NONE || kept_threads.has(stat.thread.get());
        if !(proc_ok && thread_ok) {
            continue;
        }
        let errnos_slice = from.stat_errnos(stat);
        let (errnos, errno_count) = if errnos_slice.is_empty() {
            (0, 0)
        } else {
            let start = u32::try_from(to.syscall_errnos.len()).unwrap_or(NONE);
            let count = u32::try_from(errnos_slice.len()).unwrap_or(0);
            for err in errnos_slice {
                to.syscall_errnos.push(*err);
            }
            (start, count)
        };
        to.syscall_stats.push(SyscallStat {
            process: map_id(&proc_map, stat.process.get()).into(),
            thread: map_id(&thread_map, stat.thread.get()).into(),
            errnos: errnos.into(),
            errno_count: errno_count.into(),
            ..*stat
        });
    }

    to.strings = strings.into_bytes();
    to
}

#[cfg(test)]
mod tests {
    use microscope_data::{
        capture::builder::{CaptureBuilder, LineInput},
        model::{CodeRole, Pid, Tid},
    };

    use super::*;

    #[test]
    fn splitting_multi_process_capture_leaks_zero_strings_or_frames() {
        let mut b = CaptureBuilder::new();
        b.setting("weight", "nanoseconds");
        b.setting("program", "/usr/bin/prog-c");
        let _p_a = b.process(Pid::new(10), 100);
        let t_a = b.thread(Pid::new(10), Tid::new(10), 100);
        let m_a = b.module("/usr/bin/prog-a", Some("aaa"), [1; 16], 4096);
        let f_a = b.frame(FrameKind::Native, m_a, 0x1000, CodeRole::Ordinary as u8);
        b.symbolize(
            f_a,
            m_a,
            &[LineInput {
                function: "alpha_work",
                file: Some("alpha.rs"),
                line: 10,
                column: 0,
                declared: None,
                role: CodeRole::Ordinary as u8,
            }],
        );
        let s_a = b.stack(&[f_a]);
        b.sample(t_a, 150, 10_000, s_a, NONE);

        let _p_b = b.process(Pid::new(20), 100);
        let t_b = b.thread(Pid::new(20), Tid::new(20), 100);
        let m_b = b.module("/usr/bin/prog-b", Some("bbb"), [2; 16], 4096);
        let f_b = b.frame(FrameKind::Native, m_b, 0x2000, CodeRole::Ordinary as u8);
        b.symbolize(
            f_b,
            m_b,
            &[LineInput {
                function: "beta_work",
                file: Some("beta.rs"),
                line: 20,
                column: 0,
                declared: None,
                role: CodeRole::Ordinary as u8,
            }],
        );
        let s_b = b.stack(&[f_b]);
        b.sample(t_b, 160, 20_000, s_b, NONE);

        let _p_c = b.process(Pid::new(30), 100);
        let t_c = b.thread(Pid::new(30), Tid::new(30), 100);
        let m_c = b.module("/usr/bin/prog-c-secret", Some("ccc"), [3; 16], 4096);
        let f_c = b.frame(FrameKind::Native, m_c, 0x3000, CodeRole::Ordinary as u8);
        b.symbolize(
            f_c,
            m_c,
            &[LineInput {
                function: "gamma_secret_fn",
                file: Some("gamma.rs"),
                line: 30,
                column: 0,
                declared: None,
                role: CodeRole::Ordinary as u8,
            }],
        );
        let s_c = b.stack(&[f_c]);
        b.sample(t_c, 170, 30_000, s_c, NONE);

        let full = b.finish();
        let only_a = filter_capture_by_pids(&full, &BTreeSet::from([10]));
        assert!(only_a.validate().is_ok(), "{:?}", only_a.validate());
        assert_eq!(only_a.processes.len(), 1);
        assert_eq!(only_a.processes[0].pid.get(), 10);
        assert_eq!(only_a.samples.len(), 1);
        let strings_a = String::from_utf8_lossy(&only_a.strings);
        assert!(strings_a.contains("alpha_work"));
        assert!(!strings_a.contains("beta_work"));
        assert!(!strings_a.contains("prog-b"));
        assert!(!strings_a.contains("prog-c"));
        assert!(!strings_a.contains("gamma_secret_fn"));

        let only_b = filter_capture_by_pids(&full, &BTreeSet::from([20]));
        assert!(only_b.validate().is_ok(), "{:?}", only_b.validate());
        assert_eq!(only_b.processes.len(), 1);
        assert_eq!(only_b.processes[0].pid.get(), 20);
        let strings_b = String::from_utf8_lossy(&only_b.strings);
        assert!(strings_b.contains("beta_work"));
        assert!(!strings_b.contains("alpha_work"));
        assert!(!strings_b.contains("prog-a"));
        assert!(!strings_b.contains("prog-c"));
    }

    #[test]
    fn split_remaps_per_process_settings_and_drops_unowned_markers() {
        let mut b = CaptureBuilder::new();
        b.setting("weight", "nanoseconds");
        let p0 = b.process(Pid::new(100), 1_000);
        let p1 = b.process(Pid::new(200), 1_000);
        b.setting(&format!("command {p0}"), "/bin/alpha --a");
        b.setting(&format!("arguments {p0}"), r#"["/bin/alpha","--a"]"#);
        b.setting(&format!("command {p1}"), "/bin/beta --b");
        b.setting(&format!("arguments {p1}"), r#"["/bin/beta","--b"]"#);
        let t0 = b.thread(Pid::new(100), Tid::new(100), 1_000);
        let t1 = b.thread(Pid::new(200), Tid::new(200), 1_000);
        let f = b.frame(FrameKind::Native, NONE, 0x1000, CodeRole::Ordinary as u8);
        let s = b.stack(&[f]);
        b.sample(t0, 1_100, 10, s, NONE);
        b.sample(t1, 1_200, 20, s, NONE);
        b.marker(
            1_150,
            microscope_data::capture::records::MarkerKind::Message,
            "global-secret",
        );
        let site = b.site("ev", "a.rs", 1, NONE, 0);
        b.thread_marker(
            (t1, site),
            1_250,
            microscope_data::capture::records::MarkerKind::Message,
            Some("beta-marker"),
        );

        let full = b.finish();
        let only_b = filter_capture_by_pids(&full, &BTreeSet::from([200]));
        assert!(only_b.validate().is_ok(), "{:?}", only_b.validate());
        assert_eq!(only_b.processes.len(), 1);
        assert_eq!(only_b.setting("command 0"), Some("/bin/beta --b"));
        assert_eq!(
            only_b.setting("arguments 0"),
            Some(r#"["/bin/beta","--b"]"#)
        );
        assert!(only_b.setting("command 1").is_none());
        let strings = String::from_utf8_lossy(&only_b.strings);
        assert!(strings.contains("beta-marker"));
        assert!(!strings.contains("global-secret"));
        assert!(!strings.contains("/bin/alpha"));
    }

    proptest::proptest! {
        #[test]
        fn split_partitions_samples_and_always_validates(
            proc_count in 1_usize..6,
            keep_mask in 1_u8..64,
        ) {
            let mut b = CaptureBuilder::new();
            b.setting("weight", "nanoseconds");
            let mut expected_kept_samples = 0_usize;
            let mut keep_pids = BTreeSet::new();
            for i in 0..proc_count {
                let pid = (u32::try_from(i).unwrap() + 1) * 10;
                let p = b.process(Pid::new(pid), 1000);
                b.setting(&format!("command {p}"), &format!("/bin/p{pid}"));
                let t = b.thread(Pid::new(pid), Tid::new(pid), 1000);
                let m = b.module(
                    &format!("/bin/p{pid}"),
                    None,
                    [u8::try_from(i).unwrap(); 16],
                    4096,
                );
                let f = b.frame(FrameKind::Native, m, 0x1000 + i as u64, CodeRole::Ordinary as u8);
                let s = b.stack(&[f]);
                for k in 0..=(i % 3) {
                    b.sample(
                        t,
                        2000 + i64::try_from(k).unwrap() * 100,
                        50,
                        s,
                        NONE,
                    );
                    if (keep_mask & (1_u8 << i)) != 0 {
                        expected_kept_samples += 1;
                    }
                }
                if (keep_mask & (1_u8 << i)) != 0 {
                    keep_pids.insert(pid);
                }
            }
            let full = b.finish();
            let filtered = filter_capture_by_pids(&full, &keep_pids);
            proptest::prop_assert!(filtered.validate().is_ok());
            proptest::prop_assert_eq!(filtered.samples.len(), expected_kept_samples);
            for (new_idx, proc_row) in filtered.processes.iter().enumerate() {
                let pid = proc_row.pid.get();
                proptest::prop_assert!(keep_pids.contains(&pid));
                let expected_cmd = format!("/bin/p{pid}");
                proptest::prop_assert_eq!(
                    filtered.setting(&format!("command {new_idx}")),
                    Some(expected_cmd.as_str())
                );
            }
        }
    }
}
