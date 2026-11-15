


package main

import (
	"fmt"
	"runtime"
	"strings"
	"sync"
)

const parked = 8_193

//go:noinline
func park(started *sync.WaitGroup, forever <-chan struct{}) {
	started.Done()
	<-forever // SCALE: parked
}

//go:noinline
func checkpoint() {}




func allParked() bool {
	buffer := make([]byte, 32<<20)
	for {
		size := runtime.Stack(buffer, true)
		if size < len(buffer) {
			buffer = buffer[:size]
			break
		}
		buffer = make([]byte, 2*len(buffer))
	}
	blocked := 0
	for _, block := range strings.Split(string(buffer), "\n\n") {


		header, frames, _ := strings.Cut(block, "\n")
		if strings.Contains(header, " [chan receive") && strings.HasPrefix(frames, "main.park(") {
			blocked++
		}
	}
	return blocked == parked
}

func main() {
	var started sync.WaitGroup
	forever := make(chan struct{})
	started.Add(parked)
	for range parked {
		go park(&started, forever)
	}
	started.Wait()
	for !allParked() {
		runtime.Gosched()
	}
	fmt.Println("goroutines", runtime.NumGoroutine())
	checkpoint()
}
