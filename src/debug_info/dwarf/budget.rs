//! What loading one module's debug information may cost: the bytes of the
//! records the loader builds before sealing the image, which are type
//! entries, data objects, and symbolic names. Every one comes from a DIE,
//! so the budget grows with the debug information: information that is
//! sound fits, and information describing far more than its own size fails
//! with [`DwarfError::Budget`] before it exhausts memory. A load that fails
//! so is read again without its locals and parameters, and is left out
//! only when the rest does not fit either.

use super::DwarfError;

/// How a module's budget follows from its debug information.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LoadLimits {
    /// Bytes the loader may build for each byte of uncompressed debug
    /// information.
    pub per_input_byte: u64,

    /// What any module may build, however little debug information it has.
    pub floor: u64,

    /// How many units past those added to the image each worker may walk
    /// to: what is walked waits in memory until the units before it are
    /// added.
    pub walk_ahead: usize,
}

impl Default for LoadLimits {
    /// Programs of the corpus build at most 4.4 bytes of these records
    /// for each byte of their debug information (optimized Go), and
    /// microscope's own debug build 2.1; the smallest build some 40 KB in all.
    /// Thirty-two leaves room for producers that repeat themselves more.
    fn default() -> Self {
        Self {
            per_input_byte: 32,
            floor: 32 << 20,
            // Walking 64 units ahead of the adding takes ThinLTO-built vlpds
            // 0.5 s longer than walking without bound, and its peak holds
            // 0.8 GB less.
            walk_ahead: 64,
        }
    }
}

impl LoadLimits {
    /// The budget of a module with `input` bytes of debug information.
    pub const fn budget(self, input: u64) -> Meter {
        let scaled = input.saturating_mul(self.per_input_byte);
        Meter {
            limit: if scaled > self.floor {
                scaled
            } else {
                self.floor
            },
            spent: 0,
            exceeded: None,
        }
    }
}

/// Spending against one module's budget.
#[derive(Debug, Clone)]
pub struct Meter {
    limit: u64,
    spent: u64,

    /// The first charge the budget refused.
    exceeded: Option<(&'static str, u64)>,
}

impl Meter {
    /// Spends `bytes` on `what`, or refuses when the budget cannot afford
    /// them, which every later [`Self::check`] reports.
    pub fn charge(&mut self, what: &'static str, bytes: usize) -> Result<(), DwarfError> {
        let bytes = u64::try_from(bytes).unwrap_or(u64::MAX);
        let Some(spent) = self
            .spent
            .checked_add(bytes)
            .filter(|spent| *spent <= self.limit)
        else {
            self.exceeded.get_or_insert((what, bytes));
            return self.check();
        };

        self.spent = spent;
        Ok(())
    }

    /// Fails when any charge was refused, naming the first.
    pub const fn check(&self) -> Result<(), DwarfError> {
        match self.exceeded {
            Some((what, requested)) => Err(DwarfError::Budget {
                what,
                limit: self.limit,
                requested,
            }),
            None => Ok(()),
        }
    }

    /// The bytes spent so far.
    pub const fn spent(&self) -> u64 {
        self.spent
    }

    /// What may still be spent.
    pub const fn left(&self) -> u64 {
        self.limit - self.spent
    }

    /// A meter for work done apart, allowed what this one has left.
    pub const fn remaining(&self) -> Self {
        Self {
            limit: self.limit - self.spent,
            spent: 0,
            exceeded: self.exceeded,
        }
    }

    /// What this meter spent since it had spent `mark`, as a meter of its
    /// own that refused what this one has.
    pub const fn since(&self, mark: u64) -> Self {
        Self {
            limit: self.limit,
            spent: self.spent - mark,
            exceeded: self.exceeded,
        }
    }

    /// Charges what `apart`, split from this meter, spent, when it refused
    /// nothing and what it spent is still left here; otherwise nothing.
    pub fn absorb(&mut self, apart: &Self) -> bool {
        let Some(spent) = self
            .spent
            .checked_add(apart.spent)
            .filter(|spent| apart.exceeded.is_none() && *spent <= self.limit)
        else {
            return false;
        };

        self.spent = spent;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A budget spends what it can afford and refuses the rest, reporting
    /// its first refusal from then on, even after a later charge fits.
    #[test]
    fn a_budget_reports_the_first_charge_it_refused() {
        let limits = LoadLimits {
            per_input_byte: 2,
            floor: 10,
            ..LoadLimits::default()
        };

        assert_eq!(limits.budget(3).limit, 10, "the floor");
        let mut meter = limits.budget(8);
        assert_eq!(meter.limit, 16);
        meter.charge("types", 10).unwrap();
        meter.charge("types", 6).unwrap();
        assert_eq!(meter.spent(), 16);

        let refused = |meter: &Meter| match meter.check() {
            Err(DwarfError::Budget {
                what,
                limit,
                requested,
            }) => (what, limit, requested),
            other => panic!("{other:?}"),
        };

        assert!(meter.charge("data objects", 1).is_err());
        assert!(meter.charge("types", usize::MAX).is_err());
        meter.check().unwrap_err();
        assert_eq!(refused(&meter), ("data objects", 16, 1));
        assert_eq!(meter.spent(), 16, "a refused charge spends nothing");
        assert_eq!(
            LoadLimits {
                per_input_byte: u64::MAX,
                floor: 0,
                ..LoadLimits::default()
            }
            .budget(u64::MAX)
            .limit,
            u64::MAX
        );
    }
}
