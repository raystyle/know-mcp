"""Hybrid search service for the smoke corpus.

Demonstrates the Python AST chunking path: top-level functions become
chunk boundaries, and the rel_path [lang] context is prepended.
"""


def hybrid_search(query: str, top_k: int = 10, mode: str = "auto") -> list[dict]:
    """Fused keyword + vector search with RRF rerank.

    auto mode routes by query shape: identifiers and CJK text go to the
    keyword channel, natural language goes to hybrid.
    """
    if mode == "auto":
        mode = _route(query)
    keyword_hits = _bm25_search(query, top_k)
    vector_hits = _ann_search(query, top_k)
    if mode == "keyword":
        return keyword_hits
    if mode == "vector":
        return vector_hits
    return _rrf_fuse(keyword_hits, vector_hits, top_k)


def _rrf_fuse(a: list[dict], b: list[dict], top_k: int) -> list[dict]:
    """Reciprocal Rank Fusion: score = sum(1 / (60 + rank))."""
    scores: dict[str, float] = {}
    for hits in (a, b):
        for rank, hit in enumerate(hits):
            scores[hit["path"]] = scores.get(hit["path"], 0.0) + 1.0 / (60 + rank)
    ranked = sorted(scores.items(), key=lambda kv: -kv[1])
    return [{"path": p, "score": s} for p, s in ranked[:top_k]]


def _route(query: str) -> str:
    if _is_identifier(query) or _has_cjk(query):
        return "keyword"
    return "hybrid"


def _bm25_search(query: str, top_k: int) -> list[dict]:
    return [{"path": f"bm25://{query}/{i}", "score": 1.0} for i in range(top_k)]


def _ann_search(query: str, top_k: int) -> list[dict]:
    return [{"path": f"ann://{query}/{i}", "score": 0.9} for i in range(top_k)]


def _is_identifier(query: str) -> bool:
    return query.replace("_", "").isidentifier() and " " not in query


def _has_cjk(text: str) -> bool:
    return any("一" <= ch <= "鿿" for ch in text)
