package main

type Number interface{ ~int | ~float64 }



//go:noinline
func Sum[T Number](values []T) T { // names: Sum begins
	var total T
	for _, value := range values {
		total += value
	}
	return total // names: Sum ends
}
