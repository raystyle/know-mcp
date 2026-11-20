# know-mcp

本地**代码搜索 + 文档搜索** MCP server —— 对归档的代码仓库和文档做关键字 + 向量语义混合检索。Rust workspace，两个 crate：`know-core`（引擎）+ `know-server`（daemon/MCP/CLI 入口）。二进制/命令名 `know-mcp`。

## 架构速览

```
walk → read → chunk → embed → store(zvec) → search(hybrid)
```

- **分块**：`chunker.rs`（token 预算自适应 CJK）+ `ast_chunker.rs`（代码 AST 边界）；markdown 加 heading breadcrumb 上下文，代码加 `rel_path [lang]` 上下文（`sync.rs` 里 contextualize）
- **嵌入**：`embed.rs` — Qwen3-Embedding-0.6B Q8_0 GGUF，llama-cpp-2（llama.cpp 原生内核），pooling=LAST + L2 归一。Query 侧用 instruct 模板，document 侧原文
- **存储**：`zvec_store.rs` — zvec 引擎内融合：HNSW INT8 量化向量 + jieba FTS + RRF 重排
- **增量索引**：`sync.rs` — 三段流水线（producer×N → GPU 嵌入 → 有序入库）；mtime+size 快路径 → 内容哈希 → 确定性 chunk id（`{locator}-{i}`）delete-then-upsert
- **daemon**：herdr 模式（探测-拉起-等就绪）；`daemon.lock` 原子 create_new 单实例互斥 + PID 存活检测接管；stdio 桥首调用自动拉起 daemon

## 常用命令

```bash
cargo test -p know-core                    # 单测（嵌入冒烟需本地 GGUF，缺失自动跳过）
cargo test -p know-server                  # daemon e2e + 互斥回归 + 冷启动 e2e
bash scripts/build.sh lint                 # clippy -D warnings --no-deps + 项目禁令
bash scripts/build.sh release              # 发布构建（thin-LTO）
```

- **任何 cargo 命令前必须 `source scripts/env.sh`**（CUDA 13.5 显式指定——系统 CUDA_PATH 指向 12.9 会炸 MSVC 14.44；Windows SDK LIB/INCLUDE/rc/mt 路径；详见文件内注释）
- 模型文件：`D:/search-harness/workspace/models/hf_Qwen_Qwen3-Embedding-0.6B-Q8_0.gguf`（639,150,592 字节，`validate_gguf` 校验大小）
- 配置查找顺序：`--config` > `KNOW_MCP_CONFIG` > exe 同目录 > 工作目录
- Windows 优先 `dunce::canonicalize`（非 UNC 路径）

## 项目禁令（clippy.toml disallowed-methods）

随 `cargo clippy` 强制执行，每条注明原因：

- `embeddings_ith` — 多 token 序列读嵌入有坑（用 `embeddings_seq_ith`）
- `delete_by_filter` — zvec 侧有已知问题

## 性能基线（勿回退）

- **全量索引（14,729 文件）107 分钟**，GPU ~70%，0 失败，正常退出（2026-11-20 验收，RTX 4060）
- **增量重扫 11 秒**（daemon 冷启动后，mtime 快路径全命中）
- 嵌入吞吐 92 chunks/s（`embed_documents_throughput_guard` 守护，地板 20；修复前 25.5）
- 索引流水线：三段并发（producer×N → GPU 嵌入 → 有序入库），`pipeline_shutdown_test` 守护退出死锁
- 曾经的坑：**逐文件嵌入 GPU 空转（1.1 files/s）**；**通道关闭死锁**（producer 借用共享 sender，shutdown 循环等待，工作全完成但进程永不退出）——改通道结构时必须保持"每 producer 持有 clone、main 在 join 前 drop 原始 sender"
- zvec 反模式：**flush() 会封存向量索引 block**（alibaba/zvec#869），小批量频繁 flush = 碎片化（曾出现 4892 个 segment）；攒大批量、每 16k chunks 才 flush，现在全程 ~10 个 segment

## 嵌入路径的已知取舍（2026-11-19/20 实验矩阵，RTX 4060 / Qwen3-0.6B Q8_0）

- **历史元凶是 llama.cpp 上游 bug**（[ggml-org/llama.cpp#30323](https://github.com/ggml-org/llama.cpp/issues/30323)）：pooled embedding 每次解码把全词表 logits（n_vocab=151,669，~600KB/token）从 GPU 拷回 host。修复在 [PR #30445](https://github.com/ggml-org/llama.cpp/pull/30445)（llama.cpp v0.8.0+）。**llama-cpp-2 crates.io 0.1.161 早于修复**，本项目用 git rev `0364f5da`；升级依赖时勿退回
- **multi-sequence 批处理已实测否决**：正确配置下 width-2 仍比单序列慢 1.52x（稠密 attention 对全 padding KV 宽度付 mask 计算费）；flash attention 强制 ON 也慢（89 vs 92）——都别在没有新证据时重试
- **zvec 引擎能力**（2026-11-20 起启用）：HNSW **INT8 量化索引**（FP32 存储，refiner 精排，zvec 基准同款，schema_version=2）；`initialize(num_threads=核数/2)`；索引 ≥10k chunks 后自动 `optimize()`（flat buffer 并入 HNSW，#724 的丢数据 bug 已修）
- 质量守护：`embed_documents_deterministic_and_consistent_with_embed`（cosine 0.9999989）

## 已修复的历史问题（测试守护中）

1. **daemon 单例竞态**（2026-11-20）：probe-then-bind 在双模型加载的 ~90s 窗口内竞态，双 daemon 共存互相覆盖 port file。现 `daemon.lock` create_new 原子互斥（锁在模型加载前，输家秒退）；陈旧锁按 PID 接管——Windows 必须 `WaitForSingleObject(0)` 而非 `OpenProcess`（僵尸进程句柄可开）。守护：`daemon_mutex_test`（顺序互斥 + 陈旧锁接管 + 4 路并发恰好 1 赢）
2. **冷启动**（2026-11-20）：READY_TIMEOUT 300s；stdio 桥 `forward` 走 `ensure_daemon`（daemon 不在时首调用自动拉起）。守护：`cold_start_stdio_bridge_spawns_daemon_and_searches`

## 剩余约束（配置决策，非缺陷）

- 全量索引 107 分钟是嵌入吞吐物理下限（~60 万 chunks @ 92/s ≈ 110 分钟 GPU 时间）；再快只能减语料（`exclude` 加 glob）或换硬件
