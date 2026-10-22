// Prints `EXPECT\t<expression>\t<kind>\t<value>` for each expression the
// debugger must agree with, then calls barrier(). A kind of `text` is the
// text a string holds, `range` the elements a slice of an array or slice
// holds, and `error` the kind of error the expression must be.
package main

import (
	"fmt"
	"math"
	"runtime"
	"strings"
)

type inner struct {
	s  int16
	ll int64
}

// Base and Side are embedded in node at depth one, and deep through an
// embedded pointer at depth two: node's W is Base's, which hides deep's,
// and its shared is both Base's and Side's, which Go refuses to choose.
type Base struct {
	W      int
	shared int
}

type Side struct {
	shared int
	Q      int
}

type deep struct {
	Z int
	W int
}

type Middle struct {
	deep
	M int
}

type node struct {
	Base
	*Middle
	Side
	own int
}

type fixture struct {
	small   int8
	byte_   uint8
	count   int
	inner   inner
	arr     [4]int32
	slice   []int32
	room    []int32
	text    string
	essay   string
	who     string
	ptr     *int32
	nothing *int32
	flag    bool
	real    float64
	node    node
	ages    map[string]int
	squares map[int]int
	spots   map[inner]int
	queue   chan int
}

//go:noinline
func barrier(f *fixture) {
	runtime.KeepAlive(f)
}

func expect(expression, kind string, value any) {
	fmt.Printf("EXPECT\t%s\t%s\t%v\n", expression, kind, value)
}

func main() {
	f := &fixture{
		small: -100,
		byte_: 250,
		count: -70000,
		inner: inner{s: -12, ll: 123456789012},
		arr:   [4]int32{10, 20, 30, 40},
		slice: []int32{7, 8, 9},
		room:  make([]int32, 3, 10),
		text:  "hello",
		essay: strings.Repeat("abcdefghij", 100),
		who:   "ann",
		flag:  true,
		real:  2.75,
		node: node{
			Base:   Base{W: 5, shared: 1},
			Middle: &Middle{deep: deep{Z: 7, W: 9}, M: 8},
			Side:   Side{shared: 2, Q: 3},
			own:    4,
		},
		ages:    map[string]int{"ann": 30, "bob": 41},
		squares: map[int]int{},
		spots:   map[inner]int{{1, 2}: 7},
		queue:   make(chan int, 4),
	}
	// Enough entries for several groups; a key is found by searching
	// them. The hash seed differs on every run, and with it where each
	// key lies, so the map is small enough that two searches of every
	// group fit an inspection's memory reads whatever the order.
	for index := 1; index <= 40; index++ {
		f.squares[index] = index * index
	}
	f.queue <- 1
	f.ptr = &f.arr[2]
	expect("f.small", "int", f.small)
	expect("f.byte_ + 10", "int", int(f.byte_)+10)
	expect("f.count * 2", "int", f.count*2)
	expect("f.inner.ll", "int", f.inner.ll)
	expect("f.arr[3]", "int", f.arr[3])
	expect("f.slice[1]", "int", f.slice[1])
	expect("len(f.slice)", "int", len(f.slice))
	expect(`f.text == "hello"`, "bool", f.text == "hello")
	expect("len(f.text)", "int", len(f.text))
	expect("*f.ptr", "int", *f.ptr)
	expect("f.flag", "bool", f.flag)
	expect("f.real * 2", "f64", fmt.Sprintf("%#x", math.Float64bits(f.real*2)))

	// nil is null.
	expect("f.ptr != nil", "bool", f.ptr != nil)
	expect("f.nothing == nil", "bool", f.nothing == nil)

	// Capacities.
	expect("cap(f.arr)", "int", cap(f.arr))
	expect("cap(f.slice)", "int", cap(f.slice))
	expect("cap(f.room)", "int", cap(f.room))
	expect("len(f.room)", "int", len(f.room))
	expect("cap(f.queue)", "int", cap(f.queue))
	expect("len(f.queue)", "int", len(f.queue))

	// Promoted fields.
	expect("f.node.W", "int", f.node.W)
	expect("f.node.Q", "int", f.node.Q)
	expect("f.node.M", "int", f.node.M)
	expect("f.node.Z", "int", f.node.Z)
	expect("f.node.Middle.W", "int", f.node.Middle.W)
	expect("f.node.own + f.node.Base.shared", "int", f.node.own+f.node.Base.shared)
	expect("f.node.shared", "error", "ambiguous-name")

	// Slices of arrays, slices, and strings.
	expect("f.arr[1:3]", "range", rangeText(f.arr[1:3]))
	expect("f.slice[1:]", "range", rangeText(f.slice[1:]))
	expect("f.slice[:2]", "range", rangeText(f.slice[:2]))
	expect("f.text[1:3]", "text", f.text[1:3])
	expect(`f.text[1:] == "ello"`, "bool", f.text[1:] == "ello")
	expect("len(f.text[2:])", "int", len(f.text[2:]))
	expect("f.essay[995:]", "text", f.essay[995:])
	expect("f.essay[500:503]", "text", f.essay[500:503])
	if 9 > len(f.text) {
		expect("f.text[2:9]", "error", "bounds")
	}
	if 4 > len(f.slice) {
		expect("f.slice[1:4]", "error", "bounds")
	}

	// Maps by key.
	expect(`f.ages["bob"]`, "int", f.ages["bob"])
	expect("f.ages[f.who]", "int", f.ages[f.who])
	expect(`f.ages[f.essay[1:2]]`, "error", missing(f.ages, f.essay[1:2]))
	if _, held := f.ages["zed"]; !held {
		expect(`f.ages["zed"]`, "error", "missing-key")
	}
	expect("f.squares[3]", "int", f.squares[3])
	expect("f.squares[40] + f.squares[1]", "int", f.squares[40]+f.squares[1])
	expect("f.squares[2.0]", "int", f.squares[int(2.0)])
	expect(`f.squares["3"]`, "error", "type")
	// A struct is no tuple, so a tuple key never finds a struct's entry.
	expect("f.spots[1, 2]", "error", "type")
	if _, held := f.squares[0]; !held {
		expect("f.squares[0]", "error", "missing-key")
	}
	barrier(f)
	runtime.KeepAlive(f)
}

// The elements of a slice, as the harness writes a range.
func rangeText(elements []int32) string {
	parts := make([]string, len(elements))
	for index, element := range elements {
		parts[index] = fmt.Sprint(element)
	}
	return strings.Join(parts, ",")
}

// The error a key the map does not hold must be.
func missing(m map[string]int, key string) string {
	if _, held := m[key]; held {
		return "none"
	}
	return "missing-key"
}
