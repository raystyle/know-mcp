# rcd-mcp-server

本地 daemon 形态的混合搜索 + 知识沉淀 MCP server。对归档的代码仓库、文档、逆向源码做**关键字 + 向量语义 + 混合搜索**，并提供带行号的文件读取与知识笔记闭环。

## 架构

```
┌─ MCP 客户端 (Claude Code / Desktop / 其他)
│    stdio 桥 (rcd-mcp-server mcp)          HTTP (rcd-mcp-server http → /mcp)
▼
┌─ daemon (rcd-mcp-server daemon, 后台常驻, 单实例)
│  ├─ JSON 管理面 (local socket: named pipe / UDS)
│  ├─ zvec collection (HNSW 向量 + jieba FTS + 引擎级 RRF 混合)
│  ├─ Qwen3-Embedding-0.6B ×2 (索引工作线程 + 查询线程, llama.cpp)
│  ├─ 增量索引 (mtime+size 快路径 → 内容哈希 → 确定性 chunk id 替换)
│  └─ 知识笔记 (knowledge/*.md) + 反馈记账 (feedback.json) + 缺口检测
└─ CLI: status / stop / index / search / note (都走 daemon)
```

## 两类资源 + 工作循环

- **archive**（只读，事实来源）：归档的代码仓、文档、逆向源码（配置里的 roots）
- **note**（可写，解法沉淀）：`knowledge/KB-*.md`，带 helpful 反馈分

循环：`search`（archive=事实，note=已验证解法）→ 解决问题 → `save_note`（附 sources/query）→ 笔记进入索引、二次搜索命中 → `record_feedback` 打分（高分升权/低分降权）→ `get_gaps` 看哪些问题搜不到 → 补笔记。

## MCP 工具（9 个）

| 工具 | 说明 |
|---|---|
| `search_content` | 混合检索；mode: auto/keyword/vector/hybrid；kind_filter: archive/note；path_filter |
| `read_file` | 带行号读取（roots 白名单校验，2000 行/256KB 上限） |
| `get_status` | 索引/笔记/进度状态 |
| `build_index` | 触发增量重索引（force=true 全量重嵌入） |
| `save_note` | 沉淀知识笔记（title/body/tags/sources/query） |
| `list_notes` | 笔记列表（含 helpful 分） |
| `get_note` | 读笔记全文 + 元数据 |
| `record_feedback` | 记录笔记是否有用（影响排序） |
| `get_gaps` | 零结果/低分查询列表（该补什么笔记） |

## 快速开始

```bash
cargo build --release
cp rcd-mcp.toml.example rcd-mcp.toml   # 编辑：roots / 模型路径 / index_dir
./target/release/rcd-mcp-server index   # 首次建索引（前台跑，看进度）
./target/release/rcd-mcp-server search "hybrid search"  # 冒烟
```

### 接入 Claude Code

```bash
claude mcp add rcd-search -- <repo>/target/release/rcd-mcp-server.exe mcp
```

或 `settings.json`：

```json
{ "mcpServers": { "rcd-search": {
    "command": "<repo>/target/release/rcd-mcp-server",
    "args": ["mcp"] } } }
```

### mcp2cli 模式（MCP 工具变原生 CLI 动词）

内置 [mcp2cli](https://github.com/mcp2cli/source-code) 接入：一条命令把本 server 的 MCP 工具注册成原生 CLI（`search` → `search-content --query ...`，类型化 `--flags`、JSON 输出、可管道组合）。

```bash
# 前置：安装 mcp2cli（一次性）
cargo install --git https://github.com/mcp2cli/source-code

# 注册（stdio 桥，自动拉起 daemon；或 --transport http 走 HTTP 面板）
rcd-mcp-server register-cli

# 之后即得原生动词：
rcd get-status
rcd search-content --query "混合检索" --mode hybrid
rcd save-note --title "..." --body "..."
rcd record-feedback --note-id KB-... --helpful true
```

### HTTP 模式

```bash
rcd-mcp-server http    # http://127.0.0.1:8765/mcp（挂 daemon）
```

### CLI 调试

```
rcd-mcp-server status         # 自动拉起 daemon 并回报
rcd-mcp-server search "混合检索" --mode auto
rcd-mcp-server index --force  # 全量重嵌入
rcd-mcp-server note list|show <id>
rcd-mcp-server stop
```

## 配置（rcd-mcp.toml）

查找顺序：`--config` > `RCD_MCP_CONFIG` 环境变量 > exe 同目录 > 工作目录。
从 `rcd-mcp.toml.example` 复制后编辑：索引 roots、排除 glob、chunk 尺寸、嵌入模型路径、top_k 默认值、HTTP bind、索引数据目录、笔记目录。

## 运维

- daemon 日志：`%APPDATA%\rcd-mcp\daemon.log`（Windows）或 index_dir 下 `daemon.log`
- socket：named pipe / UDS `rcd-mcp.sock`（`RCD_SOCKET_PATH` 可覆盖）
- 索引数据：配置的 `index_dir`（collection + meta.json + sync_state.json + feedback.json）
- 单实例：socket bind 即互斥；陈旧 socket 自动检测重建；`rcd-mcp-server stop` 优雅停机
- 知识笔记：配置的 `notes.dir`（人可读 markdown，建议进 git）

## 构建依赖

- Rust 1.98+ / MSVC 工具链
- **zvec C 库**：`zvec-rust` bundled 特性在构建时自动从 GitHub Release 下载预编译 DLL；离线时手动下载 `zvec-prebuilt-x86_64-pc-windows-msvc.tar.gz` 解压并设 `ZVEC_LIB_DIR`
- **llama.cpp**（llama-cpp-2 crate，源码编译）：需要 cmake 与 LLVM（bindgen 用 libclang）。构建环境：
  - `CMAKE_GENERATOR=Ninja`（或系统 cmake 在 PATH 且认识你的 VS 生成器）
  - `LIBCLANG_PATH` 指向 LLVM bin 目录
- **嵌入模型**：Qwen3-Embedding-0.6B Q8_0 GGUF（639,150,592 字节校验），从 [Qwen/Qwen3-Embedding-0.6B-GGUF](https://huggingface.co/Qwen/Qwen3-Embedding-0.6B-GGUF) 下载，`gguf_path` 指向本地文件
- **GPU（可选）**：CUDA Toolkit + llama-cpp-2 `cuda` feature；模型层卸载到 GPU 后首次索引从小时级缩到分钟级

## 开发

```bash
cargo test -p rcd-core   # 单测 + zvec 集成测试（嵌入冒烟需本地模型文件，缺失自动跳过）
cargo build              # 全量构建
bash scripts/build.sh lint   # clippy + 项目禁令（clippy.toml）
```

测试覆盖：分块器（中英文/代码栅栏/超长行）、zvec 存储三模式检索、笔记读写往返、嵌入相似度结构与吞吐下限。

项目专属 Lint：`clippy.toml` 的 `disallowed-methods` 禁用踩过坑的 API（embeddings_ith / delete_by_filter / optimize），随 `cargo clippy` 一起跑。
