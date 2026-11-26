# 冒烟语料 Smoke Corpus

项目内测试语料：用于 know-mcp 的冒烟、端到端与验收测试。
内容覆盖中英文、代码与文档（含 PDF 文档格式），供 keyword / vector / hybrid
三种检索模式与文档抽取管线验证。

顶层（旧语料，单层布局）：

- `hybrid_search.md` — 混合检索说明文档
- `daemon.rs` — daemon 启动示例代码
- `embed_notes.md` — 嵌入流水线笔记

`code/` — 代码语料（AST 分块路径验证，tree-sitter 各语言）：

- `code/search_service.py` — Python：hybrid search 服务（标识符/RRF/路由）
- `code/daemon_mutex.go` — Go：daemon 互斥与关闭（Supervisor/Worker）
- `code/store_client.ts` — TypeScript：zvec store 客户端（interface/泛型）

`docs/` — 文档语料：

- `docs/vector_notes_en.md` — 英文嵌入管线笔记（跨语言语义检索验证）
- `docs/检索手册.md` — 中文检索指南（jieba 关键字 + 向量双通道）
- `docs/hybrid_baseline.pdf` — 单页文本层 PDF（pdf-inspector 抽取管线验证；
  命中词：`quantization`、`refiner`、`granularity`）
