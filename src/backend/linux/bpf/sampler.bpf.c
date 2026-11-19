







#include <linux/bpf.h>
#include <linux/bpf_perf_event.h>
#include <bpf/bpf_helpers.h>
#include <bpf/bpf_core_read.h>

#include "python.bpf.h"
#include "v8.bpf.h"

const volatile u32 KERNEL_SAMPLES = 0;


struct kernel_sample_record {
	u64 syscall;
	u32 abi;
	u32 reserved;
};

struct sampled_python {
	struct py_stack stack;
	struct kernel_sample_record kernel;
};

struct sampled_v8 {
	struct v8_stack stack;
	struct kernel_sample_record kernel;
};


struct {
	__uint(type, BPF_MAP_TYPE_HASH);
	__uint(max_entries, 1024);
	__type(key, u32);
	__type(value, struct py_layout);
} LAYOUTS SEC(".maps");


struct {
	__uint(type, BPF_MAP_TYPE_HASH);
	__uint(max_entries, 65536);
	__type(key, u32);
	__type(value, struct py_thread);
} THREADS SEC(".maps");


struct {
	__uint(type, BPF_MAP_TYPE_HASH);
	__uint(max_entries, 1024);
	__type(key, u32);
	__type(value, struct v8_layout);
} V8_LAYOUTS SEC(".maps");


struct {
	__uint(type, BPF_MAP_TYPE_PERCPU_ARRAY);
	__uint(max_entries, 1);
	__type(key, u32);
	__type(value, struct sampled_v8);
} V8_SCRATCH SEC(".maps");



struct {
	__uint(type, BPF_MAP_TYPE_PERF_EVENT_ARRAY);
	__uint(max_entries, 1);
	__type(key, u32);
	__type(value, u32);
} OUTPUT SEC(".maps");


struct {
	__uint(type, BPF_MAP_TYPE_PERCPU_ARRAY);
	__uint(max_entries, 1);
	__type(key, u32);
	__type(value, struct sampled_python);
} SCRATCH SEC(".maps");


struct {
	__uint(type, BPF_MAP_TYPE_PERCPU_ARRAY);
	__uint(max_entries, 1);
	__type(key, u32);
	__type(value, u64);
} LOST SEC(".maps");

struct {
	__uint(type, BPF_MAP_TYPE_ARRAY);
	__uint(max_entries, 1);
	__type(key, u32);
	__type(value, u32);
} DROPPED SEC(".maps");


struct {
	__uint(type, BPF_MAP_TYPE_PERCPU_ARRAY);
	__uint(max_entries, 1);
	__type(key, u32);
	__type(value, u32);
} OWED SEC(".maps");

#define SHARES 64

static __always_inline int kept(void)
{
	u32 zero = 0;
	u32 *dropped = bpf_map_lookup_elem(&DROPPED, &zero);
	u32 *owed = bpf_map_lookup_elem(&OWED, &zero);
	if (!dropped || !owed || !*dropped || *dropped >= SHARES)
		return 1;
	*owed += SHARES - *dropped;
	if (*owed < SHARES)
		return 0;
	*owed -= SHARES;
	return 1;
}

SEC("perf_event")
int thin_sample(struct bpf_perf_event_data *ctx)
{
	return kept();
}

#ifdef MICROSCOPE_WALK_TIMING


struct v8_walk_times {
	u64 walks;
	u64 ns;
	u64 frames;
	u64 most_ns;

	u64 full_walks;
	u64 full_ns;
};

struct {
	__uint(type, BPF_MAP_TYPE_PERCPU_ARRAY);
	__uint(max_entries, 1);
	__type(key, u32);
	__type(value, struct v8_walk_times);
} V8_WALK_TIMES SEC(".maps");
#endif


static __always_inline void lost_record(void)
{
	u32 zero = 0;
	u64 *lost = bpf_map_lookup_elem(&LOST, &zero);
	if (lost)
		*lost += 1;
}

struct pt_regs___sample {
	unsigned long orig_ax;
} __attribute__((preserve_access_index));

struct thread_info___sample {
	u32 status;
} __attribute__((preserve_access_index));

struct task_struct___sample {
	struct thread_info___sample thread_info;
} __attribute__((preserve_access_index));

static __always_inline struct kernel_sample_record kernel_record(struct bpf_perf_event_data *ctx)
{
	struct kernel_sample_record record = { .syscall = ~0ULL, .abi = 3 };
	if (ctx->regs.cs & 3)
		return record;
	struct task_struct___sample *task = (void *)bpf_get_current_task_btf();
	struct pt_regs___sample *regs = (void *)bpf_task_pt_regs((void *)task);
	u32 status = 0;
	if (!regs || bpf_core_read(&record.syscall, sizeof(record.syscall), &regs->orig_ax) ||
	    !bpf_core_field_exists(task->thread_info.status) ||
	    bpf_core_read(&status, sizeof(status), &task->thread_info.status))
		return record;

	record.abi = (status & 2) ? ABI_32 : (record.syscall & (1U << 30)) ? ABI_X32 : ABI_64;
	return record;
}

static __always_inline int send_sample(struct bpf_perf_event_data *ctx, void *data,
				      u64 size, u64 capacity)
{
	if (KERNEL_SAMPLES) {
		if (size > capacity - sizeof(struct kernel_sample_record))
			return 0;
		struct kernel_sample_record record = kernel_record(ctx);
		__builtin_memcpy((u8 *)data + size, &record, sizeof(record));
		size += sizeof(record);
	}
	if (bpf_perf_event_output(ctx, &OUTPUT, BPF_F_CURRENT_CPU, data, size)) {
		lost_record();

		return KERNEL_SAMPLES ? 0 : 1;
	}
	return 1;
}






struct thread_struct___v8 {
	unsigned long fsbase;
} __attribute__((preserve_access_index));

struct task_struct___v8 {
	struct thread_struct___v8 thread;
} __attribute__((preserve_access_index));





static __always_inline void v8_isolate(const struct v8_layout *layout,
				       struct v8_stack_record *header)
{
	header->isolate_read = V8_ISOLATE_NOT_READ;
	header->isolate = 0;
	if (layout->isolate_place != V8_ISOLATE_OFFSET && layout->isolate_place != V8_ISOLATE_SLOT)
		return;
	header->isolate_read = V8_ISOLATE_UNREADABLE;
	struct task_struct___v8 *task = (void *)bpf_get_current_task_btf();
	u64 fsbase = 0;
	if (bpf_core_read(&fsbase, sizeof(fsbase), &task->thread.fsbase) || !fsbase)
		return;
	s64 offset = layout->isolate_offset;


	if (layout->isolate_place == V8_ISOLATE_SLOT &&
	    bpf_probe_read_user(&offset, sizeof(offset), (const void *)layout->isolate_slot))
		return;
	u64 isolate = 0;
	if (bpf_probe_read_user(&isolate, sizeof(isolate), (const void *)(fsbase + offset)))
		return;
	header->isolate = isolate;
	header->isolate_read = V8_ISOLATE_READ;
}



static __always_inline int v8_sample(struct bpf_perf_event_data *ctx, u32 tgid, u32 tid)
{
	u32 zero = 0;
	struct v8_layout *layout = bpf_map_lookup_elem(&V8_LAYOUTS, &tgid);
	if (!layout)
		return 0;
	struct sampled_v8 *storage = bpf_map_lookup_elem(&V8_SCRATCH, &zero);
	if (!storage)
		return 1;
	struct v8_stack *stack = &storage->stack;
	struct v8_stack_record *header = &stack->header;
	header->header.kind = RECORD_V8_STACK;
	header->header.tid = tid;
	header->header.tgid = tgid;
	header->pad[0] = header->pad[1] = 0;
	v8_isolate(layout, header);



	struct pt_regs regs = ctx->regs;
	if (!(regs.cs & 3)) {
		void *user = (void *)bpf_task_pt_regs(bpf_get_current_task_btf());
		if (!user || bpf_probe_read_kernel(&regs, sizeof(regs), user))
			regs.rbp = regs.rip = 0;
	}
#ifdef MICROSCOPE_WALK_TIMING
	u64 began = bpf_ktime_get_ns();
#endif
	v8_walk(layout, stack, regs.rbp, regs.rip);
#ifdef MICROSCOPE_WALK_TIMING
	u64 took = bpf_ktime_get_ns() - began;
	struct v8_walk_times *times = bpf_map_lookup_elem(&V8_WALK_TIMES, &zero);
	if (times) {
		times->walks += 1;
		times->ns += took;
		times->frames += header->frames;
		if (took > times->most_ns)
			times->most_ns = took;
		if (header->frames >= V8_FRAMES) {
			times->full_walks += 1;
			times->full_ns += took;
		}
	}
#endif

	u32 frames = header->frames;
	if (frames > V8_FRAMES)
		frames = V8_FRAMES;
	u64 size = sizeof(struct v8_stack_record) + (u64)frames * sizeof(struct v8_frame);
	header->header.size = size;
	if (size > sizeof(struct v8_stack))
		return 1;
	return send_sample(ctx, storage, size, sizeof(*storage)) ? 1 : 2;
}

SEC("perf_event")
int python_sample(struct bpf_perf_event_data *ctx)
{
	if (!kept())
		return 0;
	u32 zero = 0;
	struct sampled_python *storage = bpf_map_lookup_elem(&SCRATCH, &zero);
	if (!storage)
		return 1;
	struct py_stack *stack = &storage->stack;

	u64 ids = bpf_get_current_pid_tgid();
	u32 tgid = ids >> 32, tid = (u32)ids;
	int v8 = v8_sample(ctx, tgid, tid);
	if (v8)
		return v8 == 1;
	struct py_stack_record *header = &stack->header;
	header->header.kind = RECORD_PY_STACK;
	header->header.tid = tid;
	header->header.tgid = tgid;
	header->state = 0;
	header->running_task = 0;
	header->frames = 0;
	header->gil = PY_GIL_UNKNOWN;
	header->pad = 0;

	struct py_layout *layout = bpf_map_lookup_elem(&LAYOUTS, &tgid);
	struct py_thread *thread = bpf_map_lookup_elem(&THREADS, &tid);
	if (!layout)
		header->end = PY_END_NO_LAYOUT;
	else
		py_walk(layout, stack, thread ? thread->state : 0);

	u32 frames = header->frames;
	if (frames > PY_FRAMES)
		frames = PY_FRAMES;
	u64 size = sizeof(struct py_stack_record) + (u64)frames * sizeof(struct py_frame);
	header->header.size = size;
	if (size > sizeof(struct py_stack))
		return 1;
	return send_sample(ctx, storage, size, sizeof(*storage));
}

char LICENSE[] SEC("license") = "GPL";
