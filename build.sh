#!/usr/bin/env bash
# AI-Root 构建脚本（Windows + Git Bash / MSYS 用）
#
# ═══════════════════════════════════════════════════════════════════════════
#  快速使用
#    本机已装 Rust（rustup）          → bash build.sh
#    msvc 也可（需 VS Build Tools）   → powershell -File build.ps1
#    gnu 且缺 MinGW binutils          → 见下方「gnu 前置」，或改用 msvc
#
#  依赖
#    - rustup + 任一下载目标（推荐 msvc：装 VS Build Tools 即可，无需额外配置）
#
#  环境变量（可选，均有自动探测，正常无需设置）
#    AIROOT_RUST_ROOT      rust 根目录（含 cargo/ 与 rustup/）
#    AIROOT_TARGET         目标三元组（默认 x86_64-pc-windows-gnu）
#    AIROOT_TOOLCHAIN      首选 host 工具链名（默认 stable-<target>；找不到会自动
#                          改用其它已装该 target 的工具链，故一般不设）
#    MINGW_BINUTILS_BIN    MinGW binutils 的 bin 目录（仅 gnu 目标需要）
#    CARGO_TARGET_DIR      产物目录（默认 <仓库>/target）
# ═══════════════════════════════════════════════════════════════════════════
#
# 设计原则
#   一、**优先向系统自省，而非猜测路径**。rust 根由 `rustup show home` 反推，
#       工具链在 `toolchains/` 下按「是否已装目标 target」挑选 —— 只要 rustup
#       在 PATH 中即可自举，不需要任何硬编码路径。硬编码的 "D:/Kiln/..." 仅作为
#       **最后兜底**保留（本机私有位置；他人机器上不命中也不影响）。
#   二、配置下沉到 .cargo/config.toml（target-dir 等稳定配置的唯一来源）。
#       本脚本只做与机器相关的事：定位工具链、注入 binutils、调 cargo。
#   三、**报错必须可行动**：依赖缺失时直接打印可复制执行的安装命令，
#       而不是让用户对着 "program not found" 猜。
#
# 关于 host toolchain 与 target（重要，CI 上踩过坑）
#   host 工具链名**不必**等于目标三元组：
#     rustup toolchain install stable --target x86_64-pc-windows-gnu
#   装出的是 stable-x86_64-pc-windows-**msvc**（host 不变）+ 附加 gnu target，
#   此时用 msvc 的 cargo 做 cross-compile 完全可行。故本脚本按「已装该 target」
#   挑选工具链，而非按目录名硬匹配。
#
# 关于 gnu 目标为什么需要额外 binutils
#   rustup 的 self-contained 目录**缺 as.exe**（实测只有 dlltool/ld/gcc），
#   直接 `cargo build --target ...-gnu` 会在链接期报 "error calling dlltool"。
#   故 gnu 目标需要外部完整 MinGW binutils。**msvc 目标无此问题**。

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$REPO_ROOT"

TARGET_NAME="${AIROOT_TARGET:-x86_64-pc-windows-gnu}"
# ⚠ 区分两个概念（易错）：
#   TARGET_NAME     = rustc 的**目标三元组**，如 x86_64-pc-windows-gnu
#   TOOLCHAIN_NAME  = 首选 host **工具链名**，如 stable-x86_64-pc-windows-gnu
#   二者不同名（后者多 "stable-" 前缀）。**但工具链名不必与目标同名** ——
#   CI 上装的是 stable-x86_64-pc-windows-msvc + gnu target，同样能编 gnu 产物。
#   故 TOOLCHAIN_NAME 仅作为**首选**，实际采用哪个由 pick_toolchain 决定。
TOOLCHAIN_NAME="${AIROOT_TOOLCHAIN:-stable-$TARGET_NAME}"

# ---------- 工具函数 ----------
# Windows 形式 → POSIX（Git Bash 的 PATH 只认 POSIX 形式）
to_posix() { printf '%s' "$1" | sed -E 's#^([A-Za-z]):#/\L\1#' | tr '\\' '/'; }
# POSIX → 大写盘符 Windows 形式（rustc 路径处理区分盘符大小写）
to_windows() { printf '%s' "$1" | sed -E 's#^/([a-zA-Z])/#\U\1:/#' | tr '/' '\\'; }

die() { echo "[X] $*" >&2; exit 1; }
info() { echo "[*] $*"; }

# ---------- 0) 前置依赖自检 ----------
# 目的：依赖缺失时给出**可复制执行**的修复命令，而不是晦涩的链接期报错。
RUSTUP_BIN="$(command -v rustup.exe 2>/dev/null || command -v rustup 2>/dev/null || true)"

# rustup 不在 PATH 时，仍可从 CARGO_HOME 找到（新装/迁移后常见：
# 会话 PATH 未刷新，但环境变量已更新）。
if [ -z "$RUSTUP_BIN" ]; then
  for home in "${CARGO_HOME:-}" "${RUSTUP_HOME:-}"; do
    [ -z "$home" ] && continue
    cand="$(to_posix "$home")/bin/rustup.exe"
    if [ -x "$cand" ]; then RUSTUP_BIN="$cand"; break; fi
    cand="$(to_posix "$home")/rustup.exe"
    if [ -x "$cand" ]; then RUSTUP_BIN="$cand"; break; fi
  done
fi
# 最后兜底：本机私有位置（他人机器不命中）
if [ -z "$RUSTUP_BIN" ]; then
  for cand in "D:/Kiln/rust/cargo/bin/rustup.exe" "D:/software/rust/cargo/bin/rustup.exe"; do
    if [ -x "$(to_posix "$cand")" ]; then RUSTUP_BIN="$(to_posix "$cand")"; break; fi
  done
fi

if [ -z "$RUSTUP_BIN" ]; then
  die "未找到 rustup。
    请先安装 Rust（含 rustup）：
      winget install Rustlang.Rustup
    或访问 https://rustup.rs 下载安装。安装后重开终端再试。"
fi

# ---------- 1) 定位 rust 根目录（自省优先） ----------
# 顺序：显式环境变量 → rustup 自报 → 环境变量推导 → 本机兜底
#
# ⚠ 关键教训一：**所有候选都必须校验真实存在**才采用。
#   `rustup show home` 只是**回显 $RUSTUP_HOME 环境变量**，并不验证其存在性；
#   新装/迁移后会话里常残留陈旧值（如 D:\software\rust\rustup，目录已删），
#   若直接采信会得到"工具链不存在"的假结论。凡涉及环境变量的候选一律校验。
#
# ⚠ 关键教训二（CI 上惨痛验证）：**host toolchain 名 ≠ 目标三元组**。
#   `rustup toolchain install stable --target x86_64-pc-windows-gnu` 装出来的是
#   stable-x86_64-pc-windows-**msvc**（host 不变），只是**附加**了 gnu target。
#   此时 `toolchains/` 下**没有** stable-x86_64-pc-windows-gnu 目录，却能正常
#   执行 `cargo build --target x86_64-pc-windows-gnu`（cross-compile）。
#   故此处**不能**要求目录名等于目标三元组 —— 只要有任一 toolchain 已装该
#   target 即可。否则在 CI 上必然自省失败（曾因此让 gnu job 挂掉）。
#
# 内部统一用 POSIX 形式保存，仅在需要时转 Windows 形式，避免反斜杠转义问题。
RUST_ROOT_POSIX=""

# 校验：给一个候选根目录，是否具备可编 $TARGET_NAME 的 toolchain？
# 返回 0 = 可用；同时把选中的 toolchain 目录名写入全局 TOOLCHAIN_DIR_NAME。
TOOLCHAIN_DIR_NAME=""
# 探测某 toolchain 是否已装目标 target（rustlib 下有对应目录即算装好）
toolchain_has_target() {
  [ -d "$1/lib/rustlib/$TARGET_NAME" ]
}
# 在给定 rust 根下挑一个可用 toolchain：优先同名 host，其次任意已装该 target 的
pick_toolchain() {
  local root="$1" tdirs d name
  tdirs="$root/rustup/toolchains"
  [ -d "$tdirs" ] || return 1
  # a) 首选 host 与目标同名（如本机 gnu-only 环境）
  if [ -x "$tdirs/$TOOLCHAIN_NAME/bin/cargo.exe" ] && toolchain_has_target "$tdirs/$TOOLCHAIN_NAME"; then
    TOOLCHAIN_DIR_NAME="$TOOLCHAIN_NAME"; return 0
  fi
  # b) 否则扫描所有 toolchain，取第一个已装该 target 的（CI 的 cross-compile 情形）
  for d in "$tdirs"/*/; do
    name="$(basename "$d")"
    if [ -x "$d/bin/cargo.exe" ] && toolchain_has_target "$d"; then
      TOOLCHAIN_DIR_NAME="$name"; return 0
    fi
  done
  return 1
}
# 校验一个候选 rust 根是否可用（同时确定 toolchain 目录）
probe_rust_root() {
  local root="$1"
  [ -d "$root/cargo" ] || return 1
  [ -d "$root/rustup/toolchains" ] || return 1
  TOOLCHAIN_DIR_NAME=""
  pick_toolchain "$root"
}

# 1a) 显式指定（最高优先级，用于覆盖探测结果）
if [ -n "${AIROOT_RUST_ROOT:-}" ]; then
  cand="$(to_posix "$AIROOT_RUST_ROOT")"
  if probe_rust_root "$cand"; then
    RUST_ROOT_POSIX="$cand"
  else
    die "AIROOT_RUST_ROOT=$AIROOT_RUST_ROOT 下未找到可编 $TARGET_NAME 的 toolchain。
    请确认该目录含 cargo/ 与 rustup/，且已安装对应工具链：
      rustup target add $TARGET_NAME"
  fi
fi

# 1b) 让 rustup 自报家目录（可移植关键：不依赖硬编码路径）
if [ -z "$RUST_ROOT_POSIX" ]; then
  rustup_home="$("$RUSTUP_BIN" show home 2>/dev/null | head -1 | tr -d '\r\n' || true)"
  if [ -n "$rustup_home" ]; then
    cand="$(dirname "$(to_posix "$rustup_home")")"
    probe_rust_root "$cand" && RUST_ROOT_POSIX="$cand"
  fi
fi

# 1c) 回退：从 CARGO_HOME/RUSTUP_HOME 推导（同样必须校验）
if [ -z "$RUST_ROOT_POSIX" ]; then
  for var in "${RUSTUP_HOME:-}" "${CARGO_HOME:-}"; do
    [ -z "$var" ] && continue
    cand="$(dirname "$(to_posix "$var")")"
    if probe_rust_root "$cand"; then RUST_ROOT_POSIX="$cand"; break; fi
  done
fi

# 1d) 最后兜底：本机私有位置（他人机器上不命中，仅为本机省一次自省）
if [ -z "$RUST_ROOT_POSIX" ]; then
  for cand in "D:/Kiln/rust" "D:/software/rust"; do
    if probe_rust_root "$(to_posix "$cand")"; then RUST_ROOT_POSIX="$(to_posix "$cand")"; break; fi
  done
fi

if [ -z "$RUST_ROOT_POSIX" ]; then
  die "未找到可编 $TARGET_NAME 的 Rust 工具链。
    已尝试: AIROOT_RUST_ROOT、rustup 自省、\$CARGO_HOME/\$RUSTUP_HOME 推导、本机兜底路径
    请安装后重试：
      rustup toolchain install stable --target $TARGET_NAME"
fi

# 导出为大写盘符 Windows 形式（cargo 定位 registry 用；rustc 路径处理区分盘符大小写）
to_win_upper() { printf '%s' "$1" | sed -E 's#^/([a-zA-Z])/#\U\1:/#' | tr '/' '\\'; }
export CARGO_HOME="$(to_win_upper "$RUST_ROOT_POSIX/cargo")"
export RUSTUP_HOME="$(to_win_upper "$RUST_ROOT_POSIX/rustup")"

TOOLCHAIN_BIN="$RUST_ROOT_POSIX/rustup/toolchains/$TOOLCHAIN_DIR_NAME/bin"
CARGO_EXE="$TOOLCHAIN_BIN/cargo.exe"
[ -x "$CARGO_EXE" ] || die "cargo.exe 不可执行: $CARGO_EXE"
RUST_ROOT="$(to_win_upper "$RUST_ROOT_POSIX")"

# ---------- 2) gnu 目标：定位完整 MinGW binutils ----------
# msvc 目标不需要（rustup 自带链完整）。
export MSYS_NO_PATHCONV=1
if [ "$TARGET_NAME" = "x86_64-pc-windows-gnu" ]; then
  BINUTILS_BIN="${MINGW_BINUTILS_BIN:-}"

  # 2a) 环境变量未指定时：先看 PATH 里是否已有可用 as.exe
  if [ -z "$BINUTILS_BIN" ]; then
    if command -v as.exe >/dev/null 2>&1; then
      BINUTILS_BIN="$(dirname "$(command -v as.exe)")"
    fi
  fi

  # 2b) 本机兜底候选（私有路径；他人机器不命中）
  if [ -z "$BINUTILS_BIN" ]; then
    for cand in "D:/Kiln/mingw64/bin" "D:/Kiln/mingw-binutils/bin"; do
      if [ -x "$(to_posix "$cand")/as.exe" ]; then BINUTILS_BIN="$cand"; break; fi
    done
  fi

  if [ -z "$BINUTILS_BIN" ] || [ ! -x "$(to_posix "$BINUTILS_BIN")/as.exe" ]; then
    die "gnu 目标需要完整 MinGW binutils（含 as.exe），未找到。
    rustup 自带的 self-contained 目录缺 as.exe，无法独立完成链接。

    三选一：
      A) 改用 msvc（推荐，无需额外配置）：
           安装 VS Build Tools 后执行：
             rustup toolchain install stable-x86_64-pc-windows-msvc
             powershell -File build.ps1
      B) 用 MSYS2 安装 binutils 并指定路径：
             pacman -S mingw-w64-x86_64-binutils
             export MINGW_BINUTILS_BIN=/mingw64/bin   # 指向含 as.exe 的目录
      C) 已有 binutils 时直接指定其 bin 目录：
             export MINGW_BINUTILS_BIN=<你的 binutils>/bin"
  fi
  export PATH="$(to_posix "$BINUTILS_BIN"):$TOOLCHAIN_BIN:$PATH"
  info "binutils    : $BINUTILS_BIN"
fi

# ---------- 3) 构建 ----------
# toolchain bin 注入 PATH，供 rustc 的依赖调用
if [ "$TARGET_NAME" != "x86_64-pc-windows-gnu" ]; then
  export PATH="$TOOLCHAIN_BIN:$PATH"
fi

# 产物目录：CARGO_TARGET_DIR 优先（cargo 约定，CI 会设置），否则仓库内 target/
TARGET_DIR="${CARGO_TARGET_DIR:-$REPO_ROOT/target}"
EXE="$(to_posix "$TARGET_DIR")/$TARGET_NAME/release/AI-Root.exe"

info "rust 根目录 : $RUST_ROOT"
info "cargo       : $CARGO_EXE"
info "目标平台    : $TARGET_NAME"
info "产物目录    : $TARGET_DIR"

"$CARGO_EXE" build --release --target "$TARGET_NAME" "$@"

[ -f "$EXE" ] || die "构建未产出 exe: $EXE"
echo "[OK] 产物: $EXE"
echo "     SHA256: $(sha256sum "$EXE" | cut -d' ' -f1)"
