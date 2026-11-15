



package containers

import "base:intrinsics"
import "core:container/queue"
import "core:container/small_array"
import "core:strings"

sink: rawptr

barrier :: #force_no_inline proc(fixture: rawptr) {
	intrinsics.volatile_store(&sink, fixture)
}


Big :: struct {
	words: [9]u64,
}


Triple :: struct {
	a, b, c: u64,
}

main :: proc() {
	counts := make(map[string]int) // VIEW: counts => len=2 {"one": 1, "two": 2} (any order)
	defer delete(counts)
	counts["one"] = 1
	counts["two"] = 2
	none: map[int]f64 // VIEW: none => len=0 {}
	many := make(map[u32]u32) // VIEW: many => count: 300
	defer delete(many)
	for i in 0 ..< 300 {
		many[u32(i)] = u32(i * 2)
	}

	pruned := make(map[int]int) // VIEW: pruned => len=1 {2: 20}
	defer delete(pruned)
	pruned[1] = 10
	pruned[2] = 20
	delete_key(&pruned, 1)
	triples := make(map[i64]Triple) // VIEW: triples => count: 20
	defer delete(triples)
	for i in 0 ..< 20 {
		triples[i64(i)] = {u64(i), 0, 0}
	}

	bigs := make(map[Big]u8) // VIEW: bigs => count: 3
	defer delete(bigs)
	for i in 0 ..< 3 {
		bigs[Big{words = {0 = u64(i)}}] = u8(i)
	}


	ring: queue.Queue(int) // VIEW: ring => len=3 [7, 8, 9]
	queue.init(&ring, 4)
	defer queue.destroy(&ring)
	for value in 5 ..= 7 {
		queue.push_back(&ring, value)
	}
	queue.pop_front(&ring)
	queue.pop_front(&ring)
	queue.push_back(&ring, 8)
	queue.push_back(&ring, 9)

	broken: queue.Queue(int) // VIEW: broken => problem: check
	broken.len = 5

	small: small_array.Small_Array(4, i32) // VIEW: small => len=2 [5, 6]
	small_array.push_back(&small, 5)
	small_array.push_back(&small, 6)

	built := strings.builder_make() // VIEW: built => "built"
	defer strings.builder_destroy(&built)
	strings.write_string(&built, "built")

	barrier(&counts)
	barrier(&none)
	barrier(&many)
	barrier(&pruned)
	barrier(&triples)
	barrier(&bigs)
	barrier(&ring)
	barrier(&broken)
	barrier(&small)
	barrier(&built)
}
