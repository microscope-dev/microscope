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

SEC("perf_event")
int python_sample(struct bpf_perf_event_data *ctx)
{
	u32 zero = 0;
	struct py_stack *stack = bpf_map_lookup_elem(&SCRATCH, &zero);
	if (!stack)
		return 1;

	u64 ids = bpf_get_current_pid_tgid();
	u32 tgid = ids >> 32, tid = (u32)ids;
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
	if (bpf_perf_event_output(ctx, &OUTPUT, BPF_F_CURRENT_CPU, stack, size)) {
		u64 *lost = bpf_map_lookup_elem(&LOST, &zero);
		if (lost)
			*lost += 1;
	}

	return 1;
}

char LICENSE[] SEC("license") = "GPL";
