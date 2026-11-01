/* Views for microscope, carried in a program's debug information.
 *
 * MICROSCOPE_VIEWS_FILE("views/app.views"); at file scope embeds a view file in
 * the .debug_microscope_views section of the object that expands it. microscope
 * presents the module's own types with those views, and no other module's.
 * MICROSCOPE_KERNEL("tree", "kernels/tree.c", "build/tree.wasm"); embeds a
 * kernel those views may call by its name, with its source, so that it is
 * reviewed as that; microscope_kernel.h says how to write one.
 * MICROSCOPE_VISUALIZER("chess-board", "views/chess-board.js"); embeds a
 * renderer those views' drawings may call by its name, which microscope's web
 * page runs in its sandbox; docs/visualizers.md says how to write one.
 *
 * The section is not loaded when the program runs, and strip --strip-debug
 * removes it with the rest of the debug information. The paths are the
 * assembler's: relative to the directory the compiler runs in, or to a
 * directory given with -Wa,-I.
 *
 * See microscope's docs/views.md for the views language and examples.
 *
 * SPDX-License-Identifier: MIT OR Apache-2.0
 */
#ifndef MICROSCOPE_VIEWS_H
#define MICROSCOPE_VIEWS_H

/* A record: kind 1 (view text), format 1, its length, then the file. */
#define MICROSCOPE_VIEWS_FILE(path)                                        \
    __asm__(".pushsection .debug_microscope_views,\"\",@progbits\n"        \
            ".byte 1, 1\n"                                                 \
            ".long 8f - 7f\n"                                              \
            "7:\n"                                                         \
            ".incbin \"" path "\"\n"                                       \
            "8:\n"                                                         \
            ".popsection\n")

/* A record: kind 2 (kernel), format 1, its length, then the name's length
 * and the name, the source's length and the source, and the module. */
#define MICROSCOPE_KERNEL(name, source, module)                            \
    __asm__(".pushsection .debug_microscope_views,\"\",@progbits\n"        \
            ".byte 2, 1\n"                                                 \
            ".long 8f - 7f\n"                                              \
            "7:\n"                                                         \
            ".short 4f - 3f\n"                                             \
            "3:\n"                                                         \
            ".ascii \"" name "\"\n"                                        \
            "4:\n"                                                         \
            ".long 6f - 5f\n"                                              \
            "5:\n"                                                         \
            ".incbin \"" source "\"\n"                                     \
            "6:\n"                                                         \
            ".incbin \"" module "\"\n"                                     \
            "8:\n"                                                         \
            ".popsection\n")

/* A record: kind 3 (renderer), format 1, its length, then the name's
 * length and the name, and the renderer's JavaScript. */
#define MICROSCOPE_VISUALIZER(name, path)                                  \
    __asm__(".pushsection .debug_microscope_views,\"\",@progbits\n"        \
            ".byte 3, 1\n"                                                 \
            ".long 8f - 7f\n"                                              \
            "7:\n"                                                         \
            ".short 4f - 3f\n"                                             \
            "3:\n"                                                         \
            ".ascii \"" name "\"\n"                                        \
            "4:\n"                                                         \
            ".incbin \"" path "\"\n"                                       \
            "8:\n"                                                         \
            ".popsection\n")

#endif
