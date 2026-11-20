# know-mcp 统一质量闭环入口：本地与 CI 执行完全相同的序列。
# 顺序：fmt → clippy（含项目禁令）→ 测试（nextest，含阶段预算与隔离）→ 冒烟。

default:
    @just --list

# 格式检查
fmt:
    cargo fmt --all -- --check

# 官方 Clippy + 项目 disallowed-methods 禁令
lint:
    cargo clippy --workspace --all-targets --no-deps -- -D warnings

# 全量测试（nextest：进程级隔离 + slow-timeout 兜底）
# 首次需安装：cargo install cargo-nextest
test:
    cargo nextest run --profile default

# CI 门禁（重试 + JUnit 报告）
test-ci:
    cargo nextest run --profile ci

# doctest（nextest 不支持，单独跑）
test-doc:
    cargo test --doc

# 发布构建
build:
    cargo build --release

# 完整门禁 = 本地 CI 等价物
ci: fmt lint test
