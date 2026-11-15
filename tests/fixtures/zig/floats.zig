



const std = @import("std");

noinline fn floatsTarget(half: f16, quad: f128) void {
    std.mem.doNotOptimizeAway(half);
    std.mem.doNotOptimizeAway(quad);
}

pub fn main() void {
    var one: f128 = 1;
    std.mem.doNotOptimizeAway(&one);
    floatsTarget(1.5, one / 10);
}
