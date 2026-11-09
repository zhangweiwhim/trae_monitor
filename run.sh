#!/usr/bin/env bash
# ============================================================================
# 运行脚本：拉取 Trae 用量并推送飞书图表卡片
# 用法：
#   ./run.sh                       使用 ./config.yaml 正式拉取并推送
#   ./run.sh --dry-run             只打印卡片 JSON，不推送
#   ./run.sh --sample              离线样例预览（不读配置、不联网）
#   ./run.sh --config other.yaml   指定其它配置文件
#   其余参数与上述可组合，均原样透传给程序
#
# 环境变量：
#   RUST_LOG=debug ./run.sh        调整日志级别（默认 info）
# ============================================================================
set -euo pipefail

cd "$(dirname "$0")"

BIN="target/release/trae-usage-monitor"

# 二进制不存在则先自动编译
if [[ ! -x "$BIN" ]]; then
  echo "==> 未找到二进制，先执行编译..."
  ./build.sh
fi

export RUST_LOG="${RUST_LOG:-info}"

# 若用户未显式指定 --config/-c，则默认使用 ./config.yaml
has_config=false
for arg in "$@"; do
  [[ "$arg" == "--config" || "$arg" == "-c" ]] && has_config=true
done

if $has_config; then
  exec "$BIN" "$@"
else
  exec "$BIN" --config config.yaml "$@"
fi
