// A Go library that a C program hosts, built -buildmode=c-shared. The
// runtime starts as the library loads, before the host's main, and keeps
// each thread's goroutine in the library's thread-local storage, which its
// code finds through a slot the loader fills. The host calls Triple on its
// own thread, which the runtime adopts for the call.
package main

import "C"

// release lets a goroutine of the library's own end, once Triple has run.
var release = make(chan struct{})
var started = make(chan struct{})

func init() {
	go waiting()
	<-started
}

// waiting parks until Triple has run.
func waiting() {
	close(started) // HOSTED: began
	<-release      // HOSTED: waiting
}

// Triple triples a value for the host.
//
//export Triple
func Triple(value C.long) C.long { // HOSTED: entered
	tripled := value * 3
	close(release)
	return tripled
}

func main() {}
