# ADR 0005: Memory as a provider interface with scored retrieval

Status: accepted · Date: 2026-07-18

## Context

Agent memory implementations tend to hard-wire one store and "dump context
into the prompt", making retrieval untestable and backends unswappable.

## Decision

Memory is a stable provider protocol (`memory::MemoryProvider`):

```
write(episode)                 // provenance-tagged, versioned
retrieve(task) -> ranked_context  // scored relevance, bounded
upsert(entity | relation)      // graph-shaped backends plug in here
```

The reference implementation is in-memory with a simple token-overlap
relevance score — deliberately naive, but *scored and unit-tested*, which is
the contract every real backend (knowledge graph, vector store, hybrid) must
keep.

The self-improvement loop enters memory only through a **gated reflection
step**: after each run the kernel writes one `Episode` tagged
`provenance: reflection`, versioned, and the gate
(`[kernel] reflection = false`) turns it off entirely. No silent
self-mutation of memory; distillation into semantic/procedural layers is a
later phase behind the same gate.

## Consequences

- Backends are swappable without touching the kernel.
- Retrieval quality is a measurable, testable property from day one.
- Every memory write is attributable (`reflection | human | import`) and
  reversible in principle (versioned).
