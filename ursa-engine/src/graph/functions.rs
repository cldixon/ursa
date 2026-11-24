//! The graph functions in SQL: which of `ursa_core::catalog`'s functions a
//! query can call, how a call's SQL arguments become a catalog [`Call`], and
//! the wording of every message about them.
//!
//! The catalog is the single source of truth for names, parameters, defaults,
//! accepted values and output types, shared with the Python library, so a call
//! means the same thing in both. This module only adds SQL's choices:
//!
//! - **Which functions.** Every algorithm, every statistic, the neighbour
//!   aggregation, and the force-directed layouts `layout_fa2` and `layout_fr`
//!   (#395), which return `(id, x, y)`. The trivial layouts (`layout_random`,
//!   `layout_circle`) are left out ([`in_sql`]), as they are from the Python
//!   library's public API: a call fails as an unknown function would, and no
//!   message or listing names them.
//! - **Node values.** The neighbour aggregation (`neighbors_agg`) reads one
//!   value per node. SQL gives them as a query of `(id, value)` rows,
//!   `values => (SELECT id, age FROM people)` ([`VALUES`], a parameter only
//!   SQL has), or as an expression over the graph's node table, `attribute =>
//!   age` (the catalog's `attribute`). Either is planned as a child of the
//!   graph node, as a traversal's node ids are.
//! - **No `dtype`.** SQL has `CAST`; a call never narrows its own column.
//! - **`weight =>`** is an expression over the edge table's columns, planned by
//!   the relation planner as a column of the node's edge plan, not a literal
//!   resolved here.
//! - **Messages**, worded for SQL: `pagerank(damping =>) must be a number; got
//!   'high'`, and for a number outside the parameter's range (#416, declared
//!   once, in the catalog): `pagerank(damping =>) must be between 0 and 1; got
//!   -1`. An unknown parameter is answered with the one to use (#415): the
//!   closest name, or the name Ursa gives a role others name differently
//!   ([`OTHER_NAMES`]: `hop(n =>)` is `steps`), and never one whose type cannot
//!   take the value given ([`Given`]).
//! - **Direction values** are read in any case (`'BOTH'`); `'undirected'` is
//!   refused with a pointer to `'both'` ([`direction_arg`]).
//!
//! Every role has one name across the graph functions (#415): the nodes to
//! start from are `seeds`, a number of steps `steps`, and so on.
//! [`OTHER_NAMES`] lists, for each name, what other systems call it; the
//! registry test holds every parameter to it.

use std::fmt;

use datafusion::error::{DataFusionError, Result};
use ursa_core::catalog::{
    self, Arg, Call, ColumnType, Expected, Family, FunctionSpec, OutputSpec, ParamError,
    ParamErrorKind, ParamSpec, ParamType, Range, Shape, Value,
};

/// The multi-algorithm table function: `algorithms(g, degree(), c => louvain())`.
pub const ALGORITHMS: &str = "algorithms";

/// The function catalog as a table: `SELECT * FROM ursa_functions()`.
pub const URSA_FUNCTIONS: &str = "ursa_functions";

/// Whether SQL can call `spec` (see the module docs). Everything the engine
/// reads from `ursa_core::catalog` goes through this predicate, so a function
/// it rejects is unknown to SQL: not callable, not suggested, not listed by
/// `ursa_functions()`.
fn in_sql(spec: &FunctionSpec) -> bool {
    spec.family != Family::Layout || SQL_LAYOUTS.contains(&spec.name)
}

/// The layouts SQL offers.
const SQL_LAYOUTS: &[&str] = &["layout_fa2", "layout_fr"];

/// The catalog function called `name`, if SQL can call it.
pub fn lookup(name: &str) -> Option<&'static FunctionSpec> {
    catalog::lookup(name).filter(|s| in_sql(s))
}

/// Every catalog function SQL can call, in catalog order.
pub fn specs() -> impl Iterator<Item = &'static FunctionSpec> {
    catalog::FUNCTIONS.iter().filter(|s| in_sql(s))
}

/// The names of the graph table functions: the catalog's, the traversals,
/// then `algorithms`.
pub fn names() -> impl Iterator<Item = &'static str> {
    specs()
        .map(|s| s.name)
        .chain(super::traversal::TRAVERSALS.iter().map(|t| t.name))
        .chain([ALGORITHMS])
}

/// `describe`'s count of the edges left out of the graph because an endpoint
/// is not a key of its vertex table (#311). SQL's own column: a Python graph
/// has no vertex table, so it would always be 0 there.
pub const N_DROPPED_EDGES: OutputSpec = OutputSpec {
    name: "n_dropped_edges",
    ty: ColumnType::Int64,
    nullable: false,
};

/// The columns a call to `spec` returns in SQL, besides `id`: the catalog's,
/// then for `describe` [`N_DROPPED_EDGES`]. An output's `field` indexes this
/// list; past the catalog's own, the engine computes it (`kernels`).
pub fn outputs(spec: &FunctionSpec) -> Vec<OutputSpec> {
    let mut outputs = spec.outputs.to_vec();
    if spec.name == "describe" {
        outputs.push(N_DROPPED_EDGES);
    }
    outputs
}

/// Whether `spec` returns one row per node (an algorithm), as
/// opposed to one row for the whole graph (a statistic).
pub fn is_per_node(spec: &FunctionSpec) -> bool {
    spec.shape == Shape::PerNode
}

/// `neighbors_agg(values => (SELECT id, value FROM ...))`: the node values as
/// a query. SQL's alternative to the catalog's `attribute`, which in SQL is an
/// expression over the graph's node table.
pub const VALUES: ParamSpec = ParamSpec {
    name: "values",
    ty: ParamType::Attribute,
    default: Value::Null,
    range: Range::ANY,
    doc: "A query of two columns, node ids then values: the value of each node. Give this or \
          attribute.",
};

/// The SQL description of the catalog's `attribute` parameter.
const ATTRIBUTE_DOC: &str =
    "An expression over the graph's node table, the value of each node. Give this or values.";

/// `neighbors_agg`'s `agg` in SQL, where `min` and `max` also take strings,
/// dates and times.
const AGG_DOC: &str = "The aggregation; mean and sum need numbers, and min and max take numbers, \
     strings, dates or times. count and n_unique return integers.";

/// The parameters a SQL call can name: all but `dtype`, in the catalog's
/// order, after [`VALUES`] for a function that reads node values.
pub fn sql_params(spec: &FunctionSpec) -> impl Iterator<Item = &'static ParamSpec> {
    let params: &'static [ParamSpec] = spec.params;
    let values = spec.reads_attribute().then_some(&VALUES);
    values
        .into_iter()
        .chain(params.iter().filter(|p| p.ty != ParamType::Dtype))
}

/// A parameter's one-line description, as SQL reads it.
pub fn sql_doc(p: &ParamSpec) -> &'static str {
    match (p.ty, p.name) {
        (ParamType::Attribute, "attribute") => ATTRIBUTE_DOC,
        (ParamType::Choice(_), "agg") => AGG_DOC,
        _ => p.doc,
    }
}

/// How SQL writes a value of parameter `name`, of type `ty`: `number`,
/// `string`, `query` ...
pub fn sql_type(name: &str, ty: ParamType) -> &'static str {
    match ty {
        ParamType::Float => "number",
        ParamType::OptionalFloat => "number or NULL",
        ParamType::Count => "integer",
        ParamType::OptionalCount => "integer or NULL",
        ParamType::Seed => "integer or NULL",
        ParamType::Bool => "boolean",
        ParamType::Choice(_) | ParamType::Direction(_) | ParamType::Dtype => "string",
        ParamType::Attribute if name == VALUES.name => "query",
        ParamType::Weight | ParamType::Attribute => "expression",
    }
}

/// What an argument is, as written: what "did you mean" checks a suggested
/// parameter can take (#415), so `random_walk(sed => [0])` points to `seeds`,
/// not `random_seed`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Given {
    Integer,
    /// A number that is not an integer.
    Number,
    String,
    Boolean,
    Null,
    /// A list literal, `[0, 1]`.
    List,
    /// A subquery, `(SELECT ...)`.
    Query,
    /// Any other expression.
    Expression,
}

impl Given {
    /// The shape of a literal.
    pub fn of_literal(literal: &Literal) -> Given {
        match literal {
            Literal::Number(n) if n.parse::<i128>().is_ok() => Given::Integer,
            Literal::Number(_) => Given::Number,
            Literal::String(_) => Given::String,
            Literal::Boolean(_) => Given::Boolean,
            Literal::Null => Given::Null,
        }
    }
}

/// Whether a parameter of type `ty`, called `name`, can take an argument
/// shaped `given`.
pub fn takes(name: &str, ty: ParamType, given: Given) -> bool {
    use Given::*;
    match ty {
        ParamType::Float => matches!(given, Integer | Number),
        ParamType::OptionalFloat => matches!(given, Integer | Number | Null),
        ParamType::Count => given == Integer,
        ParamType::OptionalCount | ParamType::Seed => matches!(given, Integer | Null),
        ParamType::Bool => given == Boolean,
        ParamType::Choice(_) | ParamType::Direction(_) | ParamType::Dtype => given == String,
        ParamType::Attribute if name == VALUES.name => given == Query,
        ParamType::Weight => matches!(given, Integer | Number | Null | Expression),
        ParamType::Attribute => !matches!(given, List | Query),
    }
}

/// The names other systems give a graph function's parameter, by the name
/// Ursa gives it (#415): one name per role. "did you mean" answers each with
/// Ursa's name, when the function has that parameter and it can take the value
/// given; none is accepted. The registry test refuses a parameter named after
/// another role's alternative.
pub const OTHER_NAMES: &[(&str, &[&str])] = &[
    (
        "seeds",
        &[
            "start",
            "starts",
            "start_nodes",
            "sources",
            "seed_nodes",
            "roots",
        ],
    ),
    (
        "steps",
        &[
            "n",
            "k",
            "hops",
            "max_hops",
            "depth",
            "max_depth",
            "length",
            "walk_length",
        ],
    ),
    ("random_seed", &["seed", "rng_seed", "random_state"]),
    ("source", &["src", "start_node", "from"]),
    ("target", &["dst", "end_node", "to"]),
    ("walks_per_node", &["walks", "num_walks", "n_walks"]),
    ("damping", &["alpha", "damping_factor"]),
    ("direction", &["dir", "orientation"]),
    ("weight", &["weights", "weight_column"]),
];

/// The name to suggest for the unknown parameter `name` of a function whose
/// parameters are `params` (name, and whether it can take `given`): Ursa's
/// name for the role (see [`OTHER_NAMES`]), else the closest name. Neither
/// when no parameter that can take the value is close.
pub fn suggest<'a>(
    name: &str,
    given: Option<Given>,
    params: &[(&'a str, &dyn Fn(Given) -> bool)],
) -> Option<&'a str> {
    let fits = |takes: &dyn Fn(Given) -> bool| given.is_none_or(takes);
    let fitting: Vec<&'a str> = params
        .iter()
        .filter(|(_, takes)| fits(*takes))
        .map(|(n, _)| *n)
        .collect();
    OTHER_NAMES
        .iter()
        .filter(|(_, others)| others.contains(&name))
        .find_map(|(ours, _)| fitting.iter().find(|n| *n == ours).copied())
        .or_else(|| did_you_mean(name, fitting.iter().copied()))
}

/// The text after "has no parameter 'x'": the suggestion, then the list.
pub fn unknown_param_text(
    function: &str,
    name: &str,
    given: Option<Given>,
    params: &[(&str, &dyn Fn(Given) -> bool)],
) -> String {
    let names: Vec<&str> = params.iter().map(|(n, _)| *n).collect();
    let hint = suggest(name, given, params)
        .map(|s| format!("; did you mean '{s}'?"))
        .unwrap_or_else(|| ".".to_string());
    format!(
        "{function}() has no parameter '{name}'{hint} Its parameters are: {}",
        names.join(", ")
    )
}

/// A direction as written, read in any case (#415): `'BOTH'` is `'both'`.
/// `'undirected'`, the word of NetworkX, igraph and DuckPGQ, is refused with
/// a pointer to `'both'`, Ursa's one name for it.
pub fn direction_arg(function: &str, param: &str, text: &str) -> Result<String> {
    let lower = text.to_lowercase();
    if lower == "undirected" {
        return Err(plan(format!(
            "{function}({param} => '{}') is not supported; use 'both', which reads every edge \
             both ways (the undirected view)",
            text.replace('\'', "''")
        )));
    }
    Ok(lower)
}

/// A literal argument as written: a (possibly signed) number, a string, a
/// boolean or `NULL`.
#[derive(Debug, Clone, PartialEq)]
pub enum Literal {
    /// The number's text, sign included, so it parses exactly as Python
    /// parses the same text.
    Number(String),
    String(String),
    Boolean(bool),
    Null,
}

impl fmt::Display for Literal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Literal::Number(n) => write!(f, "{n}"),
            Literal::String(s) => write!(f, "'{}'", s.replace('\'', "''")),
            Literal::Boolean(b) => write!(f, "{b}"),
            Literal::Null => write!(f, "NULL"),
        }
    }
}

impl Literal {
    /// The catalog's view of the literal: an integer if the text is one, else
    /// a float if it is one.
    fn to_arg(&self) -> Arg {
        match self {
            Literal::Number(n) => n
                .parse::<i128>()
                .map(Arg::Int)
                .or_else(|_| n.parse::<f64>().map(Arg::Float))
                .unwrap_or(Arg::Other),
            Literal::String(s) => Arg::Str(s.clone()),
            Literal::Boolean(b) => Arg::Bool(*b),
            Literal::Null => Arg::Null,
        }
    }
}

/// Resolve the literal arguments of a call to `spec` (names already
/// normalized; `weight` already taken out by the caller) to a [`Call`].
pub fn resolve(spec: &'static FunctionSpec, args: &[(String, Literal)]) -> Result<Call> {
    if spec.narrows() {
        if let Some((name, _)) = args.iter().find(|(n, _)| n == "dtype") {
            let column = spec.outputs.first().map_or(spec.name, |o| o.name);
            return Err(plan(format!(
                "{}() has no parameter '{name}' in SQL; cast its column instead, for example \
                 CAST({column} AS REAL)",
                spec.name
            )));
        }
    }
    // A direction in any case (#415).
    let mut read = Vec::with_capacity(args.len());
    for (name, literal) in args {
        let literal = match (spec.param(name).map(|p| p.ty), literal) {
            (Some(ParamType::Direction(_)), Literal::String(text)) => {
                Literal::String(direction_arg(spec.name, name, text)?)
            }
            (_, other) => other.clone(),
        };
        read.push((name.as_str(), literal));
    }
    spec.resolve(read.iter().map(|(n, l)| (*n, l.to_arg())))
        .map(|resolved| resolved.call)
        .map_err(|e| param_error(spec, &e, args))
}

/// A catalog [`ParamError`] in SQL's words.
fn param_error(spec: &FunctionSpec, e: &ParamError, args: &[(String, Literal)]) -> DataFusionError {
    let (f, p) = (spec.name, e.param.as_str());
    let given = || {
        args.iter()
            .find(|(n, _)| n == p)
            .map_or(Literal::Null, |(_, l)| l.clone())
    };
    match e.kind {
        ParamErrorKind::Unknown => {
            let given = args
                .iter()
                .find(|(n, _)| n == p)
                .map(|(_, l)| Given::of_literal(l));
            unknown_param(spec, p, given)
        }
        ParamErrorKind::Duplicate => plan(format!("{f}() was given '{p}' more than once")),
        ParamErrorKind::Expected(want) => plan(format!(
            "{f}({p} =>) must be {}; got {}",
            expected_text(want),
            given()
        )),
        ParamErrorKind::OutOfRange(range) => plan(format!(
            "{f}({p} =>) must be {}; got {}",
            range_text(&range).unwrap_or_else(|| "a number".into()),
            given()
        )),
        ParamErrorKind::NotOneOf(options) => {
            let options: Vec<String> = options.iter().map(|o| format!("'{o}'")).collect();
            plan(format!(
                "{f}({p} => {}) is not supported; use one of {}",
                given(),
                options.join(", ")
            ))
        }
        ParamErrorKind::Expression => plan(format!(
            "{f}({p} =>) takes an expression over the edge table's columns; got {}",
            given()
        )),
    }
}

/// What a parameter expected, in SQL's words.
pub(crate) fn expected_text(want: Expected) -> &'static str {
    match want {
        Expected::Number => "a number",
        Expected::FiniteNumber => "a finite number",
        Expected::Count => "a non-negative integer below 2^32",
        Expected::CountOrNull => "a non-negative integer below 2^32, or NULL",
        Expected::Seed => "a non-negative integer or NULL",
        Expected::NumberOrNull => "a number or NULL",
        Expected::Bool => "a boolean",
    }
}

/// The numbers a parameter accepts, in words: `between 0 and 1`, `above 0 and
/// at most 1`, `at least 1`; `None` for a parameter with no limit. The text of
/// the `range` field of `ursa_functions()` and of the error for a value
/// outside it (#416).
pub fn range_text(range: &Range) -> Option<String> {
    range.describe()
}

/// "pagerank() has no parameter 'dampng'; did you mean 'damping'? ...":
/// `given` is the argument's shape, if known, so the suggestion can take it.
pub fn unknown_param(spec: &FunctionSpec, name: &str, given: Option<Given>) -> DataFusionError {
    let params: Vec<&ParamSpec> = sql_params(spec).collect();
    if params.is_empty() {
        return plan(format!(
            "{}() takes no parameters besides the graph; got '{name}'",
            spec.name
        ));
    }
    // `connected_components` says which way edges count with `mode` (#415).
    if name == "direction" {
        if let Some(mode) = spec.param("mode") {
            let choices: Vec<String> = mode.ty.choices().iter().map(|c| format!("'{c}'")).collect();
            return plan(format!(
                "{}() has no parameter 'direction'; use mode => {} ('weak' ignores edge \
                 direction, 'strong' needs a path each way)",
                spec.name,
                choices.join(" or ")
            ));
        }
    }
    let takers: Vec<Box<dyn Fn(Given) -> bool>> = params
        .iter()
        .map(|p| {
            let (n, ty) = (p.name, p.ty);
            Box::new(move |g| takes(n, ty, g)) as Box<dyn Fn(Given) -> bool>
        })
        .collect();
    let entries: Vec<(&str, &dyn Fn(Given) -> bool)> = params
        .iter()
        .zip(&takers)
        .map(|(p, t)| (p.name, t.as_ref()))
        .collect();
    plan(unknown_param_text(spec.name, name, given, &entries))
}

/// A catalog value as SQL writes it: `NULL`, `'out'`, `30`, `0.85`, `1e-6`.
pub fn sql_value(value: &Value) -> String {
    match value {
        Value::Null => "NULL".to_string(),
        Value::Bool(b) => b.to_string(),
        Value::UInt(u) => u.to_string(),
        Value::Float(f) => format!("{f:?}"),
        Value::Str(s) => format!("'{s}'"),
    }
}

/// The parameters of `spec` at their defaults: `damping => 0.85, ...`. Node
/// values are shown as the query they take, and `attribute`, its
/// alternative, is left out.
fn defaults(spec: &FunctionSpec) -> Vec<String> {
    sql_params(spec)
        .filter_map(|p| match (p.ty, p.name) {
            (ParamType::Attribute, "values") => Some(format!("{} => {VALUES_QUERY}", p.name)),
            (ParamType::Attribute, _) => None,
            _ => Some(format!("{} => {}", p.name, sql_value(&p.default))),
        })
        .collect()
}

/// The form of a `values =>` argument, for messages.
pub const VALUES_QUERY: &str = "(SELECT id, value FROM ...)";

/// `pagerank(<graph>, damping => 0.85, ...)`: the call with every parameter at
/// its default, for messages.
pub fn signature(spec: &FunctionSpec) -> String {
    let mut parts = vec!["<graph>".to_string()];
    parts.extend(defaults(spec));
    format!("{}({})", spec.name, parts.join(", "))
}

/// `pagerank(damping => 0.85, ...)`: a call as written inside `algorithms()`.
pub fn nested_signature(spec: &FunctionSpec) -> String {
    format!("{}({})", spec.name, defaults(spec).join(", "))
}

/// The call as SQL would spell it, every literal parameter shown, and the
/// weight when there is one: for `EXPLAIN`.
pub fn display_call(call: &Call, weight: Option<&str>) -> String {
    let values = call.values();
    let parts: Vec<String> = sql_params(call.spec())
        .filter_map(|p| {
            if p.ty == ParamType::Weight {
                return weight.map(|w| format!("{} => {w}", p.name));
            }
            values
                .iter()
                .find(|(q, _)| q.name == p.name)
                .map(|(_, v)| format!("{} => {}", p.name, sql_value(v)))
        })
        .collect();
    format!("{}({})", call.name(), parts.join(", "))
}

/// A plan error.
pub(crate) fn plan(message: String) -> DataFusionError {
    DataFusionError::Plan(message)
}

/// The closest of `candidates` to `name`, if one is close enough to be a typo
/// (the graph DDL's rule, so every "did you mean" agrees).
pub fn did_you_mean<'a>(
    name: &str,
    candidates: impl IntoIterator<Item = &'a str>,
) -> Option<&'a str> {
    super::closest_name(name, candidates)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ursa_core::catalog::Value;
    use ursa_core::Direction;

    fn call(name: &str, args: &[(&str, Literal)]) -> Result<Call> {
        let args: Vec<(String, Literal)> = args
            .iter()
            .map(|(n, v)| (n.to_string(), v.clone()))
            .collect();
        resolve(lookup(name).unwrap(), &args)
    }

    fn num(n: &str) -> Literal {
        Literal::Number(n.into())
    }

    fn s(v: &str) -> Literal {
        Literal::String(v.into())
    }

    fn message(r: Result<Call>) -> String {
        crate::user_message(&r.unwrap_err())
    }

    #[test]
    fn defaults_match_python() {
        assert_eq!(
            call("pagerank", &[]).unwrap(),
            Call::PageRank {
                damping: 0.85,
                max_iter: 30,
                tol: 1e-6,
                direction: Direction::Out
            }
        );
        assert_eq!(
            call("degree", &[]).unwrap(),
            Call::Degree {
                direction: Direction::Out
            }
        );
        assert_eq!(
            call("connected_components", &[]).unwrap(),
            Call::ConnectedComponents { strong: false }
        );
        assert_eq!(
            call("louvain", &[]).unwrap(),
            Call::Louvain {
                resolution: 1.0,
                seed: None,
                parallel: false
            }
        );
        assert_eq!(call("triangle_count", &[]).unwrap(), Call::TriangleCount);
        assert_eq!(
            call("diameter", &[]).unwrap(),
            Call::Diameter {
                approximate: true,
                direction: Direction::Out
            }
        );
    }

    #[test]
    fn reads_arguments() {
        assert_eq!(
            call(
                "pagerank",
                &[
                    ("damping", num("0.9")),
                    ("max_iter", num("100")),
                    ("tol", num("1e-10")),
                    ("direction", s("both"))
                ]
            )
            .unwrap(),
            Call::PageRank {
                damping: 0.9,
                max_iter: 100,
                tol: 1e-10,
                direction: Direction::Both
            }
        );
        // An integer is a number, as Python's float parameters take an int.
        assert!(call("pagerank", &[("damping", num("1"))]).is_ok());
        // The ends of a range that are values (#416), and a NULL where the
        // type takes one.
        for (function, param, value) in [
            ("pagerank", "damping", num("0")),
            ("pagerank", "max_iter", num("1")),
            ("pagerank", "tol", num("0")),
            ("betweenness", "sample", num("1")),
            ("betweenness", "sample", num("1e-9")),
            ("betweenness", "sample", Literal::Null),
            ("avg_path_length", "sample", num("1.0")),
            ("layout_fa2", "gravity", num("0")),
            ("layout_fa2", "theta", num("0")),
            ("layout_fa2", "theta", Literal::Null),
            ("layout_fa2", "iterations", num("0")),
            ("layout_fr", "iterations", num("0")),
        ] {
            assert!(
                call(function, &[(param, value)]).is_ok(),
                "{function} {param}"
            );
        }
        assert_eq!(
            call("louvain", &[("seed", num("7")), ("resolution", num("0.5"))]).unwrap(),
            Call::Louvain {
                resolution: 0.5,
                seed: Some(7),
                parallel: false
            }
        );
        assert_eq!(
            call("louvain", &[("seed", Literal::Null)]).unwrap(),
            call("louvain", &[]).unwrap()
        );
        assert_eq!(
            call("connected_components", &[("mode", s("strong"))]).unwrap(),
            Call::ConnectedComponents { strong: true }
        );
        assert_eq!(
            call("diameter", &[("approximate", Literal::Boolean(false))]).unwrap(),
            Call::Diameter {
                approximate: false,
                direction: Direction::Out
            }
        );
        assert_eq!(
            call("betweenness", &[("sample", Literal::Null)]).unwrap(),
            call("betweenness", &[]).unwrap()
        );
    }

    #[test]
    fn explains_bad_arguments() {
        assert_eq!(
            message(call("pagerank", &[("dampng", num("0.9"))])),
            "pagerank() has no parameter 'dampng'; did you mean 'damping'? Its parameters are: \
             damping, max_iter, tol, weight, direction"
        );
        assert_eq!(
            message(call("pagerank", &[("color", num("1"))])),
            "pagerank() has no parameter 'color'. Its parameters are: damping, max_iter, tol, \
             weight, direction"
        );
        assert_eq!(
            message(call("triangle_count", &[("direction", s("both"))])),
            "triangle_count() takes no parameters besides the graph; got 'direction'"
        );
        assert_eq!(
            message(call(
                "pagerank",
                &[("damping", num("0.9")), ("damping", num("0.8"))]
            )),
            "pagerank() was given 'damping' more than once"
        );
        assert_eq!(
            message(call("pagerank", &[("damping", s("high"))])),
            "pagerank(damping =>) must be a number; got 'high'"
        );
        assert_eq!(
            message(call("pagerank", &[("damping", num("1e999"))])),
            "pagerank(damping =>) must be a finite number; got 1e999"
        );
        assert_eq!(
            message(call("pagerank", &[("max_iter", num("-1"))])),
            "pagerank(max_iter =>) must be a non-negative integer below 2^32; got -1"
        );
        assert_eq!(
            message(call("pagerank", &[("max_iter", num("2.5"))])),
            "pagerank(max_iter =>) must be a non-negative integer below 2^32; got 2.5"
        );
        assert_eq!(
            message(call("pagerank", &[("max_iter", num("4294967296"))])),
            "pagerank(max_iter =>) must be a non-negative integer below 2^32; got 4294967296"
        );
        assert_eq!(
            message(call("pagerank", &[("max_iter", Literal::Null)])),
            "pagerank(max_iter =>) must be a non-negative integer below 2^32; got NULL"
        );
        assert_eq!(
            message(call("louvain", &[("seed", num("-3"))])),
            "louvain(seed =>) must be a non-negative integer or NULL; got -3"
        );
        assert_eq!(
            message(call("betweenness", &[("sample", s("half"))])),
            "betweenness(sample =>) must be a number or NULL; got 'half'"
        );
        // A number outside the parameter's range (#416): the range, and the
        // value as written.
        for (function, param, value, range) in [
            ("pagerank", "damping", "-1", "between 0 and 1"),
            ("pagerank", "damping", "1.5", "between 0 and 1"),
            ("pagerank", "max_iter", "0", "at least 1"),
            ("pagerank", "tol", "-1", "at least 0"),
            ("label_propagation", "max_iter", "0", "at least 1"),
            ("betweenness", "sample", "2", "above 0 and at most 1"),
            ("betweenness", "sample", "-1", "above 0 and at most 1"),
            ("betweenness", "sample", "0", "above 0 and at most 1"),
            ("avg_path_length", "sample", "5", "above 0 and at most 1"),
            ("louvain", "resolution", "-1", "above 0"),
            ("louvain", "resolution", "0.0", "above 0"),
            ("layout_fa2", "k", "0", "above 0"),
            ("layout_fa2", "gravity", "-0.5", "at least 0"),
            ("layout_fa2", "theta", "-1", "at least 0"),
            ("layout_fa2", "jitter_tolerance", "0", "above 0"),
            ("layout_fr", "k", "-2", "above 0"),
            ("layout_fr", "gravity", "-1e-3", "at least 0"),
        ] {
            assert_eq!(
                message(call(function, &[(param, num(value))])),
                format!("{function}({param} =>) must be {range}; got {value}")
            );
        }
        assert_eq!(
            message(call("diameter", &[("approximate", num("1"))])),
            "diameter(approximate =>) must be a boolean; got 1"
        );
        assert_eq!(
            message(call("pagerank", &[("direction", s("up"))])),
            "pagerank(direction => 'up') is not supported; use one of 'out', 'in', 'both'"
        );
        assert_eq!(
            message(call("degree", &[("direction", s("sideways"))])),
            "degree(direction => 'sideways') is not supported; use one of 'out', 'in', 'both'"
        );
        assert_eq!(
            message(call("connected_components", &[("mode", num("1"))])),
            "connected_components(mode => 1) is not supported; use one of 'weak', 'strong'"
        );
        assert_eq!(
            message(call("closeness", &[("dtype", s("f32"))])),
            "closeness() has no parameter 'dtype' in SQL; cast its column instead, for example \
             CAST(closeness AS REAL)"
        );
        assert_eq!(
            message(call("degree", &[("dtype", s("f32"))])),
            "degree() has no parameter 'dtype'. Its parameters are: direction"
        );
    }

    /// #415: a direction in any case; 'undirected' pointed to 'both'; 'in'
    /// for pagerank and betweenness.
    #[test]
    fn directions_in_any_case() {
        for (function, text, want) in [
            ("pagerank", "BOTH", Direction::Both),
            ("pagerank", "In", Direction::In),
            ("betweenness", "in", Direction::In),
            ("degree", "Out", Direction::Out),
        ] {
            let got = call(function, &[("direction", s(text))]).unwrap();
            let direction = got
                .values()
                .into_iter()
                .find(|(p, _)| p.name == "direction")
                .map(|(_, v)| v);
            assert_eq!(
                direction,
                Some(Value::Str(ursa_core::catalog::direction_name(want))),
                "{function} {text}"
            );
        }
        assert_eq!(
            message(call("closeness", &[("direction", s("Undirected"))])),
            "closeness(direction => 'Undirected') is not supported; use 'both', which reads \
             every edge both ways (the undirected view)"
        );
        assert_eq!(
            message(call("degree", &[("direction", s("Sideways"))])),
            "degree(direction => 'Sideways') is not supported; use one of 'out', 'in', 'both'"
        );
    }

    /// #415: "did you mean" names the parameter to use for a role others name
    /// differently, and never one whose type cannot take the value.
    #[test]
    fn suggestions_fit_the_value() {
        assert_eq!(
            message(call("pagerank", &[("alpha", num("0.5"))])),
            "pagerank() has no parameter 'alpha'; did you mean 'damping'? Its parameters are: \
             damping, max_iter, tol, weight, direction"
        );
        // 'dampng' is close to 'damping', but a string is no damping factor.
        assert_eq!(
            message(call("pagerank", &[("dampng", s("high"))])),
            "pagerank() has no parameter 'dampng'. Its parameters are: damping, max_iter, tol, \
             weight, direction"
        );
        assert_eq!(
            message(call("connected_components", &[("direction", s("both"))])),
            "connected_components() has no parameter 'direction'; use mode => 'weak' or \
             'strong' ('weak' ignores edge direction, 'strong' needs a path each way)"
        );
        let spec = lookup("louvain").unwrap();
        assert_eq!(
            crate::user_message(&unknown_param(spec, "sed", Some(Given::Integer))),
            "louvain() has no parameter 'sed'; did you mean 'seed'? Its parameters are: weight, \
             resolution, seed, parallel"
        );
        assert_eq!(
            crate::user_message(&unknown_param(spec, "sed", Some(Given::List))),
            "louvain() has no parameter 'sed'. Its parameters are: weight, resolution, seed, \
             parallel"
        );
    }

    /// #415, item 6: one name per role across the graph functions. A new
    /// function that calls its start nodes `start`, its step count `n` or
    /// `max_hops`, or uses a name for two kinds of value, fails here.
    #[test]
    fn one_name_per_role() {
        use super::super::traversal::{TraversalParamType, TRAVERSALS};
        // What a value of each parameter is, NULL aside.
        let kind = |name: &str, ty: ParamType| sql_type(name, ty).trim_end_matches(" or NULL");
        let mut params: Vec<(&str, &str, &str)> = Vec::new();
        for spec in specs() {
            for p in sql_params(spec) {
                params.push((spec.name, p.name, kind(p.name, p.ty)));
            }
        }
        for spec in TRAVERSALS {
            for p in spec.params {
                let k = match p.ty {
                    TraversalParamType::Nodes => "node ids",
                    TraversalParamType::Node => "node id",
                    TraversalParamType::Param(ty) => kind(p.name, ty),
                };
                params.push((spec.name, p.name, k));
            }
        }
        // `seed` (the algorithms' and layouts' random seed) and
        // `random_walk`'s `random_seed`, renamed apart from `seeds` (#415), are
        // one role under two names until the algorithms' name is decided.
        const SPLIT: &[(&str, &str)] = &[("seed", "random_seed")];
        // `describe(nodes => 'touched')` chooses which nodes to describe;
        // `induced_edges(nodes => [...])` is the node set. Both are the nodes
        // the call is about, given two ways.
        const TWO_KINDS: &[&str] = &["nodes"];
        // One kind of value per name.
        for (f, name, k) in &params {
            for (g, other, l) in &params {
                assert!(
                    name != other || k == l || TWO_KINDS.contains(name),
                    "{f}({name}) takes {k} but {g}({other}) takes {l}"
                );
            }
        }
        for (ours, others) in OTHER_NAMES {
            let kinds: Vec<&str> = params
                .iter()
                .filter(|(_, n, _)| n == ours)
                .map(|(_, _, k)| *k)
                .collect();
            assert!(!kinds.is_empty(), "no function has a parameter '{ours}'");
            // No parameter of the same kind goes by another name of the role.
            for (f, name, k) in &params {
                if SPLIT.contains(&(*name, *ours)) {
                    continue;
                }
                assert!(
                    !(others.contains(name) && kinds.contains(k)),
                    "{f}({name}) is what the other functions call '{ours}'"
                );
            }
        }
        // Each role's alternatives point to one name.
        for (i, (ours, others)) in OTHER_NAMES.iter().enumerate() {
            for (theirs, alternatives) in &OTHER_NAMES[i + 1..] {
                for name in *others {
                    assert!(
                        !alternatives.contains(name),
                        "'{name}' points to both '{ours}' and '{theirs}'"
                    );
                }
            }
        }
    }

    #[test]
    fn suggests_names() {
        assert_eq!(did_you_mean("pagerenk", names()), Some("pagerank"));
        assert_eq!(did_you_mean("closenes", names()), Some("closeness"));
        assert_eq!(did_you_mean("algorithm", names()), Some("algorithms"));
        assert_eq!(
            did_you_mean("shortest_paths", names()),
            Some("shortest_path")
        );
        assert_eq!(did_you_mean("random_walks", names()), Some("random_walk"));
        assert_eq!(did_you_mean("dijkstra", names()), None);
        assert_eq!(did_you_mean("neighbor_agg", names()), Some("neighbors_agg"));
    }

    #[test]
    fn neighbors_agg_takes_values_or_an_attribute() {
        let spec = lookup("neighbors_agg").unwrap();
        let params: Vec<&str> = sql_params(spec).map(|p| p.name).collect();
        assert_eq!(params, ["values", "direction", "attribute", "agg"]);
        assert_eq!(
            signature(spec),
            "neighbors_agg(<graph>, values => (SELECT id, value FROM ...), direction => 'out', \
             agg => 'mean')"
        );
        assert_eq!(
            call(
                "neighbors_agg",
                &[("agg", s("n_unique")), ("direction", s("both"))]
            )
            .unwrap(),
            Call::NeighborAgg {
                direction: Direction::Both,
                agg: ursa_core::algo::AggKind::NUnique
            }
        );
        assert_eq!(
            message(call("neighbors_agg", &[("agg", s("median"))])),
            "neighbors_agg(agg => 'median') is not supported; use one of 'mean', 'sum', 'min', \
             'max', 'count', 'n_unique'"
        );
    }

    #[test]
    fn the_trivial_layouts_are_hidden() {
        for name in ["layout_random", "layout_circle"] {
            assert!(catalog::lookup(name).is_some(), "{name} is in the catalog");
            assert!(lookup(name).is_none(), "{name} is hidden from SQL");
            assert!(names().all(|n| n != name));
            assert!(specs().all(|s| s.name != name));
        }
        for name in ["layout_fa2", "layout_fr"] {
            assert!(lookup(name).is_some(), "{name} is in SQL");
            assert!(names().any(|n| n == name));
        }
        // No suggestion leads to a hidden name.
        for typo in ["layout_randm", "layout_circl"] {
            let suggestion = did_you_mean(typo, names());
            assert!(
                !matches!(suggestion, Some("layout_random" | "layout_circle")),
                "{typo}: {suggestion:?}"
            );
        }
        assert_eq!(did_you_mean("layout_frr", names()), Some("layout_fr"));
    }

    #[test]
    fn signatures_show_defaults() {
        assert_eq!(
            signature(lookup("pagerank").unwrap()),
            "pagerank(<graph>, damping => 0.85, max_iter => 30, tol => 1e-6, weight => NULL, \
             direction => 'out')"
        );
        assert_eq!(
            nested_signature(lookup("louvain").unwrap()),
            "louvain(weight => NULL, resolution => 1.0, seed => NULL, parallel => false)"
        );
    }

    #[test]
    fn explain_spells_the_call() {
        let pagerank = call("pagerank", &[]).unwrap();
        assert_eq!(
            display_call(&pagerank, None),
            "pagerank(damping => 0.85, max_iter => 30, tol => 1e-6, direction => 'out')"
        );
        assert_eq!(
            display_call(&pagerank, Some("w")),
            "pagerank(damping => 0.85, max_iter => 30, tol => 1e-6, weight => w, \
             direction => 'out')"
        );
        assert_eq!(
            display_call(&call("louvain", &[]).unwrap(), None),
            "louvain(resolution => 1.0, seed => NULL, parallel => false)"
        );
        assert_eq!(
            display_call(&call("triangle_count", &[]).unwrap(), None),
            "triangle_count()"
        );
    }
}
