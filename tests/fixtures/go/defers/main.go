





package main


var sink int

//go:noinline
func cleanup(note int) { // DEFER: cleanup
	sink += note // DEFER: cleanup body
} // DEFER: cleanup end



//go:noinline
func direct() int {
	defer cleanup(1)
	sink++      // DEFER: direct body
	return sink // DEFER: direct return
} // DEFER: direct end



//go:noinline
func looped(count int) int {
	for note := range count {
		defer cleanup(note)
	}
	return sink // DEFER: looped return
} // DEFER: looped end

//go:noinline
func rescue() (caught bool) {
	defer func() { // DEFER: rescuer
		caught = recover() != nil // DEFER: recover
	}() // DEFER: rescuer end
	explode() // DEFER: explode call
	return false
}

//go:noinline
func explode() {
	panic("boom") // DEFER: panic
}

func main() {
	direct()  // DEFER: direct call
	looped(2) // DEFER: looped call
	rescue()  // DEFER: rescue call
	sink++    // DEFER: done
}
