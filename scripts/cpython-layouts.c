/*
 * Prints one Generated struct literal for the CPython headers this was compiled
 * against: field offsets and sizes, and the frame, code, and unicode constants
 * the debugger reads. Driven by scripts/cpython-layouts.sh, which passes the
 * interpreter directory name as argv[1].
 */
#include <Python.h>

#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "internal/pycore_code.h"
#include "internal/pycore_dict.h"
#include "internal/pycore_long.h"
#include "internal/pycore_runtime.h"
#include "internal/pycore_interp.h"
#include "internal/pycore_unicodeobject.h"
#if PY_VERSION_HEX >= 0x030e0000
#include "internal/pycore_interpframe.h"
#else
#include "internal/pycore_frame.h"
#endif

static void fail(const char *what, unsigned long long value) {
    fprintf(stderr, "%s = %llu does not fit\n", what, value);
    exit(1);
}

/* A layout stores offsets and sizes as u16s. */
static void emit_field(const char *name, size_t value) {
    if (value > UINT16_MAX) {
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
    FIELD(VarObjectSize, offsetof(PyVarObject, ob_size));
    FIELD(TypeName, offsetof(PyTypeObject, tp_name));
    FIELD(TypeFlags, offsetof(PyTypeObject, tp_flags));
    FIELD(TypeBasicsize, offsetof(PyTypeObject, tp_basicsize));
    FIELD(TypeDictoffset, offsetof(PyTypeObject, tp_dictoffset));
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
    printf("        ],\n");

    printf("        constants: &[\n");
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
    printf("        ],\n");

    printf("    },\n");
    return 0;
}
