

package main

import (
	"os"
	"runtime"
)

type Pair[K comparable, V any] struct {
	Key K
	Val V
}

type Counts map[string]int

type Point struct{ X, Y int }

//go:noinline
func genericsTarget(m map[string]int, counts Counts, pair Pair[string, int], numbers []int, ch chan int, text string, point Point) int {
	total := len(m) + len(counts) + pair.Val + len(numbers) + cap(ch) + len(text) + point.X
	return total // generics stop here
}

func main() {
	pair := Pair[string, int]{Key: "a", Val: 1}
	ch := make(chan int, 2)
	if genericsTarget(map[string]int{"one": 1}, Counts{"two": 2}, pair, []int{1, 2, 3}, ch, "go", Point{3, 4}) != 13 {
		os.Exit(1)
	}
	runtime.KeepAlive(&pair)
}
