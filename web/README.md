# Ursa Web Shell

A web page where anyone can run Ursa SQL in their browser, over sample graphs
stored in R2 (#279, part of #277). It is the `ursa` shell itself, in a
terminal, as DuckDB's web shell is DuckDB's CLI: the shell (dot-commands, SQL a
line at a time, DuckDB-style tables) is the `ursa-shell` crate, compiled into
`ursa-wasm` as `WebShell` and run in a Web Worker; the page draws it with
xterm.js. Around it is a bar laid out after DuckDB's web shell: New, Share,
Import, Graph, a Datasets menu and a light/dark toggle. The engine reads Parquet and
CSV over HTTP with range requests. The Worker here serves files and does no
query work.

The setup follows the Plain Text site's (`cldixon/plane-text`, `site/cloudflare/`):
bun for the scripts, the `cf` CLI to build and deploy, a Worker in front of
static assets and an R2 bucket.

## Layout

- `public/`: the page (`index.html`, `app.js`, `style.css`), the Datasets
  menu (`datasets.js`) and the engine's Web Worker (`engine-worker.js`).
  - **The terminal** is xterm.js (`@xterm/xterm`, copied into `dist/vendor/`
    by the build), with a line editor in `app.js`: Enter sends the line to the
    shell, which runs a statement once it ends with `;` and shows the
    continuation prompt meanwhile; Shift+Enter adds a line to what is typed;
    ↑/↓ recall history (kept in this browser); Ctrl+C copies a selection, or
    drops what was typed; Ctrl+L clears; Tab and Shift+Tab move the focus out
    of the terminal, to the bar (there is no completion); `.help shortcuts`
    lists every key (the `SHORTCUTS` text in `app.js`, which the e2e test
    checks against the keys). The cursor moves and deletes by the character
    as seen (an emoji sequence, a flag, a letter and its accent), and CJK and
    emoji take two cells, by the width table `app.js` gives xterm. Keys pressed while a
    line runs wait for it. What the shell prints (tables, errors, colour) is written to the
    terminal as is, so a selection copies as the text on screen.
  - **Dot-commands** are the CLI's (`.help`, `.tables`, `.describe`,
    `.functions`, `.maxrows`, `.maxwidth`, `.timer`, `.mode`, …; `.help graph`
    is a page on the graph SQL, whose forms the e2e test runs), and the page's
    own, which the shell lists in `.help` and hands back to the page:
    `.clear`, `.dataset ?NAME?`, `.export FORMAT` (downloads the last result as
    CSV, Parquet, JSON or Arrow), `.files ?add?`, `.graph ?QUERY|NAME? ?AS …? ?SIZE BY x? ?COLOR BY y?`
    (`.help graph` lists every clause),
    `.history ?N?` and `.share`. The explore commands (`.expand`, `.find`, `.hide`, `.show`,
    `.undo`, `.redo`, `.graph history`) are the shell's own list
    (`ursa-shell`'s `EXPLORE_COMMANDS`), handed back the same way.
  - **Datasets.** Picking one starts a fresh session and runs its commands at
    the prompt, as if typed: its views and `CREATE PROPERTY GRAPH`, then two or
    three queries that show the graph functions, `GRAPH_TABLE` patterns and
    shortest paths, each after `--` comments that say what it shows (keep the
    explanations in the commands, not in JS comments). Long lines are broken
    at spaces to fit the terminal. `?dataset=karate` in the URL loads one on
    open. The menu works from the keyboard as a menu button (arrows, Home,
    End, Enter, Escape).
  - **Share** copies a link whose fragment holds the session's commands;
    opening it lists them, with control characters shown as escapes, and asks
    `Run N shared commands? [y/N]`: only `y` replays them, and a link's
    `.export` is skipped, so a link never downloads files. **Import** adds
    files from this computer (`WebShell.addFile`, at `memory://uploads/<name>`,
    where characters other than ASCII letters, digits, `.`, `_` and `-` become
    `_` and a name so changed gets a hash of the original) and defines a view
    over each, named after the file and quoted (`"données"`, `"order"`).
    **New** starts a fresh session. The theme follows the system until the
    toggle picks one, which is remembered; the terminal's colours follow it.
    A click on a bar button hands the focus back to the terminal; pressed from
    the keyboard, the button keeps it. Under 640px the bar is one row: the
    logo without its name, and under 420px icons for New, Share, Import and
    Graph.
  - **The Graph view** (`public/graph.js`, `public/graph-worker.js`):
    `.graph` (or the Graph button) draws the last result as a graph in a
    pane beside the terminal, or under it under 640px; `.graph QUERY` draws a
    query's and `.graph NAME` a property graph's (or a table's). The engine
    makes the graph (`ursa-engine`'s `graph::view`, through `WebShell.graph`
    and the `graph` op of the worker; see "The engine's graph" below), at
    most 2,000,000 edges and 4,000,000 nodes. `ursa-viz` draws it (WebGPU,
    else WebGL2) with its ForceAtlas2 layout running in `graph-worker.js`,
    nodes sized by degree unless asked otherwise.
  - **What a result is drawn as** (#433, design §3): rows are drawn by the
    first result → graph rule that matches them. The engine reads what each
    column is from its `ursa.*` metadata (`docs/sql/reference/metadata.md`),
    which the graph functions and `GRAPH_TABLE` write, and else from its
    name and type:
    1. **Paths**: a path column (`vertices(p)`, with `edges(p)` or
       `element_id(p)`), the steps of `shortest_path()`, or, unmarked, a list
       of ids. Each row is a path; the drawing is their union, each edge
       once, and the paths are the result's highlights.
    2. **Rings**: `seed`, `id` and `distance`, a `hop` result. The seeds and
       the nodes reached, with the graph's real edges among them
       (`induced_edges`), coloured by distance.
    3. **Edges**: `src` and `dst` by role, two node variables of a pattern,
       or by name `src`/`dst`, `source`/`target`, `from`/`to`, or else the
       first two columns when both are ids of one kind. A row an edge, as
       before; a numeric `weight` is an edge column.
    4. **Pattern matches**: three or more node variables of `GRAPH_TABLE`.
       One edge for each edge of the pattern whose two ends are in `COLUMNS`,
       each distinct edge once; `a.club` is a column of `a`'s nodes; nodes
       of two vertex tables are two labels, coloured by label.
    5. **Nodes**: one node key (`pagerank`, `algorithms`, a vertex table's
       `id`). The nodes, and the graph's edges among them; the first metric
       sizes them and the first category colours them, unless `SIZE BY` or
       `COLOR BY` says otherwise.
    6. **Not a graph**: the error names the columns and their types and what
       would draw them.

    An id is an integer, a string or a date: a float or a list is never a
    node (#427). The graph a hop's or a node table's edges are read from is
    the one its columns name (`ursa.graph`), or else the session's only
    property graph; with none the nodes are drawn without edges (a hop's with
    its seeds' edges one step away), and the status line says so. The pane's
    status line says what the result was drawn as (`34 nodes · 78 edges ·
    drawn as nodes of karate (id, pagerank, community) · Done`), with what
    was left out and what else it could have been, and so does the
    terminal's line (`Drawing 5 edges, 6 nodes as a path on karate (members,
    ties) (Esc closes)`). The messages name the `AS …` clause that overrides
    the rules (below). The result also carries what a
    later view draws: the paths, the seeds, each node's level (a hop's
    distance, a path's step) and the layout the rule asks for (rings or
    layers), which `graph-data.js` reads as `graph.layout = {kind, level}`.
  - **Saying what a result is** (#434): `.graph [SOURCE] [ON graph] [AS
    edges(src, dst [, weight]) | nodes(id) | path(nodes [, edges]) |
    hops(seed, id, distance)] [NODES (query)] [SIZE BY x] [COLOR BY y]
    [LAYOUT force|rings|layers]`, the clauses in any order after the source.
    - **`AS …` is rule 0**: it says what the rows are and names the columns,
      and nothing is inferred, so a result whose columns have any names is
      drawn: `.graph SELECT a.id AS x, b.id AS y FROM … AS edges(x, y)`.
      `AS edges` draws a row an edge (its optional weight is an edge
      column), `AS nodes` the nodes with the graph's edges among them, `AS
      path` a list of ids a row (with a list of the edges' rows), and `AS
      hops` a hop's rings. A name is matched as SQL matches it (in any case
      unless quoted). A column that is missing, or is not what the shape
      needs (an id for a node, a list of ids for a path, a whole number for
      a distance, a number for a weight), is an error that names it and
      lists the result's columns with their types.
    - **`ON graph`** names the property graph the ids belong to, where the
      columns do not say (`ursa.graph`) or say otherwise: the graph `AS
      nodes` and `AS hops` read their edges from, and the one `SIZE BY
      pagerank` runs on for rows drawn as edges.
    - **`LAYOUT`** sets the result's `encodings.layout`, whatever the rule
      chose: `LAYOUT force` lays a hop result out by its edges, `LAYOUT
      layers` puts its rings in rows. Rings and layers place a node by its
      level (a hop's distance, a path's step), so a drawing without one
      (edges, nodes, a property graph) refuses them and says what has one.
    - A property graph is drawn whole: `.graph karate AS …` and `.graph
      karate ON …` say so. SQL's own `AS` and `ON` stay the query's (an
      alias, a join's condition); only `FROM t AS edges(a, b)`, a table
      alias named for a form (or `edge`, `node`, `paths`, `hop`, which are
      told the form they meant) with a column list, is read as the clause.
      After a source that is one name, everything is clauses, so `.graph
      karate LAYOUT spiral` is told what `LAYOUT` takes, not handed to SQL.
    - `.help graph` lists the clauses (the shell adds them to the page where
      the host has a `.graph` command).
  - **Sizes, colours and the legend** (#389): `.graph [SOURCE] [NODES
    (query)] [SIZE BY x] [COLOR BY y]`, the clauses in any order after the
    source. `.graph karate SIZE BY pagerank COLOR BY louvain` sizes Karate's
    members by PageRank and colours them by Louvain community. A name is, in
    turn: a column of `NODES (SELECT id, … FROM …)` (its `id` column, or else
    its first, joined to the drawing's node ids); a column of the property
    graph's vertex tables (`COLOR BY club`); `degree` (of the drawing); or a
    per-node algorithm run on the property graph with the view's defaults
    through `algorithms(G, …)` (`direction => 'both'` where it has one,
    `seed => 1` where it is random): `pagerank`, `louvain`, `betweenness`,
    `closeness`, `label_propagation`, `connected_components`,
    `triangle_count`, `clustering_coefficient`. A source that is rows (a
    query, a table, the last result) takes `NODES` and `degree` only, unless
    its graph is known: by its columns, or by `ON graph`.
    `SIZE BY` needs numbers; `COLOR BY` colours the eight largest
    categories, the rest "other", and refuses a fractional column with more
    than eight values or any with more than eight that are over half the
    nodes (it suggests `SIZE BY`). The engine parses and resolves it all
    (`ursa-engine/src/graph/view/`) into two node columns of the result,
    from which the page reads each node's size and colour index and the
    legend's labels and counts. The legend, under the pane's
    header, names what sizes the nodes (with its range) and each colour's
    category and count, in text.
  - **The explore session** (#440, design §4): the engine keeps the graph
    `.graph` opened (`ursa-engine`'s `graph::view::Session`: the graph as it
    stands, what is shown, and the history), and the commands change it:
    `.expand ID [in|out|both] [LIMIT n]` adds a node's neighbours that are
    not shown (the 100 of highest degree, then by key, so it is the same
    every time; again for the next ones) and the graph's edges among every
    node; `.hide ID…` and `.show ID…|all`; `.undo` and `.redo`, with no
    query; `.graph history` lists the steps; `--sql` prints a command's SQL
    and changes nothing. The page hands each to the worker's `explore` op
    (`WebShell.explore`), which answers `{out, err, delta, nodes, edges}`:
    the line to print (`+16 nodes, +41 edges`), the change as one UGR delta
    buffer (transferred; `null` for none), and how many nodes and edges the
    engine shows after it. Closing the pane closes the engine's graph
    (`closeGraph`), after which the commands say none is open.
    `window.ursaShell.explored()` gives the last command's
    `{command, nodes, edges, deltaBytes, drawn}`, which the e2e test reads.
  - **Search and focus** (#445, design §6): `.find TEXT` selects the nodes
    of the graph drawn whose key starts with TEXT, then those whose label
    column (the vertex table's first text column that is not its key)
    contains it, whatever the letters' case; `.find WHERE condition` those
    the vertex table's rows satisfy (`.find WHERE club = 'Officer'`). The
    engine answers (`ursa-engine`'s `graph::view::find`: one statement for
    each vertex table, which `--sql` prints; it reads the explore session
    and changes nothing, so it is no step for `.undo`), through the worker's
    `find` op (`WebShell.find`): `{out, err, selected, matches, notShown,
    notShownTotal}`. `selected` is the view's indexes of the matches it
    shows, the best first and at most 1,000; `matches` is how many there
    are, which the status says past the cap (`Selected 1,000 of 2,345
    matching nodes: …`). `graph.js`'s `selectFound` selects them and fits
    them: the camera moves over 200 ms until the selection fills 80% of the
    canvas (zooming in by 8 at most; one node is centred at the zoom the
    view has), or at once under `prefers-reduced-motion`. `f` in the pane
    fits the selection again (`0` the whole graph). `/` in the pane, or the
    header's magnifier, opens a search box in the header
    (`graph-search.js`); Enter there types `.find` and the text into the
    shell, so a search is in the terminal like any command, and Esc closes
    the box alone. The matches the view does not show (`notShown`, the
    first 20) are listed under the header: a hidden node with a "show"
    button, which runs `.show KEY`; a node of the graph that was never
    drawn is named only, until `.graph +` can add it. A search that
    matches nothing in the view leaves the selection as it was.
    `window.ursaShell.found()` gives the last search's `{command, ms,
    matched, selected, notShown, notShownTotal}`, `ms` being the engine's
    round trip, which the e2e test holds to 50 ms on rmat.
  - **Click to expand, and the deltas drawn** (#443). The pane's view is
    made from the graph result itself (`createViewFromResult`), so it
    changes in place: `graph.js`'s `applyDelta` gives each explore
    command's delta to the view (`view.applyDelta`), which writes only what
    changed.
    - **The result's highlights are drawn**, as `ursa-viz` reads a
      result: a path's route is highlighted, a `hop`'s seeds are ringed,
      and the rest is dimmed. They follow the deltas (a hidden node's
      route edge is not drawn).
    - **What it costs** (#501, #492). A view made from a result holds
      the result, and the edges once. On `.graph rmat` (1M edges) the
      view's WebAssembly memory is 17.9 MiB (it was 59.6 when the view
      kept a graph beside the result, and 16 for the edge-list view of
      #426), and the result is decoded in pieces, out of the page's
      buffer, so no task while the view is made is over 60 ms (decoding
      was one of 63 to 92). The layout worker holds 55.7 MB (it was 101):
      it builds the adjacency a layout reads and no topology, and holds a
      dense coarse level's weights only while it lays that level out.
      Positions come from the worker in an array filled once from the
      layout and transferred, and the view copies it once.
      `tests/graph-timing.ts` fails if the view is over 20 MiB or the
      worker over 60 MB.
    - **What the page knows** (each node's id, its degree among what is
      shown, the legend, the counts in the header) is read again from the
      view's own result (`view.resultBytes()`) after each delta, and the
      page sets every node's size and colour again, as it does for the
      first drawing: sizes in CSS pixels, eight colours and "other".
    - **The layout grows.** The worker gets `{type: "grow", n, src, dst}`
      (the edges shown) and calls `Layout.grow`: new nodes start beside
      their neighbours, and the nodes there were are held or slow. The
      header reads `Placing the new nodes`, then `Done`. Each message of
      the worker carries the `epoch` of the delta it follows, and the page
      drops positions of a graph it no longer draws.
    - **Undo and redo put every node back.** The page keeps the positions
      before each step (up to 16 million numbers; older steps then undo
      where the nodes are) and hands them back with the inverse delta, so
      `.undo` is one frame with no layout, the same pixels as before the
      step, and `.redo` draws the step where it had settled. The worker is
      told where the nodes are (`{type: "place", …}`, `Layout.fromPositions`).
    - **A double-click on a node** (a double tap, or Enter in the pane with
      nodes selected) runs `.expand ID` through the terminal's queue, as a
      typed line runs: echoed after the prompt with a dimmed `from the
      graph` before it, answered in one line (`+16 nodes, +41 edges`), kept
      in `.history`. With Shift it is `.expand ID LIMIT 5000`. A
      double-click on empty space still fits. `.graph echo off` runs these
      commands without writing them or their answers (an error is still
      written, and `.history` still lists them); `.graph echo on` shows
      them again.
    - **The inspector waits for a double tap.** After a tap its panel
      (#444) opens 350 ms later, once a second tap would no longer be a
      double tap: under the view on a phone, it would otherwise open over
      the node and take the second tap. A mouse's click opens it at once.
    - **While a command runs** the next ones wait their turn (the engine
      runs one at a time), and each node waiting carries a busy ring: an
      element over the canvas (`.graph-busy`), moved with its node as each
      frame is drawn, still under `prefers-reduced-motion`.
    - **`+N`.** There is no label overlay yet to draw a badge, so the
      tooltip (`not shown +4`) and the header (`Selected node 2: degree 6,
      +4 not shown`) say it. N is the engine's count
      (`Session::not_shown`, the worker's `notShown` op): the node's degree
      in the property graph, both directions, less its edges shown here. It
      counts edges, as `degree` does, so parallel edges count each. The
      page asks for it after each drawing and each delta, without waiting.
    - **Timings and sharing.** Each expansion is an action of
      `graphTimings()`: `{command: ".expand 33", at, engine, delta, frame}`,
      from the double-click to the frame that drew its delta. On the
      million-edge R-MAT graph, from a `hop` result, ten double-clicks took
      118 to 190 ms each from click to frame (50 to 100 ms of it in the
      engine; headless Chromium, software WebGL2, 2026-11-22). A drawing
      (`.graph …`) and each explore command that changed it are now part
      of what Share replays, so a shared link draws the same exploration.
  - **The engine's graph** (#425): `.graph` is resolved in `ursa-engine`
    (`graph::view`: the clauses, the source, the ids as dense node indexes,
    `SIZE BY`, `COLOR BY` and `NODES`), not in `ursa-wasm`, so the CLI and
    the notebook can draw what the shell draws. It makes a graph result
    (`ursa-viz`'s `model`), which `WebShell.graph` hands the worker as one
    UGR buffer (`ursa-viz/UGR.md`), and the worker transfers it to the page:
    `{ugr, skipped}`. `graph.js` reads it with `ursa-viz/ui/ugr.js`
    (`build.ts` copies it to `dist/ugr.js`), as typed arrays over the buffer
    with no copy, and `public/graph-data.js` makes it the drawing: `n`,
    `src`, `dst`, each node's id (decoded when asked for), the size column
    as it is, and the colour column, a dictionary whose values are the
    categories by rank, as colour indexes and the legend. The result's
    provenance says the SQL, the rule that drew it (`graph` for a property
    graph, `path`, `rings`, `edges`, `pattern` or `nodes` for rows), the
    columns the rule read and its notes; `graph-data.js`'s `drawnAs` makes
    them the words the status line and the terminal show (#433).
    `bun test tests/graph-data.test.ts` runs that path without a browser,
    on the fixtures `ursa-engine`'s `tests/graph_view.rs` writes.
  - **Theme:** `ursa-viz` draws in the page's `--graph-*` tokens
    (`style.css`: background, edge ink, plain nodes, eight categorical hues
    and "other", stepped for each theme; and `--graph-highlight`,
    `--graph-selection` and `--graph-pin` for the view's highlight layer,
    #435: a selection's rings and edges, and, since the view is made from
    the result, #443, the result's own highlights, a path's route, with the rest
    dimmed, and a hop's seeds ringed), set with `view.setTheme(…)`; the
    theme toggle (or the system's) recolours an open view. No CSS filter.
  - **Progress** (#390): the pane opens with "Loading graph view…" as soon
    as the engine takes more than 150 ms (not at all, for a quick error),
    and starts loading `ursa-viz`, here and in the layout worker, then,
    while the engine still runs the query. The graph goes to the worker
    before the page builds its view, and nothing is drawn before the
    layout's first positions. The header then reads `… · Laying out · level
    k of n` (and, while a coarse level is drawn, `· G groups`), then
    `N nodes · M edges · Done`. `window.ursaShell.graphTimings()`
    gives when each step ended, and `bun tests/graph-timing.ts [url] [runs]`
    times `.graph rmat` (1M edges): in headless Chromium, the first drawing
    went from 4.6 s after Enter to 0.95 s, and the layout's end from 12.1 s
    to 6.0 s.
  - **Timings** (#430): `graphTimings()` also says what the view costs to
    use, so the targets of `design/graph-explorer.md` §9 can be read from
    the page:

    | field | what |
    |---|---|
    | `frames` | `count`, the frames drawn since the page loaded; `render`, the time in `view.render()` of the last 120; `interval`, the time from one frame's start to the next's, of the last 120 frames that input asked for (a drag, a wheel, a key) and that began within 100 ms of the page's thread being free of the frame before: the view moving. It is the frame's whole cost; `render` is only the call. Each is `{count, p50, p95, max}` in ms. |
    | `pick` | `{count, last, p95}`: the time in `view.pick()` of a click or a hover, over the last 120. With the layout at rest a pick reads the view's spatial grid (#500), not every node. |
    | `actions` | the last 32, oldest first: `{command, at, engine, delta, frame}`. `command` is what was typed (`.graph karate`) or the input (`click`, `fit`, `key +`); `at` is when the input arrived, in ms after the last `.graph`'s Enter; `engine` is the ms the engine took and `delta` the ms applying its answer to the view (both `null` for an input that needs neither); `frame` is the ms from the input to when the page's thread was free of the frame that showed it: with no GPU the browser rasterises a frame on that thread after `render()` returns (4 s at a million edges), so this is when the pixels are there, where `firstDraw` is when `render()` returned. A click that changes no selection draws nothing, and is shown by the next frame the browser paints. |
    | `longTasks` | the page's last 64 tasks over 50 ms: `{start, duration}`, `start` in ms after Enter. |

    `graphTimings().memory` is `{edges, view, layout}` (#501): the edge
    slots of what is drawn, and for the view and for the layout worker
    `{bytes, perEdge}`, its WebAssembly memory and those bytes an edge. A
    WebAssembly memory only grows, so each is the most its instance has
    held: the worker is new with each drawing, the view's module is the
    page's.

    `await window.ursaShell.graphMemory()` gives `wasm.engine`, `wasm.view`
    and `wasm.layout`, the bytes of each WebAssembly instance's memory (the
    engine's worker, the view on the page, the layout's worker), and
    `userAgent`, `performance.measureUserAgentSpecificMemory()`'s answer for
    the page and its workers (`{bytes, breakdown}`), which the cross-origin
    isolation of #429 allows. The browser answers after a garbage collection
    of its choosing, so after `timeoutMs` (10 s) `userAgent` is `null` and
    `userAgentError` says so.

    Keeping these costs a frame three clock reads, a timer and two writes
    into arrays made once (`graph-timings.js`): under a microsecond, and no
    allocation.
    Percentiles are worked out when `graphTimings()` is read.
    `tests/graph-timing.ts` prints all of it: the steps, the `.graph`
    action, the long tasks (and the longest while the view was made, which
    fails the run over 60 ms: the view builds no topology, #426), the frame times of a scripted wheel zoom, a pick
    and the heaps (on rmat it fails if the view holds over 20 MiB or the
    layout worker over 60 MB, #501). It runs the full Chromium (`channel: "chromium"`), since
    Playwright's headless shell has no `measureUserAgentSpecificMemory`;
    `CHANNEL=shell` runs the shell. `bun test tests/graph-timings.test.ts`
    tests the arithmetic.

    On `.graph rmat` (1M edges, headless Chromium, software WebGL2, median
    of three, 2026-11-21): `firstDraw` 1.28 s after Enter, but the `.graph`
    action's `frame` 5.85 s, since the browser spends 4.5 to 6.3 s
    rasterising that first frame on the page's thread after `render()`
    returns (0.7 ms); frames while a wheel zoom runs are 3.4 s apart at the
    p95; a pick takes 2.3 ms and a click is shown 16 ms after it; the wasm
    heaps are 49 MiB (engine), 52 MiB (view) and 97 MiB (layout worker),
    of 219 MiB for the page and its workers. On karate: frames 16.7 ms
    apart (p95 20 ms), `render()` 1.4 ms, a pick 0.2 ms.

  - **The aggregated view** (#498): once a graph of 200,000 nodes and
    edges or more is laid out, it is drawn as a cut through its layout's
    hierarchy: a disc for each group of nodes (area by mass, the colour of
    most of its nodes, at most 12 CSS pixels in radius), the coarsest
    groups whose discs stand 20 CSS pixels apart on screen, and the 16,384
    heaviest merged edges between them. The header says it first, where a
    narrow pane does not cut it off: `aggregated: 92 groups of 46,318
    nodes · 1,000,000 edges · …`; the largest groups carry their node
    counts (`.graph-group` labels over the canvas, at most 24, none over
    another). `graph.js` asks the layout worker for a cut when the layout
    is done and whenever the camera has then been still for 150 ms
    (`{type: "cut", id, camera}`; `graph-worker.js` answers with
    `Layout.cut`'s typed arrays, transferred, and what it took), and draws
    the answer (`setCut`); until it comes, the last cut is drawn, moved as
    the camera is. Only the groups in view are opened, so zooming in
    reaches the graph's own nodes: the worker's answer then says `detail`,
    and the graph itself is drawn (`clearLevel`), the same frame as with no
    cut. `graph-cut.js` (no DOM, no wasm) is which cut is asked for and
    what is made of an answer.
    - A click on a group selects its nodes (Shift+click adds another
      group's, or takes a selected group's out), with the header and the
      inspector as for any selection; a disc holding a selected node
      carries a ring, so a `.find` shows where its matches are. A node or
      an edge is picked once the view is zoomed in to the graph itself.
    - `a` in the pane forces full detail, every node and edge as before
      #498, and `a` again aggregates; `.graph detail on|off` is the same
      from the terminal, and `.graph detail` says which. The choice is
      kept from one drawing to the next.
    - A step (`.expand`, `.hide`, `.undo`) ends the aggregation for that
      drawing: the layout's hierarchy is of the graph before it.
    - The worker keeps the hierarchy's members through the layout and
      makes a forest of them when it is done (5.2 MB at a million edges,
      in memory the layout has let go of: its WebAssembly memory stays at
      55.6 MB).
    `window.ursaShell.graph()` gives `cut` (what is drawn of the cut, or
    null), `cuts` (`{on, large, detail, waiting, ms, last, prepared}`),
    `groupLabels` and `camera`. On `.graph rmat` (headless Chromium,
    software WebGL2, 2026-11-24): 92 discs and 2,389 merged edges at the
    fitted zoom; a cut takes the worker 7 to 25 ms, after 41 to 46 ms once
    for the forest; a wheel zoom over cuts is drawn in frames 24 to 26 ms
    apart at the p95; nine zoom steps of 1.35 reach the graph itself.

  - **The first paint** (#497): a graph of 200,000 nodes and edges or more
    is first drawn as the coarsest level of its layout's hierarchy, laid
    out: a few dozen discs, each with area by the mass of the nodes it
    stands for, and the merged edges between them, as wide as their weights
    say. Then each level the layout comes to, twice the discs each time,
    their nodes moving out from their parents over 150 ms (at once under
    `prefers-reduced-motion`), the header counting them: `… · Laying out ·
    level 3 of 11 · 110 groups`. The graph's own nodes and edges are first
    drawn when the layout reaches them. `graph-worker.js` sends each level
    as it reaches it (`level`: masses, parents, the 16,384 heaviest merged
    edges and positions, as transferred typed arrays, read before the
    hierarchy lets the level go; then `levelPositions` after each batch)
    in place of every node's position; `graph-levels.js` keeps them
    (`levels[l]`, for whoever cuts through the hierarchy later) and says
    where the nodes are on their way; the view draws the level
    (`ursa-viz`'s `setLevel`). Nothing is picked while a level is drawn.
    The layout's final positions are the same bits, and the worker holds
    what it held (55.6 MB at a million edges).
    `window.ursaShell.graph()` gives `coarse` (what is drawn of the
    level, or null), `hierarchy` (`[level, nodes]` of the levels that
    arrived) and `levelFrames` (what the levels' frames cost);
    `graphTimings().graphDrawn` is when the graph itself was first drawn.
    On `.graph rmat` (headless Chromium, software WebGL2, three runs,
    2026-11-23) the first frame is shown 941 to 953 ms after Enter (1,130
    to 1,230 ms before), the levels' frames are 33 ms apart at the p95 and
    50 ms at most, and the graph itself is first drawn at 4.2 to 4.5 s.
    The 600 ms #497 asks for is not met: the engine answers at 345 to
    366 ms, and the worker's hierarchy is built 520 to 540 ms after that.

  - **The frame budget** (#495): while the view moves (a pan, a zoom, a
    layout still running) a frame draws only what fits in one: a prefix of
    the edges, most important first, and the largest nodes if the nodes are
    too many as well. Once nothing has moved for 200 ms, and the layout is
    done, a frame draws everything. `graph.js` draws with
    `view.renderFrame(now)`, tells the view what each moving frame cost (the
    time to the next animation frame: `view.frameCost`), and sets a timer
    for the frame at rest when one left something out. The layout worker
    posts the edges' importance order once, after its first positions
    (`{type: "order", order, ms}`, from the layout's hierarchy:
    `Layout.edgeOrder`), and the view writes it to the GPU a piece a task.
    A result that comes with removed nodes or edges is drawn in index
    order: the worker is sent only the edges shown, so its numbering is not
    the view's. `window.ursaShell.graph().frame` says what the budget is
    doing: `{k, nodes, edges, ordered, p95, frames, limit, partial,
    orderMs}` (the edges and nodes the last moving frame drew, the edges
    there are, how many are in importance order, the p95 and count of the
    moving frames' costs, the ms a frame is held to, whether the last frame
    left anything out, and the ms the worker took to order the edges).

    On `.graph rmat` (1M edges, headless Chromium, software WebGL2, three
    runs, 2026-11-22), frames of the scripted wheel zoom are 26, 26 and
    31 ms apart at the p95 (3.3 s before), each with 512 to 1,081 edges and
    170 to 360 nodes, which is what that renderer shows in a frame; the
    first frame is shown 1.3 s after Enter (6.0 s) and the layout is done at
    5.6 s (10.6 s). `tests/graph-timing.ts` starts its
    zoom from rest and fails a run whose p95 is over 50 ms; the e2e allows
    100 (`ursa-viz/BENCH.md` has the runs, and what a busy machine does to
    them).

  - **Refinement at rest** (#496): where everything does not fit in a
    frame, the frame at rest is not drawn at once (with no GPU that was
    2.5 to 6 s of drawing after the layout, after each movement and for
    each click, and a zoom begun under it waited). `renderFrame` leaves the
    canvas as it is, and `graph.js` calls `view.refine(performance.now())`
    a task at a time while `view.refining`: each call sends the GPU one
    slice of the edges or the nodes, drawn offscreen into a texture the
    size of the canvas, once the GPU has finished the last but one. Then a
    frame, which is that texture under the highlighted edges and the nodes
    in front, and so is every frame at rest after it: a hover or a click
    draws no edge. A pan or a zoom stops the slices and waits for two of
    them at most; its frames draw the texture under their own edges, moved
    and scaled with the camera, so the whole picture stays on screen while
    it moves. The status line ends in ` · refining…` while slices remain.
    `ursaShell.graph().frame` gains `refining`, `refined` and `refinement`
    (`{ms, slices, longestSliceMs, sliceEdges, sliceNodes}` of the last one
    finished), and `partial` stays true until the refined frame is drawn.

    On `.graph rmat` (1M edges, headless Chromium, software WebGL2,
    2026-11-23, three runs at a load of 4): the slices take 2.2, 2.4 and
    2.9 s (137 to 177 of them), so everything is shown 2.5 to 3.1 s after a
    zoom's last event, with no task of the page over 50 ms meanwhile; a
    click on the view at rest is shown 90 to 140 ms after it (2.2 to 2.9 s
    on main in the same hour); and the wheel zoom's frames are as far apart
    as before (p95 26 to 27 ms; main 25 to 26). On a busier machine (a
    load of 5 to 7) the slices took 2.3 to 4.6 s. `tests/graph-timing.ts` fails a run with a task
    over 50 ms while the slices are drawn, or slices that take over 5 s.
    Not measured on a GPU, where the target is 0.3 s.

    The layout worker (`graph-worker.js`) also lays out **rings and layers**
    (#436): a graph that carries `layout: {kind: "rings" | "layers", level}`,
    each node's distance from a seed or its layer, is laid out in one
    message, with no levels to wait for. Nothing sets it yet: the rules
    that draw a `hop` result as rings and paths as layers are #433.
    `graph.js` is imported on the first `.graph`, and it loads `ursa-viz`
    (about 1 MB gzipped), so a session that draws nothing downloads neither.
    The terminal keeps the focus and refits to its half (xterm's fit addon,
    then the engine's width); Esc (in the terminal or the pane) or the
    close button closes the pane, and New or a dataset does too. In the
    pane, drag or the arrow keys pan, the wheel or `+`/`-` zoom, a
    double-click on empty space or `0` fits, and a double-click on a node
    expands it (#443, above). A click on a node selects it (#438):
    it carries a ring, its edges are drawn wide, and the pane's header, a
    live region, reads `Selected node 33: degree 17, club Officer`. Shift+click adds a node (or takes it out), a click on empty
    space clears the selection, and so does Esc, which closes the pane only
    when nothing is selected. A mouse or a pen over a node rings it and
    shows a tooltip beside it: its key, its label (when the result names
    one: the engine names none yet), `SIZE BY` and `COLOR BY` values and
    degree; the pointer is looked up once a frame at most. The
    selection set is `ursa-viz`'s (`interact`); `graph.js` tells it what
    was clicked. A click within 4 px of an edge, with no node under it,
    selects the edge (`Selected edge 0 → 1`).

    **The inspector** (#444, `graph-inspector.js` and `graph-inspector.css`)
    shows what is known of the selection, in a panel over the top right of
    the view (the top left when what was selected is drawn on the right; a
    sheet over the lower half under 640 px):
    - **a node:** its key and label, its degree as drawn and how many
      neighbours it has in the whole graph, the values drawn (`SIZE BY`,
      `COLOR BY`, a hop's distance), and its row of the vertex table, read
      with one query by key (`SELECT * FROM members WHERE id = 33`, which
      the panel shows). Buttons run commands in the terminal, as if typed:
      **expand** (`.expand 33`), **hide** (`.hide 33`), and **path from
      here**, which waits for a second node and then draws
      `shortest_path` between the two. **pin** marks the node with the
      pin ring. Where the drawing is a property graph itself (`.graph
      NAME`), each property has **colour by** (and **size by**, if it is
      numeric), which run the `.graph` command again with that clause.
    - **an edge:** its two ends (each with a button that selects it), its
      edge columns, and its row of the edge table, found by its ends;
      where several rows join them, by its row number if the drawing knows
      it, and else all of them are shown, with a note.
    - **several nodes:** how many, the edges drawn among them, and a
      `WHERE id IN (…)` clause with a copy button.

    A drawing that is not of a property graph has no row to show, and the
    panel says so. `i` in the view moves the focus into the panel and Esc
    there moves it back; Esc in the view clears the selection, which closes
    the panel. Every value is set with `textContent` and wraps: nothing
    from the data is ever HTML. The rows come from the worker's `inspect`
    op (`WebShell.inspectNode`, `WebShell.inspectEdge`), which prints
    nothing and leaves the last result alone. The module imports nothing
    and takes its data and callbacks from `graph.js`, so it can move to
    `ursa-viz/ui/` as it is. `window.ursaShell.graphInspector()` gives
    what it shows, and `graphEdgeAt(x, y)` the edge a click would pick.
    On a touch screen (#428) one finger pans and two pinch: the
    view zooms by the change in the fingers' distance around the point
    between them, and pans as that point moves, so what is between the
    fingers stays there. A tap names a node (a second finger cancels it),
    and a double tap expands the node under it, or fits. The canvas draws at most 2 pixels for each CSS
    pixel, whatever the screen's ratio. A lost WebGL context or GPU device, or a layout that stops,
    closes the pane with an error in the terminal.
- `src/index.ts`: the Worker. `/data/<file>` serves `datasets/<file>` from the
  bucket with HEAD, byte ranges and validators; `/engine/<hash>.wasm` serves the
  engine; everything else is a static asset. A path that does not decode is
  404. `src/headers.ts` holds the security headers every response carries, from
  the Worker and `bun run preview` alike: a Content Security Policy (the page's
  own scripts, worker, fetches, fonts and images; WebAssembly compiles under
  `'wasm-unsafe-eval'`; `style-src` allows `'unsafe-inline'` for the `<style>`
  elements xterm.js adds; no framing), `X-Content-Type-Options: nosniff`,
  `Referrer-Policy: no-referrer`, and the three that make the page
  cross-origin isolated (#429): `Cross-Origin-Opener-Policy: same-origin`,
  `Cross-Origin-Embedder-Policy: require-corp` and
  `Cross-Origin-Resource-Policy: same-origin`. So `crossOriginIsolated` is
  true on the page and in the engine's and the layout's workers: timers are
  precise, `performance.measureUserAgentSpecificMemory()` works, and a
  `SharedArrayBuffer` can be made, which a threaded build would need. Every
  resource is same-origin, as the policy requires, so nothing else changes;
  a cross-origin resource could only be added if it sends CORP or CORS
  headers. The page has no inline script: the theme is
  set before the first paint by `public/theme.js`.
- `scripts/`: building the engine, assembling `dist/`, and uploading to R2.
  `build.ts` also builds the graph view (`scripts/viz.ts`: `ursa-viz` with
  its `web` feature and the `wasm-release` profile, then wasm-bindgen) into
  `dist/viz/<hash>/`, a static asset of about 4 MB (1 MB gzipped) named by
  a hash of its glue and `.wasm`; `dist/viz/viz.json` names the build.
  The build also copies the terminal's font, IBM Plex Mono (SIL OFL 1.1, from
  `@fontsource/ibm-plex-mono`), into `dist/fonts/`, so the page loads nothing
  from other hosts.
  Plex Mono's web fonts have no box-drawing characters, so table borders come
  from `public/fonts/ursa-box.woff2`, a font of just those, made by
  `scripts/box-font.py` with lines that run past each row so borders join.
- `cloudflare.config.ts`: the Worker (name `ursa-shell`, the `ursa-graph` bucket
  as `DATA`, on `workers.dev` for now). `wrangler.config.ts` is the setting for
  the asset bundler `cf build` uses internally; deploys use `cf` only.

**Why the engine is in R2:** Workers static assets are limited to 25 MiB a file,
and the engine is about 31 MB. It is stored brotli-compressed (about 5 MB) under
its content hash, `engine/<hash>.wasm.br`, and the Worker sends it as is with
`Content-Encoding: br`. `dist/engine/engine.json` names the build the page loads,
so a deploy switches engines atomically and old builds stay valid for open tabs.

## Build and deploy

Needs bun, the `wasm32-unknown-unknown` target and `wasm-bindgen-cli` 0.2.126
(see `ursa-wasm/README.md`), and `CLOUDFLARE_API_TOKEN` / `CLOUDFLARE_ACCOUNT_ID`
for the account.

```sh
bun install
bun run engine          # build ursa-wasm into .engine/ (a few minutes: fat LTO)
bun run upload:engine   # put engine/<hash>.wasm.br in the bucket
bun run upload:data     # stage the sample datasets (CSV + Parquet) and upload them
bun run deploy          # assemble dist/ and deploy the Worker with cf
```

Upload the engine before deploying the page that names it. The datasets only
need uploading when they change.

## Local

`cf dev` simulates the bucket under `.wrangler/state`; seed it with `--local`:

```sh
bun run engine && bun run upload:engine -- --local && bun run upload:data -- --local
bun run dev
```

Or without Cloudflare: `bun run preview` serves `dist/`, `.data/` (with byte
ranges) and the engine from `.engine/` as the Worker does, at
http://localhost:8788.

## Test

`tests/e2e.ts` drives the page in headless Chromium against a running site
(`bun run preview`, `cf dev` or a deploy): every dataset in the menu, SQL typed
at the prompt (one line, several, an error), every dot-command, a long table,
downloads in each format, Import (with awkward file names), a shared link
replayed in a fresh page (answered y and N, and a crafted one with escape
sequences and `.export`), New, the theme toggle and history; the Graph view
(`.graph` on a result, a query and a property graph, its errors, pixels drawn
in both themes; `SIZE BY` and `COLOR BY` from an algorithm, a vertex column and
`NODES (…)`, each of their errors, at least two palette colours read back in
each theme with no inversion, a theme switch with the pane open, the legend's
text, and the status from Enter to Done, within a second for rmat; click to
expand (a double-click, Shift, Enter and a double tap each running `.expand`,
its echo and `.graph echo off`, the new nodes drawn and picked, `+N` in the
header and the tooltip, the busy rings of queued commands, `.undo` restoring
the frame's pixels and `.redo` the settled places, `.hide` and `.show` drawn,
ten double-clicks on rmat with a median click-to-frame time within 250 ms, and a shared link replaying the
exploration to the same counts; the coarse levels on rmat, #497: the status counting each level's groups, coarsest first, every level of the hierarchy reaching the page, and their frames at most 100 ms apart at the p95 before the graph itself is drawn; the aggregated view on rmat, #498: at most 20,000 discs and 100,000 merged edges at the fitted zoom, the status saying `aggregated: G groups of N nodes`, the groups holding every node and labelled with their counts, a cut within 60 ms in the worker, a click on a group selecting its nodes, `a` and `.graph detail on|off`, and zooming in reaching the graph's own nodes; the frame budget on rmat, in full detail, #495: the worker's order of the million edges in the view, a wheel zoom drawn in frames at most 100 ms apart at the p95, each a prefix of the edges, and a frame with everything at rest; refinement at rest, #496: that frame drawn in slices with the status saying `refining…` and no task of the page over 50 ms, a click at rest drawing no slices, and a zoom begun under the slices stopping them); a node's tooltip on hover, a click selecting it and the status naming it, Shift+click, a click on empty space and Esc clearing the selection, typing while it shows, Esc and the close
button, the terminal refitting beside it and under it on a phone, a pinch, a
one-finger drag, a tap and a double tap from Chromium's touch input with each
node's place read back, the Graph button, and `ursa-viz` not downloaded until `.graph`); the keyboard and
phone checks: Tab out of the terminal, the menu's keys, the focus after a
click, no mid-word wraps and a one-row bar at 320px, a 16px input, and the grey
text's contrast; and the security headers, cross-origin isolation (its headers
on every kind of response, a dataset's ranges included, and
`crossOriginIsolated` on the page and in both workers), 404s for malformed and traversing
paths, and no Content Security Policy violation in the console.

```sh
bun run preview                               # in one terminal
bun run test:e2e                              # in another; or: bun tests/e2e.ts <url> [screenshot dir]
```

It needs Playwright and Chromium (`bun add --no-save playwright && bunx playwright install chromium`).

CI runs it: the "Ursa Shell (browser e2e)" job (`.github/workflows/shell-e2e.yml`)
runs on pull requests and pushes to `main` that touch `web/`, the crates built
into the page (`ursa-wasm`, `ursa-shell`, `ursa-viz` and the engine crates
under them), the Cargo files, or the datasets' inputs; it can also be started
by hand (Run workflow). It does what is above, with no Cloudflare credentials:
`bun run engine`, `bun run build`, `bun run upload:data -- --dry` (which only
stages `.data/`), then `bun scripts/preview.ts` and the suite against it, with
Playwright pinned in the workflow (`PLAYWRIGHT_VERSION`) and its own Chromium.
Cargo's dependencies are cached; so are `.engine/`, keyed by the engine's
sources, and `.data/`, keyed by the bundled CSVs, `gen.py` and
`upload-data.ts` (a miss builds the native `ursa` in release, several
minutes). A failed run uploads the screenshots and the server's and suite's
logs as the `shell-e2e` artifact.

## Datasets

- The library's bundled sample graphs (`python/ursa/datasets/data/`), each as
  CSV and as Parquet. They are redistributed from NetworkX under its 3-clause BSD
  licence (see `python/ursa/datasets/LICENSES.md`).
- `rmat_1m_edges.parquet`: a seeded R-MAT graph with 1,000,000 edges over
  46,318 nodes, made by `benchmarks/sql_scale/gen.py`. Generated here, so it is
  ours to publish (CC0).

`manifest.json` lists every file with its size, source and licence.
