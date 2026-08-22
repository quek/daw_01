// SPDX-FileCopyrightText: Copyright (C) 2026 Tahara Yoshinori
// SPDX-License-Identifier: GPL-3.0-or-later

/* bindgen entry point for the ARA C API.
   Only the core ARA interface is bound here; the CLAP/VST3 ARA companion
   structs (ARACLAP.h / ARAVST3.h) are tiny and hand-written in daw_plugin_host
   where clap-sys / vst3 types are available. */
#include "vendor/ARA_API/ARAInterface.h"
