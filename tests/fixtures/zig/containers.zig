





const std = @import("std");

noinline fn barrier(fixture: *const anyopaque) void {
    std.mem.doNotOptimizeAway(fixture);
}

const Shape = union(enum) { circle: u32, square: struct { side: u16 }, none };
const Failure = error{ Oops, Bad };

fn lessThan(context: void, a: u32, b: u32) std.math.Order {
    _ = context;
    return std.math.order(a, b);
}

noinline fn fallible(fail: bool) Failure!u32 {
    if (fail) return Failure.Bad;
    return 7;
}

pub fn main() !void {
    const allocator = std.heap.page_allocator;
    var ints: std.ArrayList(i32) = .empty; // VIEW: ints => len=3 [1, 2, 3]
    defer ints.deinit(allocator);
    try ints.appendSlice(allocator, &.{ 1, 2, 3 });
    var no_ints: std.ArrayList(u64) = .empty; // VIEW: no_ints => len=0 []
    var bytes = std.array_list.Managed(u8).init(allocator); // VIEW: bytes => "hi"
    defer bytes.deinit();
    try bytes.appendSlice("hi");
    var many: std.ArrayList(u32) = .empty; // VIEW: many => len=300 [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, …]
    defer many.deinit(allocator);
    for (0..300) |index| {
        try many.append(allocator, @intCast(index));
    }
    var hashed = std.AutoHashMap(i32, i32).init(allocator); // VIEW: hashed => len=2 {1: 10, 2: 20} (any order)
    defer hashed.deinit();
    try hashed.put(1, 10);
    try hashed.put(2, 20);
    var unmanaged: std.AutoHashMapUnmanaged(u32, u64) = .empty; // VIEW: unmanaged => len=1 {3: 30}
    defer unmanaged.deinit(allocator);
    try unmanaged.put(allocator, 3, 30);
    var no_hashed: std.AutoHashMapUnmanaged(u32, u64) = .empty; // VIEW: no_hashed => len=0 {}
    var named = std.StringHashMap(i32).init(allocator); // VIEW: named => len=1 {"one": 1}
    defer named.deinit();
    try named.put("one", 1);
    var many_hashed = std.AutoHashMap(u32, u32).init(allocator); // VIEW: many_hashed => count: 300
    defer many_hashed.deinit();
    for (0..300) |index| {
        try many_hashed.put(@intCast(index), @intCast(index * 2));
    }
    var ordered: std.AutoArrayHashMapUnmanaged(i32, i32) = .empty; // VIEW: ordered => len=2 {5: 50, 6: 60}
    defer ordered.deinit(allocator);
    try ordered.put(allocator, 5, 50);
    try ordered.put(allocator, 6, 60);
    var strings: std.StringArrayHashMapUnmanaged(i32) = .empty; // VIEW: strings => len=1 {"k": 1}
    defer strings.deinit(allocator);
    try strings.put(allocator, "k", 1);
    var no_ordered: std.AutoArrayHashMapUnmanaged(i32, i32) = .empty; // VIEW: no_ordered => len=0 {}

    var past_capacity: std.ArrayList(i32) = .empty; // VIEW: past_capacity => problem: check
    const storage = [_]i32{ 10, 11, 12, 13 };
    past_capacity.items = @constCast(storage[0..4]);
    past_capacity.capacity = 2;


    var text: std.ArrayList(u8) = .empty; // VIEW: text => "hello"
    defer text.deinit(allocator);
    try text.appendSlice(allocator, "hello");
    var binary: std.ArrayList(u8) = .empty; // VIEW: binary => len=2 [255, 0]
    defer binary.deinit(allocator);
    try binary.appendSlice(allocator, &.{ 255, 0 });
    var managed_ints = std.array_list.Managed(i16).init(allocator); // VIEW: managed_ints => len=2 [7, 8]
    defer managed_ints.deinit();
    try managed_ints.appendSlice(&.{ 7, 8 });
    var buffer = [_]u8{ 'o', 'k' };
    var byte_slice: []u8 = &buffer; // VIEW: byte_slice => "ok"
    var unmanaged_bits: std.DynamicBitSetUnmanaged = .{}; // VIEW: unmanaged_bits => len=0 []
    var deque: std.Deque(i32) = .empty; // VIEW: deque => len=3 [0, 1, 2]
    defer deque.deinit(allocator);
    try deque.pushBack(allocator, 1);
    try deque.pushBack(allocator, 2);
    try deque.pushFront(allocator, 0);
    var queue: std.PriorityQueue(u32, void, lessThan) = .empty; // VIEW: queue => len=3 [1, 5, 3]
    defer queue.deinit(allocator);
    try queue.push(allocator, 5);
    try queue.push(allocator, 3);
    try queue.push(allocator, 1);
    var set = std.BufSet.init(allocator); // VIEW: set => len=1 ["one"]
    defer set.deinit();
    try set.insert("one");
    var keys: std.AutoHashMapUnmanaged(u16, void) = .empty; // VIEW: keys => len=2 [4, 9] (any order)
    defer keys.deinit(allocator);
    try keys.put(allocator, 4, {});
    try keys.put(allocator, 9, {});
    var bits = std.StaticBitSet(10).initEmpty(); // VIEW: bits => len=2 [1, 8]
    bits.set(1);
    bits.set(8);
    var wide_bits = std.StaticBitSet(100).initEmpty(); // VIEW: wide_bits => len=2 [3, 99]
    wide_bits.set(3);
    wide_bits.set(99);
    var dynamic_bits: std.DynamicBitSet = undefined; // VIEW: dynamic_bits => len=2 [0, 69]
    dynamic_bits = try std.DynamicBitSet.initEmpty(allocator, 70);
    defer dynamic_bits.deinit();
    dynamic_bits.set(0);
    dynamic_bits.set(69);
    var counter = std.atomic.Value(u32).init(5); // VIEW: counter => 5
    var written: std.Io.Writer.Allocating = .init(allocator); // VIEW: written => "out"
    defer written.deinit();
    try written.writer.writeAll("out");



    var some: ?u32 = 5; // VIEW: some => 5
    var none: ?u32 = null; // VIEW: none => null
    var value: u64 = 9;

    var some_pointer: ?*u64 = &value; // VIEW: some_pointer => stored
    var no_pointer: ?*u64 = null; // VIEW: no_pointer => stored
    var success: Failure!u32 = fallible(false); // VIEW: success => 7
    var failure: Failure!u32 = fallible(true); // VIEW: failure => error.Bad
    var circle: Shape = .{ .circle = 3 }; // VIEW: circle => circle(3)
    var square: Shape = .{ .square = .{ .side = 4 } }; // VIEW: square => square {side: 4}
    var nothing: Shape = .none; // VIEW: nothing => none

    std.mem.doNotOptimizeAway(&ints);
    std.mem.doNotOptimizeAway(&no_ints);
    std.mem.doNotOptimizeAway(&bytes);
    std.mem.doNotOptimizeAway(&many);
    std.mem.doNotOptimizeAway(&past_capacity);
    std.mem.doNotOptimizeAway(&hashed);
    std.mem.doNotOptimizeAway(&unmanaged);
    std.mem.doNotOptimizeAway(&no_hashed);
    std.mem.doNotOptimizeAway(&named);
    std.mem.doNotOptimizeAway(&many_hashed);
    std.mem.doNotOptimizeAway(&ordered);
    std.mem.doNotOptimizeAway(&strings);
    std.mem.doNotOptimizeAway(&no_ordered);
    std.mem.doNotOptimizeAway(&text);
    std.mem.doNotOptimizeAway(&binary);
    std.mem.doNotOptimizeAway(&managed_ints);
    std.mem.doNotOptimizeAway(&byte_slice);
    std.mem.doNotOptimizeAway(&unmanaged_bits);
    std.mem.doNotOptimizeAway(&deque);
    std.mem.doNotOptimizeAway(&queue);
    std.mem.doNotOptimizeAway(&set);
    std.mem.doNotOptimizeAway(&keys);
    std.mem.doNotOptimizeAway(&bits);
    std.mem.doNotOptimizeAway(&wide_bits);
    std.mem.doNotOptimizeAway(&dynamic_bits);
    std.mem.doNotOptimizeAway(&counter);
    std.mem.doNotOptimizeAway(&written);
    std.mem.doNotOptimizeAway(&some);
    std.mem.doNotOptimizeAway(&none);
    std.mem.doNotOptimizeAway(&some_pointer);
    std.mem.doNotOptimizeAway(&no_pointer);
    std.mem.doNotOptimizeAway(&success);
    std.mem.doNotOptimizeAway(&failure);
    std.mem.doNotOptimizeAway(&circle);
    std.mem.doNotOptimizeAway(&square);
    std.mem.doNotOptimizeAway(&nothing);
    barrier(&ints);
}
