use std::collections::BTreeMap;
use std::io::Write;

use anyhow::Result;
use microscope::profiler::Profile;
use microscope::syscalls::fds::FdTable;
use microscope::syscalls::{Errno, Filter, Losses, SyscallStats, TraceEvent};

use crate::cli::terminal::{Renderer, Role};
use crate::present::syscalls::summary::{self, Column};
use crate::present::syscalls::{self as present, Context, Kind, Tokens};

pub struct Lines {
    start: Option<u64>,
    fds: FdTable,
    filter: Option<Filter>,
    renderer: Renderer,
}

impl Lines {
    pub fn new(start: Option<u64>, filter: Option<Filter>, renderer: Renderer) -> Self {
        Self {
            start,
            fds: FdTable::default(),
            filter,
            renderer,
        }
    }

    pub fn line(&mut self, event: &TraceEvent, wanted: bool) -> Option<String> {
        let TraceEvent::Call(call) = event else {
            self.fds.apply(event);
            let shown = match (event, &self.filter) {
                (TraceEvent::Signal { signal, .. }, Some(filter)) => {
                    filter.signals.contains(signal)
                }
                _ => true,
            };

            let thread = match *event {
                TraceEvent::Signal { thread, .. } | TraceEvent::ThreadExit { thread, .. } => {
                    Some(thread.get())
                }
                TraceEvent::Exit { process, .. } => Some(process.get()),
                _ => None,
            };

            let tokens = present::other(event).filter(|_| wanted && shown)?;
            return Some(self.lead(event.time(), thread, &tokens));
        };

        let fds = &self.fds;
        let shown = wanted
            && self.filter.as_ref().is_none_or(|filter| {
                let paths = microscope::syscalls::filter::paths(call);
                let paths: Vec<&str> = paths.iter().map(String::as_str).collect();
                filter.shows(call, &paths, &|fd| {
                    fds.describe(call.process, fd)
                        .map(|description| description.text.to_string())
                })
            });

        let before = Context {
            fds: Some(&self.fds),
            before: true,
        };
        let mut tokens = shown.then(|| present::entry(call, before));
        self.fds.apply_call(call);
        let mut tokens = tokens.take()?;
        let after = Context {
            fds: Some(&self.fds),
            before: false,
        };
        tokens.0.extend(present::returned(call, after).0);
        if let Some(took) = call.duration() {
            tokens.push(Kind::Time, format!(" <{}>", present::duration(took)));
        }

        Some(self.lead(call.time(), Some(call.thread.get()), &tokens))
    }

    fn lead(&mut self, time: u64, thread: Option<u32>, tokens: &Tokens) -> String {
        let start = *self.start.get_or_insert(time);
        #[expect(clippy::cast_precision_loss, reason = "seconds to show")]
        let seconds = time.saturating_sub(start) as f64 / 1e9;
        let time = self.renderer.paint(Role::Muted, format!("{seconds:>11.6}"));
        let thread = thread.map_or_else(String::new, |thread| thread.to_string());
        let thread = self.renderer.paint(Role::Name, format!("{thread:>7}"));
        format!("{time} {thread} {}", self.paint(tokens))
    }

    fn paint(&self, tokens: &Tokens) -> String {
        tokens
            .0
            .iter()
            .map(|token| {
                let role = match token.kind {
                    Kind::Name | Kind::Pid => Some(Role::Name),
                    Kind::Number => Some(Role::Number),
                    Kind::String => Some(Role::String),
                    Kind::Constant => Some(Role::Type),
                    Kind::Fd | Kind::Path | Kind::Address => Some(Role::Metadata),
                    Kind::Error => Some(Role::Error),
                    Kind::Comment | Kind::Time => Some(Role::Muted),
                    Kind::Missing => Some(Role::Warning),
                    Kind::Punctuation | Kind::Text => None,
                };

                role.map_or_else(
                    || token.text.clone(),
                    |role| self.renderer.paint(role, &token.text).to_string(),
                )
            })
            .collect()
    }
}

fn none_kept(profile: &Profile) -> Option<String> {
    if profile.has_syscalls() {
        return None;
    }

    Some(profile.setting("syscalls not recorded").map_or_else(
        || "the recording kept no system calls; record them with --syscalls calls".to_owned(),
        |why| format!("the recording kept no system calls: {why}"),
    ))
}

pub fn log(
    profile: &Profile,
    filter: Option<Filter>,
    focus: Option<&str>,
    renderer: Renderer,
    out: &mut impl Write,
) -> Result<()> {
    if let Some(why) = none_kept(profile) {
        anyhow::bail!(why);
    }

    let syscalls = profile.syscalls();
    let (start, _) = profile.span();
    let mut lines = Lines::new(Some(start.cast_unsigned()), filter, renderer);
    let mut through: BTreeMap<u32, bool> = BTreeMap::new();
    for recorded in &syscalls.events {
        let wanted = match (focus, &recorded.event) {
            (Some(function), TraceEvent::Call(_)) => recorded.stack.is_some_and(|row| {
                *through.entry(row).or_insert_with(|| {
                    profile
                        .syscall_stack(row)
                        .0
                        .iter()
                        .any(|name| name == function)
                })
            }),
            (Some(_), _) => false,
            (None, _) => true,
        };

        if let Some(line) = lines.line(&recorded.event, wanted) {
            writeln!(out, "{line}")?;
        }
    }

    trace_notes(profile, syscalls.lost, out)
}

fn trace_notes(profile: &Profile, lost: Losses, out: &mut impl Write) -> Result<()> {
    if profile.setting("syscall engine") == Some("ptrace") {
        writeln!(
            out,
            "Traced with ptrace, which stops the program at each traced call: it ran slower than it would, and each call's time includes the stop."
        )?;
    }

    for (count, says) in [
        (
            lost.dropped,
            "records the kernel had no room for, or program runs it skipped: some calls lack detail, and counts may be short",
        ),
        (
            lost.unseen,
            "calls never returned where the trace could see: they are not counted",
        ),
        (
            lost.capped,
            "calls were over the cap: counted, but not kept",
        ),
        (
            lost.overflowed,
            "calls came while the kernel's ring was full: counted, but not kept",
        ),
    ] {
        if count > 0 {
            writeln!(out, "{count} {says}.")?;
        }
    }

    Ok(())
}

fn errno_name(number: u16) -> String {
    present::errno(Errno(number)).0
}

#[expect(
    clippy::cast_precision_loss,
    reason = "seconds need no more precision than f64's"
)]
pub fn summary(profile: &Profile, limit: usize, out: &mut impl Write) -> Result<()> {
    if let Some(why) = none_kept(profile) {
        anyhow::bail!(why);
    }

    let syscalls = profile.syscalls();
    let calls = syscalls.stats().per_call();
    let (start, end) = profile.span();
    let program = profile.setting("program").unwrap_or("the program");
    let made: u64 = calls.values().map(|stats| stats.calls).sum();
    let failed: u64 = calls.values().map(|stats| stats.errors).sum();

    writeln!(
        out,
        "{program}: {made} system calls, {failed} failed, over {:.3} s",
        end.saturating_sub(start) as f64 / 1e9
    )?;

    trace_notes(profile, syscalls.lost, out)?;
    writeln!(out)?;
    for line in summary::table(&calls, &Column::DEFAULT) {
        writeln!(out, "{line}")?;
    }

    let sites = profile.syscall_sites(&syscalls);
    if sites.is_empty() {
        return Ok(());
    }

    let columns = [
        Column::TimePercent,
        Column::TotalTime,
        Column::Calls,
        Column::Errors,
        Column::P99,
        Column::Name,
    ];

    writeln!(out)?;
    writeln!(out, "By call site:")?;
    let rows = sites
        .iter()
        .map(|(site, stats)| (site.clone(), stats))
        .collect();
    for line in summary::labelled(rows, &columns, "site", limit) {
        writeln!(out, "{line}")?;
    }

    if sites.len() > limit {
        writeln!(
            out,
            "and {} more sites, counted in the total; --limit shows more",
            sites.len() - limit
        )?;
    }

    Ok(())
}

const BRIEF: usize = 3;

pub fn brief(profile: &Profile, out: &mut impl Write) -> Result<()> {
    if !profile.has_syscalls() {
        return Ok(());
    }

    let syscalls = profile.syscalls();
    let calls = syscalls.stats().per_call();
    let made: u64 = calls.values().map(|stats| stats.calls).sum();
    let failed: u64 = calls.values().map(|stats| stats.errors).sum();
    writeln!(out, "microscope: {made} system calls, {failed} failed")?;

    let named = |call: &microscope::syscalls::SyscallId| call.display_name().into_owned();
    let mut busiest: Vec<_> = calls.iter().collect();
    busiest.sort_by_key(|(_, stats)| std::cmp::Reverse(stats.total_ns));
    let busiest: Vec<String> = busiest
        .iter()
        .take(BRIEF)
        .map(|(call, stats)| {
            let plural = if stats.calls == 1 { "" } else { "s" };
            format!(
                "{} {} in {} call{plural}",
                named(call),
                present::duration(stats.total_ns),
                stats.calls,
            )
        })
        .collect();
    writeln!(out, "  most time:  {}", busiest.join(", "))?;

    let mut failing: Vec<(String, u64)> = calls
        .iter()
        .flat_map(|(call, stats)| {
            stats.errors_by_errno.iter().map(move |(&errno, &count)| {
                (format!("{} {}", named(call), errno_name(errno)), count)
            })
        })
        .collect();
    failing.sort_by_key(|(_, count)| std::cmp::Reverse(*count));
    if !failing.is_empty() {
        let failing: Vec<String> = failing
            .iter()
            .take(BRIEF)
            .map(|(what, count)| format!("{what} × {count}"))
            .collect();
        writeln!(out, "  failed:     {}", failing.join(", "))?;
    }

    let sites = profile.syscall_sites(&syscalls);
    let p99 = |stats: &SyscallStats| stats.quantile_ns(0.99).unwrap_or(stats.max_ns);
    let mut slowest: Vec<_> = sites.iter().collect();
    slowest.sort_by_key(|(_, stats)| std::cmp::Reverse(p99(stats)));
    if !slowest.is_empty() {
        let slowest: Vec<String> = slowest
            .iter()
            .take(BRIEF)
            .map(|(site, stats)| format!("{site} {} p99", present::duration(p99(stats))))
            .collect();
        writeln!(out, "  slowest:    {}", slowest.join(", "))?;
    }

    let mut notes = Vec::new();
    trace_notes(profile, syscalls.lost, &mut notes)?;
    for note in String::from_utf8_lossy(&notes).lines() {
        writeln!(out, "  {note}")?;
    }

    Ok(())
}
