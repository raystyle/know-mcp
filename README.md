# know-mcp

> 本地**代码搜索 + 文档搜索** MCP server：对归档的代码仓库和文档做关键字 + 向量语义混合检索，带行号读取，本地推理零外发。

一次构建，**MCP** 三种消费形态：**MCP STDIO**（`know-mcp stdio`，Claude Code 等本地客户端）、**MCP HTTP**（`know-mcp http`，多客户端网络共享）、**CLI 动词**（`know-mcp register` 原生注册 `know-cli`，MCP 工具一对一映射为命令行动词）。

## 功能

- **混合检索**：zvec 引擎内融合（HNSW 向量（Qwen3-Embedding-0.6B，1024 维，INT8 量化索引 + refiner 精排）+ jieba 中文分词 FTS + RRF 重排）；`auto` 模式按查询形态自动路由（标识符/中文 → 关键字，自然语言 → 混合）
- **代码感知分块**：token 预算自适应 CJK + tree-sitter AST 边界；代码 chunk 带 `rel_path [lang]` 上下文，markdown 带 heading breadcrumb 上下文
- **daemon 常驻**：探测-拉起-等就绪（冷启动自动拉起，首调用等待就绪）；索引常驻内存，多客户端共享；create_new 锁文件单实例互斥（并发拉起恰好一个赢）
- **增量索引**：mtime+size 快路径 → 内容哈希 → 确定性 chunk id 替换；二次运行秒级
- **三段流水线索引**：N 生产者线程（读+分块）→ GPU 嵌入（llama.cpp，CUDA）→ 有序入库；14k 文件全量 ~107 分钟（RTX 4060），增量重扫 11 秒
- **GPU 自适应**：检测到 CUDA 设备自动全层卸载，无 GPU 纯 CPU 亦可用，零配置
- **文本安全**：扩展名白名单 + NUL 嗅探 + UTF-8 校验三层过滤，二进制内容不入库
- **文档格式检索**（reader_rs 血统，全原生内建）：PDF（pdf-inspector 布局管线）与 Word/EPUB/Office/CSV 家族（anydoc 引擎）抽取文本层入索引；扫描页与图片走 PP-OCRv6 OCR 兜底（ppocr-rs 原生内核）；OCR 模型缓存 know-mcp 自持（`KNOW_OCR_CACHE_DIR` 或 `%LOCALAPPDATA%/know-mcp/ocr-models`，首用自动下载）

## 架构

```
┌─ MCP 客户端
│  形态一 STDIO:  know-mcp stdio      (Claude Code 等本地客户端)
│  形态二 HTTP:   know-mcp http       (→ /mcp, 多客户端共享)
│  形态三 CLI:    know-mcp register   (原生注册 know-cli <verb>)
▼
┌─ daemon (know-mcp daemon, 后台常驻, 单实例, 启动秒级)
│  ├─ JSON 管理面 (TCP loopback + admin.port 文件)
│  ├─ zvec collection (HNSW INT8 向量 + jieba FTS + 引擎级 RRF 混合)
│  ├─ Qwen3-Embedding-0.6B (懒加载: 索引/查询各自首次使用时加载, llama.cpp, GPU 自适应)
│  ├─ 文档抽取 (PDF: pdf-inspector 布局管线; Word/EPUB/Office: anydoc; OCR 兜底: ppocr-rs PP-OCRv6)
│  ├─ 索引任务 (显式触发: build_index / manage_index add_root; 三段流水线 producer×N → GPU embed → store)
│  └─ 索引锁 (daemon.lock 原子互斥 + PID 存活检测接管)
└─ CLI: status / stop / index / search / read (都走 daemon)
```

## MCP 工具（5 个）

| 工具 | 说明 |
|---|---|
| `search_content` | 混合检索；mode: auto/keyword/vector/hybrid；path_filter 范围过滤 |
| `read_file` | 带行号读取（roots 白名单校验，2000 行/256KB 上限；PDF/Word/EPUB/Office 走文本抽取管线，扫描页 OCR 兜底） |
| `get_status` | 索引/进度状态 |
| `build_index` | 触发增量重索引（force=true 全量重嵌入） |
| `manage_index` | 索引管理：查/加/删索引目录、清空索引（详见下表） |

## 工具参数详解

### `search_content` — 查索引（检索）

| 参数 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `query` | string | ✓ | 查询文本（中英文、关键词或自然语言） |
| `mode` | string | | `"auto"`（默认）/ `"keyword"`（FTS，适合标识符/CVE号/中文）/ `"vector"`（语义）/ `"hybrid"`（RRF 融合，默认走 auto 时选） |
| `top_k` | int | | 最多返回条数，默认 10 |
| `path_filter` | string | | 文件路径大小写不敏感子串过滤，如 `"zvec"` 只搜 zvec 相关路径 |

返回每条 hit 含 `path`（绝对路径）、`rel_path`、`title`、`score`、`start_line`/`end_line`（配合 `read_file` 取精确上下文）、`snippet`。

### `manage_index` — 指定目录建索引 / 查索引范围 / 删索引

`allowed_roots` 是配置文件里的授权边界；`add_root` 只能激活边界内的目录，边界外的路径一律拒绝。

| action | 额外参数 | 行为 |
|---|---|---|
| `list_roots` | — | 返回当前激活的 `roots` 与授权边界 `allowed_roots` |
| `add_root` | `path`（目录） | 激活一个目录并**立即触发索引**；新目录必须落在 `allowed_roots` 内，否则报错；写入 know-mcp.toml 持久化 |
| `remove_root` | `path`（目录） | 停用该目录并**删除其全部已索引数据**；同步回写配置 |
| `drop` | `confirm: true` | 清空**全部**索引数据（collection + sync state）；不带 confirm 会被拒绝；daemon 继续运行，之后可 rebuild |

示例（MCP 客户端视角）：

```jsonc
// 查当前索引了哪些目录
{"action": "list_roots"}
// → {"roots": ["E:/resources/code_repos"], "allowed_roots": ["E:/resources/..."]}

// 指定一个新目录建索引（必须在 allowed_roots 内）
{"action": "add_root", "path": "E:/resources/docments"}

// 删除某目录的索引
{"action": "remove_root", "path": "E:/resources/docments"}

// 清空整个索引
{"action": "drop", "confirm": true}
```

### `build_index` — 重跑索引任务

| 参数 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `force` | bool | | `true` = 忽略 mtime/哈希快路径，**全量重新嵌入**（换模型/改分块参数后用）；缺省 `false` = 增量（只处理变更文件） |

索引任务在 daemon 后台执行，用 `get_status` 轮询进度（`progress.running` / `scanned` / `total`）。索引模型懒加载：daemon 启动秒级就绪，首个索引/检索任务才加载嵌入模型。

### `get_status` — 查索引状态

无参数。返回：

```jsonc
{
  "doc_count": 1234,             // 已索引文档数
  "model": "qwen3-embedding-0.6b",
  "roots": ["..."],              // 当前激活的索引目录
  "index_dir": "...",            // 索引存储位置
  "progress": {                  // 后台索引任务进度
    "running": true, "scanned": 100, "total": 500,
    "updated": 90, "deleted": 3, "failed": 0, "started_at": "..."
  }
}
```

## 快速开始

```bash
cargo build --release
cp know-mcp.toml.example know-mcp.toml   # 编辑：roots / 模型路径 / index_dir
./target/release/know-mcp index        # 首次建索引（前台跑，看进度）
./target/release/know-mcp search "hybrid search"   # 冒烟
```

### 形态一：MCP STDIO（本地客户端，如 Claude Code）

```bash
claude mcp add know-search -- <repo>/target/release/know-mcp stdio
```

或 `settings.json`：

```json
{ "mcpServers": { "know-search": {
    "command": "<repo>/target/release/know-mcp",
    "args": ["stdio"] } } }
```

### 形态二：MCP HTTP（多客户端网络共享）

```bash
know-mcp http    # http://127.0.0.1:8765/mcp（挂 daemon）
```

### 形态三：CLI 动词（原生注册 know-cli）

```bash
know-mcp register        # 复制自身为 know-cli.exe（默认放 exe 同目录；--install-dir 指定；升级后 --force 重注册）

# 之后即得 know-cli（MCP 工具一对一动词，支持 --json 信封输出）：
know-cli get-status
know-cli search-content "混合检索" --mode hybrid --top-k 5
know-cli search-content "quantization" --mode keyword --json | jq '.data.hits[0].path'
know-cli read-file <path> --start-line 10
know-cli build-index --force
know-cli manage-index list_roots
```

零外部依赖：register 是原生子复制（同 mcp2cli 在 Windows 的 link 机制，但内化了）；动词直连 daemon 管理面，不经 MCP 协议层。

### CLI 速查

```
know-mcp daemon        # 显式启动守护（通常无需，其他命令自动拉起）
know-mcp status        # 状态（doc_count / roots / 进度）
know-mcp index         # 建库（--force 全量重嵌入）
know-mcp search        # 检索（--mode auto|keyword|vector|hybrid --top-k --path）
know-mcp read          # 带行号读文件（--start-line --end-line）
know-mcp manage        # 索引管理（与 manage_index MCP 工具对齐）
know-mcp stop          # 停守护
```

`manage` 子命令与 MCP 工具完全对齐：

```bash
know-mcp manage list-roots                  # 查激活目录 + 授权边界
know-mcp manage add-root E:/resources/docs  # 指定目录建索引（须在 allowed_roots 内）
know-mcp manage remove-root E:/resources/docs
know-mcp manage drop --confirm              # 清空全部索引
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
- 冷启动：daemon 秒级就绪（**懒加载**：模型在首个索引任务或向量搜索时才载入，一次性 ~30-60s）；首次工具调用自动拉起 daemon
- 依赖库路径：`.env` 里 `KNOW_LIB_DIR` 指向 DLL 目录（Windows 经 SetDllDirectoryW 注入加载器）

## 构建依赖

- Rust 1.98+ / MSVC 工具链
- **zvec C 库**：预编译库已归档在 `vendor/zvec-lib`（`source scripts/env.sh` 自动设 `ZVEC_LIB_DIR`），构建零网络依赖；换平台时从 [zvec-rust releases](https://github.com/zvec-ai/zvec-rust/releases) 下载对应 prebuilt 包替换
- **llama.cpp**（llama-cpp-2 crate，git rev `0364f5da`，源码编译）：需 cmake 与 LLVM（bindgen 用 libclang）；构建环境见 `scripts/env.sh`（`CMAKE_GENERATOR=Ninja` + `LIBCLANG_PATH` + CUDA 13.5 显式指定）
- **嵌入模型**：Qwen3-Embedding-0.6B Q8_0 GGUF 放 `models/`（gitignore），从 [Qwen/Qwen3-Embedding-0.6B-GGUF](https://huggingface.co/Qwen/Qwen3-Embedding-0.6B-GGUF) 下载
- **GPU（可选）**：CUDA Toolkit 13.5 + llama-cpp-2 `cuda-no-vmm` feature；首次索引小时级 → 分钟级

## 开发

```bash
just ci          # 完整门禁: fmt → clippy(-D warnings) → nextest
just test        # 测试 (nextest: 进程隔离 + slow-timeout 兜底)
just lint        # clippy -D warnings --all-targets
just build       # 发布构建 (thin-LTO)
```

测试观测：`test-support` crate 的 `with_phase(name, budget, f)` 提供阶段级时间预算断言 + `[phase]` 证据日志；`.config/nextest.toml` 配 slow-timeout/terminate-after（hang 即杀）与 CI profile（retries + JUnit）。首次运行需 `cargo install cargo-nextest` 与 `cargo install just`。

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
