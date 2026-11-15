



package main

import "sync"


var sink int




//go:noinline
func deep(depth int) int {
	var pad [256]int
	pad[depth%len(pad)] = depth
	if depth == 0 {
		return pad[0]
	}
	return deep(depth-1) + pad[depth%len(pad)]
}




//go:noinline
func bump(counter *int, by int) {
	*counter += by // WATCH: store
}

//go:noinline
func watched(rounds int) int {
	counter := 0
	for round := 1; round <= rounds; round++ { // WATCH: loop
		sink += deep(round * 16)
		bump(&counter, round)
	}
	return counter
}



//go:noinline
func finished() {
	sink++
}

func main() {
	var group sync.WaitGroup
	for range 4 {
		group.Add(1)
		go func() {
			defer group.Done()
			sink += watched(4)
		}()
	}
	group.Wait()
	finished()
}
