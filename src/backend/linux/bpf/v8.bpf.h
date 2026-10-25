// The walker of a thread's JavaScript frames, which the sampler's program
// runs at each sample and the syscall programs at each call's stack. V8
// keeps its frames on the native stack and their frame-pointer chain: the
// walker follows the chain from the thread's frame pointer and writes one
// v8_frame for each frame whose function slot holds a JavaScript function,
// with what proves the function's shared information when microscope
// names the frame later. It only copies; microscope places each frame by
// its pointer among the native frames, names it, and drops any frame whose
// code is not V8's.
//
// The walk is a global function, so the verifier checks it once for every
// program that calls it. It reads only user memory, each read failing
// whole, and ends at a bound.
#pragma once

#include "records.h"

// The most frames one record holds.
#define V8_FRAMES 64

// The most steps of the frame-pointer chain a walk takes.
#define V8_STEPS 256

// A record of a thread's JavaScript frames.
struct v8_stack {
	struct v8_stack_record header;
	struct v8_frame frames[V8_FRAMES];
};

// Reads the u64 at `address` into `word`: nonzero when it cannot be read.
static __always_inline int v8_word(u64 address, u64 *word)
{
	return bpf_probe_read_user(word, sizeof(*word), (const void *)address);
}

// Whether `word` points to a heap object.
static __always_inline int v8_object(const struct v8_layout *layout, u64 word)
{
	return (word & layout->heap_object_tag_mask) == layout->heap_object_tag &&
	       word > layout->heap_object_tag;
}

// The instance type of the object `word` points to, or -1 where it cannot
// be read.
static __always_inline s64 v8_type(const struct v8_layout *layout, u64 word)
{
	u64 map = 0;
	u16 type = 0;
	u64 base = word - layout->heap_object_tag;
	if (v8_word(base + layout->object_map, &map) || !v8_object(layout, map))
		return -1;
	if (bpf_probe_read_user(&type, sizeof(type),
				(const void *)(map - layout->heap_object_tag +
					       layout->map_instance_type)))
		return -1;
	return type;
}

// Fills `out` with the frame at `fp`, whose code is at `pc`, when its
// function slot holds a JavaScript function: whether it does.
__noinline int v8_frame(const struct v8_layout *layout, u64 fp, u64 pc, u32 first,
		       struct v8_frame *out)
{
	if (!layout || !out)
		return 0;
	u64 marker = 0, function = 0, shared = 0, script = 0;
	// A frame of V8's own keeps a Smi marker where a JavaScript frame
	// keeps its context.
	if (v8_word(fp + (s64)layout->fp_marker, &marker) || !v8_object(layout, marker))
		return 0;
	if (v8_word(fp + (s64)layout->fp_function, &function) || !v8_object(layout, function))
		return 0;
	s64 type = v8_type(layout, function);
	if (type < layout->first_function_type || type > layout->last_function_type)
		return 0;
	u64 tag = layout->heap_object_tag;
	if (v8_word(function - tag + layout->function_shared, &shared) ||
	    !v8_object(layout, shared) || v8_type(layout, shared) != layout->shared_type)
		return 0;

	out->fp = fp;
	out->pc = pc;
	out->function = function;
	out->shared = shared;
	out->script = 0;
	out->script_id = 0;
	out->bytecode = 0;
	out->bytecode_offset = 0;
	out->literal_id = -1;
	out->at_instruction = first ? 1 : 0;
	bpf_probe_read_user(&out->literal_id, sizeof(out->literal_id),
			    (const void *)(shared - tag + layout->shared_literal_id));
	if (!v8_word(shared - tag + layout->shared_script, &script) && v8_object(layout, script) &&
	    v8_type(layout, script) == layout->script_type) {
		out->script = script;
		v8_word(script - tag + layout->script_id, &out->script_id);
	}
	v8_word(fp + (s64)layout->fp_bytecode_array, &out->bytecode);
	v8_word(fp + (s64)layout->fp_bytecode_offset, &out->bytecode_offset);
	return 1;
}

// Reads the current thread's JavaScript frames into `stack`, from the frame
// at `fp`, whose code is at `pc`, as `layout` places their slots and fields.
// Returns the number of frames read.
__noinline int v8_walk(const struct v8_layout *layout, struct v8_stack *stack, u64 fp, u64 pc)
{
	if (!layout || !stack)
		return 0;

	struct v8_stack_record *header = &stack->header;
	header->frames = 0;
	header->end = V8_END_BOUNDED;
	for (u32 i = 0; i < V8_STEPS; i++) {
		if (!fp) {
			header->end = V8_END_COMPLETE;
			break;
		}
		// The count is read back from the record, so that the verifier
		// does not tell one walk's state from another's by it.
		u32 at = header->frames;
		if (at >= V8_FRAMES)
			break;
		if (v8_frame(layout, fp, pc, i == 0, &stack->frames[at & (V8_FRAMES - 1)]))
			header->frames = at + 1;

		u64 caller = 0;
		if (v8_word(fp, &caller) || v8_word(fp + 8, &pc)) {
			header->end = V8_END_UNREADABLE;
			break;
		}
		// A chain climbs the stack; a frame pointer that does not is no
		// frame's, as one a function without frames left in rbp.
		if (caller && caller <= fp) {
			header->end = V8_END_UNREADABLE;
			break;
		}
		fp = caller;
	}

	return header->frames;
}
