




package main

import (
	"bufio"
	"debug/elf"
	"debug/gosym"
	"fmt"
	"os"
)

func main() {
	if len(os.Args) != 2 {
		fmt.Fprintln(os.Stderr, "usage: gosym-oracle EXECUTABLE")
		os.Exit(2)
	}
	if err := record(os.Args[1]); err != nil {
		fmt.Fprintf(os.Stderr, "gosym-oracle: %v\n", err)
		os.Exit(1)
	}
}

func record(path string) error {
	file, err := elf.Open(path)
	if err != nil {
		return err
	}
	defer file.Close()

	section := file.Section(".gopclntab")
	if section == nil {
		return fmt.Errorf("%s has no .gopclntab section", path)
	}
	data, err := section.Data()
	if err != nil {
		return err
	}
	text, err := textStart(file)
	if err != nil {
		return err
	}
	table, err := gosym.NewTable(nil, gosym.NewLineTable(data, text))
	if err != nil {
		return err
	}

	out := bufio.NewWriter(os.Stdout)
	defer out.Flush()
	fmt.Fprintln(out, "microscope-gosym-oracle-v1")
	for _, function := range table.Funcs {
		fmt.Fprintf(out, "func\t%#x\t%#x\t%s\n", function.Entry, function.End, function.Name)
	}
	for _, function := range table.Funcs {
		if function.End <= function.Entry {
			continue
		}
		for _, pc := range []uint64{
			function.Entry,
			function.Entry + (function.End-function.Entry)/2,
			function.End - 1,
		} {
			file, line, _ := table.PCToLine(pc)
			fmt.Fprintf(out, "line\t%#x\t%s\t%d\n", pc, file, line)
		}
	}
	return nil
}






func textStart(file *elf.File) (uint64, error) {
	if symbols, err := file.Symbols(); err == nil {
		for _, symbol := range symbols {
			if symbol.Name == "runtime.text" {
				return symbol.Value, nil
			}
		}
	}
	section := file.Section(".text")
	if section == nil {
		return 0, fmt.Errorf("the image has no .text section")
	}
	return section.Addr, nil
}
