# 嵌入流水线笔记

三段流水线：N 生产者线程（读文件 + 分块）→ GPU 嵌入（llama.cpp，CUDA）→ 有序入库（zvec）。
吞吐基线 92 chunks/s（RTX 4060，Qwen3-Embedding-0.6B Q8_0）。
