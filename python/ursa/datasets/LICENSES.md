# Bundled dataset attributions

The CSV files in `data/` are small, canonical graph datasets redistributed from
[NetworkX](https://networkx.org/) (3-clause BSD license), regenerated into a
plain `src,dst[,weight]` / `id,<label>` CSV form. Each is a long-established
public dataset widely used for teaching and benchmarking.

| File | Dataset | Original source |
|------|---------|-----------------|
| `karate_edges.csv`, `karate_nodes.csv` | Zachary's karate club | W. W. Zachary, *An Information Flow Model for Conflict and Fission in Small Groups*, Journal of Anthropological Research 33 (1977). |
| `lesmis_edges.csv` | Les Misérables co-appearance | D. E. Knuth, *The Stanford GraphBase* (1993). |
| `florentine_edges.csv` | Florentine families marriage ties | J. F. Padgett & C. K. Ansell, *Robust Action and the Rise of the Medici* (1993). |
| `kite_edges.csv` | Krackhardt kite | D. Krackhardt, *Assessing the Political Landscape* (1990). |

NetworkX bundles these graphs and ships them under the BSD license; the CSV
regeneration here is a format change only. See
<https://github.com/networkx/networkx/blob/main/LICENSE.txt>.

Downloaded datasets (e.g. SNAP ego-Facebook) are **not** bundled — they are
fetched from their original hosts on first use and cached locally; their terms
are those of the upstream source (e.g. the
[SNAP](https://snap.stanford.edu/data/) dataset terms).
