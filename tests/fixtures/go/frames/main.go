package main

var sink int



//go:noinline
func leaf(x, y int) int {
	sink = x + y
	return sink
}

//go:noinline
func pass(x, y int) int {
	return leaf(1000, 2000) + 1
}




//go:noinline
func held(a, b int) int {
	return pass(a, b)
}



//go:noinline
func spilled(seed int) int {
	return held(seed+1, seed+2) + seed
}

func main() {
	println(spilled(10))
}
