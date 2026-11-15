



package main

import (
	"bufio"
	"fmt"
	"os"
	"sync"
	"sync/atomic"
	"time"
)

type op int

const (
	opGet op = iota
	opPut
)

type entry struct {
	key   string
	value string
}

type stats struct {
	gets   uint64
	puts   uint64
	misses uint64
}

type server struct {
	table map[string]*entry
	stats stats
	lock  sync.Mutex
}

type request struct {
	op    op
	key   string
	value string
}

var (
	store      = server{table: make(map[string]*entry)}
	queue      = make(chan request, 16)
	names      = [...]string{"alice", "bob", "carol", "dave"}
	inputEnded atomic.Bool
	output     sync.Mutex
)


func handleRequest(s *server, req *request) int {
	s.lock.Lock()
	defer s.lock.Unlock()
	e := s.table[req.key]
	status := 0
	switch req.op {
	case opGet:
		s.stats.gets++
		if e == nil {
			s.stats.misses++
			status = -1
		}
	case opPut:
		if e == nil {
			e = &entry{key: req.key}
			s.table[req.key] = e
		}
		s.stats.puts++
		e.value = req.value
	}
	return status
}

func say(format string, args ...any) {
	output.Lock()
	defer output.Unlock()
	fmt.Printf(format, args...)
}

func worker(id int, done *sync.WaitGroup) {
	defer done.Done()
	for req := range queue {
		status := handleRequest(&store, &req)
		verb := "get"
		if req.op == opPut {
			verb = "put"
		}
		say("worker %d: %s %s -> %d\n", id, verb, req.key, status)
	}
}

func readInput() {
	scanner := bufio.NewScanner(os.Stdin)
	for scanner.Scan() {
		say("input: %s\n", scanner.Text())
	}
	inputEnded.Store(true)
}

func main() {
	var workers sync.WaitGroup
	for id := 1; id <= 2; id++ {
		workers.Add(1)
		go worker(id, &workers)
	}
	input := make(chan struct{})
	go func() {
		readInput()
		close(input)
	}()

	for round := 0; !inputEnded.Load(); round++ {
		req := request{op: opPut, key: fmt.Sprintf("user:%d", 1000+round%24), value: names[round%4]}
		if round%3 == 2 {
			req.op = opGet
		}
		queue <- req
		time.Sleep(20 * time.Millisecond)
	}
	close(queue)
	workers.Wait()
	<-input
	fmt.Printf("served %d puts, %d gets, %d misses\n", store.stats.puts, store.stats.gets, store.stats.misses)
}
