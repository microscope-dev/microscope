// The profiler's BPF engine (dev/profiling.md). The
// program runs on the sampler's own perf events, as each takes a sample,
// and returns 1, so that perf still writes the sample as it would. Before
// that, it writes the sampled thread's Python frames to the same CPU's
// ring, through that CPU's event in OUTPUT, which microscope points at the
// ring; microscope pairs the two by thread and CPU, and splices the frames
// into the sample's native stack.

#include <linux/bpf.h>
#include <linux/bpf_perf_event.h>
#include <bpf/bpf_helpers.h>
#include <bpf/bpf_core_read.h>

#include "python.bpf.h"
#include "v8.bpf.h"

// Each Python process's layout, by its process id.
struct {
	__uint(type, BPF_MAP_TYPE_HASH);
	__uint(max_entries, 1024);
	__type(key, u32);
	__type(value, struct py_layout);
} LAYOUTS SEC(".maps");

// Each thread of a Python process that has a state, by its thread id.
struct {
	__uint(type, BPF_MAP_TYPE_HASH);
	__uint(max_entries, 65536);
	__type(key, u32);
	__type(value, struct py_thread);
} THREADS SEC(".maps");

// Each Node process's V8 layout, by its process id.
struct {
	__uint(type, BPF_MAP_TYPE_HASH);
	__uint(max_entries, 1024);
	__type(key, u32);
	__type(value, struct v8_layout);
} V8_LAYOUTS SEC(".maps");

// Where each CPU builds its record of JavaScript frames.
struct {
	__uint(type, BPF_MAP_TYPE_PERCPU_ARRAY);
	__uint(max_entries, 1);
	__type(key, u32);
	__type(value, struct v8_stack);
} V8_SCRATCH SEC(".maps");

// Each CPU's BPF output event, which writes to that CPU's ring; microscope
// sizes it to the CPUs.
struct {
	__uint(type, BPF_MAP_TYPE_PERF_EVENT_ARRAY);
	__uint(max_entries, 1);
	__type(key, u32);
	__type(value, u32);
} OUTPUT SEC(".maps");

// Where each CPU builds its record.
struct {
	__uint(type, BPF_MAP_TYPE_PERCPU_ARRAY);
	__uint(max_entries, 1);
	__type(key, u32);
	__type(value, struct py_stack);
} SCRATCH SEC(".maps");

// Each CPU's records that found no room in its ring.
struct {
	__uint(type, BPF_MAP_TYPE_PERCPU_ARRAY);
	__uint(max_entries, 1);
	__type(key, u32);
	__type(value, u64);
} LOST SEC(".maps");

#ifdef MICROSCOPE_WALK_TIMING
// What each CPU's walks of JavaScript frames took, in a build for measuring
// it alone (the bpf-walk-timing feature): the shipping program has no clock.
struct v8_walk_times {
	u64 walks;
	u64 ns;
	u64 frames;
	u64 most_ns;
	// The walks that filled the record, V8_FRAMES frames each.
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

// Counts a record that found no room in the CPU's ring.
static __always_inline void lost_record(void)
{
	u32 zero = 0;
	u64 *lost = bpf_map_lookup_elem(&LOST, &zero);
	if (lost)
		*lost += 1;
}

// The kernel's record of a thread's registers that the kernel keeps for it
// as it switches tasks, of which only its thread pointer is read. These
// are flavors (CO-RE drops the ___v8), relocated against the running
// kernel's task_struct: kernel_types.h's pt_regs would clash with the
// perf event's.
struct thread_struct___v8 {
	unsigned long fsbase;
} __attribute__((preserve_access_index));

struct task_struct___v8 {
	struct thread_struct___v8 thread;
} __attribute__((preserve_access_index));

// Reads the current thread's isolate into the record `header`, where
// `layout` places it. The thread pointer is the one the kernel set when
// the thread was made, or by arch_prctl, and saves as it switches away:
// glibc never writes it otherwise.
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
	// A slot is read at each sample, as the loader fills it only once the
	// image is mapped, after microscope may have read the layout.
	if (layout->isolate_place == V8_ISOLATE_SLOT &&
	    bpf_probe_read_user(&offset, sizeof(offset), (const void *)layout->isolate_slot))
		return;
	u64 isolate = 0;
	if (bpf_probe_read_user(&isolate, sizeof(isolate), (const void *)(fsbase + offset)))
		return;
	header->isolate = isolate;
	header->isolate_read = V8_ISOLATE_READ;
}

// Writes the sampled thread's JavaScript frames, when its process has a V8
// layout: whether it has.
static __always_inline int v8_sample(struct bpf_perf_event_data *ctx, u32 tgid, u32 tid)
{
	u32 zero = 0;
	struct v8_layout *layout = bpf_map_lookup_elem(&V8_LAYOUTS, &tgid);
	if (!layout)
		return 0;
	struct v8_stack *stack = bpf_map_lookup_elem(&V8_SCRATCH, &zero);
	if (!stack)
		return 1;
	struct v8_stack_record *header = &stack->header;
	header->header.kind = RECORD_V8_STACK;
	header->header.tid = tid;
	header->header.tgid = tgid;
	header->pad[0] = header->pad[1] = 0;
	v8_isolate(layout, header);

	// A sample taken in the kernel holds the kernel's registers; the
	// thread's own are those it entered the kernel with.
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
	if (bpf_perf_event_output(ctx, &OUTPUT, BPF_F_CURRENT_CPU, stack, size))
		lost_record();
	return 1;
}

SEC("perf_event")
int python_sample(struct bpf_perf_event_data *ctx)
{
	u32 zero = 0;
	struct py_stack *stack = bpf_map_lookup_elem(&SCRATCH, &zero);
	if (!stack)
		return 1;

	u64 ids = bpf_get_current_pid_tgid();
	u32 tgid = ids >> 32, tid = (u32)ids;
	if (v8_sample(ctx, tgid, tid))
		return 1;
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
	if (bpf_perf_event_output(ctx, &OUTPUT, BPF_F_CURRENT_CPU, stack, size))
		lost_record();

	return 1;
}

char LICENSE[] SEC("license") = "GPL";
