//! Maps keyed by tuples, which an expression indexes by a tuple key. Prints
//! `EXPECT\t<expression>\t<kind>\t<value>` for each, as the map's own
//! lookup finds it, then calls barrier(). Unlike most Rust fixtures, this
//! one uses std, whose maps it keys.

use std::collections::{BTreeMap, HashMap};
use std::hint::black_box;

#[derive(PartialEq, Eq, Hash)]
struct Point {
    x: i32,
    y: i32,
}

#[derive(PartialEq, Eq, Hash)]
struct Pair(i32, i32);

#[unsafe(no_mangle)]
#[inline(never)]
pub extern "C" fn barrier(fixture: *const u8) {
    black_box(fixture);
}

fn int(expression: &str, value: impl std::fmt::Display) {
    println!("EXPECT\t{expression}\tint\t{value}");
}

fn error(expression: &str, kind: &str) {
    println!("EXPECT\t{expression}\terror\t{kind}");
}

fn main() {
    let grid: HashMap<(i32, i32), i32> = HashMap::from([((1, 2), 12), ((2, 3), 23), ((-1, 7), -17)]);
    let names: BTreeMap<(String, u8), u32> =
        BTreeMap::from([(("ann".to_owned(), 3), 7), (("bob".to_owned(), 1), 9)]);
    let words: HashMap<(&str, i64), u64> = HashMap::from([(("x", 1), 100), (("y", 1), 200)]);
    let nested: HashMap<(u8, (i64, bool)), i32> = HashMap::from([((1, (2, true)), 5), ((1, (2, false)), 6)]);
    let triple: BTreeMap<(i8, u16, i64), i32> = BTreeMap::from([((1, 2, 3), 6), ((-1, 2, 3), 4)]);
    let single: HashMap<(i32,), i32> = HashMap::from([((4,), 40)]);
    let points: HashMap<Point, i32> = HashMap::from([(Point { x: 1, y: 2 }, 3)]);
    let pairs: HashMap<Pair, i32> = HashMap::from([(Pair(1, 2), 3)]);
    let cell = (2, 3);
    let inner = (2_i64, false);

    int("grid[1, 2]", grid[&(1, 2)]);
    int("grid[(2, 3)]", grid[&(2, 3)]);
    int("grid[-1, 7]", grid[&(-1, 7)]);
    int("grid[cell]", grid[&cell]);
    int("grid[1, 1 + 1] * 2", grid[&(1, 2)] * 2);
    int("grid[1.0, 2]", grid[&(1, 2)]);
    int("names[\"ann\", 3]", names[&("ann".to_owned(), 3)]);
    int("names[(\"bob\", 1)]", names[&("bob".to_owned(), 1)]);
    int("words[\"y\", 1]", words[&("y", 1)]);
    int("nested[1, (2, true)]", nested[&(1, (2, true))]);
    int("nested[1, inner]", nested[&(1, inner)]);
    int("triple[1, 2, 3]", triple[&(1, 2, 3)]);
    int("triple[-1, 2, 3]", triple[&(-1, 2, 3)]);
    int("single[4,]", single[&(4,)]);
    int("single[(4,)]", single[&(4,)]);
    assert!(!grid.contains_key(&(3, 3)));
    error("grid[3, 3]", "missing-key");
    assert!(!names.contains_key(&("ann".to_owned(), 4)));
    error("names[\"ann\", 4]", "missing-key");
    assert!(!nested.contains_key(&(1, (3, true))));
    error("nested[1, (3, true)]", "missing-key");
    error("grid[1]", "type");
    error("grid[1, 2, 3]", "type");
    error("grid[(1, \"2\")]", "type");
    error("nested[1, 2, true]", "type");
    error("single[4]", "type");
    error("points[1, 2]", "type");
    error("pairs[1, 2]", "type");

    barrier((&raw const grid).cast());
    black_box((&grid, &names, &words, &nested, &triple, &single, &points, &pairs, &cell, &inner));
}
