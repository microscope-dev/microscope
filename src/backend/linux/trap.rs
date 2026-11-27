//! Stepping a thread over a `0xCC` breakpoint requires lifting the trap byte, single-stepping
//! that thread alone while siblings stay stopped, and reinstalling `0xCC` afterward.

use crate::VirtualAddress;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Trap {
    Installed,
    Lifted(Lift),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Lift {
    Repair,

    Vfork,
}

/// `Reported` remembers that the thread stopped at `at` even after the site was removed,
/// so re-adding a breakpoint at `at` before resuming transitions back to `MustStepOver`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum AtTrap {
    Clear,

    Reported { at: VirtualAddress },

    MustStepOver { at: VirtualAddress },

    Repairing { at: VirtualAddress },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TrapEvent {
    Hit(VirtualAddress),

    RepairStarted,

    Stepped,

    RepairHeld,

    RepairInterrupted,

    SiteRemoved(VirtualAddress),

    SiteAdded(VirtualAddress),

    Resumed,

    Moved(VirtualAddress),
}

impl AtTrap {
    pub(super) const fn owed(self) -> Option<VirtualAddress> {
        match self {
            Self::MustStepOver { at } | Self::Repairing { at } => Some(at),
            Self::Clear | Self::Reported { .. } => None,
        }
    }

    pub(super) const fn repairing(self) -> Option<VirtualAddress> {
        match self {
            Self::Repairing { at } => Some(at),
            _ => None,
        }
    }

    pub(super) fn on(self, event: TrapEvent) -> Result<Self, String> {
        use AtTrap::{Clear, MustStepOver, Repairing, Reported};
        use TrapEvent::{
            Hit, Moved, RepairHeld, RepairInterrupted, RepairStarted, Resumed, SiteAdded,
            SiteRemoved, Stepped,
        };

        match (self, event) {
            (Clear | Reported { .. }, Hit(at)) | (Repairing { at }, RepairHeld) => {
                Ok(MustStepOver { at })
            }
            (MustStepOver { at }, RepairStarted) => Ok(Repairing { at }),
            (Repairing { .. }, Stepped | RepairInterrupted) | (Reported { .. }, Resumed) => {
                Ok(Clear)
            }
            (MustStepOver { at }, SiteRemoved(site)) if at == site => Ok(Reported { at }),
            (Reported { at }, SiteAdded(site)) if at == site => Ok(MustStepOver { at }),
            (Reported { at } | MustStepOver { at }, Moved(to)) if at != to => Ok(Clear),
            (Repairing { at }, SiteRemoved(site)) if at == site => {
                Err(format!("{self:?} on {event:?}"))
            }
            (Repairing { .. }, Moved(_))
            | (MustStepOver { .. }, Resumed)
            | (Repairing { .. } | MustStepOver { .. }, Hit(_))
            | (Clear | Reported { .. } | MustStepOver { .. }, Stepped | RepairInterrupted)
            | (Clear | Reported { .. } | Repairing { .. }, RepairStarted)
            | (Clear | Reported { .. } | MustStepOver { .. }, RepairHeld) => {
                Err(format!("{self:?} on {event:?}"))
            }
            (state, SiteRemoved(_) | SiteAdded(_) | Resumed | Moved(_)) => Ok(state),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: VirtualAddress = VirtualAddress::new(0x40);
    const B: VirtualAddress = VirtualAddress::new(0x50);

    #[test]
    fn each_trap_event_moves_each_state_as_written() {
        let states = [
            AtTrap::Clear,
            AtTrap::Reported { at: A },
            AtTrap::MustStepOver { at: A },
            AtTrap::Repairing { at: A },
        ];
        let events = [
            TrapEvent::Hit(A),
            TrapEvent::Hit(B),
            TrapEvent::RepairStarted,
            TrapEvent::Stepped,
            TrapEvent::RepairHeld,
            TrapEvent::RepairInterrupted,
            TrapEvent::SiteRemoved(A),
            TrapEvent::SiteRemoved(B),
            TrapEvent::SiteAdded(A),
            TrapEvent::SiteAdded(B),
            TrapEvent::Resumed,
            TrapEvent::Moved(A),
            TrapEvent::Moved(B),
        ];
        let rows = states
            .iter()
            .flat_map(|&state| {
                events.iter().map(move |&event| {
                    let after = state
                        .on(event)
                        .map_or_else(|_| "refused".to_owned(), |after| format!("{after:?}"));
                    format!("{state:?} / {event:?} -> {after}")
                        .replace("VirtualAddress(0x40)", "0x40")
                        .replace("VirtualAddress(0x50)", "0x50")
                })
            })
            .collect::<Vec<_>>();

        let expected = [
            "Clear / Hit(0x40) -> MustStepOver { at: 0x40 }",
            "Clear / Hit(0x50) -> MustStepOver { at: 0x50 }",
            "Clear / RepairStarted -> refused",
            "Clear / Stepped -> refused",
            "Clear / RepairHeld -> refused",
            "Clear / RepairInterrupted -> refused",
            "Clear / SiteRemoved(0x40) -> Clear",
            "Clear / SiteRemoved(0x50) -> Clear",
            "Clear / SiteAdded(0x40) -> Clear",
            "Clear / SiteAdded(0x50) -> Clear",
            "Clear / Resumed -> Clear",
            "Clear / Moved(0x40) -> Clear",
            "Clear / Moved(0x50) -> Clear",
            "Reported { at: 0x40 } / Hit(0x40) -> MustStepOver { at: 0x40 }",
            "Reported { at: 0x40 } / Hit(0x50) -> MustStepOver { at: 0x50 }",
            "Reported { at: 0x40 } / RepairStarted -> refused",
            "Reported { at: 0x40 } / Stepped -> refused",
            "Reported { at: 0x40 } / RepairHeld -> refused",
            "Reported { at: 0x40 } / RepairInterrupted -> refused",
            "Reported { at: 0x40 } / SiteRemoved(0x40) -> Reported { at: 0x40 }",
            "Reported { at: 0x40 } / SiteRemoved(0x50) -> Reported { at: 0x40 }",
            "Reported { at: 0x40 } / SiteAdded(0x40) -> MustStepOver { at: 0x40 }",
            "Reported { at: 0x40 } / SiteAdded(0x50) -> Reported { at: 0x40 }",
            "Reported { at: 0x40 } / Resumed -> Clear",
            "Reported { at: 0x40 } / Moved(0x40) -> Reported { at: 0x40 }",
            "Reported { at: 0x40 } / Moved(0x50) -> Clear",
            "MustStepOver { at: 0x40 } / Hit(0x40) -> refused",
            "MustStepOver { at: 0x40 } / Hit(0x50) -> refused",
            "MustStepOver { at: 0x40 } / RepairStarted -> Repairing { at: 0x40 }",
            "MustStepOver { at: 0x40 } / Stepped -> refused",
            "MustStepOver { at: 0x40 } / RepairHeld -> refused",
            "MustStepOver { at: 0x40 } / RepairInterrupted -> refused",
            "MustStepOver { at: 0x40 } / SiteRemoved(0x40) -> Reported { at: 0x40 }",
            "MustStepOver { at: 0x40 } / SiteRemoved(0x50) -> MustStepOver { at: 0x40 }",
            "MustStepOver { at: 0x40 } / SiteAdded(0x40) -> MustStepOver { at: 0x40 }",
            "MustStepOver { at: 0x40 } / SiteAdded(0x50) -> MustStepOver { at: 0x40 }",
            "MustStepOver { at: 0x40 } / Resumed -> refused",
            "MustStepOver { at: 0x40 } / Moved(0x40) -> MustStepOver { at: 0x40 }",
            "MustStepOver { at: 0x40 } / Moved(0x50) -> Clear",
            "Repairing { at: 0x40 } / Hit(0x40) -> refused",
            "Repairing { at: 0x40 } / Hit(0x50) -> refused",
            "Repairing { at: 0x40 } / RepairStarted -> refused",
            "Repairing { at: 0x40 } / Stepped -> Clear",
            "Repairing { at: 0x40 } / RepairHeld -> MustStepOver { at: 0x40 }",
            "Repairing { at: 0x40 } / RepairInterrupted -> Clear",
            "Repairing { at: 0x40 } / SiteRemoved(0x40) -> refused",
            "Repairing { at: 0x40 } / SiteRemoved(0x50) -> Repairing { at: 0x40 }",
            "Repairing { at: 0x40 } / SiteAdded(0x40) -> Repairing { at: 0x40 }",
            "Repairing { at: 0x40 } / SiteAdded(0x50) -> Repairing { at: 0x40 }",
            "Repairing { at: 0x40 } / Resumed -> Repairing { at: 0x40 }",
            "Repairing { at: 0x40 } / Moved(0x40) -> refused",
            "Repairing { at: 0x40 } / Moved(0x50) -> refused",
        ];
        assert_eq!(rows, expected);
    }
}
