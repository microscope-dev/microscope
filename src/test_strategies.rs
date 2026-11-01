//! Strategies the property tests share.
//!
//! proptest draws integers uniformly, so a test of `u64` values almost never
//! sees 0, the maximum, or two equal values. These strategies draw the edges
//! of a range often and the rest of it otherwise. Add a type to `edges!`
//! when a test first needs it.

use proptest::prelude::*;

macro_rules! edges {
    ($($name:ident: $type:ty),* $(,)?) => {$(
        /// Any value, with the edges of the type and small values drawn
        /// about half the time.
        pub fn $name() -> impl Strategy<Value = $type> + Clone {
            prop_oneof![
                2 => prop::sample::select(vec![
                    <$type>::MIN,
                    <$type>::MIN.wrapping_add(1),
                    <$type>::MAX,
                    <$type>::MAX - 1,
                    0,
                    1,
                    <$type>::MAX / 2,
                    (<$type>::MAX / 2).wrapping_add(1),
                ]),
                1 => (0 as $type)..16,
                2 => any::<$type>(),
            ]
        }
    )*};
}

edges! {
    u64_edges: u64,
    i128_edges: i128,
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::strategy::ValueTree;
    use proptest::test_runner::TestRunner;

    #[test]
    fn edges_come_up_often() {
        let mut runner = TestRunner::deterministic();
        let strategy = u64_edges();
        let draws = (0..1000)
            .map(|_| strategy.new_tree(&mut runner).expect("a value").current())
            .collect::<Vec<_>>();

        for edge in [0, 1, u64::MAX, u64::MAX - 1] {
            let seen = draws.iter().filter(|draw| **draw == edge).count();
            assert!(seen >= 20, "{edge:#x} drawn {seen} times of 1000");
        }
    }
}
