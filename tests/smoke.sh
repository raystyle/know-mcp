#!/usr/bin/env bash
# Smoke test: full-stack verification against a tiny fixture corpus.
# Requires: built binary + a real GGUF model (config gguf_path) + zvec DLLs.
# Usage: bash tests/smoke.sh [--release]
set -euo pipefail
cd "$(dirname "$0")/.."
source scripts/env.sh

BIN="${1:---debug}"
case "$BIN" in
  --release) EXE=target/release/know-mcp ;;
  *) EXE=target/debug/know-mcp ;;
esac
[[ -f "$EXE" ]] || { echo "FAIL: $EXE not built" >&2; exit 1; }

# Isolated environment: own config, own socket, own index dir.
SMOKE="$(cygpath -m "${TEMP:-/c/Windows/Temp}")/know-smoke-$$"
mkdir -p "$SMOKE"
cleanup() {
    "$EXE" --config "$SMOKE/know-mcp.toml" stop >/dev/null 2>&1 || true
    sleep 2
    rm -rf "$SMOKE" 2>/dev/null || true
}
trap cleanup EXIT
export KNOW_PORT_DIR="$SMOKE/portdir"
export RUST_LOG="know_mcp=debug,know_core=debug"
LOG="$APPDATA/know-mcp/daemon.log"
mkdir -p "$KNOW_PORT_DIR"
FIXTURE="$SMOKE/corpus"
mkdir -p "$FIXTURE/code" "$FIXTURE/docs" "$FIXTURE/knowledge"

cat > "$FIXTURE/code/zvec_probe.rs" <<'EOF'
pub fn zvec_hybrid_probe() -> &'static str {
    "ZVecInitialize multi_query set_rerank_rrf hybrid search"
}
EOF
cat > "$FIXTURE/docs/hybrid.md" <<'EOF'
# 混合检索方案
混合检索融合 BM25 关键字与向量语义，RRF 排序融合。
EOF

MODEL=$(python -c "import tomllib;print(tomllib.load(open('know-mcp.toml','rb'))['embed']['gguf_path'])" 2>/dev/null || true)
cat > "$SMOKE/know-mcp.toml" <<EOF
[index]
roots = ["$FIXTURE/code", "$FIXTURE/docs"]
[notes]
dir = "$FIXTURE/knowledge"
[embed]
model = "qwen3-embedding-0.6b"
gguf_path = "$MODEL"
tokenizer_repo = "Qwen/Qwen3-Embedding-0.6B"
[storage]
index_dir = "$SMOKE/index"
[server]
http_bind = "127.0.0.1:18765"
EOF

CONFIG_ARGS=(--config "$SMOKE/know-mcp.toml")

pass() { echo "PASS: $1"; }
fail() { echo "FAIL: $1" >&2; exit 1; }

# assert_log <pattern> <label> — debug-log evidence, not just exit codes.
assert_log() {
  grep -q "$1" "$LOG" || fail "log evidence missing: $2 (pattern: $1)"
}

# 1. Index the fixture (foreground, no daemon yet).
"$EXE" "${CONFIG_ARGS[@]}" index || fail "index run"
[[ -d "$SMOKE/index/collection" ]] || fail "collection dir created"

# 2. Daemon up + status answers (cold start loads the model: allow 3 min).
if ! timeout 180 "$EXE" "${CONFIG_ARGS[@]}" status >/dev/null; then
  sleep 5
  timeout 60 "$EXE" "${CONFIG_ARGS[@]}" status >/dev/null || fail "daemon auto-spawn"
fi

# 3. Identifier search hits the Rust file.
"$EXE" "${CONFIG_ARGS[@]}" search "ZVecInitialize" --mode keyword | grep -q "zvec_probe" \
  || fail "identifier keyword search"

# 4. Chinese search hits the doc (jieba).
"$EXE" "${CONFIG_ARGS[@]}" search "混合检索" --mode keyword | grep -q "hybrid" \
  || fail "chinese keyword search"

# 5. Hybrid mode works end-to-end (embeds the query).
"$EXE" "${CONFIG_ARGS[@]}" search "hybrid search 关键字" --mode hybrid | grep -qE "zvec_probe|hybrid" \
  || fail "hybrid search"

# 6. Incremental second pass is unchanged (fast path).
OUT=$(timeout 120 "$EXE" "${CONFIG_ARGS[@]}" index 2>/dev/null | grep -o "unchanged" | head -1)
[[ -n "$OUT" ]] || fail "incremental second pass reports unchanged"

# 7. Debug-log evidence chain (acceptance proof):
#    daemon lifecycle, single-instance lock, search outcomes, request ledger.
assert_log 'event="daemon.startup"' "daemon startup line"
assert_log 'event="socket.bind" outcome="ok"' "single-instance lock held"
assert_log 'event="daemon.ready"' "daemon ready"
assert_log 'event="search.complete".*outcome="ok"' "search completed with ok outcome"
assert_log 'event="admin.request.complete".*outcome="ok"' "request ledger has ok entries"

pass "smoke: all checks (functional + log evidence)"
