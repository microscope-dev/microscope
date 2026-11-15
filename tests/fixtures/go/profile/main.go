




package main

import (
	"fmt"
	"os"
	"runtime"
	"strconv"
	"sync"
)

const workers = 4


var sink []byte

//go:noinline
func work(round int) int {
	buffer := make([]byte, 64<<10)
	buffer[round%len(buffer)] = byte(round)
	sink = buffer
	total := 0
	for index := 0; index < 20000; index++ {
		total += index ^ (total >> 3)
	}
	return total
}

//go:noinline
func outer(rounds int) int {
	total := 0
	for round := 0; round < rounds; round++ {
		total += work(round)
		if round%16 == 0 {
			runtime.GC()
		}
	}
	return total
}

//go:noinline
func worker(rounds int, group *sync.WaitGroup, results []int, me int) {
	defer group.Done()
	results[me] = outer(rounds)
}

func main() {
	rounds := 5
	if len(os.Args) > 1 {
		rounds, _ = strconv.Atoi(os.Args[1])
	}
	var group sync.WaitGroup
	results := make([]int, workers)
	for me := 0; me < workers; me++ {
		group.Add(1)
		go worker(rounds, &group, results, me)
	}
	group.Wait()
	total := 0
	for _, result := range results {
		total += result
	}
	fmt.Println(total)
}
