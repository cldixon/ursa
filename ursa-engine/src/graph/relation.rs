//! Graph functions in `FROM`: `pagerank(g, direction => 'both')`,
//! `density(g)`, and `algorithms(g, degree(), community => louvain(seed => 1))`.
//!
//! [`GraphFunctions`] is a DataFusion `RelationPlanner`. It sees the call's
//! syntax, so the first argument can be a bare graph name, the others named
//! literals, `weight =>` an expression over the edge table's columns, and the
//! arguments of `algorithms()` nested calls. It plans the call as a
//! [`GraphAlgorithmNode`] over the graph's edge table.

use std::collections::HashSet;
use std::sync::Arc;

use datafusion::arrow::datatypes::DataType;
use datafusion::catalog::default_table_source::provider_as_source;
use datafusion::catalog::CatalogProviderList;
use datafusion::common::{Column, DFSchema, TableReference};
use datafusion::error::{DataFusionError, Result};
use datafusion::logical_expr::planner::{
    PlannedRelation, RelationPlanner, RelationPlannerContext, RelationPlanning,
};
use datafusion::logical_expr::TableSource;
use datafusion::logical_expr::{Expr, ExprSchemable, Extension, LogicalPlan, LogicalPlanBuilder};
use datafusion::sql::sqlparser::ast::{
    self, FunctionArg, FunctionArgExpr, FunctionArgOperator, FunctionArguments, TableFactor,
    TableFunctionArgs, UnaryOperator, Value,
};
use futures::FutureExt;
use ursa_core::catalog::{Call, FunctionSpec};

use super::functions::{
    self, did_you_mean, is_per_node, nested_signature, plan, signature, Literal, ALGORITHMS,
};
use super::node::{
    weight_column, GraphAlgorithmNode, Output, WEIGHTED_DESTINATION, WEIGHTED_SOURCE,
};
use super::{GraphDef, GraphRegistry};

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

/// Which table function a call is.
#[derive(Clone, Copy)]
enum Target {
    /// A catalog function.
    Function(&'static FunctionSpec),
    /// `algorithms(g, ...)`.
    Algorithms,
}

impl Target {
    fn name(&self) -> &'static str {
        match self {
            Target::Function(spec) => spec.name,
            Target::Algorithms => ALGORITHMS,
        }
    }

    /// The call with every parameter at its default, for messages.
    fn signature(&self) -> String {
        match self {
            Target::Function(spec) => signature(spec),
            Target::Algorithms => ALGORITHMS_SIGNATURE.to_string(),
        }
    }
}

const ALGORITHMS_SIGNATURE: &str =
    "algorithms(<graph>, degree(direction => 'both'), community => louvain(seed => 1), ...)";

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
        let target = if function_name == ALGORITHMS {
            Target::Algorithms
        } else if let Some(spec) = functions::lookup(&function_name) {
            Target::Function(spec)
        } else {
            // Not ours, unless its first argument names a graph: then it is a
            // misspelled graph function.
            if self.first_argument_is_a_graph(args, context) {
                return Err(no_such_function(
                    &function_name,
                    "",
                    functions::names().collect(),
                ));
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
                target.name(),
                target.signature()
            )));
        }
        let (graph, rest) = self.graph_argument(target, args, context)?;
        let edges = self.edge_table(&graph, context)?;
        let mut weights = WeightSet::new(edges.schema().as_ref().clone());
        let (outputs, per_node) = match target {
            Target::Function(spec) => {
                let (call, weight) = resolve_call(spec, rest, context, &mut weights, "")?;
                let outputs = spec
                    .outputs
                    .iter()
                    .enumerate()
                    .map(|(field, o)| Output {
                        name: o.name.to_string(),
                        call,
                        field,
                        weight,
                    })
                    .collect();
                (outputs, is_per_node(spec))
            }
            Target::Algorithms => (algorithms(rest, context, &mut weights)?, true),
        };
        let node = node(&graph, edges, weights, outputs, per_node)?;
        let plan = LogicalPlan::Extension(Extension {
            node: Arc::new(node),
        });
        // Unaliased, the columns are qualified by the function's name, as a
        // table's are by the table's, so `JOIN degree(g) USING (id)` can be
        // followed by another join on `id`.
        let plan = match alias {
            Some(_) => plan,
            None => LogicalPlanBuilder::from(plan)
                .alias(target.name())?
                .build()?,
        };
        Ok(RelationPlanning::Planned(Box::new(PlannedRelation::new(
            plan, alias,
        ))))
    }
}

/// "there is no graph function 'pagerenk'; did you mean 'pagerank'?", with
/// `prefix` before it (for a call nested in `algorithms()`).
fn no_such_function(name: &str, prefix: &str, candidates: Vec<&str>) -> DataFusionError {
    let hint = did_you_mean(name, candidates.iter().copied())
        .map(|s| format!("; did you mean '{s}'?"))
        .unwrap_or_else(|| format!(". The graph functions are: {}", candidates.join(", ")));
    plan(format!("{prefix}there is no graph function '{name}'{hint}"))
}

/// The distinct weight expressions of one node, over its edge table.
struct WeightSet {
    /// The edge table's schema, which a weight expression is planned against.
    schema: DFSchema,
    exprs: Vec<Expr>,
    /// Each expression as written, for `EXPLAIN`.
    written: Vec<String>,
}

impl WeightSet {
    fn new(schema: DFSchema) -> Self {
        Self {
            schema,
            exprs: Vec::new(),
            written: Vec::new(),
        }
    }

    /// The index of `weight =>` `arg` of a call to `function`, planned over the
    /// edge table and deduplicated; `None` for `NULL` (unweighted).
    fn add(
        &mut self,
        function: &str,
        arg: ast::Expr,
        context: &mut dyn RelationPlannerContext,
    ) -> Result<Option<usize>> {
        if matches!(&arg, ast::Expr::Value(v) if v.value == Value::Null) {
            return Ok(None);
        }
        let written = arg.to_string();
        let context_error = |e: DataFusionError| {
            plan(format!(
                "{function}(weight => {written}): {}",
                crate::user_message(&e).replace('\n', " ")
            ))
        };
        let expr = context
            .sql_to_expr(arg, &self.schema)
            .map_err(context_error)?;
        let data_type = expr.get_type(&self.schema).map_err(context_error)?;
        if !is_numeric(&data_type) {
            return Err(plan(format!(
                "{function}(weight => {written}): weight expression must be numeric; it \
                 produced a column of type {data_type} (a string or other non-numeric column \
                 cannot be a weight)"
            )));
        }
        if let Some(i) = self.exprs.iter().position(|e| *e == expr) {
            return Ok(Some(i));
        }
        self.exprs.push(expr);
        self.written.push(written);
        Ok(Some(self.exprs.len() - 1))
    }
}

/// Whether a weight expression of type `t` can be cast to `f64` as a number
/// (`ursa-plan`'s rule).
fn is_numeric(t: &DataType) -> bool {
    use DataType::*;
    matches!(
        t,
        Int8 | Int16
            | Int32
            | Int64
            | UInt8
            | UInt16
            | UInt32
            | UInt64
            | Float16
            | Float32
            | Float64
            | Decimal32(_, _)
            | Decimal64(_, _)
            | Decimal128(_, _)
            | Decimal256(_, _)
    )
}

/// The call to `spec` with the named arguments `args`: literals, and a weight
/// expression. `prefix` comes before every message (for a nested call).
fn resolve_call(
    spec: &'static FunctionSpec,
    args: Vec<FunctionArg>,
    context: &mut dyn RelationPlannerContext,
    weights: &mut WeightSet,
    prefix: &str,
) -> Result<(Call, Option<usize>)> {
    let in_context = |e: DataFusionError| match e {
        DataFusionError::Plan(m) => plan(format!("{prefix}{m}")),
        other => other,
    };
    let mut literals = Vec::new();
    let mut weight: Option<Option<usize>> = None;
    for arg in args {
        match arg {
            FunctionArg::Named {
                name,
                arg,
                operator: FunctionArgOperator::RightArrow,
            } => {
                let name = context.normalize_ident(name);
                if name == "weight" && spec.weighted() {
                    if weight.is_some() {
                        return Err(plan(format!(
                            "{prefix}{}() was given 'weight' more than once",
                            spec.name
                        )));
                    }
                    let FunctionArgExpr::Expr(expr) = arg else {
                        return Err(plan(format!(
                            "{prefix}{}(weight =>) must be an expression over the edge \
                             table's columns; got {arg}",
                            spec.name
                        )));
                    };
                    weight = Some(weights.add(spec.name, expr, context).map_err(in_context)?);
                    continue;
                }
                // An unknown name is the better message than its value's form.
                let known = name == "dtype" || functions::sql_params(spec).any(|p| p.name == name);
                if !known {
                    return Err(in_context(functions::unknown_param(spec, &name)));
                }
                let value = literal(&arg).ok_or_else(|| {
                    plan(format!(
                        "{prefix}{}({name} =>) must be a literal (a number, a string, a \
                         boolean or NULL); got {arg}",
                        spec.name
                    ))
                })?;
                literals.push((name, value));
            }
            other => {
                let write = if prefix.is_empty() {
                    signature(spec)
                } else {
                    nested_signature(spec)
                };
                return Err(plan(format!(
                    "{prefix}{}() takes its parameters by name, as name => value; got {other}. \
                     Write {write}",
                    spec.name
                )));
            }
        }
    }
    let call = functions::resolve(spec, &literals).map_err(in_context)?;
    Ok((call, weight.flatten()))
}

/// The outputs of `algorithms(g, ...)`: each argument a nested call to a
/// per-node function, `f(...)` or `name => f(...)`.
fn algorithms(
    args: Vec<FunctionArg>,
    context: &mut dyn RelationPlannerContext,
    weights: &mut WeightSet,
) -> Result<Vec<Output>> {
    let prefix = format!("{ALGORITHMS}(): ");
    let per_node = || {
        functions::specs()
            .filter(|s| is_per_node(s))
            .map(|s| s.name)
    };
    let mut outputs = Vec::new();
    for arg in &args {
        let (key, inner) = match arg {
            FunctionArg::Named {
                name,
                arg,
                operator: FunctionArgOperator::RightArrow,
            } => (Some(context.normalize_ident(name.clone())), arg),
            FunctionArg::Unnamed(arg) => (None, arg),
            other => {
                return Err(plan(format!(
                    "{prefix}each argument after the graph is a call to a per-node graph \
                     function, as degree() or name => louvain(seed => 1); got {other}"
                )))
            }
        };
        let function = match inner {
            FunctionArgExpr::Expr(ast::Expr::Function(f)) => f,
            other => {
                // A bare function name: say how to call it.
                let hint = match other {
                    FunctionArgExpr::Expr(ast::Expr::Identifier(i))
                        if functions::lookup(&context.normalize_ident(i.clone())).is_some() =>
                    {
                        format!(" (write {}())", context.normalize_ident(i.clone()))
                    }
                    _ => String::new(),
                };
                return Err(plan(format!(
                    "{prefix}each argument after the graph is a call to a per-node graph \
                     function, as degree() or name => louvain(seed => 1); got {arg}{hint}"
                )));
            }
        };
        let name = match function.name.0.as_slice() {
            [part] => part.as_ident().map(|i| context.normalize_ident(i.clone())),
            _ => None,
        }
        .unwrap_or_else(|| function.name.to_string());
        let Some(spec) = functions::lookup(&name) else {
            return Err(no_such_function(&name, &prefix, per_node().collect()));
        };
        if !is_per_node(spec) {
            return Err(plan(format!(
                "{prefix}{name}() is a statistic, with one row for the whole graph, not one \
                 per node; call it on its own: SELECT * FROM {name}(<graph>)"
            )));
        }
        let modified = function.uses_odbc_syntax
            || !matches!(function.parameters, FunctionArguments::None)
            || function.filter.is_some()
            || function.null_treatment.is_some()
            || function.over.is_some()
            || !function.within_group.is_empty();
        let nested_args = match &function.args {
            FunctionArguments::List(list)
                if !modified && list.duplicate_treatment.is_none() && list.clauses.is_empty() =>
            {
                list.args.clone()
            }
            FunctionArguments::None if !modified => Vec::new(),
            _ => {
                return Err(plan(format!(
                    "{prefix}{name}() is called with named parameters only; got {function}. \
                     Write {}",
                    nested_signature(spec)
                )))
            }
        };
        // The graph goes once, to algorithms() itself.
        if let Some(FunctionArg::Unnamed(FunctionArgExpr::Expr(ast::Expr::Identifier(_)))) =
            nested_args.first()
        {
            return Err(plan(format!(
                "{prefix}{name}() takes its parameters by name, as name => value; got {}. \
                 The graph is algorithms()'s first argument only: write {}",
                nested_args[0],
                nested_signature(spec)
            )));
        }
        let (call, weight) = resolve_call(spec, nested_args, context, weights, &prefix)?;
        let several = spec.outputs.len() > 1;
        for (field, o) in spec.outputs.iter().enumerate() {
            let column = match (&key, several) {
                (Some(key), true) => format!("{key}_{}", o.name),
                (Some(key), false) => key.clone(),
                (None, _) => o.name.to_string(),
            };
            outputs.push(Output {
                name: column,
                call,
                field,
                weight,
            });
        }
    }
    if outputs.is_empty() {
        return Err(plan(format!(
            "{ALGORITHMS}() needs at least one call after the graph: {ALGORITHMS_SIGNATURE}"
        )));
    }
    let mut seen = HashSet::new();
    for o in &outputs {
        if o.name == "id" {
            return Err(plan(format!(
                "{prefix}no column can be named 'id', which holds the node ids; give \
                 {}() another name with name => {}(...)",
                o.call.name(),
                o.call.name()
            )));
        }
        if !seen.insert(o.name.as_str()) {
            return Err(plan(format!(
                "{prefix}two columns are named '{}'; give each call its own name, as in \
                 in_degree => degree(direction => 'in'), out_degree => degree()",
                o.name
            )));
        }
    }
    Ok(outputs)
}

/// The node for `outputs` over the edge table `edges` of `graph`.
fn node(
    graph: &GraphDef,
    edges: LogicalPlan,
    weights: WeightSet,
    outputs: Vec<Output>,
    per_node: bool,
) -> Result<GraphAlgorithmNode> {
    let table = graph.edges.table.clone();
    let column = |name: &str| Expr::Column(Column::new(Some(table.clone()), name));
    let (source, destination) = (
        column(&graph.edges.source),
        column(&graph.edges.destination),
    );
    let no_endpoints = |e: DataFusionError| {
        DataFusionError::Plan(format!(
            "graph '{}': the edge table '{table}' has no column for its edges' source '{}' and \
             destination '{}': {}",
            graph.name,
            graph.edges.source,
            graph.edges.destination,
            crate::user_message(&e)
        ))
    };
    let input = LogicalPlanBuilder::from(edges.clone())
        .project(vec![source.clone(), destination.clone()])
        .map_err(no_endpoints)?
        .build()?;
    let weighted = if weights.exprs.is_empty() {
        None
    } else {
        let mut exprs = vec![
            source.clone().alias(WEIGHTED_SOURCE),
            destination.clone().alias(WEIGHTED_DESTINATION),
        ];
        exprs.extend(
            weights
                .exprs
                .into_iter()
                .enumerate()
                .map(|(i, e)| e.alias(weight_column(i))),
        );
        Some(Arc::new(
            LogicalPlanBuilder::from(edges).project(exprs)?.build()?,
        ))
    };
    GraphAlgorithmNode::try_new(
        graph.name.clone(),
        Arc::new(input),
        weighted,
        source,
        destination,
        weights.written,
        outputs,
        per_node,
    )
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

    /// The graph a call names first, and the arguments after it.
    fn graph_argument(
        &self,
        target: Target,
        args: TableFunctionArgs,
        context: &dyn RelationPlannerContext,
    ) -> Result<(Arc<GraphDef>, Vec<FunctionArg>)> {
        let name = target.name();
        let mut args = args.args.into_iter();
        let missing = || {
            plan(format!(
                "{name}() needs a graph as its first argument: {}",
                target.signature()
            ))
        };
        let graph = match args.next() {
            None => return Err(missing()),
            Some(FunctionArg::Unnamed(FunctionArgExpr::Expr(ast::Expr::Identifier(ident)))) => {
                self.graph(&context.normalize_ident(ident))?
            }
            Some(FunctionArg::Unnamed(FunctionArgExpr::Expr(ast::Expr::Function(_))))
                if matches!(target, Target::Algorithms) =>
            {
                return Err(missing())
            }
            Some(FunctionArg::Unnamed(other)) => {
                return Err(plan(format!(
                    "the first argument of {name}() must be the name of a graph; got {other}{}",
                    quoted_name_hint(&other),
                )))
            }
            Some(_) => return Err(missing()),
        };
        Ok((graph, args.collect()))
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
/// single-quoted string, a boolean, or `NULL`.
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
            Value::Boolean(b) => Some(Literal::Boolean(*b)),
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

    /// The graph's edge table: `<graph>.edges`, as the DDL plans it.
    fn edge_table(
        &self,
        graph: &GraphDef,
        context: &dyn RelationPlannerContext,
    ) -> Result<LogicalPlan> {
        let table = graph.edges.table.clone();
        // A file path the graph opened when it was created.
        if let Some(file) = self.registry.file_table(&graph.name, &table) {
            return super::edges_plan_from(graph, provider_as_source(file));
        }
        let source = self.table_source(&table, context).map_err(|e| {
            plan(format!(
                "graph '{}': its edge table cannot be read: {}",
                graph.name,
                crate::user_message(&e)
            ))
        })?;
        super::edges_plan_from(graph, source)
    }
}
