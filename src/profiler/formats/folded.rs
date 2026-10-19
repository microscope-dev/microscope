//! Folded stacks, as `FlameGraph` and speedscope read them: a line for each
//! stack, its frames outermost first and split by `;`, then its weight.

use std::collections::BTreeMap;

use crate::analysis::{CallTree, Keys};
use crate::capture::Capture;
use crate::capture::builder::{CaptureBuilder, LineInput};
use crate::capture::records::{FrameKind, NONE};
use crate::model::{Pid, Tid};

/// Reads folded stacks; each line's number is its stack's weight in
/// samples. The format has no times, threads, or files, and the capture
/// says so.
pub fn import(text: &str) -> Result<Capture, String> {
    let mut builder = CaptureBuilder::new();
    let thread = builder.thread(Pid::new(0), Tid::new(0), 0);
    builder.thread_name(thread, "every thread");
    let mut frames: BTreeMap<&str, u32> = BTreeMap::new();
    let mut stack = Vec::new();
    let mut total = 0_u64;

    for (number, line) in text.lines().enumerate() {
        let line = line.trim_end();
        if line.is_empty() {
            continue;
        }

        let at = || format!("line {}", number + 1);
        let (names, weight) = line
            .rsplit_once(' ')
            .ok_or_else(|| format!("{} has no weight after its stack", at()))?;
        let weight: u64 = weight
            .parse()
            .map_err(|_| format!("{} ends in {weight:?}, not a weight", at()))?;
        if weight == 0 {
            continue;
        }

        total = total
            .checked_add(weight)
            .ok_or_else(|| format!("{} brings the weights past {}", at(), u64::MAX))?;

        stack.clear();
        for name in names.trim_end().split(';').rev() {
            let next = frames.len() as u64;
            let frame = *frames.entry(name).or_insert_with(|| {
                let frame = builder.frame(FrameKind::Native, NONE, next, 0);
                let line = LineInput {
                    function: name,
                    file: None,
                    line: 0,
                    column: 0,
                    declared: None,
                    role: 0,
                };

                builder.symbolize(frame, NONE, &[line]);
                frame
            });
            stack.push(frame);
        }

        let stack = builder.stack(&stack);
        builder.sample(thread, 0, weight, stack, NONE);
    }

    builder.setting("source", "folded stacks");
    builder.setting("weight", "samples");
    builder.setting("sample times", "none");
    Ok(builder.finish())
}

/// A call tree as folded stacks: each path's own weight, outermost frame
/// first. A name's own `;` is written `:`, as the format cannot hold it.
pub fn export(keys: &Keys<'_>, tree: &CallTree) -> String {
    let mut names: BTreeMap<u32, String> = BTreeMap::new();
    let mut path = Vec::new();
    let mut out = String::new();

    for node in (0..tree.len()).filter(|&node| tree.own[node] > 0) {
        path.clear();
        let mut at = u32::try_from(node).unwrap_or(NONE);
        while at != NONE {
            path.push(tree.key[at as usize]);
            at = tree.parent[at as usize];
        }

        for (position, key) in path.iter().rev().enumerate() {
            if position > 0 {
                out.push(';');
            }

            let name = names
                .entry(*key)
                .or_insert_with(|| keys.name(*key).replace(['\n', ';'], ":"));
            out.push_str(name);
        }

        out.push(' ');
        out.push_str(&tree.own[node].to_string());
        out.push('\n');
    }

    out
}
