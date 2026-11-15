



package main

import (
	"context"
	"runtime"
	"runtime/pprof"
	"strings"
)


func worker(jobs <-chan int) {
	for range jobs {
	}
}

//go:noinline
func explode(reason string) {
	panic(reason)
}

func main() {
	jobs := make(chan int)
	for range 4 {
		go worker(jobs)
	}
	labels := pprof.Labels("job", "resize", "tenant", "a b")
	go pprof.Do(context.Background(), labels, func(context.Context) {
		worker(jobs)
	})

	buffer := make([]byte, 1<<16)
	for strings.Count(string(buffer[:runtime.Stack(buffer, true)]), "[chan receive") < 5 {
		runtime.Gosched()
	}
	explode("the workers are waiting")
}
