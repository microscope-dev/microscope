


package main

import (
	"fmt"
	"os"
)


type Hits map[string]int


type Bytes map[string]int64


type Hours [24]int

func record(hits Hits, bytes Bytes, hours *Hours, tick int) {
	paths := []string{"/api/search", "/api/items", "/static/app.js", "/api/login", "/api/cart", "/healthz"}
	for i, path := range paths {
		hits[path] += (len(paths) - i) * 100
	}
	types := []string{"text/html", "application/json", "image/png", "text/css", "image/svg+xml", "font/woff2", "text/plain", "application/wasm", "image/webp", "text/csv"}
	for i, kind := range types {
		bytes[kind] += int64((len(types)-i)*(len(types)-i)) * 1000
	}
	for hour := range hours {
		hours[hour] += 10 + (hour*7+tick)%13
	}
}

func main() {
	hits := Hits{}
	bytes := Bytes{}
	var hours Hours

	for i := range 50 {
		hits[fmt.Sprintf("/static/asset-%02d.png", i)] = 1
	}
	for tick := 1; tick <= 100; tick++ {
		record(hits, bytes, &hours, tick)
		fmt.Fprintf(os.Stdout, "tick %d: %d paths\n", tick, len(hits))
	}
}
