---
title: Architecture
description: Arrow at the boundaries, index in the middle. Three crates, a CSR topology index, and graph operators as first-class nodes in a DataFusion plan.
subtitle: The design authority is docs/SPEC.md in the repository; this page is the summary.
---

The governing rule for the whole system:

> **Arrow at the boundaries, index in the middle.** Every graph kernel takes Arrow columns plus a
> shared topology index in, and hands Arrow arrays out.

"Everything is a frame" is not a marketing promise. It is a mechanical consequence of that
operator contract.

## Crate layout

```
ursa/
├── ursa-core/    # Topology (CSR) + algorithm kernels. Pure Rust: arrow + rayon.
│                 # NO DataFusion dependency. Independently testable and benchmarkable.
├── ursa-plan/    # DataFusion extensions: custom logical nodes + ExecutionPlan impls
│                 # (-> ursa-core), the query builder, scan/session/object_store plumbing.
│                 # The ONE seam where our dialect lowers to DataFusion.
├── ursa-py/      # PyO3 bindings. Thin: plan builders, collect(), Arrow FFI.
└── python/ursa/  # The Python package: dialect, frames, IO, graph verbs, stats.
```

The `ursa-core` boundary is load-bearing: the kernels have no DataFusion dependency, so they build
and test in seconds and can be benchmarked in isolation with criterion.

## Why DataFusion, not the Polars crates

Decided, not open:

1. **Extensibility is the designed use case.** Custom logical plan nodes, user-registered
   optimizer rules and pluggable `ExecutionPlan` operators let graph operations be first-class
   citizens of *one* unified query plan. Polars' planner is closed; building on it condemns the
   project to a permanent "traffic cop" architecture coordinating two engines from outside.
2. **Stability posture.** DataFusion is Apache-governed infrastructure with an ecosystem of
   dependent engines. The Polars crates are explicitly unstable and subordinate to the Polars
   Python product.
3. **Free capabilities.** `object_store` integration delivers the S3/GCS/Azure story nearly for
   free; SQL over registered tables is available later at negligible cost.
4. **Room to diverge.** Fixpoint iteration, frontier execution and eventually worst-case-optimal
   joins for motifs need plan shapes no tabular engine anticipates.

The accepted cost: the public API is a Polars-*shaped* dialect, not literal `pl.Expr`. It is
quarantined at one seam — `python/ursa/_expr.py` builds the dialect and `ursa-plan/src/query.rs`
lowers it. There will be no Polars-expression translator; interop is Arrow, and Arrow interop is
zero-copy regardless of engine.

## The topology index

A CSR (compressed sparse row) adjacency structure with three load-bearing details.

**Dense internal indexing.** User node ids are arbitrary — gappy int64, strings, UUIDs. Kernels
never see them. Index construction builds a bidirectional mapping to dense `u32` indices `0..n`,
so all kernels operate over flat arrays and a PageRank vector is literally a `Vec<f64>` indexed by
node. Results translate back to user ids only when materializing output batches. `u32` caps the
node space at ~4.29 B, which is the correct trade for cache behaviour.

**The edge permutation array.** CSR reorders edges by source, but edge *attributes* stay in the
original Arrow columns, unduplicated. Alongside `offsets` (length `n+1`) and `targets` (length
`m`), the index keeps `edge_ids` mapping each CSR slot to its original row. A weighted kernel
evaluates the weight expression into an Arrow array first, then gathers `weight[edge_ids[k]]` on
the fly. Topology in the index; properties in Arrow; nothing copied twice. It is also the hook for
future subgraph views — a bitmask over a parent CSR instead of a rebuild after `filter`.

**Directional laziness.** CSR gives out-neighbours; in-neighbours need the transpose. Each
direction is built independently on first demand, so a pull-based PageRank pipeline does not pay
for both. Budget: roughly 12 bytes per edge plus 8 bytes per node, per direction.

**Lifecycle.** The index lives on the EdgeFrame behind an `Arc` with lazy per-direction slots, and
is immutable once built. Property-only transformations clone the `Arc`; structural transformations
drop it; concurrent queries over one frame share one build. That is the
[index-preservation contract](/docs/concepts#immutability-and-the-index-preservation-contract).

## Graph operators in the plan

Graph kernels are **physical operators that happen to consult a side data structure**. DataFusion
already contains pipeline breakers — sort, hash aggregate — that materialize before emitting; an
iterative graph algorithm is architecturally the same. A PageRank operator blocks, runs its
fixpoint over the CSR, and emits Arrow batches into the downstream plan.

Each `collect()` is **one** `LogicalPlan`: `Limit → Sort → Filter → GraphAlgorithmNode`, where
`GraphAlgorithmNode` is a real `UserDefinedLogicalNode` lowered to a `GraphAlgorithmExec` by our
own `ExtensionPlanner`.

The custom logical nodes: `GraphAlgorithmNode`, `HopNode`, `ShortestPathNode`, `RandomWalkNode`,
neighbour aggregation.

The optimizer rules — the cross-boundary moves no tabular optimizer knows — are registered
alongside DataFusion's built-ins and are **future work**:

- push node-set filters *before* traversal (filter the seeds, then hop)
- prune columns before materializing frontiers
- fuse `neighbors().agg(...)` into a segmented reduction over CSR, never materializing neighbour
  lists
- share one topology build across multiple graph ops in a plan

The v0.1 position was decided deliberately: ship the custom nodes with *naive* placement first —
execute in written order, correctness over cleverness — and land the rules incrementally. The
public API is identical either way, so semantics get proven before optimization.

## Kernel shapes

Knowing the four computational shapes bounds the work of adding an algorithm.

| Shape | Algorithms | Technique |
|---|---|---|
| Fixpoint iteration | PageRank, label propagation, Louvain phases | dense per-node vectors; Rayon sweep per iteration |
| Frontier expansion | BFS, k-hop, unweighted shortest path | visited bitmaps + frontier queues |
| Adjacency intersection | triangle count, clustering coefficient | sorted adjacency lists, parallel merge-intersections |
| Priority / disjoint-set | Dijkstra SSSP, connected components (union-find), Brandes betweenness | per-algorithm |

The GAP Benchmark Suite reference implementations are the canonical versions of exactly these
kernels; Ursa ports from them rather than inventing them. Ursa still owns its topology struct,
because the Arrow coupling and the `edge_ids` permutation are load-bearing.

## Two runtime traps, handled

**Thread pools.** DataFusion executes on tokio (async, IO-oriented); kernels want Rayon
(data-parallel compute). Running Rayon loops on tokio workers starves the runtime, so graph
execution plans dispatch compute off the async workers and stream results back.

**The GIL.** `collect()` releases the GIL for the duration of execution, so Ursa behaves inside
threaded Python servers. Arrow crosses the boundary through the PyCapsule interface, zero-copy
with polars and pyarrow.

## Version and interop policy

Arrow is the compatibility contract. `to_polars`/`from_polars` and `to_arrow`/`from_arrow` are
zero-copy via the C data interface. There is no dependency on the Polars Rust crates; `polars` for
Python is optional and touched only in interop shims. MSRV and the DataFusion version are pinned
per release.
