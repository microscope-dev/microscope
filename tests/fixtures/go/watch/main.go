package main

import (
	"runtime"
	"sync"
	"sync/atomic"
)

const (
	goroutines = 4
	increments = 5
)

var watchedCounter int64

//go:noinline
func watchReady() {
	atomic.StoreInt64(&watchedCounter, 0)
}

//go:noinline
func stackLocal() int64 {
	local := int64(5)
	local += 1
	runtime.KeepAlive(&local)
	return local
}

func main() {
	watchReady()
	var group sync.WaitGroup
	for range goroutines {
		group.Add(1)
		go func() {


			runtime.LockOSThread()
			for range increments {
				atomic.AddInt64(&watchedCounter, 1)
			}
			group.Done()
		}()
	}
	group.Wait()
	if stackLocal() != 6 || atomic.LoadInt64(&watchedCounter) != goroutines*increments {
		panic("unexpected watch fixture state")
	}
}
