



package main

import "os"

const workers = 4


var sink []byte

//go:noinline
func work(round int) int {
	buffer := make([]byte, 16<<10)
	buffer[round%len(buffer)] = byte(round)
	sink = buffer
	total := 0
	for index := 0; index < 1000000; index++ {
		total += index ^ (total >> 3)
	}
	return total
}

//go:noinline
func unit(round int) int {
	return work(round) + work(round+1)
}

func worker() {
	line := []byte("u\n")
	for round := 0; ; round++ {
		unit(round)

		if _, err := os.Stdout.Write(line); err != nil {
			os.Exit(0)
		}
	}
}

func main() {
	for index := 1; index < workers; index++ {
		go worker()
	}
	worker()
}
