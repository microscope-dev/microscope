const std = @import("std");

const Record = struct {
    name: []const u8,
    samples: []const i32,
    weight: ?f64,
};

const Label = union(enum) {
    count: usize,
    unit: []const u8,
};

pub fn main() !void {
    var storage: [4096]u8 = undefined;
    var fixed = std.heap.FixedBufferAllocator.init(&storage);
    const allocator = fixed.allocator();
    const samples = [_]i32{ 2, 3, 5, 7, 11 };
    var records: std.ArrayList(Record) = .empty;
    defer records.deinit(allocator);
    try records.append(allocator, .{
        .name = "alpha",
        .samples = samples[0..3],
        .weight = 1.5,
    });
    try records.append(allocator, .{
        .name = "beta",
        .samples = samples[3..],
        .weight = null,
    });
    var labels = std.StringHashMap(Label).init(allocator);
    defer labels.deinit();
    try labels.put("count", .{ .count = records.items.len });
    try labels.put("unit", .{ .unit = "samples" });
    std.mem.doNotOptimizeAway(&records);
    std.mem.doNotOptimizeAway(&labels);
}
