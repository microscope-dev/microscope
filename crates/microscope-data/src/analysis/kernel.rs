use super::keys::Keys;
use super::query::{Query, syscalls};
use crate::capture::Capture;
use crate::capture::records::{CallOutcome, UNSEEN};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KernelLane {
    pub thread: u32,

    pub calls: Vec<u32>,
    pub failed: Vec<u32>,

    pub long: Vec<(u32, i64, i64)>,
}

#[must_use]
pub fn kernel_active_threads(
    capture: &Capture,
    keys: &Keys<'_>,
    query: &Query,
    buckets: usize,
) -> Vec<u32> {
    let Some((start, end)) = query
        .range
        .filter(|(start, end)| end > start && buckets > 0)
    else {
        return Vec::new();
    };

    let kept = syscalls(
        capture,
        keys,
        &Query {
            range: None,
            ..query.clone()
        },
    );
    let width = (end - start) / i64::try_from(buckets).unwrap_or(i64::MAX);
    let mut active = std::collections::BTreeSet::new();
    for (row, call) in capture.syscalls.iter().enumerate() {
        if !kept.get(row).copied().unwrap_or(false) {
            continue;
        }
        let (entered, exited) = (call.entered.get(), call.exited.get());
        let returned = if exited == UNSEEN { entered } else { exited };
        let until = if exited == UNSEEN && call.outcome == CallOutcome::Unfinished as u8 {
            end
        } else {
            exited
        };
        let (from, to) = (entered.max(start), until.min(end));
        if (start..end).contains(&returned)
            || (entered != UNSEEN
                && until != UNSEEN
                && until - entered >= width.max(1)
                && from < to)
        {
            active.insert(call.thread.get());
        }
    }
    active.into_iter().collect()
}

pub fn kernel_lanes(
    capture: &Capture,
    keys: &Keys<'_>,
    query: &Query,
    buckets: usize,
) -> Vec<KernelLane> {
    let _span = crate::span!("analysis kernel lanes");
    let Some((start, end)) = query
        .range
        .filter(|(start, end)| end > start && buckets > 0)
    else {
        return Vec::new();
    };

    let kept = syscalls(
        capture,
        keys,
        &Query {
            range: None,
            ..query.clone()
        },
    );

    let span = u128::try_from(end - start).unwrap_or(1);
    let slice_of = |time: i64| {
        let offset = u128::try_from(time - start).unwrap_or(0);
        usize::try_from(offset * buckets as u128 / span).unwrap_or(buckets - 1)
    };
    let width = (end - start) / i64::try_from(buckets).unwrap_or(i64::MAX);

    let mut lanes: std::collections::BTreeMap<u32, KernelLane> = std::collections::BTreeMap::new();
    for (row, call) in capture.syscalls.iter().enumerate() {
        if !kept.get(row).copied().unwrap_or(false) {
            continue;
        }

        let (entered, exited) = (call.entered.get(), call.exited.get());
        let returned = if exited == UNSEEN { entered } else { exited };
        let thread = call.thread.get();
        let lane = || KernelLane {
            thread,
            calls: vec![0; buckets],
            failed: vec![0; buckets],
            long: Vec::new(),
        };

        if (start..end).contains(&returned) {
            let lane = lanes.entry(thread).or_insert_with(lane);
            let slice = slice_of(returned);
            if let Some(count) = lane.calls.get_mut(slice) {
                *count += 1;
            }

            if call.outcome == CallOutcome::Failed as u8
                && let Some(count) = lane.failed.get_mut(slice)
            {
                *count += 1;
            }
        }

        let until = if exited == UNSEEN && call.outcome == CallOutcome::Unfinished as u8 {
            end
        } else {
            exited
        };
        let (from, to) = (entered.max(start), until.min(end));
        if entered != UNSEEN && until != UNSEEN && until - entered >= width.max(1) && from < to {
            let lane = lanes.entry(thread).or_insert_with(lane);
            lane.long.push((super::index(row), from, to));
        }
    }

    let mut lanes: Vec<KernelLane> = lanes.into_values().collect();
    for lane in &mut lanes {
        lane.long.sort_unstable_by_key(|&(_, from, _)| from);
    }

    lanes
}
