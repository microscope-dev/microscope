/* SPDX-License-Identifier: MIT OR Apache-2.0 */
#ifndef MICROSCOPE_KERNEL_H
#define MICROSCOPE_KERNEL_H

#include <stdint.h>

#define MICROSCOPE_KERNEL_IMPORT(name) \
    __attribute__((import_module("microscope_kernel_v1"), import_name(name)))


#define MICROSCOPE_KERNEL_EXPORT __attribute__((export_name("run")))




MICROSCOPE_KERNEL_IMPORT("read")
int32_t microscope_read(uint64_t address, void *buffer, int32_t length);



MICROSCOPE_KERNEL_IMPORT("yield")
int32_t microscope_yield(const uint64_t *words, int32_t count);

static inline uint64_t microscope_load_u64(uint64_t address) {
    uint64_t value;
    microscope_read(address, &value, sizeof value);
    return value;
}

static inline uint32_t microscope_load_u32(uint64_t address) {
    uint32_t value;
    microscope_read(address, &value, sizeof value);
    return value;
}

static inline uint16_t microscope_load_u16(uint64_t address) {
    uint16_t value;
    microscope_read(address, &value, sizeof value);
    return value;
}

static inline uint8_t microscope_load_u8(uint64_t address) {
    uint8_t value;
    microscope_read(address, &value, sizeof value);
    return value;
}

#endif
