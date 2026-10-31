# Changelog

All notable changes to this project are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

The distribution is `ursa-graph`; the import name is `ursa`.

## [Unreleased]

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
