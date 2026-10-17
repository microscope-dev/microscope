// The walker of a thread's CPython frames, which the sampler's program
// runs at each sample (plans/python.html, "The BPF engine"). It reads the
// chain from the thread's innermost frame by each frame's `previous`, and
// writes one py_frame per frame, as py_layout places CPython's fields.
// microscope proves and names each frame's code object later; the walker
// only copies.
//
// The walk is one global function, so the verifier checks it once for
// every program that calls it. It reads only user memory, each read
// failing whole, and ends at a bound.
#pragma once

#include "records.h"

// The most frames one record holds.
#define PY_FRAMES 128

// A record of a thread's Python frames.
struct py_stack {
	struct py_stack_record header;
	struct py_frame frames[PY_FRAMES];
};

// Reads the u64 at `address` into `word`: nonzero when it cannot be read.
static __always_inline int py_word(u64 address, u64 *word)
{
	return bpf_probe_read_user(word, sizeof(*word), (const void *)address);
}

// Reads the C int at `address` into `value`: nonzero when it cannot be read.
static __always_inline int py_int(u64 address, s32 *value)
{
	return bpf_probe_read_user(value, sizeof(*value), (const void *)address);
}

// What the thread whose state is at `state` does with its interpreter's GIL.
static __always_inline u8 py_gil(const struct py_layout *layout, u64 state)
{
	s32 flag = 0;
	if (layout->thread_holds_gil != PY_ABSENT && layout->thread_gil_requested != PY_ABSENT) {
		if (py_int(state + layout->thread_holds_gil, &flag))
			return PY_GIL_UNKNOWN;
		if (flag)
			return PY_GIL_HELD;
		if (py_int(state + layout->thread_gil_requested, &flag))
			return PY_GIL_UNKNOWN;
		return flag ? PY_GIL_WAITING : PY_GIL_RELEASED;
	}

	if (layout->thread_interp == PY_ABSENT || layout->interp_ceval_gil == PY_ABSENT ||
	    layout->gil_locked == PY_ABSENT || layout->gil_holder == PY_ABSENT)
		return PY_GIL_UNKNOWN;

	u64 interp = 0, gil = 0, holder = 0;
	if (py_word(state + layout->thread_interp, &interp) || !interp)
		return PY_GIL_UNKNOWN;
	if (py_word(interp + layout->interp_ceval_gil, &gil) || !gil)
		return PY_GIL_UNKNOWN;
	if (layout->gil_enabled != PY_ABSENT) {
		if (py_int(gil + layout->gil_enabled, &flag))
			return PY_GIL_UNKNOWN;
		if (!flag)
			return PY_GIL_OFF;
	}

	if (py_int(gil + layout->gil_locked, &flag) || py_word(gil + layout->gil_holder, &holder))
		return PY_GIL_UNKNOWN;
	return flag == 1 && holder == state ? PY_GIL_HELD : PY_GIL_RELEASED;
}

// Where the bytecode a frame runs begins: its code object's own, or on a
// free-threaded build the thread's copy of it; 0 when it cannot be read.
static __always_inline u64 py_bytecode(const struct py_layout *layout, u64 frame, u64 code)
{
	if (layout->frame_tlbc_index == PY_ABSENT)
		return code + layout->code_code_adaptive;
	if (layout->code_tlbc == PY_ABSENT || layout->code_array_size == PY_ABSENT ||
	    layout->code_array_entries == PY_ABSENT)
		return 0;

	s32 index = 0;
	u64 copies = 0, count = 0, bytecode = 0;
	if (py_int(frame + layout->frame_tlbc_index, &index) || index < 0)
		return 0;
	if (py_word(code + layout->code_tlbc, &copies) || !copies)
		return 0;
	if (py_word(copies + layout->code_array_size, &count) || (u64)index >= count)
		return 0;
	if (py_word(copies + layout->code_array_entries + (u64)index * 8, &bytecode))
		return 0;
	return bytecode;
}

// The code object a frame's executable names, or 0 for none.
static __always_inline u64 py_code(const struct py_layout *layout, u64 executable)
{
	if (!layout->stack_refs)
		return executable;
	// A stack reference is a pointer with a reference-counting bit, or a
	// tagged integer.
	if ((executable & 3) > 1)
		return 0;
	return executable & ~(u64)1;
}

// Reads the frames of the thread whose state is at `state`, which names
// `native` as its native thread, into `stack`, as `layout` places their
// fields. Returns the number of frames read.
__noinline int py_walk(const struct py_layout *layout, struct py_stack *stack, u64 state,
		       u64 native)
{
	if (!layout || !stack)
		return 0;

	struct py_stack_record *header = &stack->header;
	header->state = state;
	header->running_task = 0;
	header->frames = 0;
	header->gil = PY_GIL_UNKNOWN;

	u64 named = 0;
	if (py_word(state + layout->thread_native_id, &named)) {
		header->end = PY_END_UNREADABLE;
		return 0;
	}
	if (named != native) {
		header->end = PY_END_STALE;
		return 0;
	}

	header->gil = py_gil(layout, state);
	if (layout->thread_running_task != PY_ABSENT)
		py_word(state + layout->thread_running_task, &header->running_task);

	u64 frame = 0, base = 0;
	if (py_word(state + layout->thread_current_frame, &frame)) {
		header->end = PY_END_UNREADABLE;
		return 0;
	}
	if (layout->cframe_current_frame != PY_ABSENT && frame &&
	    py_word(frame + layout->cframe_current_frame, &frame)) {
		header->end = PY_END_UNREADABLE;
		return 0;
	}
	if (layout->thread_base_frame != PY_ABSENT &&
	    py_word(state + layout->thread_base_frame, &base)) {
		header->end = PY_END_UNREADABLE;
		return 0;
	}

	header->end = PY_END_BOUNDED;
	u32 count = 0;
	for (u32 i = 0; i < PY_FRAMES; i++) {
		if (!frame) {
			header->end = base ? PY_END_BEFORE_BASE : PY_END_COMPLETE;
			break;
		}
		if (base && frame == base) {
			header->end = PY_END_COMPLETE;
			break;
		}

		struct py_frame *out = &stack->frames[i];
		u64 executable = 0, instruction = 0, previous = 0;
		u8 owner = 0;
		if (py_word(frame + layout->frame_executable, &executable) ||
		    py_word(frame + layout->frame_instruction, &instruction) ||
		    py_word(frame + layout->frame_previous, &previous) ||
		    bpf_probe_read_user(&owner, sizeof(owner),
					(const void *)(frame + layout->frame_owner))) {
			header->end = PY_END_UNREADABLE;
			break;
		}

		out->owner = owner;
		out->pad = 0;
		out->code = 0;
		out->qualname = 0;
		out->first_line = 0;
		out->offset = PY_NO_OFFSET;

		// An entry frame runs no code of its own: it ends a segment.
		if (owner != layout->owner_entry) {
			u64 code = py_code(layout, executable);
			if (!code || py_word(code + layout->code_qualname, &out->qualname) ||
			    py_int(code + layout->code_firstlineno, &out->first_line)) {
				header->end = PY_END_UNREADABLE;
				break;
			}

			out->code = code;
			u64 bytecode = py_bytecode(layout, frame, code);
			s64 offset = (s64)(instruction - bytecode);
			if (bytecode && offset >= -(s64)0x7fffffff && offset <= (s64)0x7fffffff)
				out->offset = (s32)offset;
		}

		count = i + 1;
		frame = previous;
	}

	header->frames = count;
	return count;
}
