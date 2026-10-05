//! The pgvector embedding-retrieval path (ADR 0028): with an embedding
//! backend configured, `write`/`put` store a `vector` column and `retrieve`
//! does an EXACT per-query cosine scan (`ORDER BY embedding <=> $q`) with NO
//! ANN index — so ranking is per-query and a `forget` (row DELETE) removes the
//! embedding too (reversibility of the vector path, not just the BM25 one).
//!
//! Uses a deterministic MOCK embedder (a fixed 3-dim vector per topic word),
//! so the whole pgvector path is exercised end to end with no embedding
//! server. Skipped when `KELTER_TEST_POSTGRES_URL` is unset.

use std::sync::Arc;

use async_trait::async_trait;
use memory::embeddings::{EmbeddingBackend, EmbeddingError};
use memory::{Episode, EpisodeOutcome, MemoryProvider, Provenance};
use pgstore::{PgMemoryProvider, PgStore};
use uuid::Uuid;

/// Deterministic topic embedder: one orthonormal axis per topic word, so the
/// nearest-cosine result for a query is predictable.
struct MockEmbedder;

#[async_trait]
impl EmbeddingBackend for MockEmbedder {
    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, EmbeddingError> {
        Ok(texts
            .iter()
            .map(|t| {
                let t = t.to_lowercase();
                if t.contains("alpha") {
                    vec![1.0, 0.0, 0.0]
                } else if t.contains("beta") {
                    vec![0.0, 1.0, 0.0]
                } else if t.contains("gamma") {
                    vec![0.0, 0.0, 1.0]
                } else {
                    vec![0.577, 0.577, 0.577]
                }
            })
            .collect())
    }
    fn describe(&self) -> String {
        "mock:topic-embedder".into()
    }
}

fn pg_url() -> Option<String> {
    std::env::var("KELTER_TEST_POSTGRES_URL").ok()
}

fn episode(task: &str) -> Episode {
    Episode {
        id: Uuid::new_v4(),
        run_id: Uuid::new_v4(),
        task: task.to_string(),
        trace: vec![],
        outcome: EpisodeOutcome::Success,
        provenance: Provenance::Reflection,
        version: 1,
    }
}

#[tokio::test]
#[ignore = "needs a live Postgres+pgvector: set KELTER_TEST_POSTGRES_URL and run with --include-ignored (see AGENTS.md)"]
async fn pgvector_cosine_retrieval_and_forget_removes_the_vector() {
    let Some(url) = pg_url() else {
        eprintln!("KELTER_TEST_POSTGRES_URL unset — skipping pgvector test");
        return;
    };
    let store = PgStore::connect(&url, Some(Arc::new(MockEmbedder)))
        .await
        .expect("connect");
    assert!(store.uses_embeddings());
    let mem = PgMemoryProvider::new(store);

    let tag = Uuid::new_v4();
    let a = episode(&format!("{tag} alpha topic report"));
    let b = episode(&format!("{tag} beta topic report"));
    let g = episode(&format!("{tag} gamma topic report"));
    mem.write(a.clone()).await.unwrap();
    mem.write(b.clone()).await.unwrap();
    mem.write(g.clone()).await.unwrap();

    // A "beta"-flavoured query must rank the beta episode first via cosine —
    // this is pgvector's `<=>` operator doing the work, not BM25 (all three
    // tasks share the same words except the topic).
    let hits = mem.retrieve("beta please", 3).await.unwrap();
    assert!(!hits.is_empty());
    assert_eq!(
        hits[0].episode_id, b.id,
        "cosine nearest is the beta episode: {hits:?}"
    );
    assert!(hits[0].score >= hits.last().unwrap().score);

    // Clean up these rows so the shared DB doesn't accumulate across runs —
    // and, incidentally, prove there is no vector residue: after deleting the
    // rows, a cosine query returns none of them.
    let client = mem_store_client(&url).await;
    for e in [&a, &b, &g] {
        client
            .execute("DELETE FROM episodes WHERE id = $1", &[&e.id])
            .await
            .unwrap();
    }
    let after = mem.retrieve("beta please", 5).await.unwrap();
    assert!(
        after.iter().all(|h| h.episode_id != b.id),
        "the deleted row's embedding is gone — cosine scan can't surface it"
    );
}

// A tiny direct client for the cleanup DELETE (the provider has no delete API
// for episodes — episodes are pruned by retention elsewhere; here we only need
// to prove the vector column vanishes with the row).
async fn mem_store_client(url: &str) -> deadpool_postgres::Object {
    use deadpool_postgres::{Config, ManagerConfig, RecyclingMethod, Runtime};
    let pg: tokio_postgres::Config = url.parse().unwrap();
    let mut cfg = Config::new();
    cfg.manager = Some(ManagerConfig {
        recycling_method: RecyclingMethod::Fast,
    });
    cfg.host = pg.get_hosts().first().and_then(|h| match h {
        tokio_postgres::config::Host::Tcp(s) => Some(s.clone()),
        _ => None,
    });
    cfg.port = pg.get_ports().first().copied();
    cfg.user = pg.get_user().map(String::from);
    cfg.password = pg
        .get_password()
        .map(|p| String::from_utf8_lossy(p).into_owned());
    cfg.dbname = pg.get_dbname().map(String::from);
    let pool = cfg
        .create_pool(Some(Runtime::Tokio1), tokio_postgres::NoTls)
        .unwrap();
    pool.get().await.unwrap()
}

/// ADR 0038 on the pgvector path: an `unverified` episode is filtered in SQL
/// (before LIMIT) and never retrieved; its `success` version is.
#[tokio::test]
#[ignore = "needs a live Postgres+pgvector: set KELTER_TEST_POSTGRES_URL and run with --include-ignored (see AGENTS.md)"]
async fn pgvector_path_never_retrieves_unverified() {
    let Some(url) = pg_url() else {
        eprintln!("KELTER_TEST_POSTGRES_URL unset — skipping pgvector test");
        return;
    };
    let store = PgStore::connect(&url, Some(Arc::new(MockEmbedder)))
        .await
        .expect("connect");
    let mem = PgMemoryProvider::new(store);
    // No topic word → the embedder's neutral vector, so this test never ties
    // with the alpha/beta/gamma rows the cosine test above ranks.
    let mut e = episode(&format!("{} unverified probe", Uuid::new_v4()));
    e.outcome = EpisodeOutcome::Unverified;
    mem.write(e.clone()).await.unwrap();
    let ids = |hits: Vec<memory::RankedContext>| -> Vec<Uuid> {
        hits.into_iter().map(|h| h.episode_id).collect()
    };
    assert!(!ids(mem.retrieve("unverified probe", 10_000).await.unwrap()).contains(&e.id));

    e.version = 2;
    e.outcome = EpisodeOutcome::Success;
    e.provenance = Provenance::Human;
    mem.write(e.clone()).await.unwrap();
    assert!(ids(mem.retrieve("unverified probe", 10_000).await.unwrap()).contains(&e.id));
}
