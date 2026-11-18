## Comparison: bsky-5m (5,119,823 edges, 107,689 nodes)

- Host: MacBook M-series, 12 cores, 32 GB, 12 CPUs; threads per engine: 12
- Limits per cell: 900 s, 16.0 GB

| Algorithm | Engine | Status | Ingest | Cold | Warm | Peak RSS | Cores | Correct |
|---|---|---|---:|---:|---:|---:|---:|---|
| degree | ursa-sql | ok | 117 ms | 177 ms | 59 ms | 0.24 GB | 1.7 |  |
| degree | ursa-df | ok | 3 ms | 159 ms | 0 ms | 0.19 GB | 1.2 | yes (exact) |
| degree | igraph | ok | 826 ms | 828 ms | 2 ms | 1.07 GB | 0.9 | yes (exact) |
| degree | rustworkx | ok | 586 ms | 619 ms | 20 ms | 0.99 GB | 0.9 | yes (exact) |
| degree | networkx | ok | 4.43 s | 4.44 s | 13 ms | 1.37 GB | 1.0 | yes (exact) |
| pagerank | ursa-sql | ok | 117 ms | 335 ms | 218 ms | 0.24 GB | 2.5 |  |
| pagerank | ursa-df | ok | 0 ms | 166 ms | 60 ms | 0.20 GB | 3.7 | yes (max abs diff 0.00e+00 (0.0e+00 of the top score); top-10 overlap 10/10) |
| pagerank | igraph | ok | 816 ms | 1.21 s | 391 ms | 1.07 GB | 1.0 | yes (max abs diff 3.67e-07 (7.4e-05 of the top score); top-10 overlap 10/10) |
| pagerank | rustworkx | ok | 570 ms | 1.70 s | 1.12 s | 0.99 GB | 1.0 | yes (max abs diff 6.50e-08 (1.3e-05 of the top score); top-10 overlap 10/10) |
| pagerank | networkx | ok | 4.41 s | 6.79 s | 2.69 s | 2.30 GB | 1.0 | yes (max abs diff 4.94e-16 (1.0e-13 of the top score); top-10 overlap 10/10) |
| wcc | ursa-sql | ok | 117 ms | 283 ms | 166 ms | 0.24 GB | 3.3 |  |
| wcc | ursa-df | ok | 0 ms | 105 ms | 17 ms | 0.19 GB | 1.6 | yes (41 components, same partition) |
| wcc | igraph | ok | 809 ms | 928 ms | 120 ms | 1.07 GB | 1.0 | yes (41 components, same partition) |
| wcc | rustworkx | ok | 582 ms | 1.02 s | 427 ms | 0.99 GB | 1.0 | yes (41 components, same partition) |
| wcc | networkx | ok | 4.39 s | 5.06 s | 658 ms | 1.38 GB | 1.0 | yes (41 components, same partition) |
| scc | ursa-sql | ok | 117 ms | 288 ms | 171 ms | 0.24 GB | 3.4 |  |
| scc | ursa-df | ok | 0 ms | 116 ms | 28 ms | 0.19 GB | 1.4 | yes (29057 components, same partition) |
| scc | igraph | ok | 816 ms | 905 ms | 77 ms | 1.07 GB | 1.0 | yes (29057 components, same partition) |
| scc | rustworkx | ok | 576 ms | 1.21 s | 621 ms | 0.99 GB | 1.0 | yes (29057 components, same partition) |
| scc | networkx | ok | 4.33 s | 5.37 s | 595 ms | 1.39 GB | 1.0 | yes (29057 components, same partition) |
| triangles | ursa-sql | ok | 117 ms | 1.40 s | 1.29 s | 0.24 GB | 8.9 |  |
| triangles | ursa-df | ok | 0 ms | 1.26 s | 1.09 s | 0.25 GB | 9.4 | yes (exact) |
| triangles | igraph | **unsupported** |  |  |  |  |  |  |
| triangles | rustworkx | **unsupported** |  |  |  |  |  |  |
| triangles | networkx | ok | 12.29 s | 35.31 s | 23.06 s | 2.07 GB | 1.0 | yes (exact) |
| louvain | ursa-sql | ok | 117 ms | 661 ms | 544 ms | 0.24 GB | 2.2 |  (modularity 0.4197; 74 communities (reference)) |
| louvain | ursa-df | ok | 0 ms | 504 ms | 380 ms | 0.46 GB | 1.7 | yes (modularity 0.4197 vs ursa-sql 0.4197; 74 communities) |
| louvain | igraph | ok | 1.20 s | 15.40 s | 14.07 s | 1.07 GB | 1.0 | yes (modularity 0.4245 vs ursa-sql 0.4197; 64 communities) |
| louvain | rustworkx | **unsupported** |  |  |  |  |  |  |
| louvain | networkx | ok | 12.29 s | 50.00 s | 38.06 s | 2.73 GB | 1.0 | yes (modularity 0.4083 vs ursa-sql 0.4197; 66 communities) |

\* first run only: warm skipped because the first took longer than the warm limit.
Cold = file to result (ingest + first run). For ursa-sql, warm is a new `ursa` process with the index stored, so it includes loading the index.
