//! The traversals in SQL: `hop`, `shortest_path` and `random_walk`.
//!
//! A traversal returns rows of its own (reached pairs, path edges, walk
//! steps), not one row per node, and some of its parameters are sets of node
//! ids rather than literals. So the traversals are not in
//! `ursa_core::catalog`, whose specs, [`ursa_core::catalog::Call`]s and
//! [`ursa_core::catalog::Evaluator`] describe per-node columns and one-row
//! statistics; they are specified here, with the same names, defaults, checks
//! and output columns as the Python library's `ur.hop(edges, n,
//! direction).from_(seeds)`, `ur.shortest_path(edges, source, target, weight,
//! direction)` and `ur.random_walk(edges, start, steps, walks_per_node,
//! seed)` (`python/ursa/_graph.py`, `ursa-plan`'s `result.rs`). The parity
//! fixture records the Python signatures and outputs, and
//! `tests/parity.rs` holds these specs to them.
//!
//! The literal parameters use the catalog's [`ParamType`]s and are checked by
//! the catalog's rules (a count is a non-negative integer below 2^32, a seed a
//! non-negative integer or NULL, a direction one of the listed names). The
//! node-id parameters are planned by the relation planner as child plans of
//! the traversal node (see `relation`), and their values are cast to the
//! graph's id type when the node runs.
//!
//! The kernels are `ursa_core::algo`'s, called exactly as `ursa-plan` calls
//! them, so the rows are the Python library's, in the same order.

use std::sync::Arc;

use datafusion::arrow::array::{ArrayRef, Float64Array, Int64Array};
use datafusion::arrow::datatypes::{DataType, Field, Schema};
use datafusion::arrow::record_batch::RecordBatch;
use datafusion::error::{DataFusionError, Result};
use ursa_core::algo::{
    k_hop_masked, random_walk_masked, shortest_path_masked, shortest_path_weighted_with_cost_masked,
};
use ursa_core::catalog::{
    direction_name, parse_direction, Expected, ParamType, Value, ALL_DIRECTIONS,
};
use ursa_core::{Direction, IdMap, Topology};

use super::functions::{did_you_mean, expected_text, plan, sql_value, Literal};

/// What a traversal parameter takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TraversalParamType {
    /// Node ids: a list literal (`[0, 1]`), a scalar subquery returning one
    /// column (`(SELECT id FROM ...)`), or one id.
    Nodes,
    /// One node id: a literal or a scalar subquery returning at most one row.
    Node,
    /// A literal or a weight expression, as a catalog function's parameter.
    Param(ParamType),
}

/// One parameter of a traversal, besides the graph.
#[derive(Debug, Clone, Copy)]
pub struct TraversalParam {
    pub name: &'static str,
    pub ty: TraversalParamType,
    /// The value when the argument is not given; `None` if it must be given.
    pub default: Option<Value>,
    /// One line on what it does.
    pub doc: &'static str,
}

impl TraversalParam {
    /// Whether the parameter is a node id or a set of them.
    pub fn takes_nodes(&self) -> bool {
        matches!(
            self.ty,
            TraversalParamType::Nodes | TraversalParamType::Node
        )
    }

    /// Whether the parameter is the weight expression.
    pub fn is_weight(&self) -> bool {
        self.ty == TraversalParamType::Param(ParamType::Weight)
    }

    /// The parameter as a signature shows it: its default, or a placeholder
    /// for a required one.
    pub fn shown_default(&self) -> String {
        match (self.default, self.ty) {
            (Some(v), _) => sql_value(&v),
            (None, TraversalParamType::Nodes) => "[<id>, ...]".to_string(),
            (None, TraversalParamType::Node) => "<id>".to_string(),
            (None, TraversalParamType::Param(_)) => "<integer>".to_string(),
        }
    }
}

/// The type of a traversal's output column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputType {
    /// A node id, of the graph's id type (`Int64` or `Utf8`).
    Id,
    Int64,
    Float64,
}

impl OutputType {
    pub fn data_type(&self, id_type: &DataType) -> DataType {
        match self {
            OutputType::Id => id_type.clone(),
            OutputType::Int64 => DataType::Int64,
            OutputType::Float64 => DataType::Float64,
        }
    }
}

/// One output column of a traversal. None is nullable.
#[derive(Debug, Clone, Copy)]
pub struct TraversalOutput {
    pub name: &'static str,
    pub ty: OutputType,
}

/// One traversal.
#[derive(Debug)]
pub struct TraversalSpec {
    pub name: &'static str,
    /// One line on what it returns.
    pub summary: &'static str,
    /// The NetworkX call it matches, as `docs/semantics.md` pins it.
    pub networkx: Option<&'static str>,
    /// The parameters besides the graph, in SQL's order: the node ids first.
    pub params: &'static [TraversalParam],
    pub outputs: &'static [TraversalOutput],
}

impl TraversalSpec {
    /// The parameter called `name`.
    pub fn param(&self, name: &str) -> Option<&'static TraversalParam> {
        let params: &'static [TraversalParam] = self.params;
        params.iter().find(|p| p.name == name)
    }

    /// The parameters that take node ids, in order: each is one child plan of
    /// the traversal node.
    pub fn node_params(&self) -> impl Iterator<Item = &'static TraversalParam> {
        let params: &'static [TraversalParam] = self.params;
        params.iter().filter(|p| p.takes_nodes())
    }

    /// The directions the traversal accepts; empty for one without a
    /// `direction` parameter.
    pub fn directions(&self) -> &'static [&'static str] {
        self.params
            .iter()
            .find_map(|p| match p.ty {
                TraversalParamType::Param(ParamType::Direction(d)) => Some(d),
                _ => None,
            })
            .unwrap_or(&[])
    }

    /// Whether the traversal takes a `weight`.
    pub fn weighted(&self) -> bool {
        self.params.iter().any(TraversalParam::is_weight)
    }

    /// The output schema over a graph whose node ids are `id_type`.
    pub fn schema(&self, id_type: &DataType) -> Schema {
        Schema::new(
            self.outputs
                .iter()
                .map(|o| Field::new(o.name, o.ty.data_type(id_type), false))
                .collect::<Vec<_>>(),
        )
    }

    /// `hop(<graph>, seeds => [<id>, ...], n => 1, direction => 'out')`: the
    /// call with every parameter at its default, for messages.
    pub fn signature(&self) -> String {
        let mut parts = vec!["<graph>".to_string()];
        parts.extend(
            self.params
                .iter()
                .map(|p| format!("{} => {}", p.name, p.shown_default())),
        );
        format!("{}({})", self.name, parts.join(", "))
    }
}

const fn param(
    name: &'static str,
    ty: TraversalParamType,
    default: Option<Value>,
    doc: &'static str,
) -> TraversalParam {
    TraversalParam {
        name,
        ty,
        default,
        doc,
    }
}

const fn out(name: &'static str, ty: OutputType) -> TraversalOutput {
    TraversalOutput { name, ty }
}

const DIRECTION: TraversalParam = param(
    "direction",
    TraversalParamType::Param(ParamType::Direction(ALL_DIRECTIONS)),
    Some(Value::Str("out")),
    "Follow out-edges, in-edges, or both (the undirected view).",
);

/// Every traversal, in the Python API's order.
pub static TRAVERSALS: &[TraversalSpec] = &[
    TraversalSpec {
        name: "hop",
        summary: "The nodes within n steps of each seed: one (src, dst) row per seed and node \
                  reached, src the seed. A seed is not its own row; a repeated seed repeats its \
                  rows.",
        networkx: Some(
            "nx.single_source_shortest_path_length(G, seed, cutoff=n), the seed left out \
             (in: on G.reverse(); both: on U)",
        ),
        params: &[
            param(
                "seeds",
                TraversalParamType::Nodes,
                None,
                "The nodes to start from: a list, a query of one column, or one id.",
            ),
            param(
                "n",
                TraversalParamType::Param(ParamType::Count),
                Some(Value::UInt(1)),
                "How many steps to take.",
            ),
            DIRECTION,
        ],
        outputs: &[out("src", OutputType::Id), out("dst", OutputType::Id)],
    },
    TraversalSpec {
        name: "shortest_path",
        summary: "One shortest path from source to target: one row per edge, in order, with its \
                  0-based position (hop) and the cost from source to its end. No path, no rows.",
        networkx: Some(
            "nx.shortest_path(G, source, target) (weighted: nx.dijkstra_path(G, source, target, \
             weight=...))",
        ),
        params: &[
            param(
                "source",
                TraversalParamType::Node,
                None,
                "The node the path starts at: an id, or a query of one value.",
            ),
            param(
                "target",
                TraversalParamType::Node,
                None,
                "The node the path ends at: an id, or a query of one value.",
            ),
            param(
                "weight",
                TraversalParamType::Param(ParamType::Weight),
                Some(Value::Null),
                "A non-negative weight per edge for the least-cost path; null is the fewest \
                 edges.",
            ),
            DIRECTION,
        ],
        outputs: &[
            out("src", OutputType::Id),
            out("dst", OutputType::Id),
            out("hop", OutputType::Int64),
            out("cost", OutputType::Float64),
        ],
    },
    TraversalSpec {
        name: "random_walk",
        summary: "Random walks along out-edges from each start node: one (walk_id, step, node) \
                  row per step. A walk stops early at a node with no out-edges.",
        networkx: None,
        params: &[
            param(
                "start",
                TraversalParamType::Nodes,
                None,
                "The nodes to start walks at: a list, a query of one column, or one id.",
            ),
            param(
                "steps",
                TraversalParamType::Param(ParamType::Count),
                None,
                "How many steps each walk takes at most.",
            ),
            param(
                "walks_per_node",
                TraversalParamType::Param(ParamType::Count),
                Some(Value::UInt(1)),
                "How many walks start at each start node.",
            ),
            param(
                "seed",
                TraversalParamType::Param(ParamType::Seed),
                Some(Value::Null),
                "Seed for the deterministic random stream; null is a fixed default.",
            ),
        ],
        outputs: &[
            out("walk_id", OutputType::Int64),
            out("step", OutputType::Int64),
            out("node", OutputType::Id),
        ],
    },
];

/// The traversal called `name`.
pub fn lookup(name: &str) -> Option<&'static TraversalSpec> {
    TRAVERSALS.iter().find(|t| t.name == name)
}

/// A validated traversal call: which traversal, with every literal parameter
/// set. Its node ids and weight are the node's child plans.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Traversal {
    Hop {
        n: u32,
        direction: Direction,
    },
    ShortestPath {
        direction: Direction,
    },
    RandomWalk {
        steps: u32,
        walks_per_node: u32,
        seed: Option<u64>,
    },
}

impl Traversal {
    pub fn spec(&self) -> &'static TraversalSpec {
        let name = match self {
            Traversal::Hop { .. } => "hop",
            Traversal::ShortestPath { .. } => "shortest_path",
            Traversal::RandomWalk { .. } => "random_walk",
        };
        lookup(name).expect("every traversal has a spec")
    }

    pub fn name(&self) -> &'static str {
        self.spec().name
    }

    /// The literal parameter `name`'s value.
    fn value(&self, name: &str) -> Option<Value> {
        let seed = |s: Option<u64>| s.map_or(Value::Null, Value::UInt);
        Some(match (self, name) {
            (Traversal::Hop { n, .. }, "n") => Value::UInt(u64::from(*n)),
            (Traversal::Hop { direction, .. }, "direction")
            | (Traversal::ShortestPath { direction }, "direction") => {
                Value::Str(direction_name(*direction))
            }
            (Traversal::RandomWalk { steps, .. }, "steps") => Value::UInt(u64::from(*steps)),
            (Traversal::RandomWalk { walks_per_node, .. }, "walks_per_node") => {
                Value::UInt(u64::from(*walks_per_node))
            }
            (Traversal::RandomWalk { seed: s, .. }, "seed") => seed(*s),
            _ => return None,
        })
    }

    /// The call as SQL would spell it, for `EXPLAIN`: the node arguments and
    /// the weight as written, then every literal parameter.
    pub fn display(&self, nodes: &[String], weight: Option<&str>) -> String {
        let mut nodes = nodes.iter();
        let parts: Vec<String> = self
            .spec()
            .params
            .iter()
            .filter_map(|p| {
                if p.takes_nodes() {
                    return nodes.next().map(|n| format!("{} => {n}", p.name));
                }
                if p.is_weight() {
                    return weight.map(|w| format!("{} => {w}", p.name));
                }
                self.value(p.name)
                    .map(|v| format!("{} => {}", p.name, sql_value(&v)))
            })
            .collect();
        format!("{}({})", self.name(), parts.join(", "))
    }
}

/// "hop() has no parameter 'm'; did you mean 'n'? ...".
pub fn unknown_param(spec: &TraversalSpec, name: &str) -> DataFusionError {
    let names: Vec<&str> = spec.params.iter().map(|p| p.name).collect();
    let hint = did_you_mean(name, names.iter().copied())
        .map(|s| format!("; did you mean '{s}'?"))
        .unwrap_or_else(|| ".".to_string());
    plan(format!(
        "{}() has no parameter '{name}'{hint} Its parameters are: {}",
        spec.name,
        names.join(", ")
    ))
}

/// "random_walk() needs 'steps': random_walk(<graph>, ...)".
pub fn missing_param(spec: &TraversalSpec, param: &TraversalParam) -> DataFusionError {
    plan(format!(
        "{}() needs '{}': {}",
        spec.name,
        param.name,
        spec.signature()
    ))
}

/// Check one literal against a parameter, by the catalog's rules.
fn check(spec: &TraversalSpec, name: &str, ty: ParamType, literal: &Literal) -> Result<Value> {
    let expected = |want: Expected| {
        plan(format!(
            "{}({name} =>) must be {}; got {literal}",
            spec.name,
            expected_text(want)
        ))
    };
    let int = || match literal {
        Literal::Number(n) => n.parse::<i128>().ok(),
        _ => None,
    };
    match ty {
        ParamType::Count => match int() {
            Some(i) if (0..=i128::from(u32::MAX)).contains(&i) => Ok(Value::UInt(i as u64)),
            _ => Err(expected(Expected::Count)),
        },
        ParamType::Seed => match (literal, int()) {
            (Literal::Null, _) => Ok(Value::Null),
            (_, Some(i)) if (0..=i128::from(u64::MAX)).contains(&i) => Ok(Value::UInt(i as u64)),
            _ => Err(expected(Expected::Seed)),
        },
        ParamType::Direction(choices) => match literal {
            Literal::String(s) if choices.contains(&s.as_str()) => Ok(Value::Str(
                choices
                    .iter()
                    .find(|c| **c == s.as_str())
                    .expect("checked above"),
            )),
            _ => {
                let options: Vec<String> = choices.iter().map(|o| format!("'{o}'")).collect();
                Err(plan(format!(
                    "{}({name} => {literal}) is not supported; use one of {}",
                    spec.name,
                    options.join(", ")
                )))
            }
        },
        other => Err(DataFusionError::Internal(format!(
            "{}({name}): no check for a parameter of type {other:?}",
            spec.name
        ))),
    }
}

/// Resolve a traversal's literal arguments (names normalized and known, none
/// repeated; node ids and the weight already taken out by the caller) to a
/// [`Traversal`].
pub fn resolve(spec: &'static TraversalSpec, literals: &[(String, Literal)]) -> Result<Traversal> {
    let mut values: Vec<(&'static str, Value)> = Vec::new();
    for p in spec.params {
        let TraversalParamType::Param(ty) = p.ty else {
            continue;
        };
        if ty == ParamType::Weight {
            continue;
        }
        let value = match literals.iter().find(|(n, _)| n == p.name) {
            Some((_, literal)) => check(spec, p.name, ty, literal)?,
            None => p.default.ok_or_else(|| missing_param(spec, p))?,
        };
        values.push((p.name, value));
    }
    let get = |name: &str| {
        values
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, v)| *v)
            .expect("every literal parameter has a value")
    };
    let count = |name: &str| match get(name) {
        Value::UInt(u) => u as u32,
        other => unreachable!("a count is checked: {other:?}"),
    };
    let seed = |name: &str| match get(name) {
        Value::UInt(u) => Some(u),
        _ => None,
    };
    let direction = |name: &str| match get(name) {
        Value::Str(s) => parse_direction(s).expect("a direction is checked"),
        other => unreachable!("a direction is checked: {other:?}"),
    };
    Ok(match spec.name {
        "hop" => Traversal::Hop {
            n: count("n"),
            direction: direction("direction"),
        },
        "shortest_path" => Traversal::ShortestPath {
            direction: direction("direction"),
        },
        "random_walk" => Traversal::RandomWalk {
            steps: count("steps"),
            walks_per_node: count("walks_per_node"),
            seed: seed("seed"),
        },
        other => unreachable!("no traversal {other}"),
    })
}

/// Run `traversal` over a graph. `nodes` holds, per node parameter in order,
/// its ids as dense indexes (`None` for an id not in the graph or a null);
/// `weights` is one `f64` per edge row for a weighted shortest path.
pub fn batch(
    topology: &Topology,
    ids: &IdMap,
    traversal: &Traversal,
    nodes: &[Vec<Option<u32>>],
    weights: Option<&[f64]>,
    schema: Arc<Schema>,
) -> Result<RecordBatch> {
    // An id not in the graph reaches nothing (#225), as in ursa-plan.
    let known = |i: usize| -> Vec<u32> { nodes[i].iter().flatten().copied().collect() };
    let columns: Vec<ArrayRef> = match *traversal {
        Traversal::Hop { n, direction } => {
            let (seed, reached) = k_hop_masked(topology, None, &known(0), n, direction);
            vec![ids.gather_user(&seed), ids.gather_user(&reached)]
        }
        Traversal::ShortestPath { direction } => {
            let one = |i: usize| nodes[i].first().copied().flatten();
            let route = match (one(0), one(1)) {
                (Some(source), Some(target)) => match weights {
                    Some(w) => shortest_path_weighted_with_cost_masked(
                        topology, None, w, source, target, direction,
                    )
                    .map(|(path, costs)| (path, Some(costs))),
                    None => shortest_path_masked(topology, None, source, target, direction)
                        .map(|path| (path, None)),
                },
                _ => None,
            };
            let (mut src, mut dst, mut hop, mut cost) = (vec![], vec![], vec![], vec![]);
            if let Some((path, costs)) = route {
                for (i, pair) in path.windows(2).enumerate() {
                    src.push(pair[0]);
                    dst.push(pair[1]);
                    hop.push(i as i64);
                    // The cost to this edge's end: the settled distance when
                    // weighted, the edge count otherwise.
                    cost.push(match &costs {
                        Some(c) => c[i + 1],
                        None => (i + 1) as f64,
                    });
                }
            }
            vec![
                ids.gather_user(&src),
                ids.gather_user(&dst),
                Arc::new(Int64Array::from(hop)),
                Arc::new(Float64Array::from(cost)),
            ]
        }
        Traversal::RandomWalk {
            steps,
            walks_per_node,
            seed,
        } => {
            let walks = random_walk_masked(topology, None, &known(0), steps, walks_per_node, seed);
            vec![
                Arc::new(Int64Array::from(walks.walk_id)),
                Arc::new(Int64Array::from(walks.step)),
                ids.gather_user(&walks.node),
            ]
        }
    };
    RecordBatch::try_new(schema, columns)
        .map_err(|e| DataFusionError::ArrowError(Box::new(e), None))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(name: &str, args: &[(&str, Literal)]) -> Result<Traversal> {
        let args: Vec<(String, Literal)> = args
            .iter()
            .map(|(n, v)| (n.to_string(), v.clone()))
            .collect();
        resolve(lookup(name).unwrap(), &args)
    }

    fn num(n: &str) -> Literal {
        Literal::Number(n.into())
    }

    fn message(r: Result<Traversal>) -> String {
        crate::user_message(&r.unwrap_err())
    }

    #[test]
    fn defaults_match_python() {
        assert_eq!(
            call("hop", &[]).unwrap(),
            Traversal::Hop {
                n: 1,
                direction: Direction::Out
            }
        );
        assert_eq!(
            call("shortest_path", &[]).unwrap(),
            Traversal::ShortestPath {
                direction: Direction::Out
            }
        );
        assert_eq!(
            call("random_walk", &[("steps", num("4"))]).unwrap(),
            Traversal::RandomWalk {
                steps: 4,
                walks_per_node: 1,
                seed: None
            }
        );
    }

    #[test]
    fn explains_bad_arguments() {
        assert_eq!(
            message(call("hop", &[("n", num("-1"))])),
            "hop(n =>) must be a non-negative integer below 2^32; got -1"
        );
        assert_eq!(
            message(call("hop", &[("n", num("1.5"))])),
            "hop(n =>) must be a non-negative integer below 2^32; got 1.5"
        );
        assert_eq!(
            message(call("hop", &[("direction", Literal::String("up".into()))])),
            "hop(direction => 'up') is not supported; use one of 'out', 'in', 'both'"
        );
        assert_eq!(
            message(call("random_walk", &[])),
            "random_walk() needs 'steps': random_walk(<graph>, start => [<id>, ...], steps => \
             <integer>, walks_per_node => 1, seed => NULL)"
        );
        assert_eq!(
            message(call(
                "random_walk",
                &[("steps", num("2")), ("seed", Literal::String("x".into()))]
            )),
            "random_walk(seed =>) must be a non-negative integer or NULL; got 'x'"
        );
        assert_eq!(
            crate::user_message(&unknown_param(lookup("hop").unwrap(), "m")),
            "hop() has no parameter 'm'; did you mean 'n'? Its parameters are: seeds, n, \
             direction"
        );
    }

    #[test]
    fn signatures_show_defaults() {
        assert_eq!(
            lookup("hop").unwrap().signature(),
            "hop(<graph>, seeds => [<id>, ...], n => 1, direction => 'out')"
        );
        assert_eq!(
            lookup("shortest_path").unwrap().signature(),
            "shortest_path(<graph>, source => <id>, target => <id>, weight => NULL, direction \
             => 'out')"
        );
    }

    #[test]
    fn explain_spells_the_call() {
        let hop = call("hop", &[("n", num("2"))]).unwrap();
        assert_eq!(
            hop.display(&["[0]".into()], None),
            "hop(seeds => [0], n => 2, direction => 'out')"
        );
        let path = call("shortest_path", &[]).unwrap();
        assert_eq!(
            path.display(&["0".into(), "33".into()], Some("weight")),
            "shortest_path(source => 0, target => 33, weight => weight, direction => 'out')"
        );
    }
}
