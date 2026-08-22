// SPDX-FileCopyrightText: Copyright (C) 2026 Tahara Yoshinori
// SPDX-License-Identifier: GPL-3.0-or-later

#ifndef _ARA_SHIM_STDDEF_H
#define _ARA_SHIM_STDDEF_H
/* Minimal freestanding stddef.h shim for parsing ARAInterface.h with a
   resource-header-less libclang. Widths are fixed for the win64 (LLP64) target,
   which is the only target we regenerate ARA bindings for. */
typedef unsigned long long size_t;
typedef long long          ptrdiff_t;
typedef unsigned short     wchar_t;
#ifndef NULL
#define NULL ((void *)0)
#endif
#define offsetof(t, m) __builtin_offsetof(t, m)
#endif
