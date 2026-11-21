# UGR: the wire format of a graph result

UGR carries a `GraphResult` or a `GraphDelta` (`ursa-viz`'s `model`) between
the engine and whatever draws it: an engine worker and its page, a kernel and
a notebook widget, a file and an exported page (design §6, #424). It is a
small JSON header followed by aligned little-endian arrays, so that:

- Rust reads and writes the arrays with `bytemuck`
  (`GraphResult::to_bytes` / `from_bytes`, and the same on `GraphDelta`);
- JavaScript reads them as typed arrays over the buffer, with no parsing and
  no copy (`ui/ugr.js`);
- the buffer is one transferable `ArrayBuffer`, one binary buffer of a
  notebook comm, or one base64 block in an HTML file.

This document is version 1 of the format, byte for byte. The model's
invariants, which a decoder checks, are in the `model` module's docs.

## Layout

All integers and floats are little-endian. `align8(x)` is `x` rounded up to a
multiple of 8.

| offset | size | what |
|---|---|---|
| 0 | 4 | the magic, ASCII `UGR1`. The digit is the version. |
| 4 | 4 | `H`, the header's length in bytes: `u32` |
| 8 | `H` | the header: one JSON object, UTF-8, no byte-order mark |
| 8 + `H` | 0–7 | zero bytes, up to `B` = `align8(8 + H)` |
| `B` | `body` | the body: the sections. `body` is a field of the header and a multiple of 8. |

The buffer ends where the body does: its length is `B + body`, a multiple
of 8.

### Sections

The header's `sections` lists each array in the body:

```json
{"name": "edges.src", "type": "u32", "offset": 24, "count": 4}
```

- `type` is `u8`, `u16`, `u32`, `u64` or `f32`.
- `offset` is in bytes from `B`, and is a multiple of 8. So in a buffer that
  itself starts at an aligned address, every section is aligned for its type.
- `count` is in elements. The section is `count × size(type)` bytes.
- The writer puts sections in the order listed, each followed by zero bytes
  up to the next multiple of 8. A reader locates a section by `name`, never
  by position, and ignores a section whose name it does not know.
- Names are unique.

**Text** is a pair of sections, `X.offsets` (`u32`) and `X.bytes` (`u8`): the
strings' UTF-8 end to end, and `n + 1` offsets into it for `n` strings.
String `i` is `bytes[offsets[i] .. offsets[i + 1]]`. The offsets start at 0,
never fall, end at the length of `bytes`, and each is on a character boundary.

**Sections left out.** Labels (`u16`) and flags (`u8`) are omitted when every
element is 0, and a reader takes an absent one as zeros. `edges.rows` is
omitted when no row is known.

### Nulls

| type | null |
|---|---|
| `f32` column | NaN |
| `u32` column, and a dictionary's codes | `0xFFFFFFFF` |
| an edge's row | `0xFFFFFFFFFFFFFFFF`: not known |

## A result

The header, with `"kind": "result"` (here the fixture
`tests/fixtures/plain.ugr`, three nodes and four edges, with some of its
sections):

```json
{
  "kind": "result",
  "graph": null,
  "nodes": {"count": 3, "labels": [{"name": "", "key": "int"}], "columns": [], "positions": false},
  "edges": {"count": 4, "labels": [""], "columns": []},
  "highlights": {"paths": 0, "seeds": 0, "distance": null},
  "encodings": {"size_by": null, "color_by": null, "group_by": null, "layout": "force"},
  "provenance": {"sql": "SELECT src, dst FROM 'edges.parquet'", "rule": "edges"},
  "body": 72,
  "sections": [
    {"name": "nodes.keys.offsets", "type": "u32", "offset": 0, "count": 4},
    {"name": "nodes.keys.bytes", "type": "u8", "offset": 16, "count": 7},
    {"name": "edges.src", "type": "u32", "offset": 24, "count": 4},
    {"name": "edges.dst", "type": "u32", "offset": 40, "count": 4}
  ]
}
```

| field | what |
|---|---|
| `graph` | the property graph the result is part of, or `null` |
| `nodes.count`, `edges.count` | how many slots, removed ones included |
| `nodes.labels` | the vertex tables: `name`, and `key`, the type of its keys: `"int"` (a signed 64-bit integer) or `"text"` |
| `edges.labels` | the edge tables' names |
| `nodes.columns`, `edges.columns` | each column's `name`, `type` (`"f32"`, `"u32"` or `"dict"`) and `role` (`null`, or the engine's `ursa.role`: `node`, `src`, `dst`, `seed`, `distance`, `path_nodes`, `path_edges`, `path_elements`, `metric`, `category`, `property`) |
| `nodes.positions` | whether the nodes have positions |
| `highlights` | how many `paths` and `seeds`, and `distance`: the name of the `u32` node column holding each node's distance from the seeds, or `null` |
| `encodings` | the node columns that size, colour and group (`null`: none), and `layout`: `"force"`, `"rings"` or `"layers"` |
| `provenance` | the `sql` that made the result and the result → graph `rule` that fired, each or `null`; and, each left out when empty, `roles` (the columns of the SQL's result the rule read, as `{"column", "role"}`, the role being the engine's `ursa.role`) and `notes` (what a status line adds: what was left out, what else the result could be drawn as) |

A reader ignores a field it does not know. A field or a value this version
does not define, where one it does define is required, is an error.

| section | type | count | what |
|---|---|---|---|
| `nodes.keys` | text | `nodes.count` | each node's key as text; an `int` label's in decimal, with no `+` and no leading zeros |
| `nodes.labels` | `u16` | `nodes.count` | each node's label, an index into `nodes.labels`. Omitted if all 0. |
| `nodes.flags` | `u8` | `nodes.count` | bit 0: the node is removed. Omitted if all 0. |
| `nodes.columns.I` | `f32` or `u32` | `nodes.count` | column `I`'s values; for a `dict` column, its codes |
| `nodes.columns.I.values` | text | any | a `dict` column's distinct values, which its codes index |
| `nodes.x`, `nodes.y` | `f32` | `nodes.count` | positions, if `nodes.positions` |
| `edges.src`, `edges.dst` | `u32` | `edges.count` | each edge's ends, as node indexes |
| `edges.labels` | `u16` | `edges.count` | each edge's label. Omitted if all 0. |
| `edges.rows` | `u64` | `edges.count` | each edge's row in its edge table. Omitted if none is known. |
| `edges.flags` | `u8` | `edges.count` | bit 0: the edge is removed. Omitted if all 0. |
| `edges.columns.I`, `.values` | | `edges.count` | as for nodes |
| `highlights.paths.offsets` | `u32` | paths + 1 | where each path's nodes start in `highlights.paths.nodes` |
| `highlights.paths.nodes` | `u32` | any | the paths' nodes, in order, end to end |
| `highlights.paths.edge_offsets` | `u32` | paths + 1 | the same for `highlights.paths.edges` |
| `highlights.paths.edges` | `u32` | any | each path's edges: none, or one between each two of its nodes |
| `highlights.seeds` | `u32` | seeds | the seed nodes |

A node's identity is its label and its key, and no two nodes of a result
share one. An index is a slot: a removed node or edge keeps its slot and its
data, and an edge is drawn when neither it nor either of its ends is removed.

The design's sketch of the format (§6) put the highlights in the header. They
are sections here, because a result of ten thousand paths is not a header.
It also listed no labels for edges and no flags; the model has both.

## A delta

The header, with `"kind": "delta"` (three of the ops of the fixture
`tests/fixtures/expand.ugr`, without their sections):

```json
{
  "kind": "delta",
  "provenance": {"sql": "SELECT * FROM hop(town, seeds => [1], n => 1)", "rule": "rings"},
  "ops": [
    {"op": "add_node_label", "name": "pets", "key": "text"},
    {"op": "add_nodes", "count": 2, "columns": [{"name": "club", "type": "dict"}, {"name": "pagerank", "type": "f32"}], "positions": true},
    {"op": "remove_nodes", "count": 2}
  ],
  "body": 128,
  "sections": []
}
```

`ops` apply in order. Op `I`'s sections are named `ops.I.…`. Each op has an
inverse that is also an op, so a delta is undone by its ops' inverses in
reverse order.

| `op` | inverse | header fields | sections under `ops.I.` |
|---|---|---|---|
| `add_node_label` | `pop_node_label` | `name`, `key` | |
| `add_edge_label` | `pop_edge_label` | `name` | |
| `add_nodes` | `pop_nodes` | `count`, `columns` (`name`, `type`), `positions` | `keys` (text), `labels` (`u16`, omitted if all 0), `columns.J`, `columns.J.values`, `x`, `y` |
| `add_edges` | `pop_edges` | `count`, `columns`, `rows` (whether `rows` is there) | `src`, `dst`, `labels`, `rows`, `columns.J`, `columns.J.values` |
| `remove_nodes` | `restore_nodes` | `count` | `index` (`u32`): the nodes |
| `remove_edges` | `restore_edges` | `count` | `index`: the edges |
| `set_column` | itself, `old` and `new` swapped | `target` (`"nodes"` or `"edges"`), `index`, `old`, `new` (each a column's `name`, `type`, `role`, or `null`) | `old`, `old.values`, `new`, `new.values` |
| `set_highlights` | itself, swapped | `old`, `new` (each as a result's `highlights`) | `old.paths.…`, `old.seeds`, `new.paths.…`, `new.seeds` |
| `set_encodings` | itself, swapped | `old`, `new` (each as a result's `encodings`) | |
| `set_positions` | itself, swapped | `old`, `new` (each whether there are positions) | `old.x`, `old.y`, `new.x`, `new.y` |

- `add_nodes` and `add_edges` append slots; `pop_nodes` and `pop_edges` take
  the same ones back off the end, and carry the same data.
- A batch's `columns` name columns the result has. For a `dict` column,
  `columns.J.values` is the text the column does not have yet, and the codes
  index the column's values followed by those. A column the batch leaves out
  gets nulls.
- `set_column` replaces the column at `index` (`old` → `new`), adds one there
  (`old` is `null`), or drops it (`new` is `null`).
- `remove_…` sets the removed flag and `restore_…` clears it. Slots do not
  move.

A delta's bytes do not say which result it is for. Applying one checks that
it fits (`GraphResult::apply`), and applies all of it or none.

## What a decoder refuses

Each with a message that says which: a buffer shorter than its header or its
body say (truncated), bytes after the body, another magic or version, a header
that is not the JSON above, a section that is missing, of another type or
count than the header gives, unaligned, or past the body's end, text that is
not UTF-8, and any value that breaks the model's invariants (two nodes of one
identity, a key that is not its label's type, an edge to a node that is not
there, a code past its dictionary, a path whose edges do not join its nodes).
The Rust decoder checks all of these. `ui/ugr.js` checks the framing and the
sections, and trusts the values, as a view of the engine's own output can.

## Size and speed

For the design's reference graph (46,318 nodes with integer keys, 1,000,000
edges; `tests/model_scale.rs`, release build, one core):

| | bytes | encode | decode and validate |
|---|---|---|---|
| nodes and edges only | 8,510,552: 8,000,000 of edges, 509,502 of keys, 1,050 of header | 2.8 ms | 6.3 ms |
| with edge labels, rows and flags, an edge column, a node column and positions | 23,113,312 | 15 ms | 9.4 ms |
| a step of 100 nodes and 200 edges, as a delta | under 4,096 | | 4.2 ms decoded and applied; 0.9 ms undone |

The design measured 4.5 ms to copy a million edges between two wasm
instances, and budgets a frame (16 ms) for an undo; these are within both.
The test asserts the sizes always, and the times in a release build against
budgets several times these.

The 1,050 bytes that are not edges or keys are the header. Nothing in the
format grows with the graph but the arrays themselves.

## Compatibility

- The magic's digit is the version. A reader refuses a version it does not
  know; there is no negotiation.
- Within version 1, a writer may add header fields and sections, which
  readers ignore. Anything a version-1 reader would misread is version 2.
- Encoding is canonical: equal values give equal bytes. The fixtures in
  `tests/fixtures/` are version 1's, and `tests/ugr_fixtures.rs` fails if the
  encoder stops writing them.
