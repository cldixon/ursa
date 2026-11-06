//! Graph functions in `FROM`: `pagerank(g, direction => 'both')`,
//! `density(g)`, `algorithms(g, degree(), community => louvain(seed => 1))`,
//! and the traversals, `hop(g, seeds => [0], n => 2)`.
//!
//! [`GraphFunctions`] is a DataFusion `RelationPlanner`. It sees the call's
//! syntax, so the first argument can be a bare graph name, the others named
//! literals, `weight =>` an expression over the edge table's columns, the
//! arguments of `algorithms()` nested calls, and a traversal's node ids a
//! list, a subquery or one id. It plans an algorithm or statistic as a
//! [`GraphAlgorithmNode`] over the graph's edge table, and a traversal as a
//! [`GraphTraversalNode`] over the same edge table plus one child plan per
//! node-id argument.
//!
//! ## Node values as a child plan
//!
//! `neighbors_agg` reads one value per node, given as `values => (SELECT id,
//! value FROM ...)`, a query of two columns, or as `attribute => <expr>`, an
//! expression over the graph's node table, planned as `SELECT id, <expr> FROM
//! <graph>.nodes`. Either becomes the node's last child (see
//! [`GraphAlgorithmNode`]), planned as a derived table and run when the node
//! runs, as a traversal's node ids are. Its column types are checked here: the
//! ids integers or strings, the values numbers, or strings for `count` and
//! `n_unique`.
//!
//! ## Node ids as child plans
//!
//! A traversal's node ids are not values the planner reads: each node-id
//! argument becomes a plan of one column under the traversal node, run when
//! the node runs. A list literal, `[0, 33]`, is a `VALUES` plan (an empty list
//! an empty relation); a single id, `0`, is a one-row `VALUES`; a subquery,
//! `(SELECT id FROM karate.nodes WHERE club = 'Officer')`, is planned as a
//! derived table, so it is optimized with the rest of the query and reads its
//! files only when the query runs.
//!
//! The values must be integers or strings, and cast to the graph's id type: on
//! a graph with integer ids a string must parse exactly as an integer, and
//! floats (even `2.0`), booleans and dates are refused, though Arrow would
//! cast them (#271). A list's values and a single id are checked and cast
//! here, one value at a time (so `[0, 'a']` is not coerced to one type
//! first), and a subquery's column type is checked here; a subquery's values
//! are cast when the node runs. Either way a value that is refused is named in
//! the message: `hop(seeds =>) must be node ids of graph 'karate', which are
//! integers; got 'a'`.

use std::collections::HashSet;
use std::sync::Arc;

use datafusion::arrow::compute::can_cast_types;
use datafusion::arrow::datatypes::{DataType, Field, Schema};
use datafusion::catalog::default_table_source::provider_as_source;
use datafusion::catalog::CatalogProviderList;
use datafusion::common::{Column, DFSchema, TableReference};
use datafusion::error::{DataFusionError, Result};
use datafusion::logical_expr::planner::{
    PlannedRelation, RelationPlanner, RelationPlannerContext, RelationPlanning,
};
use datafusion::logical_expr::simplify::SimplifyContext;
use datafusion::logical_expr::TableSource;
use datafusion::logical_expr::{
    EmptyRelation, Expr, ExprSchemable, Extension, LogicalPlan, LogicalPlanBuilder,
};
use datafusion::optimizer::simplify_expressions::ExprSimplifier;
use datafusion::sql::sqlparser::ast::{
    self, FunctionArg, FunctionArgExpr, FunctionArgOperator, FunctionArguments, TableFactor,
    TableFunctionArgs, UnaryOperator, Value,
};
use datafusion::sql::sqlparser::dialect::GenericDialect;
use datafusion::sql::sqlparser::parser::Parser;
use futures::FutureExt;
use ursa_core::algo::AggKind;
use ursa_core::catalog::{Call, FunctionSpec};

use super::functions::{
    self, did_you_mean, is_per_node, nested_signature, plan, signature, Literal, ALGORITHMS,
    VALUES_QUERY,
};
use super::kernels;
use super::node::{
    endpoint_id_type, weight_column, GraphAlgorithmNode, GraphTraversalNode, Output,
    WEIGHTED_DESTINATION, WEIGHTED_SOURCE,
};
use super::traversal::{self, TraversalParam, TraversalParamType, TraversalSpec};
use super::{GraphDef, GraphRegistry, PropertyGraph};

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
    /// A traversal.
    Traversal(&'static TraversalSpec),
}

impl Target {
    fn name(&self) -> &'static str {
        match self {
            Target::Function(spec) => spec.name,
            Target::Algorithms => ALGORITHMS,
            Target::Traversal(spec) => spec.name,
        }
    }

    /// The call with every parameter at its default, for messages.
    fn signature(&self) -> String {
        match self {
            Target::Function(spec) => signature(spec),
            Target::Algorithms => ALGORITHMS_SIGNATURE.to_string(),
            Target::Traversal(spec) => spec.signature(),
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
        if function_name == super::pattern::FUNCTION {
            return self.plan_graph_table(relation, context);
        }
        let target = if function_name == ALGORITHMS {
            Target::Algorithms
        } else if let Some(spec) = functions::lookup(&function_name) {
            Target::Function(spec)
        } else if let Some(spec) = traversal::lookup(&function_name) {
            Target::Traversal(spec)
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
            Target::Function(spec) if spec.reads_attribute() => {
                let nodes = |c: &dyn RelationPlannerContext| {
                    self.table_source(&TableReference::partial(graph.name.as_str(), "nodes"), c)
                };
                let node = values_node(spec, &graph, edges, rest, context, &nodes)?;
                let plan = LogicalPlan::Extension(Extension {
                    node: Arc::new(node),
                });
                return planned(plan, target, alias);
            }
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
            Target::Traversal(spec) => {
                let node = traversal_node(spec, &graph, edges, rest, context)?;
                let plan = LogicalPlan::Extension(Extension {
                    node: Arc::new(node),
                });
                return planned(plan, target, alias);
            }
        };
        let node = node(&graph, edges, weights, outputs, per_node)?;
        let plan = LogicalPlan::Extension(Extension {
            node: Arc::new(node),
        });
        planned(plan, target, alias)
    }
}

/// The planned relation for the graph node `plan`.
fn planned(
    plan: LogicalPlan,
    target: Target,
    alias: Option<ast::TableAlias>,
) -> Result<RelationPlanning> {
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

/// Looks up a graph's node table, `<graph>.nodes`, as a table source.
type NodeTable<'a> = dyn Fn(&dyn RelationPlannerContext) -> Result<Arc<dyn TableSource>> + 'a;

/// The node for a call to `spec`, a function that reads node values
/// (`neighbors_agg`), over the edge table `edges` of `graph`, with the
/// arguments after the graph (see the module docs).
fn values_node(
    spec: &'static FunctionSpec,
    graph: &GraphDef,
    edges: LogicalPlan,
    args: Vec<FunctionArg>,
    context: &mut dyn RelationPlannerContext,
    nodes: &NodeTable,
) -> Result<GraphAlgorithmNode> {
    let f = spec.name;
    // `values =>` and `attribute =>` are plans; the rest are literals.
    let mut values: Option<ast::Expr> = None;
    let mut attribute: Option<ast::Expr> = None;
    let mut literals = Vec::new();
    for arg in args {
        if let FunctionArg::Named {
            name,
            arg: FunctionArgExpr::Expr(expr),
            operator: FunctionArgOperator::RightArrow,
        } = &arg
        {
            let name = context.normalize_ident(name.clone());
            let slot = match name.as_str() {
                "values" => &mut values,
                "attribute" => &mut attribute,
                _ => {
                    literals.push(arg);
                    continue;
                }
            };
            if slot.replace(expr.clone()).is_some() {
                return Err(plan(format!("{f}() was given '{name}' more than once")));
            }
            continue;
        }
        literals.push(arg);
    }
    let mut weights = WeightSet::new(edges.schema().as_ref().clone());
    let (call, _) = resolve_call(spec, literals, context, &mut weights, "")?;
    let takes_strings = matches!(
        call,
        Call::NeighborAgg {
            agg: AggKind::Count | AggKind::NUnique,
            ..
        }
    );
    let edges = edge_inputs(graph, edges, weights)?;
    let id_type = endpoint_id_type(&graph.name, &edges.input, &edges.source, &edges.destination)?;
    let in_param = |param: &'static str| {
        move |e: DataFusionError| {
            plan(format!(
                "{f}({param} =>): {}",
                crate::user_message(&e).replace('\n', " ")
            ))
        }
    };
    let (param, values) = match (values, attribute) {
        (Some(_), Some(_)) => {
            return Err(plan(format!(
                "{f}() takes 'values' or 'attribute', not both: values => {VALUES_QUERY} is a \
                 query of node ids and values, attribute => <expr> an expression over the \
                 graph's node table"
            )))
        }
        (None, None) => {
            return Err(plan(format!(
                "{f}() needs the value of each node: values => {VALUES_QUERY}, a query of node \
                 ids and values, or attribute => <expr>, an expression over the graph's node \
                 table. Write {}",
                signature(spec)
            )))
        }
        (Some(ast::Expr::Subquery(query)), None) => {
            let values = context
                .plan(TableFactor::Derived {
                    lateral: false,
                    subquery: query,
                    alias: None,
                    sample: None,
                })
                .map_err(in_param("values"))?;
            ("values", values)
        }
        (Some(other), None) => {
            return Err(plan(format!(
                "{f}(values =>) must be a query of two columns, node ids then values, as \
                 values => {VALUES_QUERY}; got {other}"
            )))
        }
        // `SELECT id, <expr> FROM <graph>.nodes`, planned here: the node
        // table is not among the tables the statement names, which are all a
        // subquery could read.
        (None, Some(expr)) => {
            let table = TableReference::partial(graph.name.as_str(), "nodes");
            let scan = LogicalPlanBuilder::scan(table.clone(), nodes(&*context)?, None)?.build()?;
            let value = context
                .sql_to_expr(expr, scan.schema())
                .map_err(in_param("attribute"))?;
            let values = LogicalPlanBuilder::from(scan)
                .project(vec![
                    Expr::Column(Column::new(Some(table), "id")),
                    value.alias("value"),
                ])
                .and_then(LogicalPlanBuilder::build)
                .map_err(in_param("attribute"))?;
            ("attribute", values)
        }
    };
    let schema = values.schema();
    let columns = schema.fields().len();
    if columns != 2 {
        return Err(plan(format!(
            "{f}({param} =>) must be a query of two columns, node ids then values; it returns \
             {columns} columns"
        )));
    }
    let (ids, value) = (schema.field(0).data_type(), schema.field(1).data_type());
    if !traversal::is_node_id_type(ids, &id_type) {
        let kind = match id_type {
            DataType::Int64 => "integers",
            _ => "strings",
        };
        return Err(plan(format!(
            "{f}({param} =>): the first column must be node ids of graph '{}', which are \
             {kind}; got a column of type {ids}",
            graph.name
        )));
    }
    if kernels::is_string(value) {
        if !takes_strings {
            let agg = call
                .values()
                .into_iter()
                .find(|(p, _)| p.name == "agg")
                .map(|(_, v)| functions::sql_value(&v))
                .unwrap_or_default();
            return Err(plan(format!(
                "{f}(agg => {agg}) needs numbers, and the values are strings; only 'count' and \
                 'n_unique' take strings"
            )));
        }
    } else if !(value == &DataType::Null || can_cast_types(value, &DataType::Float64)) {
        return Err(plan(format!(
            "{f}({param} =>): the values must be numbers, or strings for 'count' and \
             'n_unique'; got a column of type {value}"
        )));
    }
    let outputs = spec
        .outputs
        .iter()
        .enumerate()
        .map(|(field, o)| Output {
            name: o.name.to_string(),
            call,
            field,
            weight: None,
        })
        .collect();
    GraphAlgorithmNode::try_new(
        graph.name.clone(),
        Arc::new(edges.input),
        None,
        Some(Arc::new(values)),
        edges.source,
        edges.destination,
        edges.weights,
        outputs,
        true,
    )
}

/// The node for a call to the traversal `spec` over the edge table `edges` of
/// `graph`, with the arguments after the graph.
fn traversal_node(
    spec: &'static TraversalSpec,
    graph: &GraphDef,
    edges: LogicalPlan,
    args: Vec<FunctionArg>,
    context: &mut dyn RelationPlannerContext,
) -> Result<GraphTraversalNode> {
    let f = spec.name;
    let mut weights = WeightSet::new(edges.schema().as_ref().clone());
    let mut literals = Vec::new();
    let node_params: Vec<&TraversalParam> = spec.node_params().collect();
    let mut nodes: Vec<Option<ast::Expr>> = vec![None; node_params.len()];
    let mut seen = HashSet::new();
    for arg in args {
        let FunctionArg::Named {
            name,
            arg,
            operator: FunctionArgOperator::RightArrow,
        } = arg
        else {
            return Err(plan(format!(
                "{f}() takes its parameters by name, as name => value; got {arg}. Write {}",
                spec.signature()
            )));
        };
        let name = context.normalize_ident(name);
        let Some(param) = spec.param(&name) else {
            return Err(traversal::unknown_param(spec, &name));
        };
        if !seen.insert(name.clone()) {
            return Err(plan(format!("{f}() was given '{name}' more than once")));
        }
        let FunctionArgExpr::Expr(expr) = arg else {
            return Err(plan(format!(
                "{f}({name} =>) must be a value or an expression; got {arg}"
            )));
        };
        if param.takes_nodes() {
            let i = node_params
                .iter()
                .position(|p| p.name == param.name)
                .expect("a node parameter");
            nodes[i] = Some(expr);
        } else if param.is_weight() {
            weights.add(f, expr, context)?;
        } else {
            let arg = FunctionArgExpr::Expr(expr);
            let value = literal(&arg).ok_or_else(|| {
                plan(format!(
                    "{f}({name} =>) must be a literal (a number, a string or NULL); got {arg}"
                ))
            })?;
            literals.push((name, value));
        }
    }
    let nodes = node_params
        .iter()
        .zip(nodes)
        .map(|(p, n)| n.ok_or_else(|| traversal::missing_param(spec, p)))
        .collect::<Result<Vec<_>>>()?;
    let edges = edge_inputs(graph, edges, weights)?;
    // The node ids are checked against the graph's id type as they are
    // planned, so the id type comes first.
    let id_type = endpoint_id_type(&graph.name, &edges.input, &edges.source, &edges.destination)?;
    let ids = NodeIds {
        spec,
        graph: &graph.name,
        id_type: &id_type,
    };
    let mut inputs = Vec::with_capacity(nodes.len());
    let mut written = Vec::with_capacity(nodes.len());
    for (p, expr) in node_params.iter().zip(nodes) {
        written.push(expr.to_string());
        inputs.push(Arc::new(ids.input(p, expr, context)?));
    }
    let traversal = traversal::resolve(spec, &literals)?;
    GraphTraversalNode::try_new(
        graph.name.clone(),
        traversal,
        Arc::new(edges.input),
        edges.weighted,
        edges.weights.into_iter().next(),
        inputs,
        written,
        edges.source,
        edges.destination,
    )
}

/// Plans a traversal's node-id arguments, for the graph `graph` whose node
/// ids have type `id_type`.
struct NodeIds<'a> {
    spec: &'static TraversalSpec,
    graph: &'a str,
    id_type: &'a DataType,
}

impl NodeIds<'_> {
    /// The plan of the node-id argument `expr` to `param`: one column (see
    /// the module docs).
    fn input(
        &self,
        param: &TraversalParam,
        expr: ast::Expr,
        context: &mut dyn RelationPlannerContext,
    ) -> Result<LogicalPlan> {
        let (f, p) = (self.spec.name, param.name);
        match expr {
            ast::Expr::Subquery(query) => {
                let subquery = context
                    .plan(TableFactor::Derived {
                        lateral: false,
                        subquery: query,
                        alias: None,
                        sample: None,
                    })
                    .map_err(|e| {
                        plan(format!(
                            "{f}({p} =>): {}",
                            crate::user_message(&e).replace('\n', " ")
                        ))
                    })?;
                let columns = subquery.schema().fields().len();
                if columns != 1 {
                    return Err(plan(format!(
                        "{f}({p} =>) must be a query of one column of node ids; it returns \
                         {columns} columns"
                    )));
                }
                // Its values are cast when the node runs; its type must be
                // one node ids may have.
                let ty = subquery.schema().field(0).data_type();
                if !traversal::is_node_id_type(ty, self.id_type) {
                    return Err(self.not_ids(param, &format!("a query of type {ty}")));
                }
                Ok(subquery)
            }
            ast::Expr::Array(array) => {
                if param.ty == TraversalParamType::Node {
                    return Err(plan(format!(
                        "{f}({p} =>) takes one node id, not a list; got {array}"
                    )));
                }
                self.values(param, array.elem, context)
            }
            one => self.values(param, vec![one], context),
        }
    }

    /// A `VALUES` plan of the node ids `elems`, one row each, each value
    /// cast to the graph's id type here, one at a time, so a list of several
    /// types (`[0, 'a']`) is checked value by value rather than coerced to
    /// one type; an empty relation for none.
    fn values(
        &self,
        param: &TraversalParam,
        elems: Vec<ast::Expr>,
        context: &mut dyn RelationPlannerContext,
    ) -> Result<LogicalPlan> {
        let (f, p) = (self.spec.name, param.name);
        if elems.is_empty() {
            let schema = Schema::new(vec![Field::new("id", DataType::Null, true)]);
            return Ok(LogicalPlan::EmptyRelation(EmptyRelation {
                produce_one_row: false,
                schema: Arc::new(DFSchema::try_from(schema)?),
            }));
        }
        let empty = Arc::new(DFSchema::empty());
        let simplifier = ExprSimplifier::new(
            SimplifyContext::builder()
                .with_schema(Arc::clone(&empty))
                .build(),
        );
        let rows = elems
            .into_iter()
            .map(|e| {
                let written = e.to_string();
                let hint = match &e {
                    ast::Expr::Identifier(i) => {
                        format!(
                            " (a string id is quoted: '{}')",
                            i.value.replace('\'', "''")
                        )
                    }
                    _ => String::new(),
                };
                let not_an_id = |err: DataFusionError| {
                    plan(format!(
                        "{f}({p} =>): {written} is not a node id{hint}: {}",
                        crate::user_message(&err).replace('\n', " ")
                    ))
                };
                // Constants are folded, so `-1` and `CAST(...)` are values.
                let expr = context
                    .sql_to_expr(e, &empty)
                    .and_then(|expr| simplifier.simplify(expr))
                    .map_err(not_an_id)?;
                let ty = expr.get_type(&empty).map_err(not_an_id)?;
                if !castable(&ty, self.id_type) {
                    return Err(self.not_ids(param, &format!("{written}, of type {ty}")));
                }
                // A float, a boolean or a date would cast, but is refused
                // (#271): the value alone says what is wrong.
                if !traversal::is_node_id_type(&ty, self.id_type) {
                    return Err(self.not_ids(param, &written));
                }
                let expr = match expr {
                    Expr::Literal(value, metadata) => {
                        // Exactly an integer: Arrow's cast would trim ' 1'.
                        if let (DataType::Int64, Some(Some(text))) =
                            (self.id_type, value.try_as_str())
                        {
                            if !traversal::is_integer_text(text) {
                                return Err(self.not_ids(param, &written));
                            }
                        }
                        let value = value
                            .cast_to(self.id_type)
                            .map_err(|_| self.not_ids(param, &written))?;
                        Expr::Literal(value, metadata)
                    }
                    // Not a constant (random() and the like): cast when the
                    // node runs.
                    other => other.cast_to(self.id_type, &empty)?,
                };
                Ok(vec![expr])
            })
            .collect::<Result<Vec<_>>>()?;
        LogicalPlanBuilder::values(rows).and_then(LogicalPlanBuilder::build)
    }

    /// The node ids given to `param` are not of the graph's id type: `got`
    /// says what they are.
    fn not_ids(&self, param: &TraversalParam, got: &str) -> DataFusionError {
        plan(traversal::not_node_ids(
            self.spec.name,
            param,
            self.graph,
            self.id_type,
            got,
        ))
    }
}

/// Whether values of type `from` can be node ids of type `id_type`: NULL, or
/// a type Arrow casts to it.
fn castable(from: &DataType, id_type: &DataType) -> bool {
    from == &DataType::Null || can_cast_types(from, id_type)
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
            .filter(|s| is_per_node(s) && !s.reads_attribute())
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
        if let Some(t) = traversal::lookup(&name) {
            return Err(plan(format!(
                "{prefix}{name}() is a traversal, which returns rows of its own, not one per \
                 node; call it on its own: SELECT * FROM {}",
                t.signature()
            )));
        }
        let Some(spec) = functions::lookup(&name) else {
            return Err(no_such_function(&name, &prefix, per_node().collect()));
        };
        if !is_per_node(spec) {
            return Err(plan(format!(
                "{prefix}{name}() is a statistic, with one row for the whole graph, not one \
                 per node; call it on its own: SELECT * FROM {name}(<graph>)"
            )));
        }
        if spec.reads_attribute() {
            return Err(plan(format!(
                "{prefix}{name}() reads node values, so it runs on its own: SELECT * FROM {}; \
                 join its result on id",
                signature(spec)
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
    let edges = edge_inputs(graph, edges, weights)?;
    GraphAlgorithmNode::try_new(
        graph.name.clone(),
        Arc::new(edges.input),
        edges.weighted,
        None,
        edges.source,
        edges.destination,
        edges.weights,
        outputs,
        per_node,
    )
}

/// A graph node's edge inputs: the edge table projected to the endpoints, and
/// to the endpoints and weights when there are weights (see `node`).
struct EdgeInputs {
    input: LogicalPlan,
    weighted: Option<Arc<LogicalPlan>>,
    source: Expr,
    destination: Expr,
    /// The weight expressions, as written.
    weights: Vec<String>,
}

/// The edge inputs of a node over the edge table `edges` of `graph`.
fn edge_inputs(graph: &GraphDef, edges: LogicalPlan, weights: WeightSet) -> Result<EdgeInputs> {
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
    Ok(EdgeInputs {
        input,
        weighted,
        source,
        destination,
        weights: weights.written,
    })
}

impl GraphFunctions {
    /// `graph_table('<body>')`: SQL/PGQ's `GRAPH_TABLE ( body )` after Ursa's
    /// parser rewrote it (see [`super::pattern`]).
    fn plan_graph_table(
        &self,
        relation: TableFactor,
        context: &mut dyn RelationPlannerContext,
    ) -> Result<RelationPlanning> {
        let TableFactor::Table {
            args: Some(args),
            alias,
            ..
        } = relation
        else {
            unreachable!("matched by the caller")
        };
        let body = match args.args.as_slice() {
            [FunctionArg::Unnamed(FunctionArgExpr::Expr(ast::Expr::Value(v)))] => match &v.value {
                Value::SingleQuotedString(s) => s.clone(),
                _ => return Err(graph_table_usage()),
            },
            _ => return Err(graph_table_usage()),
        };
        let pattern = super::pattern::parse(&body)?;
        let graph = self.graph(&pattern.graph)?;
        let source_of = |table: &TableReference, context: &dyn RelationPlannerContext| match self
            .registry
            .file_table(&graph.name, table)
        {
            Some(provider) => Ok(provider_as_source(provider)),
            None => self.table_source(table, context),
        };
        let lowered = {
            let context: &dyn RelationPlannerContext = context;
            let schema_of = |table: &TableReference| -> Result<Arc<Schema>> {
                Ok(source_of(table, context)?.schema())
            };
            super::pattern::lower(&pattern, &graph, &schema_of)?
        };
        let conditions = lowered
            .conditions
            .iter()
            .map(|c| parse_sql_expr(c))
            .collect::<Result<Vec<_>>>()?;
        let columns = parse_select_items(&lowered.columns)?;

        // The cross join of every variable's relation, as one branch's input.
        let mut from: Option<LogicalPlanBuilder> = None;
        for source in &lowered.sources {
            let table = source_of(&source.table, context)?;
            let mut scan = LogicalPlanBuilder::scan(source.table.clone(), table, None)?;
            if let Some(columns) = &source.columns {
                scan = scan.project(columns.iter().map(|(column, name)| {
                    Expr::Column(Column::new_unqualified(column)).alias(name)
                }))?;
            }
            let relation = scan.alias(source.var.as_str())?.build()?;
            from = Some(match from {
                None => LogicalPlanBuilder::from(relation),
                Some(b) => b.cross_join(relation)?,
            });
        }
        let base = from.expect("a pattern has a node").build()?;
        // A join column under the name its variable's relation gives it.
        let column = |(var, name): &super::pattern::VarColumn| {
            let source = lowered.sources.iter().find(|s| &s.var == var);
            let name = source
                .and_then(|s| s.columns.as_ref())
                .and_then(|columns| columns.iter().find(|(c, _)| c == name).map(|(_, n)| n))
                .unwrap_or(name);
            Expr::Column(Column::new(Some(var.as_str()), name))
        };
        let mut branches = Vec::new();
        for joins in &lowered.branches {
            let mut filter: Vec<Expr> = joins
                .iter()
                .map(|join| match join {
                    super::pattern::Join::Equal(a, b) => column(a).eq(column(b)),
                    super::pattern::Join::NotEqual(a, b) => column(a).not_eq(column(b)),
                })
                .collect();
            for condition in &conditions {
                filter.push(context.sql_to_expr(condition.clone(), base.schema())?);
            }
            let mut branch = LogicalPlanBuilder::from(base.clone());
            if let Some(predicate) = filter.into_iter().reduce(Expr::and) {
                branch = branch.filter(predicate)?;
            }
            let schema = branch.schema().clone();
            let mut projection = Vec::new();
            for item in &columns {
                match item {
                    ast::SelectItem::UnnamedExpr(expr) => {
                        projection.push(context.sql_to_expr(expr.clone(), &schema)?)
                    }
                    ast::SelectItem::ExprWithAlias { expr, alias } => projection.push(
                        context
                            .sql_to_expr(expr.clone(), &schema)?
                            .alias(context.normalize_ident(alias.clone())),
                    ),
                    ast::SelectItem::QualifiedWildcard(
                        ast::SelectItemQualifiedWildcardKind::ObjectName(name),
                        _,
                    ) => {
                        let var = context.normalize_ident(
                            name.0
                                .last()
                                .and_then(|p| p.as_ident().cloned())
                                .unwrap_or_else(|| ast::Ident::new(name.to_string())),
                        );
                        let columns: Vec<Expr> = schema
                            .iter()
                            .filter(|(q, _)| q.is_some_and(|q| q.table() == var))
                            .map(|(q, f)| Expr::Column(Column::new(q.cloned(), f.name())))
                            .collect();
                        if columns.is_empty() {
                            return Err(plan(format!(
                                "in GRAPH_TABLE: COLUMNS ({var}.*) names no variable '{var}'"
                            )));
                        }
                        projection.extend(columns);
                    }
                    other => {
                        return Err(plan(format!(
                        "in GRAPH_TABLE: COLUMNS takes expressions, as a.name AS name; got {other}"
                    )))
                    }
                }
            }
            branches.push(branch.project(projection)?.build()?);
        }
        let mut branches = branches.into_iter();
        let mut plan = LogicalPlanBuilder::from(branches.next().expect("one branch at least"));
        for branch in branches {
            plan = plan.union(branch)?;
        }
        let plan = match alias {
            Some(_) => plan.build()?,
            None => plan.alias(super::pattern::FUNCTION)?.build()?,
        };
        Ok(RelationPlanning::Planned(Box::new(PlannedRelation::new(
            plan, alias,
        ))))
    }

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

    /// The graph a call names first (the pair it runs on), and the arguments
    /// after it. As in DuckPGQ, `f(g, vertex_table, edge_table)` names the pair
    /// in a graph with several element tables; with one edge table, `f(g)`
    /// implies it (#295).
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
        // DuckPGQ's positional (vertex table, edge table). In algorithms(),
        // whose other arguments are calls, a bare name is taken as one only if
        // the graph has an element table by that name, so a call written
        // without parentheses (`algorithms(g, degree)`) keeps its own error.
        let mut args = args.peekable();
        let mut labels = Vec::new();
        while let Some(FunctionArg::Unnamed(FunctionArgExpr::Expr(ast::Expr::Identifier(ident)))) =
            args.peek()
        {
            if matches!(target, Target::Algorithms) {
                let label = context.normalize_ident(ident.clone());
                if graph.vertex_table(&label).is_none() && graph.edge_table(&label).is_none() {
                    break;
                }
            }
            let Some(FunctionArg::Unnamed(FunctionArgExpr::Expr(ast::Expr::Identifier(ident)))) =
                args.next()
            else {
                unreachable!("peeked")
            };
            labels.push(context.normalize_ident(ident));
        }
        let pair = match labels.as_slice() {
            [] => graph.pair(None, None)?,
            [vertex, edge] => graph.pair(Some(vertex), Some(edge))?,
            _ => {
                return Err(plan(format!(
                    "{name}() takes a graph, optionally followed by a vertex table and an edge \
                     table ({name}({}, <vertex table>, <edge table>)), then named parameters; \
                     got {} name{} after the graph",
                    graph.name,
                    labels.len(),
                    if labels.len() == 1 { "" } else { "s" }
                )))
            }
        };
        Ok((Arc::new(pair), args.collect()))
    }

    /// The graph called `name`.
    fn graph(&self, name: &str) -> Result<Arc<PropertyGraph>> {
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

/// One SQL expression, as written in a GRAPH_TABLE pattern.
fn parse_sql_expr(text: &str) -> Result<ast::Expr> {
    Parser::new(&GenericDialect {})
        .try_with_sql(text)
        .and_then(|mut p| p.parse_expr())
        .map_err(|e| plan(format!("in GRAPH_TABLE: {e} in '{text}'")))
}

/// The `COLUMNS` list, as SQL select items.
fn parse_select_items(text: &str) -> Result<Vec<ast::SelectItem>> {
    Parser::new(&GenericDialect {})
        .try_with_sql(text)
        .and_then(|mut p| p.parse_comma_separated(Parser::parse_select_item))
        .map_err(|e| plan(format!("in GRAPH_TABLE: {e} in COLUMNS ({text})")))
}

/// The error for `graph_table(…)` called other than as Ursa's parser writes it.
fn graph_table_usage() -> DataFusionError {
    DataFusionError::Plan(
        "graph_table() takes one string, a GRAPH_TABLE body; write \
         GRAPH_TABLE (graph MATCH (a)-[e]->(b) COLUMNS (a.id, b.id))"
            .to_string(),
    )
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
