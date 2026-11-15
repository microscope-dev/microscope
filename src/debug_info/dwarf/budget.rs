use super::DwarfError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LoadLimits {
    pub per_input_byte: u64,

    pub floor: u64,

    pub walk_ahead: usize,

    pub most_decompressed: u64,
}

impl Default for LoadLimits {
    fn default() -> Self {
        Self {
            per_input_byte: 32,
            floor: 32 << 20,
            walk_ahead: 64,
            most_decompressed: crate::debug_info::compressed::MOST_PER_FILE,
        }
    }
}

impl LoadLimits {
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

#[derive(Debug, Clone)]
pub struct Meter {
    limit: u64,
    spent: u64,

    exceeded: Option<(&'static str, u64)>,
}

impl Meter {
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

    pub const fn spent(&self) -> u64 {
        self.spent
    }

    pub const fn left(&self) -> u64 {
        self.limit - self.spent
    }

    pub const fn remaining(&self) -> Self {
        Self {
            limit: self.limit - self.spent,
            spent: 0,
            exceeded: self.exceeded,
        }
    }

    pub const fn since(&self, mark: u64) -> Self {
        Self {
            limit: self.limit,
            spent: self.spent - mark,
            exceeded: self.exceeded,
        }
    }

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
