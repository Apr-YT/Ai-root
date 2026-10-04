#!/usr/bin/env bash
# AI-Root 构建脚本（Git Bash / MSYS 用）
#
# 设计原则（2026-10-04 重构，从 212 行削至约 60 行）：
#   一、**只做与机器相关的事**：找出本机完整 MinGW binutils 在哪，注入 PATH。
#       其余（target-dir、链接器、rustflags）全部由 .cargo/config.toml 承载。
#   二、**不再需要外部 CARGO_TARGET_DIR**。仓库已迁至纯 ASCII 路径，直接用仓库内
#       target/。此前"仓库路径含中文 → ld 找不到产物"的问题已随路径 ASCII 化消失。
#   三、**不再校验 ASCII**。路径已是 ASCII，校验逻辑随之删除。
#
# 为什么还需要这个脚本（不能直接 `cargo build`）：
#   gnu 目标链接期需要完整 MinGW binutils（as/dlltool/ld）。rustup 自带的
#   self-contained 目录**缺 as.exe**，直接 cargo build 会在链接期报
#   "error calling dlltool: program not found"。本脚本负责把外部 binutils 注入 PATH。
#
# binutils 查找顺序（本机路径曾变更，故用候选探测而非硬编码）：
#   1) 环境变量 MINGW_BINUTILS_BIN（最高优先级，用于覆盖）
#   2) D:/Kiln/mingw64/bin          （本机当前实际位置）
#   3) D:/Kiln/mingw-binutils/bin   （旧位置，保留兼容）
#
# rust 工具链查找顺序（本机曾从 D:/software/rust 迁至 D:/Kiln/rust）：
#   1) 环境变量 AIROOT_RUST_ROOT（最高优先级）
#   2) 环境变量 CARGO_HOME/RUSTUP_HOME 推导（**校验 toolchain 真实存在**，
#      因为已运行的 shell 会话可能带着迁移前的陈旧值）
#   3) D:/Kiln/rust → D:/software/rust 候选探测

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$REPO_ROOT"

# Windows 形式路径 → POSIX（Git Bash 的 PATH 只认 POSIX 形式）
to_posix() { printf '%s' "$1" | sed -E 's#^([A-Za-z]):#/\L\1#'; }

# ---------- 1) 定位 rust 工具链根目录 ----------
TOOLCHAIN_REL="rustup/toolchains/stable-x86_64-pc-windows-gnu/bin/cargo.exe"
RUST_ROOT=""

# 1a) 显式环境变量优先
if [ -n "${AIROOT_RUST_ROOT:-}" ]; then
  if [ -x "$(to_posix "$AIROOT_RUST_ROOT")/$TOOLCHAIN_REL" ]; then
    RUST_ROOT="$AIROOT_RUST_ROOT"
  else
    echo "[X] AIROOT_RUST_ROOT=$AIROOT_RUST_ROOT 下未找到 gnu toolchain" >&2
    exit 1
  fi
fi

# 1b) 从 CARGO_HOME/RUSTUP_HOME 推导（必须校验，陈旧值不可信）
if [ -z "$RUST_ROOT" ]; then
  declared=""
  if [ -n "${RUSTUP_HOME:-}" ]; then
    declared="$(dirname "$(to_posix "$RUSTUP_HOME")")"
  elif [ -n "${CARGO_HOME:-}" ]; then
    declared="$(dirname "$(to_posix "$CARGO_HOME")")"
  fi
  if [ -n "$declared" ] && [ -x "$declared/$TOOLCHAIN_REL" ]; then
    # 转回大写盘符的 Windows 形式（rustc 的路径处理区分盘符大小写）
    RUST_ROOT="$(printf '%s' "$declared" | sed -E 's#^/([a-zA-Z])/#\U\1:/#')"
  fi
fi

# 1c) 候选探测
if [ -z "$RUST_ROOT" ]; then
  for cand in "D:/Kiln/rust" "D:/software/rust"; do
    if [ -x "$(to_posix "$cand")/$TOOLCHAIN_REL" ]; then RUST_ROOT="$cand"; break; fi
  done
fi

if [ -z "$RUST_ROOT" ]; then
  echo "[X] 未找到 rustup gnu toolchain (stable-x86_64-pc-windows-gnu)" >&2
  echo "    已尝试: AIROOT_RUST_ROOT, \$CARGO_HOME/\$RUSTUP_HOME 推导, D:/Kiln/rust, D:/software/rust" >&2
  echo "    若未安装: rustup toolchain install stable-x86_64-pc-windows-gnu" >&2
  exit 1
fi

# 导出为**大写盘符 Windows 形式**：cargo/rustc 需要它定位 registry，
# 且大写形式可让 rustc 的路径重映射行为稳定（小写盘符会不一致）。
export CARGO_HOME="$(printf '%s' "$RUST_ROOT" | sed -E 's#^([a-z]):#\U\1:#')/cargo"
export RUSTUP_HOME="$(printf '%s' "$RUST_ROOT" | sed -E 's#^([a-z]):#\U\1:#')/rustup"

TOOLCHAIN_BIN="$(to_posix "$RUST_ROOT")/rustup/toolchains/stable-x86_64-pc-windows-gnu/bin"
CARGO_EXE="$TOOLCHAIN_BIN/cargo.exe"
[ -x "$CARGO_EXE" ] || { echo "[X] cargo.exe 不可执行: $CARGO_EXE" >&2; exit 1; }

# ---------- 2) 定位完整 MinGW binutils ----------
BINUTILS_BIN="${MINGW_BINUTILS_BIN:-}"
if [ -z "$BINUTILS_BIN" ]; then
  for cand in "D:/Kiln/mingw64/bin" "D:/Kiln/mingw-binutils/bin"; do
    if [ -x "$(to_posix "$cand")/as.exe" ]; then BINUTILS_BIN="$cand"; break; fi
  done
fi

if [ -z "$BINUTILS_BIN" ] || [ ! -x "$(to_posix "$BINUTILS_BIN")/as.exe" ]; then
  echo "[X] 未找到含 as.exe 的完整 MinGW binutils（gnu 链接期必需）" >&2
  echo "    已尝试: MINGW_BINUTILS_BIN, D:/Kiln/mingw64/bin, D:/Kiln/mingw-binutils/bin" >&2
  echo "    请下载 binutils 并设置 MINGW_BINUTILS_BIN 指向其 bin 目录" >&2
  exit 1
fi

# ---------- 3) 注入 PATH 并构建 ----------
# 顺序关键: binutils 在前，确保 dlltool/as/ld 用完整版而非 self-contained 残缺版。
# MSYS_NO_PATHCONV=1 防止 MSYS 把 cargo 的参数当路径转换。
export MSYS_NO_PATHCONV=1
export PATH="$(to_posix "$BINUTILS_BIN"):$TOOLCHAIN_BIN:$PATH"

# 产物目录: 环境变量 CARGO_TARGET_DIR 优先(cargo 的约定), 否则用仓库内 target/。
# CI 会设置该变量到 runner 临时目录, 故此处必须同样遵守, 否则报错路径与实际不符。
TARGET_DIR="${CARGO_TARGET_DIR:-$REPO_ROOT/target}"
EXE="$(to_posix "$TARGET_DIR")/x86_64-pc-windows-gnu/release/AI-Root.exe"

echo "[*] rust 根目录 : $RUST_ROOT"
echo "[*] binutils    : $BINUTILS_BIN"
echo "[*] cargo       : $CARGO_EXE"
echo "[*] 产物目录    : $TARGET_DIR"

"$CARGO_EXE" build --release --target x86_64-pc-windows-gnu "$@"

[ -f "$EXE" ] || { echo "[X] 构建未产出 exe: $EXE" >&2; exit 1; }
echo "[OK] 产物: $EXE"
echo "     SHA256: $(sha256sum "$EXE" | cut -d' ' -f1)"
