# know-mcp

> 本地**代码搜索 + 文档搜索** MCP server —— 对归档的代码仓库和文档做关键字 + 向量语义混合检索，带行号读取，本地推理零外发。

一次构建，三种消费形态：**MCP server**（Claude Code / 任何 MCP 客户端）、**原生 CLI**（`know-mcp register` 一键注册）、**HTTP 服务**（多客户端共享）。

## 功能

- **混合检索**：zvec 引擎内融合——HNSW 向量（Qwen3-Embedding-0.6B，1024 维，INT8 量化索引 + refiner 精排）+ jieba 中文分词 FTS + RRF 重排；`auto` 模式按查询形态自动路由（标识符/中文 → 关键字，自然语言 → 混合）
- **代码感知分块**：token 预算自适应 CJK + tree-sitter AST 边界；代码 chunk 带 `rel_path [lang]` 上下文，markdown 带 heading breadcrumb 上下文
- **daemon 常驻**：探测-拉起-等就绪（冷启动自动拉起，首调用等待就绪）；索引常驻内存，多客户端共享；create_new 锁文件单实例互斥（并发拉起恰好一个赢）
- **增量索引**：mtime+size 快路径 → 内容哈希 → 确定性 chunk id 替换；二次运行秒级
- **三段流水线索引**：N 生产者线程（读+分块）→ GPU 嵌入（llama.cpp，CUDA）→ 有序入库；14k 文件全量 ~107 分钟（RTX 4060），增量重扫 11 秒
- **GPU 自适应**：检测到 CUDA 设备自动全层卸载，无 GPU 纯 CPU 亦可用，零配置
- **文本安全**：扩展名白名单 + NUL 嗅探 + UTF-8 校验三层过滤，二进制内容不入库

## 架构

```
┌─ MCP 客户端 (Claude Code / Desktop / 任意 MCP host)
│    stdio 桥 (know-mcp stdio)        HTTP (know-mcp http → /mcp)
│    原生 CLI (know-mcp register → know-cli <verb>)
▼
┌─ daemon (know-mcp daemon, 后台常驻, 单实例)
│  ├─ JSON 管理面 (local socket: named pipe / UDS)
│  ├─ zvec collection (HNSW INT8 向量 + jieba FTS + 引擎级 RRF 混合)
│  ├─ Qwen3-Embedding-0.6B ×2 (索引线程 + 查询线程, llama.cpp, GPU 自适应)
│  ├─ 增量索引 (三段流水线: producer×N → GPU embed → store)
│  └─ 索引锁 (daemon.lock 原子互斥 + PID 存活检测接管)
└─ CLI: status / stop / index / search (都走 daemon)
```

## MCP 工具（4 个）

| 工具 | 说明 |
|---|---|
| `search_content` | 混合检索；mode: auto/keyword/vector/hybrid；path_filter 范围过滤 |
| `read_file` | 带行号读取（roots 白名单校验，2000 行/256KB 上限） |
| `get_status` | 索引/进度状态 |
| `build_index` | 触发增量重索引（force=true 全量重嵌入） |

## 快速开始

```bash
cargo build --release
cp know-mcp.toml.example know-mcp.toml   # 编辑：roots / 模型路径 / index_dir
./target/release/know-mcp index        # 首次建索引（前台跑，看进度）
./target/release/know-mcp search "hybrid search"   # 冒烟
```

### 接入 Claude Code

```bash
claude mcp add know-search -- <repo>/target/release/know-mcp stdio
```

或 `settings.json`：

```json
{ "mcpServers": { "know-search": {
    "command": "<repo>/target/release/know-mcp",
    "args": ["stdio"] } } }
```

### 注册为原生 CLI（mcp2cli 绑定）

```bash
# 前置：安装 mcp2cli（一次性）
cargo install --git https://github.com/mcp2cli/source-code

know-mcp register        # stdio 桥（自动拉起 daemon）；--transport http 走 HTTP

# 之后即得 know-cli：
know-cli get-status
know-cli search-content --query "混合检索" --mode hybrid
```

### HTTP 模式

```bash
know-mcp http    # http://127.0.0.1:8765/mcp（挂 daemon，多客户端共享）
```

### CLI 速查

```
know-mcp daemon    # 显式启动守护（通常无需——其他命令自动拉起）
know-mcp status    # 状态
know-mcp index     # 建库（--force 全量重嵌入）
know-mcp search    # 检索（--mode auto|keyword|vector|hybrid --top-k --path）
know-mcp stop      # 停守护
```

## 配置（know-mcp.toml）

查找顺序：`--config` > `KNOW_MCP_CONFIG` 环境变量 > exe 同目录 > 工作目录。
从 `know-mcp.toml.example` 复制后编辑：

| 段 | 内容 |
|---|---|
| `[index]` | roots（索引根目录）、exclude、扩展名白名单、单文件大小上限 |
| `[chunk]` | 分块尺寸/重叠（默认 1000/120 字符） |
| `[embed]` | 模型名、GGUF 路径（大小校验 639,150,592）、gpu_layers（null=自动） |
| `[search]` | default_top_k |
| `[server]` | http_bind |
| `[storage]` | index_dir（zvec collection + sync_state） |

## 运维

- daemon 日志：`%APPDATA%\know-mcp\daemon.log`（Windows）或 index_dir 下 `daemon.log`
- 单实例：`daemon.lock` 原子 create_new 互斥；陈旧锁按 PID 存活检测自动接管
- 冷启动：首次工具调用自动拉起 daemon 并等待就绪（模型加载可到数分钟）；之后的调用即时
- 依赖库路径：`.env` 里 `KNOW_LIB_DIR` 指向 DLL 目录（Windows 经 SetDllDirectoryW 注入加载器）

## 构建依赖

- Rust 1.98+ / MSVC 工具链
- **zvec C 库**：`zvec-rust` bundled 特性构建时自动下载预编译 DLL；离线时手动下载 `zvec-prebuilt-x86_64-pc-windows-msvc.tar.gz` 解压设 `ZVEC_LIB_DIR`
- **llama.cpp**（llama-cpp-2 crate，git rev `0364f5da`，源码编译）：需 cmake 与 LLVM（bindgen 用 libclang）；构建环境见 `scripts/env.sh`（`CMAKE_GENERATOR=Ninja` + `LIBCLANG_PATH` + CUDA 13.5 显式指定）
- **嵌入模型**：Qwen3-Embedding-0.6B Q8_0 GGUF，从 [Qwen/Qwen3-Embedding-0.6B-GGUF](https://huggingface.co/Qwen/Qwen3-Embedding-0.6B-GGUF) 下载
- **GPU（可选）**：CUDA Toolkit 13.5 + llama-cpp-2 `cuda-no-vmm` feature；首次索引小时级 → 分钟级

## 开发

```bash
cargo test -p know-core        # 单测 + zvec 集成测试（嵌入冒烟需本地模型，缺失自动跳过）
cargo test -p know-server      # daemon e2e + 互斥回归 + 冷启动 e2e
bash scripts/build.sh lint     # clippy -D warnings --no-deps + 项目禁令
bash scripts/build.sh release  # 发布构建（thin-LTO）
```

项目专属 Lint：`clippy.toml` 的 `disallowed-methods` 禁用开发中踩过坑的 API（`embeddings_ith` / `delete_by_filter`），每条注明原因，随 `cargo clippy` 一起跑。

## 技术选型

| 组件 | 选择 | 理由 |
|---|---|---|
| 向量引擎 | zvec（Rust 绑定） | 原生混合检索（HNSW INT8+FTS+RRF），内嵌 jieba 中文零配置 |
| 嵌入 | Qwen3-Embedding-0.6B GGUF + llama.cpp | 中英双语强、CPU 可用 GPU 提速、本地推理无外发 |
| MCP | rmcp（官方 Rust SDK） | stdio + streamable HTTP 双传输 |
| daemon | herdr 模式 | 探测-拉起-等就绪、锁文件单实例、零 lock 文件残留 |

## License

Apache-2.0
