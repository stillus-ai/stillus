/* Copyright 2026 Evgeniy Udodov
 * SPDX-License-Identifier: GPL-3.0-only
 */

#ifndef STILLUS_MACOS_CROSS_STRING_H
#define STILLUS_MACOS_CROSS_STRING_H

#include <stddef.h>

int memcmp(const void *left, const void *right, size_t length);
void *memcpy(void *restrict destination, const void *restrict source, size_t length);
void *memmove(void *destination, const void *source, size_t length);
void *memset(void *destination, int value, size_t length);

#endif
