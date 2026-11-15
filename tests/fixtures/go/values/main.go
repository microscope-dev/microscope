















package main

import (
	"fmt"
	"iter"
	"math"
	"os"
	"reflect"
	"runtime"
	"time"
)

var sink any

//go:noinline
func reached(checkpoint string) {
	sink = checkpoint
}

func truth(checkpoint, path, kind string, value any) {
	fmt.Printf("TRUTH\t%s\t%s\t%s\t%v\n", checkpoint, path, kind, value)
}

func bits32(value float32) string { return fmt.Sprintf("%#x", math.Float32bits(value)) }
func bits64(value float64) string { return fmt.Sprintf("%#x", math.Float64bits(value)) }

//go:noinline
func complexes(small complex64, large complex128) complex128 {
	product := large * complex(2, 0)
	truth("complex", "small", "c64", bits32(real(small))+":"+bits32(imag(small)))
	truth("complex", "large", "c128", bits64(real(large))+":"+bits64(imag(large)))
	truth("complex", "product", "c128", bits64(real(product))+":"+bits64(imag(product)))
	truth("complex", "small.imag", "f32", bits32(imag(small)))
	truth("complex", "large.real", "f64", bits64(real(large)))
	truth("complex", "small", "summary", "(1.5-2i)")
	truth("complex", "large", "summary", "(0.1+3e300i)")
	reached("complex")
	runtime.KeepAlive(small)
	runtime.KeepAlive(large)
	return product
}

func functionName(function any) string {
	value := reflect.ValueOf(function)
	if value.IsNil() {
		return "nil"
	}
	return runtime.FuncForPC(value.Pointer()).Name()
}

func double(value int) int { return 2 * value }

type adder struct{ base int }

func (a adder) add(value int) int { return a.base + value }




//go:noinline
func funcs(n int) int {
	var none func(int) int
	named := double
	offset := n * 3
	total := 0
	closure := func(value int) int {
		total += value
		return value + offset + n
	}
	method := adder{base: n}.add
	truth("funcs", "none", "func", functionName(none))
	truth("funcs", "named", "func", functionName(named))
	truth("funcs", "closure", "func", functionName(closure))
	truth("funcs", "closure.offset", "int", offset)
	truth("funcs", "closure.n", "int", n)
	truth("funcs", "closure.total", "int", total)
	truth("funcs", "method", "func", functionName(method))
	reached("funcs")
	runtime.KeepAlive(none)
	return named(1) + closure(2) + method(3) + total
}




//go:noinline
func escapes(start int) *int {
	counter := start
	pointer := &counter
	sink = pointer
	counter += 2
	truth("escape", "counter", "int", counter)
	truth("escape", "counter", "addressable", "")
	truth("escape", "&counter", "absent", "")
	reached("escape")
	return pointer
}

//go:noinline
func results(value int) (sum int, err error) {
	sum = value * 2
	truth("results", "sum", "result", "")
	truth("results", "sum", "int", sum)
	truth("results", "err", "result", "")
	truth("results", "value", "int", value)
	reached("results")
	return sum, nil
}

//go:noinline
func unnamedResults(value int) (int, string) {
	truth("unnamed-results", "~r0", "result", "")
	truth("unnamed-results", "~r1", "result", "")
	reached("unnamed-results")
	return value + 1, "go"
}




//go:noinline
func visibility(n int) int {
	truth("visibility-before", "later", "absent", "")
	reached("visibility-before")
	later := n + 1
	truth("visibility-after", "later", "int", later)
	reached("visibility-after")
	return later
}

func each(values []int) iter.Seq[int] {
	return func(yield func(int) bool) {
		for _, value := range values {
			if !yield(value) {
				return
			}
		}
	}
}




//go:noinline
func temporaries(values []int) int {
	total := 0
	for value := range each(values) {
		total += value
		if value == 2 {
			truth("temporaries", "value", "int", value)
			truth("temporaries", "total", "int", total)
			truth("temporaries", ".closureptr", "hidden", "")
			reached("temporaries")
		}
	}
	return total
}


type Permission uint32

const (
	Read Permission = 1 << iota
	Write
	Execute
)





//go:noinline
func constants() {
	timeout := 1500 * time.Millisecond
	second := time.Second
	both := Read | Write
	stray := Permission(8)
	truth("constants", "timeout", "number", int64(timeout))
	truth("constants", "second", "symbol", "time.Second")
	truth("constants", "both", "symbol", "main.Read|main.Write")
	truth("constants", "stray", "number", uint32(stray))
	reached("constants")
	runtime.KeepAlive(timeout)
	runtime.KeepAlive(second)
	runtime.KeepAlive(both)
	runtime.KeepAlive(stray)
}


type Point struct{ X, Y int }


type Celsius float64

type Other struct{ Name string }

type number interface{ ~int | ~float64 }




//go:noinline
func scale[T number](checkpoint string, value T, factor T) T {
	product := value * factor
	truth(checkpoint, "value", "type", fmt.Sprintf("%T", value))
	truth(checkpoint, "product", "type", fmt.Sprintf("%T", product))
	switch typed := any(product).(type) {
	case int:
		truth(checkpoint, "product", "int", typed)
	case float64:
		truth(checkpoint, "product", "f64", bits64(typed))
	case Celsius:
		truth(checkpoint, "product", "f64", bits64(float64(typed)))
	}
	reached(checkpoint)
	return product
}



//go:noinline
func identity[T any](checkpoint string, value T) T {
	truth(checkpoint, "value", "type", fmt.Sprintf("%T", value))
	reached(checkpoint)
	return value
}




//go:noinline
func pieces(text string, numbers []int, boxed any, pair Point, ratio complex128) int {
	return len(text) + len(numbers) + pair.X + len(fmt.Sprint(boxed)) + int(real(ratio))
}





//go:noinline
func returning(n int) (count int, ok bool, ratio float64, wave complex128, text string, failure error, pair Point) {
	count, ok, ratio, wave, text, pair = n*2, true, 1.5, complex(0.5, -2), "go", Point{X: n, Y: -n}
	const checkpoint = "returned-registers"
	truth(checkpoint, "count", "int", count)
	truth(checkpoint, "ok", "summary", ok)
	truth(checkpoint, "ratio", "f64", bits64(ratio))
	truth(checkpoint, "wave", "c128", bits64(real(wave))+":"+bits64(imag(wave)))
	truth(checkpoint, "text", "string", fmt.Sprintf("%q", text))
	truth(checkpoint, "failure", "summary", "nil")
	truth(checkpoint, "pair.X", "int", pair.X)
	truth(checkpoint, "pair.Y", "int", pair.Y)
	reached(checkpoint)
	return
}





//go:noinline
func returningOnStack(n int) (grid [3]int, label string, a, b, c, d, e, f, g, last int) {
	grid, label = [3]int{n, n + 1, n + 2}, "stack"
	a, b, c, d, e, f, g, last = 1, 2, 3, 4, 5, 6, 7, n*100
	const checkpoint = "returned-stack"
	truth(checkpoint, "grid.0", "int", grid[0])
	truth(checkpoint, "grid.2", "int", grid[2])
	truth(checkpoint, "label", "string", fmt.Sprintf("%q", label))
	truth(checkpoint, "g", "int", g)
	truth(checkpoint, "last", "int", last)
	reached(checkpoint)
	return
}




//go:noinline
func returningDeferred(n int) (total int) {
	defer func() { total *= 10 }()
	truth("returned-deferred", "total", "int", n*10)
	reached("returned-deferred")
	return n
}




//go:noinline
func returningGeneric[T any](value T) T {
	truth("returned-generic", "~r0", "int", value)
	reached("returned-generic")
	return value
}

func main() {
	if complexes(complex(1.5, -2), complex(0.1, 3e300)) == 0 {
		os.Exit(1)
	}
	if funcs(5) == 0 {
		os.Exit(1)
	}
	if *escapes(40) != 42 {
		os.Exit(1)
	}
	results(21)
	unnamedResults(1)
	visibility(9)
	if temporaries([]int{1, 2, 3}) != 6 {
		os.Exit(1)
	}
	constants()
	scale("shape-int", 3, 4)
	scale("shape-float", 2.5, 4.0)
	scale("shape-celsius", Celsius(1.5), 2)
	identity("shape-point", &Point{X: 1, Y: 2})
	identity("shape-other", &Other{Name: "other"})
	text := "pieces"
	numbers := []int{4, 5, 6}
	truth("pieces", "text", "string", fmt.Sprintf("%q", text))
	truth("pieces", "numbers", "len", len(numbers))
	truth("pieces", "numbers.1", "int", numbers[1])
	truth("pieces", "pair.X", "int", 7)
	truth("pieces", "pair.Y", "int", 8)
	truth("pieces", "ratio", "c128", bits64(0.5)+":"+bits64(-1))
	pieces(text, numbers, 42, Point{X: 7, Y: 8}, complex(0.5, -1))
	returning(21)
	returningOnStack(5)
	returningDeferred(4)
	returningGeneric(7)
}
