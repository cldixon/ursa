# Changelog

All notable changes to this project are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

The distribution is `ursa-graph`; the import name is `ursa`.

## [Unreleased]

### Added

- **Ursa SQL 0.6: a SQL engine for graph analytics, and the `ursa`
  command-line tool** (crate `ursa-cli`, released as 0.6.0 on 2026-11-06;
  the engine crates are `ursa-engine` and `ursa-sql`). This is its first
  release. It is versioned apart from the Python package. Changes on `main`
  since then are released as `ursa-cli` 0.7.0 (2026-11-06), 0.8.0
  (2026-11-07), 0.9.0 (2026-11-10), 0.10.0 (2026-11-17), 0.11.0
  (2026-11-18), 0.12.0 (2026-11-24) and 0.13.0 (2026-11-25).
  - Any Parquet or CSV file is a table: `SELECT * FROM 'edges.parquet'`,
    with globs, directories, compressed CSV, and `s3://`, `gs://`, `az://`
    and `https://` URLs. Built on DataFusion 55.2. DuckDB's FROM-first
    queries work: `FROM t [WHERE …]` is `SELECT * FROM t …` (#306).
  - Graphs are defined with SQL/PGQ `CREATE PROPERTY GRAPH`: any number of
    vertex and edge tables, `AS` names, `LABEL`, `PROPERTIES`, and a vertex
    `KEY` that may be left out (the primary key, or the column the edges
    `REFERENCES`, as DuckPGQ infers it), so DuckPGQ graph definitions run as
    written (#295), DuckPGQ's `PROPERTIES (…) LABEL x` order included
    (#307). Ursa adds optional vertex tables. A graph of one edge
    table is a schema: `g.edges` and `g.nodes`. An algorithm runs on one
    vertex table and one edge table, named as DuckPGQ names them when a graph
    has several: `pagerank(g, person, knows)`. `DROP PROPERTY GRAPH`,
    `SHOW GRAPHS` and `DESCRIBE [PROPERTY] GRAPH` manage them.
  - SQL/PGQ pattern matching: `GRAPH_TABLE (g MATCH (a)-[e]->(b) WHERE …
    COLUMNS (…))` with labels, every edge direction, element and pattern
    conditions, and several paths sharing variables, planned as joins
    (#296). Quantified edges (`-[e]->{1,3}`, `->*`, `+`, `?`), path
    variables, `ANY SHORTEST`, `ALL SHORTEST`, `SHORTEST k`, the path modes
    `TRAIL`, `SIMPLE` and `ACYCLIC`, and `path_length(p)`, `vertices(p)`,
    `edges(p)` and `element_id(p)`, searched breadth-first on the graph
    index rather than joined; among equally short paths, the one whose edges
    come first in the edge table (#297).
  - Every algorithm and statistic of the Python library (not the layouts or
    the neighbour aggregations) is a table function with named parameters
    (`pagerank(g, damping => 0.9)`), with the same names, defaults and
    values. `algorithms(g, …)` runs
    several over one index in one call. `hop`, `random_walk` and
    `shortest_path` are traversals, with seeds from a list or a subquery.
    `weight =>` takes any expression over the edge columns.
  - `SELECT * FROM ursa_functions()` lists every graph function with its
    parameters, defaults, accepted values, outputs and NetworkX equivalent.
  - A `.ursa` database file keeps views, external tables and graphs, and
    stores the graph indexes it builds, so a later process does not rebuild
    them. A changed source file is noticed and its index rebuilt. `ATTACH`,
    `--readonly`, and `SET`/`SHOW ursa.index_storage_limit`.
  - An interactive shell with DuckDB-style dot-commands, `-init` files,
    `~/.ursarc` and history; output as table, CSV, TSV, JSON, NDJSON or
    Arrow (`--format`); `-c`, `-f`/`--file` and stdin run SQL and exit,
    as in DuckDB; `ursa --version`.
  - Docs: a new SQL section on the docs site, with a function reference
    generated from `ursa_functions()`. Every SQL example in it runs in CI.
  - An agent skill for writing Ursa SQL, in `skills/ursa-sql/`.
  - Prebuilt binaries for Linux (x86_64, aarch64), macOS (x86_64, Apple
    silicon) and Windows (x64) on the GitHub Releases page, and a one-line
    installer for them that checks each download's SHA-256 checksum:
    `curl -fsSL https://cldixon.github.io/ursa/install.sh | sh`, or
    `irm https://cldixon.github.io/ursa/install.ps1 | iex` on Windows.
- **Ursa SQL: DuckDB familiarity** (#313).
  - `.jsonl` and `.ndjson` files, and compressed ones (`.jsonl.gz`,
    `.ndjson.zst`, `.bz2`, `.xz`), are read as newline-delimited JSON, as
    `.json` is, by path, glob, directory or URL. `COPY … TO 'out.jsonl'`
    writes them.
  - `read_parquet(path)`, `read_csv(path, header =>, delim =>, quote =>,
    compression =>)` and `read_json(path, compression =>, format =>
    'array')` read a file in the format they name, whatever its extension.
    The path may be a glob, a directory, a URL or a list of paths; options
    are named with `=>` or DuckDB's `=`. `read_csv_auto`, `read_json_auto`
    and `read_ndjson` are other names. A view over one, in a database,
    keeps its paths relative to the database file.
  - `SHOW TABLES` leaves out `information_schema`'s own views.
  - Docs: a "Differences from DuckDB" page, the file formats and `read_*`
    functions in the statements page, and `COPY`'s row count.
- **Ursa SQL: more DuckDB and DuckPGQ habits** (#417). Each of these failed
  with a parser error or a message that did not name the fix.
  - `COPY … TO 'f' (FORMAT parquet, HEADER, DELIMITER '|', COMPRESSION
    zstd)`, and `WITH (…)`: DuckDB's option list maps onto `STORED AS` and
    `OPTIONS`. `DELIM` and `SEP` are `DELIMITER`'s other names; any other
    option is refused by name.
  - `SET threads = n` sets `datafusion.execution.target_partitions`. `SET`
    of another name without an `ursa.` or `datafusion.` prefix says that
    Ursa's settings start with one, and that `SHOW ALL` lists them.
  - `DROP GRAPH g` and `SHOW PROPERTY GRAPHS`. DuckPGQ's `SUMMARIZE
    PROPERTY GRAPH g` returns `describe(g)` for a graph of one edge table
    and says to name the pair for more. `ALTER PROPERTY GRAPH` says to use
    `CREATE OR REPLACE PROPERTY GRAPH`.
  - `ORDER BY ALL` orders by every column of the `SELECT` list; after
    `SELECT *`, the error says to name the columns or their positions.
  - `SHOW 'file.csv'` is `DESCRIBE 'file.csv'`. `CREATE TEMP TABLE` and
    `CREATE TEMP VIEW` run; a database file does not store them.
  - Hints: `SELECT pagerank(g)`, for every graph function, says it is a
    table function (`SELECT * FROM pagerank(g)`); `SELECT id, pagerank FROM
    g.nodes` says to join the function's rows on `id` (#300); an unquoted
    file name, `FROM karate_edges.csv`, says to quote the path.
  - DuckDB's names for DataFusion's functions: `list_value` (`make_array`),
    `strftime` (`to_char`, either argument order), `epoch_ms` (milliseconds
    to a timestamp and back), and the aggregates `arg_max` and `arg_min`
    (`first_value … ORDER BY`, skipping NULLs).
  - Tests: `DESCRIBE FROM`, `EXPLAIN FROM`, `CREATE VIEW v AS FROM` and
    `COPY (FROM t) TO` together, on DuckPGQ's graph.
- **`ursa` for agents and scripts** (#312):
  - `-f` files, `-c` and SQL on stdin take dot-commands, as `-init` files
    and `.read` do, and as in DuckDB: `.timer on`, `.mode csv`, `.read`. In
    a script, `.quit` and `.exit` end the run with status 0, and an unknown
    dot-command is an error. With `--format arrow`, a dot-command's text
    (the timer's, say) goes to stderr, so stdout stays one Arrow stream.
  - `ursa --skill` prints the agent skill (`skills/ursa-sql/SKILL.md`),
    built into the binary. `--help` ends with examples, a
    `CREATE PROPERTY GRAPH` line, `ursa_functions()`, `--format json`,
    `--skill` and the docs URL; the shell's `.help` and the installers point
    to the docs too.
  - `.functions NAME` shows one function's summary, its parameters one a
    line (type, default, what it does, the values it takes) and its output
    columns. A name that is not a function gets a suggestion.
  - DuckDB statements Ursa does not have (`SUMMARIZE`, `PIVOT`, `UNPIVOT`,
    `INSTALL`, `LOAD`, `PRAGMA`) and the `USING SAMPLE` clause are refused
    with what to write instead, not with a parser error.
  - `TABLESAMPLE` is refused. DataFusion parsed it but read every row, so a
    sample silently returned the whole table.
- **`ursa`: CLI and semantics polish** (#328).
  - A table shows at most 40 rows, as DuckDB's does: a longer result shows
    its first 20 and last 20 rows, three rows of `·` between them, and a
    footer, `39,650,447 rows (40 shown)`. `.maxrows N` (in the shell, a
    script or an init file) and `--max-rows N` change it, with 0 for every
    row; `--max-rows` wins over an init file's `.maxrows`. CSV, TSV, JSON,
    NDJSON and Arrow still write every row. Before, `FROM degree(g)` on a
    39.65M-node graph printed 39.65M lines.
  - SIGINT (Ctrl-C) cancels a `-c`, `-f` or stdin run: `Error: interrupted`
    on stderr, nothing more on stdout, exit status 130, and the database's
    lock released. Before, a `louvain` sent SIGINT ran to completion,
    printed its result and exited 0. In the shell, Ctrl-C still cancels the
    running statement and returns to the prompt, now also for an algorithm,
    and the shell exits at once afterwards.
  - `.timer` reports `Run Time (s): real … user … sys …`, as DuckDB does:
    CPU time in user code and in the kernel, over all threads, as well as
    wall time.
  - A syntax error names a place in the text you wrote. Before, its line and
    column pointed into the text the `GRAPH_TABLE` and FROM-first rewrites
    made of it. A reserved word used as a name says to quote it
    (`` `from` is a reserved word; to use it as a name, quote it: "from" ``).
  - `SOURCE KEY (from) DESTINATION KEY (to)` works: the FROM-first rewrite
    no longer reads `(from)` as a query, which gave "Expected: ), found: *".
  - An unknown column that is part of a name with a `-` in it (`date` from an
    unquoted `date-created`) says to quote the name.
  - A date out of a nanosecond timestamp's range in `date_trunc` (or another
    timestamp function) says to filter the dates or use `extract(year FROM
    …)`, after arrow's cast error.
  - Docs: `degree` counts each parallel edge, and its reference shows how to
    count distinct neighbours; integer division and decimals in "Differences
    from DuckDB"; convert large vertex CSV files to Parquet once, in the
    database and statements pages.
- **`ursa`: tables and prompt as DuckDB's.** A table is a box drawn with
  line characters, each column's type under its name (Arrow's name for it,
  as `DESCRIBE` and `arrow_typeof()` give it), numbers aligned right and
  `NULL` for a null. In a terminal it fits the terminal's width:
  long values are cut short and middle columns left out, with a `…` column
  and a footer, `17 columns (10 shown)`; `.maxwidth N` sets the width. Types
  and nulls are dimmed unless `NO_COLOR` is set. The prompt names the
  database, `memory U` or `club U` for `club.ursa`, and a statement's next
  lines are indented to line up with it instead of starting ` ...>`, so a
  statement copied from the terminal is still SQL. The browser shell draws
  tables with the same code, the new crate `ursa-shell`.
- **`ursa`: the shell is a library, and the browser runs it.** The shell
  (dot-commands, SQL a line at a time, output formats) moves from `ursa-cli`
  to `ursa-shell`, and `ursa-wasm`'s `WebShell` is that shell for a page's
  terminal, as DuckDB's web shell is DuckDB's CLI: fed the Getting Started
  script a line at a time it prints what `ursa` prints. `.help` lists the
  commands as DuckDB does, `.help PATTERN` the matching ones, `.help graph`
  is a page on the graph SQL and `.help shortcuts` the keys. New `.version`
  and `.print`. In a colour terminal what is typed is highlighted (keywords,
  strings, comments, dot-commands), the prompt is orange, and errors are red.
- **Ursa Web Shell: a guided tour, and "Try it live" links from the docs**
  (#391, and the discoverability half of #390). The Datasets menu's first
  item, `Tour: the karate club` (also `.tour`, or `?dataset=tour` in the
  URL), tells one story in five steps, each with a comment for a newcomer:
  the club's data and its property graph, PageRank (the two leaders come out
  on top), Louvain (its two factions match the club's real split for 33 of
  34 members), a drawing of the club, and every shortest path between the
  two leaders with `GRAPH_TABLE … ALL SHORTEST`. Its tables fit a phone. The
  karate dataset stays as it was, next to it. Each dataset now ends with a
  comment that says how to draw it (`.graph lesmis`; for rmat, that the
  layout takes a few seconds), and `.help` starts with `.tour` and `.graph`.
  The README, the SQL docs' landing page and the website (its landing page,
  the docs' overview, Introduction and Quickstart) link to the shell with
  their example preloaded, as a Share link that shows its statements and
  runs them on `y`, or with `?dataset=`. `web/scripts/share-link.ts` makes
  such links, encoded as the shell's Share does. The e2e test runs the tour
  on a phone and replays every docs link.
- **Ursa Web Shell: a start hint.** Under the banner, one line points to the
  sample graphs: `Pick a sample graph from the Datasets menu, or type
  .dataset karate` (broken between words on a narrow screen).
- **Ursa Web Shell: the browser e2e suite runs in CI** (#392). The "Ursa
  Shell (browser e2e)" job builds the engine and the page, stages the sample
  datasets locally, serves them as the Worker does and runs `web/tests/e2e.ts`
  in headless Chromium, on pull requests that touch the shell or the crates
  in it; screenshots and logs are uploaded when it fails.
- **Ursa Web Shell: the Graph view.** `.graph` draws the last result as a
  graph in a pane beside the terminal (under it on a phone), and so does
  the new Graph button: each row is an edge from its `src` column to its
  `dst` column, or else from its first column to its second, with ids of
  any type. `.graph QUERY` draws a query's result and `.graph NAME` a
  property graph (every edge table, and every vertex, edges or not) or a
  table. The shell says what it draws, `Drawing 78 edges, 34 nodes (Esc
  closes)`, and keeps the keys: queries can be typed while the graph shows,
  and the terminal refits to its half. The drawing is `ursa-viz`'s (WebGPU,
  or else WebGL2), its ForceAtlas2 layout converging live in a worker,
  nodes sized by degree: drag or the arrow keys pan, the wheel or `+` and
  `-` zoom, a double-click or `0` fits, and a click names a node. It is
  ink on white by day and starlight by night. Esc or the close button
  closes it; New and the Datasets menu do too. It is loaded on the first
  `.graph` only (about 1 MB gzipped), and draws at most 2,000,000 edges.
  `WebShell.graph(text)` gives the page the graph: node ids mapped to
  dense indexes in the order they are first seen, with clear errors when
  there is nothing to draw.
- **Ursa Web Shell: pinch to zoom the Graph view** (#428). A phone had no
  way to zoom: one finger panned, and the wheel and `+`/`-` need a mouse or
  a keyboard. Two fingers now pinch: the view zooms by the change in their
  distance around the point between them and pans as that point moves, so
  what is between the fingers stays between them. A second finger cancels
  the first one's tap, a finger left down after a pinch goes on panning, and
  a double tap fits the graph in view, as a double-click does. The canvas
  draws at most 2 pixels for each CSS pixel, so a phone with a ratio of 3
  fills under half the pixels each frame. The mouse, the wheel and the keys
  are as before.
- **Ursa Web Shell: sizes, colours and a legend for the Graph view**
  (#389). `.graph [SOURCE] [NODES (query)] [SIZE BY x] [COLOR BY y]`:
  `.graph karate SIZE BY pagerank COLOR BY louvain` sizes nodes by PageRank
  and colours them by Louvain community. x and y name a column of `NODES
  (SELECT id, … FROM …)`, a vertex column of the property graph, `degree`,
  or a per-node algorithm, run on the graph with defaults through
  `algorithms(G, …)` and joined on id. A query or the last result takes
  `NODES` and `degree` only. Errors say what fits: an unknown name lists the
  graph's columns and the algorithms, `SIZE BY` text suggests `COLOR BY`,
  and too many categories suggests `SIZE BY`. `COLOR BY` colours the eight
  largest categories and draws the rest as "other"; the pane's legend names
  what sizes the nodes and each colour's category and count. The engine does
  it all (`WebShell.graph` returns `size`, `color` and the legend; with no
  clause, the graph as before).
- **A spike: the graph view's layout on threads in the browser** (#503).
  Nothing in the release changes. `spikes/viz-threads` builds `ursa-viz`'s
  layout twice from one source, unthreaded on stable and threaded on a
  pinned nightly (`+atomics`, a rebuilt `std`), and runs the threaded one on a
  pool of Web Workers over one shared `WebAssembly.Memory`. On four cores in
  Chromium the layout is 2.0 to 2.3 times faster, with the same bits as the
  unthreaded build at every thread count, for 33 KB more gzipped and about
  25 ms more start-up. The report and its recommendation (yes, as an
  optional build) are `design/threaded-layout-spike.md`. A CI job of its
  own, `Threaded layout spike`, runs the determinism check when the spike
  changes.
- **Ursa Web Shell: the page runs cross-origin isolated** (#429). Every
  response, from the Worker and the local preview alike, carries
  `Cross-Origin-Opener-Policy: same-origin`, `Cross-Origin-Embedder-Policy:
  require-corp` and `Cross-Origin-Resource-Policy: same-origin`, beside the
  Content Security Policy. `crossOriginIsolated` is true on the page and in
  the engine's and the layout's workers, which gives precise timers and
  `performance.measureUserAgentSpecificMemory()` now, and is what a build
  with shared memory would need. Every resource was same-origin already, so
  datasets (by range), Import, downloads, Share links, fonts and the Graph
  view work as before.
- **Web shell: `GROUP BY` draws a graph as its groups, and a group opens in
  place** (#499). `.graph karate GROUP BY club` draws a disc for each club
  and the ties between the clubs, counted; `.graph rmat GROUP BY louvain`
  draws a million edges as their communities, within a quarter of a second
  of the algorithm's result. What groups the nodes is a column of `NODES
  (query)`, of the result's rows or of the graph's vertex tables, `degree`,
  or an algorithm, found as `COLOR BY` finds it. A disc's area is its share
  of the nodes, an edge's width its count, and each says its number; more
  than 2,000 groups are the 2,000 largest and one "other". A double-click on
  a disc (or `.expand group:N`) adds the group's members and the edges among
  the members of the open groups (`induced_edges`), laid out inside the
  disc; `.collapse group:N` closes it, and `.undo` takes the members away.
  After a query, `GROUP BY` is the clause when the query is in parentheses.
  `ursa_engine::graph::view` has a `groups` module, `Style::group`,
  `GraphView::groups` and `Session::grouped`; `ursa-viz`'s level scene draws
  open discs and points (`LevelScene::set_kinds`), and a view's fit can
  leave room for discs (`View::set_fit_padding`).
- **Web shell: a graph is drawn from saved positions, and the edge cap is a
  setting, 5,000,000 by default** (#502). `.graph … POSITIONS (SELECT id, x,
  y …)` draws each node where the query puts it and runs no layout, so a
  graph too large to lay out while you wait is laid out once: `CREATE TABLE
  pos AS SELECT * FROM layout_fa2(g)` (or, in the CLI, `COPY (SELECT * FROM
  layout_fa2(g)) TO 'pos.parquet'`), then `.graph g POSITIONS (SELECT * FROM
  pos)`. The ids are matched to the drawing's keys. `layout_fa2`'s rows are
  where the view's own layout leaves each node, to the bit (centred on the
  origin), so the picture is the same; on the 1M-edge R-MAT graph its first
  frame comes 0.35 to 0.47 s after the engine's answer, where the live
  layout ends after 5 to 6 s. A node the query has no place for is placed
  beside its neighbours by one warm pass, the others held, and the status
  line says how many were. The most edges a view draws is now
  `ursa.graph_max_edges`: 5,000,000 by default (it was a fixed 2,000,000),
  500,000 where the page finds a phone (a coarse pointer and at most 4 GB of
  memory), `0` for no limit; a refusal names the setting and the `SET` that
  lets the result through. Above 2,000,000 edges without `POSITIONS`, the
  status line and the terminal say how to save the positions.
  `ursa_engine::graph::view::GraphView` has a `positions` field, and
  `Style` a `positions` clause.

- **Graph view: a large graph is drawn aggregated, and opens as the view
  zooms in** (#498). Zoomed out, a million edges are a blot. Once a graph
  of 200,000 nodes and edges or more is laid out, the view draws a cut
  through its layout's hierarchy in its place: a disc for each group of
  nodes, the coarsest groups whose discs stand 20 pixels apart on screen,
  with the merged edges between them, the largest groups labelled with how
  many nodes they hold. The status line says so first: `aggregated: 92
  groups of 46,318 nodes · 1,000,000 edges · …`. The layout worker makes
  the cut again each time the camera comes to rest, opening only the groups
  in view; meanwhile the last cut is drawn, moved as the camera is. Zooming
  in reaches the graph's own nodes, and from there the drawing is the
  graph's own, byte for byte. A click on a group selects its nodes (Shift
  adds another group's), and a disc with a selected node in it carries a
  ring, so `.find` shows where its matches are. `a` in the pane, or `.graph
  detail on`, draws every node and edge as before; `.graph detail off`
  aggregates again. On `.graph rmat` (headless Chromium with no GPU): 92
  discs and 2,389 merged edges at the fitted zoom; a cut takes the layout
  worker 10 to 12 ms at the median and 26 at most in two runs of three
  (35 in the third; 30 are allowed), after 44 to 48 ms once to prepare;
  the worker's memory is unchanged at 55.6 MB. Layouts are bit-identical.
  - `ursa-core`: `Fa2Multilevel::level_members`; a layout keeps each
    level's members when it comes down from it (under 1 MB at a million
    edges).
  - `ursa-viz`: `cut` (`Hierarchy`, `Cut`, `CutParams`);
    `LiveLayout::cut`, `prepare_cut`, `has_cut` and `cut_bytes`;
    `LevelScene::discs_by`, `pick` and `set_max_radius`; `View::set_cut`,
    `draws_cut`, `pick_group`, `group_nodes`, `group_of`, `group_labels`
    and `cut_info`. In the browser: `Layout.cut`, `prepareCut`, `hasCut`
    and `cutBytes`; `GraphView.setCut`, `cutInfo`, `pickGroup`,
    `groupNodes`, `groupOf`, `selectGroup`, `groupLabels` and `camera`.
    The edges of a selection of more than 10,000 nodes are not drawn wide.
- **Graph view: a large graph is first seen as the coarsest level of its
  layout, and refines level by level** (#497). The coarse-to-fine layout
  works down a pyramid of the graph; until it reaches the graph itself, the
  view now draws the level being laid out: a disc for each of its nodes,
  with area by the mass of the nodes it stands for, and its heaviest merged
  edges, as wide as the number of edges each stands for. Each new level's
  nodes move out from their parents over 150 ms (at once under
  `prefers-reduced-motion`), and the status reads `… · Laying out · level 3
  of 11 · 110 groups`. The graph's own nodes and edges are first drawn when
  the layout reaches them. On `.graph rmat` (a million edges, headless
  Chromium with no GPU) the first frame is shown 0.94 s after Enter where
  it took 1.13 to 1.23 s, and shows the drawing's shape; the levels' frames
  come 33 ms apart at the 95th percentile. The layout ends on the same
  positions, bit for bit. Graphs under 200,000 nodes and edges are drawn as
  before.
  - `ursa-core`: `Fa2Multilevel::step_level`, `level_positions`,
    `level_mass`, `parents_below`, `level_edges` (`LevelEdges`),
    `level_finished` and `prolong`. Building a hierarchy gathers each
    coarse row without sorting its edges (the same sums, in the same
    order): 225 ms where it took 336 ms for the 1M-edge R-MAT graph on one
    core.
  - `ursa-viz`: `LiveLayout::step_level`, `level_graph` and
    `level_positions`; `levels` (`LevelGraph`, `LevelScene`) and
    `level_render`; `View::set_level`, `set_level_positions`,
    `clear_level`, `level_info` and `warm_levels`. In the browser:
    `Layout.stepLevel`, `levelGraph`, `levelPositions`, `levelFinished`
    and `work`; `GraphView.setLevel`, `setLevelPositions`, `clearLevel`
    and `levelInfo`.
- **The graph view at rest is drawn in slices, and kept: nothing waits for
  a million edges** (#496). Where everything does not fit in a frame, the
  frame at rest is no longer drawn at once (with no GPU, 2.5 to 6 s after
  the layout, after every pan and zoom, and for every click and hover on
  the view at rest, during which a zoom waited). It is drawn offscreen,
  into one texture the size of the canvas: the edges a run of the buffer
  at a time, then the nodes, each slice sized to take the GPU 16 ms and
  sent when the GPU has finished the last but one. A pan or a zoom stops
  the slices. The frame that follows them is the frame drawn at once, byte
  for byte (read back on WebGL2: fitted, zoomed in, themed, highlighted,
  after deltas); every frame at rest after it is that texture under the
  highlighted edges and the nodes in front, so a hover or a click draws no
  edge; and the frames of the next pan or zoom draw it under their own
  edges and nodes, moved and scaled with the camera, so the full picture
  stays on screen while it moves. The Graph pane's status line says
  `refining…` while slices remain. `ursa-viz` gains `refine` (the slices'
  pace, and what a change of flags asks of the texture);
  `View::plan_frame`, `draw_planned`, `refine`, `refining`,
  `snapshot_refined` and `set_refining`; `Renderer::draw_slice` and
  `draw_over`; and in the wasm API `refine`, `refining`, `setRefining`,
  `snapshotRefined` and `frameInfo().refining`, `refined` and
  `refinement`. On `.graph rmat` (1M edges) in headless Chromium's
  software WebGL2, the slices take 2.2 to 2.9 s over three runs (what
  drawing it at once takes there, to a tenth more; up to 4.6 s on a busier
  machine) with no task of the page over 50 ms, and a click at rest is
  shown 90 to 140 ms after it (2.2 to 2.9 s). Not measured on a GPU, where the target is 0.3 s
  (`ursa-viz/BENCH.md`).
- **A frame budget for the graph view: pan and zoom no longer wait for the
  graph's size** (#495). While the camera or the positions change, a frame
  draws a prefix of the edges and, where the nodes are too many as well,
  the largest of them: as much as the last frames' costs say fits in a
  frame (12 ms, or 1.3 of the display's refresh periods). At rest, 200 ms
  after the last movement, it draws everything, the same pixels as before.
  The edges are in importance order, so a prefix is a backbone of the
  drawing: by the level of the layout's hierarchy at which an edge's two
  ends merge, coarsest first, then one edge of each merged edge a round, the
  heaviest first, then a hash (`Fa2Multilevel::edge_order` in `ursa-core`;
  by the larger degree of the two ends where there is no hierarchy:
  `edge_order_by_degree`). The layout worker computes the order once
  (0.3 s at 1M edges) and the view writes it to the GPU a piece at a time.
  `ursa-viz` gains `budget` (the controller: it searches for the largest
  budget that fits, since a browser's frame cost is whole refresh periods)
  and `order`; `View::draw_frame`, `set_edge_order` and `frame_info`; and
  in the wasm API `renderFrame`, `frameCost`, `framePartial`, `keepMoving`,
  `setEdgeOrder`, `orderEdges`, `frameInfo` (also `info().frame`) and
  `Layout.edgeOrder`. On `.graph rmat` (1M edges) in headless Chromium's
  software WebGL2, frames of a scripted wheel zoom are 26 to 31 ms apart at
  the 95th percentile over three runs (3.3 s before; 25 to 46 ms on a
  busier machine), the first frame is shown 1.3 s after Enter (6.0 s) and
  the layout is done at 5.6 s (10.6 s),
  the page no longer rasterising a million edges for each set of positions;
  that renderer fits 512 to 1,081 edges and 170 to 360 nodes in a frame,
  and a frame at rest still takes it seconds. Not measured on a GPU
  (`ursa-viz/BENCH.md`).
- **Ursa Web Shell: the Graph view times itself** (#430).
  `window.ursaShell.graphTimings()` gains `frames` (the count, and the p50,
  p95 and max of the time in `render()` and of the time between frames
  while the view moves, over the last 120), `pick` (the last and the p95),
  `actions` (for each: the command, the ms in the engine, the ms applying
  its answer, and the ms from the input to the frame that showed it) and
  `longTasks` (the page's tasks over 50 ms). `graphMemory()` gives each
  wasm instance's memory (engine, view, layout worker) and
  `performance.measureUserAgentSpecificMemory()`. A frame's end is when the
  page's thread is free of it, not when `render()` returns: with no GPU the
  browser rasterises on that thread afterwards. Keeping them costs a frame
  three clock reads, a timer and two writes into arrays made once.
  `web/tests/graph-timing.ts` prints them, with the frame times of a
  scripted wheel zoom on `.graph rmat`.
- **Ursa Web Shell: the Graph view in each theme's own colours.** The view
  draws on the theme's background with its own edge ink and a categorical
  palette stepped for white and for the dark sky, so the CSS inversion is
  gone; switching the theme recolours an open view. `ursa-viz` has
  `setTheme({background, edge, node, palette})` and a `Theme` in Rust, and
  its `Layout` a `levels` getter.
- **Ursa Web Shell: the Graph view says how far it is, and draws sooner**
  (#390). The pane shows "Loading graph view…" within a moment of Enter,
  then "Laying out · level k of n", then the counts and "Done". `ursa-viz`
  and the layout worker start loading while the engine runs the query, the
  layout gets the graph before the view is built, and nothing is drawn
  before its first positions: `.graph rmat` (1M edges) first draws 0.95 s
  after Enter in headless Chromium, from 4.6 s, and finishes its layout at
  6.0 s, from 12.1 s.
- **`ursa`: shell fixes.**
  - A null in a column of type `Null` (`SELECT NULL AS n`) shows `NULL`, as
    other nulls do; it was a blank cell. So does a dictionary's null value.
  - A table too wide for the screen is narrowed only as much as it needs:
    its widest values are cut first, and no shorter than the column's name,
    before any column is left out. Before, every long value was cut to 20
    characters even when two would do. When columns are left out, a column
    too wide to fit no longer stops the others: the columns after it on the
    other side are still shown.
  - The footer stays as wide as the table: the column count goes on a line
    of its own when the two counts do not fit on one, and is left out when
    nothing was left out. The space between the counts is at least two.
  - `--max-width N` sets the table width from the command line, as
    `.maxwidth N` does, and wins over an init file's `.maxwidth`.
  - Values are drawn as they are: bidirectional overrides, zero-width
    spaces, line and paragraph separators and byte order marks are written
    as escapes (`\u{202e}`), as control characters were, and so are a
    column name's. Zero-width joiners are not: emoji sequences and the
    scripts that use them stay as they are.
  - A dot-command may end with `;` (`.tables;`, `.mode csv;`), as
    `.describe` could. The start of a command (`.q`, `.tab`) gets a
    suggestion, `did you mean '.quit'?`, and is not run.
  - `.describe NAME` finds a table or view in any case, as it found graphs
    (`.describe Mixed` for a table created as `"Mixed"`), and reads NAME as
    a name: `.describe v; SELECT 99` is an error instead of two statements.
  - What is typed is coloured as part of the statement typed so far, in the
    terminal and the browser: a string or comment left open on one line
    goes on in the next, and a next line starting `.5` is SQL, not a
    command. `E'…'` strings, `$$…$$` and `$tag$…$tag$` strings and nested
    `/* /* */ */` comments are coloured as they are read.
  - Ctrl-C during a `.read` no longer counts it as still running; after 16,
    every `.read` failed as nested too deep.
  - `--format arrow` with no query that returns a result is an error, not an
    empty output that exits 0, and `.mode arrow` says to give
    `--format arrow` on the command line, in the shell and in a script.
- **`GRAPH_TABLE` through the graph index** (#325). The results are the
  same; what changes is how a match runs.
  - A node is *bound* when a condition on it alone names its keys:
    `a.id = 7`, `a.id IN (1, 2)` or `a.id IN (SELECT …)`. From a bound node,
    the fixed-length edges of its path are followed on the graph index, the
    one `hop` uses, each from the nodes the one before reached, rather than
    joined to the whole edge table: `MATCH (a WHERE a.id = 7)-[]->(b)-[]->(c)`
    reads only the edges at the nodes it reaches. On a 20M-edge R-MAT graph
    with a stored index, one edge from a bound node takes about as long as
    `hop` (see the PR for the numbers).
  - A node the query reads only the key of is not joined to its vertex
    table: its key comes from the index. A vertex table is scanned only for
    the properties the query reads.
  - A path search whose last node is bound (`(b WHERE b.id = 9)`) stops once
    it has that node's paths: `ANY SHORTEST` from one node to another takes
    about as long as `shortest_path`.
  - `EXPLAIN` shows each edge followed on the index as a `PathSearchExec`
    of one edge (`-[]->{1,1}`), and `, targets` on a search that stops at
    its last node; `EXPLAIN ANALYZE` counts the path prefixes each search
    extended (`expanded`).
  - Edges are still joined when no node is bound, when the query reads an
    edge's columns, and when the edge table or its vertex tables are not
    files or views over files (an in-memory table's index is not kept).
  - A missing property is named in the pattern's terms: "node 'a' has no
    property 's'; its properties are club, id", with a hint to name an edge
    to read its columns, where DataFusion named an internal relation
    (`__ursa_1.s`).
- **A new website for Ursa SQL, in `site/`**, to be served at
  `https://ursa.cldixon.dev`: a landing page at `/` and the docs at `/docs/`,
  built with Astro and Starlight and laid out after duckdb.org's docs, ready
  to deploy as Workers static assets (worker `ursa-site`; not deployed yet).
  The docs are the eight `docs/sql/` pages, regrouped under Get started and
  Topics. `docs/sql/` stays, served by the Zensical site, until the cutover.
  - `ursa-cli`'s docs test runs the SQL on the new pages too
    (`site_docs_run`, with `{/* skip */}` for MDX), and
    `function_reference_is_current` checks the site's copy of the function
    reference as well. CI builds the site.
  - Zensical builds into `zensical-site/` instead of `site/`, which is now
    the new website's source.
- **Coarse-to-fine ForceAtlas2, and the graph view lays out with it.**
  `ursa_core::algo::layout_fa2_multilevel` and its incremental form
  `Fa2Multilevel` coarsen the graph (heavy-edge matching, with a hub's
  leftover leaves joining it, and isolated nodes set aside on a ring), lay
  out its 50-node top level, then refine each finer level in a few
  iterations. The output is the same bits on any thread count, and however
  the work is split into steps. On one core a 10,000-node grid comes out as
  a flat sheet in 2 s, where flat ForceAtlas2 leaves it folded after 1,500
  iterations; a 16K-node R-MAT graph is as good in 3.5 s as flat after 17 s;
  clustered graphs separate as well as flat. The `ursa-viz` worker uses it by
  default (`&multilevel=0` in the demo for flat), drawing every node from the
  first frame: a 1M-edge R-MAT graph takes shape in about 8 s in the browser,
  where flat needs minutes. `Fa2State` gains `step_weighted` (an attraction
  weight per edge) and `from_positions` (start positions and masses);
  `layout_fa2` and `layout_fr` give the same bits as before.
- **ForceAtlas2 with fixed nodes and a warm start, for a layout that
  grows** (#442). `Fa2State` gains `set_fixed` (a per-node mask: a fixed
  node is not displaced by a bit, and still repels and attracts),
  `set_inertia` (a per-node factor its displacement is divided by),
  `extend` (append nodes at given positions) and `set_mass`.
  `ursa_core::algo::fa2_grow` puts them together for a view that expands a
  node: the new nodes start on a golden-angle spiral around the node they
  came from (`place_new_nodes`), pinned nodes and the expanded node are
  fixed, the old nodes within two hops of a new one get ten times the
  inertia and the rest are fixed, and 60 iterations settle it
  (`warm_iterations`; `WarmParams` has the knobs). Fixed nodes have a
  quadtree of their own, built once (`QuadTree::accumulate_at`), so an
  iteration costs what the free nodes do: 100 new neighbours in a laid-out
  1M-edge graph take 2 ms to place and 10 ms to settle on one thread, after
  52 ms rebuilding the adjacency (`fa2_multilevel`'s new `grow` mode;
  `ursa-viz/BENCH.md`). In `ursa-viz`, `LiveLayout::grow` and
  `LiveLayout::from_positions`, and `Layout.grow` and `Layout.fromPositions`
  in the wasm API; expanding a node to degree 16 in a 500-node view moves
  the old nodes 0.003% of the view's width on average. The same additions
  in the same order give the same bits on any thread count. A layout that
  uses none of this has the bits it had (`tests/layout_golden.rs`).
- **Rings and layers: fixed layouts for neighbourhoods and paths** (#436).
  `ursa_core::algo::layout_rings` draws a `hop` result from each node's
  distance: the seed at the centre, distance `d` on the circle of radius
  `d`, a crowded ring widened so that no two nodes are nearer than a
  quarter of a unit, and several seeds as one ring system each, side by
  side. `layout_layers` draws paths from each node's position along them:
  layer `l` on the line at `x = l`, one path alone a straight line. The
  order on a ring or a line is chosen by four barycentre sweeps (ties by
  node index), and each node is then placed as near its neighbours' mean as
  its spacing allows. Neither iterates: 2 ms for 2,048 nodes and 8,334
  edges, 0.3 s at 1M edges, on one thread (`ursa-viz/BENCH.md`). Both are
  serial, so the same input gives the same bits on any thread count;
  layers are the same bits on every platform. In `ursa-viz`,
  `LiveLayout::rings` and `LiveLayout::layers` (done at once, and they
  grow like any layout), `Layout.rings`, `Layout.layers` and `Layout.kind`
  in the wasm API, and the shell's layout worker takes
  `layout: {kind, level}` and posts one message. On karate, two hops from
  node 0 are two rings of 16 and 9 nodes, and all shortest paths from 16 to
  26 are six layers, drawn without a crossing. No existing layout changes.
- **Layouts in SQL: `layout_fa2` and `layout_fr`** (#395). Both are table
  functions of `(id, x, y)`, like the algorithms, and work inside
  `algorithms(g, …)`, where their columns are `x` and `y`, or
  `<name>_x` and `<name>_y` (`pos => layout_fa2()`). `layout_fa2` is
  ForceAtlas2 coarse to fine by default (`layout_fa2_multilevel`);
  `multilevel => false` runs it flat. Its named arguments are
  `iterations` (NULL: 300 flat, the size-based schedule coarse to fine;
  given, coarse to fine, the graph's own iterations, the levels above more),
  `k`, `gravity`, `strong_gravity`, `lin_log`, `theta` (NULL: 1.0 coarse to
  fine, 0.5 flat), `jitter_tolerance` and `seed`; `layout_fr` takes
  `iterations`, `k`, `gravity` and `seed`. Positions are `ursa-core`'s, bit
  for bit, and the same on any thread count, so they can be saved with
  `COPY … TO 'layout.parquet'`. The layouts take no weight. A graph with a
  vertex table gives every vertex a position. `layout_random` and
  `layout_circle` stay hidden. The catalog gains `layout_fa2` and an
  `OptionalCount` parameter type; the Python library has
  `ursa._graph.layout_fa2`, not yet exported, as the other layouts.
- **A graph-result model and a wire format for it, in a GPU-free
  `ursa-viz`** (#424). `ursa_viz::model` is the data every drawing will be
  made of: `GraphResult` (nodes identified by a label and a typed key, so two
  vertex tables may reuse a key; edges with a label and their row; `f32`,
  `u32` and dictionary columns for both; paths, seeds and distances;
  encodings; the SQL and the rule that drew it) and `GraphDelta` (add,
  remove and restore nodes and edges, set columns, highlights, encodings and
  positions). Indexes are append-only and a removed node keeps its slot;
  every op carries its inverse, so `apply(&delta)` then
  `apply(&delta.invert())` restores a result bit for bit, with no query.
  `to_bytes` and `from_bytes` speak UGR (`ursa-viz/UGR.md`): a JSON header
  and aligned little-endian arrays, which `ursa-viz/ui/ugr.js` reads as
  typed arrays over the buffer without a copy. A million edges are 8.5 MB.
  wgpu is now the crate's `render` feature, on by default: with
  `default-features = false` it builds with no GPU dependency, for the
  engine to produce results with. The view and the shell draw as before.
- **Search in the Graph view: `/`, `.find` and `f`** (#445). `.find TEXT`
  selects the nodes of the drawn graph whose key starts with the text, then
  those whose label column (the vertex table's first text column) contains
  it; `.find WHERE condition` selects by any SQL condition on the vertex
  table's columns (`.find WHERE club = 'Officer'`). The view selects the
  matches and moves the camera to fit them (200 ms, or at once under
  `prefers-reduced-motion`). At most 1,000 are selected, and the status says
  how many matched. Matches that are not drawn are named in the terminal and
  listed under the pane's header; a hidden one has a button that shows it
  again. In the pane, `/` opens a search box that runs `.find`, and `f` fits
  the selection. `--sql` prints the search's SQL. The engine's side is
  `ursa_engine::graph::view::find`, run by `Shell::find` and
  `WebShell.find(command)`.

- **Web Shell: double-click a node and its neighbours appear** (#443). The
  Graph view now draws what the explore commands change. A double-click on a
  node (a double tap, or Enter in the pane with nodes selected) runs
  `.expand ID` in the terminal as if typed: it is echoed after a dimmed
  `from the graph`, answered in one line, kept in `.history`, and replayed
  by a shared link, which now includes `.graph` drawings and the explore
  commands that changed them. Shift expands with `LIMIT 5000`; a
  double-click on empty space still fits; `.graph echo off` runs these
  commands without writing them. The new nodes start beside their
  neighbours and the layout grows warm, the nodes already drawn held or
  slow. `.hide` and `.show` are drawn too, `.undo` puts every node back
  where it was before the step, in one frame, and `.redo` draws the step
  where it had settled. While a command waits its turn its node carries a
  busy ring. A node with edges in the graph that lead to something not
  shown says `+N`, in its tooltip and in the header when it is selected (no
  badge is drawn: there is no label overlay yet); the count is the engine's
  (`Session::not_shown`, `Shell::not_shown`, `WebShell.notShown()`). Each
  expansion is an action in `graphTimings()`. On the million-edge R-MAT
  dataset, from a `hop` result, a double-click's frame comes 118 to 190 ms
  after it in headless Chromium. The pane's view is now made with
  `createViewFromResult`, so that deltas apply to it. Such a view also
  draws the result's own highlights: a path's route is highlighted, with
  the rest dimmed, and a `hop`'s seeds are ringed (a seed alone dims
  nothing, so a neighbourhood and what is expanded from it stay at full
  strength). It costs more for a whole
  graph: at 1M edges the view's heap is 60 MiB (it was 16) and decoding the
  result is one 63 to 92 ms task on the page's thread.
- **Exploring a drawn graph from the terminal: `.expand`, `.hide`, `.show`,
  `.undo`, `.redo` and `.graph history`** (#440). The engine now keeps an
  explore session for the graph `.graph` opened
  (`ursa_engine::graph::view::Session`): the graph as it stands, with its
  `(label, key)` → node map and what is shown, and the history of steps.
  Each command is a line in the Web Shell's terminal, answers in one line
  (`+16 nodes, +41 edges`, or why nothing changed) and gives the change as a
  UGR delta for a view to apply. `.expand ID [in|out|both] [LIMIT n]` adds a
  node's neighbours that are not shown (`hop`), the 100 of highest degree
  first and then by key, so the same command always adds the same nodes, and
  the graph's edges among every node (`induced_edges`); expanding again adds
  the next ones. `.hide ID…` and `.show ID…|all` hide nodes and bring them
  back with their edges; `.undo` applies the inverse of the last step with
  no query, and `.redo` the step again. `--sql` prints a command's SQL
  instead of changing anything, and `.graph history` lists the steps. With
  no open graph, or an id that is not a node of it, the command says so.
  `ursa-shell` lists the commands in `.help` and runs them
  (`Shell::open_graph`, `Shell::explore`); `ursa-wasm` adds
  `WebShell.explore(command)` and the engine worker an `explore` op. The
  page's pane draws the deltas since #443.
- **`.graph … AS edges(…) | nodes(…) | path(…) | hops(…)`, `ON graph` and
  `LAYOUT`** (#434). The Web Shell's `.graph` takes three more clauses, in
  any order after its source, beside `NODES`, `SIZE BY` and `COLOR BY`.
  **`AS …`** says what a result's rows are and names the columns, in place
  of the rules (it is rule 0, and nothing is inferred), so a result whose
  columns have any names is drawn: `.graph SELECT a.id AS x, b.id AS y FROM
  … AS edges(x, y)`. The forms are `AS edges(src, dst [, weight])`, `AS
  nodes(id)`, `AS path(nodes [, edges])` and `AS hops(seed, id, distance)`;
  the refusal of a result that is no graph already named them. **`ON
  graph`** names the property graph the ids belong to: where `AS nodes` and
  `AS hops` read their edges, and what `SIZE BY pagerank` runs on for rows
  drawn as edges. **`LAYOUT force|rings|layers`** sets the layout, whatever
  the rule chose. A column that is missing or of the wrong type is an error
  that names it, lists the result's columns and their types, and says what
  the form needs; every other misuse (a form that is not one, the wrong
  number of columns, a clause given twice, a graph that is not there, rings
  for a drawing with no distances, a clause on a property graph) says what
  to write. `.help graph` lists the clauses. `ursa-engine`'s `graph::view`
  gains `As`, `Style::{shape, on, layout}`, `interpret_with` and `USAGE`.
- **`.graph` draws what a result means** (#433). After a graph query, the
  Web Shell's `.graph` (`ursa-engine`'s `graph::view`) now reads what each
  column is, from the `ursa.*` metadata the graph functions and `GRAPH_TABLE`
  write and else from its name and type, and draws the result by the first
  rule that matches: **paths** (`vertices(p)`, with `edges(p)` or
  `element_id(p)`, the steps of `shortest_path()`, or a list of ids) as the
  union of the rows' paths; a **`hop` result** (`seed`, `id`, `distance`) as
  its seeds and the nodes reached, with the graph's real edges among them
  and coloured by distance, where it drew a star; **edges** as before
  (`src`/`dst`, now also `source`/`target` and `from`/`to`, two pattern
  variables, or the first two id columns); **pattern matches** of three or
  more node variables as every edge of the pattern, where it drew only the
  first two columns; and **nodes** (`pagerank`, `algorithms`, a table with
  an `id`) with the graph's edges among them (`induced_edges`), sized by
  their first metric and coloured by their first category, where it refused
  or drew each id tied to its score. What is no graph says what its columns
  are and what would draw them. The pane's status line and the terminal say
  what the result was drawn as (`drawn as a path on karate (members,
  ties)`). A result's provenance (UGR) gains `roles` and `notes`, and its
  highlights carry the paths, the seeds and each node's distance or step,
  with the layout the rule asks for. A plain edge table is drawn as it was:
  the same nodes and edges, in the same order.
- **`.graph` is resolved in the engine** (#425). What turns a source and its
  `NODES`, `SIZE BY` and `COLOR BY` clauses into a graph moved from
  `ursa-wasm` to `ursa-engine` (`graph::view`), so the CLI and the notebook
  can draw what the web shell draws. It writes a `ursa_viz::model::GraphResult`:
  the ids as nodes of one label with typed keys, the edges, a node column
  for `SIZE BY` and a dictionary column for `COLOR BY` (every category, the
  largest first) which the encodings name, and the SQL and the rule that drew
  it (`edges`, or `graph` for a property graph). `GraphView::legend` gives
  the legend. `ursa-engine` depends on `ursa-viz` without its default
  features, so no GPU code is in the engine or the CLI; `ursa-viz` is now
  published with the engine's crates. SQL text is run through a `RunSql`
  the caller hands in (`ursa_sql::Sql`), since `ursa-sql` depends on the
  engine. In the web shell, `WebShell.graph` returns one UGR buffer, which
  the worker transfers and `graph.js` reads with `ursa-viz/ui/ugr.js` as
  typed arrays over it, in place of an array copied per getter. What
  `.graph` draws, its messages and its legend are unchanged.
- **An inspector in the graph view, and edge picking** (#444). Selecting a
  node in the web shell's Graph view opens a panel with its key, its degree
  as drawn and its neighbours in the whole graph, the values drawn, and its
  row of the vertex table, read with one query by key. A click within 4 px
  of an edge selects the edge, and the panel shows its ends and its row of
  the edge table. Several selected nodes show their count, the edges among
  them and a `WHERE id IN (…)` clause to copy. Buttons run `.expand`,
  `.hide`, a shortest path, and `.graph` with `COLOR BY` or `SIZE BY` a
  property, in the terminal; `i` moves the focus to the panel and Esc back.
  New: `ursa_engine::graph::view::inspect`, `Shell::inspect`,
  `WebShell.inspectNode` and `inspectEdge`, and in `ursa-viz`
  `interact::nearest_edge`, `pickEdge`, `selectEdge` and `selectedEdge`.
- **A spatial grid in the graph view: a pick reads the nodes near the
  pointer, and a zoomed-in frame draws the nodes in view** (#500). With the
  positions at rest, `ursa-viz` keeps a uniform grid over them and a pick
  or a hover tests the nodes in the cells around the pointer: 6 to 22 µs at
  a million nodes, where testing every node took 3.6 to 4.9 ms on every
  pointer move; the grid is built in 10 to 12 ms. An edge pick reads a grid
  of the edges by length. Zoomed in so that under half the nodes (and at
  most 65,536) are in view, the node pass draws those alone: under 3% of
  the instances at 1% of an R-MAT drawing. Every pick is the pick that
  tests everything, ties included, and the culled frame is the whole frame
  byte for byte; while a layout moves the nodes, picks test every node and
  frames draw every node, as before. A frame and a pick no longer add up
  every node's area to size the nodes. New in `ursa-viz`: the `spatial`
  module (`NodeGrid`, `EdgeGrid`, `Discs`), `View::prepare`,
  `View::pick_edge`, and in the wasm API `gridReady`, `rebuildGrid`,
  `nodesDrawn`, `setCulling`, `pickLinear` and `pickEdgeLinear`.
  `View::pick` takes `&mut self`.
- **Hover and selection in the graph view** (#438). In the web shell's
  `.graph` pane, a mouse or a pen over a node rings it and shows a tooltip
  (its key, label, `SIZE BY` and `COLOR BY` values and degree), looked up
  once a frame at most. A click selects a node, Shift+click adds one (or
  takes it out), and a click on empty space clears the selection; the
  selected nodes carry a ring and their edges are drawn wide. The pane's
  header, a live region, reads `Selected node 33: …` in place of the node a
  click named. Esc clears a selection before it closes the pane. The
  selection set is `ursa-viz`'s new `interact` module (no GPU), which the
  wasm view exposes as `select`, `selection`, `clearSelection`, `hover`,
  `hovered` and `selectionEdges`.
- **A highlight layer in the graph view** (#435): a path, a selection or a
  hover can be seen. `ursa-viz` keeps a flags byte per node (selected,
  hovered, pinned, dimmed, highlighted, hidden, seed) in a buffer of its
  own, and draws from it: a halo on highlighted nodes, a ring on a route's
  ends and on seeds, rings for the selection, the hover and pins, and
  nothing for a hidden node or its edges. Highlighted edges are instanced
  quads, 3 px wide with round ends, over the line list. While a highlight is
  set, everything else draws at 35%. `GraphView` gains `setHighlights`
  (UGR's path and seed sections), `clearHighlights`, `setSelection`,
  `setHovered`, `setPinned`, `setHidden`, `setFlags`, `flags`,
  `setHighlightEdges`, `nodePixel` and the `NodeFlag` bits; in Rust,
  `ursa_viz::highlight::HighlightState` (no GPU) and
  `View::set_result_highlights`, which reads a `GraphResult`'s highlights
  and removed nodes. `Theme` and `setTheme` gain `highlight`, `selection`
  and `pin`, at least 3:1 against the background in both built-in themes,
  and the shell's stylesheet the matching `--graph-highlight`,
  `--graph-selection` and `--graph-pin` tokens. It is all vertex buffers, so
  it draws on WebGL2 as on WebGPU. Setting a highlight uploads the flags
  that changed and 16 to 20 bytes for each highlighted edge and node, never
  the graph's edges; a selection of one node uploads 24 bytes. At 1M edges
  a frame with a path highlighted costs within 3% of one without
  (`ursa-viz/BENCH.md`). The shell does not drive the layer yet. `View`'s
  methods that upload (`step`, `set_positions`, `set_style`, `color_by`,
  `set_theme`) now take the device as well as the queue.
- **Making the graph view costs an upload, not a graph build** (#426). The
  page's view no longer builds a topology, an undirected adjacency and a
  layout state to read degrees and starting positions: degrees are counted
  from the edge list, the nodes start where a layout would seed them
  (`ursa_core::algo::fa2_seed_positions`), the edges go to the GPU a run at
  a time, and `createView` and `createViewFromResult` give the event loop a
  turn between compiling the shader, linking each pipeline and the uploads.
  At 46K nodes and 1M edges in headless Chromium, `createView` was one task
  of 190 to 350 ms on the page's thread and 50 MB of wasm memory; it is now
  no task over 50 ms and 16 MiB. `View::step` builds the adjacency on its
  first call. In Rust, `View::new` takes the queue as well as the device,
  `View::new_paced` and `View::from_result_paced` are what a page calls,
  `Renderer::new` takes the edge count and the caller writes the edges
  (`write_edges`), and `Graph::line_indices` takes a range.
  `tests/graph-timing.ts` prints the longest task while the view is made
  and fails over 60 ms. The shell keeps what its layout worker says while
  the view is being made, so a small graph's layout that ends first is not
  lost.
- **The graph view holds a million edges in 74 MB, not 164** (#501, with
  #492). On `.graph rmat` in the Web Shell (46,318 nodes, 1,000,000 edges)
  the page's view holds 17.9 MiB of WebAssembly memory where it held 59.6,
  and the layout worker 55.7 MB where it held 101; no task of the page's is
  over 60 ms while the view is made (decoding the result was one of 63 to
  92 ms); and a position update is copied twice on its way from the layout
  to the GPU, not six times. Layouts are the same, bit for bit.
  - **The page's view.** `createViewFromResult(canvas, bytes)` takes the
    `Uint8Array` and reads the result out of it a section at a time, with
    turns of the event loop between (`GraphResult::read_paced` over a
    `model::Source`, `View::from_ugr_paced`), so the buffer is never copied
    whole into the view's memory. A view made from a result keeps no second
    copy of the edges (`View::edges` reads the result's; `View::graph` is
    gone, for `n_nodes`, `n_edges` and `edges`). Edge labels and flags that
    are all zero hold nothing (`model::Sparse`, which `edge_label()` and
    `edge_flags()` now return); the key index is a table of node indexes
    over the result's own keys; `GraphResult::reserve` no longer doubles
    what it was asked to grow by a sixty-fourth; a paced view waits for the
    GPU to take each run of edges, so WebGL2's staging is freed as it goes;
    and `resultBytes()` writes the result into a `Uint8Array` a section at
    a time (`GraphResult::encoded`).
  - **The layout worker.** A layout reads a graph's undirected adjacency
    and nothing else, so that is all that is built
    (`UndirectedCsr::from_edges`, the same adjacency as
    `Topology::undirected` without the topology), and `Graph::topology` is
    `Graph::adjacency`. `Fa2Multilevel::new_lean`, which `LiveLayout` uses,
    holds the weights of a coarse level that grouping does not thin only
    while that level is laid out, and every level is released once the
    layout is past it: same positions at every step, for one more
    contraction for each such level between it and the graph (six at 1M
    edges on R-MAT, none for a graph whose levels shrink).
  - **An adjacency can travel with a result.** UGR has optional
    `edges.adjacency.offsets` and `.targets` sections
    (`GraphResult::to_bytes_with_adjacency`, `ugr::read_adjacency`,
    `ui/ugr.js`'s `edges.adjacency`), which `Layout.fromAdjacency` and
    `LiveLayout`'s `_on` constructors use as they are, checked in one pass
    (`UndirectedCsr::from_parts`) and with nothing sorted; the page hands
    one to its worker when a result has it. The engine does not write them
    yet: `.graph` numbers a drawing's nodes as it reads them, not as the
    graph's index does, so it has no adjacency in the drawing's numbering
    to give.
  - **Positions.** `Layout.positions()` and `GraphView.positions()` return
    a `Float32Array` filled straight from the layout's or the view's
    memory, and `setPositions` takes one and copies it once, into the
    positions the view keeps and uploads.
  - **The edge order of #495, within the same memory.** `Layout.edgeOrder`
    reads `src` and `dst` from the caller's `Uint32Array`s a run at a time,
    keeps the sort keys in a `BigUint64Array` outside its WebAssembly
    memory and sorts them there (`Fa2Multilevel::edge_order_keys` over an
    `OrderKeys`), so the worker's memory does not grow by the order (it
    grew by 11 MiB). On a lean hierarchy the weights it ranks by are
    counted again from the adjacency, two bytes an entry, and the order is
    the same; `edge_order` takes the input adjacency for that, and after
    the layout has come down a level gives the order by degree.
    `GraphView.setEdgeOrder` takes the `Uint32Array` and reads it a run at
    a time (`order::EdgeOrder`), and `orderEdges` writes 65,536 edges a
    call and waits for the GPU to take them, so the view does not hold the
    order or its staging (6 MiB). Ordering a million edges takes the worker
    510 to 620 ms where it took 285 to 325.
  - **Measured.** `graphTimings().memory` gives `{edges, view, layout}`,
    each instance's bytes and bytes an edge, and `tests/graph-timing.ts`
    fails over 20 MiB for the view or 60 MB for the worker on rmat.
    `ursa-viz/tests/memory_scale.rs` counts every allocation of a view and
    a layout at a million edges, in CI.
- **Scenes that change in place in the graph view** (#441): a view made
  from a graph result applies a delta in time proportional to the delta, not
  to the graph. `createViewFromResult(canvas, bytes)` takes a result as UGR
  and `GraphView.applyDelta(bytes, positions?)` a delta: new nodes and edges
  are written after the ones there are (the GPU buffers grow by doubling,
  what they held copied on the GPU), a removed node is hidden by its flag
  and a removed edge in its slot, and only the nodes whose radius or colour
  a delta changed are written. Sizes and colours follow the result's
  encodings (`size_by`, `color_by`; by degree without). Undo is the inverse
  delta (`invertDelta(bytes)`) with the positions saved before the step: no
  index changes, and a delta and its inverse leave the frame as it was, bit
  for bit, which the browser tests read back on WebGL2. `GraphView.info()`
  gives what a status line and a legend show (shown and hidden nodes and
  edges, what sizes the nodes, the colour legend with counts) and follows
  every delta. In Rust: `View::from_result`, `View::apply` and `apply_at`,
  `View::info`, and `ursa_viz::live::LiveResult`, the bookkeeping without a
  GPU. A delta of 100 nodes and 300 edges on a view of a million edges
  uploads 5 KB and takes 0.5 ms in Chromium (`ursa-viz/BENCH.md`).
  `scene::Graph` gains `append` and `truncate`, and builds its adjacency
  again only when a layout asks; `GraphResult` gains `reserve`, and taking
  nodes off its end no longer reads every edge. `Theme::colors` draws the
  category `u32::MAX` (none) in the node colour. The shell's `.graph` does
  not send deltas yet.
- **A benchmark for the graph view** (`ursa-viz/BENCH.md`). `bun run bench`
  in `ursa-viz/` runs the demo in Chromium on R-MAT graphs and grids of 1K
  to 1M edges, under WebGPU and WebGL2, and records time to first frame and
  to layout done (coarse to fine, and flat per iteration), synced frame times
  while panning and zooming (median and p95), pick latency, wasm and JS
  memory, and bytes uploaded; then the layout alone natively, on one thread
  and on all (`fa2_multilevel`'s new `bench` mode). It writes
  `bench/results.json`; `bun run bench:report` makes the tables. The
  baseline, from a container without a GPU: a first frame in 0.1 s up to
  100K edges and 1.1 s at 1M, a layout done in 8 s for a 1M-edge R-MAT
  graph and 32 s for a 1M-edge grid, wasm 1.4 times slower than native on
  one thread. Not in CI.
- **SQL: a result says what its columns are** (#431). Every graph function
  and `GRAPH_TABLE` marks the columns it returns with Arrow field metadata
  under an `ursa.` prefix, so a reader (the Graph view, a notebook) does not
  guess from names. No value, column name or type changes.
  - `ursa.role` is what a column is: `node` (an algorithm's `id`), `metric`
    or `category` (its output: `pagerank` is a metric, `louvain` a
    category), `seed`, `node` and `distance` (`hop`), `src`, `dst` and
    `step` (`shortest_path`), `walk`, `step` and `node` (`random_walk`), and
    in `GRAPH_TABLE` `node` (a node variable's key), `property`,
    `path_nodes` (`vertices(p)`), `path_edges` (`edges(p)`) and
    `path_elements` (`element_id(p)`). `ursa.graph` names the property
    graph, `ursa.label` the vertex or edge table, and `ursa.var` the pattern
    variable. A `GRAPH_TABLE`'s schema has `ursa.pattern`: the pattern's
    edges as JSON, each with its two node variables and its direction.
  - The metadata follows a column through `SELECT`, `AS`, `WHERE`,
    `ORDER BY`, `LIMIT`, `DISTINCT`, subqueries, views, joins, `UNION` and
    `GROUP BY` keys. A computed column (`round(pagerank, 4)`, `id + 1`) has
    none. In an inner join, a plain column equated with a node's key
    (`FROM members JOIN pagerank(g) USING (id)`) is marked as a node too.
  - `COPY … TO` a Parquet or an Arrow file keeps it: the file remembers its
    graph. `arrow_metadata(column, 'ursa.role')` reads it in SQL.
  - Docs: a "Result metadata" reference page.
- **SQL: `induced_edges(g, nodes => …)`, the graph's edges among a set of
  nodes** (#432). A traversal like `hop`: `nodes` is a list, a query of one
  column, or one id, and each row is an edge whose two ends are both in the
  set, as `src` and `dst` (in the direction the graph stores) and `edge`, its
  row in the edge table as `edges(p)` numbers it. Parallel edges are rows of
  their own, rows are ordered by `edge`, and an id that is not in the graph
  is on no edge. It reads the index (a stored one where there is one):
  the nodes are marked in a bitmap and each one's out-edges walked, so the
  time is the sum of the nodes' out-degrees, whatever the graph's size. The
  same rows from `WHERE src IN (…) AND dst IN (…)` on the edge table cost a
  scan of it. The columns carry the `ursa.*` metadata (`src`, `dst`, and a
  new role, `edge`). `ursa_core::algo::induced_edges` is the kernel.
  - In the browser's engine, on the 1M-edge R-MAT graph: the 4,832 edges
    among 5,000 nodes in 12 ms with the nodes from a query and 18 ms with
    them as a list, where the edge-table scan takes 190 ms; a two-ring `hop`
    neighbourhood and its 9,926 edges in 20 ms, where the scan form takes
    120 ms. From a stored index, natively, a fixed 20,000-edge answer among
    5,000 nodes takes 20, 21 and 27 ms on graphs of 125K, 1M and 4M edges
    (an unoptimized build; 32 times the edges, 1.3 times the time).
  - A list of 65 or more literal node ids (`seeds => [3, 17, …]`,
    `nodes => […]`, for every traversal) is planned as one column in memory
    instead of one expression per id: the same rows and the same errors,
    and 5,000 ids plan in a few milliseconds instead of 70. `EXPLAIN` shows
    it as `TableScan: node_ids`.
- **Parallel Louvain, opt-in: `louvain(g, parallel => true)`** (#387). In
  Python `ur.louvain(edges, parallel=True)`; in Rust a `parallel` argument
  to `ursa_core::algo::louvain` and `louvain_weighted`. Local moving runs in
  64 fixed sub-rounds of the seeded visit order: the nodes of a sub-round
  choose their community in parallel against the state at its start, and
  the moves are applied in order. The same seed gives the same labels on any
  number of threads, but not the labels the serial default finds, and
  NetworkX has no equivalent. The default stays serial and its results are
  unchanged, bit for bit.
  - Pinned instead by: the same labels and modularity bits on 1, 2, 3, 4 and
    8 threads; golden label hashes on karate, planted-partition and R-MAT
    graphs; and a mean modularity within 2% of the serial run's on the same
    graphs (measured: from 1.5% below to slightly above).
  - Two single-node communities in one sub-round could swap for ever, so a
    single node joins another single node of its sub-round only if that one
    has the lower id. A level stops when a sweep moves fewer than one node in
    1,000, or its real modularity gain (not the movers' estimates) is under
    1e-7, or after 64 sweeps.
  - Memory: each thread keeps a hash table sized to the degree of the node it
    is on (a dense array only for a hub as large as the graph), not one
    entry per node; each sub-round keeps a list of its active nodes, so late
    sweeps do not scan the graph; isolated nodes are skipped as in the
    serial run (#326). Peak memory is the serial run's.
  - On a shared 4-core machine: R-MAT at 80M edges in 22.3 s instead of
    41.9 s, and planted-partition graphs at 80M edges in 75.5 s instead of
    159.0 s (30% of edges between communities) and 123.3 s instead of
    333.9 s (60%), at 3.5 cores busy instead of 1.1, with the same peak
    memory and modularity from 0.7% below to slightly above the serial
    run's. On one thread it does about 1.7 times the serial run's work.
  - `examples/louvain_memory.rs` takes `--parallel`, and `--planted P` for a
    planted-partition graph with P% of edges between communities.
- **Parallel label propagation, opt-in: `label_propagation(g, parallel =>
  true)`** (#453). In Python `ur.label_propagation(edges, parallel=True)`;
  in Rust a `parallel` argument to `ursa_core::algo::label_propagation`. The
  frame of parallel Louvain: each sweep takes 64 fixed sub-rounds of the
  seeded visit order, the nodes of a sub-round vote in parallel against the
  labels at its start, and the new labels are written in order. The same
  seed gives the same labels on any number of threads, but not the labels
  the serial default finds (on a graph of at most 64 nodes it is the serial
  run), and NetworkX has no equivalent. The default stays serial.
  - Two neighbours of one sub-round would trade labels for ever (the
    two-colouring of synchronous label propagation), so a node does not
    read a higher label than its own from a node of its sub-round. Without
    that rule, 14 nodes of the R-MAT graph below still traded labels in the
    20th sweep; with it, the run stops after a sweep that changes nothing, in
    as many sweeps as the serial run (5).
  - Pinned by: the same labels on 1, 2, 3, 4 and 8 threads; golden label
    hashes on karate, planted-partition and R-MAT graphs, masked and not;
    and, on planted-partition graphs, a modularity no more than 1% below the
    serial run's and a community count within 2% of it (measured: within
    0.2% for both).
  - Each thread keeps a table sized to the degree of the node it is on, and
    each sub-round a list of its active nodes.
- **Serial label propagation is 2.7 to 3.1 times faster, with the same
  labels** (#453). A node votes again only once a neighbour has taken a
  label other than its own since its last vote; any other node would vote
  for the label it holds. On a shared 4-core machine, 32M edges, 20 sweeps
  allowed:

  | graph | before | serial now | `parallel => true` |
  |---|---|---|---|
  | R-MAT, 2^21 nodes | 11.0 s | 4.0 s | 2.3 s at 3.3 cores |
  | planted partition, 20% of edges between | 21.4 s | 7.5 s | 3.3 s at 3.6 cores |
  | planted partition, 50% between | 35.0 s | 11.2 s | 4.9 s at 3.7 cores |

  Peak memory is unchanged (605 MB, of which the graph is 575 MB) and
  8 MB more for the parallel run. Without a mask, a vote no longer reads
  the edge ids.

- **`ursa serve [DIR]`: the shell in a browser tab, over a local WebSocket
  to the native engine** (URSA-3, `design/console.md` §5; ursa-cli's
  non-default `serve` feature, over the new crate `ursa-server`). It prints
  a link with a one-time token and serves the shell's page, which talks to a
  native session (`web/public/socket-transport.js`) with the same protocol
  the browser demo speaks to its wasm worker. Each tab gets a session of its
  own (`hello {session: "new"}`, as the shell asks), or attaches to the
  server's one shared session (`"shared"`, the default). `cancel {target}` stops a running request with
  DataFusion's cancellation, and a closed tab cancels its own; Ctrl+C stops
  the server gracefully. It listens on `127.0.0.1` and `::1` only; the token
  becomes an `HttpOnly`, `SameSite=Strict` cookie, which the WebSocket and
  the served directory's files (`/data/`, with byte ranges read from the
  file) need; every request's `Host`, and every WebSocket's `Origin`, must
  be the server's own; and every response has the deploy's security
  headers. The page's files come from `--assets DIR` (a `web/dist`), or are
  built in with `rust-embed` (`serve-embed`, from a checkout). `ursa-server`
  is published with the other crates, before `ursa-cli`. Release binaries
  do not have `serve` yet.
- **The shell's e2e suite runs in CI over both transports** (URSA-5).
  `.github/workflows/shell-e2e.yml` runs a second leg, "Ursa Shell (browser
  e2e over ursa serve)", beside the worker one: it builds `ursa-cli
  --features serve` (debug) and runs `web/scripts/e2e-serve.ts`, on the same
  triggers plus changes to `ursa-server` and `ursa-cli/src/serve.rs`. The
  socket run skips exactly the suite's 6 browser-only checks, which the suite
  now lists and holds it to: a run that skips any other check fails.
- **SQL: DuckPGQ's algorithm names, and errors that point to the standard
  form** (#298, #300). Each was an error before, so no query that ran
  changes.
  - `weakly_connected_component(pg, Student, know)` runs
    `connected_components(mode => 'weak')` and returns DuckPGQ's column,
    `componentid` (unquoted names fold to lower case, so DuckPGQ's
    `componentId` finds it); `local_clustering_coefficient(pg, Student,
    know)` runs `clustering_coefficient` and returns
    `local_clustering_coefficient`. They take DuckPGQ's positional (graph,
    vertex table, edge table) form, the implied pair on a graph of one edge
    table, the named parameters of the function they run (not `mode`, which
    the name fixes), and work inside `algorithms()`. Values stay NetworkX's.
  - `ursa_functions()` lists them as rows of their own, after `algorithms`,
    and has two new columns: `same_as`, the Ursa function a DuckPGQ name
    runs, and `duckpgq`, true for the three functions DuckPGQ has by name
    (`pagerank` too). They are other systems' spellings; Ursa keeps no old
    names of its own. The function reference shows them.
  - DuckPGQ's scalar path functions (`iterativelength`, `shortestpath`,
    `reachability`, `cheapest_path_length`) say to use `GRAPH_TABLE … ANY
    SHORTEST` or `shortest_path()`.
  - A statement that starts with `MATCH` (Cypher, GQL) says to write
    `SELECT * FROM GRAPH_TABLE (g MATCH … COLUMNS (…))`. `FROM g`, a graph
    read as a table, says to read `g.nodes` or `g.edges` (or the vertex and
    edge tables, on a graph of several), use `GRAPH_TABLE`, or call a graph
    function; `FROM g.nodes` on a graph of several vertex tables says why
    there is none. A graph function named as a column of a vertex table
    (`SELECT id, pagerank FROM Student`) names the pair in the join it
    suggests: `JOIN pagerank(pg, student, know) USING (id)`.
    `pagerank(pg, vertex => Student, edge => know)` says the pair is
    positional.
  - Docs: a "Coming from SQL/PGQ or DuckPGQ" page (what runs unchanged,
    what differs, Ursa's extensions, the numeric differences from DuckPGQ,
    and how to translate), and a section in the agent skill. The
    conformance suite has a `duckpgq/` directory for DuckPGQ's algorithm
    calls, and DuckPGQ's PageRank values are on its known differences.

- **The console's e2e suites run in CI** (URSA-18). `web/tests/console-all.ts`
  (`bun run test:console-all`) runs the console, editor and panels suites in
  turn, each with its own `ursa serve`, prints a summary and fails if any
  check does. `shell-e2e.yml` runs it in a third leg, "Ursa Console (browser
  e2e over ursa serve)", built as the shell's socket leg is and sharing its
  caches, on the same paths, and keeps the console's screenshots, light and
  dark, as the `console-e2e` artifact on every run.
- **The console's result tabs** (URSA-14, `design/console.md` §6). Each
  result the console runs opens in a tab of its own, labelled with its
  statement and row count (the ARIA tabs pattern; Delete or × closes it and
  frees the result with `close`). A tab shows its result as a table: Arrow
  decoded in the page, paged with `fetch` and drawn a screenful at a time,
  so 100,000 rows scroll smoothly; each header names its column's type and
  `ursa.*` role; numbers right-aligned, NULL dimmed as the shell shows it,
  long text cut short with its whole value on hover and focus; a header
  sorts the loaded rows (and says so); a cell or a range copies as
  tab-separated text. A result whose columns make a graph has a Graph view,
  drawn with `graph.open {result}` in the page's one graph pane, whose
  commands (a double-click's `.expand 33`) go to a sink History will take
  (`results.setCommandsSink`), listed under the graph until then. A Plan
  view shows `explain`, and Export downloads CSV, Parquet, JSON or Arrow. A
  result the session has dropped (`not_found`) offers Run again. A
  statement with no columns says its notices instead of opening a tab.
  `ursaConsole.run(sql)` runs SQL into the panel; the editor (URSA-13)
  calls it.
- **Session protocol: `query` says how its result is a graph, and
  `explain {sql}`** (URSA-14). `query`'s reply has `graph`, the rule
  `graph.open` would draw the result by (`nodes`, `edges`, `path`, `rings`,
  `pattern`), read from its columns (`ursa_engine::graph::view::rules::rule_of`),
  or `null`. `explain` gives the plan of the last statement, logical then
  physical, as text, and runs nothing.

- **The console's Catalog and Sources panels** (URSA-15, URSA-16,
  `design/console.md` §6). The Catalog is a tree of the session's
  databases, schemas, tables and views (marked by kind) and their columns
  with types, and a section for property graphs with their vertex and edge
  tables. It follows the session as it changes, from any tab or agent on
  it, keeping what was open and the scroll; it has a filter and the ARIA
  tree's keys. Choosing a table puts `SELECT * FROM … LIMIT 100` in the
  editor; a graph, `.graph <name>`. Sources lists the session's root (`ursa
  serve DIR`) with each file's format and size, browses into folders, and
  browses buckets added by their `s3://`, `gs://` or `az://` prefix (kept
  per server). Choosing a file puts its query in the editor and runs it; a
  `.ursa` file can be attached. Errors, such as a bucket's missing
  credentials, show in the panel. Without the `sources` capability (the
  browser demo) the panel says so. `tests/console-panels.ts` tests both
  over `ursa serve`. The console's `session.onEvent` now also reaches the
  connection being opened, so a panel that listens while the page connects
  hears its first events.

- **The console's SQL editor** (URSA-13, `design/console.md` §6):
  CodeMirror 6 in the Editor panel. Its SQL is coloured by the session's
  `highlight`, so it looks exactly as the shell's terminal colours it, in
  light and dark, and completed by its `complete`: tables, views, columns
  (after `t.` too), graphs, functions, keywords and dot-commands, each with
  its kind and detail. Ctrl/Cmd+Enter runs the statement at the cursor, or
  the selection; Shift+Ctrl/Cmd+Enter runs every statement in turn and stops
  at the first error. The statement running is marked, and the status bar's
  Cancel stops it. An error shows at its statement: a squiggle under what it
  names (or where the parser stopped) and the message with the engine's hint.
  Dot-commands run through `feed`, as the shell's do. The text is kept across
  a reload, per server port. Tab indents and accepts a completion; Esc then
  Tab leaves the editor. Other panels put SQL in it with
  `ursaConsole.editor.insert(text, {statement, run})`. CodeMirror's packages
  (MIT) are pinned and bundled into `vendor/codemirror.mjs`, with their
  licences; the build refuses one that isn't MIT. `tests/console-editor.ts`
  tests it over `ursa serve`.

- **The Ursa Console's page: `ursa serve`'s link opens it** (URSA-12,
  `design/console.md` §6). At `/console/`, with the shell still at `/`: a
  sources and catalog column, the editor over the result tabs, and the
  inspector over History, each panel an empty mount point labelled with the
  issue that fills it. The columns and panels resize (drag a splitter, or
  focus it and use the arrow keys), fold, and become drawers under 1024px;
  skip links, F6 between panels and a visible focus ring keep it keyboard
  reachable; light and dark come from the shell's tokens. The status bar
  shows the connection (to the shared session, reconnecting when the server
  goes, and saying when it was restarted with a new link), the engine's
  version, the session's root, and the request running with a Cancel button
  (`cancel {target}`, through `engine-client.js`'s new `cancel(reply)`).
  `bun run build` builds it into `web/dist-console/`, never into the
  deployed `web/dist/`; `ursa serve` serves both (`--assets` may be
  repeated) and builds both in with `serve-embed`. The graph pane's styles
  move from `style.css` into `graph-pane.css`, which the shell links; the
  shell looks and behaves as before.
- **`ursa serve` in the release binaries, with a database file,
  `--read-only` and `--no-open`** (URSA-4, `design/console.md` §5). The
  release and nightly binaries have `ursa serve` with its page built in
  (`serve-embed`: `cli-build.yml` builds the page once, with `bun
  scripts/build.ts --no-engine`, and every platform embeds it); the crates
  on crates.io still have no page. `ursa serve club.ursa` opens the
  database in every session as `ursa club.ursa` does, rooted in its
  directory; `ursa serve DIR` roots the sessions in DIR (relative paths,
  `.read`, `ATTACH` and writes resolve under it), and with neither the
  session is in memory, in the current directory: the server no longer
  changes the process's working directory. It opens its link in the
  default browser unless `--no-open`, on port 4280 or the next free one.
  `--read-only` writes no files, for pairing with an agent: `COPY … TO`,
  `INSERT INTO` a table of files, `.output`, `.once`, `ATTACH` of a
  database that does not exist, and what changes the database (opened
  read-only) are refused by the engine (`ursa_engine::paths::set_read_only`,
  `ursa_shell::session::Options::read_only`). The shell's page keeps
  `COPY … TO 'name'` in memory, as in the browser demo; the shared session
  (the console's) writes it to disk under the root. `web/scripts/build.ts
  --no-engine` builds the page without the wasm engine. The Linux x86_64
  release binary grows from 147.2 MB to 169.6 MB (115 to 132 MB stripped;
  its archive from 40.9 to 47.0 MB): 18.7 MB for the server, 3.7 MB for the
  page.

### Changed

- **SQL: an edge with a NULL source or destination is not part of the graph,
  on every path** (behaviour change, #336). One rule now holds with or
  without a vertex table, as DuckPGQ's inner join and #311's vertex keys
  have it: such an edge is dropped and counted in `describe`'s
  `n_dropped_edges`, with the edges to no vertex key, and neither of its
  ends is a node by it. The index build drops it in one place
  (`ursa_core::EdgeInterner::dropping_nulls`, used by every SQL build), and
  `g.nodes` leaves out the ends of such edges, so the algorithms, `hop`,
  `shortest_path`, path searches and `GRAPH_TABLE` through the index or as
  joins all see the same edges and nodes. `ursa-sql/tests/null_endpoints.rs`
  runs every path on one graph (integer and string keys, with and without
  a vertex table), and `tests/conformance/sqlpgq/null_endpoints.slt` holds
  the standard's cases. The Python library is unchanged: a NULL endpoint is
  still an error unless `on_null="drop"`. What changes, on a graph **without
  a vertex table** whose edge table has a NULL endpoint:
  - **The algorithms, `hop`, `shortest_path`, `describe`, `GRAPH_TABLE` with
    a bound node and `GRAPH_TABLE` path searches no longer fail** with
    "graph 'g': an edge has a null source or destination; filter those rows
    out …". They leave the edge out, and `describe(g)`'s `n_dropped_edges`
    counts it (it was always 0 without a vertex table).
  - **`g.nodes` and `GRAPH_TABLE (g MATCH (x) …)` no longer list a value
    that is only the other end of a NULL-ended edge** (wrong answer: they
    listed it as a node, while no edge pattern could reach it and the
    algorithms refused the graph). `hop(g, seeds => [x])` from such a value
    is refused as it is for any value that is no node.
  - `GRAPH_TABLE` planned as joins (`(a)-[e]->(b)`, `-[e]-`) already left
    the edge out, and a graph with a vertex table already dropped and
    counted it: their answers do not change. A NULL vertex key is still
    refused by every path (#411).
- **Edge ids are built only when a query reads them** (#170, part of
  #155). `edge_ids`, the original row of each CSR slot, is 4 B an edge in
  each built direction, and only a weight, an edge mask or an edge's row
  reads it. It is now an `ursa_core::EdgeIds`, which builds itself on first
  read and keeps what it built: from the identity when the rows are already
  grouped by source (no source needed, #155), or from the rows' sources read
  back (`RowSource`; the SQL engine writes them to an unlinked temporary
  file in `TMPDIR`, `SpilledRows`, 4 B an edge, for an index of 65,536 edges
  or more). Kernels branch once per call: PageRank, components, BFS/`hop`,
  Louvain, label propagation, triangles, closeness, betweenness and degree
  without a mask or weight read neighbours alone.

  The in-adjacency is built without any edge ids: the first in-direction
  kernel streams the spilled sources once and reads each row's target off
  the out-adjacency, then a stable counting sort by target leaves every
  in-segment in row order, as before (`transpose_rows`). Its own ids are
  built the same way when first read. The undirected view (triangles,
  clustering) transposes in slot order for its build alone, as only the set
  of neighbours matters there: it reads no spill and keeps no in-adjacency.
  Storing an index writes the ids without keeping them, and stored files are
  unchanged (still version 5).

  When the spill cannot be written (no temporary directory, or no room in
  it), the index keeps its ids in memory, as before, and the session gets a
  notice (`could not write the index's edge rows to …; its edge ids are kept
  in memory (…)`); the query goes on. What a failed write wrote is freed at
  once, as the file is unlinked when it is created.

  Results are byte-identical: `ursa-core/tests/lazy_edge_ids.rs` runs every
  kernel, plain, weighted and masked, over eager, in-memory and spilled ids
  (sorted and unsorted rows, both node orders), checks that no plain kernel
  builds them in either direction, and counts the spill's reads: none for
  out-direction kernels and the undirected view, one stream for the first
  transpose. R-MAT, built as the engine builds it (in-degree relabel), 4
  threads, `ursa-core/examples/edge_ids_memory.rs`, eager → late; resident
  memory after the kernel and peak RSS both include the 8 B/edge edge list
  the build is given:

  | R-MAT 64M | resident after | peak RSS | build + kernel |
  |---|---|---|---|
  | index only | 569 → 325 MiB | 1108 → 865 MiB | 1.4 → 1.0 s |
  | PageRank (20 iterations) | 1165 → 614 MiB | 1254 → 975 MiB | 1.4 + 12.0 → 1.0 + 11.3 s |
  | BFS (both directions) | 1174 → 624 MiB | 1177 → 992 MiB | 1.0 + 1.4 → 0.7 + 2.1 s |
  | components (weak) | 610 → 369 MiB | 1106 → 863 MiB | 1.1 + 1.0 → 0.8 + 1.0 s |
  | `hop` (2, from the hub) | 588 → 344 MiB | 1106 → 863 MiB | 1.1 + 0.22 → 0.8 + 0.23 s |
  | Louvain (serial) | 615 → 369 MiB | 1335 → 1096 MiB | 1.1 + 13.9 → 0.8 + 14.2 s |
  | weighted PageRank | 1156 → 1185 MiB | 1733 → 1733 MiB | ids and transpose 1.4–1.7 → 2.3–2.7 s |

  At 16M edges: index 143 → 82 MiB, PageRank 292 → 155 MiB and BFS 289 →
  158 MiB resident; peaks 279 → 219 MiB (index, components, `hop`), 317 →
  248 MiB (PageRank), 297 → 252 MiB (BFS) and 338 → 278 MiB (Louvain). The
  build is faster (it writes the sources once instead of scattering the
  ids). The transpose alone takes 1.8–2.1 s at 64M streamed against
  1.3–1.6 s eager, with a 4 B/edge transient target column; a weighted or
  masked query rebuilds the ids once (about 1 s more at 64M, the weighted
  iterations themselves unchanged), and in-ids read late take 1.8–2.1 s.

- **`ursa serve`: the page holds its WebSocket in a worker, and a large part
  is not copied on the page** (URSA-7). `web/public/socket-transport.js`
  opens the socket in `socket-worker.js`, which assembles each reply and
  posts it to the page with its parts' buffers transferred, as the wasm
  engine's worker does. A part is no longer copied out of its frame
  (`data.slice(8)`): it is the frame's buffer at `offset` 8, which
  `engine-client.js`'s header now allows a part to carry. `.graph rmat`'s
  8.4 MB of UGR took 100 to 120 ms of the page's thread in long tasks
  before the reply was complete; it now takes none. Its first frame is
  where the worker's is, within noise: medians of 24 runs each, at a load
  average of 3 to 19 on 4 cores, 1,592 ms over the worker and 1,625 ms
  over the socket (1,609 ms before this change). `web/tests/graph-timing.ts`
  runs against `ursa serve` with `URSA_SERVE_TOKEN`, and stops at the first
  frame with `FIRST_ONLY=1`.

- **SQL: an index of 65,536 edges or more numbers its nodes by in-degree**
  (behaviour change: `louvain` and `label_propagation` communities, the
  last bits of floats and the order of unordered rows, #172). Dense node ids
  were first-seen order, which for anonymised ids sorted by source (the
  Bluesky export) carries no locality: every rank a pull PageRank reads is
  close to a random access. The build now numbers the nodes by in-degree,
  highest first, ties in first-seen order, and renumbers the id map with
  them (`ursa_core::NodeOrder`, `Topology::build_ordered`). The relabel is
  fused into the counting sort: its first pass counts the in-degrees too
  (from 4,194,304 edges, on one block of rows in eight: the order is a
  heuristic for the hubs), a counting sort orders the nodes, and one
  streaming pass renames the targets. Each node keeps its first-seen id
  (`Topology::original_ids`, stored with the index), and what must not
  depend on the numbering is defined on it: component labels (below), the
  sampled sources of `betweenness`, `avg_path_length` and `diameter`, the
  ties of a weighted shortest path, and the layouts. Per-node rows still
  come out in first-seen order, and label propagation's labels are
  first-seen ids. Partitions of components, counts, distances, reached sets
  and user ids do not change; `ursa-core/tests/relabel.rs` and
  `ursa-engine/tests/locality.rs` check every kernel against a first-seen
  build. A smaller graph keeps its first-seen numbering (its arrays fit in
  cache in any order), and the Python library does not relabel.

  **Behaviour change: `louvain` and `label_propagation`** visit the nodes,
  break ties and (Louvain) aggregate in node order, so on a relabelled index
  a seed gives other communities than before, of the same quality. They are
  still the same for a seed on any thread count (pinned in
  `tests/louvain_pinned.rs` and `tests/determinism_threads.rs`). They run on
  the relabelled graph itself: renumbering it back for them would cost 8
  bytes an edge (Louvain) or 16 (label propagation) on every call, about 19
  and 39 GB on the full 2.4B-edge Bluesky graph. Seed 1, community count
  and modularity, first-seen → in-degree:

  | graph | `louvain` | `louvain(parallel)` | `label_propagation` |
  |---|---|---|---|
  | R-MAT 16M | 223, 0.0682 → 222, 0.0683 | 222, 0.0680 → 223, 0.0677 | 205 → 201 |
  | R-MAT 64M | 843, 0.0622 → 839, 0.0624 | 842, 0.0619 → 840, 0.0617 | 827 → 817 |
  | sorted 64M | 839, 0.0624 → 841, 0.0624 | 840, 0.0617 → 839, 0.0621 | 836 → 817 |

  (Label propagation finds no structure in R-MAT: modularity 0.00003 in
  either order.) Serial Louvain is 9–15% faster on the relabelled 64M-edge
  graphs. `benchmarks/bluesky_cloud/run.py`'s `check_louvain_ge` now
  accepts 3,624 communities ±5%, until the next round re-records it.

  Measured with `ursa-core/examples/locality_order.rs` on a 4-core Xeon
  (504 MB L3) shared with other builds, best of 3, CPU seconds. On R-MAT
  64M edges with anonymised ids sorted by source ("sorted"), one thread:
  PageRank 0.30 → 0.19 s per iteration (-37%), BFS 0.32 → 0.26 s, weak
  components 0.44 → 0.37 s. On plain R-MAT, whose first-seen order already
  puts the hubs first, nothing measurable moves (16M and 64M edges:
  PageRank, weak and strong components, `hop`, BFS, triangles and closeness
  within the run-to-run noise). The relabel adds to a 64M-edge build
  (after #544, median of 5): R-MAT 0.79 → 0.92 s (+15%), sorted 0.35 → 0.45
  s (+27%; its first-seen build scatters in order, which no relabel can);
  peak memory +41 MB (the node order). Breadth-first and reverse
  Cuthill–McKee orders were measured too: no faster than in-degree, and 5 to
  8 times its build cost. An index stored by 0.12 (format version 4) is read
  with its numbering kept; a relabelled one is stored as version 5, with its
  original ids.
- **Component labels are canonical** (behaviour change, #150).
  `connected_components` labelled a component by its union-find root (weak)
  or Tarjan root (strong), which depend on the order the work is done in. A
  component's label is now the first-seen id of its first node to appear
  (its smallest dense id when nothing is relabelled): the same for the
  serial and parallel kernels, and unchanged by the relabel above. The
  partition is the same. Python's `ur.connected_components` gives the same
  labels.

- **The web shell talks to its engine through the session protocol**
  (#536, `design/console.md` §4). The per-operation glue of `ursa-wasm`'s
  `WebShell` (output capture, UGR encoding, inspect's JSON, uploads,
  export) is `ursa_shell::session::Session`, with every operation behind
  `Session::handle(op, args, parts)`; `WebShell` is wasm-bindgen glue over
  it, with one method, `handle`. The page calls the engine through
  `web/public/engine-client.js` (`connect(WorkerTransport)`), and the
  operations take their protocol names (`graph.open`, `graph.explore`,
  `cancelInput`, `highlightAnsi`, …). Nothing a user sees changes.

- **The session protocol is version 1** (URSA-2), with the spike's changes
  to `design/console.md` §4. The first request must be `hello`, with
  `protocol: 1` and `session: "new" | "shared"`; the session refuses any
  other request before it and any other version, with kind `protocol`. The
  session gains `cancel`, `memory` (`{bytes, kind: "wasm" | "rss"}`, which
  the worker answered itself before) and the console's `query` (typed
  columns, the schema's metadata such as `ursa.pattern`, and the first rows
  as an Arrow IPC part). `ursa_shell::session::OPS` lists the operations;
  the new reference page, `docs/reference/protocol.md`, is
  generated from it and tested against it.

- **The session keeps results, by id** (URSA-9). `query` gives its result
  an id (`"r4"`) and keeps it: `fetch {result, offset, rows}` pages through
  it as Arrow IPC, `export {result}` writes it, `graph.open {result}` draws
  it, and `close {result}` drops it, so the console can keep several
  results open. A session keeps its last 16 results within 256 MB, the
  oldest dropped first; a result over 256 MB on its own keeps only its
  first rows. A result that has gone, or rows past those kept, fail with
  `not_found` and a message to run the query again. The shell's last result
  (`.graph`, `.export`) is unchanged.

- **The graph pane is a module any page can embed** (URSA-8,
  `design/console.md` §3). `web/public/graph-pane.js`'s `createPane({container,
  engine, commands, status})` makes the Graph view's pane: its markup, the
  `/` search and `.find`, the not-shown counts, Esc, the theme, and the graph
  commands (`.graph`, `.find`, `.expand`, `.collapse`, `.hide`, `.show`,
  `.undo`, `.redo`), run through the engine client. What the view asks to run
  (a double-click's `.expand 33`, the inspector's and the search's buttons)
  goes to the host's `commands` sink: the web shell's is its terminal, as
  before, and the console's will be its History panel. `app.js` loses about
  250 lines of graph glue; the graph view behaves as before. A second host,
  `web/tests/pane-host/` (never deployed), proves it in the e2e test.

- **The session sends events, and the console's catalog, sources,
  completion and highlight operations** (URSA-13, URSA-15, URSA-16, the
  protocol's part; `design/console.md` §4). `catalog` lists the session's
  databases, schemas, tables (`table`, `view` or `external`) with their
  columns, and its property graphs with their vertex and edge tables. After
  any request that changes it (`CREATE`, `DROP`, `ATTACH`, `CREATE PROPERTY
  GRAPH`, `.open`, a dataset load), the session sends `{"event":
  "catalog"}`, found by comparing a fingerprint of the catalog before and
  after: `ursa serve` sends it to every connection on the session, the
  browser demo's worker posts it, both before the request's reply, and
  `engine-client.js`'s `on("catalog", fn)` hears it. `sources.list {url}`
  lists a directory under the session's root (symbolic links resolved, a
  path outside refused, hidden files left out) or a bucket prefix (`s3://`,
  `gs://`, `az://`, through `object_store`'s `list_with_delimiter` and the
  CLI's credential chain), with each entry's format and size, never a
  credential; `sources.attach {url, as}` is `ATTACH`. Both need capability
  `sources`, which a session with a directory root reports in `hello`.
  `complete {sql, cursor}` offers tables, columns (qualified after `t.`, an
  alias's too), functions, graphs, keywords and dot-commands, without
  running a query; `highlight {sql}` gives the shell's colouring as spans.
  Their offsets are UTF-16 code units, as JavaScript's. The protocol
  document gains an Events section, generated from
  `ursa_shell::session::EVENTS`.

- **A root per session, and `COPY … TO` to disk under it** (URSA-6,
  `design/console.md` §5). Relative paths resolve against the session's
  root (`ursa_engine::paths::Root`), which travels with the session's
  config, in place of a base URL global to the process: a directory
  natively, a URL in the browser. Two sessions in one process each read
  their own files. A session with no root is the CLI's: relative to the
  working directory, as before. The root covers `FROM 'x.csv'`,
  `read_*()`, `CREATE EXTERNAL TABLE … LOCATION`, `COPY … TO`, `ATTACH`
  (and `.open`), `.read`, `.output` and `.once`. A session with a directory
  root reads anything (an absolute path, `~/…` or a URL reads what it
  names), but writes only under the root: a `COPY … TO`, `.output` or
  `.once` target must be a relative path with no `..`, and no symbolic link
  in it may lead out; an `INSERT INTO` a table of files must be into files
  under the root. `ursa_shell::session::Options` gains `root` and
  `copy_to_disk`: a native session can keep `memory://uploads` for the
  files a page adds and write its copies to disk, as `ursa serve` will.
  The browser demo is unchanged: `WebShell.setBaseUrl` sets the page's
  root, which each session takes when it is made.

- **Default (serial) Louvain is about a third faster, with the same
  communities** (#268). Local moving, the serial core of `louvain(g)`, took
  70% of a run. Each node visit sorted every neighbouring community by id
  before scanning them for the best move; it now keeps only the communities
  whose gain beats staying (usually none, or one or two) and sorts those.
  The others could never be chosen, so the choice is the same. Local moving
  also prefetches the next node's adjacency, each neighbour's community and
  its accumulator entry a few slots ahead, and each community's total. The
  visit order, the tie rule and the order of every floating-point sum are
  unchanged: labels are the same bit for bit, for every seed, on any thread
  count. A new test, `tests/louvain_pinned.rs`, pins them with golden hashes
  recorded before the change (karate, R-MAT, isolated nodes, weighted and
  masked graphs, serial and `parallel => true`) on 1, 2, 4 and 8 threads and
  without `rayon`. R-MAT, `seed => 1`, 4 shared cores, best of 3
  interleaved runs:

  | edges | before | after | peak RSS |
  |---|---|---|---|
  | 16M (2^20 slots) | 6.44 s, 5.14 CPU-s | 4.72 s, 3.77 CPU-s | 480 → 476 MB |
  | 64M (2^22 slots) | 43.1 s, 32.5 CPU-s | 27.4 s, 21.1 CPU-s | 1,909 → 1,897 MB |

  At 64M edges, local moving went from 21.7 to 11.6 CPU-s (70% of the run
  to 50%). It stays serial, as its result depends on the visit order; the
  graph build (now a third of the run) and the aggregation (a sixth) were
  already parallel. `examples/rmat_kernel.rs` gains
  `--kernel louvain`.
- **Benchmarks: memory is the process's footprint, with peak RSS beside it**
  (#533). `benchmarks/bluesky_cloud/run.py` recorded each step's peak
  memory as `ru_maxrss`, which since #382 counts the pages of the
  memory-mapped stored index a step reads, so round 5 (#388) flagged
  PageRank and the components as memory regressions when they use less. The
  main figure (`peak_footprint_bytes`, the **Memory** column of
  `summary.md`) is now the process's own memory without those clean file
  pages: on macOS the lifetime peak `phys_footprint` (`proc_pid_rusage`,
  `RUSAGE_INFO_V4`, read before the child is reaped), on Linux the peak of
  `RssAnon + RssShmem` sampled from `/proc/<pid>/status` every 20 ms.
  `peak_rss_bytes` stays as a second column, and `meta.memory_measure`
  says how the footprint was taken. `benchmarks/sql_scale` records both the
  same way (`peak_mem_mb`, `peak_rss_mb`), and its `--mem-limit-gb` applies
  to the footprint. Still standard library only.

- **The graph view's inspector shows an edge's row in 20 to 40 ms on a
  million edges, not 90 to 160** (#489). An edge's rows were found by a
  scan of the edge table for its two ends. They are now found through the
  graph's index (`induced_edges` of the two ends gives their row numbers)
  and read by number: from a Parquet file with a row selection, which
  skips the pages before the row, with the bytes read kept for the
  session (`ursa_engine::graph::view::rows`, 64 MiB at most); from any
  other table by passing over the rows before. A table of the two key
  columns alone, such as rmat's, is not read at all, and the drawing's
  row number picks one of parallel edges among the rows found. The rows
  are the ones the query by the two ends gives, in the same order, and
  the scan remains for what the index cannot say: a graph of several edge
  tables, or a table read as several partitions. `WebShell.inspectEdge`'s
  JSON says `indexed`. On rmat in headless Chromium (medians of four
  sessions of 5 to 7 runs, under a load of 2 to 7 on 4 cores) an edge's
  row comes 21 to 41 ms after the click, 13 to 31 ms of it in the engine;
  the first click of a page takes 54 to 83 ms, of which the engine's part
  is 17 to 41 ms and the rest the page's first panel. Where the row is
  read (a view of rmat with a third column) it is 31 to 43 ms, and 77 to
  112 ms on the first click, which fetches the file's column chunks (see
  `web/tests/inspector-timing.ts`). `inspect` takes a `RunSql`.
- **The graph view keeps clear of its inspector** (#493). The inspector's
  panel is drawn over the web shell's Graph view, and the nodes under it
  could not be clicked, double-clicked or expanded. Now the camera makes
  room: 450 ms after a selection opens the panel (once a second click or
  tap would no longer be a double one), what was in sight moves into the
  part of the view the panel leaves clear, over 200 ms, or in one step
  under `prefers-reduced-motion`. Closing the panel puts the view back.
  While the panel is open, `0`, `f` and a double-click on empty space fit
  into the clear part, and a camera that follows the layout follows it
  there. The panel opens at once after a tap too (it waited 350 ms): on a
  phone, where it is a sheet across the bottom, it opens across the top
  when the node tapped is drawn low, as it opens on the left on a desktop
  when the node is on the right. The sheet is at most 45% of the view's
  height (it was 55%). `ursa-viz`: `GraphView.setFitInset(left, top,
  right, bottom)` keeps part of the canvas clear when the camera fits,
  and `following()` says whether the camera follows the layout.
- **SQL: graph function parameters and columns renamed, one name per role**
  (breaking, #415). The old names are not accepted. Each one gets a "did
  you mean" with its new name, and so do `k`, `max_hops` and `depth`:
  - `hop(n =>)` is `hop(steps =>)`, as in `random_walk`.
  - `random_walk(start =>)` is `random_walk(seeds =>)`, as in `hop`.
  - `random_walk(seed =>)` is `random_walk(random_seed =>)`.
  - The `node` column of `random_walk` is `id`, so `USING (id)` joins it.
  - The `hop` column of `shortest_path` is `step`, as in `random_walk`.

  `shortest_path` keeps `source` and `target`. A "did you mean" now names
  only a parameter that can take the value given:
  `random_walk(seed => [0])` points to `seeds`, and `seed => 7` to
  `random_seed`. The Python library's names are unchanged.
- **SQL: a literal node id that is not in the graph is an error** (breaking,
  #416). `hop(g, seeds => [99])`, `shortest_path(g, source => 0, target =>
  99)` and `random_walk(g, seeds => [99], …)` returned no rows, which looked
  the same as no path. They now fail and name the id: `hop(seeds =>): 99 is
  not a node of graph 'pg'`. For a list, the error names up to three
  missing ids and gives the count: `5 ids are not nodes of graph 'karate':
  95, 96, 97 and 2 more`. Ids from a subquery (`seeds => (SELECT …)`) are
  still skipped when they name no node. A notice on stderr says how many
  were skipped. To probe for a node, use a subquery or `g.nodes`.
  `induced_edges` still skips ids that are not nodes. `shortest_path` with
  `source` equal to `target` still returns no rows, since a path of no
  edges has no edge rows. Its reference entry now says so.
- **SQL: graph functions accept more argument forms** (#415). `direction`
  values are read in any case (`'BOTH'`, `'Out'`). `'undirected'` is
  refused with a hint to use `'both'`. `pagerank` and `betweenness` take
  `direction => 'in'`, which runs the same kernel on the reversed graph
  (SQL only: the Python verbs still take `'out'` and `'both'`).
  `connected_components(g, direction => …)` says to use `mode => 'weak'`
  or `'strong'`. Named arguments may be written `name = value` or
  `name := value` as well as `name => value`, as in `read_csv`, also inside
  `algorithms(…)`. The reference entry for `diameter` now says that its
  default (`approximate => true`) is a lower bound, and that
  `approximate => false` gives the exact value.
- **An empty `.parquet` file is an error naming it** (behaviour change: an
  empty Parquet file used to read as zero rows). DataFusion's listing
  skipped a 0-byte file before the footer check (#332) saw it, so
  `FROM 'empty.parquet'` returned no rows, and a glob or directory holding
  one read as if it were not there. Ursa now lists a Parquet table's files
  itself and stops at an empty one, as DuckDB and PyArrow do: `cannot read
  'empty.parquet': it is not a valid Parquet file: the file is empty`. It
  covers a file named directly and one found by a glob or a directory, in `FROM '…'`, `read_parquet`, `CREATE EXTERNAL TABLE … STORED
  AS PARQUET`, graphs over them and the Python scans, which name the file
  for a glob (`scan_edges('data/*.parquet'): 'data/b.parquet' is not valid
  Parquet (…)`). An empty CSV or JSON file is still skipped. A table is
  checked when it is created: an external table whose file becomes empty
  later reads it as no rows.
- **`hop`'s rows come back in no particular order** (behaviour change,
  #452). SQL `hop(…)` and Python `ur.hop(…).from_(…)` return the same rows
  as before, with the same distances, but not always in the same order: the
  nodes found at one distance from a seed may come back in another order,
  and the order can differ from one run to another (for example once the
  graph's in-adjacency has been built or read). Each seed's rows still come
  together, seeds in the order given, nearer nodes first, and the order does
  not depend on the number of threads. Add `ORDER BY` (in Python, `.sort`)
  where the order matters. `hop`'s reference entry says so.
- **Breadth-first searches switch direction on large levels: `hop`,
  `diameter`, `avg_path_length` and `bfs_distances` use 3 to 11 times less
  CPU on large graphs, with the same results** (#452). A level whose edges are a large
  share of the edges not yet read is now expanded bottom-up: each node not
  yet reached reads its edges the other way and stops at the first that
  leads to the level (direction-optimizing BFS, Beamer, Asanović and
  Patterson, 2012). A search from one source splits such a level across
  threads. Distances, reached sets, `diameter` and `avg_path_length` are
  unchanged: a new test pins hashes recorded before the change (karate, two
  R-MAT graphs, a disconnected graph, a sparse directed graph, a path; every
  direction, masked and not; several seed sets and depths; `hop`'s rows
  compared sorted), on one thread and four, and without `rayon`. Only the
  order of `hop`'s rows changes (above). In direction `out` a search goes
  bottom-up only when the in-adjacency is already built or stored; it never
  builds one for this. `shortest_path` keeps the top-down search, because
  which shortest path it returns depends on the order. On R-MAT graphs
  (shared 4-core machine, best of two runs, CPU-seconds):

  | graph | query | before | now |
  |---|---|---|---|
  | 64M edges, 4M nodes | full BFS from 3 sources, 1 thread | 2.30 s | 0.44 s |
  | 64M edges, 4M nodes | `hop`, n = 3, from the hub | 1.03 s | 0.14 s |
  | 64M edges, 4M nodes | `hop`, n = 3, `both`, 16 seeds | 28.8 s | 3.8 s |
  | 64M edges, 1M nodes | full BFS from 3 sources, 1 thread | 1.14 s | 0.10 s |
  | 64M edges, 1M nodes | `hop`, n = 2, from the hub | 0.34 s | 0.03 s |
  | 16M edges, 1M nodes | `avg_path_length`, 64 sources | 4.07 s | 1.06 s |

  A `hop` from many seeds that are not hubs gains less (16 seeds, n = 2:
  1.3–2.6×), as most of its time goes to the rows. Peak memory is the same
  within 1% (`avg_path_length` and `diameter` hold 1 byte per node per
  thread instead of 4).

- **A Parquet file's schema metadata is kept when Ursa reads it.** Ursa's
  sessions set `datafusion.execution.parquet.skip_metadata = false`, so the
  `ursa.*` column roles (#431) of a result written to Parquet come back with
  it when the file is read, as they do from an Arrow file, and `.graph` draws
  the file as it drew the result. DataFusion's default drops every Parquet
  file's metadata; `SET datafusion.execution.parquet.skip_metadata = true`
  restores that.

- **Graph function parameters are range-checked** (behaviour change, #416).
  A number with no meaning for its parameter was accepted and gave a
  plausible-looking result: `pagerank(g, damping => -1)` returned a negative
  PageRank, `max_iter => 0` the uniform start vector, `betweenness(g, sample
  => 2)` exact values and `sample => -1` wrong ones, `louvain(g, resolution
  => -1)` one community. Each is now an error that names the parameter, its
  range and the value: `pagerank(damping =>) must be between 0 and 1; got
  -1`. The ranges: `damping` between 0 and 1; `max_iter` (`pagerank`,
  `label_propagation`) at least 1; `tol` at least 0 (`tol => 0` still runs
  every iteration); `sample` (`betweenness`, `avg_path_length`) above 0 and
  at most 1; `resolution` above 0; and for the layouts `k` and
  `jitter_tolerance` above 0, `gravity` and `theta` at least 0. Every value
  inside its range computes what it did. The ranges are declared once, in
  the function catalog, so `ursa_functions()` has them (a `range` field in
  each parameter, NULL where any value is accepted), `.functions NAME` and
  the function reference show them, and the Python verbs that take these
  parameters (`ur.pagerank`, `ur.betweenness`, `ur.label_propagation`,
  `ur.louvain`) raise the same error, worded `pagerank(damping=) must be
  between 0 and 1; got -1`. Python's `ur.avg_path_length(sample=)` is
  unchanged.
- **SQL: a vertex table's key must identify one row** (behaviour change,
  #411). A vertex table with a duplicate or a NULL key is refused, with one
  message, by everything that reads it as a graph's nodes: the algorithms,
  statistics and traversals, `g.nodes`, and `GRAPH_TABLE` (`graph 'gs':
  vertex table 'v' has 1 row with a duplicate key (id = 1) and 1 row with a
  NULL key; a vertex table's KEY must identify one row. Deduplicate it in a
  view …`). SQL/PGQ requires a unique, non-null key, and DuckPGQ takes it
  from a primary key. Before, the graph was accepted and the three disagreed:
  the algorithms counted a duplicated key once and skipped a NULL key, while
  `g.nodes` and `GRAPH_TABLE` returned every row, so one node was two matches
  and `g.nodes JOIN degree(g)` repeated it; only `neighbors_agg(attribute
  =>)` noticed, with a message about `values =>`. `CREATE PROPERTY GRAPH`
  still accepts the table (it is read when a query runs); queries over it
  now fail until it is deduplicated, for example in a view with `SELECT
  DISTINCT ON (key) *`. An index build counts the rows as it numbers the
  keys; `g.nodes` and a `GRAPH_TABLE` node count the key column when the
  query runs (`EXPLAIN` reads nothing), once per version of a table's
  files: the count is kept for the session and, with a database, recorded
  beside its stored indexes (`indexes/vertex_keys.json`), and a stored index
  built from the same keys stands for it, so a later process, read-only
  included, reads only the files' metadata. A changed file is counted
  again; a table that does not read only files is counted by each query.
  Stored indexes of graphs with a vertex table are
  built again once, under a new key, so none built before the check is
  reused. Not covered: the keys of an edge table between two vertex tables
  are checked where a pattern reads those tables' rows, not in its index.
- **SQL: `GRAPH_TABLE` refuses a node label that contradicts its edge**
  (behaviour change, #410). An edge matches only between the vertex tables
  its edge table `REFERENCES`. `(a:Person)-[k:knows]->(b:School)`, where
  `knows` goes from students to students, joined `know.dst` to
  `School.school_id` on values that happened to be equal and returned rows
  for edges that are not in the graph. It is now an error that names the
  edge table, its vertex tables and the pattern as written. The standard
  gives such a pattern no rows; Ursa refuses it, as DuckPGQ does, since it
  is almost always a mistake, and an empty result would hide it.
- **SQL: `GRAPH_TABLE` refuses text left over after a `WHERE` condition or a
  `COLUMNS` item** (behaviour change, #409). `WHERE a.id = 0 b.id = 3` ran as
  `WHERE a.id = 0`, and `COLUMNS (a.id AS a nonsense, b.id AS b)` returned
  one column: the SQL parser stopped at the first token it could not use and
  the rest was dropped. Both are now syntax errors that name the token and
  its position in the pattern, for the pattern's `WHERE`, a node's or edge's
  `WHERE`, and `COLUMNS`. `KEEP …` and `ONE ROW PER …`, which were ignored
  the same way, are refused as not supported yet. Only queries that were
  already wrong are affected.
- **SQL: more of SQL/PGQ's and DuckPGQ's `GRAPH_TABLE` forms run**
  (#413, #414). Each was an error before, so no query that ran changes.
  - `COLUMNS` may be left out, or written `COLUMNS (*)`: every property of
    each named node and edge, in pattern order (`a.*, k.*, b.*`), as DuckPGQ
    returns them. A repeated name gets a suffix (`id`, `id:1`).
  - `<-[e]->` and `<->`, the standard's "left or right", mean `-[e]-` and
    `-`, since every edge of an Ursa graph is directed. The undirected `~`
    forms are refused: "Ursa's edges are directed; use -[e]- to follow them
    either way".
  - `element_id(a)` of a node is its key; `element_id(e)` of an edge is its
    row in the edge table, the number `edges(p)` lists (for the edge of a
    path with a variable; an edge read by a join is refused for now).
  - Graph predicates in `WHERE` and `COLUMNS`: `SAME(a, b, …)`,
    `ALL_DIFFERENT(a, b, …)`, `x IS [NOT] LABELED l`, and `a IS [NOT]
    SOURCE OF e` / `IS [NOT] DESTINATION OF e`.
  - `KEEP <selector or path mode>` after a pattern of one path is its prefix
    form: `MATCH p = (a)-[e]->*(b) KEEP ANY SHORTEST` is `MATCH p = ANY
    SHORTEST (a)-[e]->*(b)`. `ONE ROW PER MATCH`, the default, is accepted.
  - `CREATE PROPERTY GRAPH`: `REFERENCES t` without a column list names
    `t`'s key, when it is known (its `KEY`, its primary key, or another
    `REFERENCES`).
  - Clearer errors for what is not supported yet, in place of parser
    errors, debug dumps or a wrong variable: `KEEP` over several paths,
    `ONE ROW PER VERTEX` / `STEP`, aggregates in `COLUMNS`, a subquery in
    the pattern that reads a table the statement does not name, `LATERAL
    GRAPH_TABLE` and other references to tables outside the `GRAPH_TABLE`,
    the properties of a quantified edge variable ("stands for several
    edges"), and Cypher's property maps (`{name: 'x'}`) and ranges
    (`*1..3`), each with the SQL/PGQ to write. The label-expression error
    now points at #412.
  - A SQL/PGQ and DuckPGQ conformance suite (#299), `tests/conformance/`,
    in sqllogictest's format, with a reviewed list of known differences;
    `cargo test -p ursa-sql --test conformance` runs it.
- **SQL: a `GRAPH_TABLE` variable can be a row of several element tables**
  (#412). Each form was an error before, so no query that ran changes,
  except one that returned wrong rows (below).
  - A variable without a label is any table of its kind that its edges'
    `REFERENCES` allow: on DuckPGQ's `pg`, `MATCH (a)` is every student and
    school, and `(a:Person)-[e]->(b)` follows both `know` and `studyAt`.
  - A label on several vertex or edge tables (LDBC's `Message` on `Post`
    and `Comment`) matches all of them.
  - Label disjunctions, `(a:Person|School)`, and `%`, any label;
    `x IS LABELED a|b` too. `&`, `!` and parentheses are still refused, with
    a message.
  - The pattern is planned once per combination of tables, and the plans
    are combined with `UNION ALL`; combinations the edges rule out are left
    out. When every variable has one table, the plan is the same as before.
    More than 64 combinations is refused with a request for labels.
  - A property of a variable over several tables reads as NULL in the rows
    of the tables that lack it, as in SQL/PGQ and GQL: `MATCH (a) COLUMNS
    (a.name)` gives the students' names and NULL for the schools. A property
    no table has is an error that names the tables (so a typo is caught),
    and so are tables that have it with types that do not mix (numbers mix
    with numbers, text with text). `a.*` and `COLUMNS (*)` return only the
    properties every table has; name the others to get them.
    `element_id(a)`, `SAME` and `IS LABELED` work per table.
  - A quantified edge, or the edge of a searched path, that can be a row of
    several edge tables is refused: label it.
  - Fixed: an undirected edge between two different vertex tables,
    `(a:Person)-[s:studyAt]-(b:School)`, also joined the reverse
    orientation on key values that happened to be equal, and returned rows
    for edges that are not in the graph. It now follows the one way its edge
    table goes.

- **Triangle counting uses about a third of the CPU, with the same counts**
  (#420). `triangle_count`, and so `clustering_coefficient` and the SQL
  functions over them, was the slowest step on the full Bluesky graph (1,828 s
  on 16 cores). The kernel still orients each edge toward its higher-ranked
  end, by `(degree, id)`, and finds each triangle once. Now the nodes are
  numbered by rank and each list holds ranks, so for an edge `u → v` only the
  entries of `u`'s list after `v` are read; a list of 32 entries or more is
  marked once in a per-thread position table and the lists it meets are looked
  up there; of two lists, one 8 times the other's length is searched, not
  merged; and each node's credits are added once per list entry, not once per
  triangle, so threads no longer write the hubs' counters all the time.
  Per-node counts are bit for bit the old kernel's: a new test pins them on
  ten graphs (hubs, a clique with leaves, R-MAT, self-loops, parallel and
  reciprocal edges, vertices with no edge, masks), recorded before the change,
  and the old kernel is kept in the tests as a reference. On R-MAT graphs on a
  shared 4-core machine: 16M edges (384M triangles) 35.3 to 12.6 CPU-s, 64M
  edges (6.19B triangles) 376 to 120 CPU-s. Nodes of degree under 2 get no rank, so the
  pass costs the same among 16M vertices with no edge (38.0 to 13.9 CPU-s).
  It adds 4 bytes per node of degree 2 or more, plus up to as much again per
  thread for the position tables.
- **Betweenness with a small sample holds half the scratch memory** (#454).
  With fewer than 64 sources per thread, the kernel ran `2 × threads` sources
  at a time, each with its own state of 24 bytes per node (more when
  weighted), all kept for the whole run: about 30 GB at Bluesky's size on 16
  threads. It now runs `threads` at a time, which keeps every thread as busy.
  Results are bit for bit the same (each node's sum is taken in source order
  whatever the window; the reference-kernel and thread-count tests cover it).
  On a 32M-edge, 2.1M-node R-MAT graph with 105 sources on 4 threads: peak
  RSS 1,019 MB to 814 MB (the graph alone is 576 MB), time unchanged (61 s
  and 60 s, within the noise of a shared machine).
- **Unweighted closeness uses a sixth of the CPU or less, with the same
  results** (#422). `closeness` ran one breadth-first search per node. It now
  runs 64 sources per pass, one bit of a 64-bit word per source (multi-source
  BFS, Then et al., VLDB 2014), so a node's adjacency list is read once per
  level on which any of the 64 searches reaches it, not once per search. Each
  source's distance sum and reach count are the same integers as before, so
  every score is the same `f64`, bit for bit, in every direction and under an
  edge mask: a new test pins golden hashes recorded before the change (karate,
  R-MAT, a disconnected graph with isolated nodes, a sparse directed graph, a
  long path; masked and not) and compares with a per-source search, and the
  thread-count test now covers unweighted closeness. Weighted closeness
  (Dijkstra) is unchanged. On R-MAT graphs of 65,536 nodes on a shared 4-core
  machine (best of two runs):

  | graph | before | now |
  |---|---|---|
  | 1M edges, `out` | 38.3 s, 119 CPU-s | 6.4 s, 19.8 CPU-s |
  | 1M edges, `in` | 62.8 s, 123 CPU-s | 14.0 s, 20.5 CPU-s |
  | 1M edges, `both` | 87.5 s, 309 CPU-s | 13.0 s, 34.9 CPU-s |
  | 2M edges, `out` | 92.6 s, 213 CPU-s | 15.9 s, 29.5 CPU-s |

  Each thread holds 16 bytes per node (was 4), plus its frontier list.
- **Unweighted Louvain holds about half the memory, with the same
  communities** (#450). An unweighted run's level-0 working graph stored an
  8-byte `f64` per adjacency slot for a weight that counts parallel edges
  (1, or 2 for a reciprocal pair). It now stores that count in one byte, 5
  bytes a slot instead of 12; a graph with more than 255 parallel edges
  between two nodes, a weighted run, and every aggregated level keep `f64`s.
  Local moving (serial and `parallel => true`) and the aggregation read
  either store through the same code, compiled once for each. Results are
  unchanged, bit for bit. On R-MAT graphs (4 shared cores), Louvain's peak
  heap went from 488 to 227 MB at 20M edges and from 1,295 to 630 MB at 50M
  (peak RSS 1.72 → 1.11 GB), serial or parallel, at the same speed or
  slightly faster; weighted runs are unchanged.
- **SQL: a stored graph index is memory-mapped, not read whole** (#382).
  Loading one read the whole file on one core, so at full scale (2.42B
  edges, an 18.6 GiB index) every query in a new process paid 15 s, even a
  `hop` that touches a few thousand edges. Now the index file is mapped
  (Linux, macOS): a traversal or path search reads only the adjacency lists
  it visits, and an algorithm over the whole graph reads the file at the file
  cache's speed, on all cores. On a 250M-edge graph (a 1.9 GiB index), a
  1-step `hop` in a new process takes 0.16 s (was 6.6 s) and 0.2 GB of
  memory (was 2.4 GB); `degree` 0.33 s (was 6.7 s); PageRank 2.8 s (was
  18 s). Results and node numbering are unchanged. On Windows, which cannot
  replace a mapped file, the index is read whole as before, but on all
  cores.
  - **Index file format version 3.** The layout is version 2's (raw
    little-endian arrays at 64-byte-aligned offsets); each section now has
    its own CRC-32 in the section table, and the trailer's CRC covers the
    header, key and table. Loading checks those and the per-node sections
    (offsets, node ids) at once; the edge arrays' CRCs are checked, in
    parallel, before a whole-graph algorithm runs, and a mismatch drops the
    stored index and builds it again, with a notice. A traversal relies on the
    checked offsets and the kernels' bounds checks instead.
  - **Indexes stored by 0.10 keep loading** (read whole and checked, as
    before), and the first writer to load one rewrites it as version 3, with
    a notice (`rewriting the stored index for g in club.ursa (…) in the
    current format: …`). Version 1 files are still built again.
  - A stored index is still never changed in place (a new one is renamed
    over it, an eviction removes it, a chunk is only appended), so a process
    keeps reading the index it mapped while another process replaces it.
    Mapped pages count in a process's RSS once read, as shared, reclaimable
    file cache.
- **SQL: a new process no longer hashes every node id of a stored index
  before its first query** (#421). Loading a stored index rebuilt the
  id-to-node hash table from the stored ids, one id at a time on one core:
  0.4 s for 2M string ids, 5.5 s for 20M, in every new process, even for a
  `hop` from one seed, which undid most of the gain of mapping the file
  (#382). Now the table is stored in the index file and used as it lies
  there, and the id column is borrowed from the mapped file instead of
  copied to the heap. On a graph of 20M nodes with 32-byte string ids and
  30M edges (4 shared cores), in a new process each time: a 1-step `hop`
  from one seed takes 0.29 s (was 7.0 s) and 1.06 GB at peak (was 1.80 GB);
  `count(*) FROM degree(g)` 0.57 s (was 6.95 s); PageRank 22.6 s (was
  32.5 s) and 3.21 GB (was 3.95 GB). What a load still reads is the
  per-node sections, to check their CRCs and that the ids are UTF-8, on all
  cores. Results and node numbering are unchanged.
  - **Index file format version 4**: version 3 plus one section, the ids'
    lookup table (an open-addressing table of `u32` node indexes, 6 bytes a
    node; its layout and hashes are fixed and documented in
    `ursa_core::id_map`). The file of the graph above grows from 1.52 GB to
    1.64 GB, and building and storing its index takes as long as before.
  - **Indexes stored by 0.11 (version 3) and 0.10 (version 2) keep
    loading.** They have no table, so one is built in memory, but only when
    a query first looks a node up by its id (seeds, sources, a join on ids):
    an algorithm over the whole graph never builds it, and a `hop` on the
    version 3 file of the graph above takes 2.1 s. The first writer to load
    such a file rewrites it as version 4, with the notice `rewriting the
    stored index for g in club.ursa (…) in the current format: …`.
  - `ursa_core::IdMap::from_stored_ids` and `IdMap::lookup_table` are the
    two halves of this; `IdMap::from_user_ids` still interns eagerly.
- **SQL: `GRAPH_TABLE` path searches are about twice as fast from one start
  node, and run in parallel from several, with the same rows** (#451). The
  search copied each node's adjacency list (two lists, merged into a third,
  for an undirected edge) before reading it, and looked up and counted a
  per-edge state, through a `match` on how the states were stored, before
  the cheap test that rejects most edges. Now it reads the lists where they
  lie (an undirected step merges the two as it reads them), tests first, and
  is compiled once per kind of state table, with the loop's branches decided
  per search. `ANY SHORTEST` keeps one bit per state instead of 8 bytes, so
  its table is dense at any size; the other tables stay dense up to 16 bytes
  a node of the graph (or 512 MiB) instead of a fixed 2^26 states, and the
  map past that, the target slots and the per-target counts use FxHash
  instead of SipHash. Each query no longer copies the index's whole id column
  to build its result (`concat` of the starts and every node's id): a
  point query on a stored index reads only the ids of the nodes its paths
  pass. From several start nodes, the searches run on all cores, one walker
  per thread, each with its own state table (at most 1 GiB of them
  together), and their rows are appended in start order; when
  `ursa.max_paths` stops the query, and with which error, is what searching
  one start after another gives (each start is checked against the exact
  count of the paths before it). Rows, their order and the `expanded`
  metric are bit for bit as before: a new test pins every row of a matrix
  of searches (directions, selectors, modes, bounds, start sets, targets,
  limits, four graphs), recorded before the change, on one thread and on
  four, and another the rows of SQL path queries from a built and a stored
  index. On R-MAT graphs with a stored index, in a new process each time
  (wall, shared 4-core machine): from node 0, `ANY SHORTEST ->{1,3}`
  0.88 → 0.44 s at 32M edges, `ANY SHORTEST ->*` 0.87 → 0.39 s (with
  `vertices(p)` 0.98 → 0.55 s), undirected `{1,2}` 1.22 → 0.62 s; one pair
  0.10 → 0.03 s with 30-byte string ids; from 16 start nodes, `{1,3}`, 16.4M
  paths, 12.7 → 2.2 s (12.7 → 5.5 CPU-s); 64 starts that reach the limit
  fail after 7.6 → 1.1 s. Peak memory is about the same.
- **SQL: an index build interns a vertex table's keys while it interns the
  edges, not before them** (#385, N49). With the edge scan parallel (#351),
  reading the keys first was the serial part of a build with a vertex table:
  a `.csv.gz` decompresses on one core, and the edges waited for it. Now the
  keys and the edges are read and interned at once, on threads of their
  own: the edges with a provisional numbering, until the last key is in;
  then those are renumbered into the keys' numbering (one lookup per
  distinct endpoint, then in place, in parallel) and the rest go straight
  into it. The node numbering, the edges kept and the stored index are byte
  for byte the ones interning the keys first gives, wherever the switch
  falls; a test checks it with the keys in CSV, gzipped CSV and Parquet. On
  4 cores, 96M edges behind a filter over 160M rows of zstd Parquet and a
  40M-key vertex table: 19.8 s → 18.6 s with the keys in a 1.1 GiB
  `.csv.gz` of three columns (its single-core decompression, about 14 s, is
  now most of the build); a small `.csv.gz` or Parquet of the keys is as
  before, within noise. The browser still reads the keys first.
- **SQL: the build's read-ahead is a setting, scaled and capped**
  (#385, N45). The partitions an index build decodes ahead of the one it is
  interning held up to a fixed 1 GiB, which raised a build's peak memory by
  about 0.7 GB. It is now `ursa.build_read_ahead`: `auto` by default, 256 MiB
  per partition of the source (so 1 GiB from 4 partitions up, as before), at
  most an eighth of the memory available to the process (`MemAvailable`, or
  the cgroup's limit if lower), or a size `SET` for the session; `0` reads
  the partitions one after another. A smaller read-ahead costs time: on the
  96M-edge build above (4 cores), 1 GiB took 8.6 s at 4.2 GB peak RSS, 512
  MiB 9.5 s at 3.8 GB, 256 MiB 9.7 s at 3.4 GB, and `0` 10.4 s at 3.1 GB.
  The index is the same under any value.
- **Weak and strong connected components run in parallel, with the same
  labels** (#383). Both kernels ran on about one core (63 s and 82 s on the
  full Bluesky graph, on 16 cores). Weak components now find, per thread and
  range of edges, the edges that join two union-find sets, and replay only
  those through the serial union-find, so every root is the one the serial
  kernel picks. Strong components are found in parallel by trimming and
  colouring rounds, over out-edges only (no transpose), and a serial DFS in
  Tarjan's order, testing one bit per edge, then names each by Tarjan's root.
  Labels are bit for bit the serial kernels', which tests pin per node and
  compare at several pool sizes. On a 256M-edge R-MAT graph on a shared
  4-core machine: weak 10.4 s to 6.3 s, strong 21.1 s to 13.8 s (the strong
  kernel's serial DFS takes 0.25 s of it), with 120 MiB more heap at most
  for weak and 70 MiB less for strong, on a 2 GiB graph. The serial kernels
  still run on one thread, below 65,536 edges, and without the `rayon`
  feature. `Linked::of`, run before every compact kernel, also no longer
  makes an atomic update per edge.
- **SQL: a database always opens** (#381, N44). A catalog statement that
  fails when the database opens, such as a view over a data file that was
  moved, renamed or deleted, or a database copied without its files, no
  longer makes the database unopenable (`cannot open m.ursa: statement 1 of
  its catalog failed`), with every statement, `DROP` included, failing the
  same way. The object it defines is unavailable instead, and one notice
  says why: `ursa: view v is unavailable: no file found at '…/t.csv'; DROP
  VIEW v or restore the file`. Everything else works, in a writable or a
  read-only open; `SHOW TABLES` and `.tables` still list it, and using it
  fails with that message, as a broken view does in DuckDB. A view or graph
  that uses an unavailable object is unavailable too, with its own notice.
  `DROP` (with or without `IF EXISTS`) and `CREATE OR REPLACE` repair it.
  Its statement stays in the catalog as it was, so restoring the file and
  opening the database again brings it back.
- **SQL: `SHOW ursa.index_storage_limit` shows just the value** (behaviour
  change, #381, N46), `21GiB` or `8GiB`, like every other setting, not
  `21GiB (the default: …)`. Where it comes from starts its description,
  which `SHOW ursa.index_storage_limit VERBOSE` and `SHOW ALL VERBOSE` show.
  The default limit is rounded down to a multiple of 4 GiB (it was a whole
  GiB), so it no longer moves between runs as the free space changes a
  little.
- **SQL: storing an index's reverse adjacency is said** (#381, N47). When
  the database closes and appends the reverse adjacency (or evaluated
  weights) to a stored index, `ursa.notices = 'all'` says `stored the
  reverse adjacency of g in club.ursa (+1.0 GiB, 2.3 GiB in all)`, and
  `'on'` says it too when it brings the stored indexes to 90% of
  `ursa.index_storage_limit` or more. A chunk that cannot be written is
  said at `'on'`.
- **`ursa`: JSON and NDJSON write `null` for a null** (behaviour change,
  #350). Every row object has every column, in column order, as DuckDB
  writes them: `SELECT NULL AS a, '' AS b` is `[{"a":null,"b":""}]`, which
  was `[{"b":""}]`, so a missing key no longer stands for a null. The
  browser shell's JSON export does the same.
- **`ursa`: CSV and TSV write an empty string as `""`** (behaviour change,
  #350), as DuckDB does, so it can be told from a null, which is still an
  empty field: `SELECT NULL AS a, '' AS b` is `,""`, which was `,`. The
  browser shell's CSV export does the same.
- **`ursa`: a string inside a list, struct or map is quoted in its text**
  (behaviour change, #386), as in DuckDB, so different values print
  differently in CSV, TSV and the table: `['a, b', 'c']` is `['a, b', c]`
  and `['a', 'b', 'c']` is `[a, b, c]`, which both were `[a, b, c]`, and
  `['x]', NULL, 'NULL']` is `['x]', NULL, 'NULL']`, which was
  `[x], NULL, NULL]`. A string is quoted when it is empty, is `NULL` in any
  case, starts or ends with whitespace, or holds any of
  `' " \ , : = [ ] { } ( )`; a `'` or `\` in it is escaped with `\`. Struct
  field names and map keys follow the same rule, and a null inside a nested
  value is `NULL` in the table too. The browser shell's table and CSV export
  do the same. JSON stays the lossless format for nested results.
- **SQL: an index build decodes its source's partitions in parallel**
  (#351, N30). The build read its edge plan one partition at a time to keep
  the order that numbers the nodes, so a filtered view over a large Parquet
  file spent its build in zstd decompression on one core while the other
  scan workers waited. Each partition now runs as its own task, and its
  batches are interned in partition order, so the node numbering, the CSR and
  the stored index are byte for byte the ones a serial read gives (a test
  checks it over a multi-partition, multi-row-group source). Partitions ahead
  of the one being interned hold at most 1 GiB of decoded batches between
  them. The vertex keys, and the weights read beside an index, are read the
  same way. On a synthetic 120M-row zstd Parquet (1M-row row groups) behind a
  filter no statistics prune (`t < 5000`, 5% kept, 6M edges), on 4 cores:
  3.66 s → 1.55 s, against 1.2 s for the parallel SQL scan of the same view.
  `cargo run --release -p ursa-engine --example filtered_build` measures it.
- **SQL: the build notice no longer guesses at a filtered view's edges**
  (#351, N29). Over a filter the statistics cannot settle, DataFusion's
  estimate is its default 20% selectivity, so a view keeping 1K of 1M rows
  was announced as `about 200.0K edges`, and Bluesky's 95.7M-edge view as
  2.4B. Such a build now says what it reads: `reading about 1.0M rows to
  build the index for g2…`; an edge count is given only when the statistics
  have one, exact or unfiltered. The `built` notice gives the real count, as
  before. `ursa_engine::graph::BuildInfo` holds a `BuildSize` in place of
  its `edges` pair.
- **SQL: `EXPLAIN` marks a path search over a held index** (#351, N38).
  `PathSearchExec: graph=g, search=…, index=stored (sources not read)` (or
  `index=cached`, for one already in the session) says that the edge and
  vertex scans drawn under it will not run, where the plan read as if the
  index were not used.
- **SQL: `EXPLAIN` no longer draws the scans a held index skips** (#384,
  N48). Under a path search whose index the session holds, the edge and
  vertex scans are now counted on its line rather than drawn as its
  children: `PathSearchExec: graph=g, search=…, index=stored, sources: 2
  files, not read`. A path search over an index not held, and `EXPLAIN
  ANALYZE`, are as before.
- **SQL: `ANY SHORTEST` counts the paths it keeps, and the path limit is a
  setting, `ursa.max_paths`** (#384, N50). A path search stopped at a fixed
  10M paths, counting every partial path it held, and told an `ANY
  SHORTEST` query to use `ANY SHORTEST`: from one well-followed Bluesky
  account, the `{1,3}` histogram failed, where `hop` answers at once. Under
  `ANY SHORTEST` (and `SHORTEST k`), whose search holds at most one (`k`)
  partial path per node, the limit now counts the paths kept, never fewer
  than one (`k`) per node of the graph, so one start node is never refused.
  The limit is `SET ursa.max_paths = …` (default 10000000; 0 for none), and
  the error names it, with the remedy that fits the query: a lower upper
  bound or `ANY SHORTEST` without a selector, `hop(…)` with one.
- **Isolated vertices cost the algorithms next to nothing** (#326). A vertex
  table's keys with no edges are nodes, and on real data they can be most of
  them (37.2M of the Bluesky follows graph's 39.65M). `pagerank`, `louvain`,
  `label_propagation`, `connected_components` (weak and strong) and the
  triangle pass behind `triangle_count` and `clustering_coefficient` now run
  on the nodes with at least one edge, and fill in each isolated node's
  result, which is closed-form: its own component or community, no
  triangles, and for PageRank its share of the teleport and of the dangling
  score (an isolated node is dangling too). Results are unchanged, bit for
  bit, label values and order included: PageRank carries every isolated node
  as one value and adds it into its sums where a run over every node does,
  and the community kernels visit the other nodes in the order their seeded
  permutation of every node gives. The Python library shares the kernels, so
  a subgraph (a filtered edge frame) whose nodes lose every edge gains too.
  On an R-MAT graph of 5M edges with its 180K nodes spread among 20M (99%
  isolated), the kernels' time and peak memory, before → after: PageRank
  4.84 s / 828 MB → 0.44 s / 194 MB, Louvain 18.1 s / 2,342 MB → 2.6 s /
  283 MB, label propagation 4.8 s / 240 MB → 1.3 s / 169 MB, weak
  components 180 MB → 88 MB, triangle count 258 MB → 105 MB; the memory left
  is mostly the result, one value per node (0.06 s / 5 MB and 1.3 s /
  276 MB for PageRank and Louvain without the isolated nodes). What remains
  per isolated node is the result and, for the community kernels, the seeded
  permutation of every node, whose order they keep. `cargo run --release -p
  ursa-core --example isolated_vertices` measures the kernels, and
  `benchmarks/sql_scale/isolated.py` the `ursa` binary end to end. The function
  reference says what isolated nodes still change: PageRank's scores sum to
  1 over every node, so more keys give lower scores, and each isolated node
  is a community, so `count(DISTINCT louvain)` counts them; count
  communities of more than one member with `GROUP BY louvain HAVING
  count(*) > 1`.
- **Louvain builds its working graph in less memory** (#352). The graph was
  scattered into padded `(u32, f64)` pairs, 16 bytes an entry, and copied
  out while they were still held, so building it peaked at about the sum of
  two copies of the adjacency (5.9 GB on the Bluesky follows graph, 95.7M
  edges). Now the scatter buffers become the final arrays, compacted in
  place; targets and weights are separate arrays (12 bytes an entry); and an
  unweighted run scatters 4-byte targets and counts its parallel edges for
  their weights. Results are unchanged, bit for bit. On an R-MAT graph of
  100M edges (4.3M nodes), Louvain's peak heap went from 5,734 MB to
  3,704 MB unweighted and to 4,928 MB weighted (peak RSS 6.50 → 4.08 GB and
  7.28 → 5.57 GB); the building step itself fell from about 5.5 GB to about
  2.4 GB, and the peak is now aggregating the first level. `cargo run
  --release -p ursa-core --example louvain_memory` measures it.
- **Louvain aggregates its first level in less memory** (#359). Collapsing
  each community into a super-node ran while the level-0 graph was held, and
  grew its output in per-block vectors that were then concatenated. It now
  counts each community's neighbouring communities first and writes the
  aggregated graph straight into arrays of its exact size. And when every
  weight is a whole number (any unweighted run, or integer weights), each
  node's edges are first collapsed in place to one entry per neighbouring
  community and the level-0 arrays shrunk, before the aggregated graph is
  allocated: every sum there is exact, so the regrouping changes no bit.
  Results are unchanged, bit for bit. On an R-MAT graph of 100M edges
  (4.3M nodes), Louvain's peak heap went from 3,704 MB to 2,594 MB
  unweighted and from 4,928 MB to 3,757 MB weighted, and peak RSS from 4.13
  to 3.42 GB and from 5.50 to 5.36 GB, at the same speed. (Much of the old
  vectors' slack was never written, so it was never resident: RSS falls
  less than the heap.) Unweighted, the peak is now local moving on the
  level-0 graph; weighted with inexact weights, it is still the first
  aggregation, now holding just the level-0 and level-1 graphs.
- **SQL: `hop` returns `(seed, id, distance)`** (breaking, #328). It
  returned `(src, dst)`, but its rows are nodes reached from a seed, not
  edges: `seed` is the seed, `id` a node reached and `distance` (Int64) the
  fewest steps from the seed to it. Rename `src` to `seed` and `dst` to `id`
  in queries over `hop`. The Python library's `ur.hop` is unchanged: it still
  returns an `EdgeFrame` of `(src, dst)`.
- **SQL: notices about stored indexes and slow builds** (#327). The engine
  says, one line each, what it does on its own that a user waits for or
  loses; `ursa` prints them on stderr as `ursa: …`, so stdout holds only
  results. At `ursa.notices = 'on'` (the default): an index build that runs
  for more than a second (`building the index for g (95.7M edges)…`, then
  `built … in 30.7 s; stored in club.ursa (1.4 GiB)`), and every index not
  stored (larger than `ursa.index_storage_limit`, with the `SET` that keeps
  it; or the database is read-only), evicted, or whose write failed. `'all'`
  adds every build, load and store; `'off'` and `ursa --quiet` (`-q`) print
  none.
- **SQL: the lock error names the other process** and suggests
  `ursa --readonly` to query meanwhile (#327).

### Changed

- **SQL: `SET` lasts for the session** (behaviour change, #353; reverses
  #327's persistence). `SET` and `RESET` change only the current session, in
  any database, writable or read-only, as in DuckDB, Postgres and
  DataFusion: nothing is written to the catalog. Since 0.8, `SET
  ursa.index_storage_limit = '50MiB'` in a writable database was saved for
  good, and a limit tried "for now" evicted gigabytes of indexes at the next
  build. To keep a setting, put the `SET` in `~/.ursarc` (the shell) or an
  `-init` file (scripts too). A limit 0.8 or 0.9 saved is no longer applied:
  opening the database says so once (`ursa: club.ursa has a saved
  ursa.index_storage_limit = 50MiB, which is no longer applied; …`), and a
  writable open removes it from the catalog.
  - **Lowering the limit never removes a stored index.** A limit decides
    only whether a new index is stored; eviction makes room for a new index
    under the session's limit, least recently used first, never more than it
    needs. If the stored indexes already take more than a lower limit, no new
    index is stored and nothing is evicted, and the `SET` says so (`ursa:
    3.8 GiB of indexes are stored in club.ursa, more than the 50MiB limit;
    they are kept, and new indexes won't be stored`).
  - **The default limit follows the disk** (N32): 25% of the free space on
    the database's disk, counting the indexes it stores, in whole GiB, at
    least 4 GiB (the old default) and at most 64 GiB. At 4 GiB, three graphs
    over the same 95.7M edges evicted each other, each eviction costing a
    30 s rebuild later. The setting's value is `auto` until `SET`, and
    `SHOW ursa.index_storage_limit` says the limit in effect and where it
    comes from (`8GiB (SET for this session)`, or `25GiB (the default: 25%
    of 100.2 GiB: …)`).
  - The `stored` notice says how the index grows once an algorithm adds its
    reverse adjacency (about 1.75×): `stored in club.ursa (768.0 MiB; about
    1.5 GiB once an algorithm adds the reverse adjacency)`, or `(…, with its
    reverse adjacency)` when it is already in it.
- **SQL: `ursa.index_storage_limit` belongs to the database** (behaviour
  change, #327; its persistence is reversed by #353 above, and `SET` now
  lasts for the session). `SET` in a writable database is stored in the catalog
  (a new `settings` object; older versions ignore it), applied when the
  database is opened and shown by `SHOW`; `RESET` goes back to the default.
  Stored indexes obey only the stored limit or the default: a read-only or
  in-memory session's `SET` lasts for that session and never evicts, so a
  temporary small limit can no longer delete a large stored index. Eviction
  runs only to make room for an index being stored, least recently used
  first, evicting no more than it must and never an index a running query
  uses; if those leave no room, nothing is evicted and the new index is not
  stored.
- **SQL: a portable stored-index key** (#327). Paths inside the database's
  directory are part of the key relative to it, so a database moved with its
  data keeps its stored indexes; a file named in `FROM` is keyed by what it
  reads rather than the name it was written as, so a view defined in this
  process and the same view replayed later share an index; and only the file
  options that change what is read are in it, not DataFusion's `Debug` text
  of every listing option, so an unrelated setting or a DataFusion upgrade no
  longer invalidates every index. The manifest is version 2: indexes stored
  by earlier versions are removed once by the first writer (with a notice)
  and built again when first used, never read under the old key.

- **SQL: a vertex table defines the graph's node set** (behaviour change,
  #311). With `VERTEX TABLES`, every algorithm, statistic, traversal,
  `neighbors_agg` and `GRAPH_TABLE` path search runs on the vertex table's
  keys, as SQL/PGQ and DuckPGQ do: a node on no edge is a node (degree 0, a
  row in every result, a seed that reaches nothing), and a subgraph over a
  filtered edge view keeps every vertex, as a filtered edge frame in Python
  keeps its parent's nodes (parity tests cover it). Before, the algorithms saw
  only the edges' endpoints, so isolated vertices were missing and
  `describe(g, nodes => 'all')` and `'touched'` agreed. An edge whose source or
  destination is not a key is now left out, as DuckPGQ's inner join leaves it
  out; `describe` reports how many in a new column, `n_dropped_edges` (0
  without a vertex table). `g.nodes` lists the vertex table's rows only (it
  used to add such endpoints with NULL attributes); `g.edges` is still every
  row. Graphs without a vertex table are unchanged. The vertex keys are read
  as one column, streamed and interned before the edges, and are part of a
  stored index's key, so changing the vertex table rebuilds the index;
  indexes stored before this change stay valid for graphs without a vertex
  table.
- **Behaviour change (SQL):** `neighbors_agg`'s `count` and `n_unique`
  return `UInt32`, as `degree` does, not `Float64`. `min` and `max` take
  strings, dates, times and timestamps too, and return a value of the same
  type (#313). The Python library is unchanged.
- **The crates `ursa-cli` installs from crates.io** (`ursa-core`,
  `ursa-io`, `ursa-engine`, `ursa-sql`, `ursa-cli`, and since 0.9.0
  `ursa-shell`) are released together at one version, since 0.7.0.
  `ursa-core` and `ursa-io` no longer follow the Python package's version,
  and the release workflow refuses a tag where any of them differ. A
  development build's `ursa --version` names its commit, for example
  `ursa 0.7.0-dev (a1b2c3d4e)`. Releases publish to crates.io by
  Trusted Publishing, with no stored API token.
- `ursa-cli` releases can start from GitHub: Actions -> Release ursa-cli ->
  Run workflow on `main`, with `release` and the version. The run builds and
  tests every target, then tags the commit it built; publishing still waits
  for the maintainer's approval. Pushing a tag works as before.
- **Stable and nightly channels for `ursa-cli`** (#323, after 0.8.0). A stable release now
  starts by itself when a release PR is merged: the merge to `main` that sets
  a new release version builds every target, tags the commit, publishes to
  crates.io with no approval step, and makes the GitHub Release. Every merge
  to `main` at a `-dev` version also refreshes one rolling GitHub prerelease,
  `nightly`, with binaries only (nothing on crates.io). Install it with
  `curl -fsSL https://cldixon.github.io/ursa/install.sh | URSA_CHANNEL=nightly sh`
  (or `sh -s -- --nightly`), or `$env:URSA_CHANNEL = 'nightly'` before the
  PowerShell installer. The default stays stable. A nightly's
  `ursa --version` names its version, date and commit, for example
  `ursa 0.9.0-nightly.20261107 (a1b2c3d4e)`.
- **Behaviour change:** `.functions` lists each graph function's name,
  family and the first sentence of its summary; before, it printed every
  column of `ursa_functions()`, too wide to read. A statement that returns
  rows prints its
  header even with no rows, and DDL and `SET` print nothing; a test now
  holds every kind of statement to that (#312).
- `CREATE PROPERTY GRAPH` errors put their hint after the main clause:
  "multi-column keys are not supported yet at Line: 2, Column: 36; KEY,
  SOURCE KEY, DESTINATION KEY and REFERENCES take one column". Before, the
  hint sat in parentheses in the middle of the sentence (#312).
- A weighted algorithm over `scan_edges` reads the file as a stream. It
  builds the index and evaluates the weights one batch at a time, so the
  rows are never all held in memory. Before, it collected every row first.
  Weighted PageRank on a 20M-edge R-MAT graph now peaks at 615 MB, down
  from about 1,040 MB; the SQL engine peaks at 589 MB. Results are
  bit-identical (#267).
- The node-id rules for an in-memory table now run in Rust, in
  `ursa_io::ids`, the same code a scan and the SQL engine use. These rules
  are: which column types hold ids and what they canonicalize to,
  `string_view` columns read as strings, and the rows `on_null="drop"`
  removes. Before, `python/ursa/_io.py` held its own copy. Errors, warnings
  and drop counts are unchanged. With `on_null="drop"` and no null
  endpoints, the table's columns are no longer copied (#269).
- Source reading moved out of the `ursa-plan` crate into a new workspace
  crate, `ursa-io`: Parquet and CSV scans, the CSV options and type
  inference, the Parquet statistics guard, object-store and URL handling, and
  building a topology from a scan. It does not depend on `ursa-plan`, so any
  engine built on Ursa reads files as the DataFrame library does.
  `ursa-plan` depends on it and re-exports what it used, so nothing else
  changes: the Python API, results and error messages are the same (#233).
- The graph functions' specs and dispatch moved into `ursa-core`, as
  `ursa_core::catalog`: each algorithm's, layout's, neighbour aggregation's
  and statistic's parameters, defaults, accepted values, directions, output
  columns and NetworkX equivalent, the validation of a call's arguments, and
  the kernel dispatch (with the work sharing between `triangle_count` and
  `clustering_coefficient`, the symmetric views and a layout's two columns).
  `ursa-plan` resolves and runs every graph column through it, so the
  DataFrame library and the SQL engine share one implementation. The Python
  API, results, row order and error messages are unchanged (#236).
- Upgraded DataFusion 54 → 55.2, arrow, `arrow-pyarrow` and parquet 58 → 59.3
  and pyo3 0.28 → 0.29 (object_store stays on 0.13, the line DataFusion 55
  uses). Results, row order and error messages are unchanged. Building from
  source now needs Rust 1.94 or later (DataFusion 55's minimum; was 1.88).
- **Behaviour change:** `shortest_path` takes one node id for `source` and
  one for `target`. A list, tuple, array or other sequence of several ids (or
  of none) raises `ValueError` where `shortest_path` is called, naming the
  parameter and how many ids it got: `shortest_path(source=) must be one node
  id; got 2 ids. …`. It raised `NotImplementedError: seed ids must be all
  integers or all strings.` at `collect()`. A sequence of exactly one id is
  that id, as the SQL engine reads it; it raised the same
  `NotImplementedError` (#252).
- **Behaviour change:** a traversal's node ids (`hop(...).from_()`,
  `random_walk(start=)`, `shortest_path(source=, target=)`) are integers or
  strings, as in SQL. A float, a bool, a date or any other value raises
  `TypeError` where the traversal is called (`hop(...).from_() must be node
  ids, integers or strings; got 1.5`); it was converted with `int()`, so `1.5`
  ran as `1` and `True` as `1`. Ids are fitted to the graph's id type when the
  plan runs: on a graph of integer ids, a string that spells an integer
  (`'7'`) is that integer, where a list of only strings raised; on a graph of
  string ids, an integer is its string, where it raised. A string that is not
  an integer, on a graph of integer ids, raises `ComputeError: the node id
  'a' is not an integer, but the graph's node ids are int64; …`, which said
  `the node ids given are string but the graph's node ids are int64; …` (#271).

### Fixed

- **Web shell: bold and italic text is spaced as the rest, wherever the
  fonts load late** (URSA-18). The page waited for IBM Plex Mono's regular
  face before opening the terminal, but not its bold or italic. xterm's DOM
  renderer measures each character once per face and keeps the width, so a
  bold character first drawn while the bold face was still loading (the
  prompt's `memory U`) kept a fallback font's width after it loaded: on a
  machine whose fallback measures differently, those characters were drawn
  in spans of their own with letter-spacing, out of line, and a later
  `.tables` broke apart at its `e`. That is how the console's editor suite
  failed on CI ("each kind looks as the shell's terminal draws it": no
  `command`), and it reproduces locally with a fallback font of another
  width. The page now loads the bold and italic faces first too.
  `tests/console-editor.ts` reads the shell's colours once its highlighter
  has answered and the terminal has drawn them, not after a fixed 200 ms;
  `tests/console-results.ts` reads the PageRank grid once its first rows are
  drawn (a frame after its tab), not as the tab appears.
- **Console: the editor's Tab takes a completion list just opened, and a
  dot-command inserted as a statement gets no `;`** (URSA-21). CodeMirror's
  `acceptCompletion` declines for 75 ms after the list opens (its
  `interactionDelay`, so that Enter pressed as a list appears makes a new
  line), and a Tab pressed in that window fell through to indenting the
  line: `tests/console-editor.ts` failed at "Tab accepts it" whenever it
  pressed Tab that soon. Tab now takes the list whenever it is shown, as
  soon as CodeMirror allows, unless the text, the cursor or the list changes
  first. `ursaConsole.editor.insert(".graph karate", {statement: true})`
  gave `.graph karate;`; a dot-command now gets no `;` (the Catalog panel's
  graphs).
- **`ursa serve`: `cancel`'s reply always comes before its target's
  `cancelled`.** The server told the target to stop before handing `cancel`'s
  own reply to the writer, so the target's `cancelled` could be written first
  (`ursa-server/tests/protocol.rs`, `cancel_stops_a_running_request`, failed in
  CI). The reply is now queued first, as the protocol says.
- **`ursa serve` no longer panics on Ctrl+C** (URSA-22). It sometimes
  exited 101 (a panic: `JoinHandle polled after completion`) instead of
  stopping with "Stopping.": when a listener ended between the two polls of
  the shutdown's `select!`, which raced "asked to stop" against "a
  listener ended", the listener's task was then awaited a second time.
  `ursa-server` now keeps its listeners in a `JoinSet`, which hands each
  result over once. `ursa serve` also installs its Ctrl+C handler first, so
  a Ctrl+C while it starts stops it as soon as it listens rather than
  ending it by SIGINT's default, and its own messages no longer panic on a
  closed stdout (which left the server running after Ctrl+C). The test of
  Ctrl+C now shows the server's stderr when it fails, and new tests stop
  the server 50 times each right after its link and during a running
  request, and 100 times in process (`ursa-server/tests/protocol.rs`).

- **Web Shell: the aggregated view's e2e checks wait for the cut's frame**
  (URSA-19). A cut through the layout's hierarchy (#498) is drawn, and its
  largest groups labelled, by the frame after the layout worker's answer;
  but the view's test hook said it waited for nothing as soon as the answer
  was in, so a check that read the labels then, under load before that
  frame, found none (`a big graph: its largest groups are labelled with
  their node counts: []`, seen over `ursa serve`). The hook's `cuts.waiting`
  now also covers a cut, or the graph in place of one, that no frame has
  drawn yet; and a new check holds the animation frames to see it wait.
  The zoom into rmat also starts once the camera has given back the room
  it made for the inspector's panel (#493): under load the first zoom's
  cut was of a camera still moving (76 groups for the fitted 92).

- **Web Shell: a `.graph` that fails downloads none of the graph view**
  (URSA-11). The pane opens with "Loading graph view…" once the engine has
  taken 150 ms, and it began downloading `graph.js` and `ursa-viz` then; so
  an error that took longer to say, as on a busy machine, downloaded a few
  MB for nothing. The view's code is now first downloaded once the engine
  has the graph; the pane still opens at 150 ms, and once the code is here
  it starts a layout worker then, as before. And the e2e checks of the
  graph view no longer depend on the machine's speed: `.graph`'s errors
  download no view (as above); `graphSettled` waits for the layout's last
  positions to be drawn, not just its "done", for every check that uses it
  (URSA-10 did so for GROUP BY alone); and the checks that click or probe
  a node or an edge after a selection does so where it is once the camera
  has made room for the inspector's panel, or given it back (#493), not
  where it was 450 ms before: the edge pick within 4 px (under load the
  room was made first, and both picks missed), Shift+click, the drag that
  pans, several nodes' WHERE clause, the panel's side for the rightmost
  node, and colour by.

- **A graph with a vertex table builds its index as fast as in 0.10 again,
  and faster** (#532). Since 0.12 the edges read while the vertex keys are
  still being numbered are interned provisionally, then renumbered (#385).
  That provisional interner gave its direct table of integer ids a slot
  per edge row, as the final one does, so with keys that are compact
  integers (Bluesky's) the few distinct ids of the early edges, spread over
  the keys' whole range, grew the table to about one slot per edge, and it
  grew one full copy at a time, on the edges' thread. It now keeps a direct
  table only while that is dense in its own ids (4 slots per id), else a
  hash map, and the index is the same. Four more passes of the build no
  longer run on one core: settling a chunk of edges (keeping the rows
  between two nodes) checks in parallel and copies nothing when no row is
  dropped; interning new ids visits only the 4,096-row blocks that have
  one; the CSR's per-node passes over the chunk histograms run over ranges
  of nodes in parallel; and the stored id lookup table (#421) is filled by
  every thread at once, giving the same table as filling it in id order
  (linear probing with priority to the lower id is history independent).
  On a synthetic Bluesky-shaped graph (a Parquet vertex table of 40M
  compact integer keys, 100M edges between 2.5M of them; 4 cores, `--readonly`
  `describe(g, full => true)`, three interleaved runs, build time from the
  notice): 0.10.0 7.8–8.6 s, `main` before this 9.9–10.5 s, now 7.9–8.8 s,
  with system time back from 5.6–5.8 s to 3.7–3.9 s (page faults halved:
  1.63M to 0.82M on one thread). With a `.csv.gz` vertex table: 0.10.0
  10.3–10.6 s, before 10.4–10.6 s, now 8.7–8.8 s. Edges only: 7.6–7.8 s,
  7.9–8.8 s, 7.6–7.8 s. With 40M sparse integer keys: 16.5–18.4 s,
  11.9–14.0 s, 11.9–13.0 s. Stored (a writable database), the whole
  process: 14.0–14.8 s, 17.9–18.0 s, 15.1–15.5 s; the rest is the 0.24 GB
  larger v4 file. The stored index files are byte-identical before and
  after (compact, sparse and string keys, and the `.csv.gz` table), and a
  test pins a hash of the stored index of a graph with a vertex table, for
  integer and string keys, recorded before the change.

- **SQL: a query that fails writes nothing to stdout, not even its header**
  (#388). `ursa --format csv -c "SELECT count(*) …"` wrote the header
  (`count(*)`), then the error, and exited 1, so a script read a header
  with no rows. Every output format now waits for the first row, or for the
  end of a result with none, before it writes anything; an empty result
  that succeeds still writes its header, as before and as in DuckDB. (Rows
  are written as they arrive, so a query that fails after its first rows
  were written has written them.)
- **SQL: `COLUMNS (p)` says a path cannot be a column** (#388). A path
  variable alone in `COLUMNS` said `'p' needs its variable, as a.p`. It now
  says `'p' is a path variable, and a path cannot be a column; use
  path_length(p), vertices(p), edges(p) or element_id(p)`. A node or edge
  variable alone says the same of a node or an edge, and names one of its
  properties and `element_id`.
- **SQL: `SET ursa.max_paths` with a bad value says what it takes** (#388).
  `SET ursa.max_paths = -5` (or `'lots'`) printed `Error parsing '-5' as
  usize: invalid digit found in string`; it now says `ursa.max_paths must
  be a non-negative integer (0 for no limit); got -5`, as the other
  settings name theirs. `ursa.graph_max_edges` too. And a `ursa.max_paths`
  below the floor an `ANY SHORTEST` search keeps to (one path per node of
  the graph; `k` per node for `SHORTEST k`) is said in a notice on stderr,
  `ursa: ursa.max_paths 1000 is below the node count; ANY SHORTEST keeps up
  to 39650447 paths`, where it was silent. Results are unchanged.
- **A large graph's first frame comes sooner** (#525). On `.graph rmat`
  (1M edges) in the Web Shell it is shown 0.69 to 0.84 s after Enter
  where it was 1.0 to 1.3 s, on a machine busy with other builds
  (`ursa-viz/BENCH.md`, "The first frame"; the 0.6 s the issue asks for
  is not reached, and the section says what it would take). The final
  layout is the same, bit for bit.
  - `.graph NAME` of a property graph that is one edge table (no vertex
    table, endpoint columns of one integer or string type) is drawn from
    the session's index of the graph, the one its algorithms run over,
    and not from its rows: with the index cached nothing is scanned. The
    drawing is the one the rows give (the same nodes and edges in the
    same order); a graph the index cannot answer for (a NULL endpoint, a
    vertex table, more edges than `ursa.graph_max_edges`) is read from
    its rows as before. The first `.graph` of such a graph builds the
    index if no algorithm has, and a session with a database stores it,
    as an algorithm would. In the browser the engine answers in 0.14 to
    0.23 s where it took 0.36 to 0.54 s, and holds 6 to 14 MiB less: the
    result is not copied before its bytes are written.
  - `ursa-core`: a layout's hierarchy no longer sorts the rows of its
    coarse levels (`Fa2Multilevel`: a row's targets are read off a
    bitmap), nor `UndirectedCsr::from_edges` the long runs of an
    adjacency: 140 to 150 ms where it was 200, and 28 ms where it was 39,
    natively for a 1M-edge R-MAT graph on one core. Both are checked
    against a sort of every edge.
  - `ursa-viz`: a view made in pieces gives the event loop its turns as
    messages, not timers, which a browser makes wait 4 ms each once they
    nest (`pace`); the turns that wait for the GPU are timers still.
  - The shell's layout worker lays out a small random graph a few times
    before the graph comes, so that the browser has compiled the
    layout's code well by then (`graph-worker.js`).

- **Making the graph view of a large graph no longer holds the page for
  over 60 ms** (#520). On `.graph rmat` in the Web Shell
  `tests/graph-timing.ts` saw one task of 51 to 80 ms while the view was
  made, in four runs of five over its limit of 60 ms. It was the last task
  of `createViewFromResult`: the shader, the two pipelines and the first
  draw of the coarse levels (#514), 43 to 46 ms in one go, and then,
  with no turn of the event loop between, the page's own work with the
  view it was handed, 12 to 27 ms. Each is now a piece, paced as the rest
  of the view is (#426): `View::warm_levels_paced` and
  `LevelRenderer::new_paced` in `ursa-viz`; `createView` and
  `createViewFromResult` give the event loop a turn before they resolve,
  and one between making the canvas's context and asking for the device;
  and the shell counts the degrees in a task of its own. Six runs of six
  have no task over 50 ms (`ursa-viz/BENCH.md` has the spread); making
  the view takes up to six turns of the event loop longer, about 4 ms
  each. What is drawn is the same.

- **Web shell: `COPY … TO` writes a file in the session, and no longer ends
  the engine** (#517). A browser has no disk, so `COPY … TO 'pos.parquet'`
  (or `.csv`, `.json`, `.arrow`; `'memory://uploads/pos.parquet'` is the
  same file) keeps the file in memory beside the imported ones:
  `FROM 'pos.parquet'` reads it, a session file being found by its name
  before the datasets' files, and `.files` lists it as `written here`. So a
  layout is saved as the CLI saves it: `COPY (SELECT * FROM layout_fa2(g))
  TO 'pos.parquet'`, then `.graph g POSITIONS (SELECT * FROM
  'pos.parquet')`, which the terminal's hint for a large graph now gives
  first. Before, a relative path failed with "relative URL without a base",
  and a `memory://uploads/…` target ended the engine: DataFusion's file
  sinks start tokio tasks, and the browser has no tokio runtime to start
  them on. The engine now writes the file itself in such a session
  (`ursa_engine::session_files`), with the serializers and options
  DataFusion's sinks use. What it cannot write is refused with a message
  that says what works: a name with a folder or other characters than ASCII
  letters, digits, `.`, `_` and `-`; a URL elsewhere; `PARTITIONED BY`;
  compressed CSV or JSON; and `INSERT INTO` a table of files or `EXPLAIN
  ANALYZE` of a `COPY`, which ran the same sinks. An `.arrow` file is
  written uncompressed. `WebShell.files()` lists the session's files. The
  CLI's `COPY` is unchanged.

- **Graph view: a snapshot of a level, a cut or a grouped drawing is of the
  view as it is** (#499). `GraphView.snapshot()` drew the discs the last
  frame had given the GPU, so a read-back taken before the next frame
  still showed a selection ring that had been cleared.
  `GraphView.refreshLevel()` gives the GPU the discs as they are now, and
  the page's read-back calls it first. On screen nothing changes: a frame
  always did. The snapshot borrows the view shared, as it did before (it
  borrowed it to change it for a day, and every call made during a read-back
  then threw and left the view unable to be freed, so the next `.graph`
  failed to start); the pane's hover waits for a read-back, and a pane
  closed or drawn again during one frees its view when the pixels have come.
  A read-back also gives the view the layout's newest positions first
  (URSA-10): a small grouped layout's last positions can come with its
  "done", before the frame that draws them, and a read-back then drew the
  discs where the frame before had them (the flaky e2e check `GROUP BY:
  .undo restores the closed drawing`, at 28409 px for 28729). And where the
  page gives the view those positions ahead of a frame (a test hook's
  lookup, a fit to the selection), a grouped drawing's discs move with
  them; they stayed where they were until the layout's next positions.
  The check itself now clears the selection, and waits for the camera to
  give back the room it made for the inspector's panel, before `.undo`:
  under more load the panel's room was in the read-back.

- **Shell: the DuckDB dot-commands people reach for first** (#418).
  - `.mode markdown` writes a Markdown table, as DuckDB does (names centred,
    a number column's separator ending in `:`, `NULL` for a null; a `|` in
    a value is `\|`). `.mode line` writes one `column = value` a line, a
    blank line between rows. Both are `--format markdown` and `--format
    line` too.
  - DuckDB's mode names work: `duckbox` and `box` are `table`, `jsonlines`
    is `ndjson`, `lines` is `line`, in `.mode` and `--format`.
  - `.output FILE` sends what the shell prints to a file until `.output`;
    `.once FILE` for the next command only, so the output of `SHOW GRAPHS`
    or `DESCRIBE GRAPH` can be saved too.
  - `.schema ?PATTERN?` prints the statements that define the database's
    views, external tables and graphs (the session's views without a
    database file), each ending with `;`.
  - `.headers on|off` turns the header row of `csv` and `tsv` on or off.
  - `.help` names the DuckDB aliases. `.nullvalue`, `.separator`,
    `.databases` and `.shell` are not added.
- **SQL: loose ends of unavailable objects** (#401, after #381).
  - `g.edges` and `g.nodes` of an unavailable graph fail with the graph's
    message, as its functions, `GRAPH_TABLE` and `DESCRIBE GRAPH` do; they
    said "table not found".
  - The notice's remedy follows the cause: `or restore the file` for a
    missing file, `or fix the cause and open the database again` for
    anything else (expired `s3://` credentials, a network error, a file
    without the columns a view reads).
  - Repairs reach dependants: after `CREATE OR REPLACE` or `DROP` of an
    object, the objects unavailable because they use it are defined again
    from their catalog statements, so repairing a view brings back the views
    and graphs over it without reopening the database. One that still
    fails says why, and is tried again after the next change.
  - `CREATE OR REPLACE EXTERNAL TABLE` over an unavailable table, with a
    definition that fails too, keeps the table (unavailable, as it was); it
    disappeared for the rest of the session.
- **SQL: `SHOW GRAPHS` lists unavailable graphs, with a `status` and a
  `reason`** (behaviour change, #401). A graph of the database that could
  not be defined when it was opened (a file it reads was moved) was left
  out, so it looked lost, and `CREATE PROPERTY GRAPH` with its name said
  "already exists" for a graph nobody could see. It is now listed from its
  stored definition, as DuckDB lists a view over a missing file, and two
  columns are added after `vertex_key`: `status`, `ok` or `unavailable`,
  and `reason`, NULL for a usable graph and otherwise the notice's message
  after `graph g is unavailable: ` (`no file found at '…'; DROP PROPERTY
  GRAPH g or restore the file`). A vertex key the stored definition leaves
  out is NULL for such a graph. `.graphs` shows the same. `SHOW TABLES`
  already listed unavailable views and tables.
- **A truncated or fake `.parquet` file is an error, not a crash** (#332).
  A file whose footer claims more metadata than the file holds (a
  truncated download, a file still being written, `PAR1PAR1`) made the
  parquet crate subtract with overflow: a panic in a debug build, a garbage
  read in a release build. Ursa now checks each Parquet file's ends before
  the parquet crate reads it (at least 12 bytes, `PAR1` at both ends, a
  metadata length that fits) and names the file: `cannot read 'x.parquet':
  it is not a valid Parquet file: it is 8 bytes, and a Parquet file is at
  least 12`. It covers `FROM 'x.parquet'`, globs, `read_parquet`, `CREATE
  EXTERNAL TABLE … STORED AS PARQUET`, graphs over them, and the Python
  scans (`scan_edges('x.parquet')`: `the file is not valid Parquet (…)`),
  in the CLI, Python and the browser alike. arrow-rs fixed the overflow in
  parquet 60; Ursa is on 59 until DataFusion moves.
- **SQL: `DESCRIBE FROM t` lists the columns** (#417). It returned a table
  of no rows, for a table or a file (`DESCRIBE FROM 'edges.csv'`): the
  FROM-first rewrite did not run under `DESCRIBE`, so the statement described
  a query of no columns. `DESC FROM t`, `EXPLAIN ANALYZE FROM t` and
  `EXPLAIN VERBOSE FROM t`, which failed to parse, work too.
- **SQL: an edge table's endpoints may be integers of different widths**
  (#416). `SOURCE KEY (src) DESTINATION KEY (dst)` with an `INT` and a
  `BIGINT` column was refused ("must have the same type"), though a vertex
  key of another width was accepted and node ids are `BIGINT` whatever the
  stored type. Any two integer types are now one graph, as are two string
  types; an integer against a string is still an error.
- **Ursa Web Shell: `.graph` no longer draws scores or paths as nodes**
  (#427). The two columns `.graph` reads as each edge's ends must hold ids:
  integers, strings or dates, and, when they are the first two columns
  rather than `src` and `dst`, both of one kind. A float, a boolean, a list
  or a struct is refused, naming the columns and their types and what to
  type instead. After `SELECT * FROM pagerank(karate) LIMIT 5`, `.graph`
  drew five edges from each id to its score; after an `ANY SHORTEST` query,
  one edge from a number to a list. Now: `cannot draw the first two columns
  (id: Int64, pagerank: Float64) as edges: 'pagerank' is a number that is
  not whole (Float64). … To draw an algorithm's result, draw the graph and
  size or colour it: .graph NAME SIZE BY pagerank. To draw other rows, name
  the ends: .graph SELECT … AS src, … AS dst FROM …`. Columns named `src`
  and `dst` of those types are refused the same way. Two integer or two
  string columns draw as before.
- `ursa --format csv` and `tsv` write lists, structs and maps, as DuckDB
  does, as their text form, quoted where CSV needs it (`"[1, 2]"`, with
  `NULL` for a null inside one). They failed with "Nested type List(Int64)
  is not supported in CSV", so `vertices(p)`, `edges(p)`, `array_agg` and
  `random_walk` paths could not be written (#350).
- A table shows an untyped null (`SELECT NULL AS n`, of type `Null`) as
  `NULL`, as it shows a typed one; the cell was blank (#350).
- The skill and the shell page say which format to read results in (`json`
  or `ndjson`), that a table shows at most 40 rows while CSV and JSON write
  every row, and that piped output is never cut to a width (#350).
- Ctrl-C in the `ursa` shell prints `Interrupted` (stderr) and draws the
  prompt again at once. The signal was waited for on the query's own
  runtime, so a statement that kept every worker busy (an index build, a
  large join) could hold it back; it said `^C (cancelled)` when it was seen
  (#354).
- An error names the file it cannot read once: `cannot read 'x.csv': Csv
  error: …`, not `cannot read 'x.csv': cannot read /…/x.csv: Csv error: …`.
  For a glob or a list it names the file that failed. A Parquet error says
  `Parquet error: …`, not `Parquet: Parquet error: …` (#354).
- The docs and the skill take the top k before joining attributes
  (`WITH top AS (SELECT * FROM pagerank(g) ORDER BY pagerank DESC LIMIT 5)
  SELECT … FROM top JOIN g.nodes USING (id)`), and say why: an
  `ORDER BY … LIMIT` is not moved below a join, so joining first sorts every
  node's attributes too, about twice the memory on a large graph (#354).
- The docs' landing page leads with Ursa SQL: the install one-liner and a
  `CREATE PROPERTY GRAPH` and `pagerank` example, run in CI with the SQL
  docs. The Python library is the second path (#354).

- The browser shell's accessibility, look and onboarding. Tab and Shift+Tab
  move the focus out of the terminal (they inserted spaces, so the keyboard
  could not leave it), and the Datasets menu is a menu button: arrow keys,
  Home, End, Enter, Space and Escape. A click on the theme toggle, Import,
  Share, New or a dataset hands the keys back to the terminal; from the
  keyboard the focus stays put. The grey text (types, footers, nulls) is
  5.5:1 on white (it was 3.6:1). The logo is drawn at a legible size, the
  favicon has no white tile and follows a dark browser, and the scrollbars
  take the theme. On a phone the bar is one row (icons for New, Share and
  Import under 420px), the terminal's input is 16px so iOS does not zoom
  in, and a dataset's SQL is broken at spaces to fit the screen rather than
  wrapped mid-word. Each dataset explains its steps in `--` comments at the
  prompt, and the R-MAT dataset turns `.timer on`.

- The browser shell no longer crashes on a fixed-length `GRAPH_TABLE`
  pattern from a bound node (`MATCH (a WHERE a.id = 0)-[e]-(b)`). rustc
  miscompiled the path search's target check at `opt-level` "s" and "z",
  the browser engine's build, and indexed past the end of an empty list;
  the check is written without the nested `Option` it miscompiled. CI's
  browser test now builds at "z" too, and runs a `GRAPH_TABLE` step.

- The browser shell's line editor:
  - CJK and emoji take two cells and combining marks none, so the cursor is
    where the characters are, and a long CJK line that wraps is drawn once
    (it left a copy of its prompt for each edit). An emoji sequence (a ZWJ
    family, a skin tone, a flag) is one character on screen, and Left,
    Right, Backspace and Delete move over and delete what is seen as one
    character whole; they split emoji into broken halves.
  - A line of several that fills the terminal's width exactly no longer
    leaves an empty row after it.
  - Resizing the window redraws what is being typed for the new width at
    once; it was left as the terminal rewrapped it until the next key.
  - A line typed and run before the highlighter answered is coloured in the
    scrollback; it stayed plain. Late replies for text since changed are
    dropped.
  - Shift+Enter adds a line without also sending the line to the shell.
  - `.help shortcuts` lists every key the editor knows, and only those:
    Left and Right, Home and End, Backspace and Delete, Tab and the Ctrl
    keys were missing, and Up and Down move between the lines typed first.

- A CSV file DataFusion cannot read is named by its path, with its leading
  `/` (`cannot read /Users/me/x.csv: …`), not by its object store location
  (`Error when processing CSV file Users/me/x.csv`); stored index keys keep
  local paths whole too (#327).
- `RESET ursa.<setting>` works; DataFusion refused it for Ursa's settings.
- `ursa --format csv … | head` (and `tsv`) ends quietly with status 0 once
  the reader is gone, as the other formats do. Before, it printed "Error: Csv
  error: Broken pipe" and exited 1 (#317).

### Security

- **The browser shell is safer to open from a link.** A shared link
  (`#run=`) lists its commands and asks `Run N shared commands? [y/N]`; only
  `y` runs them, and a link's `.export` is skipped, so it cannot download
  files. Control characters (ESC above all) in a replayed command are shown
  as escapes (`\u{1b}`), and `.print` writes them so in the CLI too, keeping
  lines and tabs, so text cannot move the cursor or restyle the terminal.
  Import takes any file name (`données.csv`, `a#b.csv`, `order.csv`), and
  its errors print below the prompt instead of over it. The Worker answers
  a path with a malformed `%` escape with 404, not 500, and every response
  carries a Content Security Policy (`frame-ancestors 'none'`, the page's
  own scripts, fonts and fetches only), `X-Content-Type-Options: nosniff`
  and `Referrer-Policy: no-referrer`; the local preview server sends the
  same, and the e2e test checks them and that the page runs with no
  violation.
- The upgrade clears RUSTSEC-2026-0435 (`thrift` 0.17, a parquet 58
  dependency that parquet 59 no longer has) and RUSTSEC-2026-0176/0177
  (`pyo3` 0.28), and drops `paste` (RUSTSEC-2024-0436) from the tree. Bumped
  `zstd` 0.13.3 → 0.13.4, `zstd-safe` 7.2.4 → 7.4.0 and `zstd-sys` 2.0.16 →
  2.1.2, the patched releases on the 0.13 line, for
  RUSTSEC-2026-0391/0398/0403/0410. Their waivers are gone from `deny.toml`;
  only `quick-xml` 0.39 (RUSTSEC-2026-0194/0195, via object_store 0.13)
  remains waived.

## [0.5.0] — 2026-10-31

### Added

- CSV options on `scan_edges`, `scan_nodes`, `read_edges` and `read_nodes`:
  `delimiter`, `has_header`, `quote`, `escape`, `null_value` and `comment`,
  passed to DataFusion's CSV reader. The defaults read a file as before
  (comma-separated, a header row, `"` quotes, an empty field as null). The
  options are checked where the scan is made: each character option is one
  ASCII character other than a line break, the four must differ, and an
  option on a scan with no `.csv` path raises `ValueError`. `escape` applies
  inside a quoted field, as in the CSV reader DataFusion uses; in an unquoted
  field it is an ordinary character. `null_value` reads that text as null, as
  an empty field is read. DataFusion 54 applies a null pattern when it infers
  a schema but not when it reads rows, so with a `null_value` Ursa reads each
  file with the same arrow-csv reader, options and inferred schema, plus the
  pattern: every value parses (or fails to parse, with the same error) as it
  does without a `null_value`; only which fields are null differs. Such a
  file is read whole rather than in byte ranges. The tests compare each
  layout with `pyarrow.csv` given the same options, with empty fields read as
  null in both (`strings_can_be_null=True`) and pyarrow's headerless column
  names (`f0`, `f1`, ...) renamed to Ursa's (`column_1`, `column_2`, ...) (#216).
- A scan of a list of paths collects: the paths (files or globs) are read as
  one table, in the order given, as the files read into memory and
  concatenated would be. Every graph operation sees that table, weighted
  algorithms included: one index over the union of the files, with the
  weights read from the same rows. The reshaped-topology cache keys such a
  scan by every file it reads. A tuple works as a list, and a path may be a
  `pathlib.Path`. The paths must have the same columns and types (their
  nullability may differ), and be on one store; otherwise `collect()` names
  each column that differs, its type in each file, and what to do. Each CSV
  path infers its own types, so a column of `1`s in one file and `0.5`s in
  another is such a difference, as it is for a glob (#216).

- `ur.density`, `ur.avg_path_length` and `ur.diameter` take `lazy=True`, which
  returns a lazy one-row frame like `ur.describe`: one non-null column named
  after the stat (`density` and `avg_path_length` are `float64`, `diameter`
  `int64`), with `schema()`, `explain()` and the relational methods. Called as
  before they still return a Python `float` / `int`: they now collect that
  frame (#213).
- `ur.describe(edges, nodes="touched")` describes only the nodes the view
  touches, those at an end of a kept edge: over a filter or a traversal result,
  `n_nodes` counts them, `density` and `avg_degree` divide by that count, and
  `n_components` leaves the other nodes out. These are the figures a new frame
  over the kept rows gives. The default, `nodes="all"`, keeps the parent's node
  set as before; over a whole graph the two agree. Any other value raises
  `ValueError: describe(nodes=...) is not supported; use 'all' or 'touched'.`
  where `describe` is called (#213).
- `frame.schema()` returns the schema `collect()` returns, as a
  `pyarrow.Schema` (names, types, nullability and metadata), on every frame:
  sources and scans, graph columns, joins, `group_by`, traversals, walks and
  `describe`. It runs nothing: the plan is built over empty inputs, so a scan
  reads its file's schema and no rows, and no graph index is built.
  `frame.explain()` now shows the plan's steps (one line each, with the frames
  a step reads indented under it), that schema, and the DataFusion
  logical plans a collect runs, optimized; `explain(physical=True)` adds their
  physical plans. Both are generated in Rust. The text is for reading, not a
  stable format (#193).
- Criterion micro-benchmarks for the `ursa-core` kernels (`cargo bench -p
  ursa-core`, `ursa-core/benches/kernels.rs`): topology build and views, id
  interning, and every kernel family on seeded Erdős–Rényi and R-MAT graphs up
  to 1M edges. A `Bench` workflow runs them weekly and on demand;
  `benchmarks/README.md` describes comparing two commits (#187).

### Changed

- **Behaviour change:** every function that takes an edge frame checks it
  where it is called: `density`, `avg_path_length`, `diameter`, `describe`,
  each graph algorithm (`degree`, `pagerank`, `connected_components`,
  `triangle_count`, `clustering_coefficient`, `betweenness`, `closeness`,
  `label_propagation`, `louvain`), `neighbors`, `hop`, `shortest_path` and
  `random_walk`. A node frame, a graph verb, a `describe`, stat or
  `random_walk` result, a collected result, a table or any other value
  raises `TypeError`, which names the function, what it got and what to
  pass: `density() takes an edge frame (from ur.from_arrow(..., src=, dst=)
  or ur.scan_edges); got a node frame (ur.degree(...)); pass the edges the
  verb was built from.` Before, a frame raised `NotImplementedError` about a
  call the user did not make (`with_columns() of a graph verb is not
  supported on an edge frame ...`, or `collect() needs edges from
  ur.from_arrow(...) ...`), and any other value raised `AttributeError`
  (`density() takes an EdgeFrame (the graph) as its edges; got int`). A graph
  algorithm raised only when its frame was used or collected; it now raises
  where it is called, and the edges are checked before the other arguments.
  An edge frame of any kind, a `hop` or `shortest_path` result included,
  gives the same result as before (#223).
- **Behaviour change:** an unknown keyword argument to `scan_edges`,
  `scan_nodes`, `read_edges` or `read_nodes` raises `TypeError` naming the
  function called (`read_edges() got an unexpected keyword argument 'foo'.`),
  with the option to use for another library's name for one (`sep` and
  `separator` → `delimiter`, `header` → `has_header`, `na_values` and
  `null_values` → `null_value`, `quotechar` → `quote`, `escapechar` →
  `escape`), or the closest option it takes for a misspelling
  (`delimter` → `did you mean delimiter=?`). It raised
  `NotImplementedError: scan format options (...) are not supported yet`
  (#216).
- **Behaviour change:** a scan's paths are checked where it is made. A path
  with an extension a scan does not read, or an HTTP URL with a query string,
  raises the error (class and message) `collect()` raised. A path that is
  not a string, a `pathlib.Path` or a list or tuple of them raises
  `TypeError` (#216).
- `sample(n)` without a seed is documented as deterministic: it draws with a
  fixed default seed, so it keeps the same rows on every call and every run.
  Pass a different `seed` for a different draw. Results are unchanged (#216).
- A `pathlib.Path` works as a scan path. It raised `NotImplementedError` at
  `collect()` (#216).

- **Behaviour change:** `group_by` keys, `distinct`, `n_unique` and join keys
  compare floats in the total order filters and `sort()` use: `-0.0` equals
  `0.0`, every NaN equals every NaN whatever its sign and payload, and NaN joins
  NaN. They compared raw bits, so the same column grouped one way and filtered
  another. This covers top-level `float16`, `float32`, `float64` and
  dictionary-of-float columns, every join `how` (including right/outer key
  coalescing, where a right key fits the left type in total order: `-0.0` fits
  an integer key as `0`) and engine frames. A float nested in a struct or list
  column still compares by its bits. Nulls are unchanged: one group in `group_by` and `distinct`, and never
  a join match. A group or distinct row shows its first row's value, matching
  first-appearance row order; a joined row keeps its own key value, and an
  aggregate over a key column folds the original values. With `k = [-0.0, 0.0,
  NaN, -NaN, null]` (`-NaN` is what `0/0` gives on x86), `v = [1, 2, 3, 4, 5]`:

  | call | before | after |
  |---|---|---|
  | `group_by("k").agg(s=col("v").sum())` | `-0.0: 1`, `0.0: 2`, `NaN: 3`, `-NaN: 4`, `null: 5` | `-0.0: 3`, `NaN: 7`, `null: 5` |
  | `select("k").distinct()` | 5 rows | `-0.0, NaN, null` |
  | `group_by(...).agg(u=col("k").n_unique())` | 4 | 2 |
  | `join(other, on="k")`, `other.k = [0.0, NaN]` | `0.0` and `NaN` match once each | `-0.0` and `0.0` match `0.0`, `NaN` and `-NaN` match `NaN` |

  Results without a `-0.0` or a NaN other than the positive quiet one are
  unchanged, byte for byte. The key is canonicalized once, as a hidden copy,
  before anything hashes it, so the parallel order-preserving partition (#212)
  and the exact float folds are unchanged (#215).
- **Behaviour change:** error messages no longer show the engine's internals
  (#214). A message names the step or argument, says what was wrong and what to
  do; no message contains DataFusion's `?table?` qualifier, a private
  `__ursa_` column, serde's or pyo3's wording, or a DataFusion/Arrow variant
  prefix (`Execution error:`, `Arrow error:`, `Error during planning:`,
  `This feature is not implemented:`, `Schema error:`). Where Ursa rewords an
  engine error, the engine's text is the exception's `__cause__` (a
  `RuntimeError`); in Rust, `PlanError::source()` returns it. Every exception
  class is unchanged, and so is every result. The messages that changed (old →
  new; `…` is the rest of the new message):
  - comparisons: `Function 'greater' has no kernel matching input types
    (string, int64)` → `cannot compare string with int64 (string > int64);
    compare values of one type` (every operator and type pair), and `Integer
    value N not in range: LO to HI` → `the integer N does not fit the type both
    sides are compared as (range LO to HI), so the comparison would not be
    exact; …`;
  - DataFusion's planning errors: `Error during planning: Cannot coerce
    arithmetic expression Utf8 * Int64 to valid types` → `cannot apply '*' to
    string and int64 (string * int64); arithmetic needs numeric operands`;
    `… logical boolean operation Boolean AND Int64` → `'&' combines boolean
    predicates (comparisons such as ur.col('a') > 1); got bool and int64` (and
    `'|'`); `… Int64 IS DISTINCT FROM Boolean` (from `~`) → `'~' negates a
    boolean predicate …; got int64`; `Function 'sum' failed to match any
    signature, errors: …` → `sum() needs a numeric column, and got a string
    column; sum() and mean() take numbers only, while min(), max(), count()
    and n_unique() take strings too` (and `mean()`, with the column's type;
    another aggregation reads `<fn>() cannot aggregate a <type> column`);
    `Cannot get result type for temporal operation Timestamp(µs) *
    Timestamp(µs): …` → `cannot apply '*' to timestamp[us] and timestamp[us]
    (…); this arithmetic is not defined for these types`; type names in these
    messages are pyarrow's (`dictionary<values=string, …>`, `decimal128(10,
    2)`, `timestamp[us]`), never DataFusion's (`Dictionary(Int32, Utf8)`);
    `Physical plan does not support
    logical expression AggregateFunction(…)` (an aggregation in a weight) →
    `an aggregation (.sum(), .mean(), ...) is not allowed here; …`;
  - Arrow errors: `Arrow error: Divide by zero error` → `division by zero in an
    integer expression; …`; `Arrow error: Cast error: Cannot cast string 'a' to
    value of Int64 type` → for join keys, `join(on=): the left and right keys
    have different types (the string 'a' cannot be read as int64); cast one
    side's key to the other's type first`, and for a graph column over a frame
    whose ids are of another type, `with_columns(): the frame's id column and
    the graph's node ids have different types (…); build the frame with ids of
    the graph's id type`; `join(on=): key 'k' keeps the left frame's type
    (Int64), which can't hold every value of the right frame's key (Utf8); …`
    names the types `int64` and `string`; `Arrow error: Invalid argument error: must
    either specify a row count or at least one column` → `select() needs at
    least one column`;
  - schema errors: `Schema error: No field named x. Valid fields are
    "?table?".a, …` → `unknown column 'x'; the frame has ['a', …].`, and
    `Schema error: Schema contains duplicate qualified field name "?table?".a`
    (a graph column named like an existing one) → `the result would have two
    columns named 'a'; …`;
  - graph verb arguments: `Execution error: invalid columns spec: invalid type:
    string "x", expected f64` → `pagerank(damping=) must be a number; got 'x'`,
    and `… invalid value: integer -1, expected u32` → `pagerank(max_iter=)
    must be a non-negative integer below 2**32; got -1` (every parameter of
    every verb); `ValueError: invalid plan: a plan argument is NaN, which has no
    JSON form` → `pagerank(damping=) must be a finite number; got NaN`;
    `This feature is not implemented: direction must be 'out', 'in', or 'both';
    got "x"` → `degree(direction='x') is not supported; use one of 'out', 'in',
    'both'` (also `closeness`, `neighbors().agg`, `hop`, `shortest_path`);
    `weight expression must be numeric; it produced a column of type Utf8` →
    `… of type string`; `weight expression references unknown edge column(s):
    ['x']` gains `; the edges have [...]`;
  - integer arguments: `argument 'n': 'str' object cannot be interpreted as an
    integer` → `hop(n=) must be an integer; got a str`, `out of range integral
    type conversion attempted` → `hop(n=) must be a non-negative integer; got
    -1` or `hop(n=) is too large; it must be below 2**32`, `can't convert
    negative int to unsigned` → `random_walk(seed=) must be a non-negative
    integer; got -1`, `int too big to convert` → `… is too large; it must be
    below 2**64` (also `random_walk(steps=)` and `walks_per_node=`);
    `head("x")`/`sample("x")`: `invalid literal for int() with base 10: 'x'`
    → `head(n=) takes an integer row count; got 'x' (str)`; `sample(-1)`:
    `Sample larger than population or is negative` → `sample(n=) must be a
    non-negative row count; got -1`;
  - plan arguments: `ValueError: invalid plan: invalid type: integer 1,
    expected a string` → `join(how=) must be a string; got 1` (also
    `hop(direction=)`, `shortest_path(direction=)`, `describe(direction=)`);
    `rename({"a": 1})` → `a rename() column name must be a string; got 1`;
    a plan argument of a type with no JSON form, `TypeError: Object of type X is
    not JSON serializable` → `<argument> cannot take a value of type 'X'` (as
    `pagerank(damping=) cannot take a value of type 'object'`), with the
    original exception as its `__cause__`; `rename(["a"])`/`rename(None)` → `rename() takes a
    dict of {old: new} column names; got …`; `TypeError: Object of type X is
    not JSON serializable` (a filter literal) → `a literal of type 'X' is not
    supported in a filter expression; use a bool, int, float, str or None, …`;
  - frames and edges: `AttributeError: 'str' object has no attribute 'kind'`
    → `filter() takes an ursa expression, such as ur.col('x') or
    ur.col('x') > 1; got 'age' (str)` (also `weight=`); `'NodeFrame' object has
    no attribute 'nodes'` / `'int' object has no attribute '_node'` → a
    `TypeError` that names the function, what it got and what to pass (every
    verb, traversal and stat; see the edge-frame entry under Changed, #223);
    `cannot pickle 'ursa._ursa.PlanNode'
    object` → `EdgeFrame objects cannot be pickled or deep-copied: a frame is a
    lazy plan over live data. …`; `argument 'src': 'int' object is not an
    instance of 'str'` → `src= must be a column name (a str); got 1 (int)`
    (`scan_edges(src=)` for a scan; `id=` raised `expected bytes, int found`);
    `KeyError: 'Field "x" does not exist in schema'` → `src column 'x' is not
    in the table; it has [...]`; `argument 'storage_options': …` →
    `storage_options= must be a dict of option names to string values; got …`;
  - seeds of another id type: `src and dst node-id columns must have the same
    type (both int64 or both string)` → `the node ids given are string but the
    graph's node ids are int64; pass ids of the graph's id type`;
  - unsupported shapes, now raised where the step is added: `collect() does not
    yet support the 'with_columns' step on an edge frame.` → `with_columns() of
    a graph verb is not supported on an edge frame; graph columns belong on the
    node frame: edges.nodes().with_columns(...).`; `… step after a
    random_walk.` → `with_columns() after random_walk() is not supported yet;
    collect() the walk and build a new frame from it with ur.from_arrow(...)
    first.`; `… step after describe().` → `with_columns() after describe() …`;
    `… step in a composed pipeline.` → `<step>() is not supported at this point
    in the pipeline yet; …`; `select(col("x").sum())` named `with_columns()`
    and now names `select()` (and `sort()`);
  - scans, now led by the scan and its path (`scan_edges('edges.parquet'):
    …`): `Error during planning: No files found at file:///…. Cannot infer
    schema …` → `no file matches the path; check it (a glob must match at
    least one file)`; `Execution error: scan_edges: endpoint column "x" not
    found in edge file "…"` and `Schema error: No field named x …` → `src
    column 'x' not found in the file; it has [...]` (`id column` for
    `scan_nodes`); `Parquet error: Parquet error: Invalid Parquet file. Corrupt
    footer` → `the file is not valid Parquet (corrupt footer)`; `Error when
    processing CSV file …\ncaused by\nObject Store error: Generic
    LineDelimiter error: …` → `could not read the file as CSV: …`; `This
    feature is not implemented: scan_edges supports .parquet and .csv in v0.1;
    got path "…"` → `only .parquet and .csv files can be scanned; …`;
    `scan: unsupported URL scheme "ftp" (…)` → `unsupported URL scheme 'ftp';
    use a local path or a file://, …`; `edge source "…" resolved but contained
    no rows; …` → `the file has no rows; …`;
  - more scan and storage messages: `src and dst node-id columns must be the
    same type (both int or both string); got Int64 and Utf8 in "…"` → `the src
    and dst columns must have one id type (both integer or both string); got
    int64 and string`; `node ids must be an integer or string column; Float64
    is not a supported id type` → `node ids must be an integer or string
    column; got double` (and `unsupported node-id type …; node ids must be
    int64 or string (Utf8)` → `node ids must be an integer or string column;
    got …`); `scan: bad object-store url: …` → `not a valid URL: …`; the http
    query-string message loses its `scan_edges:` prefix and trailing `got
    "…"` (the label names the scan and its path); object_store's `Generic HTTP
    error: Error performing HEAD <url> in …, after 10 retries, … - HTTP error:
    <cause>` → `could not read <url>: <cause>`, `Configuration key: 'k' is not
    valid for store 'S3'.` → `storage_options key 'k' is not an option of the
    S3 store; …`, and other `Generic S3 error: …` messages lose that prefix;
  - classes: every class is as before, with one fix: an engine error is a
    `ColumnNotFoundError` by its structure (a schema error, or Ursa's own
    "column … not found"), so a storage error whose path holds the word
    "column" (`s3://b/columns/x.parquet` not found) is now the `ComputeError`
    it is. An exception that is a subclass (`UnicodeEncodeError` for a string
    with no UTF-8 form) is raised as it is, and a reworded exception keeps the
    one it replaced as its `__cause__`;
  - the stats as plan nodes (#217): a `sample=` or `approximate=` of the wrong
    type, `argument 'sample': 'str' object cannot be converted to 'PyFloat'`
    → `avg_path_length(sample=) must be a number or None; got 'x' (str)`, and
    `diameter(approximate=) must be a bool; got 'x' (str)`. #217's own
    messages (`bool(expr)`'s `TypeError`, `describe(nodes=...)`, the stats'
    `direction=`, steps after a stat or on a bare `nodes()`) were already in
    this form and are pinned by the tests;
  - Ursa's own messages lose the variant prefix DataFusion put before them
    (`Execution error: output column name 'id' is reserved …`, `This feature is
    not implemented: neighbors().agg() needs a node attribute table …`, and
    the rest of the `weight expression …` and `neighbors().agg() …` family);
  - the scan messages #216 adds, in the same form (pinned in
    `tests/test_error_messages.py`): a list of paths is labelled with the list
    (`scan_edges(['a.csv', 'b.csv']): the paths in a list are read as one
    table, so each needs the same columns, with the same types, in the same
    order. Column 'w' is int64 in 'a.csv' and double in 'b.csv'. Write the
    files with matching types …`, and `… 'a.parquet' has the columns [s, d, w]
    and 'b.parquet' has [s, d]. Give every file the same columns …`; `… the
    paths in a list must be on one store …`), the CSV options name the call
    (`scan_edges(): delimiter must be one ASCII character; got ';;'. Pass a
    single character, such as ';'.`, `read_edges(): the CSV options
    (null_value=) only apply to .csv files, …`), and an unknown keyword names
    the function and the option to use (`read_edges() got an unexpected
    keyword argument 'sep'; use delimiter= instead.`). A stack too small for a
    plan raises `UrsaError: collect(): the frame's plan has N relational
    steps, more than the 65280 one plan can run. …`;
  - a CSV value that does not parse as its column's inferred type (#222):
    `scan_edges('g.csv'): Parser error: Error while parsing value 'yes' as
    type 'Boolean' for column 2 at line 1001. Row data: '[2,3,yes]'` →
    `scan_edges('g.csv'): column 'f' was inferred as bool from the first 1000
    rows, but line 1002 has 'yes', which is not a valid bool. Fix the value,
    pass null_value='yes' if it marks a missing value, or read the file with
    pyarrow.csv (column_types=) and pass the table to ur.from_arrow().` The
    message names the column, its type in pyarrow's spelling (`bool`,
    `int64`, `double`, `date32[day]`, `timestamp[s]`, ...), the value, and
    the value's line in the file, counted from 1 with the header as line 1.
    arrow-csv counted from 0, and from the start of each byte range when
    DataFusion split a large file across cores, so its line depended on the
    core count. Ursa now reads the failing column again, file by file in path
    order, and reports the first value that does not parse. The value and
    line are the same on any core count, and the message is the same with or
    without `null_value`. A glob or a list of paths also names the file
    (`line 9 of 'data/b.csv'`). When the engine's text for a timestamp has no
    value, the message says `has a value that is not a valid timestamp[s]`.
    The class is `ComputeError`, as before, and the engine's text is the
    `__cause__`.
- **Behaviour change:** a standalone graph verb is a full `NodeFrame` (#213).
  `ur.degree(e)`, `ur.pagerank(e)` and the other node-valued verbs called on
  their own are `edges.nodes().with_columns(<verb>=...)` with every frame
  method: `join`, `group_by`, `rename`, `sample` and `limit` now work (they
  raised `AttributeError: 'GraphExpr' object has no attribute 'join'`), and
  `isinstance(ur.degree(e), ur.NodeFrame)` is true, so a verb is also a
  `join`'s right side and a seed set. The methods return a plain `NodeFrame`.
  Inside `with_columns(...)` a verb is still an expression and builds no frame
  of its own; its `repr` and operators are unchanged.
- **Behaviour change:** `edges.nodes()` with no columns collects to its node
  set: one `id` column, the rows `with_columns(degree=ur.degree(edges))` has in
  the same order (the parent graph's nodes over a filtered frame, the nodes of
  the rows after a reshaping step). Relational steps and joins after it run on
  those rows, and a `join` whose right side is a bare `nodes()` now builds. It
  raised `NotImplementedError: collect() on this NodeFrame needs a source ...`
  (#213). A first `with_columns` of scalar expressions runs on that node set
  too (`e.nodes().with_columns(y=ur.col("id") + 1)`), as do the steps before
  and after it. Over a filtered frame or a traversal result the node set is
  the parent graph's (`ur.hop(e, 1).from_([0]).nodes()` has every node), as
  `degree` and `describe` read it; `describe(f, nodes="touched")` counts the
  touched ones. The row is gone from `docs/limits.md`.
- **Behaviour change:** an ursa expression has no truth value. `bool(expr)`
  raises `TypeError: the truth value of an ursa expression is ambiguous ...`,
  as Polars does, so `if ur.col("a") == 1:`, `ur.degree(e) in frames` and
  `a and b` over expressions fail instead of always passing. `==` still builds
  an expression; combine predicates with `&`, `|` and `~` (#213).
- `ur.density`, `ur.avg_path_length` and `ur.diameter` run as plan nodes, as
  `describe` does (`Plan::Stat`). `ursa._ursa.graph_density`,
  `graph_avg_path_length`, `graph_diameter` and `PlanNode.graph_index` are gone.
  Over a whole graph or a reshaped edge set the values are byte-identical.
- Frames hold Rust plan objects (#193, the last item of #189). An `EdgeFrame`
  or `NodeFrame` holds one `ursa._ursa.PlanNode`, as a Polars `LazyFrame` holds
  its plan: each method converts its arguments, calls into Rust, which builds
  and validates the next step (`ursa_plan::lazy`, `ursa_plan::infer`), and wraps
  the new node in a frame of the same type. `collect()`, `schema()` and
  `explain()` run there, and so do the caches: a frame's topology index (with
  its weights, masks and symmetric views) and a scanned node table live on the
  Rust state a frame shares with the derivations that keep its rows, so a
  derivation made before a build still sees it. Python keeps the API surface,
  input conversion (pandas/polars/numpy/lists to Arrow, expressions and
  arguments to the plan IR), interop egress and the datasets, plus four small
  callbacks (`ursa._execute`): a seed iterable's conversion, the stdlib's
  seeded draw for `sample`, the `on_null="drop"` warning and an in-memory
  table's identity token. Nothing in Python lists the engine's steps, verbs,
  aggregations, joins, directions or modes any more; a test fails if a module
  does. `ursa._ursa.run_plan`, `resolve_index` and `scan_build_index` are gone.
  Results are byte-identical: three parity harnesses (10,928 plans) give the
  same schema, values (floats by their bits), row order and errors as before,
  on one core and on four. Building a step, its validation included, takes
  about 8 µs (2.3 µs before, when nothing was checked), however long the chain
  before it and however wide the frame: a check reads its input's results, and
  a frame joined with itself is checked once per side, not once per path. A
  20-step pipeline on karate builds and collects in 5.8 ms, as before, and a
  small collect is 0.18 ms against 0.21 ms (4 cores, medians of 5 interleaved
  runs). A frame drops its chain a step at a time, so a long one drops on a
  small thread stack.
- **Behaviour change:** a step that cannot run raises on the line that adds it,
  not at `collect()`. The exception class and message are the ones `collect()`
  raised. What moved:
  - an unknown column in `filter`, `select`, `with_columns`, `rename`, `sort`,
    `group_by` or `agg` (`ColumnNotFoundError: sort() references unknown column
    'x'; the frame has [...]`), a `join` key missing from either side, the node
    id column a graph column attaches by, a weight's edge column, and an edge
    frame whose steps dropped `src` or `dst` under a graph op;
  - two output columns with one name (`select`, `rename`, a join's non-key
    columns), and a graph column named `id`;
  - a filter that is not a boolean predicate, an operator or expression node a
    filter does not take, a role (`ur.src()`, `ur.dst()`, `ur.id()`) the frame
    does not have, and a comparison between types that do not compare
    (`ComputeError: ... has no kernel matching input types (string, int64)`, or
    the `ValueError` for an aware against a naive timestamp);
  - every unsupported shape (`NotImplementedError`): a step after `describe()`
    or a `random_walk` other than a relational step or a join, a graph verb in
    an edge frame's `with_columns`, `reverse()` after a traversal, a traversal of
    a traversal result, a step between a traversal and a graph op over it, a
    `with_columns` after another step on `edges.nodes()`, an unknown `how`, a
    graph verb's unsupported `mode=`, `dtype=`, `weight=` or `neighbors()` form,
    and a `join` whose right side does not collect;
  - an argument the step could not take: `sort([])`, a `descending` list of the
    wrong length, a `select` item that is not an expression, `group_by` keys or
    aggregations of the wrong form, `join(on=None)`, a non-integer `head(n)` or
    `sample(n)`, a non-frame `join(other)`, and a literal with no JSON form.

  These stay at `collect()`, since only the rows (or the files) tell: a null
  endpoint, an integer divide by zero, a value out of range, a right or outer
  join key the left type cannot hold, a missing or empty file, a scan's columns
  (the file is read then, and may change in between), `store=`, a scan of
  several files, and an invalid graph verb parameter value. When a
  plan has a step the plan alone shows is wrong and also an error only the rows
  show earlier in it, the step's error is now the one raised. A step on an edge
  frame is checked even where a later use reads only the frame's topology:
  `ur.degree(e.select("s", "d", "nope"))` and
  `e.filter(ur.col("nope") > 1).nodes()` used to run, skipping the step, and
  now raise where the step is added. One message reads differently: a NaN or
  infinite graph-verb parameter raises `ValueError: invalid plan: a plan
  argument is NaN, which has no JSON form` (it quoted a JSON parser offset).
  The guard on steps on a traversal's edges before `nodes()` is raised by the
  first `with_columns` after `nodes()`, which raises the graph op's own guard
  instead when its edges are those, as `collect()` did. Two other errors read differently. A role column that is not a string
  (`scan_edges(path, src=1, dst="d")`) raises `TypeError: argument 'src': 'int'
  object is not an instance of 'str'` where the frame is made (`collect()`
  raised `ValueError: invalid plan: invalid type: integer \`1\`, expected a
  string`). An argument the plan could not read keeps that message but no longer
  ends with the JSON parser's `at line 1 column N`.
- `collect()` now hands the whole query to Rust as one plan. The frame is
  serialized to a plan IR in `ursa-plan` (`ursa_plan::plan::Plan`), and a single
  entry point, `execute_plan` (`ursa._ursa.run_plan` in Python), picks which
  executor runs it and rejects unsupported shapes, with the same error messages
  as before. The Python layer no longer decides which of the engine's seven entry
  points to call. Results are unchanged. Caches, subgraph masks, plain-source
  reads and the steps the engine's tail can't run in written order still run in
  Python, called back from the executor as it needs them. `run_node_query`,
  `run_join_query`, `run_hop_query`, `run_path_query`, `run_walk_query` and
  `graph_describe` are gone from `ursa._ursa`. `ursa-plan` gains plan
  conformance tests: JSON plans over karate and other small graphs, with
  expected output recorded from the Python package (#190, part of #189).
- One expression evaluator: every `filter` now runs in Rust through the same
  DataFusion lowering, wherever it sits. Before, `pyarrow.compute` evaluated
  filters on plain frames, subgraph-view masks and steps after the engine's
  tail. Subgraph masks are built in Rust, in dense id space; a traversal
  result's mask comes straight from the reached node set. Plain sources are
  DataFusion scans: `scan_edges(...).filter(...)` pushes its leading filters
  into the read, so Parquet skips row groups whose statistics rule them out.
  Masks are cached on the graph index, keyed by the view, so two frames with
  the same filter share one. A `scan_nodes` frame keeps one table, the union
  of the columns read so far, instead of one table per projection (#178).
  Results are unchanged, except for NaN (next entry) (#192, part of #189). On 10M
  edges (4 cores): the first masked `degree` over a scan, 0.24s to 0.11s; the
  mask of a 2-hop traversal result from 2,000 seeds, 0.92s to 0.24s; a
  1%-selective filter on a 10M-row Parquet file sorted by the filtered column,
  0.12s to 0.010s.
- **Behaviour change:** NaN compares in total order in every filter: NaN equals
  NaN and is greater than every number, as in `sort()`. Filters inside an
  engine query already did; filters on plain frames, subgraph masks and steps
  after the engine's tail compared IEEE-style (any comparison with NaN false,
  `!=` true). With `f = [0.5, NaN, null, 2.0, -1.0]`, a plain frame keeps:

  | filter | before | after |
  |---|---|---|
  | `f > 1` | `2.0` | `NaN, 2.0` |
  | `~(f > 1)` | `0.5, NaN, -1.0` | `0.5, -1.0` |
  | `f == nan` | none | `NaN` |
  | `f != nan` | `0.5, NaN, 2.0, -1.0` | `0.5, 2.0, -1.0` |
  | `f != f` | `NaN` | none |
  | `f / 0 > 1` | `0.5, 2.0` | `0.5, NaN, 2.0` |

  Nulls are unchanged: a comparison with null is null, and the row is dropped
  (#199).
- Filters inside an engine query (after graph columns, a join or a traversal)
  accept every literal a plain-frame filter does: datetimes, dates, times,
  timedeltas, `numpy.datetime64`/`timedelta64` (a unit of a day or finer),
  `Decimal` (up to 76 digits), `bytes`, NaN, infinity and `None`. They used to
  raise `TypeError: Object of type ... is not JSON serializable` (or
  `ComputeError` for NaN and infinity, "unsupported literal type" for `None`).
  A day-unit `numpy.datetime64` now works on plain frames too (it raised a
  `TypeError`); a month or year unit raises a `TypeError` that says so (#192).
- Every filter compares the way plain frames did: operand types pyarrow had no
  comparison kernel for stay errors, timestamps, durations and times compare
  at the finer unit, an integer past 2**53 against a float and a `uint64` past
  2**63 against a signed integer stay "not in range" errors. Engine filters
  used DataFusion's coercion, which parsed `ur.col("n") == "1"` as a number and
  compared timestamps at the coarser unit (#192).
- Such errors are `ursa.ComputeError` on every path. Plain frames and subgraph
  masks used to raise pyarrow's exceptions: `ArrowNotImplementedError` (a
  `NotImplementedError` subclass) for a type mismatch such as
  `ur.col("n") == "x"`, and `ArrowInvalid` (a `ValueError` subclass) for an
  integer divide by zero or a value "not in range". The engine already raised
  `ComputeError`. A timezone-aware against a naive timestamp of the same unit
  stays a `ValueError` (#192).
- A `string_view` column compares with a string literal; pyarrow had no kernel
  for it (#192).
- `ursa._ursa` no longer has `hop_reached_nodes_query`,
  `shortest_path_nodes_query`, `scan_edges_arrow`, `scan_edges_table_arrow` or
  `scan_nodes_arrow`; the plan reads and masks itself (#192).
- **Behaviour change:** `louvain` partitions differ from 0.4.1 for the same
  seed. Local moving now revisits only the neighbours of nodes that moved, and a
  level stops when a sweep's worth of work gains less than 1e-7 modularity (with
  a hard cap of 64 sweeps' worth), instead of sweeping until no node moves.
  Modularity is equal or higher on most graphs; results stay deterministic for a
  given seed and identical on any thread count. Louvain is now near-linear in
  graph size: 1M edges takes 2.2s, down from about 6 minutes (#125, #185).
- Faster and leaner; outputs are byte-identical unless noted. Timings on a
  4-core machine:
  - Repeated weighted collects over a `scan_edges` frame no longer rescan the
    file and rebuild the index. The evaluated `weight=` vector is cached on the
    index (up to two expressions), so a repeat skips the read entirely: weighted
    PageRank over a 10M-edge scan, 2.2s to 1.4s, and peak memory 1.0 to 0.68 GB
    (#179).
  - `direction="both"` builds its symmetric graph once per index, straight from
    the cached undirected adjacency, and it is its own transpose; the weighted
    view is built flat and shared within a query. PageRank with
    `direction="both"` on 10M edges: 0.91s to 0.34s; weighted, 4.3s to 1.4s and
    1.9 to 1.2 GB peak (#180).
  - A filtered or traversal-result edge frame computes its edge mask once and
    reuses it on later collects: a repeated masked `degree` over a 10M-edge scan,
    0.19s to 0.03s (#181).
  - Graph columns on a node attribute frame are attached by a dense-id gather
    instead of a hash join and a re-sort into table order: degree onto a
    1M-node table, 0.10s to 0.02s (#182). A float `group_by().agg(mean)` over
    such a frame now sums in table order; it previously followed the join's
    emit order, which varied in the last bits between runs.
  - Queries and scans reuse their DataFusion session, and scans of one object
    store with the same `storage_options` reuse one client and its connection
    pool. Local Parquet footers and statistics are cached across collects and
    revalidated by size and modification time, so a file rewritten in place is
    read afresh (#183).
  - Weighted closeness sums only the nodes each source reaches, so sparse
    graphs no longer pay O(n²): 80k nodes, 1.27s to 0.23s. `avg_path_length`
    and `diameter` reuse per-thread BFS scratch: about 12× faster on sparse
    graphs. Betweenness reduces its partial sums in bounded windows, so exact
    runs no longer hold O(sources/64 × n) memory; small samples run their
    sources in parallel, and unweighted Brandes reads predecessors from the
    in-adjacency (#184).
  - Building the index over compact integer ids (`0..n`, or any dense range, in
    any order) interns through a direct table instead of a hash map: about 1.4×
    faster in memory and up to 1.7× from a Parquet scan, with a smaller id map.
    Sparse ids switch to the hash map automatically. Id lookups over columns with
    no nulls skip per-row null checks (#186).

- Every relational step now runs in the engine, lowered to one DataFusion plan
  in the order written: `filter`, `select`, `with_columns`, `rename`, `sort`,
  `head`, `distinct`, `sample` and `group_by().agg()`, on any frame. Before,
  the engine ran a fixed-order tail of them (`filter → group_by → distinct →
  sample → sort → head → rename`) and pyarrow ran whatever didn't fit, after the
  engine returned. The order guarantees (unsorted output in input order, stable
  sorts, `distinct` and `group_by` at first occurrence, `sample` in input order)
  are implemented once, and cost an ordinal only above an operator that
  reorders rows (#191, part of #189). This lifts these limits:
  - two `with_columns` steps in one plan, and `with_columns` after any other
    step: it computes over the rows and columns the steps before it leave, and
    an algorithm's column attaches to them by the node id column;
  - `with_columns` of expressions other than graph verbs, such as
    `with_columns(v2=ur.col("v") * 2)`; a keyword naming an existing column
    replaces it in place;
  - a tail (`filter`, `sort`, `head`, ...) after `describe()`;
  - `sort` by an expression, such as `sort(ur.col("v") * -1)`;
  - `select` of an expression, named by its `.alias()` or else its first
    column.

  On a 1M-node graph (4 cores, medians of 6 interleaved runs): a `filter` after
  an algorithm, 6.8 ms to 5.7 ms; a `head`, 2.3 ms to 2.5 ms (within noise); a
  `filter` + `group_by` (a count, a float sum and a float max) + `sort` tail over
  a 1M-row attribute frame, 26 ms to 39 ms. The float aggregations now fold each
  group's values in input order, as a plain frame always has, which keeps every
  bit of a sum reproducible but runs them less in parallel.
- Joins run in the engine, anywhere in a plan (#191, part 2 of 3, part of
  #189). The right side of a `join` is built like any frame, with its steps
  lowered, instead of being executed as a separate plan; both sides are put in
  order, numbered and joined in one DataFusion plan, and the steps after the
  join lower onto it. This lifts these limits:
  - any number of `join` steps in one plan (`a.join(b).join(c)`), before or
    after any other step, `with_columns` included;
  - `join` on a traversal result (`hop`, `shortest_path`), after a
    `random_walk`, and after `describe()`, on either side of the join;
  - a right side that is any frame: a graph column, an algorithm result, a
    traversal or another join.

  `how` takes `"right"`, `"outer"` (also spelled `"full"`, Polars' name),
  `"semi"` and `"anti"` as well as `"inner"` and `"left"`. The output is the
  left frame's columns, then the right frame's non-key columns (none for
  `semi`/`anti`); each key appears once, holding the right key on a row only the
  right frame has, with the left key's type for every `how` (a `right` or
  `outer` join whose right key holds a value that type can't raises a
  `ValueError`). Null keys never match. The row order is fixed:
  - `inner`, `left`, `semi`, `anti`: the left rows in order, each followed by
    its matches in right-frame order (as before for `inner` and `left`);
  - `right`: the mirror image, the right rows in order, each followed by its
    matches in left-frame order;
  - `outer`: `left`'s order, then the unmatched right rows in right-frame
    order.

  An unknown `how` now says which are supported: `join how='cross' is not
  supported (use ['anti', 'full', 'inner', 'left', 'outer', 'right', 'semi']).`
  On a 4-core machine (medians of 6 interleaved runs): a plain join of two
  1M-row tables, 71 ms to 68 ms (within noise); a 1M-row attribute frame
  joined to a degree result, 74 ms to 73 ms. A two-join pipeline over them
  runs as one plan in 98 ms, against 115 ms as two collects with the first
  result re-ingested, the only way to write it before.
- Graph ops run over reshaped edge sets (#191, part 3 of 3, part of #189). A
  graph algorithm, traversal, `random_walk` or `describe` over an edge frame
  after `distinct`, `sample`, `join`, `group_by().agg()`, `sort` or `head` runs
  over the graph of that frame's rows: exactly what collecting the rows and
  building a new frame with `ur.from_arrow(...)` gives, node order and row
  order included. For example `ur.pagerank(e.group_by("s", "d").agg(w=ur.col("w").sum()),
  weight=ur.col("w"))`, `ur.degree(e.join(allowed, on="s", how="semi"))` or
  `ur.hop(e.distinct(), 2).from_([0])`. A `filter` written after the reshaping
  step is a subgraph view of the reshaped graph, as on any edge frame;
  `select`, `rename` and `with_columns` keep the rows, so a weight or filter
  after them reads the columns they leave. The edges must keep their `src` and
  `dst` columns; a step that drops one raises `ColumnNotFoundError` naming the
  step (a `rename` does not move a role).

  The topology of a reshaped edge set is cached for the session under a
  fingerprint of its edge plan: the plan as written, an in-memory table's
  identity, and a local file's size and modification time (a rewritten file is
  read again). Repeated collects reuse it, with its weight, mask and
  symmetric-view caches, as a source frame's index is reused (#179). The cache
  holds four topologies, least recently used evicted, each a whole graph index
  with its weights and masks, for the life of the process; a plan over a
  remote file, or a `sample` whose seed has no exact JSON form (bytes, an int
  past 64 bits, a non-finite float), builds its topology on every collect. This lifts these limits:
  - a graph algorithm over a `distinct`, `sample`, `join` or `group_by` result;
  - a step other than `filter` on an `EdgeFrame` before `nodes()`:
    `e.head(100).nodes()` has the node set of those rows;
  - graph columns over different edge frames in one `with_columns`
    (`nodes.with_columns(pr=ur.pagerank(e), d=ur.degree(e.filter(...)))`). Each
    edge frame's columns attach to the frame's rows by node id, as an attribute
    frame's always have; a node another graph lacks gets null there. On
    `edges.nodes()` the first graph column's edges give the node set (or the
    reshaped edges `nodes()` was called on);
  - `with_columns` of scalar expressions on an `EdgeFrame`.

  On a 4-core machine (medians of 5 interleaved runs): weighted pagerank over a
  10M-edge frame aggregated by `group_by("s", "d")` takes 6.0s on its first
  collect and 1.7s repeated, against 6.2s for collecting the aggregate and
  re-ingesting it every time. A `with_columns` of pagerank over 10M edges and
  degree over a filtered view of them, on a 1M-row attribute frame, takes 0.25s
  in one collect against 0.27s as two.
- **Behaviour change:** a graph op over an edge frame whose `select`, `rename`
  or `group_by` dropped its `src` or `dst` column raises `ColumnNotFoundError`:
  `a graph op needs the edge frame's src column 's', but rename() renamed it to
  'a'; a rename does not move the src role, ...`. A traversal, `random_walk` or
  `describe` over such a frame used to ignore the step (#191).
- **Behaviour change:** a step behaves the same wherever it runs. Before, the
  same step could give different results on a frame the engine ran (after a
  graph algorithm, a traversal or a join) than on a plain frame, depending on
  where it fell in the fixed-order tail. Every step now has the plain frame's
  semantics:
  - `sample(seed=)` after a graph algorithm, join or group_by now picks the
    same rows as on a plain frame; picks for a given seed differ from 0.4.x.
    Rows are drawn with the stdlib's seeded generator over the row positions,
    so any seed Python's `random` accepts works there too (a negative, string
    or very large seed used to raise).
  - `group_by().agg()` float `sum`, `mean`, `min` and `max` follow pyarrow's
    rules on every frame: values are added in input order, `min`/`max` skip NaN
    unless every value is NaN, and ties keep the first value. On an engine
    frame `max` could return NaN, or a value that was not the maximum, when the
    group had NaNs. An integer or boolean `mean` adds its values as floats in
    input order too, so it gives the same bits on any number of cores.
  - Every `group_by().agg()` output column is nullable, as on a plain frame (an
    engine frame marked `count`, `n_unique` and keys that held no null as
    non-nullable).
  - An unknown column raises `ColumnNotFoundError` on every frame and every
    step (`filter`, `select`, `with_columns`, `rename`, `sort`, `group_by`,
    `agg`), with one message shape: `sort() references unknown column 'x'; the
    frame has ['a', 'b'].` Before, the class depended on the step and the frame:
    `ValueError`, `ColumnNotFoundError` or `ComputeError`, with differing
    messages. `ColumnNotFoundError` now subclasses `ValueError` as well as
    `UrsaError`, so `except ValueError` still catches it.
  - `head(n)` with a negative `n` keeps no rows on every frame; an engine frame
    raised `OverflowError`.
  - A `select` or `rename` that would give two columns one name raises
    `ValueError` before the plan runs: `select() would produce duplicate column
    'f'.` A plain frame returned both columns (`select("f", "f")`, or
    `rename({"f": "g"})` when `g` exists).
- `group_by().agg()` with a float `sum`, `mean`, `min` or `max` (or an integer
  or boolean `mean`) no longer reads its rows through one serial hash
  repartition (#204). The rows are cut into fixed-size chunks, each chunk is
  hash-partitioned by the group key in parallel, and every partition reads its
  chunks' rows back in chunk order. Each group still folds its values in input
  order, so results are unchanged, bit for bit, on any number of cores. On 10M
  rows (4 cores, medians of 5 interleaved runs), before → after (the release
  before #191 part 1 in brackets):

  | `agg` | ~1k groups | ~1M groups | one group holds half the rows |
  |---|---|---|---|
  | float `sum` | 0.083s → 0.067s (0.100s) | 0.69s → 0.21s (0.46s) | 0.57s → 0.28s (0.43s) |
  | `count` + `sum` + `max` | 0.106s → 0.072s (0.108s) | 0.95s → 0.28s (0.64s) | 0.81s → 0.36s (0.61s) |
  | `count` alone | unchanged, 0.027s | unchanged, 0.19s | unchanged, 0.20s |

  PageRank over edges grouped by `(s, d)` with a float sum (10M edges), 3.96s
  to 2.08s, with peak memory 2.29 GB to 1.71 GB. The `filter` + `group_by` +
  `sort` tail over a 1M-row attribute frame, 23 ms to 22 ms (17 ms before #191
  part 1): its ~50 groups spread over 4 partitions only as evenly as they hash.

### Fixed

- **Behaviour change:** a CSV column of numbers whose first 1,000 rows hold a
  float spelling that arrow-csv's type inference reads as text (`+5`, `+.5`,
  `Inf`, `-nan`, `NAN`, `Infinity` and the other cases and signs of `nan`,
  `inf` and `infinity`) is `double`, as pyarrow reads it. It was `string`
  (even its `1.5`s), so a weight column with one such value failed in a
  weighted algorithm with a type error. Each value parses to pyarrow's bits:
  `+5` is `5.0`, `-nan` keeps its sign. `1e400` (`inf`), `-0` next to a float
  (`-0.0`), `nan`, `inf` and `-inf` already read as pyarrow reads them. A
  column of integers only, or with any other text in it, keeps its type, and
  so does every column without such a spelling. This holds with and without
  a `null_value`. `nan` still reads as NaN, as before; pyarrow's default
  options read it as null. Pass `null_value="nan"` to read it as null. A scan whose sample has a column of text reads that sample a
  second time, to check its spellings (#221).
- Long chains no longer crash the process (#216). `ur.degree` over 20,000
  chained filters segfaulted on the default stack (a recursive clone of the
  plan), and collecting a 20,000-step chain segfaulted on a 256 KB thread
  stack (a recursive drop). A plan's inputs are now shared, so cloning a plan
  copies one step, and dropped a step at a time; the reshaped-topology
  fingerprint walks the chain in a loop; plan (de)serialization and `Debug`
  move onto a heap-allocated stack segment when the thread's runs low. A
  frame with more than 64 relational steps runs its `collect()`, `schema()`,
  `explain()` and graph index build on a stack sized to it, since DataFusion's
  optimizer and planner recurse once per operator: a node frame filtered
  10,000 times after a graph column segfaulted on the default stack there.
  Plans of up to 64 steps run as before; the stack a plan may reserve is
  capped at 1 GiB (about 65,000 relational steps), past which `collect()`,
  `schema()` and `explain()` raise `UrsaError` naming the limit and saying to
  collect part of the chain first. `collect()`, `schema()`, `explain()` and
  graph ops over 20,000-step chains now work on a 256 KB thread stack.

- **Behaviour change:** `ur.density`, `ur.diameter` and `ur.avg_path_length`
  over a filtered edge frame or a `hop` / `shortest_path` result read its
  subgraph view, as `describe` and every graph op do (#210). They used to
  compute over the whole parent graph, ignoring the filter: on the issue's
  repro `ur.density(f)` was 0.333 (now 0.1667), `ur.diameter(f,
  approximate=False)` 2 (now 1) and `ur.avg_path_length(f)` 1.333 (now 1.0).
  The view is the parent's node set and only the kept edges, so
  `ur.density(f) == ur.describe(f).collect()["density"][0]`, and an exact path
  stat equals its value over a new frame of the kept rows. A sampled estimate
  (`avg_path_length(sample=)`, the default approximate `diameter`) picks its
  sources among the nodes at an end of a kept edge. Since the view's filter
  now runs, an error only the filter's rows show (an integer divide by zero) is
  raised by the stat too.
- `group_by()` by a dictionary (categorical) column with `int8`, `int16`,
  `uint8` or `uint16` codes, and `distinct()` over one, raised "Dictionary key
  bigger than the key type" when the result had more groups than the codes
  index: a key whose batches carry dictionaries of their own, or a second key
  that multiplies the groups. The aggregate now indexes such a key with
  `int32` codes and the result narrows them back to the column's type when its
  values fit; a result with more distinct values than the type indexes keeps
  `int32` codes.
- `describe()` over a filtered edge frame reported the unfiltered graph: every
  edge was counted, while `degree()` over the same frame read the filter as a
  subgraph view. It now reads the same view: the parent's node set, and only
  the kept edges for `n_edges`, `density`, `avg_degree` and `n_components`. A
  traversal result is described as the subgraph its reached nodes induce, and a
  filter above a reshaping step (`e.distinct().filter(p)`) masks the reshaped
  rows' graph. For `s = [0, 0, 1, 2]`, `d = [1, 2, 2, 3]`,
  `w = [0.1, 0.9, 0.9, 0.1]`, `ur.describe(e.filter(ur.col("w") > 0.5))` was
  `n_nodes=4, n_edges=4, density=0.333, avg_degree=1.0` and is now
  `n_nodes=4, n_edges=2, density=0.167, avg_degree=0.5` (#207). As with
  `degree`, the node set stays the parent graph's. So over a traversal result
  `n_nodes` is the whole graph's, every unreached node is its own component,
  and `density` divides by the parent's node count. A 10-node neighbourhood of
  a large graph has a density near 0.
- A Parquet scan could read a column as a single repeated value. With row
  groups `[NaN, NaN]` and `[1.0]`, or one row group `[NaN, 1.0, 1.0]`,
  `ur.scan_edges(p, ...)` read float column `r` as `[1.0, 1.0, 1.0]`; with
  string row groups `["a", "a"]` and two strings over 4 KiB, it read all four
  rows as `"a"`. A weight, filter or graph op over the column saw the same
  wrong values; `pq.read_table` and an in-memory frame read what was written,
  as the scan now does. Parquet statistics carry no min/max for an all-NaN
  float row group or a string past pyarrow's 4 KiB limit, and leave NaN out of
  float bounds. DataFusion 54 still marked the file's bounds exact, and read a
  column whose exact min equalled its exact max as that constant. Scans now
  keep a column's file-level min/max exact only when every row group backs
  them, and never for a float column: the bounds still prune, but no longer
  stand in for the values (#202).
- A comparison on a dictionary-encoded float column (`ur.col("w") > 1` on a
  categorical of floats, in memory or from Parquet) raised "Unsupported data
  type Dictionary(Int32, Float64) for function isnan". It now compares by
  value under the same total order as a plain float column (#199): NaN is
  above every number (#202).
- `sort()` orders floats as filters compare them (#201): every NaN is above
  every number whatever its sign bit, and `-0.0` equals `0.0`, so they tie and
  keep input order, dictionary-encoded float columns included. A NaN with the
  sign bit set, the usual result of `0/0`, used to sort before every number
  after a graph algorithm or a join. A descending sort now puts NaN first on
  every frame; a plain frame used to put it last in either direction. Nulls still come last. For `r = [1.0, NaN, None,
  2.0]`, `sort("r", descending=True)` was `2.0, 1.0, NaN, None` on a plain frame
  and is now `NaN, 2.0, 1.0, None`.
- On a frame the engine ran (after a graph algorithm or a join), a second
  `distinct()` over a dictionary-encoded (categorical) column could return
  duplicate rows (`f.distinct().select("c").distinct()`), and a `distinct()`
  after `group_by().agg()` raised "Schema contains duplicate unqualified field
  name" (#191).
- `hop`, `shortest_path` and `random_walk` over a filtered edge frame ignored the
  filter: `ur.hop(e.filter(ur.col("w") > 5), 2)` reached nodes through edges
  the filter removed. They now follow only the kept edges, and a graph op over
  such a traversal result (`ur.degree(ur.hop(e.filter(...), 1).from_(s))`)
  stays inside the filtered subgraph (#192).
- A graph op over a traversal of a reshaped scan
  (`ur.pagerank(ur.hop(ur.scan_edges(p).sample(100), 1).from_([1]))`) ran over
  the whole file's index; it now runs over the sampled rows' graph, as the
  in-memory equivalent does (#190, #191).
- `hop`, `shortest_path`, `random_walk`, `describe` and a graph column on an
  attribute frame ignored a `sort` or `head` on their edges, and ran over the
  whole graph: `ur.random_walk(e.head(3), ...)` walked edges `head` dropped.
  They now run over the graph of the kept rows (#191). A `sort` also changes
  the order rows come back in: `ur.hop(e.sort("w"), 2)` and
  `ur.shortest_path(e.sort("w"), ...)` follow the sorted rows' node and edge
  order (as a frame built from those rows does), not the unsorted frame's.
- The same graph ops ran over the original endpoints after a step that wrote
  new values into the `src` or `dst` column: `e.rename({"s": "d", "d": "s"})`
  or `e.with_columns(s=ur.col("s") + 10)`. They now read the endpoints from
  the rows the step leaves, as a frame built from those rows does (#191).
- A weight, or a filter on the edges, read the source's column where a
  `with_columns` on the edges had replaced it, or a column `select` had
  dropped: `ur.shortest_path(e.with_columns(w=ur.col("w") * 2), a, b,
  weight=ur.col("w"))` used the original `w` (#191).
- Graph columns on a 0-row in-memory node frame return an empty frame with the
  table's columns plus the computed ones, instead of raising "No field named
  __ursa_gid" (#153, #182).

### Security

- Bumped `rustls-webpki` 0.103.15 → 0.103.16 (RUSTSEC-2026-0382) and `tokio`
  1.53.1 → 1.54.1 (RUSTSEC-2026-0386). RUSTSEC-2026-0391/0398/0403/0410 in
  `zstd` 0.13 are waived in `deny.toml`: the fixes need zstd 0.14, which arrow,
  parquet and DataFusion 54 do not accept yet, and none of the affected code is
  reached (#188).

## [0.4.1] — 2026-10-19

### Fixed

- Unsorted output now has a fixed row order on the engine paths that had none:
  an attribute frame with graph algorithms (`ur.from_arrow(attrs, id=...)
  .with_columns(pr=ur.pagerank(edges))`) returns the attribute table's row
  order, and `join()` returns the left rows in order, each followed by its
  matches in right-frame order. `group_by().agg()` and `distinct()` return rows
  in order of first appearance, and `sample()` keeps its picks in input order,
  on every path including plain frames. `head(n)` takes the first `n` rows of
  that order. Before, the order changed with the number of CPUs (DataFusion's
  partition count) on large inputs (#168).

## [0.4.0] — 2026-10-18

### Added

- `sort()` takes several keys, Polars-style: `sort(["club", "pagerank"],
  descending=[False, True])` or `sort("a", "b")`. `descending` is one bool for
  every key or one per key; later keys break ties; nulls sort last for every
  key; dictionary-encoded keys sort by value. Works on plain, scanned and edge
  frames, grouped and joined results, graph-derived frames and traversal
  outputs (#163). The sort is stable on every path: rows that tie on every key
  keep their input order (#165).

- `direction=` on `density`, `avg_path_length`, `diameter`, `describe`,
  `pagerank`, `closeness` and `betweenness`. The default stays `"out"`, so
  existing results are unchanged. `direction="both"` reads the rows as a simple
  undirected graph: each pair of nodes joined in either direction is one edge,
  and self-loops are dropped. It matches NetworkX on `G.to_undirected()`: on
  Zachary's karate club, density 0.139, `avg_path_length` 2.41 and diameter 5,
  against 0.0695, 1.27 and 3 with the default (#124). `closeness` also takes
  `"in"`; `pagerank` and `betweenness` reject it. See *Semantics vs NetworkX*.
- `join()` and `group_by().agg()` now compose in one lazy pipeline, in either
  order, and run as written: `join(...).group_by(k).agg(...)` groups the joined
  rows, and `group_by(k).agg(...).join(...)` joins onto the grouped result.
  Previously this raised `NotImplementedError` and needed two `collect()` steps.
- `from_networkx(..., symmetrize=)` controls whether reverse rows are written.

### Fixed

- A `sort()` after a graph algorithm or traversal returned rows that tie on
  every key in an order that depended on the machine's core count, not in
  input order. Ties now keep their input order, as on a plain frame: node
  order (or the attribute table's row order) after a graph algorithm, and the
  traversal's own output order after `hop`/`shortest_path`/`random_walk`
  (#165).
- `select("c").distinct()` kept duplicate rows on plain frames, because
  `distinct` ran over every column before `select` narrowed them (#162).
  Other orderings that were silently reordered are fixed too: `head` before
  `sort`/`filter`/`distinct`/`sample`/`join` now takes the first rows as
  written, a rename that swaps two names no longer mislabels the columns, and
  two `sort`s on a graph-derived frame no longer drop the first.
- `rename(...).sort(new_name)` (and `filter`/`select`/`group_by` on the new
  name) failed with "unknown column", because rename was applied last (#164).

- A plain `collect()` on a `scan_edges` frame returned only the endpoint
  columns, so `filter`/`sort`/`select`/`group_by` on any other edge column
  failed with "unknown column", and `sink_parquet`/`sink_csv` wrote endpoints
  only. A scanned edge frame now has every column of the file, in file order
  and with the file's types, exactly like the same table loaded in memory;
  a `select()` is pushed down into the scan. Graph ops still read only the
  columns they need, so index-build memory is unchanged (#160). Scanned
  endpoints also keep their file type in relational output (an `int32` column
  stays `int32`) instead of being widened to `int64`.
- Weight expressions on a `scan_edges` frame can reference the `src`/`dst`
  columns; previously they failed with "No field named ..." (#154).
- `sort()` on a dictionary-encoded (categorical) column raised
  `ArrowNotImplementedError`, and so did `sum`/`mean`/`min`/`max` over one in
  `group_by().agg()`. Sorting now orders by the values (not the dictionary
  indices), and the column keeps its dictionary type in the output (#159).
- `scan_edges` and `scan_nodes` now return rows in file order on every run.
  A file large enough to be read as several parallel partitions previously
  came back in a run-dependent order, so dense ids, component labels,
  `edge_ids` and PageRank's last bits changed between runs over the same
  file. Output over a scan now matches `from_arrow` over the same table
  exactly (#148).
- `scan_edges` and `scan_nodes` over a Parquet file with string ids failed
  with "Utf8View is not a supported id type", even for a plain `string`
  column, because DataFusion reads Parquet strings as `Utf8View`. Scans now
  read string columns as the file declares them, and the id map reads
  `Utf8View` natively (no cast, so the 2 GiB `Utf8` cap is not involved).
- Every string layout now works as node ids, scanned or in memory: `string`,
  `large_string`, `string_view` (including tables that came from polars
  through the Arrow C stream) and dictionary-encoded (categorical) ids, which
  previously raised `TypeError` or "Dictionary(Int32, Utf8) is not a supported
  id type".
- `filter()`/`sort()` on a `scan_nodes` frame with a string attribute column
  failed with `ArrowNotImplementedError` (no `string_view` kernel); string
  attributes now arrive as `string`. `neighbors().agg(n_unique=...)` over a
  dictionary-encoded string attribute is treated as a string, not cast to a
  number.
- A kernel that asked for the lazily built transpose or undirected view from
  inside a parallel loop could deadlock, e.g. `neighbors().agg()` or `hop` with
  `direction="in"`/`"both"` on graphs above 65,536 edges.

### Changed

- **Behaviour change:** relational steps (`filter`, `select`, `rename`,
  `sort`, `head`, `distinct`, `sample`, `group_by().agg()`, `join`) now take
  effect in the order they are written, as in Polars, on every collect path.
  Previously each path applied them in a fixed internal order, which gave
  wrong results for some orderings (see Fixed) and rejected others. Orderings
  that used to raise now run as written: `sort`/`head`/`rename` before
  `group_by`, a `filter` after `group_by` (SQL `HAVING`), `select` after
  `group_by`, chained `rename`s, and `rename` then `join` on the new key. A
  few pipelines that used to succeed incorrectly now raise: a `filter` on a
  column's old name after `rename`, a `filter` on a column that an earlier
  `select` dropped, and a non-filter step on the edges before `nodes()`. The
  common filter/sort/head/select tail on graph-derived frames still runs in
  DataFusion.

- **Behaviour change:** `from_networkx` now writes each edge of an undirected
  `nx.Graph`/`nx.MultiGraph` in both directions (a self-loop stays one row), so
  the default `direction="out"` gives NetworkX's undirected values for
  `pagerank`, `closeness`, `density`, `diameter` and the rest. Previously each
  edge was written once, which silently produced directed results. Pass
  `symmetrize=False` for the old one-row-per-edge frame. `nx.DiGraph` input is
  unchanged. On a symmetrized frame `betweenness` counts each unordered pair
  twice with the default; pass `direction="both"` to match NetworkX (#124).
- `.to_polars()` no longer appears in first-contact examples (the module and
  `ursa.datasets` docstrings, the Quickstart, `examples/quickstart.py`), which
  now print `collect()` or use `.to_arrow()`/`.to_dicts()`, so they run on a
  bare install. Polars remains an optional extra; its one example lives in the
  Output guide.
- The package summary (`pip show ursa-graph`) and the README install line now
  name the import name, `ursa`.

- Building the graph index over an unweighted `scan_edges` source no longer
  holds the scanned Arrow endpoints for the whole build. Each batch is interned
  and freed as it is decoded, inside one native call, so the 16 B/edge input
  never accumulates. On a 30M-edge Parquet file the build's peak memory fell
  from about 45 to 24 B/edge, and the footprint after the build from about 33
  to 17 B/edge, with build time unchanged (#149). Weighted scans keep the
  previous path, since their weight columns must outlive the build.
- Faster and leaner throughout; every kernel's output is unchanged unless
  noted. Timings on a 4-core machine:
  - Index build: known ids are interned in parallel, and scans read 128K-row
    batches. A 30M-edge Parquet scan and build went from ~2.9s to ~1.65s;
    with 2M string ids, from ~8.5s to ~3.1s.
  - In-memory frames (`from_arrow`, `from_polars`, `EdgeFrame(...)`, ...)
    no longer keep a concatenated copy of the `src`/`dst` columns next to the
    table; they share its chunks. On 30M int edges the build went from 2.7s
    to 1.6s (27 to 22 B/edge over the table); on 20M string edges, from 6.9s
    and 71 B/edge to 1.9s and 24 B/edge.
  - String node ids are stored once, in the id column's own buffers, instead
    of about 140 B of per-id overhead. On 20M edges and 2M ids the build peak
    fell from 36 to 27 B/edge.
  - The transpose that in-direction kernels use is built straight from the
    out-CSR, without per-row endpoint arrays, so it is no longer the peak:
    35 to 30 B/edge on a 30M-edge graph.
  - `triangle_count` uses the degree-ordered forward algorithm (6–7× faster
    on skewed graphs), and `clustering_coefficient` benefits too.
  - `label_propagation` (6×) and `louvain` (3–4×) count votes and community
    weights in dense arrays instead of hash maps. Louvain's aggregation now
    sums in a fixed order rather than a randomly seeded hash map's.
  - `pagerank` divides by out-degree once per node, not per edge (1.8×).
  - Weight expressions are compiled once and evaluated per batch, and
    columns that share an expression share one evaluation.
  - Smaller items: masks built from packed bits, a bounded CSR-build
    histogram on many-core machines, and a leaner strongly-connected-components
    kernel.
- An edge mask whose length does not match the graph's edge count is rejected
  instead of silently dropping the edges past its end.

### Security

- Bumped `rustls` (pulled in by the object-store TLS stack used for
  `s3://`/`gs://`/`az://`/`https://` scans) from 0.23.45 to 0.23.46 for
  RUSTSEC-2026-0346, -0347, -0348 and -0349.

## [0.3.0] — 2026-10-08

### Added

- `ursa.datasets`: bundled graphs that load offline from the wheel (`karate`
  with faction labels, `lesmis` weighted, `florentine`, `kite`), plus
  `facebook` (SNAP ego-Facebook), downloaded on first use and cached under
  `$URSA_DATA_HOME`. `list_datasets()` returns the registry.
- Interop constructors `from_edgelist`, `from_networkx`, `nodes_from_networkx`,
  `from_numpy` and `from_scipy_sparse`. networkx, numpy and scipy remain
  optional and are imported only when the matching constructor is called.
- `group_by(*keys).agg(...)`, with `mean`, `sum`, `min`, `max`, `count` and
  `n_unique` aggregations, optionally aliased or named through `agg(name=...)`.
- `join(other, on=, how=)`: an equi-join of two frames on shared key columns,
  `inner` or `left`. Distinct from the automatic attribute attach.
- `rename(mapping)` and `sample(n, seed=)` on the shared relational tail.
- Filter predicates now lower as a full algebra: string and numeric
  comparisons, boolean `&` / `|` / `~`, arithmetic, and column-to-column
  comparisons. Previously only a single `col <op> number` comparison executed.
- Graph operations over a filtered edge frame (subgraph views), and over a
  traversal result from `hop` or `shortest_path`.
- `dtype="f32"` on the float-valued kernels (`pagerank`, `closeness`,
  `betweenness`, `clustering_coefficient`) and on `neighbors().agg()`, which
  emits the result column as 32-bit float. The kernel still accumulates in f64
  and the emitted value is that result cast down, so it agrees within f32
  precision. The integer-valued kernels take no `dtype`. The default output is
  unchanged.
- `http(s)://` reads for a single hosted Parquet or CSV file.
- `on_null="drop"` on edge inputs (`EdgeFrame`, `from_arrow`, `from_edgelist`,
  `scan_edges`, `read_edges`), which filters rows with a null `src` or `dst`
  and reports the dropped count as a warning. The default remains `"error"`.
- Documentation site at <https://cldixon.github.io/ursa/>: a quickstart, one
  guide per task, a reference, a limits page, and a semantics reference that
  pins each kernel to the NetworkX call it is checked against. Every example
  on the site runs in the test suite.

### Changed

- A query naming several graph algorithms now shares work across its columns
  instead of recomputing per column. Two columns naming the same kernel with the
  same parameters run it once — including the same kernel at two output dtypes,
  since `dtype=` narrows on emit and so is not part of a computation's identity.
  Separately, `triangle_count` and `clustering_coefficient` in one
  `with_columns` now share the sorted-adjacency intersection pass they both rest
  on, where previously each ran its own; over a subgraph view they also share the
  masked undirected adjacency, which is rebuilt per use rather than cached. Values
  are unchanged and bit-identical either way.
- `rayon` is now an on-by-default feature of the `ursa-core` crate. With it
  disabled the crate compiles for `wasm32-unknown-unknown`, single-threaded, and
  `rand`'s default features — and so `getrandom` — are dropped, since the
  stochastic kernels use only seeded ChaCha streams. Serial and parallel outputs
  are bit-identical. This affects Rust consumers of `ursa-core` only: the Python
  wheels build with default features and are unchanged.
- `repr()` of a collected frame renders the head as a table with the shape,
  column names and dtypes, capped at 10 rows. It previously showed only a
  shape and a column list, which left results opaque in a REPL. The preview
  reads only the rows it displays, so its cost does not scale with row count.
- `to_polars()` raises a `ModuleNotFoundError` naming the `ursa-graph[polars]`
  extra when polars is absent, in place of a bare import error.
- The README documents installation, and the PyPI project metadata links the
  documentation site.
- `version`, `ModuleType` and `PackageNotFoundError` are no longer exposed in
  the `ursa` namespace. `__version__` and `__core_version__` are unchanged.

### Fixed

- `edges.filter(...).collect()` and `edges.distinct().collect()` return rows;
  both previously raised `NotImplementedError`.

## [0.2.0] — 2026-08-01

Performance improvements and syntax standardization.

## [0.1.1] — 2026-07-23

Fixed a missing Arrow dependency.

## [0.1.0] — 2026-07-21

Initial release.

[Unreleased]: https://github.com/cldixon/ursa/compare/v0.5.0...HEAD
[0.5.0]: https://github.com/cldixon/ursa/compare/v0.4.0...v0.5.0
[0.4.1]: https://github.com/cldixon/ursa/compare/v0.4.0...v0.4.1
[0.4.0]: https://github.com/cldixon/ursa/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/cldixon/ursa/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/cldixon/ursa/compare/v0.1.1...v0.2.0
[0.1.1]: https://github.com/cldixon/ursa/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/cldixon/ursa/releases/tag/v0.1.0
