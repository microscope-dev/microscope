// The BPF engine's kernel side (dev/syscalls.md). It filters by
// process, timestamps each chosen call's entry, and at its exit counts it,
// copies the bytes behind its arguments as its spec says, and sends one
// record. microscope decodes and interprets everything in Rust. With
// stacks, a thread of a Python process microscope told it of sends its
// Python frames before its stack, read by the sampler's walker.
//
// The verifier proves these programs terminate and touch only what they
// may; they keep to bounded loops, no tail calls, and per-CPU scratch for
// anything larger than the stack.

#include <linux/bpf.h>
#include <bpf/bpf_helpers.h>
#include <bpf/bpf_core_read.h>

#include "kernel_types.h"
#include "python.bpf.h"
#include "v8.bpf.h"

#define TS_COMPAT 0x0002
#define SIGNAL_GROUP_EXIT 0x00000004
#define X32_SYSCALL_BIT 0x40000000
#define SLOTS 3072
// A per-CPU array's value holds at most 32 KiB.
#define SCRATCH 32768
// Captures start in the first half of the scratch record, so that one of
// at most CHUNK bytes always fits after its header. Four of the largest
// fit, and no call has more.
#define CAPTURES_END 16384
// The most bytes one capture holds; captures::MOST_BYTES.
#define CHUNK 4096
#define MOST_ELEMENTS 64
#define MOST_IOVECS 16
#define MOST_COUNTED 4096
#define MAX_ERRNO 4095
// The most frames of the kernel's walk, as perf's default.
#define STACK_FRAMES 127
// A stack copy, in chunks that each read or fail whole.
#define STACK_CHUNK 512
#define STACK_CHUNKS 16

#ifdef MICROSCOPE_FAULTS
// The first argument of a getppid whose entry the bpf-faults build skips,
// as tests/fixtures/c/syscalls.c's `skipped` mode makes it.
#define FAULT_SKIP_ENTRY 0x5ca1ab1e
#endif

struct {
	__uint(type, BPF_MAP_TYPE_HASH);
	__uint(max_entries, 8192);
	__type(key, u32);
	__type(value, u8);
} TARGETS SEC(".maps");

struct {
	__uint(type, BPF_MAP_TYPE_ARRAY);
	__uint(max_entries, SLOTS);
	__type(key, u32);
	__type(value, struct call_spec);
} CALLS SEC(".maps");

struct {
	__uint(type, BPF_MAP_TYPE_ARRAY);
	__uint(max_entries, 1);
	__type(key, u32);
	__type(value, struct config);
} CONFIG SEC(".maps");

// Each thread's call in progress, held in the thread itself: cheaper than
// any map keyed by its id, and gone with it. The in_flight iterator reads
// them all.
struct {
	__uint(type, BPF_MAP_TYPE_TASK_STORAGE);
	__uint(map_flags, BPF_F_NO_PREALLOC);
	__type(key, int);
	__type(value, struct entry);
} ENTRIES SEC(".maps");

// The Python thread state the walker last found each thread's frames from,
// which it tries first: searching every interpreter's threads costs a read
// for each, at every call.
struct {
	__uint(type, BPF_MAP_TYPE_TASK_STORAGE);
	__uint(map_flags, BPF_F_NO_PREALLOC);
	__type(key, int);
	__type(value, u64);
} PY_STATES SEC(".maps");

// Each thread's statistics of each chosen call. A thread counts on one CPU
// at a time, so its values need no atomics.
struct {
	__uint(type, BPF_MAP_TYPE_HASH);
	__uint(max_entries, 8192);
	__type(key, struct stat_key);
	__type(value, struct stat_value);
} THREAD_STATS SEC(".maps");

// Each chosen call's statistics, by its call_spec.stat, of threads
// THREAD_STATS has no room for; microscope sizes it to the calls it
// chooses.
struct {
	__uint(type, BPF_MAP_TYPE_PERCPU_ARRAY);
	__uint(max_entries, 1);
	__type(key, u32);
	__type(value, struct stat_value);
} STATS SEC(".maps");

// Zeroes, to start a thread's statistics from: too large for the stack.
struct {
	__uint(type, BPF_MAP_TYPE_ARRAY);
	__uint(max_entries, 1);
	__type(key, u32);
	__type(value, struct stat_value);
} BLANK SEC(".maps");

struct {
	__uint(type, BPF_MAP_TYPE_PERCPU_HASH);
	__uint(max_entries, 8192);
	__type(key, struct errno_key);
	__type(value, u64);
} ERRNOS SEC(".maps");

struct {
	__uint(type, BPF_MAP_TYPE_PERCPU_ARRAY);
	__uint(max_entries, LOSSES);
	__type(key, u32);
	__type(value, u64);
} LOST SEC(".maps");

struct {
	__uint(type, BPF_MAP_TYPE_PERCPU_ARRAY);
	__uint(max_entries, 1);
	__type(key, u32);
	__type(value, struct bucket);
} BUCKETS SEC(".maps");

// A map of maps whose only use is that setting its element waits until
// every program running has finished: after a generation's switch, none
// still counts into the old one.
struct fence_token {
	__uint(type, BPF_MAP_TYPE_ARRAY);
	__uint(max_entries, 1);
	__type(key, u32);
	__type(value, u32);
} FENCE_TOKEN SEC(".maps");

struct {
	__uint(type, BPF_MAP_TYPE_ARRAY_OF_MAPS);
	__uint(max_entries, 1);
	__type(key, u32);
	__array(values, struct fence_token);
} FENCE SEC(".maps");

// Each CPU's count of the chosen calls it saw return: what tracing cost,
// as microscope estimates it where the kernel does not time programs.
struct {
	__uint(type, BPF_MAP_TYPE_PERCPU_ARRAY);
	__uint(max_entries, 1);
	__type(key, u32);
	__type(value, u64);
} RUNS SEC(".maps");

// Each CPU's count of calls, for sampling one in config.sample.
struct {
	__uint(type, BPF_MAP_TYPE_PERCPU_ARRAY);
	__uint(max_entries, 1);
	__type(key, u32);
	__type(value, u64);
} SAMPLED SEC(".maps");

// Whether this call is the one in `every` that is sent.
static __always_inline int sampled_in(u32 every)
{
	if (every <= 1)
		return 1;
	u32 key = 0;
	u64 *count = bpf_map_lookup_elem(&SAMPLED, &key);
	if (!count)
		return 1;
	*count += 1;
	return *count % every == 0;
}

// Whether a call is sent to the stream; the kernel counts those it is not,
// when it counts.
static __always_inline int sent(struct config *settings, u8 flags)
{
	return (settings->flags & CONFIG_STREAM) && !(flags & CALL_SAMPLED_OUT);
}

// Where each CPU builds a record. Naming every record here also puts its
// layout in the object's BTF, which a test compares with the decoders.
struct scratch {
	union {
		u8 data[SCRATCH];
		struct call_record call;
		struct capture_header capture;
		struct spawn_record spawn;
		struct exec_record exec;
		struct handoff_record handoff;
		struct exit_record exit;
		struct signal_record signal;
		struct file_record file;
		struct stack_record stack;
		struct in_flight_record in_flight;
	};
};

struct {
	__uint(type, BPF_MAP_TYPE_PERCPU_ARRAY);
	__uint(max_entries, 1);
	__type(key, u32);
	__type(value, struct scratch);
} BUFFER SEC(".maps");

// A CPU's ring of records; microscope sizes each.
struct ring {
	__uint(type, BPF_MAP_TYPE_RINGBUF);
	__uint(max_entries, 4096);
};

// Each CPU's ring, by its number: CPUs writing one ring between them
// contend for its lock, which slows each call eightfold over eight
// threads. Microscope merges the rings by the records' times.
struct {
	__uint(type, BPF_MAP_TYPE_ARRAY_OF_MAPS);
	__uint(max_entries, 1);
	__type(key, u32);
	__array(values, struct ring);
} EVENTS SEC(".maps");

// Each Python process's layout, by its process id as the kernel's initial
// pid namespace numbers it.
struct {
	__uint(type, BPF_MAP_TYPE_HASH);
	__uint(max_entries, 1024);
	__type(key, u32);
	__type(value, struct py_layout);
} LAYOUTS SEC(".maps");

// Where each CPU builds a record of Python frames.
struct {
	__uint(type, BPF_MAP_TYPE_PERCPU_ARRAY);
	__uint(max_entries, 1);
	__type(key, u32);
	__type(value, struct py_stack);
} PY_SCRATCH SEC(".maps");

// Each Node process's V8 layout, by its process id as the kernel's initial
// pid namespace numbers it.
struct {
	__uint(type, BPF_MAP_TYPE_HASH);
	__uint(max_entries, 1024);
	__type(key, u32);
	__type(value, struct v8_layout);
} V8_LAYOUTS SEC(".maps");

// Where each CPU builds a record of JavaScript frames.
struct {
	__uint(type, BPF_MAP_TYPE_PERCPU_ARRAY);
	__uint(max_entries, 1);
	__type(key, u32);
	__type(value, struct v8_stack);
} V8_SCRATCH SEC(".maps");


// `value & mask`, computed where the verifier sees it: the barrier keeps
// the compiler from dropping a mask it can prove redundant.
static __always_inline u32 bounded(u32 value, u32 mask)
{
	asm volatile("" : "+r"(value));
	return value & mask;
}

// The last word of the scratch record, which no record reaches.
#define SPARE (SCRATCH - 8)

// `value & mask`, read back from map memory, where the verifier sees it
// with no history. A loop whose iterations fork on how a length was
// found then reaches the same state on every path, which the verifier
// prunes, instead of one state per path per iteration.
static __always_inline u32 forget(struct scratch *buffer, u32 value, u32 mask)
{
	volatile u32 *spare = (volatile u32 *)(buffer->data + SPARE);
	*spare = value;
	return *spare & mask;
}

static __always_inline struct config *config(void)
{
	u32 zero = 0;
	return bpf_map_lookup_elem(&CONFIG, &zero);
}

// Whether the current task's process is a target.
static __always_inline int traced(u32 tgid)
{
	return bpf_map_lookup_elem(&TARGETS, &tgid) != 0;
}

static __always_inline void lost_as(u32 why)
{
	u64 *count = bpf_map_lookup_elem(&LOST, &why);
	if (count)
		*count += 1;
}

static __always_inline void lost(void)
{
	lost_as(LOSS_DROPPED);
}

static __always_inline void *ring(void)
{
	u32 cpu = bpf_get_smp_processor_id();
	return bpf_map_lookup_elem(&EVENTS, &cpu);
}

static __always_inline void *reserve(u64 size)
{
	void *events = ring();
	return events ? bpf_ringbuf_reserve(events, size, 0) : 0;
}

// Sends `size` bytes from `data`, or counts them lost; nonzero when lost.
static __always_inline int output(void *data, u64 size)
{
	void *events = ring();
	if (events && !bpf_ringbuf_output(events, data, size, 0))
		return 0;
	lost();
	return -1;
}

// Sends `size` bytes from `data`; nonzero when the ring had no room.
static __always_inline int emit(void *data, u64 size)
{
	void *events = ring();
	return events ? bpf_ringbuf_output(events, data, size, 0) : -1;
}

// Whether this CPU's bucket lets one more failed or slow call through at
// `now`. It fills at config.cap calls a second up to config.burst, and
// starts full.
static __always_inline int admitted(struct config *settings, u64 now)
{
	if (!settings->cap)
		return 1;
	u32 zero = 0;
	struct bucket *bucket = bpf_map_lookup_elem(&BUCKETS, &zero);
	if (!bucket)
		return 0;
	u64 one = 1000000000ULL;
	u64 full = (u64)settings->burst * one;
	u64 elapsed = now - bucket->filled;
	bucket->filled = now;
	u64 credit = elapsed >= full / settings->cap ? full : bucket->credit + elapsed * settings->cap;
	if (credit > full)
		credit = full;
	if (credit < one) {
		bucket->credit = credit;
		return 0;
	}
	bucket->credit = credit - one;
	return 1;
}

static __always_inline int failed(s64 ret)
{
	return ret < 0 && ret >= -MAX_ERRNO;
}

// Why counting keeps a call whole: CALL_KEPT_FAILED, CALL_KEPT_SLOW, or 0.
static __always_inline u8 exception(struct config *settings, u64 entered, u64 exited, s64 ret,
				    int returned)
{
	if (!(settings->flags & CONFIG_EXCEPTIONS))
		return 0;
	if (returned && failed(ret))
		return CALL_KEPT_FAILED;
	if (entered && exited - entered >= settings->slow_ns)
		return CALL_KEPT_SLOW;
	return 0;
}

// The ABI of the current call: compat when the thread entered through
// int 0x80 or a 32-bit process's entry, x32 by its number's bit. A kernel
// whose BTF lacks the status field leaves the ABI uncertain, not guessed.
static __always_inline u8 abi_of(u64 id, u8 *flags)
{
	struct task_struct *task = bpf_get_current_task_btf();
	if (bpf_core_field_exists(task->thread_info.status)) {
		if (BPF_CORE_READ(task, thread_info.status) & TS_COMPAT)
			return ABI_32;
	} else {
		*flags |= CALL_ABI_UNCERTAIN;
	}
	return (id & X32_SYSCALL_BIT) ? ABI_X32 : ABI_64;
}

static __always_inline void read_args(struct pt_regs *regs, u8 abi, u64 *args)
{
	if (abi == ABI_32) {
		args[0] = BPF_CORE_READ(regs, bx) & 0xffffffff;
		args[1] = BPF_CORE_READ(regs, cx) & 0xffffffff;
		args[2] = BPF_CORE_READ(regs, dx) & 0xffffffff;
		args[3] = BPF_CORE_READ(regs, si) & 0xffffffff;
		args[4] = BPF_CORE_READ(regs, di) & 0xffffffff;
		args[5] = BPF_CORE_READ(regs, bp) & 0xffffffff;
	} else {
		args[0] = BPF_CORE_READ(regs, di);
		args[1] = BPF_CORE_READ(regs, si);
		args[2] = BPF_CORE_READ(regs, dx);
		args[3] = BPF_CORE_READ(regs, r10);
		args[4] = BPF_CORE_READ(regs, r8);
		args[5] = BPF_CORE_READ(regs, r9);
	}
}

static __always_inline u32 slot_of(u8 abi, u32 nr)
{
	return ((u32)abi << 10) | (nr & 1023);
}

// The count an argument's bytes are measured by.
static __always_inline u64 count_of(struct arg_copy *copy, u64 *args, s64 ret)
{
	u32 at = copy->count_arg < 6 ? copy->count_arg : 0;
	u32 value = 0;
	switch (copy->count) {
	case COUNT_ARG:
		return args[at] & 0xffffffffffffULL;
	case COUNT_RETURN:
		return failed(ret) || ret < 0 ? 0 : (u64)ret;
	case COUNT_POINTED:
		if (bpf_probe_read_user(&value, 4, (void *)args[at]))
			return 0;
		return value;
	default:
		return 1;
	}
}

// Copies `wanted` bytes, at most `limit` of them, to `out`, as the ptrace
// engine's reader does: what it took, and whether it took all.
static __always_inline u32 copy_bytes(u8 *out, u64 address, u64 wanted, u32 limit, u8 *state)
{
	u64 take = wanted < limit ? wanted : limit;
	if (take > CHUNK)
		take = CHUNK;
	if (take && bpf_probe_read_user(out, take & (CHUNK * 2 - 1), (void *)address)) {
		*state = CAPTURE_UNREADABLE;
		return 0;
	}
	*state = wanted > take ? CAPTURE_TRUNCATED : CAPTURE_COMPLETE;
	return take;
}

#define STRINGS_UNENDED 0xffffffff
#define STRINGS_UNREADABLE 0xfffffffe

// Where a count of a string array's elements stands.
struct counting {
	u64 address;
	u32 abi, most, result;
};

// Element `i` of a count: 1 once `result` holds the answer, 0 to go on.
static __always_inline long count_step(u32 i, struct counting *counting)
{
	u32 step = counting->abi == ABI_32 ? 4 : 8;
	if (i >= counting->most) {
		counting->result = STRINGS_UNENDED;
		return 1;
	}
	u64 pointer = 0;
	if (bpf_probe_read_user(&pointer, step, (void *)(counting->address + (u64)i * step))) {
		counting->result = i == 0 ? STRINGS_UNREADABLE : STRINGS_UNENDED;
		return 1;
	}
	if (!pointer) {
		counting->result = i;
		return 1;
	}
	return 0;
}

static long count_callback(u32 i, void *context)
{
	return count_step(i, context);
}

// The elements of the string array at `address` before its NULL, counted
// to `most`: their number, STRINGS_UNENDED when its end was not found, or
// STRINGS_UNREADABLE when not even its first pointer could be read. Global
// functions are verified on their own, which keeps a long loop cheap.
__noinline u32 count_strings(u64 address, u32 abi, u32 most)
{
	struct counting counting = {
		.address = address,
		.abi = abi,
		.most = most,
		.result = STRINGS_UNENDED,
	};
	if (loop_helper) {
		bpf_loop(MOST_COUNTED, count_callback, &counting, 0);
	} else {
		for (u32 i = 0; i < MOST_COUNTED; i++)
			if (count_step(i, &counting))
				break;
	}
	return counting.result;
}

// Copies the first `count` elements of the string array at `address` into
// the scratch record from `start`, after the four bytes of its count, each
// cut at `limit` bytes and NUL-terminated: the bytes used, count included.
// Elements that no longer fit in a capture are left out.
__noinline u32 copy_strings(u32 start, u64 address, u32 abi, u32 limit, u32 count)
{
	u32 zero = 0;
	struct scratch *buffer = bpf_map_lookup_elem(&BUFFER, &zero);
	if (!buffer)
		return 4;
	u32 step = abi == ABI_32 ? 4 : 8;
	u32 size = limit + 2;
	if (size > 512)
		size = 512;
	u32 pos = 4;
	for (u32 i = 0; i < MOST_ELEMENTS; i++) {
		if (i >= count || pos > CHUNK - 512)
			break;
		u64 pointer = 0;
		if (bpf_probe_read_user(&pointer, step, (void *)(address + (u64)i * step)))
			break;
		u32 at = bounded(start + pos, SCRATCH - 1);
		if (at > SCRATCH - 512)
			break;
		long read = bpf_probe_read_user_str(buffer->data + at, size, (void *)pointer);
		u32 kept = 0;
		if (read >= size)
			kept = limit + 1 < size ? limit + 1 : size - 1;
		else if (read > 0)
			kept = read - 1;
		kept = forget(buffer, kept, 511);
		buffer->data[(at + kept) & (SCRATCH - 1)] = 0;
		pos += kept + 1;
	}
	return pos;
}

#define IOVECS_UNREADABLE 0xffffffff

// Copies the first elements of the iovec array at `address` into the
// scratch record from `start`, as blobs::push_iovec lays them out: each
// base and length, then up to `limit` of its bytes, while `left` bytes
// remain. `shape` holds the ABI above eight bits of the element count,
// since a BPF call takes five arguments. The bytes used, or
// IOVECS_UNREADABLE when not even the first element could be read.
__noinline u32 copy_iovecs(u32 start, u64 address, u32 shape, u64 left, u32 limit)
{
	u32 abi = shape >> 8;
	u32 count = shape & 0xff;
	u32 zero = 0;
	struct scratch *buffer = bpf_map_lookup_elem(&BUFFER, &zero);
	if (!buffer)
		return 0;
	u32 step = abi == ABI_32 ? 4 : 8;
	u32 most = limit < 512 ? limit : 512;
	u32 pos = 0;
	for (u32 i = 0; i < MOST_IOVECS; i++) {
		if (i >= count || pos > CHUNK - 512 - 20)
			break;
		u64 base = 0, size = 0;
		u64 element = address + (u64)i * step * 2;
		if (bpf_probe_read_user(&base, step, (void *)element) ||
		    bpf_probe_read_user(&size, step, (void *)(element + step)))
			return i == 0 ? IOVECS_UNREADABLE : pos;
		u32 at = bounded(start + pos, SCRATCH - 1);
		if (at > SCRATCH - 512 - 20)
			break;
		u64 want = size < left ? size : left;
		left -= want;
		u32 take = forget(buffer, want < most ? want : most, 511);
		if (take && bpf_probe_read_user(buffer->data + at + 20, take, (void *)base))
			take = 0;
		take = forget(buffer, take, 511);
		__builtin_memcpy(buffer->data + at, &base, 8);
		__builtin_memcpy(buffer->data + at + 8, &size, 8);
		__builtin_memcpy(buffer->data + at + 16, &take, 4);
		pos += 20 + take;
	}
	return pos;
}

// One argument's capture, at `off` in the scratch record: the offset
// after it. A global function, so the verifier checks it once.
__noinline int copy_arg(u32 slot, u32 index, u32 off, u32 phase, s64 ret)
{
	u32 zero = 0;
	struct scratch *buffer = bpf_map_lookup_elem(&BUFFER, &zero);
	struct call_spec *spec = bpf_map_lookup_elem(&CALLS, &slot);
	struct config *settings = config();
	if (!buffer || !spec || !settings || index >= 6)
		return off;
	struct arg_copy copy = spec->args[index];
	if (copy.kind == COPY_NONE || copy.when != phase)
		return off;
	struct call_record *record = (void *)buffer->data;
	u64 args[6];
	__builtin_memcpy(args, record->args, sizeof(args));
	u64 address = args[index];
	u8 abi = record->abi;
	if (!address)
		return off;
	// What a call writes is there only once it succeeded.
	if (phase == READ_EXIT && copy.writes && failed(ret))
		return off;
	if (off >= CAPTURES_END)
		return off;
	u32 at = bounded(off, CAPTURES_END - 1);
	struct capture_header *header = (void *)buffer->data + at;
	u8 *data = (void *)buffer->data + at + sizeof(struct capture_header);
	u8 state = CAPTURE_COMPLETE;
	u32 len = 0;
	u64 wanted = 0;
	switch (copy.kind) {
	case COPY_PATH:
	case COPY_STRING: {
		// One byte past the limit, and its NUL, show a longer string.
		u32 size = copy.limit + 2;
		if (size > CHUNK)
			size = CHUNK;
		long read = bpf_probe_read_user_str(data, size, (void *)address);
		if (read <= 0) {
			state = CAPTURE_UNREADABLE;
		} else if (read >= size) {
			len = copy.limit < CHUNK ? copy.limit : CHUNK;
			state = CAPTURE_TRUNCATED;
			wanted = 0xffffffff;
		} else {
			len = read - 1;
		}
		break;
	}
	case COPY_STRINGS:
	case COPY_STRINGS_COUNTED: {
		// The element count, when the array's end was found, then each
		// element read, NUL-terminated.
		u32 most = MOST_COUNTED;
		if (copy.kind == COPY_STRINGS)
			most = settings->elements < MOST_ELEMENTS ? settings->elements : MOST_ELEMENTS;
		u32 count = count_strings(address, abi, most);
		if (count == STRINGS_UNREADABLE) {
			state = CAPTURE_UNREADABLE;
			break;
		}
		__builtin_memcpy(data, &count, 4);
		len = 4;
		if (copy.kind == COPY_STRINGS)
			len = copy_strings(at + sizeof(struct capture_header), address, abi, copy.limit,
					   count == STRINGS_UNENDED ? most : count);
		if (count == STRINGS_UNENDED) {
			state = CAPTURE_TRUNCATED;
			wanted = 0xffffffff;
		}
		break;
	}
	case COPY_BYTES:
		wanted = count_of(&copy, args, ret);
		len = copy_bytes(data, address, wanted, copy.limit, &state);
		break;
	case COPY_SOCKADDR:
		wanted = copy.count == COUNT_ONE ? copy.size : count_of(&copy, args, ret);
		len = copy_bytes(data, address, wanted, copy.size, &state);
		break;
	case COPY_STRUCT:
		wanted = count_of(&copy, args, ret) * copy.size;
		len = copy_bytes(data, address, wanted, copy.limit, &state);
		break;
	case COPY_FD_SET:
		wanted = (((args[0] & 0xffffffff) + 63) / 64) * 8;
		len = copy_bytes(data, address, wanted, 128, &state);
		break;
	case COPY_IOVEC: {
		u64 count = count_of(&copy, args, 0);
		// What a reading call filled is its result; a write sends all.
		u64 left = copy.writes ? (failed(ret) || ret < 0 ? 0 : (u64)ret) : ~0ULL;
		u32 shown = count < MOST_IOVECS ? count : MOST_IOVECS;
		len = copy_iovecs(at + sizeof(struct capture_header), address, (abi << 8) | shown,
				  left, copy.limit);
		if (len == IOVECS_UNREADABLE) {
			state = CAPTURE_UNREADABLE;
			len = 0;
		} else if (count > MOST_IOVECS) {
			state = CAPTURE_TRUNCATED;
			wanted = 0xffffffff;
		}
		break;
	}
	default:
		return off;
	}
	if (state == CAPTURE_UNREADABLE)
		len = 0;
	if (len > CHUNK)
		len = CHUNK;
	header->arg = index;
	header->when = phase;
	header->state = state;
	header->pad = 0;
	header->len = len;
	header->wanted = wanted > 0xffffffff ? 0xffffffff : (u32)wanted;
	header->pad2 = 0;
	record->captures += 1;
	return off + sizeof(struct capture_header) + ((len + 7) & ~7);
}

// Builds a call record for `entry` in the scratch buffer and sends it;
// nonzero when it was not sent, which the caller counts.
static __always_inline int send_call(u32 tid, u32 tgid, struct entry *entry, u32 kind, u32 phase,
				      s64 ret, u64 exited)
{
	// Exec can change a worker's id even when loading the new image fails.
	if (kind == RECORD_CALL && entry->exec_tid && entry->exec_tid != tid) {
		struct handoff_record *handoff = reserve(sizeof(*handoff));
		if (handoff) {
			handoff->header.kind = RECORD_HANDOFF;
			handoff->header.size = sizeof(*handoff);
			handoff->header.tid = tid;
			handoff->header.tgid = tgid;
			handoff->entered = entry->entered;
			handoff->former = entry->exec_tid;
			handoff->pad = 0;
			bpf_ringbuf_submit(handoff, 0);
		} else {
			lost();
		}
	}
	u32 zero = 0;
	struct scratch *buffer = bpf_map_lookup_elem(&BUFFER, &zero);
	if (!buffer)
		return -1;
	struct call_record *record = (void *)buffer->data;
	record->header.kind = kind;
	record->header.tid = tid;
	record->header.tgid = tgid;
	record->entered = entry->entered;
	record->exited = exited;
	record->ret = ret;
	__builtin_memcpy(record->args, entry->args, sizeof(entry->args));
	record->nr = entry->nr;
	record->abi = entry->abi;
	record->flags = entry->flags;
	record->captures = 0;
	u32 slot = slot_of(entry->abi, entry->nr);
	u32 off = sizeof(struct call_record);
	if (phase <= READ_EXIT) {
		for (u32 index = 0; index < 6; index++)
			off = copy_arg(slot, index, off, phase, ret);
	}
	if (off > SCRATCH)
		off = SCRATCH;
	record->header.size = off;
	return emit(buffer->data, off);
}

// The statistics thread `tid` counts into: its own, or once THREAD_STATS
// is full, every such thread's, with `tid` and `tgid` cleared.
static __always_inline struct stat_value *stats_of(u32 *tid, u32 *tgid, u32 row)
{
	struct stat_key key = { .tid = *tid, .tgid = *tgid, .row = row };
	struct stat_value *value = bpf_map_lookup_elem(&THREAD_STATS, &key);
	if (value)
		return value;
	u32 zero = 0;
	struct stat_value *blank = bpf_map_lookup_elem(&BLANK, &zero);
	if (blank && !bpf_map_update_elem(&THREAD_STATS, &key, blank, BPF_NOEXIST)) {
		value = bpf_map_lookup_elem(&THREAD_STATS, &key);
		if (value)
			return value;
	}
	*tid = 0;
	*tgid = 0;
	return bpf_map_lookup_elem(&STATS, &row);
}

// Folds one call into its thread's statistics.
static __always_inline void count(struct config *settings, struct call_spec *spec, u32 slot,
				  u32 tid, u32 tgid, u64 entered, u64 exited, s64 ret,
				  int returned)
{
	u32 generation = (settings->flags & CONFIG_GENERATION) ? 1 : 0;
	u32 row = spec->stat + generation * settings->rows;
	struct stat_value *value = stats_of(&tid, &tgid, row);
	if (!value)
		return;
	value->calls += 1;
	if (returned && failed(ret)) {
		value->errors += 1;
		struct errno_key which = {
			.tid = tid,
			.tgid = tgid,
			.slot = slot | (generation << 31),
			.err = (u32)-ret,
		};
		u64 one = 1;
		u64 *errors = bpf_map_lookup_elem(&ERRNOS, &which);
		if (errors)
			*errors += 1;
		else
			bpf_map_update_elem(&ERRNOS, &which, &one, BPF_NOEXIST);
	}
	if (entered && exited) {
		u64 duration = exited - entered;
		if (!value->timed || duration < value->min_ns)
			value->min_ns = duration;
		value->timed += 1;
		value->total_ns += duration;
		if (duration > value->max_ns)
			value->max_ns = duration;
		u32 bucket = duration ? 64 - __builtin_clzll(duration) : 0;
		if (bucket > 39)
			bucket = 39;
		value->log2_ns[bucket] += 1;
	}
	if (returned && spec->moves && ret > 0)
		value->bytes += ret;
}

// Counts a call whose record was not sent, when the kernel counts calls.
static __always_inline void unsent(struct config *settings, struct call_spec *spec, u32 slot,
				   u32 tid, u32 tgid, u64 entered, u64 exited, s64 ret,
				   int returned)
{
	if (!(settings->flags & CONFIG_COUNT)) {
		lost();
		return;
	}
	count(settings, spec, slot, tid, tgid, entered, exited, ret, returned);
	lost_as(LOSS_OVERFLOWED);
}

// The longest dentry name a file record carries.
#define NAME_MAX 255

// Describes what descriptor `fd` of the current task refers to, before
// the call record that made it, so that microscope names it as the kernel
// does without reading /proc after the fact.
static __always_inline void send_file(u32 tid, u32 tgid, s32 fd)
{
	struct task_struct *task = bpf_get_current_task_btf();
	struct fdtable *table = BPF_CORE_READ(task, files, fdt);
	if (!table || fd < 0 || (u32)fd >= BPF_CORE_READ(table, max_fds))
		return;
	struct file **fds = BPF_CORE_READ(table, fd);
	struct file *file = 0;
	if (bpf_probe_read_kernel(&file, sizeof(file), &fds[fd]) || !file)
		return;
	u32 zero = 0;
	struct scratch *buffer = bpf_map_lookup_elem(&BUFFER, &zero);
	if (!buffer)
		return;
	struct file_record *record = &buffer->file;
	struct inode *inode = BPF_CORE_READ(file, f_inode);
	record->header.kind = RECORD_FILE;
	record->header.tid = tid;
	record->header.tgid = tgid;
	record->fd = fd;
	record->mode = BPF_CORE_READ(inode, i_mode);
	record->ino = BPF_CORE_READ(inode, i_ino);
	record->magic = BPF_CORE_READ(inode, i_sb, s_magic);
	record->rdev = BPF_CORE_READ(inode, i_rdev);
	unsigned long *cloexec = BPF_CORE_READ(table, close_on_exec);
	unsigned long word = 0;
	bpf_probe_read_kernel(&word, sizeof(word), &cloexec[(u32)fd / 64]);
	record->cloexec = (word >> ((u32)fd % 64)) & 1;
	record->pad = 0;
	long read = bpf_probe_read_kernel_str(buffer->data + sizeof(*record), NAME_MAX + 1,
					      BPF_CORE_READ(file, f_path.dentry, d_name.name));
	u32 len = read > 0 ? read - 1 : 0;
	if (len > NAME_MAX)
		len = NAME_MAX;
	record->len = len;
	u32 size = (sizeof(*record) + len + 7) & ~7;
	if (size > sizeof(*record) + NAME_MAX + 1)
		size = sizeof(*record) + NAME_MAX + 1;
	record->header.size = size;
	output(buffer->data, size);
}

// Sends the current thread's Python frames, when microscope told the walker
// its process's layout, before its stack: whether it did.
static __always_inline int send_python(u32 tid, u32 tgid)
{
	u32 host = bpf_get_current_pid_tgid() >> 32, zero = 0;
	struct py_layout *layout = bpf_map_lookup_elem(&LAYOUTS, &host);
	if (!layout)
		return 0;
	struct py_stack *stack = bpf_map_lookup_elem(&PY_SCRATCH, &zero);
	if (!stack)
		return 0;
	struct py_stack_record *header = &stack->header;
	header->header.kind = RECORD_PY_STACK;
	header->header.tid = tid;
	header->header.tgid = tgid;
	header->pad = 0;
	u64 *known = bpf_task_storage_get(&PY_STATES, bpf_get_current_task_btf(), 0,
					  BPF_LOCAL_STORAGE_GET_F_CREATE);
	py_walk(layout, stack, known ? *known : 0);
	if (known)
		*known = header->state;
	u32 frames = header->frames;
	if (frames > PY_FRAMES)
		frames = PY_FRAMES;
	u64 size = sizeof(struct py_stack_record) + (u64)frames * sizeof(struct py_frame);
	header->header.size = size;
	if (size > sizeof(struct py_stack))
		return 0;
	return !output(stack, size);
}

// Sends the current thread's JavaScript frames, from the frame pointer and
// instruction `regs` holds, when microscope told the walker its process's
// V8 layout, before its stack: whether it did.
static __always_inline int send_v8(u32 tid, u32 tgid, struct pt_regs *regs)
{
	u32 host = bpf_get_current_pid_tgid() >> 32, zero = 0;
	struct v8_layout *layout = bpf_map_lookup_elem(&V8_LAYOUTS, &host);
	if (!layout)
		return 0;
	struct v8_stack *stack = bpf_map_lookup_elem(&V8_SCRATCH, &zero);
	if (!stack)
		return 0;
	struct v8_stack_record *header = &stack->header;
	header->header.kind = RECORD_V8_STACK;
	header->header.tid = tid;
	header->header.tgid = tgid;
	header->isolate_read = V8_ISOLATE_NOT_READ;
	header->pad[0] = header->pad[1] = 0;
	header->isolate = 0;
	v8_walk(layout, stack, BPF_CORE_READ(regs, bp), BPF_CORE_READ(regs, ip));
	u32 frames = header->frames;
	if (frames > V8_FRAMES)
		frames = V8_FRAMES;
	u64 size = sizeof(struct v8_stack_record) + (u64)frames * sizeof(struct v8_frame);
	header->header.size = size;
	if (size > sizeof(struct v8_stack))
		return 0;
	return !output(stack, size);
}

// Sends the stack of the current thread, whose user registers `regs`
// holds, before the call or signal record that follows: the kernel's
// walk of its frame pointers, and with CONFIG_STACK_COPY, a copy of its
// top, as far as it reads. Fails, having counted the loss, when the ring
// has no room for it; the record it precedes then says so, so that the
// reader shows it without a stack rather than with the thread's last one.
//
// The thread's interpreted frames go before it, and it says whether they
// did: when they did not fit, the reader drops any it holds of the thread's,
// which are of a stack that did not fit.
//
// A stack taken as a waiting call begins, at `entered`, carries that time:
// the call can return on another CPU, whose ring this stack is not in.
static __always_inline int send_stack(void *ctx, u32 tid, u32 tgid, struct pt_regs *regs,
				      u32 flags, u64 entered)
{
	u32 interpreted = send_python(tid, tgid) | send_v8(tid, tgid, regs);
	u32 zero = 0;
	struct scratch *buffer = bpf_map_lookup_elem(&BUFFER, &zero);
	if (!buffer) {
		lost();
		return -1;
	}
	struct stack_record *record = &buffer->stack;
	record->header.kind = RECORD_STACK;
	record->header.tid = tid;
	record->header.tgid = tgid;
	record->entered = entered;
	record->interpreted = interpreted;
	record->pad = 0;
	record->regs[0] = BPF_CORE_READ(regs, ax);
	record->regs[1] = BPF_CORE_READ(regs, dx);
	record->regs[2] = BPF_CORE_READ(regs, cx);
	record->regs[3] = BPF_CORE_READ(regs, bx);
	record->regs[4] = BPF_CORE_READ(regs, si);
	record->regs[5] = BPF_CORE_READ(regs, di);
	record->regs[6] = BPF_CORE_READ(regs, bp);
	record->regs[7] = BPF_CORE_READ(regs, sp);
	record->regs[8] = BPF_CORE_READ(regs, r8);
	record->regs[9] = BPF_CORE_READ(regs, r9);
	record->regs[10] = BPF_CORE_READ(regs, r10);
	record->regs[11] = BPF_CORE_READ(regs, r11);
	record->regs[12] = BPF_CORE_READ(regs, r12);
	record->regs[13] = BPF_CORE_READ(regs, r13);
	record->regs[14] = BPF_CORE_READ(regs, r14);
	record->regs[15] = BPF_CORE_READ(regs, r15);
	record->regs[16] = BPF_CORE_READ(regs, ip);
	long walked = bpf_get_stack(ctx, buffer->data + sizeof(*record), STACK_FRAMES * 8,
				    BPF_F_USER_STACK);
	u32 frames = walked > 0 ? (u32)walked / 8 : 0;
	if (frames > STACK_FRAMES)
		frames = STACK_FRAMES;
	record->frames = frames;
	u32 copied = 0;
	if (flags & CONFIG_STACK_COPY) {
		u32 base = sizeof(*record) + bounded(frames * 8, 1023);
		u64 sp = record->regs[7];
		for (u32 chunk = 0; chunk < STACK_CHUNKS; chunk++) {
			if (bpf_probe_read_user(buffer->data + base + chunk * STACK_CHUNK, STACK_CHUNK,
						(void *)(sp + chunk * STACK_CHUNK)))
				break;
			copied += STACK_CHUNK;
		}
	}
	record->copied = copied;
	u32 size = sizeof(*record) + frames * 8 + copied;
	record->header.size = size;
	if (output(buffer->data, bounded(size, 16383)))
		return -1;
	return 0;
}

// Whether a call waits, as its spec says, from its arguments.
static __always_inline int waits(struct call_spec *spec, u64 *args)
{
	if (spec->waits != WAITS_BY_FUTEX_OP)
		return spec->waits;
	// Less FUTEX_PRIVATE_FLAG and FUTEX_CLOCK_REALTIME: FUTEX_WAIT,
	// FUTEX_LOCK_PI, FUTEX_WAIT_BITSET, FUTEX_WAIT_REQUEUE_PI, and
	// FUTEX_LOCK_PI2.
	u32 op = args[1] & ~(u32)(128 | 256);
	return op == 0 || op == 6 || op == 9 || op == 11 || op == 13;
}

// Describes the descriptors a call made, as its spec says.
static __always_inline void send_files(u32 tid, u32 tgid, struct call_spec *spec,
				       struct entry *entry, s64 ret)
{
	if (spec->makes == MAKES_RESULT && ret >= 0) {
		send_file(tid, tgid, ret);
	} else if (spec->makes == MAKES_PAIR && ret == 0 && spec->makes_arg < 6) {
		s32 pair[2] = {};
		u64 address = entry->args[spec->makes_arg & 7];
		if (bpf_probe_read_user(pair, sizeof(pair), (void *)address))
			return;
		send_file(tid, tgid, pair[0]);
		send_file(tid, tgid, pair[1]);
	}
}

// Marks `task` known to the trace (struct entry's `known`).
static __always_inline void know(struct task_struct *task)
{
	struct entry *held = bpf_task_storage_get(&ENTRIES, task, 0,
						  BPF_LOCAL_STORAGE_GET_F_CREATE);
	if (held)
		held->known = 1;
}

// A call's entry, with the program's context for its stack, the registers
// it entered with, and its number.
static __always_inline int on_enter(void *ctx, struct pt_regs *regs, u64 id, int ending)
{
#ifdef MICROSCOPE_FAULTS
	// Test builds only: the kernel skips this program for a getppid whose
	// first argument is FAULT_SKIP_ENTRY, as it does when it finds the
	// program already running on this CPU.
	if (id == 110 && BPF_CORE_READ(regs, di) == FAULT_SKIP_ENTRY)
		return 0;
#endif
	u64 ids = bpf_get_current_pid_tgid();
	u32 tgid = ids >> 32, tid = (u32)ids;
	if (!traced(tgid))
		return 0;
	struct entry entry = {};
	entry.abi = abi_of(id, &entry.flags);
	entry.nr = entry.abi == ABI_X32 ? (id & ~X32_SYSCALL_BIT) : id;
	u32 slot = slot_of(entry.abi, entry.nr);
	struct call_spec *spec = bpf_map_lookup_elem(&CALLS, &slot);
	if (!spec || !spec->selected || spec->ends != ending)
		return 0;
	struct config *settings = config();
	if (!settings)
		return 0;
	// Counting needs only when the call began, unless it keeps the
	// calls that fail or are slow.
	if (settings->flags & (CONFIG_STREAM | CONFIG_EXCEPTIONS))
		read_args(regs, entry.abi, entry.args);
	entry.tgid = tgid;
	entry.exec_tid = spec->execs ? tid : 0;
	entry.known = 1;
	entry.entered = bpf_ktime_get_ns();
	if ((settings->flags & CONFIG_STREAM) && !sampled_in(settings->sample))
		entry.flags |= CALL_SAMPLED_OUT;
	struct entry *held = bpf_task_storage_get(&ENTRIES, bpf_get_current_task_btf(), 0,
						  BPF_LOCAL_STORAGE_GET_F_CREATE);
	if (!held) {
		lost();
		return 0;
	}
	// The thread's last call never had its return seen: a function's exit
	// programs are skipped for calls in progress when another tracer
	// attaches to it.
	if (held->entered)
		lost_as(LOSS_UNSEEN);
	*held = entry;
	if (!sent(settings, entry.flags))
		return 0;
	// Taken now, a waiting call's stack costs the thread while it waits,
	// not once it is woken, as one taken at its exit would. The calls
	// that end their thread never wait, and their program leaves this out.
	if (!ending && (settings->flags & CONFIG_STACKS) && waits(spec, entry.args)) {
		held->stacked = 1;
		if (send_stack(ctx, tid, tgid, regs, settings->flags, entry.entered))
			held->flags |= CALL_STACK_LOST;
	}
	// Arguments the old image holds, as execve's, are read now.
	for (u32 index = 0; index < 6; index++) {
		if (spec->args[index].kind != COPY_NONE && spec->args[index].when == READ_ENTRY) {
			if (send_call(tid, tgid, &entry, RECORD_ENTRY, READ_ENTRY, 0, 0))
				lost();
			break;
		}
	}
	return 0;
}

// A call's exit, with the program's context for its stack.
static __always_inline int on_exit(void *ctx, struct pt_regs *regs, s64 ret)
{
	u64 ids = bpf_get_current_pid_tgid();
	u32 tgid = ids >> 32, tid = (u32)ids;
	if (!traced(tgid))
		return 0;
	u64 now = bpf_ktime_get_ns();
	struct config *settings = config();
	if (!settings)
		return 0;
	struct entry entry = {};
	struct call_spec *spec;
	// A held entry whose time is zero has already ended. Asked to create
	// one, the kernel fails only when it cannot take its per-CPU task
	// storage lock, as when another program holds it on this CPU: the
	// entry is there but unread, so the call is lost, not unseen.
	struct entry *found = bpf_task_storage_get(&ENTRIES, bpf_get_current_task_btf(), 0,
						   BPF_LOCAL_STORAGE_GET_F_CREATE);
	if (!found) {
		lost();
		return 0;
	}
	u8 known = found->known;
	if (!found->entered)
		found = 0;
	if (found) {
		entry = *found;
		found->entered = 0;
		u32 slot = slot_of(entry.abi, entry.nr);
		spec = bpf_map_lookup_elem(&CALLS, &slot);
	} else {
		// A call that is not chosen, or one that began before tracing
		// did, which a launched program's never do but microscope's own.
		// A thread the trace knows began this call while traced: the
		// kernel skipped its entry, finding the entry program already
		// running on its CPU, and the call is shown without it.
		if ((settings->flags & CONFIG_SKIP_UNSEEN) && !known)
			return 0;
		u64 id = BPF_CORE_READ(regs, orig_ax);
		entry.flags = CALL_ENTRY_UNSEEN;
		entry.abi = abi_of(id, &entry.flags);
		entry.nr = entry.abi == ABI_X32 ? (id & ~X32_SYSCALL_BIT) : id;
		u32 slot = slot_of(entry.abi, entry.nr);
		spec = bpf_map_lookup_elem(&CALLS, &slot);
		if (!spec || !spec->selected)
			return 0;
		// A new thread's or child's return from the call that made it.
		if (spec->forks && ret == 0)
			return 0;
		read_args(regs, entry.abi, entry.args);
	}
	if (!spec || !spec->selected)
		return 0;
	u32 zero = 0;
	u64 *runs = bpf_map_lookup_elem(&RUNS, &zero);
	if (runs)
		*runs += 1;
	u32 slot = slot_of(entry.abi, entry.nr);
	int send = sent(settings, entry.flags);
	int counted = (settings->flags & CONFIG_COUNT) && !send;
	if (counted) {
		count(settings, spec, slot, tid, tgid, entry.entered, now, ret, 1);
		u8 kept = exception(settings, entry.entered, now, ret, 1);
		if (kept && admitted(settings, now)) {
			entry.flags |= kept;
			send = 1;
		} else if (kept) {
			lost_as(LOSS_CAPPED);
		}
	}
	if (send) {
		if (spec->makes)
			send_files(tid, tgid, spec, &entry, ret);
		if ((settings->flags & CONFIG_STACKS) && !entry.stacked &&
		    send_stack(ctx, tid, tgid, regs, settings->flags, 0))
			entry.flags |= CALL_STACK_LOST;
		if (send_call(tid, tgid, &entry, RECORD_CALL, READ_EXIT, ret, now)) {
			if (counted)
				lost();
			else
				unsent(settings, spec, slot, tid, tgid, entry.entered, now, ret, 1);
		}
	}
	return 0;
}

// Every call, on every thread of the host: the default.
SEC("tp_btf/sys_enter")
int sys_enter(u64 *ctx)
{
	return on_enter(ctx, (struct pt_regs *)ctx[0], ctx[1], 0);
}

SEC("tp_btf/sys_exit")
int sys_exit(u64 *ctx)
{
	return on_exit(ctx, (struct pt_regs *)ctx[0], ctx[1]);
}

SEC("kprobe")
int terminal_enter(void *ctx)
{
	struct pt_regs *regs = (struct pt_regs *)bpf_task_pt_regs(bpf_get_current_task_btf());
	return on_enter(ctx, regs, BPF_CORE_READ(regs, orig_ax), 1);
}

// Or only the chosen calls' kernel functions, each `__x64_sys_NAME(regs)`
// or its compat twin, which costs calls not chosen nothing. Userspace
// binds each slot to one function as it loads it; the call's number is
// in the registers.
#define FUNCTION_SLOT(n)                                                           \
	SEC("fentry")                                                              \
	int enter_##n(u64 *ctx)                                                    \
	{                                                                          \
		struct pt_regs *regs = (struct pt_regs *)ctx[0];                   \
		return on_enter(ctx, regs, BPF_CORE_READ(regs, orig_ax), 0);       \
	}                                                                          \
	SEC("fexit")                                                               \
	int exit_##n(u64 *ctx)                                                     \
	{                                                                          \
		return on_exit(ctx, (struct pt_regs *)ctx[0], ctx[1]);             \
	}

FUNCTION_SLOT(0) FUNCTION_SLOT(1) FUNCTION_SLOT(2) FUNCTION_SLOT(3)
FUNCTION_SLOT(4) FUNCTION_SLOT(5) FUNCTION_SLOT(6) FUNCTION_SLOT(7)
FUNCTION_SLOT(8) FUNCTION_SLOT(9) FUNCTION_SLOT(10) FUNCTION_SLOT(11)
FUNCTION_SLOT(12) FUNCTION_SLOT(13) FUNCTION_SLOT(14) FUNCTION_SLOT(15)
FUNCTION_SLOT(16) FUNCTION_SLOT(17) FUNCTION_SLOT(18) FUNCTION_SLOT(19)
FUNCTION_SLOT(20) FUNCTION_SLOT(21) FUNCTION_SLOT(22) FUNCTION_SLOT(23)
FUNCTION_SLOT(24) FUNCTION_SLOT(25) FUNCTION_SLOT(26) FUNCTION_SLOT(27)
FUNCTION_SLOT(28) FUNCTION_SLOT(29) FUNCTION_SLOT(30) FUNCTION_SLOT(31)

SEC("tp_btf/sched_process_fork")
int process_fork(u64 *ctx)
{
	struct task_struct *parent = (struct task_struct *)ctx[0];
	struct task_struct *child = (struct task_struct *)ctx[1];
	u32 parent_tgid = BPF_CORE_READ(parent, tgid);
	if (!traced(parent_tgid))
		return 0;
	u32 child_tgid = BPF_CORE_READ(child, tgid);
	struct config *settings = config();
	if (!settings)
		return 0;
	if (child_tgid != parent_tgid && (settings->flags & CONFIG_FOLLOW)) {
		u8 one = 1;
		bpf_map_update_elem(&TARGETS, &child_tgid, &one, BPF_ANY);
	}
	// Traced from its first instruction, the child is known to the trace.
	if (child_tgid == parent_tgid || (settings->flags & CONFIG_FOLLOW))
		know(child);
	// Sent in count mode too: microscope follows which processes live by them.
	struct spawn_record *record = reserve(sizeof(*record));
	if (!record) {
		lost();
		return 0;
	}
	u32 parent_tid = (u32)bpf_get_current_pid_tgid();
	u32 child_tid = BPF_CORE_READ(child, pid);
	record->header.kind = RECORD_SPAWN;
	record->header.size = sizeof(*record);
	record->header.tid = parent_tid;
	record->header.tgid = parent_tgid;
	record->time = bpf_ktime_get_ns();
	record->child = child_tid;
	record->child_tgid = child_tgid;
	bpf_ringbuf_submit(record, 0);
	return 0;
}

SEC("tp_btf/sched_process_exec")
int process_exec(u64 *ctx)
{
	struct task_struct *task = (struct task_struct *)ctx[0];
	struct linux_binprm *binary = (struct linux_binprm *)ctx[2];
	u32 tgid = BPF_CORE_READ(task, tgid);
	if (!traced(tgid))
		return 0;
	// A launched program's first thread is known to the trace from its
	// exec on; the call it was in before, at microscope's gate, is not.
	know(task);
	struct config *settings = config();
	u32 zero = 0;
	struct scratch *buffer = bpf_map_lookup_elem(&BUFFER, &zero);
	if (!settings || !(settings->flags & CONFIG_STREAM) || !buffer)
		return 0;
	struct exec_record *record = (void *)buffer->data;
	u32 tid = BPF_CORE_READ(task, pid);
	record->header.kind = RECORD_EXEC;
	record->header.tid = tid;
	record->header.tgid = tgid;
	record->time = bpf_ktime_get_ns();
	record->pad = 0;
	long read = bpf_probe_read_kernel_str(buffer->data + sizeof(*record), 4096,
					      BPF_CORE_READ(binary, filename));
	u32 len = read > 0 ? read - 1 : 0;
	if (len > 4095)
		len = 4095;
	record->len = len;
	u32 size = (sizeof(*record) + len + 7) & ~7;
	if (size > SCRATCH)
		size = SCRATCH;
	record->header.size = size;
	output(buffer->data, size);
	return 0;
}

static __always_inline void send_exit(u32 tid, u32 tgid, u64 now, s32 code, u32 last)
{
	struct exit_record *record = reserve(sizeof(*record));
	if (!record) {
		lost();
		return;
	}
	record->header.kind = RECORD_EXIT;
	record->header.size = sizeof(*record);
	record->header.tid = tid;
	record->header.tgid = tgid;
	record->time = now;
	record->code = code;
	record->last = last;
	bpf_ringbuf_submit(record, 0);
}

// Kprobes disable preemption before entering BPF's per-CPU recursion guard.
SEC("kprobe/exit_notify")
int process_exit(void *ctx)
{
	struct task_struct *task = bpf_get_current_task_btf();
	u32 tgid = BPF_CORE_READ(task, tgid);
	u32 tid = BPF_CORE_READ(task, pid);
	if (!traced(tgid))
		return 0;
	u64 now = bpf_ktime_get_ns();
	struct config *settings = config();
	if (!settings)
		return 0;
	// A call the thread never returned from, as exit_group.
	struct entry *found = bpf_task_storage_get(&ENTRIES, task, 0, 0);
	if (found && found->entered) {
		struct entry entry = *found;
		found->entered = 0;
		entry.flags |= CALL_PROCESS_EXITED;
		u32 slot = slot_of(entry.abi, entry.nr);
		struct call_spec *spec = bpf_map_lookup_elem(&CALLS, &slot);
		if (spec && spec->selected) {
			if (!sent(settings, entry.flags)) {
				if (settings->flags & CONFIG_COUNT)
					count(settings, spec, slot, tid, tgid, 0, 0, 0, 0);
			} else if (send_call(tid, tgid, &entry, RECORD_CALL, READ_EXIT + 1, 0, 0)) {
				unsent(settings, spec, slot, tid, tgid, 0, 0, 0, 0);
			}
		}
	}
	s32 code = BPF_CORE_READ(task, exit_code);
	// Workers keep their own status; the leader identifies the final process exit.
	if (tid != tgid && (settings->flags & CONFIG_STREAM))
		send_exit(tid, tgid, now, code, 0);
	return 0;
}

SEC("fentry/do_notify_parent")
int process_end(u64 *ctx)
{
	struct task_struct *task = (struct task_struct *)ctx[0];
	u32 host = BPF_CORE_READ(task, tgid);
	if (!bpf_map_lookup_elem(&TARGETS, &host))
		return 0;
	if (task != task->group_leader || BPF_CORE_READ(task, signal, live.counter) != 0 ||
	    BPF_CORE_READ(task, signal, nr_threads) != 1)
		return 0;
	u32 tgid = BPF_CORE_READ(task, tgid), tid = BPF_CORE_READ(task, pid);
	s32 code = BPF_CORE_READ(task, signal, flags) & SIGNAL_GROUP_EXIT
		? BPF_CORE_READ(task, signal, group_exit_code) : BPF_CORE_READ(task, exit_code);
	send_exit(tid, tgid, bpf_ktime_get_ns(), code, 1);
	bpf_map_delete_elem(&TARGETS, &host);
	return 0;
}

SEC("tp_btf/signal_deliver")
int signal_deliver(u64 *ctx)
{
	u64 ids = bpf_get_current_pid_tgid();
	u32 tgid = ids >> 32, tid = (u32)ids;
	if (!traced(tgid))
		return 0;
	s32 signal = (s32)ctx[0];
	void *info = (void *)ctx[1];
	// get_signal reports group-exit wakeups as SIGKILL with SEND_SIG_NOINFO.
	// The exit record, not this synthetic signal, carries the fatal reason.
	if (signal == 9 && !info)
		return 0;
	struct config *settings = config();
	if (!settings || !(settings->flags & CONFIG_STREAM))
		return 0;
	u32 flags = 0;
	if (settings->flags & CONFIG_STACKS) {
		struct pt_regs *regs = (struct pt_regs *)bpf_task_pt_regs(bpf_get_current_task_btf());
		if (send_stack(ctx, tid, tgid, regs, settings->flags, 0))
			flags = CALL_STACK_LOST;
	}
	struct signal_record *record = reserve(sizeof(*record));
	if (!record) {
		lost();
		return 0;
	}
	record->header.kind = RECORD_SIGNAL;
	record->header.size = sizeof(*record);
	record->header.tid = tid;
	record->header.tgid = tgid;
	record->time = bpf_ktime_get_ns();
	record->signal = signal;
	record->flags = flags;
	if (bpf_probe_read_kernel(record->info, sizeof(record->info), info)) {
		__builtin_memset(record->info, 0, sizeof(record->info));
		record->flags |= SIGNAL_INFO_LOST;
	}
	bpf_ringbuf_submit(record, 0);
	return 0;
}

// Lists every traced thread's call in progress, for microscope's in-flight
// scan: one in_flight_record each.
SEC("iter/task")
int in_flight(struct bpf_iter__task *ctx)
{
	struct task_struct *task = ctx->task;
	if (!task)
		return 0;
	// Only a traced thread holds an entry.
	struct config *settings = config();
	if (!settings)
		return 0;
	if (!bpf_map_lookup_elem(&TARGETS, &(u32){BPF_CORE_READ(task, tgid)}))
		return 0;
	struct entry *held = bpf_task_storage_get(&ENTRIES, task, 0, 0);
	if (!held || !held->entered)
		return 0;
	struct in_flight_record record = {};
	record.tid = BPF_CORE_READ(task, pid);
	record.entry = *held;
	bpf_seq_write(ctx->meta->seq, &record, sizeof(record));
	return 0;
}

char LICENSE[] SEC("license") = "GPL";
