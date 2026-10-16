//! Each recorded process's anonymous code, and the perf map that names
//! the functions a compiler wrote there. Each anonymous range a map covers
//! becomes a module of its own (see [`crate::debug_info::perf_map`]).
//!
//! The map is append-only and written as it is read, so it is read on
//! from where it was last read, and only when a sample lands in anonymous
//! code no function it named covers. A function's code is read with it:
//! its frames are proved from the code as it is then.

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::backend::perf::{self, jit::MapError};
use crate::debug_info::perf_map::{self, Jitted};

/// The most of one function's code read: its frames are proved from its
/// entry, so a frame further in than this has none.
const CODE_READ: u64 = 64 * 1024;

/// One anonymous executable mapping.
#[derive(Debug, Clone, Copy)]
struct Range {
    end: u64,

    /// Whether its module holds every function the map named in it.
    current: bool,
}

/// What a process's anonymous code is known to hold.
#[derive(Debug, Default)]
pub struct Jit {
    ranges: BTreeMap<u64, Range>,

    /// Each function the map named, by start, a later one replacing those
    /// it overlaps.
    functions: BTreeMap<u64, Jitted>,

    /// How many bytes of the map are read, and the path the process
    /// names it by, once it has been read.
    read: u64,
    path: Option<String>,

    /// Why the map was last not read, until it is taken.
    problem: Option<String>,
}

/// An anonymous range whose module is out of date, with the functions in
/// it.
pub struct Stale {
    pub start: u64,
    pub end: u64,
    pub path: String,
    pub functions: Vec<Jitted>,
}

impl Jit {
    /// Records a mapping of `start..end`, which replaces whatever it
    /// overlaps, and is anonymous code when `anonymous`.
    pub fn mapped(&mut self, start: u64, end: u64, anonymous: bool) {
        self.ranges
            .retain(|&range, held| held.end <= start || range >= end);
        if anonymous {
            self.ranges.insert(
                start,
                Range {
                    end,
                    current: false,
                },
            );
        }
    }

    /// Whether the anonymous range that holds `address`, if any, needs its
    /// module made again: the map named functions in it since, or none
    /// covers `address`, so the map may have more.
    pub fn needs(&self, address: u64) -> bool {
        self.range(address)
            .is_some_and(|(_, range)| !range.current || !self.covers(address))
    }

    fn range(&self, address: u64) -> Option<(u64, Range)> {
        self.ranges
            .range(..=address)
            .next_back()
            .filter(|(_, range)| address < range.end)
            .map(|(&start, &range)| (start, range))
    }

    fn covers(&self, address: u64) -> bool {
        self.functions
            .range(..=address)
            .next_back()
            .is_some_and(|(_, function)| address < function.entry.end)
    }

    /// Reads the map of process `pid` on when some sampled `addresses`
    /// are anonymous code no function covers, and returns each range
    /// whose module is out of date. Each range's module is made again
    /// from what it returns, which marks it current.
    pub fn refresh(&mut self, pid: u32, addresses: impl IntoIterator<Item = u64>) -> Vec<Stale> {
        let mut uncovered = false;
        let mut needed = false;
        for address in addresses {
            if self.needs(address) {
                needed = true;
                uncovered |= !self.covers(address);
            }
        }

        if !needed {
            return Vec::new();
        }

        if uncovered {
            self.read_on(pid);
        }

        let Some(path) = self.path.clone() else {
            return Vec::new();
        };

        let mut stale = Vec::new();
        for (&start, range) in &mut self.ranges {
            if range.current {
                continue;
            }

            range.current = true;
            let functions: Vec<Jitted> = self
                .functions
                .range(start..range.end)
                .map(|(_, function)| function.clone())
                .collect();
            if !functions.is_empty() {
                stale.push(Stale {
                    start,
                    end: range.end,
                    path: path.clone(),
                    functions,
                });
            }
        }

        stale
    }

    /// Why the map was not read, the first time it is asked after a read
    /// found a map that cannot be used.
    pub const fn take_problem(&mut self) -> Option<String> {
        self.problem.take()
    }

    /// Reads the lines written since the last read, with each new
    /// function's code.
    fn read_on(&mut self, pid: u32) {
        let (path, bytes) = match perf::jit::read_map(pid, self.read) {
            Ok(read) => read,
            Err(MapError::Absent) => return,
            Err(problem) => {
                self.problem = Some(problem.to_string());
                return;
            }
        };

        self.path = Some(path);
        let (entries, used) = perf_map::parse(&bytes);
        self.read += used as u64;
        for entry in entries {
            let length = (entry.end - entry.start).min(CODE_READ);
            let mut code = vec![0; usize::try_from(length).unwrap_or(0)];
            let code = perf::read_memory(pid, entry.start, &mut code)
                .ok()
                .map(|read| Arc::from(&code[..read.min(code.len())]));
            let overlapped: Vec<u64> = self
                .functions
                .range(..entry.end)
                .rev()
                .take_while(|(_, earlier)| earlier.entry.end > entry.start)
                .map(|(&start, _)| start)
                .collect();
            for start in overlapped {
                self.functions.remove(&start);
            }

            if let Some((_, range)) = self.ranges.range_mut(..=entry.start).next_back()
                && entry.start < range.end
            {
                range.current = false;
            }

            self.functions.insert(entry.start, Jitted { entry, code });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::debug_info::perf_map::Entry;

    fn function(start: u64, end: u64) -> Jitted {
        Jitted {
            entry: Entry {
                start,
                end,
                name: "f".into(),
            },
            code: None,
        }
    }

    #[test]
    fn anonymous_code_needs_a_module_until_its_functions_have_one() {
        let mut jit = Jit::default();
        jit.mapped(0x1000, 0x2000, true);
        jit.mapped(0x3000, 0x4000, false);
        assert!(jit.needs(0x1000));
        assert!(!jit.needs(0x3000), "file-backed");
        assert!(!jit.needs(0x2000), "past the range");

        // Functions read since, as the map would give them.
        jit.functions.insert(0x1000, function(0x1000, 0x1010));
        jit.path = Some("/tmp/perf-1.map".into());
        let stale = jit.refresh(u32::MAX, [0x1008]);
        assert_eq!(stale.len(), 1);
        assert_eq!((stale[0].start, stale[0].end), (0x1000, 0x2000));
        assert!(!jit.needs(0x1008), "covered and current");
        assert!(jit.needs(0x1010), "uncovered: the map may name it since");
        assert!(jit.refresh(u32::MAX, [0x1008]).is_empty());

        // A mapping over the range replaces it.
        jit.mapped(0x1800, 0x2800, false);
        assert!(!jit.needs(0x1008));
    }
}
