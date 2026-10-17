/* A C extension that holds Python objects in a native frame's locals while
 * it calls back into Python, so that the frame, below the Python frames the
 * callback runs, shows each PyObject * as the Python value it points to.
 * Built against each interpreter's own headers. */

#define PY_SSIZE_T_CLEAN
#include <Python.h>

/* inspect(point, numbers, mapping, nothing, callback): calls callback with
 * no arguments while its locals hold the other four and a null object, and
 * returns what the callback returns. */
static PyObject *inspect(PyObject *module, PyObject *const *args, Py_ssize_t count) {
    (void)module;
    if (count != 5) {
        PyErr_SetString(PyExc_TypeError, "inspect takes five arguments");
        return NULL;
    }

    PyObject *point = args[0];
    PyObject *numbers = args[1];
    PyObject *mapping = args[2];
    PyObject *nothing = args[3];
    PyObject *missing = NULL;
    PyObject *result = PyObject_CallNoArgs(args[4]);

    /* The locals stay in the frame, where the debugger reads them, until
     * the callback returns. */
    __asm__ volatile("" : : "g"(point), "g"(numbers), "g"(mapping), "g"(nothing), "g"(missing)
                     : "memory");
    return result;
}

static PyMethodDef methods[] = {
    {"inspect", (PyCFunction)(void (*)(void))inspect, METH_FASTCALL, NULL},
    {NULL, NULL, 0, NULL},
};

static PyModuleDef_Slot slots[] = {
#ifdef Py_GIL_DISABLED
    {Py_mod_gil, Py_MOD_GIL_NOT_USED},
#endif
    {0, NULL},
};

static struct PyModuleDef module = {
    PyModuleDef_HEAD_INIT,
    .m_name = "microscope_extension",
    .m_methods = methods,
    .m_slots = slots,
};

PyMODINIT_FUNC PyInit_microscope_extension(void) {
    return PyModuleDef_Init(&module);
}
