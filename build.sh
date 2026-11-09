#!/usr/bin/env bash
# ============================================================================
# 编译脚本：编译 release 版二进制 (target/release/trae-usage-monitor)
# 用法：
#   ./build.sh              编译 release
#   ./build.sh --locked     额外透传给 cargo 的参数
# ============================================================================
set -euo pipefail

# 切到脚本所在目录，保证从任意位置调用都能编译
cd "$(dirname "$0")"

if ! command -v cargo >/dev/null 2>&1; then
  echo "错误：未找到 cargo，请先安装 Rust 工具链 → https://rustup.rs" >&2
  exit 1
fi

echo "==> 正在编译 (release)..."
cargo build --release "$@"

BIN="target/release/trae-usage-monitor"
echo "==> 编译完成：$(pwd)/${BIN}"
