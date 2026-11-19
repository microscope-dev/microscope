





#pragma once

typedef unsigned char u8;
typedef unsigned short u16;
typedef unsigned int u32;
typedef int s32;
typedef unsigned long long u64;
typedef long long s64;

enum record_kind {
	RECORD_CALL = 1,


	RECORD_ENTRY = 2,
	RECORD_SPAWN = 3,
	RECORD_EXEC = 4,
	RECORD_EXIT = 5,
	RECORD_SIGNAL = 6,

	RECORD_FILE = 7,

	RECORD_STACK = 8,


	RECORD_PY_STACK = 9,


	RECORD_V8_STACK = 10,

	RECORD_HANDOFF = 11,
};


enum makes {
	MAKES_NONE = 0,

	MAKES_RESULT = 1,

	MAKES_PAIR = 2,
};


enum config_flags {
	CONFIG_STREAM = 1,
	CONFIG_COUNT = 2,
	CONFIG_FOLLOW = 4,


	CONFIG_SKIP_UNSEEN = 8,


	CONFIG_STACKS = 16,

	CONFIG_STACK_COPY = 32,


	CONFIG_GENERATION = 64,


	CONFIG_EXCEPTIONS = 128,
};


enum call_flags {



	CALL_EXIT_SKIPPED = 1,
	CALL_ENTRY_UNSEEN = 2,
	CALL_ABI_UNCERTAIN = 4,

	CALL_PROCESS_EXITED = 8,

	CALL_SAMPLED_OUT = 16,

	CALL_STACK_LOST = 32,

	CALL_KEPT_FAILED = 64,
	CALL_KEPT_SLOW = 128,
};


enum loss {

	LOSS_DROPPED = 0,

	LOSS_UNSEEN = 1,

	LOSS_CAPPED = 2,

	LOSS_OVERFLOWED = 3,
	LOSSES = 4,
};

enum signal_flags {
	SIGNAL_INFO_LOST = 1,
};

enum abi {
	ABI_64 = 0,
	ABI_32 = 1,
	ABI_X32 = 2,
};


enum copy_kind {
	COPY_NONE = 0,
	COPY_PATH = 1,
	COPY_STRING = 2,
	COPY_STRINGS = 3,

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

	u8 writes;
	u8 count;
	u8 count_arg;
	u8 pad[3];

	u32 size;
	u32 limit;
};


struct call_spec {
	u8 selected;

	u8 moves;
	u8 makes;
	u8 makes_arg;


	u8 forks;
	u8 execs;

	u16 stat;
	u8 ends;

	u8 waits;
	u16 pad;
	struct arg_copy args[6];
};



enum waits {
	WAITS_NOT = 0,
	WAITS = 1,

	WAITS_BY_FUTEX_OP = 2,
};

struct config {
	u32 flags;
	u32 elements;


	u32 sample;

	u32 rows;

	u64 slow_ns;


	u32 cap;
	u32 pad;
};


struct entry {
	u64 entered;
	u64 args[6];
	u32 nr;
	u32 tgid;

	u32 exec_tid;
	u8 abi;
	u8 flags;

	u8 stacked;




	u8 known;
};


struct in_flight_record {
	u32 tid;
	u32 pad;
	struct entry entry;
};

struct record {
	u32 kind;

	u32 size;
	u32 tid;
	u32 tgid;
};



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


struct exec_record {
	struct record header;
	u64 time;
	u32 pad;
	u32 len;
};

struct handoff_record {
	struct record header;
	u64 entered;
	u32 former;
	u32 pad;
};


struct exit_record {
	struct record header;
	u64 time;

	s32 code;

	u32 last;
};

struct signal_record {
	struct record header;
	u64 time;
	s32 signal;

	u32 flags;

	u8 info[48];
};



struct stack_record {
	struct record header;
	u32 frames;
	u32 copied;



	u64 entered;

	u32 interpreted;
	u32 pad;


	u64 regs[17];
};


struct file_record {
	struct record header;
	s32 fd;
	u32 mode;
	u64 ino;
	u64 magic;

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



struct stat_key {
	u32 tid;
	u32 tgid;
	u32 row;
	u32 reserved;
};

struct errno_key {
	u32 tid;
	u32 tgid;
	u32 slot;
	u32 err;
};






#define PY_ABSENT 0xffffffff

struct py_layout {
	u32 thread_native_id;


	u32 thread_current_frame;
	u32 cframe_current_frame;

	u32 thread_base_frame;

	u32 thread_running_task;


	u32 thread_holds_gil;
	u32 thread_gil_requested;


	u32 thread_interp;
	u32 interp_ceval_gil;
	u32 gil_locked;
	u32 gil_holder;

	u32 gil_enabled;
	u32 frame_executable;
	u32 frame_previous;
	u32 frame_instruction;
	u32 frame_owner;


	u32 frame_tlbc_index;
	u32 code_qualname;
	u32 code_firstlineno;
	u32 code_linetable;
	u32 code_code_adaptive;


	u32 code_tlbc;
	u32 code_array_size;
	u32 code_array_entries;

	u32 stack_refs;


	u32 owner_entry;


	u32 runtime_interpreters_head;
	u32 interp_next;
	u32 interp_threads_head;
	u32 thread_next;

	u32 frame_function;
	u32 pad;

	u64 runtime;


	u64 ns_dev;
	u64 ns_ino;
};




struct py_thread {
	u64 state;


	u64 native;
};


enum py_end {

	PY_END_COMPLETE = 0,

	PY_END_BOUNDED = 1,

	PY_END_UNREADABLE = 2,

	PY_END_NO_LAYOUT = 3,

	PY_END_NO_THREAD = 4,


	PY_END_STALE = 5,

	PY_END_BEFORE_BASE = 6,
};


enum py_gil {
	PY_GIL_UNKNOWN = 0,
	PY_GIL_HELD = 1,

	PY_GIL_WAITING = 2,
	PY_GIL_RELEASED = 3,

	PY_GIL_OFF = 4,
};


#define PY_NO_OFFSET ((s32)0x80000000)




struct py_frame {
	u64 code;
	u64 qualname;
	u64 linetable;

	u64 function;


	s32 offset;
	s32 first_line;
	u32 owner;
	u32 pad;
};


struct py_stack_record {
	struct record header;
	u64 state;

	u64 running_task;
	u32 frames;
	u8 end;
	u8 gil;
	u16 pad;
};





struct v8_layout {


	s32 fp_function;
	s32 fp_marker;

	s32 fp_bytecode_array;
	s32 fp_bytecode_offset;
	u32 heap_object_tag_mask;
	u32 heap_object_tag;
	u32 smi_tag_mask;
	u32 smi_tag;
	u32 object_map;
	u32 map_instance_type;
	u32 first_function_type;
	u32 last_function_type;
	u32 shared_type;
	u32 script_type;
	u32 function_shared;
	u32 shared_script;
	u32 shared_literal_id;
	u32 script_id;




	u32 isolate_place;
	u32 isolate_pad;
	s64 isolate_offset;
	u64 isolate_slot;
};


enum v8_isolate_place {


	V8_ISOLATE_UNPLACED = 0,

	V8_ISOLATE_OFFSET = 1,

	V8_ISOLATE_SLOT = 2,
};


enum v8_isolate {

	V8_ISOLATE_NOT_READ = 0,

	V8_ISOLATE_READ = 1,


	V8_ISOLATE_UNREADABLE = 2,
};


enum v8_end {

	V8_END_COMPLETE = 0,

	V8_END_BOUNDED = 1,


	V8_END_UNREADABLE = 2,

	V8_END_NO_LAYOUT = 3,
};





struct v8_frame {

	u64 fp;
	u64 pc;
	u64 function;
	u64 shared;
	u64 script;

	u64 script_id;
	u64 bytecode;
	u64 bytecode_offset;
	s32 literal_id;


	u32 at_instruction;
};


struct v8_stack_record {
	struct record header;
	u32 frames;
	u8 end;

	u8 isolate_read;
	u8 pad[2];
	u64 isolate;
};
