//! What a result's columns are: the `ursa.*` Arrow field metadata the graph
//! functions and `GRAPH_TABLE` put on their output (#431).
//!
//! A consumer (the graph view, a notebook, a file's reader) should not guess
//! from names that `id` is a node key and `pagerank` a metric. So every graph
//! function marks its output fields, and DataFusion carries the marks with
//! the column: through a projection of the column, an alias, a filter, a
//! sort, a limit, a join, a subquery and `SELECT *`, and into exported Arrow
//! and Parquet files. An expression that computes a new value
//! (`round(pagerank, 4)`, `id + 1`) has none, which is right: the new column
//! is no longer known to be what its input was. `tests/provenance.rs` in
//! `ursa-sql` holds DataFusion to this, operator by operator.
//!
//! The keys:
//!
//! - [`ROLE`] (`ursa.role`): what the column is, one of the `ROLE_*` values.
//! - [`GRAPH`] (`ursa.graph`): the property graph the column came from.
//! - [`LABEL`] (`ursa.label`): the vertex table a node key belongs to, or the
//!   edge table an edge row or an edge's property belongs to. Left out for a
//!   node of a graph without a vertex table.
//! - [`VAR`] (`ursa.var`): in `GRAPH_TABLE`, the pattern variable the column
//!   reads.
//! - [`PATTERN`] (`ursa.pattern`): on the schema of a `GRAPH_TABLE`, not on a
//!   field: the pattern's edges, as JSON (see [`PatternEdge`]).
//!
//! Nothing here changes a result's values, column names or types.

use std::collections::BTreeMap;
use std::sync::Arc;

use datafusion::arrow::datatypes::{Field, Schema};
use datafusion::common::metadata::FieldMetadata;
use datafusion::common::tree_node::Transformed;
use datafusion::common::{Column, ExprSchema, JoinType};
use datafusion::error::Result;
use datafusion::logical_expr::expr::Alias;
use datafusion::logical_expr::utils::split_conjunction;
use datafusion::logical_expr::{BinaryExpr, Expr, LogicalPlan, LogicalPlanBuilder, Operator};

/// What the column is.
pub const ROLE: &str = "ursa.role";
/// The property graph the column came from.
pub const GRAPH: &str = "ursa.graph";
/// The vertex or edge table the column belongs to (its element name).
pub const LABEL: &str = "ursa.label";
/// The `GRAPH_TABLE` pattern variable the column reads.
pub const VAR: &str = "ursa.var";
/// On a `GRAPH_TABLE`'s schema: the pattern's edges, as JSON.
pub const PATTERN: &str = "ursa.pattern";

/// A node key: an algorithm's `id`, `hop`'s `id`, `random_walk`'s `node`, a
/// node variable's key in `GRAPH_TABLE`.
pub const ROLE_NODE: &str = "node";
/// The node key an edge or a path step starts at.
pub const ROLE_SRC: &str = "src";
/// The node key an edge or a path step ends at.
pub const ROLE_DST: &str = "dst";
/// An edge's row in its edge table, as `edges(p)` numbers them.
pub const ROLE_EDGE: &str = "edge";
/// The node key a `hop` row started from.
pub const ROLE_SEED: &str = "seed";
/// The fewest steps from the seed to the node.
pub const ROLE_DISTANCE: &str = "distance";
/// A row's position in its path or walk.
pub const ROLE_STEP: &str = "step";
/// Which walk a `random_walk` row belongs to.
pub const ROLE_WALK: &str = "walk";
/// A per-node number an algorithm computed (`pagerank`, `degree`).
pub const ROLE_METRIC: &str = "metric";
/// A per-node group an algorithm assigned (`louvain`'s community).
pub const ROLE_CATEGORY: &str = "category";
/// A property of a `GRAPH_TABLE` variable.
pub const ROLE_PROPERTY: &str = "property";
/// `vertices(p)`: a path's node keys, in order.
pub const ROLE_PATH_NODES: &str = "path_nodes";
/// `edges(p)`: a path's edge rows, in order.
pub const ROLE_PATH_EDGES: &str = "path_edges";
/// `element_id(p)`: a path's nodes and edges, alternating.
pub const ROLE_PATH_ELEMENTS: &str = "path_elements";

/// The metadata of one field: its role in `graph`, and the element table
/// `label` it belongs to, if one is known.
pub fn metadata(role: &str, graph: &str, label: Option<&str>) -> BTreeMap<String, String> {
    let mut m = BTreeMap::new();
    m.insert(ROLE.to_string(), role.to_string());
    m.insert(GRAPH.to_string(), graph.to_string());
    if let Some(label) = label {
        m.insert(LABEL.to_string(), label.to_string());
    }
    m
}

/// `field`, marked with `role` (see [`metadata`]).
pub fn field(field: Field, role: &str, graph: &str, label: Option<&str>) -> Field {
    field.with_metadata(metadata(role, graph, label).into_iter().collect())
}

/// `expr` under the same name and qualifier, its field marked with
/// `metadata`. A column stays a column reference (aliased to its own name),
/// so nothing about the plan's shape or the output's names changes.
pub fn marked(expr: Expr, metadata: BTreeMap<String, String>) -> Expr {
    let metadata = FieldMetadata::from(metadata);
    match expr {
        Expr::Alias(Alias {
            expr,
            relation,
            name,
            metadata: prior,
        }) => {
            let merged = FieldMetadata::merge_options(prior.as_ref(), Some(&metadata));
            expr.alias_qualified_with_metadata(relation, name, merged)
        }
        Expr::Column(c) => {
            let (relation, name) = (c.relation.clone(), c.name.clone());
            Expr::Column(c).alias_qualified_with_metadata(relation, name, Some(metadata))
        }
        other => other,
    }
}

/// One edge of a `GRAPH_TABLE` pattern, as [`PATTERN`] lists it.
///
/// `ursa.pattern` is a JSON array with one object per edge of the pattern,
/// in the order written:
///
/// ```json
/// [{"from": "a", "to": "b", "directed": true, "edge": "e", "label": "ties"}]
/// ```
///
/// `from` and `to` are node variables, in the edge's direction (`(a)<-[e]-(b)`
/// has `from` `b`); for an undirected edge (`-[e]-`), `directed` is false and
/// they are in the order written. A variable the pattern does not name is
/// `null`. `label` is the edge table. A quantified edge (`-[e]->{1,3}`,
/// `->*`) also has `"min"` and `"max"` (`null` for no upper bound), and the
/// edge of a path with a variable has `"path"`: such an edge stands for a
/// path between its two nodes, not for one edge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PatternEdge {
    pub from: Option<String>,
    pub to: Option<String>,
    pub directed: bool,
    pub edge: Option<String>,
    pub label: String,
    /// `(min, max)` of a quantified edge.
    pub quantifier: Option<(u32, Option<u32>)>,
    /// The path variable of the edge's path.
    pub path: Option<String>,
}

/// [`PATTERN`]'s value for `edges`.
pub fn pattern_json(edges: &[PatternEdge]) -> String {
    use serde_json::{json, Map, Value};
    let edges: Vec<Value> = edges
        .iter()
        .map(|e| {
            let mut o = Map::new();
            o.insert("from".into(), json!(e.from));
            o.insert("to".into(), json!(e.to));
            o.insert("directed".into(), json!(e.directed));
            o.insert("edge".into(), json!(e.edge));
            o.insert("label".into(), json!(e.label));
            if let Some((min, max)) = e.quantifier {
                o.insert("min".into(), json!(min));
                o.insert("max".into(), json!(max));
            }
            if let Some(path) = &e.path {
                o.insert("path".into(), json!(path));
            }
            Value::Object(o)
        })
        .collect();
    Value::Array(edges).to_string()
}

/// `schema` with `key` set to `value` in its own (not a field's) metadata.
pub fn with_schema_metadata(schema: &Schema, key: &str, value: &str) -> Arc<Schema> {
    let mut metadata = schema.metadata().clone();
    metadata.insert(key.to_string(), value.to_string());
    Arc::new(schema.clone().with_metadata(metadata))
}

/// Carry a node key's metadata across an inner join on it.
///
/// `FROM members JOIN pagerank(karate) USING (id)` returns one `id`, and
/// DataFusion takes it from `members`: a plain table's column, which says
/// nothing. But an inner join on `members.id = pagerank.id` keeps only the
/// rows where the two are equal, so `members.id` is the node key too. This
/// rewrite says so: under every inner join, a plain column that `USING`,
/// `NATURAL` or an `ON a.x = b.y` equates with a node key (a field whose role is [`ROLE_NODE`]) of
/// the same type takes that field's metadata, through a projection of its
/// side that changes nothing else.
///
/// Only an inner join, where every row kept has the two equal; only a
/// column equated as written, not one a cast was needed to compare; and only
/// a column with no role of its own.
pub fn carry_through_joins(plan: LogicalPlan) -> Result<LogicalPlan> {
    // Whether a join below was rewritten: the plans above it then take
    // their schemas from their new inputs.
    let mut carried = false;
    plan.transform_up_with_subqueries(|plan| {
        let LogicalPlan::Join(join) = &plan else {
            return match carried {
                true => plan.recompute_schema().map(Transformed::yes),
                false => Ok(Transformed::no(plan)),
            };
        };
        if join.join_type != JoinType::Inner {
            return match carried {
                true => plan.recompute_schema().map(Transformed::yes),
                false => Ok(Transformed::no(plan)),
            };
        }
        // Per side, the columns to mark and what with.
        let mut marks: [Vec<(Column, FieldMetadata)>; 2] = [Vec::new(), Vec::new()];
        // The pairs of columns the join equates: `USING`'s, and those of
        // `ON a.x = b.y [AND …]`, which is still a filter when this runs.
        let mut pairs: Vec<(&Column, &Column)> = Vec::new();
        for (l, r) in &join.on {
            if let (Expr::Column(l), Expr::Column(r)) = (l, r) {
                pairs.push((l, r));
            }
        }
        for conjunct in join.filter.iter().flat_map(split_conjunction) {
            let Expr::BinaryExpr(BinaryExpr {
                left,
                op: Operator::Eq,
                right,
            }) = conjunct
            else {
                continue;
            };
            let (Expr::Column(x), Expr::Column(y)) = (left.as_ref(), right.as_ref()) else {
                continue;
            };
            let on_left = |c: &Column| join.left.schema().has_column(c);
            let on_right = |c: &Column| join.right.schema().has_column(c);
            if on_left(x) && on_right(y) && !on_right(x) && !on_left(y) {
                pairs.push((x, y));
            } else if on_left(y) && on_right(x) && !on_right(y) && !on_left(x) {
                pairs.push((y, x));
            }
        }
        for (l, r) in pairs {
            let (Ok(lf), Ok(rf)) = (
                join.left.schema().field_from_column(l),
                join.right.schema().field_from_column(r),
            ) else {
                continue;
            };
            if lf.data_type() != rf.data_type() {
                continue;
            }
            let role = |f: &Field| f.metadata().get(ROLE).cloned();
            let ours = |f: &Field| -> BTreeMap<String, String> {
                f.metadata()
                    .iter()
                    .filter(|(k, _)| [ROLE, GRAPH, LABEL].contains(&k.as_str()))
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect()
            };
            match (role(lf).as_deref(), role(rf).as_deref()) {
                (Some(ROLE_NODE), None) => marks[1].push((r.clone(), ours(lf).into())),
                (None, Some(ROLE_NODE)) => marks[0].push((l.clone(), ours(rf).into())),
                _ => {}
            }
        }
        if marks.iter().all(Vec::is_empty) {
            return match carried {
                true => plan.recompute_schema().map(Transformed::yes),
                false => Ok(Transformed::no(plan)),
            };
        }
        carried = true;
        let side = |input: &Arc<LogicalPlan>, marks: &[(Column, FieldMetadata)]| {
            if marks.is_empty() {
                return Ok(input.as_ref().clone());
            }
            let columns = input.schema().iter().map(|(qualifier, field)| {
                let column = Column::new(qualifier.cloned(), field.name());
                let mark = marks.iter().find(|(c, _)| {
                    input.schema().index_of_column(c).ok()
                        == input.schema().index_of_column(&column).ok()
                });
                match mark {
                    Some((_, metadata)) => Expr::Column(column).alias_qualified_with_metadata(
                        qualifier.cloned(),
                        field.name(),
                        Some(metadata.clone()),
                    ),
                    None => Expr::Column(column),
                }
            });
            LogicalPlanBuilder::from(input.as_ref().clone())
                .project(columns)?
                .build()
        };
        let inputs = vec![side(&join.left, &marks[0])?, side(&join.right, &marks[1])?];
        Ok(Transformed::yes(
            plan.with_new_exprs(plan.expressions(), inputs)?,
        ))
    })
    .map(|t| t.data)
}
