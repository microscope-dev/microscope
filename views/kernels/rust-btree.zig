















const microscope = @import("microscope_kernel");

const Layout = struct {
    len: u64,
    keys: u64,
    values: u64,
    key_size: u64,
    value_size: u64,
    edges: u64,
    capacity: u64,
};



const Frame = struct {
    node: u64,
    len: u16,
    height: u64,
    index: u16,
};

var stack: [64]Frame = undefined;

export fn run(arguments: [*]const u64, count: i32) i32 {
    if (count != 9) return 1;
    const root = arguments[0];
    const height = arguments[1];
    const layout = Layout{
        .len = arguments[2],
        .keys = arguments[3],
        .values = arguments[4],
        .key_size = arguments[5],
        .value_size = arguments[6],
        .edges = arguments[7],
        .capacity = arguments[8],
    };
    if (root == 0) return 0;
    if (height >= stack.len) return 2;
    var depth: usize = 0;
    descend(&layout, root, height, &depth) catch |failure| return status(failure);
    while (depth > 0) {
        const top = &stack[depth - 1];
        if (top.index == top.len) {
            depth -= 1;
            continue;
        }
        const index: u64 = top.index;
        top.index += 1;
        const item = [2]u64{
            top.node +% layout.keys +% index *% layout.key_size,
            top.node +% layout.values +% index *% layout.value_size,
        };
        if (!microscope.emit(&item)) return 0;
        if (top.height > 0) {
            const edge = microscope.load(u64, top.node +% layout.edges +% (index + 1) *% 8);
            descend(&layout, edge, top.height - 1, &depth) catch |failure| return status(failure);
        }
    }
    return 0;
}


fn descend(layout: *const Layout, node: u64, height: u64, depth: *usize) error{ Overfull, NullEdge }!void {
    var current = node;
    var level = height;
    while (true) {
        if (current == 0) return error.NullEdge;
        const len = microscope.load(u16, current +% layout.len);
        if (len > layout.capacity) return error.Overfull;
        stack[depth.*] = .{ .node = current, .len = len, .height = level, .index = 0 };
        depth.* += 1;
        if (level == 0) return;
        current = microscope.load(u64, current +% layout.edges);
        level -= 1;
    }
}

fn status(failure: error{ Overfull, NullEdge }) i32 {
    return switch (failure) {
        error.Overfull => 3,
        error.NullEdge => 4,
    };
}
