



package main

import (
	"os"
	"os/exec"
)

func main() {
	if err := exec.Command("true").Run(); err != nil {
		os.Exit(3)
	}
	os.Exit(0)
}
