//! What Zig functions return. Before each checkpoint the program prints its
//! own truth, one line per value, its fields separated by tabs,
//!
//!     TRUTH <checkpoint> <path> <kind> <value>
//!
//! and then calls reached(checkpoint). Every checkpoint is named
//! `returned-`: the tests finish the function that reached it and inspect
//! what it returned, which is named for the function. Zig leaves its own
//! calling convention unspecified, so only scalars are known; floats are
//! their bits in hexadecimal.

const std = @import("std");

var sink: usize = 0;

noinline fn reached(checkpoint: []const u8) void {
    sink = checkpoint.len;
    std.mem.doNotOptimizeAway(&sink);
}

fn truth(checkpoint: []const u8, path: []const u8, kind: []const u8, comptime format: []const u8, value: anytype) void {
    var buffer: [256]u8 = undefined;
    const line = std.fmt.bufPrint(&buffer, "TRUTH\t{s}\t{s}\t{s}\t" ++ format ++ "\n", .{ checkpoint, path, kind, value }) catch unreachable;
    _ = std.os.linux.write(1, line.ptr, line.len);
}

const Level = enum(u8) { low, high };

const Pair = struct { first: i32, second: i32 };

noinline fn r_int(n: i32) i32 {
    const value = n * -11;
    truth("returned-int", "r_int", "int", "{d}", value);
    reached("returned-int");
    return value;
}

noinline fn r_bool(n: i32) bool {
    const value = n > 0;
    truth("returned-bool", "r_bool", "summary", "{}", value);
    reached("returned-bool");
    return value;
}

noinline fn r_u64(n: i32) u64 {
    const value = @as(u64, @intCast(n)) << 40 | 9;
    truth("returned-u64", "r_u64", "int", "{d}", value);
    reached("returned-u64");
    return value;
}

noinline fn r_f64(n: i32) f64 {
    const value = -6.5 * @as(f64, @floatFromInt(n));
    truth("returned-f64", "r_f64", "f64", "0x{x}", @as(u64, @bitCast(value)));
    reached("returned-f64");
    return value;
}

noinline fn r_f32(n: i32) f32 {
    const value = 0.75 * @as(f32, @floatFromInt(n));
    truth("returned-f32", "r_f32", "f32", "0x{x}", @as(u32, @bitCast(value)));
    reached("returned-f32");
    return value;
}

noinline fn r_level(n: i32) Level {
    const value: Level = if (n > 0) .high else .low;
    truth("returned-level", "r_level", "symbol", "{s}", @tagName(value));
    reached("returned-level");
    return value;
}

// Zig's LLVM backend says a function returning a struct through memory
// returns void, so it shows nothing returned; the self-hosted backend names
// the struct, which the tests require to be unknown.
noinline fn r_pair(n: i32) Pair {
    const value = Pair{ .first = n, .second = -n };
    truth("returned-pair", "r_pair", "absent", "{s}", "");
    reached("returned-pair");
    return value;
}

noinline fn r_void() void {
    truth("returned-void", "r_void", "absent", "{s}", "");
    reached("returned-void");
    sink += 1;
    std.mem.doNotOptimizeAway(&sink);
}

pub fn main(init: std.process.Init.Minimal) void {
    _ = init;
    r_void();
    var n: i32 = 1;
    std.mem.doNotOptimizeAway(&n);
    var total: i64 = r_int(n);
    total += @intFromBool(r_bool(n));
    total += @intCast(r_u64(n) >> 40);
    total += @intFromFloat(r_f64(n) + r_f32(n));
    total += @intFromEnum(r_level(n));
    total += r_pair(n).second;
    std.mem.doNotOptimizeAway(&total);
}
