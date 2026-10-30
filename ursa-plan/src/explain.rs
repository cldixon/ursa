//! `explain()` text (#193): a frame's plan tree, the schema it collects to, and
//! the DataFusion plans a collect runs. The format is for reading, not parsing.

use std::fmt::Write as _;

use arrow::datatypes::SchemaRef;
use datafusion::logical_expr::LogicalPlan;
use serde_json::Value;

use crate::plan::{EdgeInput, Expr, Plan, SelectItem};

/// Append `plan`'s steps, its last step first, one per line at `depth`; the
/// frames a step reads besides its input (a join's right side, a graph op's
/// edges) are indented under it. A chain is walked in a loop, so a frame of
/// many steps costs a line each.
pub fn tree(plan: &Plan, depth: usize, out: &mut String) {
    let pad = "  ".repeat(depth);
    let mut next = Some(plan);
    while let Some(plan) = next {
        let _ = writeln!(out, "{pad}{}", line(plan));
        match plan {
            Plan::Join { right, .. } => {
                let _ = writeln!(out, "{pad}  right:");
                tree(right, depth + 2, out);
            }
            Plan::WithColumns { columns, .. } => {
                for c in columns {
                    if let Expr::Graph(verb) = &c.expr {
                        if let Some(edges) = &verb.edges {
                            let _ = writeln!(out, "{pad}  {} edges:", c.name);
                            edge_tree(edges, depth + 2, out);
                        }
                    }
                }
            }
            Plan::Hop { edges, .. }
            | Plan::ShortestPath { edges, .. }
            | Plan::RandomWalk { edges, .. }
            | Plan::Describe { edges, .. } => {
                let _ = writeln!(out, "{pad}  edges:");
                edge_tree(edges, depth + 2, out);
            }
            _ => {}
        }
        next = plan.input();
    }
}

fn edge_tree(edges: &EdgeInput, depth: usize, out: &mut String) {
    tree(&edges.plan, depth, out);
}

/// One step, as the frame API writes it.
fn line(plan: &Plan) -> String {
    let invalid = if plan.invalid() {
        " [invalid arguments]"
    } else {
        ""
    };
    let body = match plan {
        Plan::ScanEdges {
            path,
            src,
            dst,
            on_null,
            ..
        } => format!(
            "scan_edges({}, src={src:?}, dst={dst:?}, on_null={on_null:?})",
            path.as_ref()
                .map(|p| format!("{p:?}"))
                .unwrap_or_else(|| "<path>".into())
        ),
        Plan::ScanNodes { path, id, .. } => format!(
            "scan_nodes({}, id={id:?})",
            path.as_ref()
                .map(|p| format!("{p:?}"))
                .unwrap_or_else(|| "<path>".into())
        ),
        Plan::FromArrow { src, dst, id, .. } => match (src, dst, id) {
            (Some(s), Some(d), _) => format!("from_arrow(src={s:?}, dst={d:?})"),
            (_, _, Some(i)) => format!("from_arrow(id={i:?})"),
            _ => "from_arrow()".into(),
        },
        Plan::Filter { predicate, .. } => format!("filter({})", expr(predicate)),
        Plan::Select { columns, .. } => {
            let items: Vec<String> = columns
                .iter()
                .map(|c| match c {
                    SelectItem::Name(n) => format!("{n:?}"),
                    SelectItem::Expr(v) => value(v),
                })
                .collect();
            format!("select({})", items.join(", "))
        }
        Plan::WithColumns { columns, .. } => {
            let items: Vec<String> = columns
                .iter()
                .map(|c| format!("{}={}", c.name, expr(&c.expr)))
                .collect();
            format!("with_columns({})", items.join(", "))
        }
        Plan::Rename { mapping, .. } => {
            let items: Vec<String> = mapping
                .iter()
                .map(|(o, n)| format!("{o:?}: {n:?}"))
                .collect();
            format!("rename({{{}}})", items.join(", "))
        }
        Plan::Sort { by, .. } => {
            let items: Vec<String> = by
                .iter()
                .map(|k| {
                    let key = match (&k.column, &k.expr) {
                        (Some(c), _) => format!("{c:?}"),
                        (None, Some(v)) => value(v),
                        _ => "?".into(),
                    };
                    if k.descending {
                        format!("{key} desc")
                    } else {
                        key
                    }
                })
                .collect();
            format!("sort({})", items.join(", "))
        }
        Plan::Head { n, .. } => format!("head({})", n.0),
        Plan::Sample { n, seed, .. } => match seed {
            Some(s) if !s.0.is_null() => format!("sample({}, seed={})", n.0, s.0),
            _ => format!("sample({})", n.0),
        },
        Plan::Distinct { .. } => "distinct()".into(),
        Plan::Join { on, how, .. } => format!("join(on={on:?}, how={how:?})"),
        Plan::GroupByAgg { keys, aggs, .. } => {
            let items: Vec<String> = aggs
                .iter()
                .map(|a| format!("{}=col({:?}).{}()", a.output, a.column, a.func))
                .collect();
            format!("group_by({keys:?}).agg({})", items.join(", "))
        }
        Plan::Nodes { .. } => "nodes()".into(),
        Plan::Reverse { .. } => "reverse()".into(),
        Plan::Hop { n, direction, .. } => format!("hop(n={}, direction={direction:?})", n.0),
        Plan::ShortestPath {
            weight, direction, ..
        } => match weight {
            Some(w) => format!(
                "shortest_path(weight={}, direction={direction:?})",
                value(w)
            ),
            None => format!("shortest_path(direction={direction:?})"),
        },
        Plan::RandomWalk {
            steps,
            walks_per_node,
            seed,
            ..
        } => format!(
            "random_walk(steps={}, walks_per_node={}{})",
            steps.0,
            walks_per_node.0,
            seed.as_ref()
                .filter(|s| !s.0.is_null())
                .map(|s| format!(", seed={}", s.0))
                .unwrap_or_default()
        ),
        Plan::Describe {
            full, direction, ..
        } => format!("describe(full={full}, direction={direction:?})"),
        Plan::Invalid { .. } => "<invalid>".into(),
    };
    format!("{body}{invalid}")
}

/// An expression of a step, as the dialect writes it.
fn expr(e: &Expr) -> String {
    match e {
        Expr::Scalar(v) => value(v),
        Expr::Graph(verb) => {
            // The edges are shown under the step, not as a parameter.
            let mut params_of = (**verb).clone();
            params_of.edges = None;
            let v = serde_json::to_value(&params_of).unwrap_or(Value::Null);
            let mut params = vec![];
            if let Value::Object(m) = v {
                for (k, p) in m {
                    if matches!(k.as_str(), "verb" | "edges") || p.is_null() {
                        continue;
                    }
                    params.push(format!("{k}={}", value_or_json(&p)));
                }
            }
            format!("{}({})", verb.verb, params.join(", "))
        }
    }
}

fn value_or_json(v: &Value) -> String {
    if v.get("kind").is_some() {
        value(v)
    } else {
        v.to_string()
    }
}

/// An expression's JSON (`crate::expr`'s grammar) as the dialect writes it.
fn value(v: &Value) -> String {
    let s = |k: &str| v.get(k).and_then(Value::as_str).unwrap_or("");
    let sub = |k: &str| value(v.get(k).unwrap_or(&Value::Null));
    match s("kind") {
        "col" => format!("col({:?})", s("name")),
        "lit" => match v.get("type") {
            Some(t) => format!("lit({}: {})", v.get("value").unwrap_or(&Value::Null), t),
            None => format!("lit({})", v.get("value").unwrap_or(&Value::Null)),
        },
        "src" | "dst" | "id" => format!("{}()", s("kind")),
        "binary" => format!("({} {} {})", sub("left"), s("op"), sub("right")),
        "unary" => format!("{}{}", s("op"), sub("operand")),
        "agg" => format!("{}.{}()", sub("operand"), s("fn")),
        "alias" => format!("{}.alias({:?})", sub("operand"), s("name")),
        "" => v.to_string(),
        other => format!("<{other}>"),
    }
}

/// Append the collected schema.
pub fn schema(schema: &SchemaRef, out: &mut String) {
    out.push_str("\nSchema:\n");
    for f in schema.fields() {
        let null = if f.is_nullable() { "" } else { " not null" };
        let _ = writeln!(out, "  {}: {}{null}", f.name(), f.data_type());
    }
}

/// Append the DataFusion plans a collect runs, each optimized, and with
/// `physical` its physical plan.
pub fn datafusion(plans: &[LogicalPlan], physical: bool, out: &mut String) {
    let ctx = crate::lower::session();
    let state = ctx.state();
    if plans.is_empty() {
        out.push_str("\nDataFusion plans: none (the rows are the source's, as they are)\n");
        return;
    }
    for (i, plan) in plans.iter().enumerate() {
        let _ = writeln!(
            out,
            "\nDataFusion logical plan {} of {} (optimized):",
            i + 1,
            plans.len()
        );
        let optimized = state.optimize(plan);
        let shown = optimized.as_ref().unwrap_or(plan);
        for l in shown.display_indent().to_string().lines() {
            let _ = writeln!(out, "  {l}");
        }
        if physical {
            let _ = writeln!(
                out,
                "DataFusion physical plan {} of {}:",
                i + 1,
                plans.len()
            );
            let made = crate::runtime::block_on(state.create_physical_plan(shown));
            match made {
                Ok(Ok(p)) => {
                    let text = datafusion::physical_plan::displayable(p.as_ref())
                        .indent(true)
                        .to_string();
                    for l in text.lines() {
                        let _ = writeln!(out, "  {l}");
                    }
                }
                Ok(Err(e)) | Err(e) => {
                    let _ = writeln!(out, "  not available ({e})");
                }
            }
        }
    }
}
