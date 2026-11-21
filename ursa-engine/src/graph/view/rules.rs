//! The result → graph rules (design §3): which rule draws a result, and from
//! which of its columns.
//!
//! Today there are two: a property graph named to `.graph` is drawn whole,
//! and rows are drawn as edges, a row an edge, from the columns
//! [`endpoint_columns`] picks. #433 adds the rest (paths, rings, nodes,
//! pattern matches, the `AS …` clause): each is a [`Rule`] and a [`Shape`],
//! chosen in [`shape_of`] and built in `build`.

use datafusion::arrow::datatypes::{DataType, Schema};
use datafusion::error::Result;

use super::plan_err;
use super::style::per_node_algorithm;

/// The rule that drew a result. A result's provenance names it
/// ([`Rule::name`]), for the pane's status line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Rule {
    /// `.graph NAME`: a property graph's vertices and edges.
    Graph,
    /// Rows drawn as edges, one a row (rule 3).
    Edges,
}

impl Rule {
    /// The rule's name in a result's provenance.
    pub fn name(self) -> &'static str {
        match self {
            Rule::Graph => "graph",
            Rule::Edges => "edges",
        }
    }
}

/// What a result's rows are, by the rule that matched: the columns to read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Shape {
    /// Each row is an edge between the ids in columns `src` and `dst`.
    Edges { src: usize, dst: usize },
}

impl Shape {
    pub(crate) fn rule(self) -> Rule {
        match self {
            Shape::Edges { .. } => Rule::Edges,
        }
    }
}

/// The first rule that matches a result of `schema`, or why none does.
pub(crate) fn shape_of(schema: &Schema) -> Result<Shape> {
    let (src, dst) = endpoint_columns(schema)?;
    Ok(Shape::Edges { src, dst })
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
