




package main

import (
	"bytes"
	"container/list"
	"encoding/json"
	"errors"
	"fmt"
	"math"
	"math/big"
	"os"
	"reflect"
	"runtime"
	"strconv"
	"strings"
	"sync"
	"sync/atomic"
	"syscall"
	"time"
)

//go:noinline
func barrier(checkpoint int) {
	runtime.KeepAlive(checkpoint)
}


type Point struct{ X, Y int }

var (
	second, negative, micro, overnight, zero, extreme time.Duration

	epoch, zeroTime, fixed, local, now time.Time

	unlocked, locked, contended                      sync.Mutex
	rwUnlocked, readLocked, writeLocked, contendedRW sync.RWMutex

	i32        atomic.Int32
	i64        atomic.Int64
	u32        atomic.Uint32
	u64        atomic.Uint64
	uptr       atomic.Uintptr
	set, unset atomic.Bool
	pointer    atomic.Pointer[Point]
	nilPointer atomic.Pointer[Point]
	value      atomic.Value
	emptyValue atomic.Value

	builder                              strings.Builder
	buffer, drained, noBuffer            bytes.Buffer
	data, lengthy, binary, empty, noData []byte

	base, wrapped, both, joined, missing error
	errno                                syscall.Errno

	idleGroup, busyGroup sync.WaitGroup
	once, notOnce        sync.Once

	numbers, noNumbers list.List

	small, negativeBig, twoWords, zeroBig, huge big.Int

	raw json.RawMessage
)


func view(expression, summary string) {
	fmt.Printf("VIEW: %s => %s\n", expression, summary)
}


func quoted(text string) string {
	return strconv.Quote(text)
}



func typeName(value any) string {
	ty := reflect.TypeOf(value)
	pointers := ""
	for ty.Kind() == reflect.Pointer {
		pointers += "*"
		ty = ty.Elem()
	}
	if ty.PkgPath() == "" {
		return pointers + ty.String()
	}
	return pointers + ty.PkgPath() + "." + ty.Name()
}



func errorText(err error) string {
	return fmt.Sprintf("%s *%s", typeName(err), quoted(err.Error()))
}



func parked(reason string, count int) {
	buffer := make([]byte, 1<<20)
	for {
		dump := string(buffer[:runtime.Stack(buffer, true)])
		if strings.Count(dump, " ["+reason+"]:\n") == count {
			return
		}
		runtime.Gosched()
	}
}



func monotonic(t time.Time) time.Duration {
	text := t.String()
	reading := text[strings.LastIndex(text, " m=")+3:]
	sign := time.Duration(1)
	if reading[0] == '-' {
		sign = -1
	}
	whole, fraction, _ := strings.Cut(reading[1:], ".")
	seconds, _ := strconv.ParseInt(whole, 10, 64)
	nanoseconds, _ := strconv.ParseInt(fraction, 10, 64)
	return sign * time.Duration(seconds*1e9+nanoseconds)
}

func main() {

	runtime.KeepAlive([]any{&unlocked, &rwUnlocked, &emptyValue, &idleGroup, &notOnce, &noNumbers, &zeroBig})

	second = 1500 * time.Millisecond
	negative = -(2*time.Minute + 3500*time.Millisecond)
	micro = 1500 * time.Nanosecond
	overnight = 26*time.Hour + 3*time.Second
	extreme = math.MinInt64
	for _, duration := range []struct {
		name  string
		value time.Duration
	}{
		{"second", second}, {"negative", negative}, {"micro", micro},
		{"overnight", overnight}, {"zero", zero}, {"extreme", extreme},
	} {
		view("main."+duration.name, duration.value.String())
	}



	epoch = time.Date(2009, 11, 10, 23, 0, 0, 500, time.UTC)
	fixed = epoch.In(time.FixedZone("EST", -5*60*60))
	local = time.Unix(epoch.Unix(), 0)
	view("main.epoch", fmt.Sprintf("{wall: %s, location: %s}", epoch.UTC(), quoted("UTC")))
	view("main.zeroTime", fmt.Sprintf("{wall: %s, location: %s}", zeroTime.UTC(), quoted("UTC")))
	view("main.fixed", fmt.Sprintf("{wall: %s, location: %s}", fixed.UTC(), quoted(fixed.Location().String())))
	view("*main.fixed.loc", quoted(fixed.Location().String()))
	view("main.local", fmt.Sprintf("{wall: %s, location: Local (not yet loaded)}", local.UTC()))

	locked.Lock()
	contended.Lock()
	for range 2 {
		go contended.Lock()
	}
	parked("sync.Mutex.Lock", 2)
	view("main.unlocked", "unlocked")
	view("main.locked", "locked")
	view("main.contended", "{locked: true, waiters: 2, woken: false, starving: false}")



	readLocked.RLock()
	readLocked.RLock()
	writeLocked.Lock()
	contendedRW.RLock()
	go contendedRW.Lock()
	parked("sync.RWMutex.Lock", 1)
	go contendedRW.RLock()
	parked("sync.RWMutex.RLock", 1)
	go contendedRW.Lock()
	parked("sync.Mutex.Lock", 3)
	view("main.rwUnlocked", "unlocked")
	view("main.readLocked", "{writer: false, readers: 2, waiting_readers: 0, waiting_writers: 0}")
	view("main.writeLocked", "{writer: true, readers: 0, waiting_readers: 0, waiting_writers: 0}")
	view("main.contendedRW", "{writer: true, readers: 1, waiting_readers: 1, waiting_writers: 1}")

	i32.Store(-7)
	i64.Store(1 << 40)
	u32.Store(4_000_000_000)
	u64.Store(math.MaxUint64)
	uptr.Store(0xdead)
	set.Store(true)
	pointer.Store(&Point{X: 1, Y: 2})
	value.Store(42)
	view("main.i32", fmt.Sprint(i32.Load()))
	view("main.i64", fmt.Sprint(i64.Load()))
	view("main.u32", fmt.Sprint(u32.Load()))
	view("main.u64", fmt.Sprint(u64.Load()))
	view("main.uptr", fmt.Sprint(uptr.Load()))
	view("main.set", fmt.Sprint(set.Load()))
	view("main.unset", fmt.Sprint(unset.Load()))
	view("main.pointer", fmt.Sprintf("%p", pointer.Load()))
	view("main.nilPointer", fmt.Sprintf("%p", nilPointer.Load()))
	view("main.value", fmt.Sprintf("%s %v", typeName(value.Load()), value.Load()))
	view("main.emptyValue", "nil")


	builder.WriteString("hello, ")
	builder.WriteString("world")
	buffer.WriteString("consumed unread")
	buffer.Next(len("consumed "))
	drained.WriteString("all of it")
	drained.Next(drained.Len())
	view("main.builder", quoted(builder.String()))
	view("main.buffer", quoted(buffer.String()))
	view("main.drained", quoted(drained.String()))
	view("main.noBuffer", quoted(noBuffer.String()))



	data = []byte("text\twith a tab")
	lengthy = append([]byte("a"), bytes.Repeat([]byte("µ"), 200)...)
	binary = []byte{0xff, 0, 1}
	empty = []byte{}
	view("main.data", quoted(string(data)))
	elements := []string{}
	for index, element := range data {
		elements = append(elements, fmt.Sprintf("[%d] = %d", index, element))
	}
	view("main.data", "children: "+strings.Join(elements, ", ")+", [raw]")

	view("main.lengthy", fmt.Sprintf("%s... (%d bytes)", quoted(string(lengthy[:255])), len(lengthy)))
	for _, slice := range []struct {
		name  string
		value []byte
	}{{"binary", binary}, {"empty", empty}, {"noData", noData}} {
		view("main."+slice.name, fmt.Sprintf("len=%d %s", len(slice.value),
			strings.ReplaceAll(fmt.Sprint(slice.value), " ", ", ")))
	}

	base = errors.New("bad")
	wrapped = fmt.Errorf("read config: %w", base)
	both = fmt.Errorf("both: %w and %w", base, wrapped)
	joined = errors.Join(base, wrapped)
	_, missing = os.Open("/nonexistent/microscope")
	errno = syscall.ENOENT
	view("main.base", errorText(base))
	view("main.wrapped", errorText(wrapped))
	view("main.wrapped", fmt.Sprintf("children: wrapped = %s, [raw]", errorText(errors.Unwrap(wrapped))))
	view("main.both", errorText(both))

	view("*(errors.errorString*)main.base.data", quoted(base.Error()))
	view("*(fmt.wrapError*)main.wrapped.data", quoted(wrapped.Error()))
	view("*(fmt.wrapErrors*)main.both.data", quoted(both.Error()))
	view("*(errors.joinError*)main.joined.data", fmt.Sprintf("len=2 [%s, %s]", errorText(base), errorText(wrapped)))
	view("main.joined", fmt.Sprintf("%s *len=2 [%s, %s]", typeName(joined), errorText(base), errorText(wrapped)))
	var pathError *os.PathError
	errors.As(missing, &pathError)
	view("main.missing", fmt.Sprintf("%s *{Op: %s, Path: %s, Err: %s %s}", typeName(missing),
		quoted(pathError.Op), quoted(pathError.Path), typeName(pathError.Err), quoted(pathError.Err.Error())))
	view("main.errno", quoted(errno.Error()))


	busyGroup.Add(3)
	go busyGroup.Wait()
	parked("sync.WaitGroup.Wait", 1)
	view("main.idleGroup", "{counter: 0, waiters: 0}")
	view("main.busyGroup", "{counter: 3, waiters: 1}")
	once.Do(func() {})
	view("main.once", "done")
	view("main.notOnce", "not done")

	for _, number := range []int{1, 2, 3} {
		numbers.PushBack(number)
	}
	listed := []string{}
	for element := numbers.Front(); element != nil; element = element.Next() {
		listed = append(listed, fmt.Sprintf("%s %v", typeName(element.Value), element.Value))
	}
	view("main.numbers", fmt.Sprintf("len=%d [%s]", numbers.Len(), strings.Join(listed, ", ")))
	view("main.noNumbers", "len=0 []")


	small.SetInt64(12345)
	negativeBig.SetInt64(-9876543210)
	twoWords.Lsh(big.NewInt(3), 70)
	huge.Lsh(big.NewInt(1), 130)
	for _, number := range []struct {
		name  string
		value *big.Int
	}{{"small", &small}, {"negativeBig", &negativeBig}, {"twoWords", &twoWords}, {"zeroBig", &zeroBig}} {
		view("main."+number.name, number.value.String())
	}
	view("main.huge", "{neg: false, abs: […]}")

	raw = json.RawMessage(`{"a":1}`)
	view("main.raw", quoted(string(raw)))
	barrier(1)



	now = time.Now()
	view("main.now", fmt.Sprintf("{wall: %s, location: %s, monotonic: %s}",
		now.UTC(), quoted(now.Location().String()), monotonic(now)))
	view("main.local", fmt.Sprintf("{wall: %s, location: %s}", local.UTC(), quoted(local.Location().String())))
	barrier(2)
}
