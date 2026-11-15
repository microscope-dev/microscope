





















import os

import gdb

SCALAR_CODES = {
    gdb.TYPE_CODE_INT,
    gdb.TYPE_CODE_PTR,
    gdb.TYPE_CODE_ENUM,
    gdb.TYPE_CODE_BOOL,
    gdb.TYPE_CODE_CHAR,
}



C_SOURCES = (".c", ".cc", ".cpp", ".cxx", ".h", ".hpp")


def describe(value, c_source):

    try:
        code = value.type.strip_typedefs().code
        if code == gdb.TYPE_CODE_STRUCT and c_source:
            return ("struct", "", list(members(value)))
        if value.is_optimized_out:
            return ("optimized-out", "", [])
        if code in SCALAR_CODES:
            return ("int", str(int(value)), [])
        if code == gdb.TYPE_CODE_FLT:
            return ("float", repr(float(value)), [])

        value.fetch_lazy()
        return ("other", "", [])
    except gdb.error:
        return ("error", "", [])


def members(value):

    for field in value.type.strip_typedefs().fields():
        if not field.name or field.is_base_class or not hasattr(field, "bitpos"):
            continue
        state, text, nested = describe(value[field], True)
        yield (field.name, state, text)
        for path, state, text in nested:
            yield (f"{field.name}.{path}", state, text)


def frame_lines(frame, level):
    name = frame.name() or ""
    try:
        block = frame.block()
    except RuntimeError:
        return [f"frame\t{level}\t{name}\tnoscope"]
    lines = [f"frame\t{level}\t{name}\tscope"]
    symtab = frame.find_sal().symtab
    c_source = symtab is not None and symtab.filename.endswith(C_SOURCES)
    while block is not None:
        for symbol in block:
            if not (symbol.is_argument or symbol.is_variable):
                continue
            kind = "argument" if symbol.is_argument else "local"
            try:
                value = symbol.value(frame)
            except gdb.error:
                lines.append(f"var\t{symbol.name}\t{kind}\terror\t")
                continue
            state, text, nested = describe(value, c_source)
            lines.append(f"var\t{symbol.name}\t{kind}\t{state}\t{text}")
            for path, state, text in nested:
                lines.append(f"member\t{symbol.name}.{path}\t{state}\t{text}")
        if block.function is not None:
            break
        block = block.superblock
    return lines


def main():
    gdb.execute("set pagination off")
    lines = ["microscope-frame-variables-oracle-v2"]
    threads = sorted(gdb.selected_inferior().threads(), key=lambda thread: thread.ptid[1])
    for thread in threads:
        thread.switch()
        lines.append(f"thread\t{thread.ptid[1]}")
        frame = gdb.newest_frame()
        level = 0
        while frame is not None:
            lines.extend(frame_lines(frame, level))
            try:
                frame = frame.older()
            except gdb.error:
                frame = None
            level += 1
    with open(os.environ["MICROSCOPE_FRAME_ORACLE"], "w", encoding="utf-8") as oracle:
        oracle.write("\n".join(lines) + "\n")


main()
