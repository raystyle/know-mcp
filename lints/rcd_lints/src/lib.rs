//! rcd_lints: project-specific semantic lints for rcd-mcp-server.
//!
//! Every rule here is distilled from a real bug we hit during development
//! (see git history and the daemon log archives). They exist so those bugs
//! cannot silently come back.
//!
//! - `RCD_EMBEDDINGS_ITH`: llama-cpp-2 `embeddings_ith` returns per-token
//!   rows; with pooling=LAST the pooled vector lives in the *sequence* slot.
//!   We shipped a bug where every embedding was zeros until this became
//!   `embeddings_seq_ith`.
//! - `RCD_LLAMA_DOUBLE_INIT`: `LlamaBackend::init()` is once-per-process;
//!   two call sites (even each behind their own OnceLock) panic with
//!   BackendAlreadyInitialized. Must sit inside a shared `get_or_init`.
//! - `RCD_LLAMA_CTX_REUSE`: decoding a second sequence into the same llama
//!   context without `clear_kv_cache()` fails with "inconsistent sequence
//!   positions".
//! - `RCD_ZVEC_DELETE_BY_FILTER`: zvec `delete_by_filter` throws when
//!   nothing matches — our store uses deterministic pks + `delete` instead.
//! - `RCD_ZVEC_OPTIMIZE`: `optimize()` trips Parquet footer errors on some
//!   volumes; keep it behind an explicit `optimize` opt-in.
//! - `RCD_ZVEC_UPSERT_WITHOUT_FLUSH`: upserts must be followed by `flush()`
//!   in the same function (WAL contract; a crash between batches otherwise
//!   loses the batch).
//! - `RCD_UNDETACHED_COMMAND`: spawns must null all three stdio handles
//!   (detached-daemon hygiene).
#![feature(rustc_private)]
#![warn(unused_extern_crates)]

extern crate rustc_driver;
extern crate rustc_errors;
extern crate rustc_lint;
extern crate rustc_session;
extern crate rustc_hir;

use rustc_errors::{Diag, DiagDecorator};
use rustc_hir::{Expr, ExprKind, Node};
use rustc_lint::{LateContext, LateLintPass, LintContext};
use rustc_session::{declare_lint, declare_lint_pass};

declare_lint! {
    pub RCD_EMBEDDINGS_ITH,
    Deny,
    "`embeddings_ith` yields per-token rows and returns nothing useful under pooling; use `embeddings_seq_ith`"
}
declare_lint! {
    pub RCD_LLAMA_DOUBLE_INIT,
    Warn,
    "`LlamaBackend::init()` outside a shared `OnceLock::get_or_init` risks BackendAlreadyInitialized panics"
}
declare_lint! {
    pub RCD_LLAMA_CTX_REUSE,
    Warn,
    "multiple `decode` calls on one context without an intervening `clear_kv_cache` fail with inconsistent sequence positions"
}
declare_lint! {
    pub RCD_ZVEC_DELETE_BY_FILTER,
    Warn,
    "`delete_by_filter` throws when nothing matches; prefer deterministic pks + `delete`"
}
declare_lint! {
    pub RCD_ZVEC_OPTIMIZE,
    Warn,
    "`optimize` is fragile on some volumes (Parquet footer errors); keep it behind an explicit opt-in check"
}
declare_lint! {
    pub RCD_ZVEC_UPSERT_WITHOUT_FLUSH,
    Warn,
    "`upsert` without a following `flush` in the same function loses the batch on crash"
}
declare_lint! {
    pub RCD_UNDETACHED_COMMAND,
    Warn,
    "spawn without stdin/stdout/stderr set to null can inherit console handles"
}

declare_lint_pass!(RcdLints => [
    RCD_EMBEDDINGS_ITH,
    RCD_LLAMA_DOUBLE_INIT,
    RCD_LLAMA_CTX_REUSE,
    RCD_ZVEC_DELETE_BY_FILTER,
    RCD_ZVEC_OPTIMIZE,
    RCD_ZVEC_UPSERT_WITHOUT_FLUSH,
    RCD_UNDETACHED_COMMAND,
]);

/// Body-snippet heuristics: each rule scans the source text of the enclosing
/// function. This survives rustc-internals churn far better than deep HIR
/// walks, and each rule's help text documents the bug it guards against.
impl<'tcx> LateLintPass<'tcx> for RcdLints {
    fn check_expr(&mut self, cx: &LateContext<'tcx>, expr: &'tcx Expr<'tcx>) {
        let ExprKind::MethodCall(path, _recv, _args, span) = expr.kind else { return };
        let name = path.ident.name.as_str();

        match name {
            // Bug: all-zero embeddings under pooling=LAST.
            "embeddings_ith" => {
                cx.opt_span_lint(RCD_EMBEDDINGS_ITH, Some(span), DiagDecorator(
                    |diag: &mut Diag<'_, ()>| {
                        diag.primary_message("called `embeddings_ith`, which yields per-token rows");
                        diag.help(
                            "with pooling enabled the pooled embedding lives in the sequence \
                             slot — use `embeddings_seq_ith(0)` (this exact bug produced \
                             all-zero embeddings once)",
                        );
                    },
                ));
            }
            // Bug: BackendAlreadyInitialized panic from two separate OnceLocks.
            "init" => {
                let Some(snippet) = snippet_of(cx, expr) else { return };
                if snippet.contains("LlamaBackend::init") && !snippet.contains("get_or_init") {
                    cx.opt_span_lint(RCD_LLAMA_DOUBLE_INIT, Some(span), DiagDecorator(
                        |diag: &mut Diag<'_, ()>| {
                            diag.primary_message("`LlamaBackend::init()` outside `get_or_init`");
                            diag.help(
                                "init is once-per-process; route every caller through one \
                                 shared `OnceLock::get_or_init` (two separate OnceLocks \
                                 panicked with BackendAlreadyInitialized during development)",
                            );
                        },
                    ));
                }
            }
            // Bug: decode #2 without clear_kv_cache → decode failure.
            "decode" => {
                let Some(snippet) = snippet_of(cx, expr) else { return };
                if snippet.contains(".decode(") {
                    let decodes = snippet.matches(".decode(").count();
                    let clears = snippet.matches("clear_kv_cache").count();
                    if decodes > 1 && clears < decodes - 1 {
                        cx.opt_span_lint(RCD_LLAMA_CTX_REUSE, Some(span), DiagDecorator(
                            |diag: &mut Diag<'_, ()>| {
                                diag.primary_message("context reused across sequences without clearing");
                                diag.help(
                                    "call `ctx.clear_kv_cache()` between sequences — positions \
                                     must restart at 0, otherwise decode fails with \
                                     'inconsistent sequence positions'",
                                );
                            },
                        ));
                    }
                }
            }
            // Gotcha: no-match → Err.
            "delete_by_filter" => {
                cx.opt_span_lint(RCD_ZVEC_DELETE_BY_FILTER, Some(span), DiagDecorator(
                    |diag: &mut Diag<'_, ()>| {
                        diag.primary_message("`delete_by_filter` throws when nothing matches");
                        diag.help("prefer deterministic chunk pks + `delete(pks)` (our store contract)");
                    },
                ));
            }
            // Gotcha: Parquet footer errors on some volumes.
            "optimize" => {
                let Some(snippet) = snippet_of(cx, expr) else { return };
                if snippet.contains(".optimize()") && !snippet.contains("if optimize") {
                    cx.opt_span_lint(RCD_ZVEC_OPTIMIZE, Some(span), DiagDecorator(
                        |diag: &mut Diag<'_, ()>| {
                            diag.primary_message("unconditional `optimize()`");
                            diag.help("guard with `if optimize` (explicit --optimize opt-in; fragile on some volumes)");
                        },
                    ));
                }
            }
            // Contract: WAL flush per batch.
            "upsert" => {
                let Some(snippet) = snippet_of(cx, expr) else { return };
                if snippet.contains(".upsert(") && !snippet.contains(".flush(") {
                    cx.opt_span_lint(RCD_ZVEC_UPSERT_WITHOUT_FLUSH, Some(span), DiagDecorator(
                        |diag: &mut Diag<'_, ()>| {
                            diag.primary_message("`upsert` without `flush` in the same function");
                            diag.help("call `flush()` after each batch so a crash cannot lose it");
                        },
                    ));
                }
            }
            // Hygiene: detached spawns.
            "status" | "spawn" => {
                let Some(snippet) = snippet_of(cx, expr) else { return };
                if snippet.contains("Command") {
                    let has = |m: &str| snippet.contains(m);
                    if !(has(".stdin(") && has(".stdout(") && has(".stderr(")) {
                        cx.opt_span_lint(RCD_UNDETACHED_COMMAND, Some(span), DiagDecorator(
                            |diag: &mut Diag<'_, ()>| {
                                diag.primary_message("spawn without all stdio handles set");
                                diag.help(
                                    "set .stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()) \
                                     — or document why inheriting handles is intended",
                                );
                            },
                        ));
                    }
                }
            }
            _ => {}
        }
    }
}

/// Source text of the enclosing function (item span), best-effort.
fn snippet_of(cx: &LateContext<'_>, expr: &Expr<'_>) -> Option<String> {
    let mut id = expr.hir_id;
    for _ in 0..24 {
        let parent = cx.tcx.parent_hir_id(id);
        match cx.tcx.hir_node(parent) {
            Node::Item(_) | Node::ImplItem(_) | Node::TraitItem(_) => {
                return cx.sess().source_map().span_to_snippet(cx.tcx.hir_span(parent)).ok();
            }
            _ => id = parent,
        }
    }
    None
}

/// dylint ABI: exported symbol returning the runtime-expected version
/// (dylint_driver 6.4.0 expects "0.1.0"). Mirrors dylint_linting's
/// `dylint_library!` expansion without the extra dependency.
#[unsafe(no_mangle)]
pub extern "C" fn dylint_version() -> *mut std::os::raw::c_char {
    std::ffi::CString::new("0.1.0")
        .expect("static version string")
        .into_raw()
}

/// dylint ABI: the driver calls this to register our lint passes.
#[unsafe(no_mangle)]
pub fn register_lints(_sess: &rustc_session::Session, lint_store: &mut rustc_lint::LintStore) {
    lint_store.register_lints(&[
        RCD_EMBEDDINGS_ITH,
        RCD_LLAMA_DOUBLE_INIT,
        RCD_LLAMA_CTX_REUSE,
        RCD_ZVEC_DELETE_BY_FILTER,
        RCD_ZVEC_OPTIMIZE,
        RCD_ZVEC_UPSERT_WITHOUT_FLUSH,
        RCD_UNDETACHED_COMMAND,
    ]);
    lint_store.register_late_lint_pass(Box::new(|_| Box::new(RcdLints)));
}
