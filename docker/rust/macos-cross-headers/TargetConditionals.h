/* Copyright 2026 Evgeniy Udodov
 * SPDX-License-Identifier: GPL-3.0-only
 */

#ifndef STILLUS_MACOS_CROSS_TARGET_CONDITIONALS_H
#define STILLUS_MACOS_CROSS_TARGET_CONDITIONALS_H

/* Minimal SDK compatibility header used only by the Linux cargo-check target. */
#define TARGET_OS_MAC 1
#define TARGET_OS_OSX 1
#define TARGET_OS_IPHONE 0
#define TARGET_OS_IOS 0
#define TARGET_OS_TV 0
#define TARGET_OS_WATCH 0

#endif
