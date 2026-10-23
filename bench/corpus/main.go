package main

import "runtime"

type Record struct {
	Name    string
	Samples []int
	Weight  *float64
}

type Named interface {
	NameText() string
}

func (r *Record) NameText() string { return r.Name }

func main() {
	weight := 1.5
	records := []Record{
		{Name: "alpha", Samples: []int{2, 3, 5}, Weight: &weight},
		{Name: "beta", Samples: []int{7, 11}},
	}
	lookup := map[string]*Record{"alpha": &records[0], "beta": &records[1]}
	var selected Named = lookup["alpha"]
	labels := map[string]any{"count": len(records), "unit": "samples"}
	ready := make(chan string, 1)
	go func(value Named) { ready <- value.NameText() }(selected)
	if <-ready != "alpha" {
		panic("unexpected record")
	}
	runtime.KeepAlive(records)
	runtime.KeepAlive(lookup)
	runtime.KeepAlive(labels)
	runtime.KeepAlive(selected)
}
