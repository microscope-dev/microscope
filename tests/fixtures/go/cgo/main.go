




package main

/*
#include <stdint.h>

extern int64_t callback(int64_t);

// counts is C's own thread-local storage, beside the runtime's.
static __thread int64_t counts[4];

// leaf doubles a value.
static __attribute__((noinline)) int64_t leaf(int64_t value) {
	int64_t doubled = value * 2; // CGO: leaf
	counts[value & 3] += doubled;
	return doubled;
}

// calls calls back into Go.
static __attribute__((noinline)) int64_t calls(int64_t value) {
	int64_t result = callback(value + 1); // CGO: calls
	return result + 1;
}

// fault writes through a null pointer.
static __attribute__((noinline)) void fault(void) {
	volatile int *pointer = 0;
	*pointer = 1; // CGO: fault
}
*/
import "C"

import (
	"fmt"
	"os"
)


var sink int64

func main() {
	sink += int64(C.leaf(20)) // GO: leaf
	sink += int64(C.calls(4)) // GO: calls
	if len(os.Args) > 1 && os.Args[1] == "fault" {
		C.fault() // GO: fault
	}
	fmt.Println(sink)
}
