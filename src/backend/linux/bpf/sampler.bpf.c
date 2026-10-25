// The profiler's BPF engine (plans/python.html, "The BPF engine"). The
// program runs on the sampler's own perf events, as each takes a sample,
// and returns 1, so that perf still writes the sample as it would. Before
// that, it writes the sampled thread's Python frames to the same CPU's
// ring, through that CPU's event in OUTPUT, which microscope points at the
// ring; microscope pairs the two by thread and CPU, and splices the frames
// into the sample's native stack.

#include <linux/bpf.h>
#include <linux/bpf_perf_event.h>
#include <bpf/bpf_helpers.h>

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

// Counts a record that found no room in the CPU's ring.
static __always_inline void lost_record(void)
{
	u32 zero = 0;
	u64 *lost = bpf_map_lookup_elem(&LOST, &zero);
	if (lost)
		*lost += 1;
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
	header->pad[0] = header->pad[1] = header->pad[2] = 0;

	// A sample taken in the kernel holds the kernel's registers; the
	// thread's own are those it entered the kernel with.
	struct pt_regs regs = ctx->regs;
	if (!(regs.cs & 3)) {
		void *user = (void *)bpf_task_pt_regs(bpf_get_current_task_btf());
		if (!user || bpf_probe_read_kernel(&regs, sizeof(regs), user))
			regs.rbp = regs.rip = 0;
	}
	v8_walk(layout, stack, regs.rbp, regs.rip);

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
