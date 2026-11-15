





package main

import (
	"debug/dwarf"
	"debug/elf"
	"encoding/binary"
	"fmt"
	"os"
	"runtime"
	"runtime/debug"
	"strconv"
	"strings"
	"sync/atomic"
	"unsafe"
)


type layout struct {
	allgs            uintptr
	goid, status     uintptr
	stackLo, stackHi uintptr
	schedPC, schedSP uintptr
	waiting          uint32
}

func field(record *dwarf.StructType, name string) *dwarf.StructField {
	for _, member := range record.Field {
		if member.Name == name {
			return member
		}
	}
	panic("runtime.g has no " + name)
}

func offset(record *dwarf.StructType, path ...string) uintptr {
	total := uintptr(0)
	for index, name := range path {
		member := field(record, name)
		total += uintptr(member.ByteOffset)
		if index+1 < len(path) {
			kind := member.Type
			for {
				named, ok := kind.(*dwarf.TypedefType)
				if !ok {
					break
				}
				kind = named.Type
			}
			record = kind.(*dwarf.StructType)
		}
	}
	return total
}



func readLayout() layout {
	path, err := os.Executable()
	if err != nil {
		panic(err)
	}
	executable, err := elf.Open(path)
	if err != nil {
		panic(err)
	}
	data, err := executable.DWARF()
	if err != nil {
		panic(err)
	}
	var found layout
	reader := data.Reader()
	for {
		entry, err := reader.Next()
		if err != nil {
			panic(err)
		}
		if entry == nil {
			break
		}
		name, _ := entry.Val(dwarf.AttrName).(string)
		switch {
		case entry.Tag == dwarf.TagStructType && name == "runtime.g":
			kind, err := data.Type(entry.Offset)
			if err != nil {
				panic(err)
			}
			g := kind.(*dwarf.StructType)
			found.goid = offset(g, "goid")
			found.status = offset(g, "atomicstatus")
			found.stackLo = offset(g, "stack", "lo")
			found.stackHi = offset(g, "stack", "hi")
			found.schedPC = offset(g, "sched", "pc")
			found.schedSP = offset(g, "sched", "sp")
		case entry.Tag == dwarf.TagVariable && name == "runtime.allgs":
			location := entry.Val(dwarf.AttrLocation).([]byte)

			found.allgs = uintptr(binary.LittleEndian.Uint64(location[1:]))
		case entry.Tag == dwarf.TagConstant && name == "runtime._Gwaiting":
			found.waiting = uint32(entry.Val(dwarf.AttrConstValue).(int64))
		}
	}
	return found
}


func goid() uint64 {
	buffer := make([]byte, 64)
	header := strings.Fields(string(buffer[:runtime.Stack(buffer, false)]))
	id, err := strconv.ParseUint(header[1], 10, 64)
	if err != nil {
		panic(err)
	}
	return id
}

func word(address uintptr) *uintptr {
	return (*uintptr)(unsafe.Pointer(address))
}

//go:noinline
func parked(ids chan<- uint64, forever chan struct{}) {
	ids <- goid()
	<-forever // CORRUPT: parked
}

//go:noinline
func checkpoint() {}

func main() {
	debug.SetGCPercent(-1)
	where := readLayout()
	ids := make(chan uint64)
	forever := make(chan struct{})
	for range 3 {
		go parked(ids, forever)
	}
	victim := <-ids
	<-ids
	<-ids


	allgs := *(*[]uintptr)(unsafe.Pointer(where.allgs))
	var g uintptr
	for _, candidate := range allgs {
		if *(*uint64)(unsafe.Pointer(candidate + where.goid)) == victim {
			g = candidate
		}
	}
	status := (*uint32)(unsafe.Pointer(g + where.status))
	for atomic.LoadUint32(status) != where.waiting {
		runtime.Gosched()
	}
	*word(g + where.schedPC) = 0xdead
	*word(g + where.schedSP) = 0x10
	*word(g + where.stackLo) = 0x30
	*word(g + where.stackHi) = 0x20
	fmt.Println("corrupted goroutine", victim)
	checkpoint()
}
