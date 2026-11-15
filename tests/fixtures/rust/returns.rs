











use std::hint::black_box;
use std::io::Write;
use std::task::Poll;

#[inline(never)]
fn reached(checkpoint: &str) {
    black_box(checkpoint);
}

fn truth(checkpoint: &str, path: &str, kind: &str, value: impl std::fmt::Display) {
    println!("TRUTH\t{checkpoint}\t{path}\t{kind}\t{value}");
}

fn reach(_checkpoint: &str) {
    std::io::stdout().flush().expect("flush");
}

#[derive(Clone, Copy, PartialEq, Debug)]
#[allow(dead_code)]
enum Level {
    Low,
    High,
}

#[derive(Clone, Copy)]
struct Pair {
    first: i32,
    second: i32,
}

#[derive(Clone, Copy)]
struct Triple {
    a: u8,
    b: u8,
    c: u8,
}

#[inline(never)]
fn r_int(n: i32) -> i32 {
    let value = n * -9;
    truth("returned-int", "r_int", "int", value);
    reach("returned-int");
    reached("returned-int");
    black_box(value)
}

#[inline(never)]
fn r_bool(n: i32) -> bool {
    let value = n > 0;
    truth("returned-bool", "r_bool", "summary", value);
    reach("returned-bool");
    reached("returned-bool");
    black_box(value)
}

#[inline(never)]
fn r_u128(n: i32) -> u128 {
    let value = (n as u128) << 100 | 3;
    truth("returned-u128", "r_u128", "int", value);
    reach("returned-u128");
    reached("returned-u128");
    black_box(value)
}

#[inline(never)]
fn r_f64(n: i32) -> f64 {
    let value = -2.75 * f64::from(n);
    truth("returned-f64", "r_f64", "f64", format!("{:#x}", value.to_bits()));
    reach("returned-f64");
    reached("returned-f64");
    black_box(value)
}

#[inline(never)]
fn r_f32(n: i32) -> f32 {
    let value = 0.5 * n as f32;
    truth("returned-f32", "r_f32", "f32", format!("{:#x}", value.to_bits()));
    reach("returned-f32");
    reached("returned-f32");
    black_box(value)
}

#[inline(never)]
fn r_level(n: i32) -> Level {
    let value = if n > 0 { Level::High } else { Level::Low };
    truth("returned-level", "r_level", "symbol", format!("{value:?}"));
    reach("returned-level");
    reached("returned-level");
    black_box(value)
}

#[inline(never)]
fn r_pair(n: i32) -> Pair {
    let value = Pair { first: n, second: -n };
    truth("returned-pair", "r_pair.first", "int", value.first);
    reach("returned-pair");
    reached("returned-pair");
    black_box(value)
}

#[inline(never)]
fn r_poll(n: i32) -> Poll<u32> {
    let value = Poll::Ready(n.unsigned_abs() + 6);
    truth("returned-poll", "r_poll", "summary", format!("{value:?}"));
    reach("returned-poll");
    reached("returned-poll");
    black_box(value)
}

#[inline(never)]
fn r_pending(n: i32) -> Poll<u32> {
    let value = if n > 0 { Poll::Pending } else { Poll::Ready(0) };
    truth("returned-pending", "r_pending", "summary", format!("{value:?}"));
    reach("returned-pending");
    reached("returned-pending");
    black_box(value)
}

#[inline(never)]
fn r_option(n: i32) -> Option<u64> {
    let value = Some(u64::from(n.unsigned_abs()) + 41);
    truth("returned-option", "r_option", "summary", format!("{value:?}"));
    reach("returned-option");
    reached("returned-option");
    black_box(value)
}

#[inline(never)]
fn r_floats(n: i32) -> (f32, f32) {
    let value = (n as f32 + 0.5, -(n as f32));
    truth("returned-floats", "r_floats.__0", "f32", format!("{:#x}", value.0.to_bits()));
    truth("returned-floats", "r_floats.__1", "f32", format!("{:#x}", value.1.to_bits()));
    reach("returned-floats");
    reached("returned-floats");
    black_box(value)
}

#[inline(never)]
fn r_triple(n: i32) -> Triple {
    let value = Triple {
        a: n as u8,
        b: 2,
        c: 3,
    };
    truth("returned-triple", "r_triple.a", "int", value.a);
    reach("returned-triple");
    reached("returned-triple");
    black_box(value)
}

#[inline(never)]
fn r_unit(n: i32) {
    truth("returned-unit", "r_unit", "absent", "");
    reach("returned-unit");
    reached("returned-unit");
    black_box(n);
}

fn main() {
    let n = black_box(std::env::args().count() as i32);
    let mut total = i64::from(r_int(n)) + i64::from(r_bool(n));
    total += (r_u128(n) >> 100) as i64 + r_f64(n) as i64 + r_f32(n) as i64;


    let pair = r_pair(n);
    total += (r_level(n) == Level::High) as i64 + i64::from(pair.first + pair.second);
    if let Poll::Ready(ready) = r_poll(n) {
        total += i64::from(ready);
    }
    total += i64::from(r_pending(n).is_pending());
    let (a, b) = r_floats(n);
    total += r_option(n).unwrap_or(0) as i64 + (a + b) as i64;
    let triple = r_triple(n);
    total += i64::from(triple.a + triple.b + triple.c);
    r_unit(n);
    black_box(total);
}
