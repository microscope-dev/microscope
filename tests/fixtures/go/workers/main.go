








package main

import (
	"fmt"
	"os"
	"runtime"
	"strings"
	"sync"
	"syscall"
	"time"
)


var sink int



//go:noinline
func reached(name string) {
	sink += len(name)
}


func truth(fields ...any) {
	text := make([]string, len(fields))
	for index, field := range fields {
		text[index] = fmt.Sprint(field)
	}
	fmt.Println("TRUTH\t" + strings.Join(text, "\t"))
}


type goroutine struct {
	id     string
	status string
	frames []string
}


func dump() []goroutine {
	buffer := make([]byte, 1<<20)
	text := string(buffer[:runtime.Stack(buffer, true)])
	var goroutines []goroutine
	for _, block := range strings.Split(strings.TrimSpace(text), "\n\n") {
		lines := strings.Split(block, "\n")

		header := strings.TrimSuffix(strings.TrimPrefix(lines[0], "goroutine "), ":")
		id, rest, _ := strings.Cut(header, " ")
		status := strings.TrimSuffix(strings.TrimPrefix(rest, "["), "]")
		status, _, _ = strings.Cut(status, ",")
		current := goroutine{id: id, status: status}


		for index := 1; index+1 < len(lines); index += 2 {
			call := lines[index]
			if strings.HasPrefix(call, "created by ") {
				break
			}
			function := call[:strings.LastIndex(call, "(")]
			position := strings.TrimSpace(lines[index+1])
			position, _, _ = strings.Cut(position, " ")
			current.frames = append(current.frames, function+"\t"+position)
		}
		goroutines = append(goroutines, current)
	}
	return goroutines
}


func me() string {
	buffer := make([]byte, 64)
	header := string(buffer[:runtime.Stack(buffer, false)])
	id, _, _ := strings.Cut(strings.TrimPrefix(header, "goroutine "), " ")
	return id
}


func awaitStatuses(statuses ...string) {
	for {
		seen := map[string]int{}
		for _, goroutine := range dump() {
			seen[goroutine.status]++
		}
		missing := false
		for _, status := range statuses {
			if seen[status] == 0 {
				missing = true
			}
			seen[status]--
		}
		if !missing {
			return
		}
		runtime.Gosched()
	}
}


func checkpoint(name string) {
	goroutines := dump()
	truth("checkpoint", name)
	truth("count", runtime.NumGoroutine())
	truth("main", me(), syscall.Gettid())
	for _, goroutine := range goroutines {
		truth("task", goroutine.id, goroutine.status)
		for _, frame := range goroutine.frames {
			truth("frame", goroutine.id, frame)
		}
	}
	reached(name)
}


func worker(jobs <-chan int, results chan<- int, group *sync.WaitGroup) {
	defer group.Done()
	for job := range jobs {
		results <- job * job
	}
}

func main() {

	runtime.LockOSThread()

	jobs := make(chan int)
	results := make(chan int, 16)
	var workers sync.WaitGroup
	for range 4 {
		workers.Add(1)
		go worker(jobs, results, &workers)
	}


	var lock sync.Mutex
	lock.Lock()
	go func() {
		lock.Lock()
		lock.Unlock()
	}()

	never := make(chan int)
	done := make(chan struct{})
	go func() {
		select {
		case <-never:
		case <-done:
		}
	}()

	go func() {
		time.Sleep(time.Hour)
	}()

	var waiting sync.Mutex
	condition := sync.NewCond(&waiting)
	signalled := false
	go func() {
		waiting.Lock()
		for !signalled {
			condition.Wait()
		}
		waiting.Unlock()
	}()

	reader, writer, err := os.Pipe()
	if err != nil {
		panic(err)
	}
	go func() {
		buffer := make([]byte, 1)
		reader.Read(buffer)
	}()



	var raw [2]int
	if err := syscall.Pipe(raw[:]); err != nil {
		panic(err)
	}
	go func() {
		buffer := make([]byte, 1)
		syscall.Read(raw[0], buffer)
	}()

	awaitStatuses(
		"chan receive", "chan receive", "chan receive", "chan receive",
		"sync.Mutex.Lock", "select", "sleep", "sync.Cond.Wait", "IO wait",
		"syscall",
	)
	checkpoint("parked")

	for job := range 8 {
		jobs <- job
	}
	close(jobs)
	workers.Wait()
	close(results)
	total := 0
	for result := range results {
		total += result
	}

	lock.Unlock()
	close(done)
	waiting.Lock()
	signalled = true
	condition.Broadcast()
	waiting.Unlock()
	writer.Write([]byte{1})
	syscall.Write(raw[1], []byte{1})

	if total != 140 {
		os.Exit(1)
	}
}
