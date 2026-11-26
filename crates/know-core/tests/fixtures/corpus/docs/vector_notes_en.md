# Embedding Pipeline Notes

These notes cover the query and document embedding paths for the
hybrid search daemon, written in English to exercise the semantic
(vector) channel on cross-language queries.

## Pooling and normalization

The embedding model uses last-token pooling: the final hidden state of
the sequence becomes the sentence vector. Documents are embedded
verbatim while queries are wrapped in an instruction template, which
lifts retrieval quality for asymmetric search. Every vector is
L2-normalized before insertion so cosine similarity reduces to a dot
product inside the index.

## Quantization

Vectors are stored as float32 but indexed through an INT8 scalar
quantization layer: the HNSW graph traverses compressed codes, and a
refiner stage re-scores the top candidates with full precision before
fusion. The recall/throughput tradeoff was measured against a flat
brute-force baseline before enabling quantization.

## Batching discipline

The GPU embeds documents in batches sized by a token budget rather
than a chunk count, so one very long chunk cannot evict the rest of
its batch. Producers stage chunk text on the host; the embed thread
drains the queue; the store thread persists in deterministic order so
incremental passes stay reproducible.
