




package main

import (
	"runtime"
	"strconv"
	"strings"
	"sync"
)


var sink int


func me() int {
	buffer := make([]byte, 64)
	header := string(buffer[:runtime.Stack(buffer, false)])
	id, _, _ := strings.Cut(strings.TrimPrefix(header, "goroutine "), " ")
	number, err := strconv.Atoi(id)
	if err != nil {
		panic(err)
	}
	return number
}




//go:noinline
func deep(depth int) int {
	var pad [512]int
	pad[depth%len(pad)] = depth
	if depth == 0 {
		return pad[0]
	}
	return deep(depth-1) + pad[depth%len(pad)] // recurse
}



//go:noinline
func work(rounds int) int {
	id := me()

	total := deep(32)
	for round := 0; round < rounds; round++ { // WALK: loop
		total += round            // WALK: add
		runtime.Gosched()         // WALK: yield
		total += deep(round % 64) // WALK: deep
		total ^= id               // WALK: mix
	}
	return total + id
}

func main() {
	var group sync.WaitGroup
	for range 8 {
		group.Add(1)
		go func() {
			defer group.Done()
			total := work(10000)
			sink += total // returned
		}()
	}
	group.Wait()
}
