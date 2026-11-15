/* SPDX-License-Identifier: MIT OR Apache-2.0 */
#ifndef MICROSCOPE_VIEWS_H
#define MICROSCOPE_VIEWS_H


#define MICROSCOPE_VIEWS_FILE(path)                                        \
    __asm__(".pushsection .debug_microscope_views,\"\",@progbits\n"        \
            ".byte 1, 1\n"                                                 \
            ".long 8f - 7f\n"                                              \
            "7:\n"                                                         \
            ".incbin \"" path "\"\n"                                       \
            "8:\n"                                                         \
            ".popsection\n")



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
