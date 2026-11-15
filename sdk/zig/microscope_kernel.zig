






















//! SPDX-License-Identifier: MIT OR Apache-2.0




pub extern "microscope_kernel_v1" fn read(
    address: u64,
    buffer: [*]u8,
    length: i32,
) i32;



pub extern "microscope_kernel_v1" fn yield(words: [*]const u64, count: i32) i32;


pub fn load(comptime T: type, address: u64) T {
    var value: T = undefined;
    _ = read(address, @ptrCast(&value), @sizeOf(T));
    return value;
}


pub fn emit(words: []const u64) bool {
    return yield(words.ptr, @intCast(words.len)) != 0;
}
