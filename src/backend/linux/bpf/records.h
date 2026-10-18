// What the programs and microscope exchange: ring records, map keys and
// values. src/backend/linux/syscalls/bpf.rs and src/backend/linux/bpf/
// sampler.rs decode each with a struct of the same layout, and tests
// compare the two through the objects' BTF.
// Every field is little-endian and naturally aligned; records are padded
// to eight bytes.
#pragma once

typedef unsigned char u8;
typedef unsigned short u16;
typedef unsigned int u32;
typedef int s32;
typedef unsigned long long u64;
typedef long long s64;

enum record_kind {
	RECORD_CALL = 1,
	// A call's arguments read at its entry, as execve's must be; its exit
	// record follows.
	RECORD_ENTRY = 2,
	RECORD_SPAWN = 3,
	RECORD_EXEC = 4,
	RECORD_EXIT = 5,
	RECORD_SIGNAL = 6,
	// What a descriptor the call that follows made refers to.
	RECORD_FILE = 7,
	// The stack of the thread whose call or signal follows.
	RECORD_STACK = 8,
	// A thread's Python frames, which the sample or call that follows
	// on its CPU was taken in.
	RECORD_PY_STACK = 9,
};

// Which descriptors a call makes, from call_spec.makes.
enum makes {
	MAKES_NONE = 0,
	// Its result.
	MAKES_RESULT = 1,
	// The two ints argument makes_arg points at, as pipe2's.
	MAKES_PAIR = 2,
};

// Bits of config.flags.
enum config_flags {
	CONFIG_STREAM = 1,
	CONFIG_COUNT = 2,
	CONFIG_FOLLOW = 4,
	// Leave out calls whose entry was not seen: a launched program's are
	// microscope's own, made before it executes.
	CONFIG_SKIP_UNSEEN = 8,
	// Send each call's and signal's stack: the kernel's walk of its
	// frame pointers.
	CONFIG_STACKS = 16,
	// With each stack, a copy of its top, for microscope to unwind.
	CONFIG_STACK_COPY = 32,
	// Count into the second half of STATS, and ERRNOS keys with their top
	// bit set, while microscope reads and clears the first, and the reverse.
	CONFIG_GENERATION = 64,
};

// Bits of call_record.flags, as EventFlags.
enum call_flags {
	CALL_ENTRY_UNSEEN = 2,
	CALL_ABI_UNCERTAIN = 4,
	// The thread ended inside the call, which never returned.
	CALL_PROCESS_EXITED = 8,
	// Left out of the stream by sampling, so counted by the kernel.
	CALL_SAMPLED_OUT = 16,
};

enum abi {
	ABI_64 = 0,
	ABI_32 = 1,
	ABI_X32 = 2,
};

// How the copier reads one argument.
enum copy_kind {
	COPY_NONE = 0,
	COPY_PATH = 1,
	COPY_STRING = 2,
	COPY_STRINGS = 3,
	// An array of strings whose elements are only counted.
	COPY_STRINGS_COUNTED = 4,
	COPY_BYTES = 5,
	COPY_IOVEC = 6,
	COPY_SOCKADDR = 7,
	COPY_STRUCT = 8,
	COPY_FD_SET = 9,
};

enum copy_count {
	COUNT_ONE = 0,
	COUNT_ARG = 1,
	COUNT_RETURN = 2,
	// The 32-bit integer the argument count_arg points at.
	COUNT_POINTED = 3,
};

enum read_at {
	READ_ENTRY = 0,
	READ_EXIT = 1,
};

enum capture_state {
	CAPTURE_COMPLETE = 0,
	CAPTURE_TRUNCATED = 1,
	CAPTURE_UNREADABLE = 2,
};

struct arg_copy {
	u8 kind;
	u8 when;
	// Whether the call writes it, so a failed call leaves nothing to read.
	u8 writes;
	u8 count;
	u8 count_arg;
	u8 pad[3];
	// A structure's size, the bytes kept of a buffer, or a string's limit.
	u32 size;
	u32 limit;
};

// The value of each call's slot in the CALLS map: (abi << 10) | number.
struct call_spec {
	u8 selected;
	// Whether its result counts bytes the call moved.
	u8 moves;
	u8 makes;
	u8 makes_arg;
	// Whether it creates a thread or process, whose own return from it
	// is not a call of its own.
	u8 forks;
	u8 pad;
	// Its row in STATS, which holds only the chosen calls.
	u16 stat;
	struct arg_copy args[6];
};

struct config {
	u32 flags;
	u32 elements;
	// While streaming, send one call in this many and count the rest;
	// 0 or 1 sends them all.
	u32 sample;
	// The rows of statistics in each half of STATS.
	u32 rows;
};

// A thread's call in progress.
struct entry {
	u64 entered;
	u64 args[6];
	u32 nr;
	u32 tgid;
	u8 abi;
	u8 flags;
	u8 pad[6];
};

// What the in_flight iterator writes for each call in progress.
struct in_flight_record {
	u32 tid;
	u32 pad;
	struct entry entry;
};

struct record {
	u32 kind;
	// The whole record's bytes, captures included.
	u32 size;
	u32 tid;
	u32 tgid;
};

// Followed by `captures` capture headers, each followed by its bytes
// padded to eight.
struct call_record {
	struct record header;
	u64 entered;
	u64 exited;
	s64 ret;
	u64 args[6];
	u32 nr;
	u8 abi;
	u8 flags;
	u16 captures;
};

struct capture_header {
	u8 arg;
	u8 when;
	u8 state;
	u8 pad;
	u32 len;
	u32 wanted;
	u32 pad2;
};

struct spawn_record {
	struct record header;
	u64 time;
	u32 child;
	u32 child_tgid;
};

// Followed by the new image's path, `len` bytes.
struct exec_record {
	struct record header;
	u64 time;
	u32 former;
	u32 len;
};

// A thread's end, and when it was the last, its process's.
struct exit_record {
	struct record header;
	u64 time;
	// The exit code as wait(2) encodes it.
	s32 code;
	// Whether the process ended with the thread.
	u32 last;
};

struct signal_record {
	struct record header;
	u64 time;
	s32 signal;
	u32 pad;
	// The kernel's siginfo, as much as every kind of signal fills.
	u8 info[48];
};

// Followed by `frames` addresses, the kernel's walk of the frame pointers
// from the instruction, then `copied` bytes of the stack from its pointer.
struct stack_record {
	struct record header;
	u32 frames;
	u32 copied;
	// The user registers by DWARF number: ax, dx, cx, bx, si, di, bp, sp,
	// r8 to r15, and the instruction pointer.
	u64 regs[17];
};

// Followed by the dentry's name, `len` bytes.
struct file_record {
	struct record header;
	s32 fd;
	u32 mode;
	u64 ino;
	u64 magic;
	// The kernel's dev_t: twelve bits of major above twenty of minor.
	u32 rdev;
	u32 len;
	u32 cloexec;
	u32 pad;
};

struct stat_value {
	u64 calls;
	u64 errors;
	u64 timed;
	u64 total_ns;
	u64 min_ns;
	u64 max_ns;
	u64 bytes;
	u64 log2_ns[40];
};

struct errno_key {
	u32 slot;
	u32 err;
};

// The offsets the Python walker reads a process's CPython by, each from the
// start of its structure, or PY_ABSENT where the version has no such field,
// and where the process keeps its runtime and which pid namespace numbers
// its threads. PythonLayout fills the offsets; the sampler and the syscall
// programs keep one per process in LAYOUTS.
#define PY_ABSENT 0xffffffff

struct py_layout {
	u32 thread_native_id;
	// The innermost frame; in 3.12, the _PyCFrame pointer whose
	// cframe_current_frame holds it.
	u32 thread_current_frame;
	u32 cframe_current_frame;
	// From 3.15, the frame each thread's chain ends at.
	u32 thread_base_frame;
	// From 3.14, the asyncio task a thread runs.
	u32 thread_running_task;
	// From 3.15, whether a thread holds its interpreter's GIL, and whether
	// it asks for it, each an int.
	u32 thread_holds_gil;
	u32 thread_gil_requested;
	// Before 3.15, a thread's interpreter, the GIL it takes, and that GIL's
	// locked int and last holder, the last two from the GIL's start.
	u32 thread_interp;
	u32 interp_ceval_gil;
	u32 gil_locked;
	u32 gil_holder;
	// In a free-threaded build, whether its GIL is on, an int.
	u32 gil_enabled;
	u32 frame_executable;
	u32 frame_previous;
	u32 frame_instruction;
	u32 frame_owner;
	// In a free-threaded build, which copy of its code's bytecode a frame
	// runs, an int32.
	u32 frame_tlbc_index;
	u32 code_qualname;
	u32 code_firstlineno;
	u32 code_code_adaptive;
	// In a free-threaded build, a code object's copies of its bytecode:
	// their array, its size, and its entries.
	u32 code_tlbc;
	u32 code_array_size;
	u32 code_array_entries;
	// 1 where frames hold _PyStackRefs, which tag their low bits.
	u32 stack_refs;
	// The owner of the frame each run of the evaluation loop pushes
	// beneath the frames it runs, which runs no code of its own.
	u32 owner_entry;
	// The lists a thread's state is found in: the runtime's interpreters,
	// and each interpreter's thread states.
	u32 runtime_interpreters_head;
	u32 interp_next;
	u32 interp_threads_head;
	u32 thread_next;
	u32 pad;
	// Where the process keeps _PyRuntime.
	u64 runtime;
	// The device and inode of the process's pid namespace, whose ids its
	// thread states name their threads by.
	u64 ns_dev;
	u64 ns_ino;
};

// A thread's state, by its thread id as the program shows it to
// microscope; THREADS holds one for each thread of a Python process, which
// the walker tries before it looks through the process's states.
struct py_thread {
	u64 state;
	// The thread's id as its own pid namespace numbers it, which the state
	// names as its native_thread_id.
	u64 native;
};

// Why a thread's frames end.
enum py_end {
	// At the end of the chain: no previous frame, or the base frame.
	PY_END_COMPLETE = 0,
	// At the most frames a record holds.
	PY_END_BOUNDED = 1,
	// At a frame or field that could not be read, as one paged out.
	PY_END_UNREADABLE = 2,
	// Before the walk began: the process's layout is not yet known.
	PY_END_NO_LAYOUT = 3,
	// Before the walk began: no state, or more than one, names the thread.
	PY_END_NO_THREAD = 4,
	// Before the walk began: the state known for the thread is another's.
	// The walker no longer ends so; it looks for the thread's state.
	PY_END_STALE = 5,
	// From 3.15, at no previous frame, before the base frame.
	PY_END_BEFORE_BASE = 6,
};

// What the thread does with its interpreter's GIL.
enum py_gil {
	PY_GIL_UNKNOWN = 0,
	PY_GIL_HELD = 1,
	// Only 3.15 and later say.
	PY_GIL_WAITING = 2,
	PY_GIL_RELEASED = 3,
	// A free-threaded build's GIL is off.
	PY_GIL_OFF = 4,
};

// Where an instruction offset could not be read.
#define PY_NO_OFFSET ((s32)0x80000000)

// One interpreter frame. The code object's qualified-name pointer and
// first line prove, when microscope reads it later, that it is the object
// the frame ran.
struct py_frame {
	u64 code;
	u64 qualname;
	// The byte offset of the frame's instruction from the start of the
	// bytecode it runs, or PY_NO_OFFSET.
	s32 offset;
	s32 first_line;
	u32 owner;
	u32 pad;
};

// Followed by `frames` py_frames, innermost first.
struct py_stack_record {
	struct record header;
	u64 state;
	// From 3.14, the asyncio task the thread runs, or 0.
	u64 running_task;
	u32 frames;
	u8 end;
	u8 gil;
	u16 pad;
};
