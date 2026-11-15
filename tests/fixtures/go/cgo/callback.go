package main

// #include <stdint.h>
import "C"



//export callback
func callback(value C.int64_t) C.int64_t { // GO: entered
	tripled := value * 3 // GO: callback
	return tripled
}
