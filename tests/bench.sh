#!/usr/bin/env bash
# Performance bench: index throughput, incremental overhead, query latency.
# Requires: built binary + real GGUF model (know-mcp.toml) + zvec DLLs.
# Usage: bash tests/bench.sh [--release] [N_FILES]
set -euo pipefail
cd "$(dirname "$0")/.."
source scripts/env.sh

case "${1:---debug}" in
  --release) EXE=target/release/know-mcp ;;
  *) EXE=target/debug/know-mcp ;;
esac
N_FILES="${2:-100}"
[[ -f "$EXE" ]] || { echo "bench: $EXE not built" >&2; exit 1; }

BENCH="$(cygpath -m "${TEMP:-/c/Windows/Temp}")/know-bench-$$"
mkdir -p "$BENCH/corpus" "$BENCH/knowledge"
trap 'rm -rf "$BENCH"; "$EXE" stop >/dev/null 2>&1 || true' EXIT
export KNOW_SOCKET_PATH="know-mcp-bench-$RANDOM"

MODEL=$(python -c "import tomllib;print(tomllib.load(open('know-mcp.toml','rb'))['embed']['gguf_path'])")

# --- Synthetic, reproducible corpus: N markdown+rust files with mixed
# --- Chinese/English content and varied chunk counts.
python - "$N_FILES" "$BENCH/corpus" <<'PY'
import sys, random
n, root = int(sys.argv[1]), sys.argv[2]
random.seed(42)
words_en = "vector hybrid search embedding index shard segment merge query rank fuse".split()
words_zh = "向量 混合 检索 索引 分段 融合 排序 查询 语义 关键字".split()
for i in range(n):
    lines = [f"# bench file {i} 混合检索基准"]
    for _ in range(random.randint(20, 120)):
        en = " ".join(random.choices(words_en, k=12))
        zh = "".join(random.choices(words_zh, k=8))
        lines.append(f"line {i}: {en} {zh}")
    ext = "md" if i % 2 else "rs"
    open(f"{root}/bench_{i:04d}.{ext}", "w", encoding="utf8").write("\n".join(lines))
print(f"generated {n} files")
PY

cat > "$BENCH/know-mcp.toml" <<EOF
[index]
roots = ["$BENCH/corpus"]
[notes]
dir = "$BENCH/knowledge"
[embed]
model = "qwen3-embedding-0.6b"
gguf_path = "$MODEL"
tokenizer_repo = "Qwen/Qwen3-Embedding-0.6B"
[storage]
index_dir = "$BENCH/index"
EOF
CFG=(--config "$BENCH/know-mcp.toml")

ms() { # ms <n> — print elapsed ms of the last command via TIMEFORMAT
  :
}
now_ms() { date +%s%3N; }

# --- 1. Full index throughput ------------------------------------------
T0=$(now_ms)
"$EXE" "${CFG[@]}" index >/dev/null 2>&1
T1=$(now_ms)
FULL_MS=$((T1 - T0))
RATE=$(awk "BEGIN{printf \"%.1f\", ${N_FILES} * 1000 / (${FULL_MS} + 1)}")
echo "index.full:        ${N_FILES} files in ${FULL_MS}ms  (${RATE} files/s)"

# --- 2. Incremental re-pass (all unchanged) ----------------------------
T0=$(now_ms)
"$EXE" "${CFG[@]}" index >/dev/null 2>&1
T1=$(now_ms)
echo "index.incremental: $((T1 - T0))ms"

# --- 3. Query latency (10 samples each) --------------------------------
"$EXE" "${CFG[@]}" status >/dev/null 2>&1   # daemon up (same KNOW_SOCKET_PATH)
bench_query() { # bench_query <label> <mode> <query>
  local label=$1 mode=$2 query=$3
  local times=() t0 t1
  for _ in $(seq 10); do
    t0=$(now_ms)
    "$EXE" "${CFG[@]}" search "$query" --mode "$mode" >/dev/null 2>&1
    t1=$(now_ms)
    times+=($((t1 - t0)))
  done
  local sorted sum p95 max=0
  sorted=$(printf '%s\n' "${times[@]}" | sort -n)
  sum=$(printf '%s\n' "${times[@]}" | awk '{s+=$1} END{print s}')
  p95=$(printf '%s\n' "$sorted" | awk 'NR==9{print; exit}')
  avg=$((sum / 10))
  echo "query.$label:      avg ${avg}ms  p95 ${p95}ms"
}
bench_query keyword keyword "混合检索"
bench_query vector  vector  "hybrid semantic search"
bench_query hybrid  hybrid  "向量 semantic 融合"

# --- 4. Admin round-trip ------------------------------------------------
T0=$(now_ms)
for _ in $(seq 20); do "$EXE" "${CFG[@]}" status >/dev/null 2>&1; done
T1=$(now_ms)
echo "admin.rtt:         avg $(( (T1 - T0) / 20 ))ms"

echo "bench: done"
