//! The graph functions in SQL: which of `ursa_core::catalog`'s functions a
//! query can call, how a call's SQL arguments become a catalog [`Call`], and
//! the wording of every message about them.
//!
//! The catalog is the single source of truth for names, parameters, defaults,
//! accepted values and output types, shared with the Python library, so a call
//! means the same thing in both. This module only adds SQL's choices:
//!
//! - **Which functions.** Every per-node function (algorithms and layouts) and
//!   every statistic. The neighbour aggregation (`neighbors_agg`) reads a node
//!   attribute, which has no natural place in a table function over a graph
//!   yet, so SQL leaves it out.
//! - **No `dtype`.** SQL has `CAST`; a call never narrows its own column.
//! - **`weight =>`** is an expression over the edge table's columns, planned by
//!   the relation planner as a column of the node's edge plan, not a literal
//!   resolved here.
//! - **Messages**, worded for SQL: `pagerank(damping =>) must be a number; got
//!   'high'`.

use std::fmt;

use datafusion::error::{DataFusionError, Result};
use ursa_core::catalog::{
    self, Arg, Call, Expected, Family, FunctionSpec, ParamError, ParamErrorKind, ParamSpec,
    ParamType, Shape, Value,
};

/// The multi-algorithm table function: `algorithms(g, degree(), c => louvain())`.
pub const ALGORITHMS: &str = "algorithms";

/// The function catalog as a table: `SELECT * FROM ursa_functions()`.
pub const URSA_FUNCTIONS: &str = "ursa_functions";

/// Whether SQL can call `spec` (see the module docs).
fn in_sql(spec: &FunctionSpec) -> bool {
    spec.family != Family::Aggregation
}

/// The catalog function called `name`, if SQL can call it.
pub fn lookup(name: &str) -> Option<&'static FunctionSpec> {
    catalog::lookup(name).filter(|s| in_sql(s))
}

/// Every catalog function SQL can call, in catalog order.
pub fn specs() -> impl Iterator<Item = &'static FunctionSpec> {
    catalog::FUNCTIONS.iter().filter(|s| in_sql(s))
}

/// The names of the graph table functions: the catalog's, then `algorithms`.
pub fn names() -> impl Iterator<Item = &'static str> {
    specs().map(|s| s.name).chain([ALGORITHMS])
}

/// Whether `spec` returns one row per node (an algorithm or a layout), as
/// opposed to one row for the whole graph (a statistic).
pub fn is_per_node(spec: &FunctionSpec) -> bool {
    spec.shape == Shape::PerNode
}

/// The parameters a SQL call can name, in the catalog's order: all but
/// `dtype`.
pub fn sql_params(spec: &FunctionSpec) -> impl Iterator<Item = &'static ParamSpec> {
    let params: &'static [ParamSpec] = spec.params;
    params.iter().filter(|p| p.ty != ParamType::Dtype)
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
    spec.resolve(args.iter().map(|(n, l)| (n.as_str(), l.to_arg())))
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
        ParamErrorKind::Unknown => unknown_param(spec, p),
        ParamErrorKind::Duplicate => plan(format!("{f}() was given '{p}' more than once")),
        ParamErrorKind::Expected(want) => {
            let want = match want {
                Expected::Number => "a number",
                Expected::FiniteNumber => "a finite number",
                Expected::Count => "a non-negative integer below 2^32",
                Expected::Seed => "a non-negative integer or NULL",
                Expected::NumberOrNull => "a number or NULL",
                Expected::Bool => "a boolean",
            };
            plan(format!("{f}({p} =>) must be {want}; got {}", given()))
        }
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

/// "pagerank() has no parameter 'dampng'; did you mean 'damping'? ...".
pub fn unknown_param(spec: &FunctionSpec, name: &str) -> DataFusionError {
    let names: Vec<&str> = sql_params(spec).map(|p| p.name).collect();
    if names.is_empty() {
        return plan(format!(
            "{}() takes no parameters besides the graph; got '{name}'",
            spec.name
        ));
    }
    let hint = did_you_mean(name, names.iter().copied())
        .map(|s| format!("; did you mean '{s}'?"))
        .unwrap_or_else(|| ".".to_string());
    plan(format!(
        "{}() has no parameter '{name}'{hint} Its parameters are: {}",
        spec.name,
        names.join(", ")
    ))
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

/// The parameters of `spec` at their defaults: `damping => 0.85, ...`.
fn defaults(spec: &FunctionSpec) -> Vec<String> {
    sql_params(spec)
        .map(|p| format!("{} => {}", p.name, sql_value(&p.default)))
        .collect()
}

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
                seed: None
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
        assert_eq!(
            call(
                "louvain",
                &[("seed", num("7")), ("resolution", num("-0.5"))]
            )
            .unwrap(),
            Call::Louvain {
                resolution: -0.5,
                seed: Some(7)
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
        assert_eq!(
            message(call("diameter", &[("approximate", num("1"))])),
            "diameter(approximate =>) must be a boolean; got 1"
        );
        assert_eq!(
            message(call("pagerank", &[("direction", s("in"))])),
            "pagerank(direction => 'in') is not supported; use one of 'out', 'both'"
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

    #[test]
    fn suggests_names() {
        assert_eq!(did_you_mean("pagerenk", names()), Some("pagerank"));
        assert_eq!(did_you_mean("closenes", names()), Some("closeness"));
        assert_eq!(did_you_mean("algorithm", names()), Some("algorithms"));
        assert_eq!(did_you_mean("shortest_paths", names()), None);
        assert!(lookup("neighbors_agg").is_none());
    }

    #[test]
    fn signatures_show_defaults() {
        assert_eq!(
            signature(lookup("pagerank").unwrap()),
            "pagerank(<graph>, damping => 0.85, max_iter => 30, tol => 1e-6, weight => NULL, \
             direction => 'out')"
        );
        assert_eq!(
            nested_signature(lookup("layout_fr").unwrap()),
            "layout_fr(iterations => 300, k => 1.0, gravity => 0.02, seed => NULL)"
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
            "louvain(resolution => 1.0, seed => NULL)"
        );
        assert_eq!(
            display_call(&call("triangle_count", &[]).unwrap(), None),
            "triangle_count()"
        );
    }
}
