



package main

//go:noinline
func fill(entries map[int]int, count int) {
	for next := 0; next < count; next++ {
		entries[next] = 7 * next
	}
}

func main() {
	entries := map[int]int{}
	fill(entries, 1024)
}
