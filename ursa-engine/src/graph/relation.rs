//! Graph functions in `FROM`: `pagerank(g, direction => 'both')`.
//!
//! [`GraphFunctions`] is a DataFusion `RelationPlanner`. It sees the call's
//! syntax, so the first argument can be a bare graph name and the others
//! named literals. It plans the call as a [`GraphAlgorithmNode`] over the
//! graph's edge table.

use std::sync::Arc;

use datafusion::catalog::default_table_source::provider_as_source;
use datafusion::catalog::CatalogProviderList;
use datafusion::common::{Column, TableReference};
use datafusion::error::{DataFusionError, Result};
use datafusion::logical_expr::planner::{
    PlannedRelation, RelationPlanner, RelationPlannerContext, RelationPlanning,
};
use datafusion::logical_expr::TableSource;
use datafusion::logical_expr::{Expr, Extension, LogicalPlan, LogicalPlanBuilder};
use datafusion::sql::sqlparser::ast::{
    self, FunctionArg, FunctionArgExpr, FunctionArgOperator, TableFactor, TableFunctionArgs,
    UnaryOperator, Value,
};
use futures::FutureExt;

use super::functions::{self, did_you_mean, plan, FunctionSpec, Literal};
use super::node::GraphAlgorithmNode;
use super::{GraphDef, GraphRegistry};

/// A call's named arguments, names normalized, in the order written.
type NamedArgs = Vec<(String, Literal)>;

/// Plans graph function calls in `FROM`.
#[derive(Debug)]
pub struct GraphFunctions {
    registry: Arc<GraphRegistry>,
    /// The session's catalogs, where edge tables are looked up.
    catalogs: Arc<dyn CatalogProviderList>,
}

impl GraphFunctions {
    pub fn new(registry: Arc<GraphRegistry>, catalogs: Arc<dyn CatalogProviderList>) -> Self {
        Self { registry, catalogs }
    }
}

impl RelationPlanner for GraphFunctions {
    fn plan_relation(
        &self,
        relation: TableFactor,
        context: &mut dyn RelationPlannerContext,
    ) -> Result<RelationPlanning> {
        let TableFactor::Table {
            name,
            args: Some(args),
            ..
        } = &relation
        else {
            return Ok(RelationPlanning::Original(Box::new(relation)));
        };
        let [part] = name.0.as_slice() else {
            return Ok(RelationPlanning::Original(Box::new(relation)));
        };
        let Some(ident) = part.as_ident() else {
            return Ok(RelationPlanning::Original(Box::new(relation)));
        };
        let function_name = context.normalize_ident(ident.clone());
        let Some(spec) = functions::lookup(&function_name) else {
            // Not ours, unless its first argument names a graph: then it is a
            // misspelled graph function.
            if self.first_argument_is_a_graph(args, context) {
                let hint = did_you_mean(&function_name, functions::names())
                    .map(|s| format!("; did you mean '{s}'?"))
                    .unwrap_or_else(|| {
                        format!(
                            ". The graph functions are: {}",
                            functions::names().collect::<Vec<_>>().join(", ")
                        )
                    });
                return Err(plan(format!(
                    "there is no graph function '{function_name}'{hint}"
                )));
            }
            return Ok(RelationPlanning::Original(Box::new(relation)));
        };
        let TableFactor::Table {
            args: Some(args),
            alias,
            with_hints,
            version,
            with_ordinality,
            partitions,
            json_path,
            sample,
            index_hints,
            ..
        } = relation
        else {
            unreachable!("matched above")
        };
        if !with_hints.is_empty()
            || version.is_some()
            || with_ordinality
            || !partitions.is_empty()
            || json_path.is_some()
            || sample.is_some()
            || !index_hints.is_empty()
            || args.settings.is_some()
        {
            return Err(plan(format!(
                "{}() takes no table modifiers; write {}",
                spec.name,
                spec.signature()
            )));
        }
        let (graph, named) = self.arguments(spec, args, context)?;
        let algorithm = spec.resolve(named)?;
        let edges = self.edge_plan(&graph, context)?;
        let source = Expr::Column(Column::new(
            Some(graph.edges.table.clone()),
            graph.edges.source.clone(),
        ));
        let destination = Expr::Column(Column::new(
            Some(graph.edges.table.clone()),
            graph.edges.destination.clone(),
        ));
        let node = GraphAlgorithmNode::try_new(
            graph.name.clone(),
            Arc::new(edges),
            source,
            destination,
            vec![algorithm],
        )?;
        let plan = LogicalPlan::Extension(Extension {
            node: Arc::new(node),
        });
        // Unaliased, the columns are qualified by the function's name, as a
        // table's are by the table's, so `JOIN degree(g) USING (id)` can be
        // followed by another join on `id`.
        let plan = match alias {
            Some(_) => plan,
            None => LogicalPlanBuilder::from(plan).alias(spec.name)?.build()?,
        };
        Ok(RelationPlanning::Planned(Box::new(PlannedRelation::new(
            plan, alias,
        ))))
    }
}

impl GraphFunctions {
    fn first_argument_is_a_graph(
        &self,
        args: &TableFunctionArgs,
        context: &dyn RelationPlannerContext,
    ) -> bool {
        match args.args.first() {
            Some(FunctionArg::Unnamed(FunctionArgExpr::Expr(ast::Expr::Identifier(ident)))) => self
                .registry
                .get(&context.normalize_ident(ident.clone()))
                .is_some(),
            _ => false,
        }
    }

    /// The graph and the named arguments of a call to `spec`.
    fn arguments(
        &self,
        spec: &FunctionSpec,
        args: TableFunctionArgs,
        context: &dyn RelationPlannerContext,
    ) -> Result<(Arc<GraphDef>, NamedArgs)> {
        let mut args = args.args.into_iter();
        let missing = || {
            plan(format!(
                "{}() needs a graph as its first argument: {}",
                spec.name,
                spec.signature()
            ))
        };
        let graph = match args.next() {
            None => return Err(missing()),
            Some(FunctionArg::Unnamed(FunctionArgExpr::Expr(ast::Expr::Identifier(ident)))) => {
                self.graph(&context.normalize_ident(ident))?
            }
            Some(FunctionArg::Unnamed(other)) => {
                return Err(plan(format!(
                    "the first argument of {}() must be the name of a graph; got {other}{}",
                    spec.name,
                    quoted_name_hint(&other),
                )))
            }
            Some(_) => return Err(missing()),
        };
        let mut named = Vec::new();
        for arg in args {
            match arg {
                FunctionArg::Named {
                    name,
                    arg,
                    operator: FunctionArgOperator::RightArrow,
                } => {
                    let name = context.normalize_ident(name);
                    let value = literal(&arg).ok_or_else(|| {
                        plan(format!(
                            "{}({name} =>) must be a literal (a number, a string or NULL); got \
                             {arg}",
                            spec.name
                        ))
                    })?;
                    named.push((name, value));
                }
                other => {
                    return Err(plan(format!(
                        "{}() takes its parameters by name, as name => value; got {other}. \
                         Write {}",
                        spec.name,
                        spec.signature()
                    )))
                }
            }
        }
        Ok((graph, named))
    }

    /// The graph called `name`.
    fn graph(&self, name: &str) -> Result<Arc<GraphDef>> {
        if let Some(graph) = self.registry.get(name) {
            return Ok(graph);
        }
        let names = self.registry.names();
        let hint = match did_you_mean(name, names.iter().map(String::as_str)) {
            Some(s) => format!("; did you mean '{s}'?"),
            None if names.is_empty() => {
                "; no graphs are defined (define one with CREATE PROPERTY GRAPH)".to_string()
            }
            None => format!(". The graphs are: {}", names.join(", ")),
        };
        Err(plan(format!("graph '{name}' does not exist{hint}")))
    }
}

/// "pagerank('g')" names a graph by a string; say how to write it.
fn quoted_name_hint(arg: &FunctionArgExpr) -> String {
    match arg {
        FunctionArgExpr::Expr(ast::Expr::Value(v)) => match &v.value {
            Value::SingleQuotedString(s) => format!(" (write the name without quotes: {s})"),
            _ => String::new(),
        },
        _ => String::new(),
    }
}

/// The literal `arg` is, if it is one: a number (with an optional sign), a
/// single-quoted string, or `NULL`.
fn literal(arg: &FunctionArgExpr) -> Option<Literal> {
    let FunctionArgExpr::Expr(expr) = arg else {
        return None;
    };
    fn number(expr: &ast::Expr) -> Option<String> {
        match expr {
            ast::Expr::Value(v) => match &v.value {
                Value::Number(n, false) => Some(n.to_string()),
                _ => None,
            },
            ast::Expr::UnaryOp {
                op: UnaryOperator::Minus,
                expr,
            } => number(expr).map(|n| match n.strip_prefix('-') {
                Some(positive) => positive.to_string(),
                None => format!("-{n}"),
            }),
            ast::Expr::UnaryOp {
                op: UnaryOperator::Plus,
                expr,
            } => number(expr),
            ast::Expr::Nested(inner) => number(inner),
            _ => None,
        }
    }
    if let Some(n) = number(expr) {
        return Some(Literal::Number(n));
    }
    match expr {
        ast::Expr::Value(v) => match &v.value {
            Value::SingleQuotedString(s) => Some(Literal::String(s.clone())),
            Value::Null => Some(Literal::Null),
            _ => None,
        },
        _ => None,
    }
}

impl GraphFunctions {
    /// The table `table`, as a source to plan a scan of.
    ///
    /// DataFusion's SQL planner resolves the tables a statement names before
    /// planning it, and its context provider knows only those; a graph's edge
    /// table is not named in the statement. So it is looked up in the
    /// session's catalogs, where a table or view is held in memory and the
    /// lookup completes at once. A table the catalog would have to open
    /// first (a file path the statement does not name) is reported instead.
    fn table_source(
        &self,
        table: &TableReference,
        context: &dyn RelationPlannerContext,
    ) -> Result<Arc<dyn TableSource>> {
        let provider = context.context_provider();
        if let Ok(source) = provider.get_table_source(table.clone()) {
            return Ok(source);
        }
        let options = provider.options();
        let resolved = table.clone().resolve(
            &options.catalog.default_catalog,
            &options.catalog.default_schema,
        );
        let not_found = || plan(format!("table '{table}' not found"));
        let schema = self
            .catalogs
            .catalog(&resolved.catalog)
            .and_then(|c| c.schema(&resolved.schema))
            .ok_or_else(not_found)?;
        match schema.table(&resolved.table).now_or_never() {
            Some(Ok(Some(table))) => Ok(provider_as_source(table)),
            Some(Ok(None)) => Err(not_found()),
            Some(Err(e)) => Err(e),
            None => Err(plan(format!(
                "'{table}' must be a table or view in the session (create a view over it)"
            ))),
        }
    }

    /// The graph's edge table, projected to its source and destination
    /// columns.
    fn edge_plan(
        &self,
        graph: &GraphDef,
        context: &dyn RelationPlannerContext,
    ) -> Result<LogicalPlan> {
        let table = graph.edges.table.clone();
        // A file path the graph opened when it was created.
        if let Some(file) = self.registry.file_table(&graph.name, &table) {
            return edge_projection(graph, provider_as_source(file));
        }
        let source = self.table_source(&table, context).map_err(|e| {
            plan(format!(
                "graph '{}': its edge table cannot be read: {}",
                graph.name,
                crate::user_message(&e)
            ))
        })?;
        edge_projection(graph, source)
    }
}

/// `<graph>.edges` (the DDL's plan of the edge table), projected to the endpoints.
fn edge_projection(graph: &GraphDef, source: Arc<dyn TableSource>) -> Result<LogicalPlan> {
    let table = graph.edges.table.clone();
    let column = |name: &str| Expr::Column(Column::new(Some(table.clone()), name));
    LogicalPlanBuilder::from(super::edges_plan_from(graph, source)?)
        .project(vec![
            column(&graph.edges.source),
            column(&graph.edges.destination),
        ])
        .map_err(|e| {
            DataFusionError::Plan(format!(
                "graph '{}': the edge table '{table}' has no column for its edges' source \
                 '{}' and destination '{}': {}",
                graph.name,
                graph.edges.source,
                graph.edges.destination,
                crate::user_message(&e)
            ))
        })?
        .build()
}
