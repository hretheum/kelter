//! Lexical retrieval scoring — Okapi BM25 over a document collection.
//!
//! Deliberately dependency-free: tokenization + BM25 with in-collection
//! IDF. This is the tested scoring contract every provider uses; swapping
//! in embeddings later happens behind the same `MemoryProvider::retrieve`
//! seam without touching callers.

use std::collections::HashMap;

const K1: f64 = 1.2;
const B: f64 = 0.75;

pub fn tokenize(s: &str) -> Vec<String> {
    s.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| t.len() > 1)
        .map(|t| t.to_string())
        .collect()
}

/// Rank `docs` (id, text) against `query`; returns (index into docs, score)
/// for scoring > 0, best first, at most `limit`.
pub fn rank_bm25(query: &str, docs: &[String], limit: usize) -> Vec<(usize, f64)> {
    let query_terms = tokenize(query);
    if query_terms.is_empty() || docs.is_empty() {
        return Vec::new();
    }

    let tokenized: Vec<Vec<String>> = docs.iter().map(|d| tokenize(d)).collect();
    let n = docs.len() as f64;
    let avg_len = tokenized.iter().map(|t| t.len() as f64).sum::<f64>() / n.max(1.0);

    // Document frequency per query term.
    let mut df: HashMap<&str, f64> = HashMap::new();
    for term in &query_terms {
        let count = tokenized
            .iter()
            .filter(|doc| doc.iter().any(|t| t == term))
            .count() as f64;
        df.insert(term.as_str(), count);
    }

    let mut scored: Vec<(usize, f64)> = tokenized
        .iter()
        .enumerate()
        .map(|(i, doc)| {
            let len = doc.len() as f64;
            let mut score = 0.0;
            for term in &query_terms {
                let tf = doc.iter().filter(|t| *t == term).count() as f64;
                if tf == 0.0 {
                    continue;
                }
                let dfi = df[term.as_str()];
                // BM25 IDF with the +1 smoothing (never negative).
                let idf = ((n - dfi + 0.5) / (dfi + 0.5) + 1.0).ln();
                score +=
                    idf * (tf * (K1 + 1.0)) / (tf + K1 * (1.0 - B + B * len / avg_len.max(1.0)));
            }
            (i, score)
        })
        .filter(|(_, s)| *s > 0.0)
        .collect();

    scored.sort_by(|a, b| b.1.total_cmp(&a.1));
    scored.truncate(limit);
    scored
}

/// Normalize BM25 scores into (0, 1] relative to the best hit — keeps the
/// `RankedContext.score` contract stable across scoring backends.
pub fn normalize(scored: &[(usize, f64)]) -> Vec<(usize, f64)> {
    let max = scored
        .first()
        .map(|(_, s)| *s)
        .unwrap_or(1.0)
        .max(f64::EPSILON);
    scored.iter().map(|(i, s)| (*i, s / max)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn corpus() -> Vec<String> {
        vec![
            "extract links and headings from a webpage into structured json".into(),
            "download a file from a url and store it locally".into(),
            "summarize the quarterly sales report for the finance team".into(),
            "append an entry to the audit ledger with an idempotency key".into(),
            "compute word statistics for a markdown document".into(),
            "write a markdown report from extracted structure".into(),
            "read a local file and save its raw content".into(),
            "retrieve past episodes relevant to the current task".into(),
        ]
    }

    fn top1(query: &str) -> usize {
        rank_bm25(query, &corpus(), 3)[0].0
    }

    #[test]
    fn bm25_ranks_the_expected_document_first() {
        // A small quality contract: each query's intended document wins.
        assert_eq!(top1("extract structured links from a webpage"), 0);
        assert_eq!(top1("download url file"), 1);
        assert_eq!(top1("quarterly sales summary"), 2);
        assert_eq!(top1("ledger idempotency append"), 3);
        assert_eq!(top1("word statistics markdown"), 4);
        assert_eq!(top1("markdown report from structure"), 5);
    }

    #[test]
    fn bm25_beats_naive_overlap_on_term_rarity() {
        // "markdown" appears in several docs; "statistics" is rare. Naive
        // overlap would tie docs 4 and 5 for "markdown statistics"; BM25's
        // IDF must prefer the doc with the rare term.
        let hits = rank_bm25("markdown statistics", &corpus(), 2);
        assert_eq!(hits[0].0, 4);
        assert!(hits[0].1 > hits[1].1);
    }

    #[test]
    fn irrelevant_queries_return_nothing() {
        assert!(rank_bm25("zzz qqq xyzzy", &corpus(), 5).is_empty());
        assert!(rank_bm25("", &corpus(), 5).is_empty());
    }

    #[test]
    fn normalize_caps_at_one() {
        let hits = normalize(&rank_bm25("download url file", &corpus(), 5));
        assert!((hits[0].1 - 1.0).abs() < f64::EPSILON);
        assert!(hits.iter().all(|(_, s)| *s > 0.0 && *s <= 1.0));
    }
}
