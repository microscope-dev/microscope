use std::fmt;

#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Default,
    serde::Serialize,
    serde::Deserialize,
)]
#[serde(transparent)]
pub struct Pid(u32);

#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Default,
    serde::Serialize,
    serde::Deserialize,
)]
#[serde(transparent)]
pub struct Tid(u32);

macro_rules! os_id {
    ($name:ident) => {
        impl $name {
            #[must_use]
            pub const fn new(id: u32) -> Self {
                Self(id)
            }

            #[must_use]
            pub const fn get(self) -> u32 {
                self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(formatter)
            }
        }
    };
}

os_id!(Pid);
os_id!(Tid);

impl Pid {
    #[must_use]
    pub const fn led_by(leader: Tid) -> Self {
        Self(leader.0)
    }

    #[must_use]
    pub const fn leader(self) -> Tid {
        Tid(self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Architecture {
    X86_64,
    Aarch64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ByteOrder {
    Little,

    Big,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PointerWidth {
    Bits32,
    Bits64,
}

impl PointerWidth {
    #[must_use]
    pub const fn bytes(self) -> u8 {
        match self {
            Self::Bits32 => 4,
            Self::Bits64 => 8,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TargetDescription {
    pub architecture: Architecture,
    pub byte_order: ByteOrder,
    pub pointer_width: PointerWidth,
}

impl TargetDescription {
    pub const X86_64: Self = Self {
        architecture: Architecture::X86_64,
        byte_order: ByteOrder::Little,
        pointer_width: PointerWidth::Bits64,
    };
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum CodeRole {
    #[default]
    Ordinary,

    Wrapper,

    RuntimeInternal,

    Panic,

    StackSwitch,

    Outermost,

    TrapEntry,

    SignalTrampoline,

    Dispatch,

    Interpreter,
}
