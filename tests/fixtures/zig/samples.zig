


const std = @import("std");

const Ring = struct {
    samples: [256]f32 = [_]f32{0} ** 256,
    next: usize = 0,

    fn push(self: *Ring, sample: f32) void {
        self.samples[self.next] = sample;
        self.next = (self.next + 1) % self.samples.len;
    }
};

fn wave(tick: usize) f32 {
    const t: f32 = @floatFromInt(tick);
    return 10.0 * @sin(t / 9.0) + @as(f32, @floatFromInt(tick % 7));
}

fn report(tick: usize, window: []const f32) void {
    std.debug.print("tick {d}: {d} samples\n", .{ tick, window.len });
}

pub fn main() void {
    var ring = Ring{};
    var tick: usize = 0;
    while (tick < 300) : (tick += 1) {
        ring.push(wave(tick));
        report(tick, ring.samples[0..@min(tick + 1, ring.samples.len)]);
    }
}
