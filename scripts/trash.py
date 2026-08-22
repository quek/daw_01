#!/usr/bin/env python3
"""ファイル / ディレクトリを **ゴミ箱へ** 送る (rm -rf の代わり)。

`rm -rf` は復元手段が無い。こちらが実行する削除は既定で戻せるべき、という方針
(2026-08-22 にユーザー指摘: 「ファイル消した時ゴミ箱に入らないのよくないと思います」)。

    python scripts/trash.py <path> [<path> ...]

stdlib のみ。Windows は `SHFileOperationW` + `FOF_ALLOWUNDO`、それ以外は
freedesktop.org の Trash spec (`~/.local/share/Trash`) に自前で入れる。

**ゴミ箱に入れても disk は空かない。** 巨大なビルド成果物を大量に流すと
ゴミ箱が膨れるので、そういうものは意図して `rm -rf` を選ぶこと (guard が
一度止めるので、そこで意図を確認できる)。
"""

import os
import sys


def _trash_windows(paths):
    import ctypes
    from ctypes import wintypes

    FO_DELETE = 3
    FOF_SILENT = 0x0004
    FOF_NOCONFIRMATION = 0x0010
    FOF_ALLOWUNDO = 0x0040  # ← これが「ゴミ箱へ」の本体
    FOF_NOERRORUI = 0x0400

    class SHFILEOPSTRUCTW(ctypes.Structure):
        _fields_ = [
            ("hwnd", wintypes.HWND),
            ("wFunc", wintypes.UINT),
            ("pFrom", wintypes.LPCWSTR),
            ("pTo", wintypes.LPCWSTR),
            ("fFlags", ctypes.c_uint16),
            ("fAnyOperationsAborted", wintypes.BOOL),
            ("hNameMappings", ctypes.c_void_p),
            ("lpszProgressTitle", wintypes.LPCWSTR),
        ]

    shell32 = ctypes.WinDLL("shell32", use_last_error=True)
    shell32.SHFileOperationW.argtypes = [ctypes.POINTER(SHFILEOPSTRUCTW)]
    shell32.SHFileOperationW.restype = ctypes.c_int

    # pFrom は **二重 NUL 終端**の連結リスト。単一 NUL だと隣の領域まで読む。
    joined = "\0".join(os.path.abspath(p) for p in paths) + "\0\0"
    op = SHFILEOPSTRUCTW(
        hwnd=None,
        wFunc=FO_DELETE,
        pFrom=joined,
        pTo=None,
        fFlags=FOF_ALLOWUNDO | FOF_NOCONFIRMATION | FOF_SILENT | FOF_NOERRORUI,
        fAnyOperationsAborted=False,
        hNameMappings=None,
        lpszProgressTitle=None,
    )
    rc = shell32.SHFileOperationW(ctypes.byref(op))
    if rc != 0:
        raise OSError(f"SHFileOperationW failed (code {rc:#x})")
    if op.fAnyOperationsAborted:
        raise OSError("SHFileOperationW: 中断された")


def _trash_xdg(paths):
    """freedesktop.org Trash spec の最小実装 (同一ファイルシステム内のみ)。"""
    import shutil
    import time
    import urllib.parse

    home = os.path.expanduser("~")
    trash = os.path.join(
        os.environ.get("XDG_DATA_HOME", os.path.join(home, ".local", "share")), "Trash"
    )
    files_dir, info_dir = os.path.join(trash, "files"), os.path.join(trash, "info")
    os.makedirs(files_dir, exist_ok=True)
    os.makedirs(info_dir, exist_ok=True)

    for p in paths:
        src = os.path.abspath(p)
        base = os.path.basename(src.rstrip("/"))
        name, n = base, 1
        # 同名が既にゴミ箱にあれば連番を振る (spec の要求)。
        while os.path.lexists(os.path.join(files_dir, name)):
            name = f"{base}.{n}"
            n += 1
        with open(os.path.join(info_dir, name + ".trashinfo"), "w", encoding="utf-8") as fh:
            fh.write("[Trash Info]\n")
            fh.write("Path=" + urllib.parse.quote(src) + "\n")
            fh.write("DeletionDate=" + time.strftime("%Y-%m-%dT%H:%M:%S") + "\n")
        shutil.move(src, os.path.join(files_dir, name))


def main(argv):
    if not argv:
        print(__doc__.strip(), file=sys.stderr)
        return 2
    missing = [p for p in argv if not os.path.lexists(p)]
    if missing:
        for p in missing:
            print(f"trash: 存在しません: {p}", file=sys.stderr)
        return 1
    (_trash_windows if os.name == "nt" else _trash_xdg)(argv)
    for p in argv:
        print(f"trash: ゴミ箱へ送りました: {p}")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
