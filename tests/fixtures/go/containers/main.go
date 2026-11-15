



package main

import (
	"errors"
	"fmt"
	"runtime"
)

//go:noinline
func barrier(fixture any) {
	runtime.KeepAlive(fixture)
}


type Counts map[string]int


type Point struct {
	X, Y int
}


type Handle *Point


type Pair struct {
	Left, Right *Point
}

func main() {
	small := map[string]int{"one": 1, "two": 2} // VIEW: small => len=2 {"one": 1, "two": 2} (any order)
	empty := map[int]int{}                      // VIEW: empty => len=0 {}
	var none map[int]int                        // VIEW: none => nil
	counts := Counts{"a": 1}                    // VIEW: counts => len=1 {"a": 1}
	big := map[int]int{}                        // VIEW: big => count: 300
	for index := 0; index < 300; index++ {
		big[index] = index * 10
	}

	huge := map[int]int{} // VIEW: huge => count: 5000
	for index := 0; index < 5000; index++ {
		huge[index] = index
	}



	queue := make(chan int, 4) // VIEW: queue => len=3 [3, 4, 5]
	for _, value := range []int{1, 2, 3} {
		queue <- value
	}
	<-queue
	<-queue
	queue <- 4
	queue <- 5
	words := make(chan string, 2) // VIEW: words => children: [0] = "go", capacity = 2, closed = true, [raw]
	words <- "go"
	close(words)
	unbuffered := make(chan bool) // VIEW: unbuffered => len=0 []
	var no_channel chan int       // VIEW: no_channel => nil

	var number any = 42                         // VIEW: number => int 42
	var text any = "hi"                         // VIEW: text => string "hi"
	var nothing any                             // VIEW: nothing => nil
	var point any = Point{X: 1, Y: 2}           // VIEW: point => main.Point {X: 1, Y: 2}
	var stringer fmt.Stringer = nil             // VIEW: stringer => nil
	var failure error = errors.New("bad")       // VIEW: failure => *errors.errorString *"bad"
	var direct any = Handle(&Point{X: 3, Y: 4}) // VIEW: direct => main.Handle *{X: 3, Y: 4}



	pointer := &Point{X: 5, Y: 6} // VIEW: pointer => *{X: 5, Y: 6}
	// VIEW: pointer => children: X = 5, Y = 6, [raw]
	var no_pointer *Point                   // VIEW: no_pointer => nil
	pair := &Pair{Left: &Point{X: 7, Y: 8}} // VIEW: pair => *{Left: *{X: 7, Y: 8}, Right: nil}
	barrier(&small)
	runtime.KeepAlive(empty)
	runtime.KeepAlive(none)
	runtime.KeepAlive(counts)
	runtime.KeepAlive(big)
	runtime.KeepAlive(huge)
	runtime.KeepAlive(queue)
	runtime.KeepAlive(words)
	runtime.KeepAlive(unbuffered)
	runtime.KeepAlive(no_channel)
	runtime.KeepAlive(number)
	runtime.KeepAlive(text)
	runtime.KeepAlive(nothing)
	runtime.KeepAlive(point)
	runtime.KeepAlive(stringer)
	runtime.KeepAlive(failure)
	runtime.KeepAlive(direct)
	runtime.KeepAlive(pointer)
	runtime.KeepAlive(no_pointer)
	runtime.KeepAlive(pair)
}
