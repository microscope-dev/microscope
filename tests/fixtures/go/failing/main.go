




package main

import (
	"errors"
	"fmt"
	"os"
	"runtime"
	"runtime/debug"
	"sync"
)


var sink int

type celsius float64

type code int

type reading struct {
	value int
}

type named struct{}

func (named) String() string { return "a stringer" }

//go:noinline
func writeNilMap() {
	var counts map[string]int
	counts["a"] = 1 // FAIL: nil map
}

//go:noinline
func dereference(pointer *int) {
	sink += *pointer // FAIL: dereference
}

//go:noinline
func index(values []int, at int) {
	sink += values[at] // FAIL: index
}

//go:noinline
func recurse(depth int) int {
	var pad [64]int
	pad[depth%64] = depth
	return recurse(depth+1) + pad[depth%64]
}

//go:noinline
func raise(value any) {
	panic(value) // FAIL: raise
}

func main() {
	switch os.Args[1] {
	case "nil-map":
		writeNilMap()
	case "nil-dereference":
		dereference(nil)
	case "recovered":
		func() {
			defer func() {
				fmt.Println("recovered:", recover())
			}()
			dereference(nil)
		}()
	case "index":
		index([]int{1, 2, 3}, 5)
	case "wrapped":
		raise(fmt.Errorf("outer: %w", errors.New("inner")))
	case "stringer":
		raise(named{})
	case "float":
		raise(1.5)
	case "custom":
		raise(code(7))
	case "custom-float":
		raise(celsius(-40))
	case "goroutine":


		go func() {
			raise("from a goroutine")
		}()
		select {}
	case "nested":
		defer func() {
			raise("second")
		}()
		raise("first")
	case "repanic":
		defer func() {
			raise(recover())
		}()
		raise("first")
	case "deadlock":
		select {}
	case "goexit":
		runtime.Goexit()
	case "unlock":
		var lock sync.Mutex
		lock.Unlock()
	case "overflow":
		debug.SetMaxStack(1 << 20)
		recurse(0)
	case "breakpoint":
		runtime.Breakpoint()
		fmt.Println("after the breakpoint")
	case "exit":
		defer fmt.Println("never printed")
		os.Exit(3)
	}
	_ = reading{}
}
