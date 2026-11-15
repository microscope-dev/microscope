package main

type record struct {
	id    int32
	label string
}


var crashTarget *int32

//go:noinline
func crashNow(r *record, depth int32) int32 {
	doubled := depth * 2
	*crashTarget = doubled + r.id
	return doubled
}

func main() {
	crashNow(&record{id: 42, label: "go crash"}, 3)
}
