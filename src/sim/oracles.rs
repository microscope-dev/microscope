//! Checks of the debugger against the simulation's ground truth.
//!
//! Each oracle states one rule and, when it fails, cites both sides of the
//! disagreement. An oracle is never loosened to make a run pass; one that
//! is wrong is fixed in a commit that explains why.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use super::corpus::Run;
use super::kernel::{ExitStatus, Kernel, Parent, State, Tgid, Tid};
use super::loader::Image;
use crate::backend::sim_edge::Truth;

const BREAKPOINT: u8 = 0xcc;

/// The bytes of code [`code_integrity`] compares at once.
const LINE: usize = 64;

/// Code integrity: every byte of executable memory holds the image's
/// original, or `0xcc` exactly where the controller says it installed a
/// trap over that original.
pub fn code_integrity(kernel: &Kernel, truth: &Truth, image: &Image) -> Result<(), String> {
    let Some(process) = truth.inferior.and_then(|tgid| kernel.processes.get(&tgid)) else {
        return Ok(());
    };

    // Every trap the controller installed is in memory, over the byte it
    // remembers.
    for (&address, site) in &truth.sites {
        let Some(byte) = process.space.peek_bytes(address, 1) else {
            continue;
        };

        if site.installed && byte[0] != BREAKPOINT {
            return Err(format!(
                "the controller installed a trap at {address:#x}, but memory holds {:#04x}",
                byte[0]
            ));
        }

        if let Some(was) = image.original_byte(address)
            && site.original_byte != was
        {
            return Err(format!(
                "the controller remembers {:#04x} under its site at {address:#x}, but the \
                 program has {was:#04x} there",
                site.original_byte
            ));
        }
    }

    // Every other byte of code is the program's own. A page still shared
    // with the image was never written.
    for (page_address, original_page) in image.code() {
        if process
            .space
            .page(page_address)
            .is_some_and(|page| Arc::ptr_eq(page, original_page))
        {
            continue;
        }

        let current = process
            .space
            .peek_page(page_address)
            .ok_or_else(|| format!("code page {page_address:#x} is no longer mapped"))?;

        // Most of a page with traps in it is unchanged, which comparing it a
        // line at a time skips quickly.
        let lines = current.chunks(LINE).zip(original_page.chunks(LINE));
        for (line, (now, was)) in lines.enumerate() {
            if now == was {
                continue;
            }

            for (offset, (&now, &was)) in now.iter().zip(was).enumerate() {
                if now == was {
                    continue;
                }

                let address = page_address + (line * LINE + offset) as u64;
                let site = truth.sites.get(&address);
                if now != BREAKPOINT || !site.is_some_and(|site| site.installed) {
                    return Err(format!(
                        "code at {address:#x} is {now:#04x}, but the program has {was:#04x} \
                         there and the controller's site is {site:?}"
                    ));
                }
            }
        }
    }

    Ok(())
}

/// Breakpoint ownership: every site has an owner, and only the execution
/// in progress owns plan sites. A published stop ended every plan. The
/// controller restores nothing in a process that is ending as a whole.
pub fn site_ownership(kernel: &Kernel, truth: &Truth) -> Result<(), String> {
    if truth.inferior.is_some_and(|tgid| ending(kernel, tgid)) {
        return Ok(());
    }

    for (&address, site) in &truth.sites {
        if site.owners == 0 {
            return Err(format!("site {address:#x} has no owner"));
        }

        for &execution in &site.plans {
            if Some(execution) != truth.active_execution {
                return Err(format!(
                    "execution {execution}'s plan still owns {address:#x}, but the active \
                     execution is {:?}",
                    truth.active_execution
                ));
            }
        }
    }

    if truth.public_stop.is_some() && !truth.plan_sites.is_empty() {
        return Err(format!(
            "stop {:?} is published while plans still record sites: {:?}",
            truth.public_stop, truth.plan_sites
        ));
    }

    Ok(())
}

/// Whether `tgid` is ending as a whole: killed, by the debugger or from
/// outside, or exiting its group. Its threads then leave their stops
/// whatever the debugger publishes, until it hears of the exit.
pub(super) fn ending(kernel: &Kernel, tgid: Tgid) -> bool {
    kernel
        .processes
        .get(&tgid)
        .is_none_or(|process| process.group_exit.is_some())
}

/// All-stop: while a stop is published, every thread of the inferior is in
/// a ptrace-stop or has ended, and the controller knows exactly the stopped
/// ones.
pub fn all_stop(kernel: &Kernel, truth: &Truth) -> Result<(), String> {
    let (Some(stop), Some(tgid)) = (truth.public_stop, truth.inferior) else {
        return Ok(());
    };

    if ending(kernel, tgid) {
        return Ok(());
    }

    let mut stopped = BTreeSet::new();
    let mut ended = BTreeSet::new();

    for thread in kernel.threads_of(tgid) {
        match thread.state {
            State::Stopped { .. } => {
                stopped.insert(thread.tid);
            }
            State::Zombie(_) => {
                ended.insert(thread.tid);
            }
            state => {
                return Err(format!(
                    "stop {stop} is published while thread {} is {state:?}",
                    thread.tid
                ));
            }
        }
    }

    let unknown = stopped.difference(&truth.threads).next().is_some();
    let vanished = truth
        .threads
        .iter()
        .any(|tid| !stopped.contains(tid) && !ended.contains(tid));
    if unknown || vanished {
        return Err(format!(
            "stop {stop} is published with threads {:?}; the kernel has {stopped:?} stopped \
             and {ended:?} ended",
            truth.threads
        ));
    }

    Ok(())
}

/// Breakpoint accounting, unseen hits: no thread executed the program's own
/// instruction where the user's breakpoint is enabled, except to step over
/// the trap it just reported there.
pub fn unseen_hits(kernel: &Kernel) -> Result<(), String> {
    kernel.unseen_hits.first().map_or(Ok(()), |hit| {
        Err(format!(
            "thread {} executed the instruction at {:#x}, where the user's breakpoint is \
             enabled, without a trap reporting it",
            hit.tid, hit.address
        ))
    })
}

/// A trap whose stop the controller hears of.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeardTrap {
    pub tid: Tid,
    pub address: u64,

    /// When the thread executes the trap again, having executed nothing
    /// since it last trapped there, the breakpoints that counted its
    /// arrival and have owned the site ever since.
    pub again: Option<BTreeSet<u64>>,

    /// Whether the controller had begun to shut down.
    pub in_shutdown: bool,
}

/// Breakpoint accounting, hit counts: handling one message counts at most
/// the trap it reports, one hit for every user breakpoint owning the site.
/// The same arrival again (a signal interrupted the step over the trap)
/// counts nothing for a breakpoint that counted it, and at most one for one
/// that came to the site since, a breakpoint that left the site and came
/// back among them: whether the thread ever ran there, a debugger can only
/// guess. A new inferior starts every count again. A trap
/// may go uncounted once its process is exiting as a whole, and one heard
/// during a shutdown is no hit: the controller kills a launched process and
/// releases an attached one with the thread rewound.
pub fn hit_counts(
    before: &Truth,
    after: &Truth,
    trap: Option<&HeardTrap>,
    disturbed: bool,
) -> Result<(), String> {
    let started = after.inferior.is_some() && after.inferior != before.inferior;
    let owners = trap
        .and_then(|trap| before.sites.get(&trap.address))
        .map(|site| site.users.clone())
        .unwrap_or_default();
    let again = trap.and_then(|trap| trap.again.as_ref());
    let in_shutdown = trap.is_some_and(|trap| trap.in_shutdown);

    for (&id, breakpoint) in &after.breakpoints {
        let previous = before
            .breakpoints
            .get(&id)
            .map_or(0, |earlier| earlier.hit_count);
        let now = breakpoint.hit_count;
        let owned = owners.contains(&id);
        let allowed = if started {
            now == 0
        } else if !owned || in_shutdown || again.is_some_and(|counted| counted.contains(&id)) {
            now == previous
        } else if disturbed || again.is_some() {
            now == previous || now == previous + 1
        } else {
            now == previous + 1
        };

        if !allowed {
            let reason = trap.map_or_else(
                || "the message reported no trap".to_owned(),
                |trap| {
                    format!(
                        "thread {} trapped at {:#x}{}",
                        trap.tid,
                        trap.address,
                        if again.is_some() { " again" } else { "" }
                    )
                },
            );
            return Err(format!(
                "breakpoint {id}'s hit count went from {previous} to {now}; {reason}, and \
                 the breakpoint {} that site",
                if owned { "owned" } else { "did not own" }
            ));
        }
    }

    Ok(())
}

/// Keeps, of the breakpoints that counted a thread's arrival at `address`,
/// those that still own the site there. One that left it, because it was
/// disabled or lost the location, has come to the site since if it comes
/// back.
pub fn still_counting(counted: &mut BTreeSet<u64>, truth: &Truth, address: u64) {
    let owners = truth.sites.get(&address).map(|site| &site.users);
    counted.retain(|id| owners.is_some_and(|users| users.contains(id)));
}

/// Breakpoint accounting, ownership: while a stop is published, every
/// breakpoint the user was told exists, and has not asked to remove, owns
/// an installed site at each of its locations.
pub fn user_breakpoints(
    kernel: &Kernel,
    truth: &Truth,
    intent: &BTreeMap<u64, BTreeSet<u64>>,
) -> Result<(), String> {
    let (Some(stop), Some(tgid)) = (truth.public_stop, truth.inferior) else {
        return Ok(());
    };

    if ending(kernel, tgid) {
        return Ok(());
    }

    for (&id, addresses) in intent {
        let Some(breakpoint) = truth.breakpoints.get(&id) else {
            return Err(format!(
                "stop {stop}: the controller forgot breakpoint {id} at {addresses:x?}"
            ));
        };

        if breakpoint.addresses != *addresses {
            return Err(format!(
                "stop {stop}: breakpoint {id} was added at {addresses:x?}, but the controller \
                 has it at {:x?}",
                breakpoint.addresses
            ));
        }

        for address in addresses {
            if !truth
                .sites
                .get(address)
                .is_some_and(|site| site.installed && site.users.contains(&id))
            {
                return Err(format!(
                    "stop {stop}: breakpoint {id} owns no installed site at {address:#x}: {:?}",
                    truth.sites.get(address)
                ));
            }
        }
    }

    Ok(())
}

/// Breakpoint accounting, disabled breakpoints: one the debugger said it
/// disabled, and the user has not asked to enable or remove since, owns no
/// site. A process ending as a whole cannot have its memory written, so its
/// sites stay as they were, as a removed breakpoint's do.
pub fn disabled_breakpoints(
    kernel: &Kernel,
    truth: &Truth,
    disabled: &BTreeSet<u64>,
) -> Result<(), String> {
    if truth.inferior.is_some_and(|tgid| ending(kernel, tgid)) {
        return Ok(());
    }

    for (&address, site) in &truth.sites {
        if let Some(id) = site.users.intersection(disabled).next() {
            return Err(format!(
                "disabled breakpoint {id} owns the site at {address:#x}: {site:?}"
            ));
        }
    }

    Ok(())
}

/// Transparency, while the program runs: what it wrote so far begins what
/// it writes undisturbed.
pub fn output_so_far(kernel: &Kernel, run: &Run) -> Result<(), String> {
    for output in kernel.outputs.values() {
        if !run.output.as_bytes().starts_with(output) {
            return Err(format!(
                "the program wrote {:?}, which does not begin {:?}",
                String::from_utf8_lossy(output),
                run.output
            ));
        }
    }

    Ok(())
}

/// Clean exit: when the session is over, no simulated process remains,
/// not even one waiting to be reaped.
pub fn clean_exit(kernel: &Kernel) -> Result<(), String> {
    if let Some(thread) = kernel.threads.values().next() {
        return Err(format!(
            "thread {} of process {} outlived the session, {:?}",
            thread.tid, thread.tgid, thread.state
        ));
    }

    if let Some((tgid, zombie)) = kernel.zombies.iter().next() {
        return Err(format!(
            "process {tgid} outlived the session unreaped by {}",
            zombie.parent
        ));
    }

    Ok(())
}

/// Transparency, once the program ended. A process the tracer launched or
/// attached to that exited by itself wrote, with the children it forked,
/// exactly what it writes undisturbed, and exited as it does; one killed,
/// which only SIGKILL does to a program that raises no signal, wrote only
/// a beginning of that. A child its parent reaped shows in what
/// the parent wrote. A child init reaped ran on alone once released: the
/// corpus's children check their own work and exit 0 when it is right.
pub fn transparency(kernel: &Kernel, run: &Run) -> Result<(), String> {
    for (tgid, ended) in &kernel.ended {
        match ended.reaper {
            Parent::Tracer | Parent::Launcher => {}
            Parent::Init if !ended.killed_externally && ended.status != ExitStatus::Code(0) => {
                return Err(format!(
                    "child {tgid}, released and orphaned, ended {:?}; undisturbed it exits 0",
                    ended.status
                ));
            }
            Parent::Init | Parent::Process(_) => continue,
        }

        let status = &ended.status;
        if let ExitStatus::Signal(signal, _) = *status
            && signal != nix::libc::SIGKILL
        {
            return Err(format!(
                "process {tgid} died of signal {signal}; undisturbed it exits {}",
                run.exit_code
            ));
        }

        let output = kernel
            .outputs
            .get(&ended.root)
            .map_or(&[][..], Vec::as_slice);
        if !run.output.as_bytes().starts_with(output) {
            return Err(format!(
                "process {tgid} wrote {:?}, which does not begin {:?}",
                String::from_utf8_lossy(output),
                run.output
            ));
        }

        if let ExitStatus::Code(code) = *status
            && (code & 0xff != run.exit_code || output != run.output.as_bytes())
        {
            return Err(format!(
                "process {tgid} exited {code} after writing {:?}; undisturbed it exits {} \
                 after writing {:?}",
                String::from_utf8_lossy(output),
                run.exit_code,
                run.output
            ));
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::sim_edge::{Site, UserBreakpoint};
    use crate::sim::corpus::Corpus;
    use crate::sim::kernel::Ended;

    fn site(original_byte: u8, installed: bool, plans: Vec<u64>) -> Site {
        Site {
            original_byte,
            installed,
            owners: plans.len().max(1),
            plans,
            users: BTreeSet::new(),
        }
    }

    /// Code integrity accepts a trap the controller installed over the
    /// byte it remembers, and refuses a trap it does not know of and one
    /// over a byte it misremembers.
    #[test]
    fn code_integrity_requires_every_trap_to_be_owned_and_remembered() {
        let corpus = Corpus::load().expect("load the golden corpus");
        let variant = &corpus.programs[0].variants[0];
        let mut kernel = Kernel::new(100);
        let tgid = kernel.spawn(Arc::clone(&variant.image), &variant.path, &[], [0; 16]);
        let entry = variant.image.entry();
        let word = kernel.peek(tgid, entry).expect("read the entry");
        let original = word.to_le_bytes()[0];
        kernel
            .poke(tgid, entry, (word & !0xff) | u64::from(BREAKPOINT))
            .expect("plant a trap");
        let truth = |sites: Vec<(u64, Site)>| Truth {
            sites: sites.into_iter().collect(),
            inferior: Some(Tgid::led_by(tgid)),
            ..Truth::default()
        };

        assert_eq!(
            code_integrity(
                &kernel,
                &truth(vec![(entry, site(original, true, vec![]))]),
                &variant.image
            ),
            Ok(())
        );
        let unknown = code_integrity(&kernel, &truth(vec![]), &variant.image);
        assert!(
            unknown.is_err_and(|message| message.contains("program has")),
            "unknown trap"
        );
        let misremembered = code_integrity(
            &kernel,
            &truth(vec![(entry, site(original ^ 1, true, vec![]))]),
            &variant.image,
        );
        assert!(misremembered.is_err_and(|message| message.contains("remembers")));
        let lifted = code_integrity(
            &kernel,
            &truth(vec![(entry, site(original, false, vec![]))]),
            &variant.image,
        );
        assert!(lifted.is_err(), "a lifted site must hold its original byte");
    }

    /// Only the active execution owns plan sites, a published stop ended
    /// every plan, and no site is left without owners.
    #[test]
    fn site_ownership_ends_plans_with_their_execution() {
        let owned = |plans: Vec<u64>| Truth {
            sites: BTreeMap::from([(0x1000, site(0x55, true, plans))]),
            ..Truth::default()
        };

        assert_eq!(
            site_ownership(
                &Kernel::new(100),
                &Truth {
                    active_execution: Some(2),
                    ..owned(vec![2])
                }
            ),
            Ok(())
        );
        assert!(
            site_ownership(
                &Kernel::new(100),
                &Truth {
                    active_execution: Some(3),
                    ..owned(vec![2])
                }
            )
            .is_err()
        );
        assert!(
            site_ownership(
                &Kernel::new(100),
                &Truth {
                    public_stop: Some(4),
                    plan_sites: BTreeMap::from([(2, BTreeSet::from([0x1000]))]),
                    ..owned(vec![])
                }
            )
            .is_err()
        );
        let mut orphan = owned(vec![]);
        orphan.sites.get_mut(&0x1000).expect("the site").owners = 0;
        assert!(site_ownership(&Kernel::new(100), &orphan).is_err());
    }

    /// A program that exited by itself must have done exactly what it does
    /// undisturbed; a killed one may have done only a beginning of it.
    #[test]
    fn transparency_compares_ends_with_the_manifest() {
        let run = Run {
            arguments: Vec::new(),
            exit_code: 2,
            output: "total 202\n".into(),
        };

        let ended = |status, output: &str| {
            let mut kernel = Kernel::new(100);
            kernel.ended.insert(
                Tgid::new(1000),
                Ended {
                    status,
                    root: 1000,
                    reaper: Parent::Tracer,
                    killed_externally: false,
                },
            );
            kernel.outputs.insert(1000, output.as_bytes().to_vec());

            transparency(&kernel, &run)
        };

        assert_eq!(ended(ExitStatus::Code(2), "total 202\n"), Ok(()));
        assert_eq!(ended(ExitStatus::Signal(9, false), "tot"), Ok(()));
        assert!(ended(ExitStatus::Code(3), "total 202\n").is_err());
        assert!(ended(ExitStatus::Code(2), "total 20").is_err());
        assert!(ended(ExitStatus::Signal(9, false), "total 9").is_err());
    }

    /// A user site at 0x1000 owned by breakpoints 1 and 2, with breakpoint
    /// 3 elsewhere, each with `hits`.
    fn with_hits(hits: [u64; 3]) -> Truth {
        let mut users = site(0x55, true, vec![]);
        users.users = BTreeSet::from([1, 2]);
        Truth {
            sites: BTreeMap::from([(0x1000, users)]),
            breakpoints: (1..=3)
                .zip(hits)
                .map(|(id, hit_count)| {
                    (
                        id,
                        UserBreakpoint {
                            addresses: BTreeSet::from([if id == 3 { 0x2000 } else { 0x1000 }]),
                            hit_count,
                        },
                    )
                })
                .collect(),
            inferior: Some(Tgid::new(1000)),
            ..Truth::default()
        }
    }

    /// An arrival at a trap counts one hit for each owner of its site and
    /// none for any other breakpoint; the trap executed again counts none
    /// for a breakpoint that counted the arrival and at most one for any
    /// other owner; a message without a trap counts none; a new inferior
    /// starts the counts again; a trap a group exit disturbed may go
    /// uncounted; and one heard during a shutdown counts none.
    #[test]
    fn hit_counts_follow_the_traps_the_controller_hears_of() {
        let before = with_hits([4, 0, 7]);
        let arrival = HeardTrap {
            tid: 1001,
            address: 0x1000,
            again: None,
            in_shutdown: false,
        };

        let trap = Some(&arrival);
        assert_eq!(
            hit_counts(&before, &with_hits([5, 1, 7]), trap, false),
            Ok(())
        );
        for after in [[6, 1, 7], [5, 0, 7], [5, 1, 8]] {
            assert!(hit_counts(&before, &with_hits(after), trap, false).is_err());
        }

        let again = HeardTrap {
            again: Some(BTreeSet::from([1])),
            ..arrival.clone()
        };

        for after in [[4, 0, 7], [4, 1, 7]] {
            assert_eq!(
                hit_counts(&before, &with_hits(after), Some(&again), false),
                Ok(())
            );
        }

        for after in [[5, 0, 7], [4, 2, 7], [4, 0, 8]] {
            assert!(hit_counts(&before, &with_hits(after), Some(&again), false).is_err());
        }

        assert_eq!(hit_counts(&before, &before, None, false), Ok(()));
        assert!(hit_counts(&before, &with_hits([5, 0, 7]), None, false).is_err());

        let relaunched = |hits| Truth {
            inferior: Some(Tgid::new(1002)),
            ..with_hits(hits)
        };

        assert_eq!(
            hit_counts(&before, &relaunched([0, 0, 0]), None, false),
            Ok(())
        );
        assert!(hit_counts(&before, &relaunched([4, 0, 7]), None, false).is_err());
        assert_eq!(hit_counts(&before, &before, trap, true), Ok(()));
        assert!(hit_counts(&before, &with_hits([6, 1, 7]), trap, true).is_err());

        let in_shutdown = HeardTrap {
            in_shutdown: true,
            ..arrival
        };

        assert_eq!(
            hit_counts(&before, &before, Some(&in_shutdown), false),
            Ok(())
        );
        assert!(hit_counts(&before, &with_hits([5, 1, 7]), Some(&in_shutdown), false).is_err());
    }

    /// A published stop requires every live thread stopped and known.
    #[test]
    fn all_stop_requires_every_live_thread_stopped_and_known() {
        let corpus = Corpus::load().expect("load the golden corpus");
        let variant = &corpus.programs[0].variants[0];
        let mut kernel = Kernel::new(100);
        let tgid = kernel.spawn(Arc::clone(&variant.image), &variant.path, &[], [0; 16]);
        let truth = |threads: Vec<Tid>| Truth {
            public_stop: Some(1),
            inferior: Some(Tgid::led_by(tgid)),
            threads: threads.into_iter().collect(),
            ..Truth::default()
        };

        assert_eq!(all_stop(&kernel, &truth(vec![tgid])), Ok(()));
        assert!(all_stop(&kernel, &truth(vec![])).is_err(), "unknown thread");
        assert!(
            all_stop(&kernel, &truth(vec![tgid, tgid + 1])).is_err(),
            "vanished thread"
        );
        kernel
            .resume(tgid, None, false)
            .expect("resume the stopped thread");

        assert!(all_stop(&kernel, &truth(vec![tgid])).is_err(), "running");
        assert_eq!(
            all_stop(
                &kernel,
                &Truth {
                    public_stop: None,
                    ..truth(vec![tgid])
                }
            ),
            Ok(())
        );
    }

    /// An arrival stays counted by the breakpoints that still own its site.
    #[test]
    fn arrivals_stay_counted_only_by_breakpoints_that_stay() {
        let truth = with_hits([0, 0, 0]);
        let mut counted = BTreeSet::from([1, 2, 3]);
        still_counting(&mut counted, &truth, 0x1000);
        assert_eq!(counted, BTreeSet::from([1, 2]));
        still_counting(&mut counted, &truth, 0x2000);
        assert!(counted.is_empty());
    }

    /// Every breakpoint the user was told exists owns an installed site at
    /// each of its locations while a stop is published.
    #[test]
    fn user_breakpoints_own_their_sites_while_stopped() {
        let corpus = Corpus::load().expect("load the golden corpus");
        let variant = &corpus.programs[0].variants[0];
        let mut kernel = Kernel::new(100);
        let tgid = kernel.spawn(Arc::clone(&variant.image), &variant.path, &[], [0; 16]);
        let truth = Truth {
            public_stop: Some(1),
            inferior: Some(Tgid::led_by(tgid)),
            ..with_hits([0, 0, 0])
        };

        let intent = |entries: Vec<(u64, u64)>| {
            entries
                .into_iter()
                .map(|(id, address)| (id, BTreeSet::from([address])))
                .collect::<BTreeMap<_, _>>()
        };

        assert_eq!(
            user_breakpoints(&kernel, &truth, &intent(vec![(1, 0x1000), (2, 0x1000)])),
            Ok(())
        );
        assert!(user_breakpoints(&kernel, &truth, &intent(vec![(4, 0x1000)])).is_err());
        assert!(user_breakpoints(&kernel, &truth, &intent(vec![(1, 0x3000)])).is_err());

        // Breakpoint 3's location has no site.
        assert!(user_breakpoints(&kernel, &truth, &intent(vec![(3, 0x2000)])).is_err());
    }

    /// A breakpoint the user was told is disabled owns no site.
    #[test]
    fn disabled_breakpoints_own_no_site() {
        let corpus = Corpus::load().expect("load the golden corpus");
        let variant = &corpus.programs[0].variants[0];
        let mut kernel = Kernel::new(100);
        let tgid = kernel.spawn(Arc::clone(&variant.image), &variant.path, &[], [0; 16]);
        let truth = Truth {
            inferior: Some(Tgid::led_by(tgid)),
            ..with_hits([0, 0, 0])
        };

        assert_eq!(
            disabled_breakpoints(&kernel, &truth, &BTreeSet::from([3])),
            Ok(())
        );
        assert!(disabled_breakpoints(&kernel, &truth, &BTreeSet::from([2, 3])).is_err());
    }
}
