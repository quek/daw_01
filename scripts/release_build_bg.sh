#!/bin/sh
# Detached background release build, launched by .githooks/lib/release_build_trigger.sh after
# every commit/merge ON main. Builds only the 3 runtime exes. PowerShell-free (bash; no JSON
# to parse, so no Python needed). Replaces release_build_bg.ps1.
#
# Appends all cargo output to target/release-build.log. On failure writes the marker
# target/.release-build-failed (the GUI shows a banner when it sees this). Exit code is not
# consumed by git (this process is detached).
#
# usage: release_build_bg.sh [<repo>]
set -u

repo="${1:-$(cd "$(dirname "$0")/.." && pwd)}"
target="$repo/target"
mkdir -p "$target"
log="$target/release-build.log"
marker="$target/.release-build-failed"
rm -f "$marker"

sha="$(git -C "$repo" rev-parse --short HEAD 2>/dev/null)"
printf '[post-commit] release build start %s at %s\n' "$sha" "$(date '+%Y-%m-%d %H:%M:%S')" > "$log"

# Build only the 3 runtime exes (examples are not needed to run the DAW and dominate link time).
( cd "$repo" && cargo build --release -p daw_gui -p daw_audio -p daw_plugin_host >> "$log" 2>&1 )
code=$?

if [ "$code" -ne 0 ]; then
  printf '[post-commit] release build FAILED (exit %s) at %s\n' "$code" "$(date '+%Y-%m-%d %H:%M:%S')" >> "$log"
  : > "$marker"
else
  printf '[post-commit] release build OK at %s\n' "$(date '+%Y-%m-%d %H:%M:%S')" >> "$log"
fi
