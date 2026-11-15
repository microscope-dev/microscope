





package main

import (
	"fmt"
	"iter"
)



//go:noinline
func Count(n int) iter.Seq[int] {
	return func(yield func(int) bool) {
		for i := 0; i < n; i++ {
			if !yield(i) {
				return
			}
		}
	}
}


func Evens(n int) iter.Seq[int] {
	return func(yield func(int) bool) {
		for i := 0; i < n; i += 2 {
			if !yield(i) {
				return
			}
		}
	}
}


var sink int

//go:noinline
func counted() int {
	total := 0
	scale := 3
	for v := range Count(5) { // WALK: counted loop
		total += v * scale // WALK: counted add
		if v == 3 {        // WALK: counted check
			break // WALK: counted break
		}
	} // WALK: counted end
	return total // WALK: counted after
}

//go:noinline
func evens() int {
	total := 0
	for v := range Evens(6) { // WALK: evens loop
		total += v // WALK: evens add
	} // WALK: evens end
	sink = total // WALK: evens after
	return total
}

func main() {
	fmt.Println(counted(), evens())
}
