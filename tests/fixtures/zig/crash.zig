const std = @import("std");

const Record = struct {
    id: i32,
    scale: f64,
};


var crash_target: usize = 8;

noinline fn crashNow(record: *const Record, depth: i32) void {
    const doubled = depth * 2;
    const target: *volatile i32 = @ptrFromInt(crash_target);
    target.* = doubled + record.id;
    std.mem.doNotOptimizeAway(&doubled);
}

pub fn main() void {
    const record = Record{ .id = 42, .scale = 2.5 };
    crashNow(&record, 3);
}
