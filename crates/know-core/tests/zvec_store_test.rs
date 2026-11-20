//! Integration test: ZvecStore against a real zvec collection
//! (requires the zvec_c_api shared library — staged by zvec-rust-build).

use know_core::store::{ChunkDoc, SearchOptions, SearchStore};
use know_core::zvec_store::ZvecStore;
use know_core::Kind;

fn chunk(pk: &str, text: &str, embedding: Vec<f32>) -> ChunkDoc {
    ChunkDoc {
        pk: pk.to_string(),
        kind: Kind::Archive,
        path: format!("D:/fake/{pk}.md"),
        root: "D:/fake".to_string(),
        rel_path: format!("{pk}.md"),
        title: pk.to_string(),
        text: text.to_string(),
        chunk_index: 0,
        start_line: 1,
        end_line: 10,
        lang: "md".to_string(),
        embedding,
    }
}

#[test]
fn store_crud_and_three_search_modes() {
    let tmp = tempfile::tempdir().unwrap();
    let store = ZvecStore::open(tmp.path()).expect("open store");

    // 1024-dim embeddings: two clusters (positive axis, negative axis).
    let mut pos = vec![0.0f32; 1024];
    pos[0] = 1.0;
    let mut neg = vec![0.0f32; 1024];
    neg[1] = 1.0;

    let docs = vec![
        chunk("ml-0", "machine learning is powerful 混合检索", pos.clone()),
        chunk(
            "ml-1",
            "deep learning uses neural networks 神经网络",
            pos.clone(),
        ),
        chunk(
            "rust-0",
            "Rust is a systems language 系统编程语言",
            neg.clone(),
        ),
    ];
    store.upsert(&docs).expect("upsert");
    store.flush().expect("flush");
    assert_eq!(store.doc_count().expect("count"), 3);

    // Keyword: FTS should rank the two "learning" docs.
    let opts = SearchOptions {
        top_k: 10,
        kind_filter: None,
    };
    let kw = store
        .search_keyword("learning", &opts)
        .expect("keyword search");
    assert!(!kw.is_empty(), "keyword search returned nothing");
    let pks: Vec<&str> = kw.iter().map(|h| h.title.as_str()).collect();
    assert!(
        pks.contains(&"ml-0") || pks.contains(&"ml-1"),
        "keyword hits: {pks:?}"
    );

    // Chinese FTS via jieba.
    let zh = store.search_keyword("混合检索", &opts).expect("zh keyword");
    assert!(
        zh.iter().any(|h| h.title == "ml-0"),
        "jieba should hit ml-0: {:?}",
        zh.iter().map(|h| h.title.clone()).collect::<Vec<_>>()
    );

    // Vector: query near the positive cluster → ml-0/ml-1 on top.
    let vv = store.search_vector(&pos, &opts).expect("vector search");
    assert!(!vv.is_empty());
    assert_eq!(vv[0].kind, Kind::Archive);
    let top_titles: Vec<String> = vv.iter().map(|h| h.title.clone()).collect();
    assert!(top_titles.contains(&"ml-0".to_string()));

    // Hybrid: text+vector fused.
    let hy = store
        .search_hybrid("learning", &pos, &opts)
        .expect("hybrid search");
    assert!(!hy.is_empty());

    // Delete + re-count.
    store.delete(&["rust-0".to_string()]).expect("delete");
    assert_eq!(store.doc_count().expect("count after delete"), 2);
}
