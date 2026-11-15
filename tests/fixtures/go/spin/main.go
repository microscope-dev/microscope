






package main

import (
	"fmt"
	"runtime"
)


var sink int



//go:noinline
func spin(rounds int) int {
	total := 0
	for round := 0; round < rounds; round++ {
		total += round ^ (total >> 3) // the loop
	}
	return total
}

func main() {
	runtime.LockOSThread()
	go func() {
		for {
			runtime.GC()
		}
	}()
	fmt.Println("spinning")
	for {
		sink += spin(1 << 20)
	}
}
