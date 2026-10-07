#!/usr/bin/env bash
# ===========================================================================
#  rdep 发布构建脚本
#
#  用法：
#    ./deploy/build-release.sh              # 构建本机 release（service + forwarder + client）
#    ./deploy/build-release.sh --server     # 仅构建 service / forwarder（无 GUI，最快）
#    ./deploy/build-release.sh --client     # 交叉编译 client 到 dist/（win/mac/linux）
#
#  产物统一输出到 dist/。
# ===========================================================================
set -euo pipefail

cd "$(dirname "$0")/.."
ROOT="$PWD"
DIST="$ROOT/dist"
MODE="${1:-all}"

# cargo 需要一个稳定的 TMPDIR（本机 /tmp 为小 tmpfs，大项目链接易失败）
export TMPDIR="${TMPDIR_OVERRIDE:-$ROOT/target/tmp}"
mkdir -p "$TMPDIR"

log() { echo -e "\033[1;34m[build]\033[0m $*"; }

build_server() {
    log "构建 rdep-service / rdep-forwarder (release) ..."
    cargo build --release -p rdep-service -p rdep-forwarder
    mkdir -p "$DIST"
    cp -f target/release/rdep-service   "$DIST/"
    cp -f target/release/rdep-forwarder "$DIST/"
    log "server 产物 -> $DIST/{rdep-service,rdep-forwarder}"
}

build_client_host() {
    log "构建 rdep-client (本机 release, GUI) ..."
    cargo build --release -p rdep-client
    mkdir -p "$DIST"
    cp -f target/release/rdep-client "$DIST/"
}

build_client_cross() {
    log "交叉编译 rdep-client 到 dist/ ..."
    mkdir -p "$DIST"
    # 目标三元组 -> 产物名
    build_one() {
        local target="$1" out="$2"
        if rustup target list --installed | grep -qx "$target"; then
            log "  -> $target"
            cargo build --release -p rdep-client --target "$target" || {
                echo "    [warn] $target 构建失败（可能缺少该 target 或系统依赖），跳过"; return 0; }
            mkdir -p "$DIST/$out"
            cp -f "target/$target/release/rdep-client"* "$DIST/$out/" 2>/dev/null || true
        else
            echo "    [skip] $target 未安装（rustup target add $target）"
        fi
    }
    build_one x86_64-pc-windows-msvc  rdep-client-windows-x64
    build_one x86_64-apple-darwin     rdep-client-macos-x64
    build_one aarch64-apple-darwin    rdep-client-macos-arm64
    build_one x86_64-unknown-linux-gnu rdep-client-linux-x64
    log "client 产物 -> $DIST/rdep-client-*"
}

case "$MODE" in
    --server)  build_server ;;
    --client)  build_client_cross ;;
    all)       build_server; build_client_host ;;
    *) echo "未知参数: $MODE（可用: --server / --client / 默认 all）"; exit 1 ;;
esac

echo
log "完成。产物目录：$DIST"
ls -lh "$DIST" 2>/dev/null || true
