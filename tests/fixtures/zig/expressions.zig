


const std = @import("std");

const Inner = struct {
    s: i16,
    ll: i64,
};

const Fixture = struct {
    small: i8,
    byte: u8,
    count: i32,
    inner: Inner,
    arr: [4]i32,
    slice: []const i32,
    ptr: *const i32,
    flag: bool,
    real: f64,
};

const slice_items = [_]i32{ 7, 8, 9 };

export fn barrier(fixture: *const anyopaque) void {
    std.mem.doNotOptimizeAway(fixture);
}

pub fn main() void {
    var f = Fixture{
        .small = -100,
        .byte = 250,
        .count = -70000,
        .inner = .{ .s = -12, .ll = 123456789012 },
        .arr = .{ 10, 20, 30, 40 },
        .slice = &slice_items,
        .ptr = undefined,
        .flag = true,
        .real = 2.75,
    };
    f.ptr = &f.arr[2];
    std.debug.print("EXPECT\tf.small\tint\t{d}\n", .{f.small});
    std.debug.print("EXPECT\tf.byte + 10\tint\t{d}\n", .{@as(i32, f.byte) + 10});
    std.debug.print("EXPECT\tf.count * 2\tint\t{d}\n", .{@as(i64, f.count) * 2});
    std.debug.print("EXPECT\tf.inner.ll\tint\t{d}\n", .{f.inner.ll});
    std.debug.print("EXPECT\tf.arr[3]\tint\t{d}\n", .{f.arr[3]});
    std.debug.print("EXPECT\tf.slice[1]\tint\t{d}\n", .{f.slice[1]});
    std.debug.print("EXPECT\tlen(f.slice)\tint\t{d}\n", .{f.slice.len});
    std.debug.print("EXPECT\t*f.ptr\tint\t{d}\n", .{f.ptr.*});
    std.debug.print("EXPECT\tf.flag\tbool\t{}\n", .{f.flag});
    std.debug.print("EXPECT\tf.real * 2\tf64\t0x{x}\n", .{@as(u64, @bitCast(f.real * 2))});
    barrier(&f);
    std.mem.doNotOptimizeAway(&f);
}
