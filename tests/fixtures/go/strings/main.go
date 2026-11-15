

package main

import (
	"os"
	"strings"
)

//go:noinline
func stringsTarget(name string, long string, empty string) int {
	total := len(name) + len(long) + len(empty) + len(greeting) + len(longGreeting)
	return total // strings stop here
}

func main() {
	if stringsTarget("gopher", strings.Repeat("g", 300), "") != 619 {
		os.Exit(1)
	}
}

var greeting = "gopher global"
var longGreeting = strings.Repeat("h", 300)
