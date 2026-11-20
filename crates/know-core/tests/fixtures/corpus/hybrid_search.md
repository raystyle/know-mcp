# 混合检索 Hybrid Search

混合检索融合两条通道：关键字（BM25/FTS，jieba 中文分词）与向量语义（Qwen3-Embedding，1024 维），
引擎侧用 RRF（Reciprocal Rank Fusion）重排。auto 模式按查询形态路由：
标识符与中文走关键字，自然语言走混合。
