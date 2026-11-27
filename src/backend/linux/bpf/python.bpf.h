// In-kernel CPython frame walker shared by `sampler.bpf.c` and `syscalls.bpf.c`.
// Walks `_PyInterpreterFrame` chains using version-specific offsets from `struct py_layout`.
#pragma once

#include "records.h"


#define PY_FRAMES 128



#define PY_SEARCH 1024


struct py_stack {
	struct py_stack_record header;
	struct py_frame frames[PY_FRAMES];
};


static __always_inline int py_word(u64 address, u64 *word)
{
	return bpf_probe_read_user(word, sizeof(*word), (const void *)address);
}


static __always_inline int py_int(u64 address, s32 *value)
{
	return bpf_probe_read_user(value, sizeof(*value), (const void *)address);
}


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


static __always_inline u64 py_code(const struct py_layout *layout, u64 executable)
{
	if (!layout->stack_refs)
		return executable;


	// CPython 3.14+ stores tagged `_PyStackRef` values in `f_executable`: tag bits 0..=1
	// encode deferred/owned heap pointers, while higher tags denote non-code immediates.
	if ((executable & 3) > 1)
		return 0;
	return executable & ~(u64)1;
}



static __always_inline u64 py_native(const struct py_layout *layout)
{
	struct bpf_pidns_info ids = {};
	if (bpf_get_ns_current_pid_tgid(layout->ns_dev, layout->ns_ino, &ids, sizeof(ids)))
		return 0;
	return ids.pid;
}





const volatile u32 loop_helper = 0;



struct py_search {
	u64 interp_next, interp_threads_head, thread_native_id, thread_next;
	u64 native, interp, state, found, result;
	u32 done;
};


static __always_inline long py_search_step(struct py_search *search)
{
	if (!search->state) {
		if (py_word(search->interp + search->interp_next, &search->interp))
			goto none;
		if (!search->interp) {
			search->result = search->found;
			search->done = 1;
			return 1;
		}
		if (py_word(search->interp + search->interp_threads_head, &search->state))
			goto none;
		return 0;
	}

	u64 named = 0;
	if (py_word(search->state + search->thread_native_id, &named))
		goto none;
	if (named == search->native) {
		if (search->found)
			goto none;
		search->found = search->state;
	}
	if (py_word(search->state + search->thread_next, &search->state))
		goto none;
	return 0;

none:
	search->result = 0;
	search->done = 1;
	return 1;
}

static long py_search_callback(u32 index, void *context)
{
	return py_search_step(context);
}




__noinline u64 py_find(const struct py_layout *layout, u64 native)
{
	if (!layout || !native || !layout->runtime ||
	    layout->runtime_interpreters_head == PY_ABSENT || layout->interp_next == PY_ABSENT ||
	    layout->interp_threads_head == PY_ABSENT || layout->thread_next == PY_ABSENT)
		return 0;

	struct py_search search = {
		.interp_next = layout->interp_next,
		.interp_threads_head = layout->interp_threads_head,
		.thread_native_id = layout->thread_native_id,
		.thread_next = layout->thread_next,
		.native = native,
	};
	if (py_word(layout->runtime + layout->runtime_interpreters_head, &search.interp) ||
	    !search.interp || py_word(search.interp + search.interp_threads_head, &search.state))
		return 0;
	if (loop_helper) {
		bpf_loop(PY_SEARCH, py_search_callback, &search, 0);
	} else {
		for (u32 i = 0; i < PY_SEARCH; i++)
			if (py_search_step(&search))
				break;
	}

	return search.done ? search.result : 0;
}




__noinline int py_walk(const struct py_layout *layout, struct py_stack *stack, u64 known)
{
	if (!layout || !stack)
		return 0;

	struct py_stack_record *header = &stack->header;
	header->state = 0;
	header->running_task = 0;
	header->frames = 0;
	header->gil = PY_GIL_UNKNOWN;

	u64 native = py_native(layout), named = 0, state = known;
	if (!state || py_word(state + layout->thread_native_id, &named) || named != native)
		state = py_find(layout, native);
	if (!native || !state) {
		header->end = PY_END_NO_THREAD;
		return 0;
	}

	header->state = state;

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
			header->end = base && i ? PY_END_BEFORE_BASE : PY_END_COMPLETE;
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
		out->linetable = 0;
		out->function = 0;
		out->first_line = 0;
		out->offset = PY_NO_OFFSET;


		if (owner != layout->owner_entry) {
			u64 code = py_code(layout, executable);
			if (!code || py_word(code + layout->code_qualname, &out->qualname) ||
			    py_word(code + layout->code_linetable, &out->linetable) ||
			    py_int(code + layout->code_firstlineno, &out->first_line)) {
				header->end = PY_END_UNREADABLE;
				break;
			}

			out->code = code;


			if (layout->frame_function != PY_ABSENT)
				py_word(frame + layout->frame_function, &out->function);
			u64 bytecode = py_bytecode(layout, frame, code);
			s64 offset = (s64)(instruction - bytecode);
			if (bytecode && offset >= -(s64)0x7fffffff && offset <= (s64)0x7fffffff)
				out->offset = (s32)offset;
		}

		count = i + 1;
		frame = previous;
	}


	if (header->end == PY_END_BOUNDED && (!frame || (base && frame == base)))
		header->end = !frame && base ? PY_END_BEFORE_BASE : PY_END_COMPLETE;

	header->frames = count;
	return count;
}
