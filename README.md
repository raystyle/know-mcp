# rcd-mcp

> **rcd** = **R**eference **C**ode & **D**ocumentation — 归档参考代码与文档的本地混合检索 + 知识沉淀 MCP server

对归档的代码仓库、文档、逆向源码做**关键字 + 向量语义 + 混合搜索**，提供带行号的文件读取，并把解决问题的经验沉淀为可检索、可反馈打分的知识笔记——越用越准。

一次构建，三种消费形态：**MCP server**（Claude Code / 任何 MCP 客户端）、**原生 CLI**（`rcd-mcp register` 一键注册）、**HTTP 服务**（多客户端共享）。

## 特性

- **混合检索**：zvec 引擎内融合——HNSW 向量（Qwen3-Embedding-0.6B，1024 维）+ jieba 中文分词 FTS + RRF 重排；`auto` 模式按查询形态自动路由（标识符/中文 → 关键字，自然语言 → 混合）
- **两类资源**：`archive`（只读事实来源）与 `note`（可写解法沉淀）同库检索、显式区分；笔记按 helpful 反馈分动态加权
- **知识闭环**：搜索 → 解决 → 沉淀笔记 → 笔记参与二次搜索 → 反馈打分 → 缺口检测提示该补什么
- **daemon 常驻**：herdr 模式——探测-拉起-等就绪，socket bind 即单实例互斥；索引常驻内存，多客户端共享
- **增量索引**：mtime+size 快路径 → 内容哈希 → 确定性 chunk id 替换；二次运行为秒级
- **GPU 自适应**：检测到 CUDA 设备自动全层卸载（RTX 4060 上首次全量索引 ~8 分钟 vs CPU ~3 小时），无 GPU 纯 CPU 亦可用，零配置
- **文本安全**：扩展名白名单 + NUL 嗅探 + UTF-8 校验三层过滤，二进制内容不入库
- **自包含部署**：运行时 DLL 归档 `vendor/dlls/`，`.env` + `RCD_LIB_DIR` 指定依赖库路径，拷目录即用

## 架构

```
┌─ MCP 客户端 (Claude Code / Desktop / 任意 MCP host)
│    stdio 桥 (rcd-mcp stdio)        HTTP (rcd-mcp http → /mcp)
│    原生 CLI (rcd-mcp register → rcd-cli <verb>)
▼
┌─ daemon (rcd-mcp daemon, 后台常驻, 单实例)
│  ├─ JSON 管理面 (local socket: named pipe / UDS)
│  ├─ zvec collection (HNSW 向量 + jieba FTS + 引擎级 RRF 混合)
│  ├─ Qwen3-Embedding-0.6B ×2 (索引线程 + 查询线程, llama.cpp, GPU 自适应)
│  ├─ 增量索引 (mtime+size → 内容哈希 → 确定性 chunk id 替换)
│  └─ 知识笔记 (knowledge/KB-*.md) + 反馈记账 + 缺口检测
└─ CLI: status / stop / index / search / note (都走 daemon)
```

## 工作循环（为什么不只是搜索）

1. `search_content` —— 归档（事实来源）与旧笔记（已验证解法，看 helpful 分）同库命中
2. 用命中内容解决问题，`read_file` 取精确上下文
3. `save_note` 沉淀结论（附 sources 归档出处 + 触发 query）
4. 笔记即时入索引，下次同类问题直接命中
5. 用后 `record_feedback`：真解决了 → 升权；没解决 → 降权
6. `get_gaps` 看哪些问题反复搜不到 → 该补什么笔记一目了然

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
./target/release/rcd-mcp index        # 首次建索引（前台跑，看进度）
./target/release/rcd-mcp search "hybrid search"   # 冒烟
```

### 接入 Claude Code

```bash
claude mcp add rcd-search -- <repo>/target/release/rcd-mcp stdio
```

或 `settings.json`：

```json
{ "mcpServers": { "rcd-search": {
    "command": "<repo>/target/release/rcd-mcp",
    "args": ["stdio"] } } }
```

### 注册为原生 CLI（mcp2cli 绑定）

MCP 工具一键变原生动词（类型化 `--flags`、JSON 输出、可管道组合）：

```bash
# 前置：安装 mcp2cli（一次性）
cargo install --git https://github.com/mcp2cli/source-code

rcd-mcp register        # stdio 桥（自动拉起 daemon）；--transport http 走 HTTP

# 之后即得 rcd-cli：
rcd-cli get-status
rcd-cli search-content --query "混合检索" --mode hybrid
rcd-cli save-note --title "..." --body "..."
```

### HTTP 模式

```bash
rcd-mcp http    # http://127.0.0.1:8765/mcp（挂 daemon，多客户端共享）
```

### CLI 速查

```
rcd-mcp daemon    # 显式启动守护（通常无需——其他命令自动拉起）
rcd-mcp status    # 状态
rcd-mcp index     # 建库（--force 全量重嵌入）
rcd-mcp search    # 检索（--mode auto|keyword|vector|hybrid --top-k --path）
rcd-mcp note      # 笔记（list / show <id>）
rcd-mcp stop      # 停守护
```

## 配置（rcd-mcp.toml）

查找顺序：`--config` > `RCD_MCP_CONFIG` 环境变量 > exe 同目录 > 工作目录。
从 `rcd-mcp.toml.example` 复制后编辑：

| 段 | 内容 |
|---|---|
| `[index]` | roots（索引根目录）、exclude、扩展名白名单、单文件大小上限 |
| `[notes]` | 笔记目录（save_note 落盘处，也是可索引 root） |
| `[chunk]` | 分块尺寸/重叠（默认 1000/120 字符） |
| `[embed]` | 模型名、GGUF 路径（大小校验 639,150,592）、gpu_layers（null=自动） |
| `[search]` | default_top_k |
| `[server]` | http_bind |
| `[storage]` | index_dir（zvec collection + sync_state + feedback） |

## 运维

- daemon 日志：`%APPDATA%\rcd-mcp\daemon.log`（Windows）或 index_dir 下 `daemon.log`
- socket：named pipe / UDS `rcd-mcp.sock`（`RCD_SOCKET_PATH` 可覆盖）
- 单实例：socket bind 即互斥；陈旧 socket 自动检测重建；`rcd-mcp stop` 优雅停机
- 依赖库路径：`.env` 里 `RCD_LIB_DIR` 指向 DLL 目录（Windows 经 SetDllDirectoryW 注入加载器）
- 知识笔记：人可读 markdown，建议进 git

## 构建依赖

- Rust 1.98+ / MSVC 工具链
- **zvec C 库**：`zvec-rust` bundled 特性构建时自动下载预编译 DLL；离线时手动下载 `zvec-prebuilt-x86_64-pc-windows-msvc.tar.gz` 解压设 `ZVEC_LIB_DIR`
- **llama.cpp**（llama-cpp-2 crate，源码编译）：需 cmake 与 LLVM（bindgen 用 libclang）；构建环境见 `scripts/env.sh`（`CMAKE_GENERATOR=Ninja` + `LIBCLANG_PATH`）
- **嵌入模型**：Qwen3-Embedding-0.6B Q8_0 GGUF，从 [Qwen/Qwen3-Embedding-0.6B-GGUF](https://huggingface.co/Qwen/Qwen3-Embedding-0.6B-GGUF) 下载
- **GPU（可选）**：CUDA Toolkit + llama-cpp-2 `cuda-no-vmm` feature；首次索引小时级 → 分钟级

## 开发

```bash
cargo test -p rcd-core        # 单测 + zvec 集成测试（嵌入冒烟需本地模型，缺失自动跳过）
bash scripts/build.sh lint     # clippy -D warnings --no-deps + 项目禁令
bash scripts/build.sh release  # 发布构建（thin-LTO）
```

项目专属 Lint：`clippy.toml` 的 `disallowed-methods` 禁用开发中踩过坑的 API（`embeddings_ith` / `delete_by_filter` / `optimize`），每条注明原因，随 `cargo clippy` 一起跑。

## 技术选型

| 组件 | 选择 | 理由 |
|---|---|---|
| 向量引擎 | zvec（Rust 绑定） | 原生混合检索（HNSW+FTS+RRF），内嵌 jieba 中文零配置 |
| 嵌入 | Qwen3-Embedding-0.6B GGUF + llama.cpp | 中英双语强、CPU 可用 GPU 提速、本地推理无外发 |
| MCP | rmcp（官方 Rust SDK） | stdio + streamable HTTP 双传输 |
| daemon | herdr 模式 | 探测-拉起-等就绪、socket 单实例、零 lock 文件 |
| CLI 注册 | mcp2cli 绑定 | MCP 工具自动变类型化原生动词 |

## License

Apache-2.0
