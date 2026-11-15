






package main

import (
	"fmt"
	"os"
	"os/signal"
	"runtime"
	"strings"
	"syscall"
	"unsafe"
)


var sink int




//go:noinline
func stats() {
	var memory runtime.MemStats
	runtime.ReadMemStats(&memory)
	sink += int(memory.NumGC)
}




//go:noinline
func interrupt() {
	received := make(chan os.Signal, 1)
	signal.Notify(received, syscall.SIGUSR1)
	runtime.LockOSThread()
	syscall.Tgkill(syscall.Getpid(), syscall.Gettid(), syscall.SIGUSR1)
	runtime.UnlockOSThread()
	<-received
	signal.Stop(received)
}




//go:noinline
func fault() (recovered bool) {
	defer func() {
		recovered = recover() != nil
	}()
	var pointer *int
	sink += *pointer // the fault
	return false
}



//go:noinline
func grow(depth int) int {
	var pad [64]int
	pad[depth%64] = depth
	if depth == 0 {
		return pad[0]
	}
	return grow(depth-1) + pad[depth%64]
}



//go:noinline
func below() uintptr {
	local := 7
	return uintptr(unsafe.Pointer(&local))
}



//go:noinline
func hold(slot **int) {
	sink += int(uintptr(unsafe.Pointer(*slot)))
}





//go:noinline
func stale() {
	pointer := (*int)(unsafe.Pointer(below()))
	hold(&pointer)
}



func awaitParked(function, status string) {
	buffer := make([]byte, 1<<16)
	for {
		dump := string(buffer[:runtime.Stack(buffer, true)])
		for _, block := range strings.Split(dump, "\n\n") {
			if strings.Contains(block, "["+status) && strings.Contains(block, function+"(") {
				return
			}
		}
		runtime.Gosched()
	}
}

func main() {
	stats()
	interrupt()
	if !fault() {
		os.Exit(1)
	}
	stale()

	never := make(chan int)
	go func() {
		<-never
	}()
	done := make(chan int)
	go func() {
		done <- grow(2000)
	}()

	sink += <-done
	awaitParked("main.main.func1", "chan receive")
	fmt.Println(sink)
}
