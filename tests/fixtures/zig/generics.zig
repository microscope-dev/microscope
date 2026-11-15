

const std = @import("std");

fn Pair(comptime K: type, comptime V: type) type {
    return struct { key: K, val: V };
}

noinline fn genericsTarget(
    list: *const std.ArrayList(u32),
    pair: *const Pair(u32, []const u8),
    text: []const u8,
    terminated: [:0]const u8,
    c_text: [*:0]const u8,
    ints: []const i32,
) usize {
    std.mem.doNotOptimizeAway(list);
    std.mem.doNotOptimizeAway(pair);
    std.mem.doNotOptimizeAway(text);
    std.mem.doNotOptimizeAway(terminated);
    std.mem.doNotOptimizeAway(c_text);
    std.mem.doNotOptimizeAway(ints);
    return list.items.len + pair.val.len + text.len + terminated.len + ints.len; // generics stop here
}

pub fn main() u8 {
    var storage: [1024]u8 = undefined;
    var fixed = std.heap.FixedBufferAllocator.init(&storage);
    const allocator = fixed.allocator();
    var list: std.ArrayList(u32) = .empty;
    list.append(allocator, 7) catch return 2;
    const pair = Pair(u32, []const u8){ .key = 1, .val = "val" };
    const ints = [_]i32{ 1, 2 };
    const total = genericsTarget(&list, &pair, "hello", "zero", "cstr", &ints);
    return @intFromBool(total != 15);
}
