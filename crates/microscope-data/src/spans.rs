use std::sync::OnceLock;

pub struct Timer {
    pub open: fn(&'static str) -> bool,
    pub close: fn(),
}

static TIMER: OnceLock<Timer> = OnceLock::new();

pub fn time_with(timer: Timer) {
    let _ = TIMER.set(timer);
}

#[cfg(feature = "self-probes")]
pub type Zone = microscope_probes::Zone;

#[cfg(not(feature = "self-probes"))]
pub type Zone = microscope_probes::Off;

#[must_use = "a span measures until it is dropped"]
pub struct Span {
    open: bool,

    _zone: Zone,
}

#[inline]
pub fn span(name: &'static str, zone: Zone) -> Span {
    Span {
        open: TIMER.get().is_some_and(|timer| (timer.open)(name)),
        _zone: zone,
    }
}

impl Drop for Span {
    #[inline]
    fn drop(&mut self) {
        if self.open
            && let Some(timer) = TIMER.get()
        {
            (timer.close)();
        }
    }
}

#[macro_export]
#[doc(hidden)]
macro_rules! span {
    ($name:literal) => {
        $crate::spans::span($name, ::microscope_probes::zone!($name))
    };
}
