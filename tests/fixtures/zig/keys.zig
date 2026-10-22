//! Maps keyed by tuples, which an expression indexes by a tuple key.
//! Prints `EXPECT\t<expression>\t<kind>\t<value>` to standard error for
//! each, as the map's own lookup finds it, then calls barrier().

const std = @import("std");

const Cell = struct { i32, i32 };
const Inner = struct { i64, bool };
const Point = struct { x: i32, y: i32 };

export fn barrier(fixture: *const anyopaque) void {
    std.mem.doNotOptimizeAway(fixture);
}

fn int(expression: []const u8, value: anytype) void {
    std.debug.print("EXPECT\t{s}\tint\t{d}\n", .{ expression, value });
}

fn failure(expression: []const u8, kind: []const u8) void {
    std.debug.print("EXPECT\t{s}\terror\t{s}\n", .{ expression, kind });
}

pub fn main() !void {
    const allocator = std.heap.page_allocator;

    var grid = std.AutoHashMap(Cell, i32).init(allocator);
    try grid.put(.{ 1, 2 }, 12);
    try grid.put(.{ 2, 3 }, 23);
    try grid.put(.{ -1, 7 }, -17);
    var nested = std.AutoHashMap(struct { u8, Inner }, i32).init(allocator);
    try nested.put(.{ 1, .{ 2, true } }, 5);
    try nested.put(.{ 1, .{ 2, false } }, 6);
    var triple = std.AutoHashMap(struct { i8, u16, i64 }, i32).init(allocator);
    try triple.put(.{ 1, 2, 3 }, 6);
    try triple.put(.{ -1, 2, 3 }, 4);
    // A managed map presents the unmanaged one it holds, whose entries
    // are found by any key.
    var ages = std.AutoHashMap(i32, i32).init(allocator);
    try ages.put(1, 10);
    var points = std.AutoHashMap(Point, i32).init(allocator);
    try points.put(.{ .x = 1, .y = 2 }, 3);
    var cell: Cell = .{ 2, 3 };
    var inner: Inner = .{ 2, false };

    int("grid[1, 2]", grid.get(.{ 1, 2 }).?);
    int("grid[(2, 3)]", grid.get(.{ 2, 3 }).?);
    int("grid[-1, 7]", grid.get(.{ -1, 7 }).?);
    int("grid[cell]", grid.get(cell).?);
    int("nested[1, (2, true)]", nested.get(.{ 1, .{ 2, true } }).?);
    int("nested[1, inner]", nested.get(.{ 1, inner }).?);
    int("triple[-1, 2, 3]", triple.get(.{ -1, 2, 3 }).?);
    int("ages[1]", ages.get(1).?);
    if (!grid.contains(.{ 3, 3 })) failure("grid[3, 3]", "missing-key");
    if (!nested.contains(.{ 1, .{ 3, true } })) failure("nested[1, (3, true)]", "missing-key");
    failure("grid[1]", "type");
    failure("grid[1, 2, 3]", "type");
    failure("nested[1, 2, true]", "type");
    failure("points[1, 2]", "type");

    barrier(&grid);
    std.mem.doNotOptimizeAway(.{ &grid, &nested, &triple, &ages, &points, &cell, &inner });
}
