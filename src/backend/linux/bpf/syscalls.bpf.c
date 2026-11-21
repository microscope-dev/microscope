










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
#define PID_LIMIT 4194304

#define SCRATCH 32768



#define CAPTURES_END 16384

#define CHUNK 4096
#define MOST_ELEMENTS 64
#define MOST_IOVECS 16
#define MOST_COUNTED 4096
#define MAX_ERRNO 4095

#define STACK_FRAMES 127

#define STACK_CHUNK 512
#define STACK_CHUNKS 16

#ifdef MICROSCOPE_FAULTS


#define FAULT_SKIP_ENTRY 0x5ca1ab1e

#define FAULT_SKIP_EXIT 0x5ca1ab1f

#define FAULT_INTERLEAVE 0x5ca1ab20
#endif

#define TARGET_WORDS (PID_LIMIT / 64)

struct {
	__uint(type, BPF_MAP_TYPE_ARRAY);
	__uint(max_entries, TARGET_WORDS);
	__type(key, u32);
	__type(value, u64);
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




struct {
	__uint(type, BPF_MAP_TYPE_TASK_STORAGE);
	__uint(map_flags, BPF_F_NO_PREALLOC);
	__type(key, int);
	__type(value, struct entry);
} ENTRIES SEC(".maps");




struct {
	__uint(type, BPF_MAP_TYPE_TASK_STORAGE);
	__uint(map_flags, BPF_F_NO_PREALLOC);
	__type(key, int);
	__type(value, u64);
} PY_STATES SEC(".maps");



struct {
	__uint(type, BPF_MAP_TYPE_HASH);
	__uint(max_entries, 8192);
	__type(key, struct stat_key);
	__type(value, struct stat_value);
} THREAD_STATS SEC(".maps");




struct {
	__uint(type, BPF_MAP_TYPE_PERCPU_ARRAY);
	__uint(max_entries, 1);
	__type(key, u32);
	__type(value, struct stat_value);
} STATS SEC(".maps");


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
	__uint(type, BPF_MAP_TYPE_LRU_HASH);
	__uint(max_entries, 4096);
	__type(key, u32);
	__type(value, u64);
} KEPT SEC(".maps");

// A launched program is traced from its exec. Until then the process is still
// the launcher's, between fork and exec, and only its execve is the program's.
struct {
	__uint(type, BPF_MAP_TYPE_ARRAY);
	__uint(max_entries, 1);
	__type(key, u32);
	__type(value, u32);
} UNEXECED SEC(".maps");

struct {
	__uint(type, BPF_MAP_TYPE_LRU_HASH);
	__uint(max_entries, 65536);
	__type(key, struct example_key);
	__type(value, u64);
} EXAMPLES SEC(".maps");




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



struct {
	__uint(type, BPF_MAP_TYPE_PERCPU_ARRAY);
	__uint(max_entries, 1);
	__type(key, u32);
	__type(value, u64);
} RUNS SEC(".maps");


struct {
	__uint(type, BPF_MAP_TYPE_PERCPU_ARRAY);
	__uint(max_entries, 1);
	__type(key, u32);
	__type(value, u64);
} SAMPLED SEC(".maps");


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



static __always_inline int sent(struct config *settings, u8 flags)
{
	return (settings->flags & CONFIG_STREAM) && !(flags & CALL_SAMPLED_OUT);
}





#define SCRATCHES 4



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
	__uint(max_entries, SCRATCHES);
	__type(key, u32);
	__type(value, struct scratch);
} BUFFER SEC(".maps");


struct claims {
	u32 busy[SCRATCHES];
};

struct {
	__uint(type, BPF_MAP_TYPE_PERCPU_ARRAY);
	__uint(max_entries, 1);
	__type(key, u32);
	__type(value, struct claims);
} CLAIMS SEC(".maps");


struct ring {
	__uint(type, BPF_MAP_TYPE_RINGBUF);
	__uint(max_entries, 4096);
};




struct {
	__uint(type, BPF_MAP_TYPE_ARRAY_OF_MAPS);
	__uint(max_entries, 1);
	__type(key, u32);
	__array(values, struct ring);
} EVENTS SEC(".maps");



struct {
	__uint(type, BPF_MAP_TYPE_HASH);
	__uint(max_entries, 1024);
	__type(key, u32);
	__type(value, struct py_layout);
} LAYOUTS SEC(".maps");


struct {
	__uint(type, BPF_MAP_TYPE_PERCPU_ARRAY);
	__uint(max_entries, SCRATCHES);
	__type(key, u32);
	__type(value, struct py_stack);
} PY_SCRATCH SEC(".maps");



struct {
	__uint(type, BPF_MAP_TYPE_HASH);
	__uint(max_entries, 1024);
	__type(key, u32);
	__type(value, struct v8_layout);
} V8_LAYOUTS SEC(".maps");


struct {
	__uint(type, BPF_MAP_TYPE_PERCPU_ARRAY);
	__uint(max_entries, SCRATCHES);
	__type(key, u32);
	__type(value, struct v8_stack);
} V8_SCRATCH SEC(".maps");

enum tail_slot {
	TAIL_WALK_STACK = 0,
	TAIL_EXIT_FINISH = 1,
	TAIL_SLOTS = 2,
};

enum handoff_caller {
	CALLER_ENTER = 0,
	CALLER_EXIT = 1,
	CALLER_SIGNAL = 2,
};

struct stack_handoff {
	u64 entered;
	u64 now;
	s64 ret;
	u64 info;
	u32 tid;
	u32 tgid;
	u32 flags;
	u32 slot;
	s32 signal;
	u8 caller;
	u8 counted;
	u8 pad[2];
	struct entry entry;
};

struct {
	__uint(type, BPF_MAP_TYPE_PROG_ARRAY);
	__uint(max_entries, TAIL_SLOTS);
	__type(key, u32);
	__type(value, u32);
} TAIL_CALLS SEC(".maps");

struct {
	__uint(type, BPF_MAP_TYPE_PERCPU_ARRAY);
	__uint(max_entries, 1);
	__type(key, u32);
	__type(value, struct stack_handoff);
} STACK_HANDOFF SEC(".maps");






static __always_inline u32 claim(void)
{
	u32 zero = 0;
	struct claims *claims = bpf_map_lookup_elem(&CLAIMS, &zero);
	if (!claims)
		return SCRATCHES;
	for (u32 which = 0; which < SCRATCHES; which++)
		if (!__sync_lock_test_and_set(&claims->busy[which], which + 1))
			return *(volatile u32 *)&claims->busy[which] - 1;
	return SCRATCHES;
}

static __always_inline void release(u32 which)
{
	u32 zero = 0;
	struct claims *claims = bpf_map_lookup_elem(&CLAIMS, &zero);
	if (claims && which < SCRATCHES)
		*(volatile u32 *)&claims->busy[which] = 0;
}


static __always_inline struct scratch *scratch(u32 which)
{
	return bpf_map_lookup_elem(&BUFFER, &which);
}



#define PLACE(which, offset) (((which) << 16) | (offset))



static __always_inline u32 bounded(u32 value, u32 mask)
{
	asm volatile("" : "+r"(value));
	return value & mask;
}


#define SPARE (SCRATCH - 8)





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


static __always_inline u64 *target_word(u32 tgid)
{
	u32 word = (tgid / 64) & (TARGET_WORDS - 1);
	return bpf_map_lookup_elem(&TARGETS, &word);
}

static __always_inline int traced(u32 tgid)
{
	u64 *word = target_word(tgid);
	return word && (*word >> (tgid % 64)) & 1;
}

static __always_inline void set_traced(u32 tgid)
{
	u64 *word = target_word(tgid);
	if (word)
		__sync_fetch_and_or(word, 1ULL << (tgid % 64));
}

static __always_inline void clear_traced(u32 tgid)
{
	u64 *word = target_word(tgid);
	if (word)
		__sync_fetch_and_and(word, ~(1ULL << (tgid % 64)));
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

#define WAKE_SHIFT 2

static __always_inline u64 wakeup(void *events, u64 size, u64 otherwise)
{
	u64 held = bpf_ringbuf_query(events, BPF_RB_AVAIL_DATA);
	u64 mark = bpf_ringbuf_query(events, BPF_RB_RING_SIZE) >> WAKE_SHIFT;
	return held <= mark && held + size > mark ? BPF_RB_FORCE_WAKEUP : otherwise;
}

static __always_inline void *reserve(u64 size, u64 otherwise, u64 *wake)
{
	void *events = ring();
	if (!events)
		return 0;
	*wake = wakeup(events, size, otherwise);
	return bpf_ringbuf_reserve(events, size, 0);
}


static __always_inline int output(void *data, u64 size)
{
	void *events = ring();
	if (events && !bpf_ringbuf_output(events, data, size, wakeup(events, size, BPF_RB_NO_WAKEUP)))
		return 0;
	lost();
	return -1;
}


static __always_inline int emit(void *data, u64 size)
{
	void *events = ring();
	return events ? bpf_ringbuf_output(events, data, size, wakeup(events, size, BPF_RB_NO_WAKEUP)) : -1;
}




static __always_inline u64 *counter_in(void *map, void *key)
{
	u64 *counter = bpf_map_lookup_elem(map, key);
	if (counter)
		return counter;
	u64 zero = 0;
	bpf_map_update_elem(map, key, &zero, BPF_NOEXIST);
	return bpf_map_lookup_elem(map, key);
}




static __always_inline int counted_in(u64 *counter, u64 period, u32 most)
{
	for (int tries = 0; tries < 8; tries++) {
		u64 seen = *(volatile u64 *)counter;
		u64 count = seen >> 32 == period ? seen & 0xffffffffULL : 0;
		if (count >= most)
			return 0;
		if (__sync_val_compare_and_swap(counter, seen, period << 32 | (count + 1)) == seen)
			return 1;
	}
	return 0;
}

static __always_inline int admitted(struct config *settings, u32 tgid, u64 now)
{
	if (!settings->cap)
		return 1;
	u64 *kept = counter_in(&KEPT, &tgid);
	return kept && counted_in(kept, (now / 1000000000ULL) & 0xffffffffULL, settings->cap);
}

static __always_inline void streamed(struct config *settings, u32 tgid, struct entry *entry,
				     u64 now)
{
	if ((settings->flags & CONFIG_STREAM) && !sampled_in(settings->sample))
		entry->flags |= CALL_SAMPLED_OUT;
	if ((settings->flags & CONFIG_CAP_STREAM) && sent(settings, entry->flags) &&
	    !admitted(settings, tgid, now)) {
		entry->flags |= CALL_SAMPLED_OUT;
		entry->capped = 1;
		lost_as(LOSS_CAPPED);
	}
}

static __always_inline int exemplary(struct config *settings, u32 tgid, u32 slot, u32 err, u64 now)
{
	if (!settings->examples)
		return 1;
	struct example_key key = { .tgid = tgid, .slot = slot, .err = err };
	u64 *examples = counter_in(&EXAMPLES, &key);
	return examples &&
	       counted_in(examples, (now / 60000000000ULL) & 0xffffffffULL, settings->examples);
}

static __always_inline int failed(s64 ret)
{
	return ret < 0 && ret >= -MAX_ERRNO;
}


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




static __always_inline u8 abi_of(u64 id, u8 *flags)
{
	struct task_struct *task = bpf_get_current_task_btf();
	if (bpf_core_field_exists(task->thread_info.status)) {
		if (task->thread_info.status & TS_COMPAT)
			return ABI_32;
	} else {
		*flags |= CALL_ABI_UNCERTAIN;
	}
	return (id & X32_SYSCALL_BIT) ? ABI_X32 : ABI_64;
}

static __always_inline void read_args(struct pt_regs *regs, u8 abi, u64 *args)
{
	if (abi == ABI_32) {
		args[0] = regs->bx & 0xffffffff;
		args[1] = regs->cx & 0xffffffff;
		args[2] = regs->dx & 0xffffffff;
		args[3] = regs->si & 0xffffffff;
		args[4] = regs->di & 0xffffffff;
		args[5] = regs->bp & 0xffffffff;
	} else {
		args[0] = regs->di;
		args[1] = regs->si;
		args[2] = regs->dx;
		args[3] = regs->r10;
		args[4] = regs->r8;
		args[5] = regs->r9;
	}
}

static __always_inline u32 slot_of(u8 abi, u32 nr)
{
	return ((u32)abi << 10) | (nr & 1023);
}


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


struct counting {
	u64 address;
	u32 abi, most, result;
};


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





__noinline u32 copy_strings(u32 place, u64 address, u32 abi, u32 limit, u32 count)
{
	u32 start = place & 0xffff;
	struct scratch *buffer = scratch(place >> 16);
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







__noinline u32 copy_iovecs(u32 place, u64 address, u32 shape, u64 left, u32 limit)
{
	u32 abi = shape >> 8;
	u32 count = shape & 0xff;
	u32 start = place & 0xffff;
	struct scratch *buffer = scratch(place >> 16);
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



__noinline int copy_arg(u32 slot, u32 index, u32 place, u32 phase, s64 ret)
{
	u32 which = place >> 16;
	u32 off = place & 0xffff;
	struct scratch *buffer = scratch(which);
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
			len = copy_strings(PLACE(which, at + sizeof(struct capture_header)), address,
					   abi, copy.limit, count == STRINGS_UNENDED ? most : count);
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

		u64 left = copy.writes ? (failed(ret) || ret < 0 ? 0 : (u64)ret) : ~0ULL;
		u32 shown = count < MOST_IOVECS ? count : MOST_IOVECS;
		len = copy_iovecs(PLACE(which, at + sizeof(struct capture_header)), address,
				  (abi << 8) | shown, left, copy.limit);
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



static __always_inline int send_call(u32 tid, u32 tgid, struct entry *entry, u32 kind, u32 phase,
				      s64 ret, u64 exited)
{

	if (kind == RECORD_CALL && entry->exec_tid && entry->exec_tid != tid) {
		u64 wake = 0;
		struct handoff_record *handoff = reserve(sizeof(*handoff), BPF_RB_NO_WAKEUP, &wake);
		if (handoff) {
			handoff->header.kind = RECORD_HANDOFF;
			handoff->header.size = sizeof(*handoff);
			handoff->header.tid = tid;
			handoff->header.tgid = tgid;
			handoff->entered = entry->entered;
			handoff->former = entry->exec_tid;
			handoff->pad = 0;
			bpf_ringbuf_submit(handoff, wake);
		} else {
			lost();
		}
	}
	u32 which = claim();
	struct scratch *buffer = scratch(which);
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
			off = copy_arg(slot, index, PLACE(which, off), phase, ret);
	}
	if (off > SCRATCH)
		off = SCRATCH;
#ifdef MICROSCOPE_FAULTS


	if (kind == RECORD_CALL && entry->nr == 121 && entry->args[1] == FAULT_INTERLEAVE) {
		u32 another = claim();
		struct scratch *other = scratch(another);
		if (other)
			other->call.nr = 39;
		release(another);
	}
#endif
	record->header.size = off;
	int unsent = emit(buffer->data, off);
	release(which);
	return unsent;
}



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


#define NAME_MAX 255




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
	u32 which = claim();
	struct scratch *buffer = scratch(which);
	if (!buffer) {
		lost();
		return;
	}
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
	release(which);
}



static __always_inline int send_python(u32 which, u32 tid, u32 tgid)
{
	u32 host = bpf_get_current_pid_tgid() >> 32;
	struct py_layout *layout = bpf_map_lookup_elem(&LAYOUTS, &host);
	if (!layout)
		return 0;
	struct py_stack *stack = bpf_map_lookup_elem(&PY_SCRATCH, &which);
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




static __always_inline int send_v8(u32 which, u32 tid, u32 tgid, struct pt_regs *regs)
{
	u32 host = bpf_get_current_pid_tgid() >> 32;
	struct v8_layout *layout = bpf_map_lookup_elem(&V8_LAYOUTS, &host);
	if (!layout)
		return 0;
	struct v8_stack *stack = bpf_map_lookup_elem(&V8_SCRATCH, &which);
	if (!stack)
		return 0;
	struct v8_stack_record *header = &stack->header;
	header->header.kind = RECORD_V8_STACK;
	header->header.tid = tid;
	header->header.tgid = tgid;
	header->isolate_read = V8_ISOLATE_NOT_READ;
	header->pad[0] = header->pad[1] = 0;
	header->isolate = 0;
	v8_walk(layout, stack, regs->bp, regs->ip);
	u32 frames = header->frames;
	if (frames > V8_FRAMES)
		frames = V8_FRAMES;
	u64 size = sizeof(struct v8_stack_record) + (u64)frames * sizeof(struct v8_frame);
	header->header.size = size;
	if (size > sizeof(struct v8_stack))
		return 0;
	return !output(stack, size);
}














static __always_inline int send_native_stack_with(void *ctx, u32 which, u32 tid, u32 tgid,
						  struct pt_regs *regs, u32 flags, u64 entered,
						  u32 interpreted)
{
	struct scratch *buffer = scratch(which);
	if (!buffer) {
		release(which);
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
	record->regs[0] = regs->ax;
	record->regs[1] = regs->dx;
	record->regs[2] = regs->cx;
	record->regs[3] = regs->bx;
	record->regs[4] = regs->si;
	record->regs[5] = regs->di;
	record->regs[6] = regs->bp;
	record->regs[7] = regs->sp;
	record->regs[8] = regs->r8;
	record->regs[9] = regs->r9;
	record->regs[10] = regs->r10;
	record->regs[11] = regs->r11;
	record->regs[12] = regs->r12;
	record->regs[13] = regs->r13;
	record->regs[14] = regs->r14;
	record->regs[15] = regs->r15;
	record->regs[16] = regs->ip;
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
	int unsent = output(buffer->data, bounded(size, 16383));
	release(which);
	return unsent;
}


static __always_inline int send_stack(void *ctx, u32 tid, u32 tgid, struct pt_regs *regs,
				      u32 flags, u64 entered)
{
	u32 which = claim();
	u32 interpreted = send_python(which, tid, tgid) | send_v8(which, tid, tgid, regs);
	return send_native_stack_with(ctx, which, tid, tgid, regs, flags, entered, interpreted);
}


static __always_inline void send_signal_record(u32 tid, u32 tgid, s32 signal, void *info, u32 flags)
{
	u64 wake = 0;
	struct signal_record *record = reserve(sizeof(*record), 0, &wake);
	if (!record) {
		lost();
		return;
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
	bpf_ringbuf_submit(record, wake);
}


static __always_inline int waits(struct call_spec *spec, u64 *args)
{
	if (spec->waits != WAITS_BY_FUTEX_OP)
		return spec->waits;



	u32 op = args[1] & ~(u32)(128 | 256);
	return op == 0 || op == 6 || op == 9 || op == 11 || op == 13;
}


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


static __always_inline void know(struct task_struct *task, u32 session)
{
	struct entry *held = bpf_task_storage_get(&ENTRIES, task, 0,
						  BPF_LOCAL_STORAGE_GET_F_CREATE);
	if (held) {
		if (held->session != session) {
			held->entered = 0;
			held->session = session;
		}
		held->known = 1;
	}
}




static __always_inline void send_unreturned(struct config *settings, u32 tid, u32 tgid,
					    struct entry *entry)
{
	u32 slot = slot_of(entry->abi, entry->nr);
	struct call_spec *spec = bpf_map_lookup_elem(&CALLS, &slot);
	if (!spec || !spec->selected)
		return;
	if (!sent(settings, entry->flags)) {
		if (settings->flags & CONFIG_COUNT)
			count(settings, spec, slot, tid, tgid, 0, 0, 0, 0);
	} else if (send_call(tid, tgid, entry, RECORD_CALL, READ_EXIT + 1, 0, 0)) {
		unsent(settings, spec, slot, tid, tgid, 0, 0, 0, 0);
	}
}


SEC("raw_tp")
int walk_stack(struct bpf_raw_tracepoint_args *ctx)
{
	u32 zero = 0;
	struct stack_handoff *h = bpf_map_lookup_elem(&STACK_HANDOFF, &zero);
	if (!h)
		return 0;
	struct pt_regs *regs = (struct pt_regs *)bpf_task_pt_regs(bpf_get_current_task_btf());
	if (!regs)
		return 0;
	u32 which = claim();
	u32 interpreted = send_python(which, h->tid, h->tgid) | send_v8(which, h->tid, h->tgid, regs);
	int unsent = send_native_stack_with(ctx, which, h->tid, h->tgid, regs, h->flags, h->entered,
					    interpreted);
	if (h->caller == CALLER_ENTER) {
		if (unsent) {
			struct config *settings = config();
			struct entry *held = bpf_task_storage_get(&ENTRIES,
								  bpf_get_current_task_btf(), 0, 0);
			if (held && settings && held->session == settings->session &&
			    held->entered == h->entered)
				held->flags |= CALL_STACK_LOST;
		}
		return 0;
	}
	if (h->caller == CALLER_SIGNAL) {
		send_signal_record(h->tid, h->tgid, h->signal, (void *)h->info,
				   unsent ? CALL_STACK_LOST : 0);
		return 0;
	}
	if (unsent)
		h->entry.flags |= CALL_STACK_LOST;
	bpf_tail_call(ctx, &TAIL_CALLS, TAIL_EXIT_FINISH);
	lost();
	return 0;
}


SEC("raw_tp")
int exit_finish(struct bpf_raw_tracepoint_args *ctx)
{
	u32 zero = 0;
	struct stack_handoff *h = bpf_map_lookup_elem(&STACK_HANDOFF, &zero);
	if (!h)
		return 0;
	struct config *settings = config();
	if (!settings)
		return 0;
	u32 slot = h->slot;
	struct call_spec *spec = bpf_map_lookup_elem(&CALLS, &slot);
	if (!spec || !spec->selected)
		return 0;
	if (send_call(h->tid, h->tgid, &h->entry, RECORD_CALL, READ_EXIT, h->ret, h->now)) {
		if (h->counted)
			lost();
		else
			unsent(settings, spec, slot, h->tid, h->tgid, h->entry.entered, h->now,
			       h->ret, 1);
	}
	return 0;
}



static __always_inline int on_enter(void *ctx, struct pt_regs *regs, u64 id, int ending, int tail)
{
#ifdef MICROSCOPE_FAULTS



	if (id == 121 && regs->si == FAULT_SKIP_ENTRY)
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
	u32 first = 0;
	u32 *unexeced = bpf_map_lookup_elem(&UNEXECED, &first);
	if (unexeced && *unexeced == tgid && !spec->execs)
		return 0;
	struct config *settings = config();
	if (!settings)
		return 0;


	if (settings->flags & (CONFIG_STREAM | CONFIG_EXCEPTIONS))
		read_args(regs, entry.abi, entry.args);
	entry.tgid = tgid;
	entry.exec_tid = spec->execs ? tid : 0;
	entry.known = 1;
	entry.session = settings->session;
	entry.entered = bpf_ktime_get_ns();
	streamed(settings, tgid, &entry, entry.entered);
	struct entry *held = bpf_task_storage_get(&ENTRIES, bpf_get_current_task_btf(), 0,
						  BPF_LOCAL_STORAGE_GET_F_CREATE);
	if (!held) {
		lost();
		return 0;
	}




	if (held->session == settings->session && held->entered) {
		held->flags |= CALL_EXIT_SKIPPED;
		send_unreturned(settings, tid, tgid, held);
	}
	*held = entry;
	if (!sent(settings, entry.flags))
		return 0;

	if (tail) {
		for (u32 index = 0; index < 6; index++) {
			if (spec->args[index].kind != COPY_NONE &&
			    spec->args[index].when == READ_ENTRY) {
				if (send_call(tid, tgid, &entry, RECORD_ENTRY, READ_ENTRY, 0, 0))
					lost();
				break;
			}
		}
		if (!ending && (settings->flags & CONFIG_STACKS) && waits(spec, entry.args)) {
			held->stacked = 1;
			u32 zero = 0;
			struct stack_handoff *h = bpf_map_lookup_elem(&STACK_HANDOFF, &zero);
			if (h) {
				h->caller = CALLER_ENTER;
				h->tid = tid;
				h->tgid = tgid;
				h->flags = settings->flags;
				h->entered = entry.entered;
				bpf_tail_call(ctx, &TAIL_CALLS, TAIL_WALK_STACK);
			}
			held->flags |= CALL_STACK_LOST;
			lost();
		}
		return 0;
	}

	if (!ending && (settings->flags & CONFIG_STACKS) && waits(spec, entry.args)) {
		held->stacked = 1;
		if (send_stack(ctx, tid, tgid, regs, settings->flags, entry.entered))
			held->flags |= CALL_STACK_LOST;
	}

	for (u32 index = 0; index < 6; index++) {
		if (spec->args[index].kind != COPY_NONE && spec->args[index].when == READ_ENTRY) {
			if (send_call(tid, tgid, &entry, RECORD_ENTRY, READ_ENTRY, 0, 0))
				lost();
			break;
		}
	}
	return 0;
}


static __always_inline int on_exit(void *ctx, struct pt_regs *regs, s64 ret, int tail)
{
#ifdef MICROSCOPE_FAULTS

	if (regs->orig_ax == 121 && regs->si == FAULT_SKIP_EXIT)
		return 0;
#endif
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




	struct entry *found = bpf_task_storage_get(&ENTRIES, bpf_get_current_task_btf(), 0,
						   BPF_LOCAL_STORAGE_GET_F_CREATE);
	if (!found) {
		lost();
		return 0;
	}
	if (found->session != settings->session) {
		found->entered = 0;
		found->known = 0;
		found->session = settings->session;
	}
	u8 known = found->known;
	u64 id = regs->orig_ax;





	if (found->entered && !found->exec_tid && id != (u64)-1 &&
	    found->nr != (u32)(id & ~(u64)X32_SYSCALL_BIT)) {
		found->flags |= CALL_EXIT_SKIPPED;
		send_unreturned(settings, tid, tgid, found);
		found->entered = 0;
	}
	if (!found->entered)
		found = 0;
	if (found) {
		entry = *found;
		found->entered = 0;
		u32 slot = slot_of(entry.abi, entry.nr);
		spec = bpf_map_lookup_elem(&CALLS, &slot);
	} else {





		if ((settings->flags & CONFIG_SKIP_UNSEEN) && !known)
			return 0;
		entry.flags = CALL_ENTRY_UNSEEN;
		entry.abi = abi_of(id, &entry.flags);
		entry.nr = entry.abi == ABI_X32 ? (id & ~X32_SYSCALL_BIT) : id;
		u32 slot = slot_of(entry.abi, entry.nr);
		spec = bpf_map_lookup_elem(&CALLS, &slot);
		if (!spec || !spec->selected)
			return 0;

		if (spec->forks && ret == 0)
			return 0;
		read_args(regs, entry.abi, entry.args);
		streamed(settings, tgid, &entry, now);
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
		u8 kept = entry.capped ? 0 : exception(settings, entry.entered, now, ret, 1);
		u32 err = kept == CALL_KEPT_FAILED ? (u32)-ret : 0;
		if (kept && exemplary(settings, tgid, slot, err, now)) {
			if (admitted(settings, tgid, now)) {
				entry.flags |= kept;
				send = 1;
			} else {
				lost_as(LOSS_CAPPED);
			}
		}
	}
	if (send) {
		if (spec->makes)
			send_files(tid, tgid, spec, &entry, ret);
		if (tail) {
			struct stack_handoff *h = bpf_map_lookup_elem(&STACK_HANDOFF, &zero);
			if (!h) {
				lost();
				return 0;
			}
			h->caller = CALLER_EXIT;
			h->tid = tid;
			h->tgid = tgid;
			h->flags = settings->flags;
			h->entered = 0;
			h->now = now;
			h->ret = ret;
			h->slot = slot;
			h->counted = (u8)counted;
			h->entry = entry;
			if ((settings->flags & CONFIG_STACKS) && !entry.stacked)
				bpf_tail_call(ctx, &TAIL_CALLS, TAIL_WALK_STACK);
			else
				bpf_tail_call(ctx, &TAIL_CALLS, TAIL_EXIT_FINISH);
			lost();
			return 0;
		}
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


SEC("raw_tp/sys_enter")
int sys_enter(struct bpf_raw_tracepoint_args *ctx)
{
	struct pt_regs *regs = (struct pt_regs *)bpf_task_pt_regs(bpf_get_current_task_btf());
	if (!regs)
		return 0;
	return on_enter(ctx, regs, ctx->args[1], 0, 1);
}

SEC("raw_tp/sys_exit")
int sys_exit(struct bpf_raw_tracepoint_args *ctx)
{
	struct pt_regs *regs = (struct pt_regs *)bpf_task_pt_regs(bpf_get_current_task_btf());
	if (!regs)
		return 0;
	return on_exit(ctx, regs, (s64)ctx->args[1], 1);
}

SEC("kprobe")
int terminal_enter(void *ctx)
{
	struct pt_regs *regs = (struct pt_regs *)bpf_task_pt_regs(bpf_get_current_task_btf());
	return on_enter(ctx, regs, regs->orig_ax, 1, 0);
}





#define FUNCTION_SLOT(n)                                                           \
	SEC("fentry")                                                              \
	int enter_##n(u64 *ctx)                                                    \
	{                                                                          \
		struct pt_regs *regs = (struct pt_regs *)ctx[0];                   \
		return on_enter(ctx, regs, regs->orig_ax, 0, 0);                   \
	}                                                                          \
	SEC("fexit")                                                               \
	int exit_##n(u64 *ctx)                                                     \
	{                                                                          \
		return on_exit(ctx, (struct pt_regs *)ctx[0], ctx[1], 0);          \
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
		set_traced(child_tgid);
	}

	if (child_tgid == parent_tgid || (settings->flags & CONFIG_FOLLOW))
		know(child, settings->session);

	u64 wake = 0;
	struct spawn_record *record = reserve(sizeof(*record), 0, &wake);
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
	bpf_ringbuf_submit(record, wake);
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
	u32 first = 0;
	u32 *unexeced = bpf_map_lookup_elem(&UNEXECED, &first);
	if (unexeced && *unexeced == tgid)
		*unexeced = 0;


	struct config *settings = config();
	if (!settings)
		return 0;
	know(task, settings->session);
	if (!(settings->flags & CONFIG_STREAM))
		return 0;
	u32 which = claim();
	struct scratch *buffer = scratch(which);
	if (!buffer) {
		lost();
		return 0;
	}
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
	release(which);
	return 0;
}

static __always_inline void send_exit(u32 tid, u32 tgid, u64 now, s32 code, u32 last)
{
	u64 wake = 0;
	struct exit_record *record = reserve(sizeof(*record), 0, &wake);
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
	bpf_ringbuf_submit(record, wake);
}


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

	struct entry *found = bpf_task_storage_get(&ENTRIES, task, 0, 0);
	if (found && found->session == settings->session && found->entered) {
		found->flags |= CALL_PROCESS_EXITED;
		send_unreturned(settings, tid, tgid, found);
		found->entered = 0;
	}
	s32 code = BPF_CORE_READ(task, exit_code);

	if (tid != tgid && (settings->flags & CONFIG_STREAM))
		send_exit(tid, tgid, now, code, 0);
	return 0;
}

SEC("fentry/do_notify_parent")
int process_end(u64 *ctx)
{
	struct task_struct *task = (struct task_struct *)ctx[0];
	u32 host = BPF_CORE_READ(task, tgid);
	if (!traced(host))
		return 0;
	if (task != task->group_leader || BPF_CORE_READ(task, signal, live.counter) != 0 ||
	    BPF_CORE_READ(task, signal, nr_threads) != 1)
		return 0;
	u32 tgid = BPF_CORE_READ(task, tgid), tid = BPF_CORE_READ(task, pid);
	s32 code = BPF_CORE_READ(task, signal, flags) & SIGNAL_GROUP_EXIT
		? BPF_CORE_READ(task, signal, group_exit_code) : BPF_CORE_READ(task, exit_code);
	send_exit(tid, tgid, bpf_ktime_get_ns(), code, 1);
	clear_traced(host);
	return 0;
}

SEC("raw_tp/signal_deliver")
int signal_deliver(struct bpf_raw_tracepoint_args *ctx)
{
	u64 ids = bpf_get_current_pid_tgid();
	u32 tgid = ids >> 32, tid = (u32)ids;
	if (!traced(tgid))
		return 0;
	s32 signal = (s32)ctx->args[0];
	void *info = (void *)ctx->args[1];


	if (signal == 9 && !info)
		return 0;
	struct config *settings = config();
	if (!settings || !(settings->flags & CONFIG_STREAM))
		return 0;
	if (settings->flags & CONFIG_STACKS) {
		u32 zero = 0;
		struct stack_handoff *h = bpf_map_lookup_elem(&STACK_HANDOFF, &zero);
		if (h) {
			h->caller = CALLER_SIGNAL;
			h->tid = tid;
			h->tgid = tgid;
			h->flags = settings->flags;
			h->entered = 0;
			h->signal = signal;
			h->info = (u64)info;
			bpf_tail_call(ctx, &TAIL_CALLS, TAIL_WALK_STACK);
		}
		send_signal_record(tid, tgid, signal, info, CALL_STACK_LOST);
		return 0;
	}
	send_signal_record(tid, tgid, signal, info, 0);
	return 0;
}



SEC("iter/task")
int in_flight(struct bpf_iter__task *ctx)
{
	struct task_struct *task = ctx->task;
	if (!task)
		return 0;

	struct config *settings = config();
	if (!settings)
		return 0;
	if (!traced(BPF_CORE_READ(task, tgid)))
		return 0;
	struct entry *held = bpf_task_storage_get(&ENTRIES, task, 0, 0);
	if (!held || held->session != settings->session || !held->entered)
		return 0;
	struct in_flight_record record = {};
	record.tid = BPF_CORE_READ(task, pid);
	record.entry = *held;
	bpf_seq_write(ctx->meta->seq, &record, sizeof(record));
	return 0;
}

char LICENSE[] SEC("license") = "GPL";
