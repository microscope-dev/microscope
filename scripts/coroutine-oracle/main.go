











package main

import (
	"bufio"
	"fmt"
	"io"
	"os"
	"regexp"
	"strconv"
	"strings"
)

func main() {
	if len(os.Args) != 2 {
		fmt.Fprintln(os.Stderr, "usage: coroutine-oracle DUMP")
		os.Exit(2)
	}
	if err := reduce(os.Args[1], os.Stdout); err != nil {
		fmt.Fprintf(os.Stderr, "coroutine-oracle: %v\n", err)
		os.Exit(1)
	}
}


type entry struct {
	depth                                         int
	offset, tag, name, typ, location, line, discr string
}

type reducer struct {
	out *bufio.Writer



	named   map[string]string
	awaitee map[string]string

	parents, tags, names []string

	coroutine          string
	coroutineDepth     int
	stateOffset, state string
}

func reduce(path string, out io.Writer) error {
	r := reducer{
		out:     bufio.NewWriterSize(out, 1<<16),
		named:   map[string]string{},
		awaitee: map[string]string{},
	}
	for pass := 1; pass <= 2; pass++ {
		if err := r.read(path, pass); err != nil {
			return err
		}
	}
	return r.out.Flush()
}

func (r *reducer) read(path string, pass int) error {
	file, err := os.Open(path)
	if err != nil {
		return err
	}
	defer file.Close()
	scanner := bufio.NewScanner(file)
	scanner.Buffer(make([]byte, 1<<20), 1<<30)
	r.coroutine = ""
	var current *entry
	for scanner.Scan() {
		line := scanner.Text()

		if strings.HasPrefix(line, "    ") && !readAttribute(secondField(line)) {
			continue
		}
		if next, ok := header(line); ok {
			if current != nil {
				r.finish(current, pass)
			}
			current = &next
			continue
		}
		if current == nil {
			continue
		}
		rest, ok := attribute(line)
		if !ok {
			continue
		}
		switch {
		case strings.HasPrefix(rest, "DW_AT_name "):
			current.name = value(line)
		case strings.HasPrefix(rest, "DW_AT_type "):
			current.typ = reference(line)
		case strings.HasPrefix(rest, "DW_AT_data_member_location"):
			current.location = value(line)
		case strings.HasPrefix(rest, "DW_AT_decl_line"):
			current.line = value(line)
		case strings.HasPrefix(rest, "DW_AT_discr_value"):
			current.discr = value(line)
		}
	}
	if err := scanner.Err(); err != nil {
		return err
	}
	if current != nil {
		r.finish(current, pass)
	}
	return nil
}

var attributes = []string{
	"DW_AT_name", "DW_AT_type", "DW_AT_data_member_location", "DW_AT_decl_line",
	"DW_AT_discr_value",
}

func readAttribute(field string) bool {
	for _, prefix := range attributes {
		if strings.HasPrefix(field, prefix) {
			return true
		}
	}
	return false
}


func secondField(line string) string {
	rest := strings.TrimLeft(line, " \t")
	end := strings.IndexAny(rest, " \t")
	if end < 0 {
		return ""
	}
	return strings.TrimLeft(rest[end:], " \t")
}


func header(line string) (entry, bool) {
	if !strings.HasPrefix(line, " <") {
		return entry{}, false
	}
	rest := line[2:]
	depthEnd := strings.IndexByte(rest, '>')
	if depthEnd <= 0 || !all(rest[:depthEnd], isDigit) {
		return entry{}, false
	}
	depth, err := strconv.Atoi(rest[:depthEnd])
	if err != nil {
		return entry{}, false
	}
	rest = rest[depthEnd+1:]
	if !strings.HasPrefix(rest, "<") {
		return entry{}, false
	}
	offsetEnd := strings.IndexByte(rest, '>')
	offset := rest[1:max(offsetEnd, 1)]
	if offsetEnd <= 1 || !all(offset, isHex) {
		return entry{}, false
	}
	rest, ok := strings.CutPrefix(rest[offsetEnd+1:], ": Abbrev Number: ")
	if !ok || rest == "" || !isDigit(rest[0]) {
		return entry{}, false
	}
	tag := "null"
	if open := strings.LastIndex(line, "(DW_TAG_"); open >= 0 && strings.HasSuffix(line, ")") {
		if name := line[open+1 : len(line)-1]; all(name[len("DW_TAG_"):], isTagByte) &&
			len(name) > len("DW_TAG_") {
			tag = name
		}
	}
	return entry{depth: depth, offset: offset, tag: tag}, true
}


func attribute(line string) (string, bool) {
	rest := strings.TrimLeft(line, " ")
	if len(rest) == len(line) || !strings.HasPrefix(rest, "<") {
		return "", false
	}
	end := strings.IndexByte(rest, '>')
	if end <= 1 || !all(rest[1:end], isHex) {
		return "", false
	}
	after := strings.TrimLeft(rest[end+1:], " ")
	if len(after) == len(rest[end+1:]) {
		return "", false
	}
	return after, true
}

var (
	form          = regexp.MustCompile(`^\([a-z0-9_]+\) `)
	stringOffset  = regexp.MustCompile(`^\(((indirect|indexed) (line )?string, )?offset: 0x[0-9a-f]+\): `)
	indexedString = regexp.MustCompile(`^\(indexed string: 0x[0-9a-f]+\): `)
	referenced    = regexp.MustCompile(`<0x([0-9a-f]+)>`)
)



func value(line string) string {
	if colon := strings.IndexByte(line, ':'); colon >= 0 && strings.HasPrefix(line[colon+1:], " ") {
		line = line[colon+2:]
	}
	if strings.HasPrefix(line, "(") {
		for _, prefix := range []*regexp.Regexp{form, stringOffset, indexedString} {
			if match := prefix.FindStringIndex(line); match != nil {
				line = line[match[1]:]
			}
		}
	}
	return line
}

func reference(line string) string {
	if match := referenced.FindStringSubmatch(line); match != nil {
		return match[1]
	}
	return ""
}


func (r *reducer) finish(e *entry, pass int) {
	r.tags = setAt(r.tags, e.depth, e.tag)
	r.names = setAt(r.names, e.depth, e.name)
	if pass == 1 {
		if e.name != "" {
			r.named[e.offset] = e.name
		}
		if e.tag == "DW_TAG_member" && e.name == "__awaitee" && e.depth > 0 {
			r.awaitee[at(r.parents, e.depth-1)] = e.typ
		}
		r.parents = setAt(r.parents, e.depth, e.offset)
		return
	}
	if r.coroutine != "" && e.depth <= r.coroutineDepth {
		r.coroutine = ""
	}
	if e.tag == "DW_TAG_structure_type" && isCoroutineName(e.name) {
		var path strings.Builder
		for d := range e.depth {
			if at(r.tags, d) == "DW_TAG_namespace" {
				path.WriteString(at(r.names, d))
				path.WriteString("::")
			}
		}
		r.coroutine = path.String() + e.name
		r.coroutineDepth = e.depth
		r.stateOffset = "?"
	} else if r.coroutine != "" {
		switch {
		case e.tag == "DW_TAG_member" && e.depth == r.coroutineDepth+2 && e.name == "__state":
			r.stateOffset = e.location
		case e.tag == "DW_TAG_variant" && e.depth == r.coroutineDepth+2:
			r.state = e.discr
		case e.tag == "DW_TAG_member" && e.depth == r.coroutineDepth+3:
			awaited := "-"
			if future, ok := r.awaitee[e.typ]; ok {
				if name, ok := r.named[future]; ok {
					awaited = name
				}
			}


			record, ok := r.named[e.typ]
			if !ok {
				r.named[e.typ] = ""
			}
			fmt.Fprintf(r.out, "%s\t%s\t%s\t%s\t%s\t%s\n", r.coroutine, r.stateOffset, r.state,
				record, e.line, awaited)
		}
	}
}



func isCoroutineName(name string) bool {
	for _, kind := range []string{"{async_fn_env#", "{async_block_env#", "{async_closure_env#"} {
		if rest, ok := strings.CutPrefix(name, kind); ok {
			digits := 0
			for digits < len(rest) && isDigit(rest[digits]) {
				digits++
			}
			return digits > 0 && digits < len(rest) && rest[digits] == '}'
		}
	}
	return false
}

func setAt(values []string, index int, value string) []string {
	for len(values) <= index {
		values = append(values, "")
	}
	values[index] = value
	return values
}

func at(values []string, index int) string {
	if index < len(values) {
		return values[index]
	}
	return ""
}

func all(text string, accept func(byte) bool) bool {
	for i := range len(text) {
		if !accept(text[i]) {
			return false
		}
	}
	return true
}

func isDigit(b byte) bool   { return '0' <= b && b <= '9' }
func isHex(b byte) bool     { return isDigit(b) || 'a' <= b && b <= 'f' }
func isTagByte(b byte) bool { return 'a' <= b && b <= 'z' || b == '_' }
