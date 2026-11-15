





package main

import (
	"fmt"
	"os"
	"runtime"
	"sync"
	"sync/atomic"
	"syscall"
)

const (
	workers = 8
	rounds  = 200
)


var sink []byte




//go:noinline
func deep(depth int) int {
	var pad [256]int
	pad[depth%len(pad)] = depth
	if depth == 0 {
		return pad[0]
	}
	return deep(depth-1) + pad[depth%len(pad)]
}



//go:noinline
func step(id, round int, total *int) {
	*total += deep(round % 32) // TORTURE: step
	*total ^= id               // TORTURE: mix
}

//go:noinline
func work(id int) int {
	total := 0
	for round := 0; round < rounds; round++ {
		step(id, round, &total) // TORTURE: call
		runtime.Gosched()
		if round%5 == 0 {
			syscall.Kill(os.Getpid(), syscall.SIGURG)
		}
	}
	return total
}


func expected(id int) int {
	total := 0
	for round := 0; round < rounds; round++ {
		depth := round % 32
		total += depth * (depth + 1) / 2
		total ^= id
	}
	return total
}

func main() {
	var stop atomic.Bool
	collected := make(chan struct{})
	go func() {
		defer close(collected)
		for !stop.Load() {
			sink = make([]byte, 64<<10)
			runtime.GC()
		}
	}()

	var group sync.WaitGroup
	totals := make([]int, workers)
	for id := range workers {
		group.Add(1)
		go func() {
			defer group.Done()
			totals[id] = work(id)
		}()
	}
	group.Wait()
	stop.Store(true)
	<-collected
	for id, total := range totals {
		if total != expected(id) {
			fmt.Printf("worker %d computed %d, not %d\n", id, total, expected(id))
			os.Exit(1)
		}
	}
	fmt.Println("every worker's work is right")
}
