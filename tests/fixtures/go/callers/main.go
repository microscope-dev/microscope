



package main

import (
	"fmt"
	"runtime"
)

var sink int




func init() {
	runtime.LockOSThread()
}

//go:noinline
func reached(name string) {
	sink += len(name)
}




//go:noinline
func checkpoint(name string) {
	pcs := make([]uintptr, 64)

	frames := runtime.CallersFrames(pcs[:runtime.Callers(2, pcs)])
	for {
		frame, more := frames.Next()
		fmt.Printf("TRUTH\tframe\t%s\t%s\t%d\n", frame.Function, frame.File, frame.Line)
		if !more {
			break
		}
	}
	reached(name)
}

type walker struct {
	depth int
}


func relay(w *walker) {
	checkpoint("descend")
}

//go:noinline
func (w *walker) descend(n int) {
	if n == 0 {
		relay(w)
		return
	}
	w.depth++ // descend
	w.descend(n - 1)
	sink += w.depth
}

func main() {
	run := func() {
		(&walker{}).descend(2)
	}
	run()
	fmt.Println("done", sink)
}
