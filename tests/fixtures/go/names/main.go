




package main

import (
	"encoding/hex"
	"fmt"
	"math/rand/v2"
	"os"
)

type Celsius float64

//go:noinline
func (c Celsius) Fahrenheit() float64 { // names: Fahrenheit begins
	return float64(c)*9/5 + 32 // names: Fahrenheit ends
}

type Counter struct{ n int }

//go:noinline
func (c *Counter) Add(by int) { // names: Add begins
	c.n += by // names: Add ends
}

type Stack[T any] struct{ items []T }

//go:noinline
func (s *Stack[T]) Push(v T) { // names: Push begins
	s.items = append(s.items, v) // names: Push ends
}



var hook func(int) int

//go:noinline
func apply(f func(int) int, x int) int {
	return f(x)
}



//go:noinline
func grow(seed int) int {
	var frame [64 << 10]byte
	frame[seed%len(frame)] = byte(seed)
	return int(frame[(seed+1)%len(frame)]) + seed
}

func main() {
	seed := len(os.Args)
	var counter Counter
	counter.Add(seed)
	hook = func(x int) int { // names: closure begins
		return x + counter.n // names: closure ends
	}
	var ints Stack[int]
	ints.Push(seed)
	var words Stack[string]
	words.Push("go")
	total := Sum([]int{seed, 2}) + int(Sum([]float64{1.5, float64(seed)}))
	total += apply(hook, seed) // names: before blank
	// names: no statement
	total += rand.IntN(2) // names: after blank
	done := make(chan int)
	go func() { done <- grow(seed) }()
	total += <-done
	fmt.Println(Celsius(total).Fahrenheit(), hex.Dump([]byte{byte(total)}), len(ints.items)+len(words.items))
}
