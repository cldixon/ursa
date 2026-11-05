//! The function specs: what each graph function takes and returns.

use arrow::datatypes::DataType;

use super::call::{Call, Values};
use crate::algo::AggKind;
use crate::topology::Direction;

/// What kind of function a spec describes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Family {
    /// A per-node algorithm: one value per node.
    Algorithm,
    /// A layout: per-node `x` and `y` positions from one run.
    Layout,
    /// A per-node aggregation of a node attribute over each node's neighbours.
    Aggregation,
    /// A whole-graph statistic: one row.
    Statistic,
}

/// How many rows a function's outputs have.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shape {
    /// One row per node, in dense node order.
    PerNode,
    /// A single row.
    OneRow,
}

/// A parameter value as the catalog stores it: a default, or a resolved
/// argument (see [`Call::values`]).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    UInt(u64),
    Float(f64),
    Str(&'static str),
}

/// The directions every directed function accepts.
pub const ALL_DIRECTIONS: &[&str] = &["out", "in", "both"];
/// The directions of `pagerank` and `betweenness`, which reject `'in'`.
pub const OUT_OR_BOTH: &[&str] = &["out", "both"];
/// The output dtypes a float-valued function can emit.
const DTYPES: &[&str] = &["f64", "f32"];
/// The neighbour aggregations.
const AGGS: &[&str] = &["mean", "sum", "min", "max", "count", "n_unique"];

/// What a parameter accepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParamType {
    /// A finite number; an integer is accepted.
    Float,
    /// A finite number, or null.
    OptionalFloat,
    /// A non-negative integer below 2^32.
    Count,
    /// A non-negative integer below 2^64, or null for the kernel's fixed
    /// default seed.
    Seed,
    /// A boolean.
    Bool,
    /// One of these strings.
    Choice(&'static [&'static str]),
    /// A direction: one of these of `'out'`, `'in'` and `'both'`.
    Direction(&'static [&'static str]),
    /// The output dtype of a float-valued function: `'f64'` or `'f32'`, which
    /// narrows the emitted column (the kernel still computes in `f64`).
    Dtype,
    /// A per-edge weight: an expression over the edge columns the front end
    /// evaluates to one non-negative `f64` per edge row and passes as
    /// [`Inputs::weights`](super::Inputs). Null (the default) is unweighted.
    Weight,
    /// A per-node attribute: the column a neighbour aggregation reads, evaluated
    /// by the front end and passed as [`Inputs::attribute`](super::Inputs).
    Attribute,
}

impl ParamType {
    /// The accepted strings of a choice, direction or dtype; empty otherwise.
    pub fn choices(&self) -> &'static [&'static str] {
        match self {
            ParamType::Choice(c) | ParamType::Direction(c) => c,
            ParamType::Dtype => DTYPES,
            _ => &[],
        }
    }

    /// Whether the front end evaluates this parameter itself (a weight or an
    /// attribute) instead of passing it to [`FunctionSpec::resolve`].
    pub fn is_expression(&self) -> bool {
        matches!(self, ParamType::Weight | ParamType::Attribute)
    }
}

/// One parameter of a function.
#[derive(Debug, Clone, Copy)]
pub struct ParamSpec {
    pub name: &'static str,
    pub ty: ParamType,
    /// The value when the argument is not given.
    pub default: Value,
    /// One line on what it does.
    pub doc: &'static str,
}

/// The Arrow type of an output column, before any `dtype` narrowing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColumnType {
    UInt32,
    Int64,
    Float32,
    Float64,
}

impl ColumnType {
    pub fn data_type(&self) -> DataType {
        match self {
            ColumnType::UInt32 => DataType::UInt32,
            ColumnType::Int64 => DataType::Int64,
            ColumnType::Float32 => DataType::Float32,
            ColumnType::Float64 => DataType::Float64,
        }
    }
}

/// One output column of a function.
#[derive(Debug, Clone, Copy)]
pub struct OutputSpec {
    /// The column's name: the function's name for a single-output function.
    pub name: &'static str,
    /// Its type at `dtype='f64'`. `dtype='f32'` narrows a `Float64` column of a
    /// function with a [`ParamType::Dtype`] parameter to `Float32`.
    pub ty: ColumnType,
    /// Whether a value can be null (a neighbour aggregation over no attributed
    /// neighbours, `describe`'s `n_components` without `full`).
    pub nullable: bool,
}

/// One graph function.
#[derive(Debug)]
pub struct FunctionSpec {
    pub name: &'static str,
    pub family: Family,
    /// One line on what it computes.
    pub summary: &'static str,
    /// The NetworkX call it matches, as `docs/semantics.md` pins it (`G` the
    /// `DiGraph` of the rows, `U = nx.Graph(G)`); `None` where nothing is pinned.
    pub networkx: Option<&'static str>,
    /// The parameters besides the graph, in the Python API's order.
    pub params: &'static [ParamSpec],
    pub outputs: &'static [OutputSpec],
    pub shape: Shape,
    /// Builds the call from validated values, one per parameter.
    pub(super) build: fn(&Values) -> Call,
}

impl FunctionSpec {
    /// The parameter called `name`.
    pub fn param(&self, name: &str) -> Option<&'static ParamSpec> {
        // `params` is `'static`, so the reference outlives `self`.
        let params: &'static [ParamSpec] = self.params;
        params.iter().find(|p| p.name == name)
    }

    /// The directions the function accepts, as their names; empty for one
    /// without a `direction` parameter.
    pub fn directions(&self) -> &'static [&'static str] {
        self.params
            .iter()
            .find_map(|p| match p.ty {
                ParamType::Direction(d) => Some(d),
                _ => None,
            })
            .unwrap_or(&[])
    }

    /// Whether the function takes a `weight`.
    pub fn weighted(&self) -> bool {
        self.params.iter().any(|p| p.ty == ParamType::Weight)
    }

    /// Whether the function takes a `dtype`: its output is `Float64` and can be
    /// narrowed to `Float32`.
    pub fn narrows(&self) -> bool {
        self.params.iter().any(|p| p.ty == ParamType::Dtype)
    }

    /// Whether the function reads a node attribute (a neighbour aggregation).
    pub fn reads_attribute(&self) -> bool {
        self.params.iter().any(|p| p.ty == ParamType::Attribute)
    }
}

/// The function called `name`.
pub fn lookup(name: &str) -> Option<&'static FunctionSpec> {
    FUNCTIONS.iter().find(|f| f.name == name)
}

/// Every function's name, in catalog order.
pub fn names() -> impl Iterator<Item = &'static str> {
    FUNCTIONS.iter().map(|f| f.name)
}

/// A direction's name: `'out'`, `'in'` or `'both'`.
pub fn direction_name(d: Direction) -> &'static str {
    match d {
        Direction::Out => "out",
        Direction::In => "in",
        Direction::Both => "both",
    }
}

/// The direction called `name`.
pub fn parse_direction(name: &str) -> Option<Direction> {
    match name {
        "out" => Some(Direction::Out),
        "in" => Some(Direction::In),
        "both" => Some(Direction::Both),
        _ => None,
    }
}

// --- the table ---------------------------------------------------------------

const fn param(name: &'static str, ty: ParamType, default: Value, doc: &'static str) -> ParamSpec {
    ParamSpec {
        name,
        ty,
        default,
        doc,
    }
}

const fn out(name: &'static str, ty: ColumnType) -> OutputSpec {
    OutputSpec {
        name,
        ty,
        nullable: false,
    }
}

const DIRECTION_ALL: ParamSpec = param(
    "direction",
    ParamType::Direction(ALL_DIRECTIONS),
    Value::Str("out"),
    "Follow out-edges, in-edges, or both (the undirected view).",
);
const DIRECTION_OUT_OR_BOTH: ParamSpec = param(
    "direction",
    ParamType::Direction(OUT_OR_BOTH),
    Value::Str("out"),
    "Follow the rows as stored, or read the graph as undirected.",
);
const WEIGHT: ParamSpec = param(
    "weight",
    ParamType::Weight,
    Value::Null,
    "A non-negative weight per edge; null is unweighted.",
);
const DTYPE: ParamSpec = param(
    "dtype",
    ParamType::Dtype,
    Value::Str("f64"),
    "Emit the column as 'f64' or 'f32'.",
);
const SEED: ParamSpec = param(
    "seed",
    ParamType::Seed,
    Value::Null,
    "Seed for the deterministic random stream; null is a fixed default.",
);
const LAYOUT_K: ParamSpec = param(
    "k",
    ParamType::Float,
    Value::Float(1.0),
    "Ideal edge length; sets the units of the positions.",
);
const XY: &[OutputSpec] = &[out("x", ColumnType::Float32), out("y", ColumnType::Float32)];

/// Every graph function the Python API exposes, besides the traversals.
pub static FUNCTIONS: &[FunctionSpec] = &[
    FunctionSpec {
        name: "degree",
        family: Family::Algorithm,
        summary: "Per-node degree.",
        networkx: Some("G.out_degree() (in: G.in_degree(); both: U.degree() on a simple graph)"),
        params: &[DIRECTION_ALL],
        outputs: &[out("degree", ColumnType::UInt32)],
        shape: Shape::PerNode,
        build: |v| Call::Degree {
            direction: v.direction("direction"),
        },
    },
    FunctionSpec {
        name: "pagerank",
        family: Family::Algorithm,
        summary: "PageRank (pull-based fixpoint); a dangling node spreads its score to all nodes.",
        networkx: Some("nx.pagerank(G, alpha=damping) (both: nx.pagerank(U))"),
        params: &[
            param(
                "damping",
                ParamType::Float,
                Value::Float(0.85),
                "Probability of following an edge.",
            ),
            param(
                "max_iter",
                ParamType::Count,
                Value::UInt(30),
                "Iteration cap.",
            ),
            param(
                "tol",
                ParamType::Float,
                Value::Float(1e-6),
                "Convergence tolerance on the L1 change.",
            ),
            WEIGHT,
            DTYPE,
            DIRECTION_OUT_OR_BOTH,
        ],
        outputs: &[out("pagerank", ColumnType::Float64)],
        shape: Shape::PerNode,
        build: |v| Call::PageRank {
            damping: v.float("damping"),
            max_iter: v.count("max_iter"),
            tol: v.float("tol"),
            direction: v.direction("direction"),
        },
    },
    FunctionSpec {
        name: "connected_components",
        family: Family::Algorithm,
        summary: "A component label per node: weak (ignoring direction) or strong.",
        networkx: Some("nx.connected_components(U) (strong: nx.strongly_connected_components(G))"),
        params: &[param(
            "mode",
            ParamType::Choice(&["weak", "strong"]),
            Value::Str("weak"),
            "'weak' ignores edge direction; 'strong' needs a path each way.",
        )],
        outputs: &[out("connected_components", ColumnType::UInt32)],
        shape: Shape::PerNode,
        build: |v| Call::ConnectedComponents {
            strong: v.str("mode") == "strong",
        },
    },
    FunctionSpec {
        name: "triangle_count",
        family: Family::Algorithm,
        summary: "Per-node triangle count, reading edges as undirected.",
        networkx: Some("nx.triangles(U)"),
        params: &[],
        outputs: &[out("triangle_count", ColumnType::UInt32)],
        shape: Shape::PerNode,
        build: |_| Call::TriangleCount,
    },
    FunctionSpec {
        name: "clustering_coefficient",
        family: Family::Algorithm,
        summary: "Local clustering coefficient, derived from the triangle counts.",
        networkx: Some("nx.clustering(U)"),
        params: &[DTYPE],
        outputs: &[out("clustering_coefficient", ColumnType::Float64)],
        shape: Shape::PerNode,
        build: |_| Call::ClusteringCoefficient,
    },
    FunctionSpec {
        name: "betweenness",
        family: Family::Algorithm,
        summary: "Betweenness centrality (Brandes), unnormalized.",
        networkx: Some(
            "nx.betweenness_centrality(G, normalized=False, endpoints=False) \
             (both: nx.betweenness_centrality(U, normalized=False))",
        ),
        params: &[
            param(
                "sample",
                ParamType::OptionalFloat,
                Value::Null,
                "Estimate from this fraction of the sources; null is exact.",
            ),
            WEIGHT,
            SEED,
            DTYPE,
            DIRECTION_OUT_OR_BOTH,
        ],
        outputs: &[out("betweenness", ColumnType::Float64)],
        shape: Shape::PerNode,
        build: |v| Call::Betweenness {
            sample: v.optional_float("sample"),
            seed: v.seed("seed"),
            direction: v.direction("direction"),
        },
    },
    FunctionSpec {
        name: "closeness",
        family: Family::Algorithm,
        summary: "Closeness centrality, reachable / sum of distances.",
        networkx: Some(
            "nx.closeness_centrality(G.reverse(), wf_improved=False) \
             (in: on G; both: on U)",
        ),
        params: &[WEIGHT, DTYPE, DIRECTION_ALL],
        outputs: &[out("closeness", ColumnType::Float64)],
        shape: Shape::PerNode,
        build: |v| Call::Closeness {
            direction: v.direction("direction"),
        },
    },
    FunctionSpec {
        name: "label_propagation",
        family: Family::Algorithm,
        summary: "Community labels by label propagation.",
        networkx: Some("nx.community.label_propagation_communities(U)"),
        params: &[
            param(
                "max_iter",
                ParamType::Count,
                Value::UInt(20),
                "Iteration cap.",
            ),
            SEED,
        ],
        outputs: &[out("label_propagation", ColumnType::UInt32)],
        shape: Shape::PerNode,
        build: |v| Call::LabelPropagation {
            max_iter: v.count("max_iter"),
            seed: v.seed("seed"),
        },
    },
    FunctionSpec {
        name: "louvain",
        family: Family::Algorithm,
        summary: "Community labels by Louvain modularity optimization.",
        networkx: Some("nx.community.louvain_communities(U)"),
        params: &[
            WEIGHT,
            param(
                "resolution",
                ParamType::Float,
                Value::Float(1.0),
                "Modularity resolution; larger gives smaller communities.",
            ),
            SEED,
        ],
        outputs: &[out("louvain", ColumnType::UInt32)],
        shape: Shape::PerNode,
        build: |v| Call::Louvain {
            resolution: v.float("resolution"),
            seed: v.seed("seed"),
        },
    },
    FunctionSpec {
        name: "layout_fr",
        family: Family::Layout,
        summary: "Fruchterman-Reingold force-directed layout.",
        networkx: None,
        params: &[
            param(
                "iterations",
                ParamType::Count,
                Value::UInt(300),
                "Simulation steps.",
            ),
            LAYOUT_K,
            param(
                "gravity",
                ParamType::Float,
                Value::Float(0.02),
                "Pull toward the origin, which keeps components together.",
            ),
            SEED,
        ],
        outputs: XY,
        shape: Shape::PerNode,
        build: |v| Call::LayoutFr {
            iterations: v.count("iterations"),
            k: v.float("k"),
            gravity: v.float("gravity"),
            seed: v.seed("seed"),
        },
    },
    FunctionSpec {
        name: "layout_random",
        family: Family::Layout,
        summary: "A deterministic spread over a phyllotactic spiral.",
        networkx: None,
        params: &[LAYOUT_K, SEED],
        outputs: XY,
        shape: Shape::PerNode,
        build: |v| Call::LayoutRandom {
            k: v.float("k"),
            seed: v.seed("seed"),
        },
    },
    FunctionSpec {
        name: "layout_circle",
        family: Family::Layout,
        summary: "Nodes evenly spaced on a circle, in node order.",
        networkx: None,
        params: &[LAYOUT_K],
        outputs: XY,
        shape: Shape::PerNode,
        build: |v| Call::LayoutCircle { k: v.float("k") },
    },
    FunctionSpec {
        name: "neighbors_agg",
        family: Family::Aggregation,
        summary: "A node attribute aggregated over each node's neighbours.",
        networkx: None,
        params: &[
            DIRECTION_ALL,
            param(
                "attribute",
                ParamType::Attribute,
                Value::Null,
                "The node attribute to aggregate.",
            ),
            param(
                "agg",
                ParamType::Choice(AGGS),
                Value::Str("mean"),
                "The aggregation; mean/sum/min/max need a numeric attribute.",
            ),
            DTYPE,
        ],
        outputs: &[OutputSpec {
            name: "neighbors_agg",
            ty: ColumnType::Float64,
            nullable: true,
        }],
        shape: Shape::PerNode,
        build: |v| Call::NeighborAgg {
            direction: v.direction("direction"),
            agg: match v.str("agg") {
                "sum" => AggKind::Sum,
                "min" => AggKind::Min,
                "max" => AggKind::Max,
                "count" => AggKind::Count,
                "n_unique" => AggKind::NUnique,
                _ => AggKind::Mean,
            },
        },
    },
    FunctionSpec {
        name: "density",
        family: Family::Statistic,
        summary: "Edges present over edges possible.",
        networkx: Some("nx.density(G) on a simple graph (both: nx.density(U))"),
        params: &[DIRECTION_ALL],
        outputs: &[out("density", ColumnType::Float64)],
        shape: Shape::OneRow,
        build: |v| Call::Density {
            direction: v.direction("direction"),
        },
    },
    FunctionSpec {
        name: "avg_path_length",
        family: Family::Statistic,
        summary: "Mean shortest-path length over reachable ordered pairs.",
        networkx: Some("nx.average_shortest_path_length(U) on a connected graph (both)"),
        params: &[
            param(
                "sample",
                ParamType::OptionalFloat,
                Value::Null,
                "Estimate from this fraction of the sources; null is exact.",
            ),
            DIRECTION_ALL,
        ],
        outputs: &[out("avg_path_length", ColumnType::Float64)],
        shape: Shape::OneRow,
        build: |v| Call::AvgPathLength {
            sample: v.optional_float("sample"),
            direction: v.direction("direction"),
        },
    },
    FunctionSpec {
        name: "diameter",
        family: Family::Statistic,
        summary: "The longest shortest path over reachable ordered pairs.",
        networkx: Some("nx.diameter(U) on a connected graph (both, approximate=False)"),
        params: &[
            param(
                "approximate",
                ParamType::Bool,
                Value::Bool(true),
                "A lower bound from a bounded sample of sources.",
            ),
            DIRECTION_ALL,
        ],
        outputs: &[out("diameter", ColumnType::Int64)],
        shape: Shape::OneRow,
        build: |v| Call::Diameter {
            approximate: v.bool("approximate"),
            direction: v.direction("direction"),
        },
    },
    FunctionSpec {
        name: "describe",
        family: Family::Statistic,
        summary: "A one-row summary: nodes, edges, density, mean degree, components.",
        networkx: Some("U.number_of_edges(), nx.density(U) and the mean degree (both)"),
        params: &[
            param(
                "full",
                ParamType::Bool,
                Value::Bool(false),
                "Also count the weak components (n_components is null otherwise).",
            ),
            DIRECTION_ALL,
            param(
                "nodes",
                ParamType::Choice(&["all", "touched"]),
                Value::Str("all"),
                "Describe every node, or only those at an end of a kept edge.",
            ),
        ],
        outputs: &[
            out("n_nodes", ColumnType::Int64),
            out("n_edges", ColumnType::Int64),
            out("density", ColumnType::Float64),
            out("avg_degree", ColumnType::Float64),
            OutputSpec {
                name: "n_components",
                ty: ColumnType::Int64,
                nullable: true,
            },
        ],
        shape: Shape::OneRow,
        build: |v| Call::Describe {
            full: v.bool("full"),
            direction: v.direction("direction"),
            touched: v.str("nodes") == "touched",
        },
    },
];
