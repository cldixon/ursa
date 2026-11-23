//! SQL/PGQ's element functions and graph predicates in a `GRAPH_TABLE`'s
//! conditions and `COLUMNS` (#413): `element_id(a)` of a node or an edge,
//! `SAME`, `ALL_DIFFERENT`, `IS [NOT] LABELED`, and `IS [NOT] SOURCE OF` /
//! `IS [NOT] DESTINATION OF`.
//!
//! Each variable of a pattern reads one element table, so each is a plain
//! rewrite once the pattern is lowered (see [`super::pattern::lower`]):
//!
//! - `element_id(a)` of a node is its key, `a.<key>`; of an edge, its row in
//!   the edge table, the number `edges(p)` lists, which only a path search
//!   reads: the edge must be the one edge of a path with a variable;
//! - `SAME(a, b, …)` is `a.<key> = b.<key> AND …`, and `FALSE` for nodes of
//!   two tables;
//! - `ALL_DIFFERENT(a, b, …)` is `<>` between each pair of nodes of one
//!   table (nodes of two tables always differ);
//! - `x IS LABELED l` is `TRUE` or `FALSE`: whether `x`'s table has label `l`;
//! - `a IS SOURCE OF e` is `a.<key> = e.<source column>`, and `FALSE` when
//!   `e`'s table does not start at `a`'s table; `IS DESTINATION OF`, the same
//!   at the other end.
//!
//! The pattern's parser has already rewritten `IS LABELED`, `IS SOURCE OF`
//! and `IS DESTINATION OF`, which SQL cannot parse, into function calls
//! ([`super::pattern::IS_LABELED`] and its neighbours).

use std::ops::ControlFlow;

use datafusion::error::Result;
use datafusion::logical_expr::planner::RelationPlannerContext;
use datafusion::sql::sqlparser::ast::{
    self, visit_expressions_mut, FunctionArg, FunctionArgExpr, FunctionArguments, Value, VisitMut,
};
use datafusion::sql::sqlparser::dialect::GenericDialect;
use datafusion::sql::sqlparser::parser::Parser;

use super::functions::plan;
use super::pattern::{Lowered, Quantifier, VariableKind, IS_DESTINATION, IS_LABELED, IS_SOURCE};
use super::{EdgeTable, PropertyGraph};

/// What a variable of the pattern is, as the rewrites need it.
enum Element<'a> {
    Node {
        /// Its vertex table's element name; `None` for the graph's `nodes`.
        table: Option<&'a str>,
        /// The property name its key is read under.
        key: &'a str,
    },
    Edge(&'a EdgeTable),
}

/// Rewrite the element functions and graph predicates in `node` (a
/// condition or a `COLUMNS` item) for the pattern `lowered` of `graph`.
pub(super) fn rewrite<V: VisitMut>(
    node: &mut V,
    lowered: &Lowered,
    graph: &PropertyGraph,
    context: &dyn RelationPlannerContext,
) -> Result<()> {
    let flow = visit_expressions_mut(node, |expr: &mut ast::Expr| {
        match replacement(expr, lowered, graph, context) {
            Ok(Some(new)) => {
                *expr = new;
                ControlFlow::Continue(())
            }
            Ok(None) => ControlFlow::Continue(()),
            Err(e) => ControlFlow::Break(e),
        }
    });
    match flow {
        ControlFlow::Break(e) => Err(e),
        ControlFlow::Continue(()) => Ok(()),
    }
}

/// What `expr` is rewritten to, if it is one of the functions.
fn replacement(
    expr: &ast::Expr,
    lowered: &Lowered,
    graph: &PropertyGraph,
    context: &dyn RelationPlannerContext,
) -> Result<Option<ast::Expr>> {
    let ast::Expr::Function(f) = expr else {
        return Ok(None);
    };
    let [name] = f.name.0.as_slice() else {
        return Ok(None);
    };
    let Some(name) = name.as_ident() else {
        return Ok(None);
    };
    let function = name.value.to_lowercase();
    if !matches!(
        function.as_str(),
        "element_id" | "same" | "all_different" | IS_LABELED | IS_SOURCE | IS_DESTINATION
    ) {
        return Ok(None);
    }
    let args: Vec<&ast::Expr> = match &f.args {
        FunctionArguments::List(list) => list
            .args
            .iter()
            .filter_map(|a| match a {
                FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) => Some(e),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    };
    let var = |e: &ast::Expr| match e {
        ast::Expr::Identifier(i) => Some(context.normalize_ident(i.clone())),
        _ => None,
    };
    let element = |v: &str| element(v, lowered, graph);
    match function.as_str() {
        "element_id" => {
            // A path's element_id(p) is the path's (see `path_functions`).
            let ([arg], 1) = (args.as_slice(), arg_count(f)) else {
                return Ok(None);
            };
            let Some(v) = var(arg) else {
                return Ok(None);
            };
            match element(&v) {
                Some(Element::Node { key, .. }) => Ok(Some(column(&v, key))),
                Some(Element::Edge(_)) => edge_element_id(&v, lowered).map(Some),
                None => Ok(None),
            }
        }
        "same" | "all_different" => {
            let shown = function.to_uppercase();
            let nodes = args
                .iter()
                .map(|a| {
                    let v = var(a).ok_or_else(|| {
                        plan(format!(
                            "in GRAPH_TABLE: {shown} takes node variables of the pattern, as {shown}(a, b); got {a}"
                        ))
                    })?;
                    match element(&v) {
                        Some(Element::Node { table, key }) => Ok((v, table, key)),
                        Some(Element::Edge(_)) => Err(plan(format!(
                            "in GRAPH_TABLE: {shown} compares nodes for now; '{v}' is an edge, and an edge has no key to compare"
                        ))),
                        None => Err(not_a_variable(&v, lowered)),
                    }
                })
                .collect::<Result<Vec<_>>>()?;
            if nodes.len() < 2 || arg_count(f) != nodes.len() {
                return Err(plan(format!(
                    "in GRAPH_TABLE: {shown} takes two node variables or more, as {shown}(a, b)"
                )));
            }
            let pairs: Vec<(usize, usize)> = match function.as_str() {
                "same" => (1..nodes.len()).map(|i| (0, i)).collect(),
                _ => (0..nodes.len())
                    .flat_map(|i| (i + 1..nodes.len()).map(move |j| (i, j)))
                    .collect(),
            };
            let mut parts = Vec::new();
            for (i, j) in pairs {
                let ((a, a_table, a_key), (b, b_table, b_key)) = (&nodes[i], &nodes[j]);
                let op = match function.as_str() {
                    "same" => ast::BinaryOperator::Eq,
                    _ => ast::BinaryOperator::NotEq,
                };
                match (a_table == b_table, function.as_str()) {
                    // Nodes of two tables are two nodes.
                    (false, "same") => return Ok(Some(boolean(false))),
                    (false, _) => continue,
                    (true, _) => parts.push(ast::Expr::BinaryOp {
                        left: Box::new(column(a, a_key)),
                        op,
                        right: Box::new(column(b, b_key)),
                    }),
                }
            }
            Ok(Some(
                parts
                    .into_iter()
                    .reduce(|l, r| ast::Expr::BinaryOp {
                        left: Box::new(l),
                        op: ast::BinaryOperator::And,
                        right: Box::new(r),
                    })
                    .map(|e| ast::Expr::Nested(Box::new(e)))
                    .unwrap_or_else(|| boolean(true)),
            ))
        }
        IS_LABELED => {
            let [x, label] = args.as_slice() else {
                return Ok(None);
            };
            let (Some(v), ast::Expr::Value(label)) = (var(x), label) else {
                return Ok(None);
            };
            let Value::SingleQuotedString(label) = &label.value else {
                return Ok(None);
            };
            let known = graph.vertex_tables.iter().any(|t| t.answers_to(label))
                || graph.edge_tables.iter().any(|t| t.answers_to(label));
            if !known {
                return Err(plan(format!(
                    "in GRAPH_TABLE: graph '{}' has no label '{label}' (in {v} IS LABELED {label})",
                    graph.name
                )));
            }
            let labeled = match element(&v) {
                Some(Element::Node { table: Some(t), .. }) => graph
                    .vertex_tables
                    .iter()
                    .any(|vt| vt.name == t && vt.answers_to(label)),
                Some(Element::Node { table: None, .. }) => false,
                Some(Element::Edge(e)) => e.answers_to(label),
                None => return Err(not_a_variable(&v, lowered)),
            };
            Ok(Some(boolean(labeled)))
        }
        IS_SOURCE | IS_DESTINATION => {
            let source = function == IS_SOURCE;
            let shown = if source { "SOURCE" } else { "DESTINATION" };
            let [a, e] = args.as_slice() else {
                return Ok(None);
            };
            let (Some(a), Some(e)) = (var(a), var(e)) else {
                return Ok(None);
            };
            let Some(Element::Node { table, key }) = element(&a) else {
                return Err(plan(format!(
                    "in GRAPH_TABLE: {a} IS {shown} OF {e} takes a node variable before IS; '{a}' is not one"
                )));
            };
            let Some(Element::Edge(edge)) = element(&e) else {
                return Err(plan(format!(
                    "in GRAPH_TABLE: {a} IS {shown} OF {e} takes an edge variable after OF; '{e}' is not one"
                )));
            };
            let Some(relation) = lowered.sources.iter().find(|s| s.var == e) else {
                return Err(several_edges(&e, lowered));
            };
            let end = graph.endpoint_vertex(edge, source).map(|v| v.name.as_str());
            if let (Some(table), Some(end)) = (table, end) {
                if table != end {
                    return Ok(Some(boolean(false)));
                }
            }
            let endpoint = if source {
                &edge.source
            } else {
                &edge.destination
            };
            let name = relation
                .columns
                .as_ref()
                .and_then(|cs| cs.iter().find(|(c, _)| c == endpoint).map(|(_, n)| n))
                .unwrap_or(endpoint);
            Ok(Some(ast::Expr::Nested(Box::new(ast::Expr::BinaryOp {
                left: Box::new(column(&a, key)),
                op: ast::BinaryOperator::Eq,
                right: Box::new(column(&e, name)),
            }))))
        }
        _ => Ok(None),
    }
}

/// How many arguments `f` was called with, of any kind.
fn arg_count(f: &ast::Function) -> usize {
    match &f.args {
        FunctionArguments::List(list) => list.args.len(),
        _ => 0,
    }
}

/// The variable `var` of the pattern, if it is a named node or edge.
fn element<'a>(var: &str, lowered: &'a Lowered, graph: &'a PropertyGraph) -> Option<Element<'a>> {
    let variable = lowered.variables.iter().find(|v| v.var == var)?;
    match &variable.kind {
        VariableKind::Node { label, keys } => Some(Element::Node {
            table: label.as_deref(),
            key: keys.first()?.as_str(),
        }),
        VariableKind::Edge { label } => graph
            .edge_tables
            .iter()
            .find(|e| &e.name == label)
            .map(Element::Edge),
        VariableKind::Path { .. } => None,
    }
}

/// `element_id(e)` of the edge variable `e`: its row in the edge table, the
/// first of `edges(p)` when `e` is the one edge of path `p`.
fn edge_element_id(e: &str, lowered: &Lowered) -> Result<ast::Expr> {
    let segment = lowered.segments.iter().find(|s| s.edge == e);
    match segment {
        Some(s) if s.quantifier == Quantifier::ONCE => match &s.path {
            Some(p) => parse(&format!("array_element(edges({}), 1)", quoted(p))),
            None => Err(plan(format!(
                "in GRAPH_TABLE: element_id({e}) of an edge in a searched path needs the path's variable: write p = … and element_id({e}), or edges(p)"
            ))),
        },
        Some(_) => Err(several_edges(e, lowered)),
        None => Err(plan(format!(
            "in GRAPH_TABLE: element_id({e}) of an edge is its row in the edge table, which only a path search reads for now; name the path, as p = (a)-[{e}]->(b), or return edges(p)"
        ))),
    }
}

/// The refusal of an edge variable that stands for several edges.
fn several_edges(e: &str, lowered: &Lowered) -> datafusion::error::DataFusionError {
    let quantified = lowered
        .segments
        .iter()
        .any(|s| s.edge == e && s.quantifier != Quantifier::ONCE);
    plan(match quantified {
        true => format!(
            "in GRAPH_TABLE: edge '{e}' is quantified, so it stands for several edges; use edges(p), or an aggregate once group variables are supported"
        ),
        false => format!(
            "in GRAPH_TABLE: edge '{e}' is the edge of a searched path (one with a path variable, a selector or a path mode), so it is not read as a row yet; use edges(p)"
        ),
    })
}

fn not_a_variable(var: &str, lowered: &Lowered) -> datafusion::error::DataFusionError {
    let vars: Vec<&str> = lowered
        .variables
        .iter()
        .filter(|v| !matches!(v.kind, VariableKind::Path { .. }))
        .map(|v| v.var.as_str())
        .collect();
    plan(format!(
        "in GRAPH_TABLE: '{var}' is not a node or edge variable of the pattern; its variables are {}",
        vars.join(", ")
    ))
}

/// `"var"."name"`.
fn column(var: &str, name: &str) -> ast::Expr {
    ast::Expr::CompoundIdentifier(vec![
        ast::Ident::with_quote('"', var),
        ast::Ident::with_quote('"', name),
    ])
}

fn boolean(b: bool) -> ast::Expr {
    ast::Expr::value(Value::Boolean(b))
}

fn quoted(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

fn parse(text: &str) -> Result<ast::Expr> {
    Ok(Parser::new(&GenericDialect {})
        .try_with_sql(text)?
        .parse_expr()?)
}
