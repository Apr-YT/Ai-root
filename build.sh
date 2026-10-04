#!/usr/bin/env bash
# AI-Root 本机构建脚本(Git Bash / MSYS 环境)
#
# 为什么本机用本脚本而不是 build.ps1:
#   本机为「gnu-only」rustup 工具链 + 无 MSVC。gnu 目标链接期需要完整的 MinGW
#   binutils(as/dlltool/ld)。实测在 PowerShell 下调用这些 binutils 会出现
#   静默失败 / "failed to add native library ... os error 2"; 而在 Git Bash 下
#   完全正常。故本机开发环境使用本脚本; CI(GitHub Actions) 走 build.ps1 + msvc。
#
# 用法:  bash build.sh
# 产物:  $CARGO_TARGET_DIR/x86_64-pc-windows-gnu/release/AI-Root.exe
#
# 前置:
#   - rustup toolchain: stable-x86_64-pc-windows-gnu
#   - 完整 MinGW binutils(含 as.exe) 位于 D:\Kiln\mingw-binutils\bin
#     获取: 清华 msys2 镜像 mingw-w64-x86_64-binutils-*.pkg.tar.zst, 解压后
#           把 mingw64/bin 内容放入 D:\Kiln\mingw-binutils\bin
#
# ── 关键坑(2026-10-04 实测定位, 勿回退) ────────────────────────────────────
#   MSYS/Git Bash 的 PATH **查找** 只认 POSIX 形式路径 (/d/Kiln/...), 不认
#   Windows 形式 (D:/Kiln/... 或 D:\Kiln\...)。把 Windows 形式写进 PATH 后,
#   `command -v dlltool.exe` / `which` 一律返回 "not found"(注意不是
#   Permission denied, 是彻底查不到), 于是:
#     1) cargo 解析回退到 PATH 末尾的 C:\...\rust\cargo\bin —— 那是 rustup 的
#        12721664 字节 shim(rustup.exe 的硬链接, 全部同 MD5 b1472212...), 并非
#        真实 cargo;
#     2) shim 转发给真实 cargo/rustc 时 PATH 被裁剪, rustc 找不到 dlltool,
#        报 "error calling dlltool 'dlltool.exe': program not found"。
#   因此本脚本: 所有加入 PATH 的路径必须先转 POSIX 形式(to_posix), 且 cargo
#   直接指向 toolchain 内的真实 cargo.exe, 不再依赖 PATH 解析。

set -euo pipefail

# 把 Windows 路径(D:/x 或 D:\x)转成 MSYS 能正确做 PATH 查找的 POSIX 形式(/d/x)。
# 已是 POSIX 形式(以 / 开头)或空串则原样返回。
to_posix() {
  local p="$1"
  [ -z "$p" ] && { printf '%s' ""; return; }
  case "$p" in
    /*) printf '%s' "$p"; return ;;   # 已是 POSIX 绝对路径
  esac
  # D:/a/b 或 D:\a\b -> /d/a/b
  local drive rest
  drive="$(printf '%s' "$p" | cut -c1 | tr 'A-Z' 'a-z')"
  rest="$(printf '%s' "$p" | cut -c3- | tr '\\' '/')"
  printf '/%s/%s' "$drive" "${rest#/}"
}

# 允许外部覆盖; 默认与本机一致(均为 Windows 形式, 后面统一转 POSIX)
CARGO_HOME_WIN="${CARGO_HOME:-D:/software/rust/cargo}"
RUSTUP_HOME_WIN="${RUSTUP_HOME:-D:/software/rust/rustup}"
CARGO_TARGET_DIR_WIN="${CARGO_TARGET_DIR:-D:/AI-Root-target}"
BINUTILS_BIN_WIN="${MINGW_BINUTILS_BIN:-D:/Kiln/mingw-binutils/bin}"

CARGO_HOME_POSIX="$(to_posix "$CARGO_HOME_WIN")"
RUSTUP_HOME_POSIX="$(to_posix "$RUSTUP_HOME_WIN")"
CARGO_TARGET_DIR_POSIX="$(to_posix "$CARGO_TARGET_DIR_WIN")"
BINUTILS_BIN_POSIX="$(to_posix "$BINUTILS_BIN_WIN")"

TOOLCHAIN_DIR="$RUSTUP_HOME_POSIX/toolchains/stable-x86_64-pc-windows-gnu"
TOOLCHAIN_BIN="$TOOLCHAIN_DIR/bin"

if [ ! -x "$BINUTILS_BIN_POSIX/as.exe" ]; then
  echo "[X] 未找到完整 MinGW binutils: $BINUTILS_BIN_POSIX/as.exe" >&2
  echo "    获取方式见本脚本头部注释。" >&2
  exit 1
fi
if [ ! -x "$TOOLCHAIN_BIN/cargo.exe" ]; then
  echo "[X] 未找到 rustup gnu toolchain: $TOOLCHAIN_BIN" >&2
  echo "    请先执行: rustup toolchain install stable-x86_64-pc-windows-gnu" >&2
  exit 1
fi

# 顺序关键: binutils 在前(确保 dlltool/as 用完整版而非 self-contained 残缺版),
# toolchain bin 紧随其后。
export PATH="$BINUTILS_BIN_POSIX:$TOOLCHAIN_BIN:$PATH"
hash -r 2>/dev/null || true

# 直接使用真实 cargo(绝对 POSIX 路径), 绕开 rustup shim, 避免其裁剪子进程 PATH。
CARGO_BIN="$TOOLCHAIN_BIN/cargo.exe"

# 自检: 确认 rustc 能在当前 PATH 下找到 dlltool(否则提前报清晰错误, 而不是
# 等 chrono 编译到一半才报 "program not found")。
if ! command -v dlltool.exe >/dev/null 2>&1; then
  echo "[X] 当前 PATH 下找不到 dlltool.exe(gnu 链接期必需)" >&2
  echo "    PATH 首段: $BINUTILS_BIN_POSIX" >&2
  exit 1
fi

# 传给 cargo/rustc 的环境变量统一用 MSYS 原生格式, 避免混用两种路径风格
export CARGO_HOME="$CARGO_HOME_POSIX"
export RUSTUP_HOME="$RUSTUP_HOME_POSIX"
export CARGO_TARGET_DIR="$CARGO_TARGET_DIR_POSIX"

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$REPO_ROOT"

echo "[*] cargo     : $CARGO_BIN"
echo "[*] toolchain : $TOOLCHAIN_DIR"
echo "[*] binutils  : $BINUTILS_BIN_POSIX"
echo "[*] dlltool   : $(command -v dlltool.exe)"
echo "[*] target dir: $CARGO_TARGET_DIR"

"$CARGO_BIN" build --release --target x86_64-pc-windows-gnu

EXE="$CARGO_TARGET_DIR/x86_64-pc-windows-gnu/release/AI-Root.exe"
if [ -f "$EXE" ]; then
  echo "[OK] 产物: $EXE"
  ls -la "$EXE"
  sha256sum "$EXE" || true
else
  echo "[X] 构建未产出 exe" >&2
  exit 1
fi
