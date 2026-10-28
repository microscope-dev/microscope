/*
 * Prints a Generated literal of offsets, sizes, and frame/code/Unicode constants from CPython headers.
 * scripts/cpython-layouts.sh passes the interpreter directory name as argv[1].
 */
#include <Python.h>

#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "datetime.h"

#include "internal/pycore_code.h"
#include "internal/pycore_dict.h"
#include "internal/pycore_long.h"
#include "internal/pycore_moduleobject.h"
#include "internal/pycore_object.h"
#include "internal/pycore_typeobject.h"
#include "internal/pycore_runtime.h"
#include "internal/pycore_interp.h"
#include "internal/pycore_unicodeobject.h"
#if PY_VERSION_HEX >= 0x030e0000
#include "internal/pycore_interpframe.h"
#include "internal/pycore_genobject.h"
#include "internal/pycore_llist.h"
#include "internal/pycore_tstate.h"
#include "internal/pycore_ceval.h"
#endif
#include "internal/pycore_frame.h"

/* The standard library's types that only their own C defines, copied as
 * 3.12 to 3.15 define them: collections.deque and defaultdict, from
 * Modules/_collectionsmodule.c; OrderedDict, from Objects/odictobject.c;
 * datetime.timezone, from Modules/_datetimemodule.c; and decimal.Decimal,
 * from Modules/_decimal/_decimal.c, with libmpdec's number as its 64-bit
 * builds define it in mpdecimal.h. conformance.rs checks the first three
 * against the runtime's DWARF; the others are in extension modules. */
#if PY_VERSION_HEX >= 0x030e0000
/* asyncio.Task, as Modules/_asynciomodule.c defines it in 3.14 and 3.15:
 * FutureObj_HEAD(task), then the task's own fields. conformance.rs checks
 * it against each build's .AsyncioDebug section, which gives the offsets
 * external debuggers read. */
typedef enum {
    MICROSCOPE_STATE_PENDING,
    MICROSCOPE_STATE_CANCELLED,
    MICROSCOPE_STATE_FINISHED
} microscope_fut_state;

typedef struct {
    PyObject_HEAD
    PyObject *task_loop;
    PyObject *task_callback0;
    PyObject *task_context0;
    PyObject *task_callbacks;
    PyObject *task_exception;
    PyObject *task_exception_tb;
    PyObject *task_result;
    PyObject *task_source_tb;
    PyObject *task_cancel_msg;
    PyObject *task_cancelled_exc;
    PyObject *task_awaited_by;
    microscope_fut_state task_state;
    char task_is_task;
    char task_awaited_by_is_set;
    unsigned task_log_tb: 1;
    unsigned task_blocking: 1;
    unsigned task_must_cancel: 1;
    unsigned task_log_destroy_pending: 1;
    int task_num_cancels_requested;
    PyObject *task_fut_waiter;
    PyObject *task_coro;
    PyObject *task_name;
    PyObject *task_context;
    struct llist_node task_node;
#ifdef Py_GIL_DISABLED
    uintptr_t task_tid;
#endif
} microscope_task;
#endif

#define MICROSCOPE_BLOCKLEN 64
#define MICROSCOPE_MAXFREEBLOCKS 16

typedef struct microscope_block {
    struct microscope_block *leftlink;
    PyObject *data[MICROSCOPE_BLOCKLEN];
    struct microscope_block *rightlink;
} microscope_block;

typedef struct {
    PyObject_VAR_HEAD
    microscope_block *leftblock;
    microscope_block *rightblock;
    Py_ssize_t leftindex;
    Py_ssize_t rightindex;
    size_t state;
    Py_ssize_t maxlen;
    Py_ssize_t numfreeblocks;
    microscope_block *freeblocks[MICROSCOPE_MAXFREEBLOCKS];
    PyObject *weakreflist;
} microscope_deque;

typedef struct {
    PyDictObject dict;
    PyObject *default_factory;
} microscope_defdict;

typedef struct microscope_odict_node {
    PyObject *key;
    Py_hash_t hash;
    struct microscope_odict_node *next;
    struct microscope_odict_node *prev;
} microscope_odict_node;

typedef struct {
    PyDictObject od_dict;
    microscope_odict_node *od_first;
    microscope_odict_node *od_last;
    microscope_odict_node **od_fast_nodes;
    Py_ssize_t od_fast_nodes_size;
    void *od_resize_sentinel;
    size_t od_state;
    PyObject *od_inst_dict;
    PyObject *od_weakreflist;
} microscope_odict;

typedef struct {
    PyObject_HEAD
    PyObject *offset;
    PyObject *name;
} microscope_timezone;

typedef struct {
    uint8_t flags;
    int64_t exp;
    int64_t digits;
    int64_t len;
    int64_t alloc;
    uint64_t *data;
} microscope_mpd;

typedef struct {
    PyObject_HEAD
    Py_hash_t hash;
    microscope_mpd dec;
    uint64_t data[4];
} microscope_decimal;

static void fail(const char *what, unsigned long long value) {
    fprintf(stderr, "%s = %llu does not fit\n", what, value);
    exit(1);
}

/* A layout stores offsets and sizes as u32s. */
static void emit_field(const char *name, size_t value) {
    if (value >= UINT32_MAX) {
        fail(name, value);
    }
    printf("            (Field::%s, %zu),\n", name, value);
}

static void emit_constant(const char *name, unsigned long long value) {
    if (value > UINT8_MAX) {
        fail(name, value);
    }
    printf("            (Constant::%s, %llu),\n", name, value);
}

#define FIELD(name, expr) emit_field(#name, (size_t)(expr))
#define CONSTANT(name, expr) emit_constant(#name, (unsigned long long)(expr))

/* Index of the lowest set bit in the 32-bit `state` word, after checking that
 * exactly one bit is set. The caller sets a single bitfield to 1 first. */
static unsigned lone_bit_index(const PyASCIIObject *obj) {
    uint32_t word;
    memcpy(&word, &obj->state, sizeof word);
    if (__builtin_popcount(word) != 1) {
        fprintf(stderr, "unicode state word %#x has no single bit\n", word);
        exit(1);
    }
    return (unsigned)__builtin_ctz(word);
}

static unsigned kind_shift(void) {
    PyASCIIObject obj;
    memset(&obj, 0, sizeof obj);
    obj.state.kind = 1;
    return lone_bit_index(&obj);
}

static unsigned compact_shift(void) {
    PyASCIIObject obj;
    memset(&obj, 0, sizeof obj);
    obj.state.compact = 1;
    return lone_bit_index(&obj);
}

static unsigned ascii_shift(void) {
    PyASCIIObject obj;
    memset(&obj, 0, sizeof obj);
    obj.state.ascii = 1;
    return lone_bit_index(&obj);
}

int main(int argc, char **argv) {
    if (argc != 2) {
        fprintf(stderr, "usage: %s NAME\n", argv[0]);
        return 2;
    }

    printf("    Generated {\n");
    printf("        name: \"%s\",\n", argv[1]);
    printf("        version: 0x%04x_%04x,\n", (unsigned)PY_VERSION_HEX >> 16,
           (unsigned)PY_VERSION_HEX & 0xffff);
#ifdef Py_GIL_DISABLED
    printf("        free_threaded: true,\n");
#else
    printf("        free_threaded: false,\n");
#endif

    printf("        fields: &[\n");
    FIELD(RuntimeInterpretersHead, offsetof(_PyRuntimeState, interpreters.head));
    FIELD(RuntimeInitialized, offsetof(_PyRuntimeState, initialized));
    FIELD(RuntimeFinalizing, offsetof(_PyRuntimeState, _finalizing));
    FIELD(InterpreterId, offsetof(PyInterpreterState, id));
    FIELD(InterpreterNext, offsetof(PyInterpreterState, next));
    FIELD(InterpreterThreadsHead, offsetof(PyInterpreterState, threads.head));
    FIELD(ThreadSize, sizeof(PyThreadState));
    FIELD(ThreadPrev, offsetof(PyThreadState, prev));
    FIELD(ThreadNext, offsetof(PyThreadState, next));
    FIELD(ThreadInterp, offsetof(PyThreadState, interp));
#if PY_VERSION_HEX < 0x030d0000
    FIELD(ThreadCurrentFrame, offsetof(PyThreadState, cframe));
#else
    FIELD(ThreadCurrentFrame, offsetof(PyThreadState, current_frame));
#endif
#if PY_VERSION_HEX >= 0x030f0000
    FIELD(ThreadBaseFrame, offsetof(PyThreadState, base_frame));
#endif
    FIELD(ThreadId, offsetof(PyThreadState, thread_id));
    FIELD(ThreadNativeId, offsetof(PyThreadState, native_thread_id));
#if PY_VERSION_HEX >= 0x030e0000
    FIELD(DebugEvalBreaker, offsetof(PyThreadState, eval_breaker));
    FIELD(DebugSupport, offsetof(PyThreadState, remote_debugger_support));
    FIELD(DebugEnabled, offsetof(PyInterpreterState, config.remote_debug));
    FIELD(DebugPending, offsetof(_PyRemoteDebuggerSupport, debugger_pending_call));
    FIELD(DebugScript, offsetof(_PyRemoteDebuggerSupport, debugger_script_path));
    FIELD(DebugScriptSize, _Py_MAX_SCRIPT_PATH_SIZE);
#endif
    FIELD(InterpreterCevalGil, offsetof(PyInterpreterState, ceval.gil));
    FIELD(InterpreterGil, offsetof(PyInterpreterState, _gil));
    FIELD(InterpreterGilLocked, offsetof(PyInterpreterState, _gil.locked));
    FIELD(InterpreterGilHolder, offsetof(PyInterpreterState, _gil.last_holder));
#ifdef Py_GIL_DISABLED
    FIELD(GilEnabled, offsetof(struct _gil_runtime_state, enabled));
#endif
#if PY_VERSION_HEX >= 0x030f0000
    FIELD(ThreadHoldsGil, offsetof(PyThreadState, holds_gil));
    FIELD(ThreadGilRequested, offsetof(PyThreadState, gil_requested));
#endif
#if PY_VERSION_HEX < 0x030d0000
    FIELD(CframeCurrentFrame, offsetof(_PyCFrame, current_frame));
#endif
    FIELD(FrameSize, sizeof(_PyInterpreterFrame));
    FIELD(FramePrevious, offsetof(_PyInterpreterFrame, previous));
#if PY_VERSION_HEX < 0x030d0000
    FIELD(FrameExecutable, offsetof(_PyInterpreterFrame, f_code));
    FIELD(FrameInstruction, offsetof(_PyInterpreterFrame, prev_instr));
#else
    FIELD(FrameExecutable, offsetof(_PyInterpreterFrame, f_executable));
    FIELD(FrameInstruction, offsetof(_PyInterpreterFrame, instr_ptr));
#endif
    FIELD(FrameLocalsplus, offsetof(_PyInterpreterFrame, localsplus));
    FIELD(FrameOwner, offsetof(_PyInterpreterFrame, owner));
    FIELD(FrameGlobals, offsetof(_PyInterpreterFrame, f_globals));
    FIELD(FrameBuiltins, offsetof(_PyInterpreterFrame, f_builtins));
    FIELD(FrameFunction, offsetof(_PyInterpreterFrame, f_funcobj));
#if PY_VERSION_HEX >= 0x030e0000
    FIELD(FrameStackpointer, offsetof(_PyInterpreterFrame, stackpointer));
    FIELD(ThreadAsyncioRunningTask, offsetof(_PyThreadStateImpl, asyncio_running_task));
    FIELD(ThreadAsyncioTasksHead, offsetof(_PyThreadStateImpl, asyncio_tasks_head));
    FIELD(InterpreterAsyncioTasksHead, offsetof(PyInterpreterState, asyncio_tasks_head));
    FIELD(LlistNext, offsetof(struct llist_node, next));
    FIELD(LlistPrev, offsetof(struct llist_node, prev));
    FIELD(TaskState, offsetof(microscope_task, task_state));
    FIELD(TaskAwaitedBy, offsetof(microscope_task, task_awaited_by));
    FIELD(TaskIsTask, offsetof(microscope_task, task_is_task));
    FIELD(TaskAwaitedByIsSet, offsetof(microscope_task, task_awaited_by_is_set));
    FIELD(TaskFutWaiter, offsetof(microscope_task, task_fut_waiter));
    FIELD(TaskCoro, offsetof(microscope_task, task_coro));
    FIELD(TaskName, offsetof(microscope_task, task_name));
    FIELD(TaskNode, offsetof(microscope_task, task_node));
    FIELD(TaskObjectSize, sizeof(microscope_task));
#endif
    FIELD(GenFrameState, offsetof(PyGenObject, gi_frame_state));
    FIELD(GenFrame, offsetof(PyGenObject, gi_iframe));
#if defined(Py_GIL_DISABLED) && PY_VERSION_HEX >= 0x030e0000
    FIELD(FrameTlbcIndex, offsetof(_PyInterpreterFrame, tlbc_index));
#endif
    FIELD(CodeSize, sizeof(PyCodeObject));
    FIELD(CodeFilename, offsetof(PyCodeObject, co_filename));
    FIELD(CodeName, offsetof(PyCodeObject, co_name));
    FIELD(CodeQualname, offsetof(PyCodeObject, co_qualname));
    FIELD(CodeLinetable, offsetof(PyCodeObject, co_linetable));
    FIELD(CodeFirstlineno, offsetof(PyCodeObject, co_firstlineno));
    FIELD(CodeArgcount, offsetof(PyCodeObject, co_argcount));
    FIELD(CodeKwonlyargcount, offsetof(PyCodeObject, co_kwonlyargcount));
    FIELD(CodeLocalsplusnames, offsetof(PyCodeObject, co_localsplusnames));
    FIELD(CodeLocalspluskinds, offsetof(PyCodeObject, co_localspluskinds));
    FIELD(CodeCodeAdaptive, offsetof(PyCodeObject, co_code_adaptive));
#if defined(Py_GIL_DISABLED) && PY_VERSION_HEX >= 0x030e0000
    FIELD(CodeTlbc, offsetof(PyCodeObject, co_tlbc));
#endif
#if defined(Py_GIL_DISABLED) && PY_VERSION_HEX >= 0x030e0000
    FIELD(CodeArraySize, offsetof(_PyCodeArray, size));
    FIELD(CodeArrayEntries, offsetof(_PyCodeArray, entries));
#endif
    FIELD(CodeFlags, offsetof(PyCodeObject, co_flags));
    FIELD(CodeNlocalsplus, offsetof(PyCodeObject, co_nlocalsplus));
    FIELD(ObjectType, offsetof(PyObject, ob_type));
    FIELD(ObjectSize, sizeof(PyObject));
    FIELD(VarObjectSize, offsetof(PyVarObject, ob_size));
    FIELD(TypeName, offsetof(PyTypeObject, tp_name));
    FIELD(TypeFlags, offsetof(PyTypeObject, tp_flags));
    FIELD(TypeBasicsize, offsetof(PyTypeObject, tp_basicsize));
    FIELD(TypeDictoffset, offsetof(PyTypeObject, tp_dictoffset));
    FIELD(TypeDict, offsetof(PyTypeObject, tp_dict));
    FIELD(TypeMro, offsetof(PyTypeObject, tp_mro));
    FIELD(TypeBase, offsetof(PyTypeObject, tp_base));
    FIELD(TypeSubclasses, offsetof(PyTypeObject, tp_subclasses));
#if PY_VERSION_HEX >= 0x030d0000
    FIELD(InterpreterStaticTypes, offsetof(PyInterpreterState, types.builtins.initialized));
    FIELD(InterpreterStaticTypesEnd, offsetof(PyInterpreterState, types.builtins.initialized)
                                         + sizeof(((PyInterpreterState *)0)->types.builtins.initialized));
    FIELD(StaticTypeStateSize, sizeof(managed_static_type_state));
    FIELD(StaticTypeStateDict, offsetof(managed_static_type_state, tp_dict));
    FIELD(StaticTypeStateType, offsetof(managed_static_type_state, type));
    FIELD(InterpreterStaticExtensionTypes,
          offsetof(PyInterpreterState, types.for_extensions.initialized));
    FIELD(InterpreterStaticExtensionTypesEnd,
          offsetof(PyInterpreterState, types.for_extensions.initialized)
              + sizeof(((PyInterpreterState *)0)->types.for_extensions.initialized));
#else
    FIELD(InterpreterStaticTypes, offsetof(PyInterpreterState, types.builtins));
    FIELD(InterpreterStaticTypesEnd, offsetof(PyInterpreterState, types.builtins)
                                         + sizeof(((PyInterpreterState *)0)->types.builtins));
    FIELD(StaticTypeStateSize, sizeof(static_builtin_state));
    FIELD(StaticTypeStateDict, offsetof(static_builtin_state, tp_dict));
    FIELD(StaticTypeStateType, offsetof(static_builtin_state, type));
#endif
    FIELD(TypeGetattro, offsetof(PyTypeObject, tp_getattro));
    FIELD(TypeDescrGet, offsetof(PyTypeObject, tp_descr_get));
    FIELD(TypeDescrSet, offsetof(PyTypeObject, tp_descr_set));
    FIELD(HeapTypeQualname, offsetof(PyHeapTypeObject, ht_qualname));
    FIELD(HeapTypeCachedKeys, offsetof(PyHeapTypeObject, ht_cached_keys));
    FIELD(HeapTypeSlots, offsetof(PyHeapTypeObject, ht_slots));
    FIELD(MemberDescrMember, offsetof(PyMemberDescrObject, d_member));
    FIELD(MemberDefType, offsetof(PyMemberDef, type));
    FIELD(MemberDefOffset, offsetof(PyMemberDef, offset));
    FIELD(CellRef, offsetof(PyCellObject, ob_ref));
    FIELD(SetUsed, offsetof(PySetObject, used));
    FIELD(SetMask, offsetof(PySetObject, mask));
    FIELD(SetTable, offsetof(PySetObject, table));
    FIELD(SetEntrySize, sizeof(setentry));
    FIELD(SetEntryKey, offsetof(setentry, key));
    FIELD(SetEntryHash, offsetof(setentry, hash));
    FIELD(ModuleDict, offsetof(PyModuleObject, md_dict));
    FIELD(FunctionQualname, offsetof(PyFunctionObject, func_qualname));
    FIELD(FunctionCode, offsetof(PyFunctionObject, func_code));
    FIELD(BuiltinMethod, offsetof(PyCFunctionObject, m_ml));
    FIELD(BuiltinSelf, offsetof(PyCFunctionObject, m_self));
    FIELD(MethodDefName, offsetof(PyMethodDef, ml_name));
    FIELD(MethodDefMeth, offsetof(PyMethodDef, ml_meth));
    FIELD(MethodDescrMethod, offsetof(PyMethodDescrObject, d_method));
    FIELD(WrapperDescrWrapped, offsetof(PyWrapperDescrObject, d_wrapped));
    FIELD(TupleItems, offsetof(PyTupleObject, ob_item));
    FIELD(TupleSize, offsetof(PyTupleObject, ob_base.ob_size));
    FIELD(ListItems, offsetof(PyListObject, ob_item));
    FIELD(ListSize, offsetof(PyListObject, ob_base.ob_size));
    FIELD(DictKeys, offsetof(PyDictObject, ma_keys));
    FIELD(DictValues, offsetof(PyDictObject, ma_values));
    FIELD(DictUsed, offsetof(PyDictObject, ma_used));
    FIELD(DictKeysLog2Size, offsetof(PyDictKeysObject, dk_log2_size));
    FIELD(DictKeysLog2IndexBytes, offsetof(PyDictKeysObject, dk_log2_index_bytes));
    FIELD(DictKeysKind, offsetof(PyDictKeysObject, dk_kind));
    FIELD(DictKeysNentries, offsetof(PyDictKeysObject, dk_nentries));
    FIELD(DictKeysIndices, offsetof(PyDictKeysObject, dk_indices));
    FIELD(DictEntrySize, sizeof(PyDictKeyEntry));
    FIELD(DictEntryHash, offsetof(PyDictKeyEntry, me_hash));
    FIELD(DictEntryKey, offsetof(PyDictKeyEntry, me_key));
    FIELD(DictEntryValue, offsetof(PyDictKeyEntry, me_value));
    FIELD(DictUnicodeEntrySize, sizeof(PyDictUnicodeEntry));
    FIELD(DictUnicodeEntryKey, offsetof(PyDictUnicodeEntry, me_key));
    FIELD(DictUnicodeEntryValue, offsetof(PyDictUnicodeEntry, me_value));
    FIELD(DictValuesValues, offsetof(PyDictValues, values));
#if PY_VERSION_HEX >= 0x030d0000
    FIELD(DictValuesCapacity, offsetof(PyDictValues, capacity));
    FIELD(DictValuesSize, offsetof(PyDictValues, size));
    FIELD(DictValuesValid, offsetof(PyDictValues, valid));
#endif
    FIELD(FloatValue, offsetof(PyFloatObject, ob_fval));
    FIELD(LongTag, offsetof(PyLongObject, long_value.lv_tag));
    FIELD(LongDigits, offsetof(PyLongObject, long_value.ob_digit));
    FIELD(BytesSize, offsetof(PyBytesObject, ob_base.ob_size));
    FIELD(BytesData, offsetof(PyBytesObject, ob_sval));
    FIELD(UnicodeState, offsetof(PyASCIIObject, state));
    FIELD(UnicodeHash, offsetof(PyASCIIObject, hash));
    FIELD(UnicodeLength, offsetof(PyASCIIObject, length));
    FIELD(UnicodeAsciiSize, sizeof(PyASCIIObject));
    FIELD(UnicodeCompactSize, sizeof(PyCompactUnicodeObject));
    FIELD(UnicodeData, offsetof(PyUnicodeObject, data));

    FIELD(DequeLeftBlock, offsetof(microscope_deque, leftblock));
    FIELD(DequeLeftIndex, offsetof(microscope_deque, leftindex));
    FIELD(DequeMaxlen, offsetof(microscope_deque, maxlen));
    FIELD(DequeObjectSize, sizeof(microscope_deque));
    FIELD(DequeBlockData, offsetof(microscope_block, data));
    FIELD(DequeBlockRightLink, offsetof(microscope_block, rightlink));
    FIELD(DefaultDictFactory, offsetof(microscope_defdict, default_factory));
    FIELD(DefaultDictObjectSize, sizeof(microscope_defdict));
    FIELD(OrderedDictFirst, offsetof(microscope_odict, od_first));
    FIELD(OrderedDictObjectSize, sizeof(microscope_odict));
    FIELD(OrderedDictNodeKey, offsetof(microscope_odict_node, key));
    FIELD(OrderedDictNodeHash, offsetof(microscope_odict_node, hash));
    FIELD(OrderedDictNodeNext, offsetof(microscope_odict_node, next));
    FIELD(DateHasTzinfo, offsetof(PyDateTime_Date, hastzinfo));
    FIELD(DateData, offsetof(PyDateTime_Date, data));
    FIELD(DateObjectSize, sizeof(PyDateTime_Date));
    FIELD(TimeData, offsetof(PyDateTime_Time, data));
    FIELD(TimeFold, offsetof(PyDateTime_Time, fold));
    FIELD(TimeTzinfo, offsetof(PyDateTime_Time, tzinfo));
    FIELD(TimeObjectSize, sizeof(PyDateTime_Time));
    FIELD(DateTimeData, offsetof(PyDateTime_DateTime, data));
    FIELD(DateTimeFold, offsetof(PyDateTime_DateTime, fold));
    FIELD(DateTimeTzinfo, offsetof(PyDateTime_DateTime, tzinfo));
    FIELD(DateTimeObjectSize, sizeof(PyDateTime_DateTime));
    FIELD(DeltaDays, offsetof(PyDateTime_Delta, days));
    FIELD(DeltaSeconds, offsetof(PyDateTime_Delta, seconds));
    FIELD(DeltaMicroseconds, offsetof(PyDateTime_Delta, microseconds));
    FIELD(DeltaObjectSize, sizeof(PyDateTime_Delta));
    FIELD(TimeZoneOffset, offsetof(microscope_timezone, offset));
    FIELD(TimeZoneName, offsetof(microscope_timezone, name));
    FIELD(TimeZoneObjectSize, sizeof(microscope_timezone));
    FIELD(DecimalFlags, offsetof(microscope_decimal, dec.flags));
    FIELD(DecimalExponent, offsetof(microscope_decimal, dec.exp));
    FIELD(DecimalDigits, offsetof(microscope_decimal, dec.digits));
    FIELD(DecimalLength, offsetof(microscope_decimal, dec.len));
    FIELD(DecimalData, offsetof(microscope_decimal, dec.data));
    FIELD(DecimalObjectSize, sizeof(microscope_decimal));
    printf("        ],\n");

    printf("        constants: &[\n");
#if PY_VERSION_HEX >= 0x030e0000
    CONSTANT(DebugStopBit, _PY_EVAL_PLEASE_STOP_BIT);
#endif
    CONSTANT(OwnerThread, FRAME_OWNED_BY_THREAD);
    CONSTANT(OwnerGenerator, FRAME_OWNED_BY_GENERATOR);
    CONSTANT(OwnerFrameObject, FRAME_OWNED_BY_FRAME_OBJECT);
#if PY_VERSION_HEX < 0x030e0000
    CONSTANT(OwnerEntry, FRAME_OWNED_BY_CSTACK);
#else
    CONSTANT(OwnerEntry, FRAME_OWNED_BY_INTERPRETER);
#endif
    emit_constant("UnicodeKindShift", kind_shift());
    emit_constant("UnicodeCompactShift", compact_shift());
    emit_constant("UnicodeAsciiShift", ascii_shift());
    CONSTANT(LongShift, PyLong_SHIFT);
    CONSTANT(LongNonSizeBits, _PyLong_NON_SIZE_BITS);
#if PY_VERSION_HEX >= 0x030e0000
    CONSTANT(StackRefs, 1);
#else
    CONSTANT(StackRefs, 0);
#endif
    CONSTANT(FastLocal, CO_FAST_LOCAL);
    CONSTANT(FastCell, CO_FAST_CELL);
    CONSTANT(FastFree, CO_FAST_FREE);
#ifdef CO_FAST_HIDDEN
    CONSTANT(FastHidden, CO_FAST_HIDDEN);
#else
    CONSTANT(FastHidden, 0);
#endif
    CONSTANT(Optimized, CO_OPTIMIZED);
    CONSTANT(VarArgs, CO_VARARGS);
    CONSTANT(VarKeywords, CO_VARKEYWORDS);
    CONSTANT(DictKeysGeneral, DICT_KEYS_GENERAL);
    CONSTANT(DictKeysUnicode, DICT_KEYS_UNICODE);
    CONSTANT(DictKeysSplit, DICT_KEYS_SPLIT);
    CONSTANT(TypeHeapBit, __builtin_ctzl(Py_TPFLAGS_HEAPTYPE));
    CONSTANT(TypeStaticBuiltinBit, __builtin_ctzl(_Py_TPFLAGS_STATIC_BUILTIN));
    CONSTANT(TypeTypeSubclassBit, __builtin_ctzl(Py_TPFLAGS_TYPE_SUBCLASS));
    CONSTANT(TypeManagedDictBit, __builtin_ctzl(Py_TPFLAGS_MANAGED_DICT));
#if PY_VERSION_HEX >= 0x030d0000
    CONSTANT(TypeInlineValuesBit, __builtin_ctzl(Py_TPFLAGS_INLINE_VALUES));
    CONSTANT(ManagedDictWords, -MANAGED_DICT_OFFSET / (Py_ssize_t)sizeof(PyObject *));
    CONSTANT(ManagedDictTagged, 0);
#else
    /* 3.12 keeps a dict or, tagged by its low bit, values, in the word
     * _PyObject_DictOrValuesPointer gives. */
    CONSTANT(ManagedDictWords, 3);
    CONSTANT(ManagedDictTagged, 1);
#endif
    CONSTANT(MemberObjectEx, Py_T_OBJECT_EX);
    CONSTANT(TypeImmutableBit, __builtin_ctzl(Py_TPFLAGS_IMMUTABLETYPE));
    CONSTANT(DequeBlockLength, MICROSCOPE_BLOCKLEN);
    /* A generator's state is an int8_t, negative before 3.15. 3.12 has no
     * state for a generator suspended in a yield from, so its are its
     * suspended state; the model reads what a generator awaits only from
     * 3.14, whose frames have a stack pointer. */
    CONSTANT(GeneratorCreated, (uint8_t)(int8_t)FRAME_CREATED);
    CONSTANT(GeneratorSuspended, (uint8_t)(int8_t)FRAME_SUSPENDED);
#if PY_VERSION_HEX < 0x030d0000
    CONSTANT(GeneratorYieldFrom, (uint8_t)(int8_t)FRAME_SUSPENDED);
#else
    CONSTANT(GeneratorYieldFrom, (uint8_t)(int8_t)FRAME_SUSPENDED_YIELD_FROM);
#endif
#if PY_VERSION_HEX >= 0x030f0000
    CONSTANT(GeneratorYieldFromLocked, (uint8_t)(int8_t)FRAME_SUSPENDED_YIELD_FROM_LOCKED);
#elif PY_VERSION_HEX >= 0x030d0000
    CONSTANT(GeneratorYieldFromLocked, (uint8_t)(int8_t)FRAME_SUSPENDED_YIELD_FROM);
#else
    CONSTANT(GeneratorYieldFromLocked, (uint8_t)(int8_t)FRAME_SUSPENDED);
#endif
    CONSTANT(GeneratorExecuting, (uint8_t)(int8_t)FRAME_EXECUTING);
    /* _PyGen_yf peeks at the top of the stack; from 3.15, GET_ITER pushes
     * the iterator's index above it, and gen_getyieldfrom peeks at 2. */
#if PY_VERSION_HEX >= 0x030f0000
    CONSTANT(GeneratorAwaitedDepth, 2);
#else
    CONSTANT(GeneratorAwaitedDepth, 1);
#endif
#if PY_VERSION_HEX >= 0x030e0000
    CONSTANT(FuturePending, MICROSCOPE_STATE_PENDING);
#else
    CONSTANT(FuturePending, 0);
#endif
    printf("        ],\n");

    printf("    },\n");
    return 0;
}
