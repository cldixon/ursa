# Concepts

Ursa has a small model. Three ideas explain almost all of the API.

<div class="grid cards" markdown>

-   **[Frames](frames.md)**

    ---

    There is no `Graph` object. An `EdgeFrame` *is* the graph. A `NodeFrame` is an optional table of node attributes. Every operation returns a frame.

-   **[Expressions](expressions.md)**

    ---

    You describe columns with expressions, as in Polars. A graph algorithm is an expression too. Nothing runs until you call `collect()`.

-   **[Direction](direction.md)**

    ---

    Every edge row is directed. Direction is a parameter of each operation, not a property of the graph.

</div>

## The shape of a query

Most Ursa code has this shape:

```py
result = (
    edges.nodes()                       # 1. start from the nodes of a graph
    .with_columns(                      # 2. add computed columns
        pagerank=ur.pagerank(edges),
        degree=ur.degree(edges),
    )
    .filter(ur.col("degree") > 2)       # 3. shape the rows
    .sort("pagerank", descending=True)
    .head(10)
    .collect()                          # 4. run it
)
```

Steps 1 to 3 build a plan and do no work. Step 4 runs the plan as one query in the engine and returns the rows as Arrow data.

## Where the work happens

Ursa is three layers:

| Layer | Language | Role |
|---|---|---|
| `ursa` | Python | The API you write. It builds a plan. |
| `ursa-plan` | Rust | Turns the plan into one [DataFusion](https://datafusion.apache.org/) query. Graph operations are nodes in that query. |
| `ursa-core` | Rust | The graph index (CSR) and the algorithm kernels. Parallel, deterministic. |

You never call the Rust layers directly. Data crosses the boundary as Arrow, without a copy.
