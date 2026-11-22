//! The result → graph rules (design §3, #433): which rule draws a result,
//! and from which of its columns.
//!
//! [`read`] says what each column is, **by provenance first, names second**,
//! and then takes the first rule that matches:
//!
//! 1. a path column (`vertices(p)`, with `edges(p)` or `element_id(p)`), or
//!    the steps of `shortest_path()`: **paths**;
//! 2. `seed`, `id` and `distance` (a `hop` result): **rings**;
//! 3. two endpoint columns: **edges**, a row an edge;
//! 4. three or more node variables of `GRAPH_TABLE`: **pattern matches**;
//! 5. one node key: **nodes**, with the graph's edges among them;
//! 6. none of these: not a graph, and the message says what would be.
//!
//! Provenance is the `ursa.*` field metadata the graph functions and
//! `GRAPH_TABLE` put on their output ([`crate::graph::provenance`]). A column
//! without it is read by its name and type: `src`/`dst`, `source`/`target`
//! and `from`/`to` are endpoints, `id` or `node` a node key, `seed`, `id`
//! and `distance` together a hop result, a list of ids a path, and, last,
//! the first two columns when both are ids of one kind, as before #433.
//!
//! A node key or an endpoint is an integer, a string or a date
//! ([`id_kind`]): a float or a list is never a node id.
//!
//! A property graph named to `.graph` is drawn whole ([`Rule::Graph`]). The
//! `AS …` clause that overrides the rules is #434.

use datafusion::arrow::datatypes::{DataType, Field, Schema};
use datafusion::error::Result;
use ursa_core::catalog::OutputKind;

use super::plan_err;
use super::style::per_node_algorithm;
use crate::graph::functions;
use crate::graph::provenance as meta;

/// The rule that drew a result. A result's provenance names it
/// ([`Rule::name`]), for the pane's status line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Rule {
    /// `.graph NAME`: a property graph's vertices and edges.
    Graph,
    /// Rows that are paths, or the steps of one (rule 1).
    Path,
    /// A `hop` result: seeds, the nodes reached and their distances (rule 2).
    Rings,
    /// Rows drawn as edges, one a row (rule 3).
    Edges,
    /// The matches of a `GRAPH_TABLE` pattern of three node variables or
    /// more (rule 4).
    Matches,
    /// Rows that are nodes, with the graph's edges among them (rule 5).
    Nodes,
}

impl Rule {
    /// The rule's name in a result's provenance.
    pub fn name(self) -> &'static str {
        match self {
            Rule::Graph => "graph",
            Rule::Path => "path",
            Rule::Rings => "rings",
            Rule::Edges => "edges",
            Rule::Matches => "pattern",
            Rule::Nodes => "nodes",
        }
    }
}

/// One edge of a pattern, between two of the node variables in `COLUMNS`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MatchEdge {
    /// The key columns of its two ends, as indexes into [`Shape::Matches`]'s
    /// `keys`.
    pub from: usize,
    pub to: usize,
    pub directed: bool,
    /// The edge table.
    pub label: String,
}

/// What a result's rows are, by the rule that matched: the columns to read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Shape {
    /// Each row is an edge between the ids in columns `src` and `dst`.
    Edges { src: usize, dst: usize },
    /// Each row is a path: its nodes in a list column, or its nodes and
    /// edges alternating in `elements`; `edges` is its edges' rows.
    Path {
        nodes: Option<usize>,
        edges: Option<usize>,
        elements: Option<usize>,
    },
    /// Each row is a step of a path (`shortest_path()`): an edge, and its
    /// position.
    Steps { src: usize, dst: usize, step: usize },
    /// Each row is a node reached from a seed, and how far.
    Hop {
        seed: usize,
        id: usize,
        distance: usize,
    },
    /// Each row is a match of a pattern: the key column of each node
    /// variable, and the pattern's edges between them.
    Matches {
        keys: Vec<usize>,
        edges: Vec<MatchEdge>,
    },
    /// Each row is a node.
    Nodes { id: usize },
}

impl Shape {
    pub(crate) fn rule(&self) -> Rule {
        match self {
            Shape::Edges { .. } => Rule::Edges,
            Shape::Path { .. } | Shape::Steps { .. } => Rule::Path,
            Shape::Hop { .. } => Rule::Rings,
            Shape::Matches { .. } => Rule::Matches,
            Shape::Nodes { .. } => Rule::Nodes,
        }
    }
}

/// A result as the rules read it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Reading {
    pub shape: Shape,
    /// The property graph the node columns say they are from (`ursa.graph`).
    pub graph: Option<String>,
    /// Whether the columns were read by their metadata (else by name).
    pub marked: bool,
    /// The vertex table of each key column the shape reads: (column, table).
    pub labels: Vec<(usize, String)>,
    /// The edge table of the edges, if the result says.
    pub edge_label: Option<String>,
    /// Columns that become edge columns: numbers, one per row.
    pub attributes: Vec<usize>,
    /// Columns that describe nodes: (column, the key column of its nodes).
    pub properties: Vec<(usize, usize)>,
    /// Rule 5's defaults: the column that sizes the nodes and the one that
    /// colours them.
    pub size: Option<usize>,
    pub color: Option<usize>,
    /// Each column the drawing reads, and what as: (name, role).
    pub roles: Vec<(String, String)>,
    /// What the status line adds: what was left out, and other readings.
    pub notes: Vec<String>,
}

impl Reading {
    fn of(shape: Shape) -> Reading {
        Reading {
            shape,
            graph: None,
            marked: false,
            labels: Vec::new(),
            edge_label: None,
            attributes: Vec::new(),
            properties: Vec::new(),
            size: None,
            color: None,
            roles: Vec::new(),
            notes: Vec::new(),
        }
    }

    /// The vertex table of key column `column`, if the result says.
    pub(crate) fn label_of(&self, column: usize) -> Option<&str> {
        self.labels
            .iter()
            .find(|(c, _)| *c == column)
            .map(|(_, l)| l.as_str())
    }
}

/// The most columns carried as edge columns or node properties.
const MAX_CARRIED: usize = 8;

/// The first rule that matches a result of `schema`, or why none does.
#[cfg(test)]
pub(crate) fn shape_of(schema: &Schema) -> Result<Shape> {
    Ok(read(schema)?.shape)
}

/// A column's metadata.
struct Col<'a> {
    i: usize,
    field: &'a Field,
    role: Option<&'a str>,
    graph: Option<&'a str>,
    label: Option<&'a str>,
    var: Option<&'a str>,
}

impl Col<'_> {
    fn name(&self) -> &str {
        self.field.name()
    }
    fn is(&self, role: &str) -> bool {
        self.role == Some(role)
    }
    fn is_id(&self) -> bool {
        id_kind(self.field.data_type()).is_some()
    }
}

/// The type of a list column's elements.
fn list_of(t: &DataType) -> Option<&DataType> {
    match t {
        DataType::List(f)
        | DataType::LargeList(f)
        | DataType::FixedSizeList(f, _)
        | DataType::ListView(f)
        | DataType::LargeListView(f) => Some(f.data_type()),
        _ => None,
    }
}

fn is_id_list(t: &DataType) -> bool {
    list_of(t).is_some_and(|e| id_kind(e).is_some())
}

/// What a column named `name` holds per node, if it is named after an
/// algorithm or, for a category, is one of the usual names for a group.
fn kind_by_name(name: &str, t: &DataType) -> Option<OutputKind> {
    let name = name.to_ascii_lowercase();
    if let Some(kind) = functions::lookup(&name)
        .filter(|s| functions::is_per_node(s))
        .and_then(|s| s.output_kind())
    {
        return match kind {
            OutputKind::Metric if !t.is_numeric() => None,
            kind => Some(kind),
        };
    }
    matches!(
        name.as_str(),
        "community" | "component" | "cluster" | "label" | "group"
    )
    .then_some(OutputKind::Category)
}

/// What the rules make of a result of `schema`: the first rule that matches,
/// or why none does (rule 6).
pub(crate) fn read(schema: &Schema) -> Result<Reading> {
    let cols: Vec<Col> = schema
        .fields()
        .iter()
        .enumerate()
        .map(|(i, f)| {
            let get = |key: &str| f.metadata().get(key).map(String::as_str);
            Col {
                i,
                field: f.as_ref(),
                role: get(meta::ROLE),
                graph: get(meta::GRAPH),
                label: get(meta::LABEL),
                var: get(meta::VAR),
            }
        })
        .collect();
    if cols.iter().any(|c| c.role.is_some()) {
        if let Some(reading) = read_marked(schema, &cols)? {
            return Ok(reading);
        }
    }
    read_named(schema, &cols)
}

/// The roles a node's key has.
const KEY_ROLES: [&str; 6] = [
    meta::ROLE_NODE,
    meta::ROLE_SRC,
    meta::ROLE_DST,
    meta::ROLE_SEED,
    meta::ROLE_PATH_NODES,
    meta::ROLE_PATH_ELEMENTS,
];

/// The reading of a result some of whose columns say what they are, or
/// `None` if they are not enough for a rule.
fn read_marked(schema: &Schema, cols: &[Col]) -> Result<Option<Reading>> {
    let first = |role: &str| cols.iter().find(|c| c.is(role));
    let key = |role: &str| cols.iter().find(|c| c.is(role) && c.is_id());
    let done = |mut r: Reading, used: &[&Col]| -> Result<Option<Reading>> {
        r.marked = true;
        // The graph of the columns the drawing reads; ids of two graphs do
        // not meet.
        let mut graphs: Vec<&str> = Vec::new();
        for c in used {
            if let Some(g) = c
                .graph
                .filter(|_| c.role.is_some_and(|r| KEY_ROLES.contains(&r)))
            {
                if !graphs.contains(&g) {
                    graphs.push(g);
                }
            }
            r.roles
                .push((c.name().to_string(), c.role.unwrap_or("").to_string()));
            if let (Some(label), true) = (c.label, c.role.is_some_and(|r| KEY_ROLES.contains(&r))) {
                r.labels.push((c.i, label.to_string()));
            }
        }
        if graphs.len() > 1 {
            return plan_err(format!(
                "cannot draw this result as one graph: its node columns come from the graphs \
                 {}, and ids of two graphs do not meet. Draw one graph's columns: .graph SELECT \
                 … FROM …",
                graphs.join(" and ")
            ));
        }
        r.graph = graphs.first().map(|g| g.to_string());
        Ok(Some(r))
    };
    let numbers = |skip: &[usize]| -> Vec<usize> {
        cols.iter()
            .filter(|c| !skip.contains(&c.i) && c.field.data_type().is_numeric())
            .map(|c| c.i)
            .take(MAX_CARRIED)
            .collect()
    };
    let endpoints = (key(meta::ROLE_SRC), key(meta::ROLE_DST));

    // Rule 1: a path column.
    let path_nodes = cols
        .iter()
        .find(|c| c.is(meta::ROLE_PATH_NODES) && is_id_list(c.field.data_type()));
    let path_elements = cols
        .iter()
        .find(|c| c.is(meta::ROLE_PATH_ELEMENTS) && is_id_list(c.field.data_type()));
    if let Some(anchor) = path_nodes.or(path_elements) {
        // The edges of the same path variable.
        let edges = cols.iter().find(|c| {
            c.is(meta::ROLE_PATH_EDGES)
                && c.var == anchor.var
                && list_of(c.field.data_type()).is_some_and(DataType::is_integer)
        });
        let elements = path_elements.filter(|c| c.var == anchor.var && path_nodes.is_none());
        let mut r = Reading::of(Shape::Path {
            nodes: path_nodes.map(|c| c.i),
            edges: edges.filter(|_| path_nodes.is_some()).map(|c| c.i),
            elements: elements.map(|c| c.i),
        });
        r.edge_label = edges.and_then(|c| c.label).map(str::to_string);
        if let (Some(s), Some(d)) = endpoints {
            r.notes.push(format!(
                "also edges ({}, {}): .graph AS edges({}, {})",
                s.name(),
                d.name(),
                s.name(),
                d.name()
            ));
        }
        let mut used = vec![anchor];
        used.extend(edges.filter(|_| path_nodes.is_some()));
        return done(r, &used);
    }
    // Rule 1 too: the steps of shortest_path(), in order.
    if let (Some(s), Some(d), Some(step)) = (
        endpoints.0,
        endpoints.1,
        first(meta::ROLE_STEP).filter(|c| c.field.data_type().is_integer()),
    ) {
        let mut r = Reading::of(Shape::Steps {
            src: s.i,
            dst: d.i,
            step: step.i,
        });
        r.attributes = numbers(&[s.i, d.i, step.i]);
        return done(r, &[s, d, step]);
    }
    // Rule 2: a hop result.
    let seed = key(meta::ROLE_SEED);
    let nodes: Vec<&Col> = cols
        .iter()
        .filter(|c| c.is(meta::ROLE_NODE) && c.is_id())
        .collect();
    if let (Some(seed), [id], Some(distance)) = (
        seed,
        nodes.as_slice(),
        first(meta::ROLE_DISTANCE).filter(|c| c.field.data_type().is_integer()),
    ) {
        let r = Reading::of(Shape::Hop {
            seed: seed.i,
            id: id.i,
            distance: distance.i,
        });
        return done(r, &[seed, id, distance]);
    }
    // Rule 3: endpoints by role.
    if let (Some(s), Some(d)) = endpoints {
        let mut r = Reading::of(Shape::Edges { src: s.i, dst: d.i });
        r.attributes = numbers(&[s.i, d.i]);
        r.edge_label = first(meta::ROLE_EDGE)
            .and_then(|c| c.label)
            .map(str::to_string);
        return done(r, &[s, d]);
    }
    // A hop result without its distance: two keys are two ends.
    if let (Some(seed), [id]) = (seed, nodes.as_slice()) {
        let mut r = Reading::of(Shape::Edges {
            src: seed.i,
            dst: id.i,
        });
        r.notes.push(format!(
            "no distance column, so no rings: each row is an edge {}–{}",
            seed.name(),
            id.name()
        ));
        return done(r, &[seed, id]);
    }
    // The columns that describe a key column's nodes: the properties of its
    // pattern variable.
    let properties = |keys: &[&Col]| -> Vec<(usize, usize)> {
        cols.iter()
            .filter(|c| c.is(meta::ROLE_PROPERTY) && c.var.is_some())
            .filter_map(|c| {
                let key = keys.iter().find(|k| k.var == c.var)?;
                Some((c.i, key.i))
            })
            .take(MAX_CARRIED)
            .collect()
    };
    match nodes.as_slice() {
        [] => Ok(None),
        // Rule 5: one node key.
        [id] => {
            let mut r = Reading::of(Shape::Nodes { id: id.i });
            let kind = |c: &Col| match c.role {
                Some(meta::ROLE_METRIC) => Some(OutputKind::Metric),
                Some(meta::ROLE_CATEGORY) => Some(OutputKind::Category),
                Some(_) => None,
                None => kind_by_name(c.name(), c.field.data_type()),
            };
            let others = || cols.iter().filter(|c| c.i != id.i);
            let size = others()
                .find(|c| kind(c) == Some(OutputKind::Metric) && c.field.data_type().is_numeric());
            let color = others().find(|c| kind(c) == Some(OutputKind::Category));
            r.size = size.map(|c| c.i);
            r.color = color.map(|c| c.i);
            let mut used = vec![*id];
            used.extend(size);
            used.extend(color);
            let mut r = done(r, &used)?.expect("a reading");
            // A column read by its name says so.
            for c in size.into_iter().chain(color) {
                if c.role.is_none() {
                    let role = if Some(c.i) == r.size {
                        meta::ROLE_METRIC
                    } else {
                        meta::ROLE_CATEGORY
                    };
                    if let Some(entry) = r.roles.iter_mut().find(|(n, _)| n == c.name()) {
                        entry.1 = role.to_string();
                    }
                }
            }
            Ok(Some(r))
        }
        // Rule 3: two node keys are two ends (two variables of a pattern, or
        // a self-join).
        [a, b] => {
            let mut r = Reading::of(Shape::Edges { src: a.i, dst: b.i });
            r.properties = properties(&[a, b]);
            let carried: Vec<usize> = r.properties.iter().map(|p| p.0).collect();
            r.attributes = numbers(&[a.i, b.i])
                .into_iter()
                .filter(|i| !carried.contains(i))
                .collect();
            let pattern = pattern_of(schema);
            r.edge_label = pattern
                .iter()
                .find(|e| {
                    let ends = (e.from.as_deref(), e.to.as_deref());
                    a.var.is_some() && (ends == (a.var, b.var) || ends == (b.var, a.var))
                })
                .map(|e| e.label.clone());
            done(r, &[a, b])
        }
        // Rule 4: three node variables or more.
        keys => {
            let pattern = pattern_of(schema);
            let at = |var: &Option<String>| {
                let var = var.as_deref()?;
                keys.iter().position(|k| k.var == Some(var))
            };
            let mut edges: Vec<MatchEdge> = Vec::new();
            for e in &pattern {
                if let (Some(from), Some(to)) = (at(&e.from), at(&e.to)) {
                    edges.push(MatchEdge {
                        from,
                        to,
                        directed: e.directed,
                        label: e.label.clone(),
                    });
                }
            }
            if edges.is_empty() {
                return Ok(None);
            }
            let mut r = Reading::of(Shape::Matches {
                keys: keys.iter().map(|k| k.i).collect(),
                edges,
            });
            r.properties = properties(keys);
            done(r, keys)
        }
    }
}

/// The edges of the `GRAPH_TABLE` pattern a result of `schema` matched
/// (`ursa.pattern`), or none.
fn pattern_of(schema: &Schema) -> Vec<meta::PatternEdge> {
    let Some(text) = schema.metadata().get(meta::PATTERN) else {
        return Vec::new();
    };
    let Ok(serde_json::Value::Array(edges)) = serde_json::from_str(text) else {
        return Vec::new();
    };
    edges
        .iter()
        .filter_map(|e| {
            let text = |key: &str| e.get(key).and_then(|v| v.as_str()).map(str::to_string);
            Some(meta::PatternEdge {
                from: text("from"),
                to: text("to"),
                directed: e.get("directed")?.as_bool()?,
                edge: text("edge"),
                label: text("label")?,
                quantifier: None,
                path: text("path"),
            })
        })
        .collect()
}

/// The reading of a result by its columns' names and types.
fn read_named(schema: &Schema, cols: &[Col]) -> Result<Reading> {
    let find = |name: &str| cols.iter().find(|c| c.name().eq_ignore_ascii_case(name));
    let id = |name: &str| find(name).filter(|c| c.is_id());
    let role = |c: &Col, role: &str| (c.name().to_string(), role.to_string());
    let weight = |r: &mut Reading| {
        r.attributes = find("weight")
            .filter(|c| c.field.data_type().is_numeric())
            .map(|c| c.i)
            .into_iter()
            .collect();
    };
    // Rule 3: endpoints by name. `src` and `dst` that are not ids are an
    // error, not another shape: the names say what was meant.
    if find("src").is_some() && find("dst").is_some() {
        let (s, d) = endpoint_columns(schema)?;
        let mut r = Reading::of(Shape::Edges { src: s, dst: d });
        r.roles = vec![
            role(&cols[s], meta::ROLE_SRC),
            role(&cols[d], meta::ROLE_DST),
        ];
        weight(&mut r);
        return Ok(r);
    }
    for (s, d) in [("source", "target"), ("from", "to")] {
        if let (Some(s), Some(d)) = (id(s), id(d)) {
            let mut r = Reading::of(Shape::Edges { src: s.i, dst: d.i });
            r.roles = vec![role(s, meta::ROLE_SRC), role(d, meta::ROLE_DST)];
            weight(&mut r);
            return Ok(r);
        }
    }
    // Rule 2: seed, id and distance together.
    let key = id("id").or_else(|| id("node"));
    if let (Some(seed), Some(key)) = (id("seed"), key) {
        let distance = find("distance").filter(|c| c.field.data_type().is_integer());
        let mut r = match distance {
            Some(distance) => {
                let mut r = Reading::of(Shape::Hop {
                    seed: seed.i,
                    id: key.i,
                    distance: distance.i,
                });
                r.roles.push(role(distance, meta::ROLE_DISTANCE));
                r
            }
            None => {
                let mut r = Reading::of(Shape::Edges {
                    src: seed.i,
                    dst: key.i,
                });
                r.notes.push(format!(
                    "no distance column, so no rings: each row is an edge {}–{}",
                    seed.name(),
                    key.name()
                ));
                r
            }
        };
        r.roles.insert(0, role(key, meta::ROLE_NODE));
        r.roles.insert(0, role(seed, meta::ROLE_SEED));
        return Ok(r);
    }
    // Rule 5: a node key by name.
    if let Some(key) = key {
        let mut r = Reading::of(Shape::Nodes { id: key.i });
        r.roles.push(role(key, meta::ROLE_NODE));
        let others = || cols.iter().filter(|c| c.i != key.i);
        let kind = |c: &Col| kind_by_name(c.name(), c.field.data_type());
        if let Some(c) = others().find(|c| kind(c) == Some(OutputKind::Metric)) {
            r.size = Some(c.i);
            r.roles.push(role(c, meta::ROLE_METRIC));
        }
        if let Some(c) = others().find(|c| kind(c) == Some(OutputKind::Category)) {
            r.color = Some(c.i);
            r.roles.push(role(c, meta::ROLE_CATEGORY));
        }
        if let Some(path) = others().find(|c| is_id_list(c.field.data_type())) {
            r.notes.push(format!(
                "also a path ({}): .graph AS path({})",
                path.name(),
                path.name()
            ));
        }
        return Ok(r);
    }
    // The first two columns, when both are ids of one kind, as ever.
    let path = cols.iter().find(|c| is_id_list(c.field.data_type()));
    if cols.len() >= 2 {
        match (endpoint_columns(schema), path) {
            (Ok((s, d)), _) => {
                let mut r = Reading::of(Shape::Edges { src: s, dst: d });
                r.roles = vec![
                    role(&cols[s], meta::ROLE_SRC),
                    role(&cols[d], meta::ROLE_DST),
                ];
                weight(&mut r);
                if let Some(path) = path {
                    r.notes.push(format!(
                        "also a path ({}): .graph AS path({})",
                        path.name(),
                        path.name()
                    ));
                }
                return Ok(r);
            }
            (Err(_), _) => {}
        }
    }
    // Rule 1: a list of ids is a path.
    if let Some(path) = path {
        let mut r = Reading::of(Shape::Path {
            nodes: Some(path.i),
            edges: None,
            elements: None,
        });
        r.roles.push(role(path, meta::ROLE_PATH_NODES));
        return Ok(r);
    }
    Err(not_a_graph(schema))
}

/// Rule 6: a result that is not a graph. The message names the columns and
/// their types, and the `AS` clause that would draw them.
fn not_a_graph(schema: &Schema) -> datafusion::error::DataFusionError {
    let columns: Vec<String> = schema
        .fields()
        .iter()
        .map(|f| format!("{}: {}", f.name(), f.data_type()))
        .collect();
    let ids: Vec<&str> = schema
        .fields()
        .iter()
        .filter(|f| id_kind(f.data_type()).is_some())
        .map(|f| f.name().as_str())
        .collect();
    let lists: Vec<&str> = schema
        .fields()
        .iter()
        .filter(|f| is_id_list(f.data_type()))
        .map(|f| f.name().as_str())
        .collect();
    let mut says = Vec::new();
    match ids.as_slice() {
        [] => {}
        [a] => says.push(format!("if {a} is a node's id: .graph AS nodes({a})")),
        [a, b, ..] => {
            says.push(format!(
                "if {a} and {b} are the two ends of an edge: .graph AS edges({a}, {b})"
            ));
            says.push(format!("if {a} is a node's id: .graph AS nodes({a})"));
        }
    }
    if let Some(l) = lists.first() {
        says.push(format!("if {l} is a path's nodes: .graph AS path({l})"));
    }
    let says = if says.is_empty() {
        "None of its columns can be a node's id (an integer, a string or a date). Draw a \
         property graph by name (.graph NAME), or a query whose rows are edges: .graph SELECT … \
         AS src, … AS dst FROM …"
            .to_string()
    } else {
        format!(
            "Say what its columns are: {}. Or name the ends in a query: .graph SELECT … AS src, \
             … AS dst FROM …",
            says.join("; ")
        )
    };
    let n = schema.fields().len();
    let detail = format!(
        "cannot draw this result as a graph: it has {n} {} ({}), which {} not edges (columns \
         src and dst, or two columns of ids), nodes (a column id), a hop result (seed, id, \
         distance) or paths (a list of ids). {says}",
        if n == 1 { "column" } else { "columns" },
        columns.join(", "),
        if n == 1 { "is" } else { "are" },
    );
    datafusion::error::DataFusionError::Plan(detail)
}

/// The kind of id a column of type `t` holds, if it can be a node's id: an
/// integer, a string or a date. A float, a boolean, a list or a struct is a
/// value, not an id (a score, a flag, a path), and drawing it as a node is
/// nonsense.
fn id_kind(t: &DataType) -> Option<&'static str> {
    match t {
        DataType::Dictionary(_, values) => id_kind(values),
        DataType::Decimal128(_, 0) | DataType::Decimal256(_, 0) => Some("integer"),
        DataType::Utf8 | DataType::LargeUtf8 | DataType::Utf8View => Some("string"),
        DataType::Date32 | DataType::Date64 | DataType::Timestamp(_, _) => Some("date"),
        t if t.is_integer() => Some("integer"),
        _ => None,
    }
}

/// What a column that is no id is, in a word, for the message.
fn value_kind(t: &DataType) -> &'static str {
    match t {
        DataType::Boolean => "a boolean",
        DataType::Struct(_) => "a struct",
        DataType::Map(_, _) => "a map",
        DataType::List(_)
        | DataType::LargeList(_)
        | DataType::FixedSizeList(_, _)
        | DataType::ListView(_)
        | DataType::LargeListView(_) => "a list",
        t if t.is_floating()
            || matches!(t, DataType::Decimal128(_, _) | DataType::Decimal256(_, _)) =>
        {
            "a number that is not whole"
        }
        _ => "not an id",
    }
}

/// The columns that hold each edge's endpoints: `src` and `dst` (in any
/// case), or else the first two. Either way both must hold ids: an integer, a
/// string or a date. The first two columns must also be ids of one kind, since
/// nothing but their position says they are a pair.
pub fn endpoint_columns(schema: &Schema) -> Result<(usize, usize)> {
    let n = schema.fields().len();
    if n < 2 {
        return plan_err(format!(
            "the result has {n} column{}; a graph needs two, each edge's source and destination \
             (columns named src and dst, or else the first two)",
            if n == 1 { "" } else { "s" }
        ));
    }
    let find = |name: &str| {
        schema
            .fields()
            .iter()
            .position(|f| f.name().eq_ignore_ascii_case(name))
    };
    let (s, d, named) = match (find("src"), find("dst")) {
        (Some(s), Some(d)) => (s, d, true),
        _ => (0, 1, false),
    };
    let (source, destination) = (schema.field(s), schema.field(d));
    let kinds = (
        id_kind(source.data_type()),
        id_kind(destination.data_type()),
    );
    let what = match kinds {
        (Some(a), Some(b)) if named || a == b => return Ok((s, d)),
        (Some(a), Some(b)) => format!(
            "'{}' holds {a} ids and '{}' holds {b} ids, so they are not the two ends of an edge",
            source.name(),
            destination.name()
        ),
        _ => [(source, kinds.0), (destination, kinds.1)]
            .iter()
            .filter(|(_, kind)| kind.is_none())
            .map(|(f, _)| {
                format!(
                    "'{}' is {} ({})",
                    f.name(),
                    value_kind(f.data_type()),
                    f.data_type()
                )
            })
            .collect::<Vec<_>>()
            .join(" and "),
    };
    // A column named after a per-node algorithm is that algorithm's result.
    let metric = schema
        .fields()
        .iter()
        .find_map(|f| per_node_algorithm(&f.name().to_ascii_lowercase()))
        .unwrap_or("pagerank");
    plan_err(format!(
        "cannot draw {} as edges: {what}. An edge's ends are node ids (integers, strings or \
         dates, both of one kind). To draw an algorithm's result, draw the graph and size or \
         colour it: .graph NAME SIZE BY {metric}. To draw other rows, name the ends: \
         .graph SELECT … AS src, … AS dst FROM …",
        if named {
            "columns src and dst".to_string()
        } else {
            format!(
                "the first two columns ({}: {}, {}: {})",
                source.name(),
                source.data_type(),
                destination.name(),
                destination.data_type()
            )
        }
    ))
}
