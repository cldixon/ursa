//! A graph as a schema: `<graph>.edges` and `<graph>.nodes`.
//!
//! Each graph registers a [`GraphSchema`] in the default catalog under its
//! name. Its two tables are views ([`ViewTable`]s) whose plans are built from
//! the graph's implied pair ([`PropertyGraph::default_pair`]: its edge table
//! and the vertex table that edge table refers to) every time a query names
//! them, so they always read the current contents of the tables (and files)
//! the graph is defined over. A graph with several vertex or edge tables has no
//! implied pair, and so no `nodes` or `edges` table: its element tables are
//! session tables already, and `MATCH` reads across them.
//!
//! - `edges` is the edge table: every row and column, unchanged.
//! - `nodes` is one row per node, the node id in a column named `id`:
//!   - without a vertex table, the distinct non-null endpoints (`source` and
//!     `destination` values) of the edges;
//!   - with one, every row of the vertex table (its key renamed to `id`,
//!     followed by its other columns), plus every endpoint that is not a key
//!     of the vertex table, with NULL attributes. When the key and the
//!     endpoints have different types (`Int32` and `Int64`, `Utf8` and a
//!     dictionary), `id` has their common type, a dictionary as its values.
//!
//! The plan builders ([`edges_plan`], [`nodes_plan`] and their `_from`
//! variants, which take table sources already resolved) are public so the graph
//! algorithms can reuse them.
//!
//! When a table under the graph cannot be read (it was deregistered through
//! the DataFusion API, say), `edges` and `nodes` are still listed, with the
//! columns they last had, and fail with the reason when a query reads them.
//! Session listings (`SHOW TABLES`, `information_schema`) look every table up,
//! so an error here would break them for the whole session.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, Weak};

use async_trait::async_trait;
use datafusion::arrow::datatypes::{DataType, Schema, SchemaRef};
use datafusion::catalog::{SchemaProvider, Session, TableProvider};
use datafusion::common::{Column, JoinType, ScalarValue, TableReference};
use datafusion::datasource::{provider_as_source, TableType, ViewTable};
use datafusion::error::{DataFusionError, Result};
use datafusion::execution::context::SessionState;
use datafusion::logical_expr::expr_fn::cast;
use datafusion::logical_expr::type_coercion::binary::comparison_coercion;
use datafusion::logical_expr::{col, lit, Expr, LogicalPlan, LogicalPlanBuilder, TableSource};
use datafusion::physical_plan::ExecutionPlan;
use parking_lot::RwLock;

use super::{registry, GraphDef, PropertyGraph};

/// The name of a graph's edge table in its schema.
pub const EDGES: &str = "edges";
/// The name of a graph's node table in its schema.
pub const NODES: &str = "nodes";
/// The node id column of `<graph>.nodes`.
pub const ID: &str = "id";

/// Qualifiers used inside the `nodes` plan.
const ENDPOINTS: &str = "__ursa_endpoints";
const VERTICES: &str = "__ursa_vertices";

/// The schema of one graph: its `edges` and `nodes` tables.
#[derive(Debug)]
pub struct GraphSchema {
    graph: String,
    session: Weak<RwLock<SessionState>>,
    /// The columns `edges` and `nodes` had when last planned.
    last_columns: Mutex<HashMap<&'static str, SchemaRef>>,
}

impl GraphSchema {
    pub(crate) fn new(graph: String, session: Weak<RwLock<SessionState>>) -> Self {
        Self {
            graph,
            session,
            last_columns: Mutex::default(),
        }
    }

    /// The graph, if the session and the graph still exist.
    fn graph(&self) -> Option<(SessionState, std::sync::Arc<PropertyGraph>)> {
        let state = self.session.upgrade()?.read().clone();
        let graph = registry(&state).get(&self.graph)?;
        Some((state, graph))
    }

    /// Whether the graph has `nodes` and `edges` tables (an implied pair).
    fn has_tables(&self) -> bool {
        self.graph()
            .is_some_and(|(_, graph)| graph.default_pair().is_some())
    }
}

#[async_trait]
impl SchemaProvider for GraphSchema {
    fn table_names(&self) -> Vec<String> {
        if self.has_tables() {
            vec![EDGES.to_string(), NODES.to_string()]
        } else {
            Vec::new()
        }
    }

    async fn table(&self, name: &str) -> Result<Option<Arc<dyn TableProvider>>> {
        let name = match name {
            EDGES => EDGES,
            NODES => NODES,
            _ => return Ok(None),
        };
        let state = self
            .session
            .upgrade()
            .map(|state| state.read().clone())
            .ok_or_else(|| DataFusionError::Internal("the session has been dropped".into()))?;
        let Some(def) = registry(&state)
            .get(&self.graph)
            .and_then(|graph| graph.default_pair())
        else {
            return Ok(None);
        };
        let plan = if name == EDGES {
            edges_plan(&state, &def).await
        } else {
            nodes_plan(&state, &def).await
        };
        let mut last_columns = self.last_columns.lock().unwrap_or_else(|e| e.into_inner());
        match plan {
            Ok(plan) => {
                let view = ViewTable::new(plan, None);
                last_columns.insert(name, view.schema());
                Ok(Some(Arc::new(view)))
            }
            Err(e) => Ok(Some(Arc::new(UnreadableTable {
                schema: last_columns
                    .get(name)
                    .cloned()
                    .unwrap_or_else(|| Arc::new(Schema::empty())),
                message: format!(
                    "cannot read {}.{name}: {}",
                    def.name,
                    crate::user_message(&e)
                ),
            }))),
        }
    }

    fn table_exist(&self, name: &str) -> bool {
        (name == EDGES || name == NODES) && self.has_tables()
    }
}

/// A graph table whose plan cannot be built: listed with the columns it last
/// had, and failing with `message` when a query reads it.
#[derive(Debug)]
struct UnreadableTable {
    schema: SchemaRef,
    message: String,
}

#[async_trait]
impl TableProvider for UnreadableTable {
    fn schema(&self) -> SchemaRef {
        Arc::clone(&self.schema)
    }

    fn table_type(&self) -> TableType {
        TableType::View
    }

    async fn scan(
        &self,
        _state: &dyn Session,
        _projection: Option<&Vec<usize>>,
        _filters: &[Expr],
        _limit: Option<usize>,
    ) -> Result<Arc<dyn ExecutionPlan>> {
        Err(DataFusionError::Plan(self.message.clone()))
    }
}

/// The table `table` names for the graph called `graph`: one the graph opened
/// from a file path when it was created, or else the table or view in `state`.
pub async fn graph_table(
    state: &SessionState,
    graph: &str,
    table: &TableReference,
) -> Result<Arc<dyn TableProvider>> {
    match registry(state).file_table(graph, table) {
        Some(provider) => Ok(provider),
        None => resolve_table(state, table).await,
    }
}

/// The table provider `table` names in `state`, or an error saying it does not
/// exist.
pub async fn resolve_table(
    state: &SessionState,
    table: &TableReference,
) -> Result<Arc<dyn TableProvider>> {
    lookup_table(state, table)
        .await?
        .ok_or_else(|| DataFusionError::Plan(format!("table '{table}' not found")))
}

/// The table provider `table` names in `state`, if there is one.
pub async fn lookup_table(
    state: &SessionState,
    table: &TableReference,
) -> Result<Option<Arc<dyn TableProvider>>> {
    match state.schema_for_ref(table.clone()) {
        Ok(schema) => schema.table(table.table()).await,
        Err(_) => Ok(None),
    }
}

/// The plan of `<graph>.edges`: a scan of the edge table, as it is now.
pub async fn edges_plan(state: &SessionState, def: &GraphDef) -> Result<LogicalPlan> {
    let edges = graph_table(state, &def.name, &def.edges.table).await?;
    edges_plan_from(def, provider_as_source(edges))
}

/// [`edges_plan`], with the edge table's source already resolved.
pub fn edges_plan_from(def: &GraphDef, edges: Arc<dyn TableSource>) -> Result<LogicalPlan> {
    LogicalPlanBuilder::scan(def.edges.table.clone(), edges, None)?.build()
}

/// The plan of `<graph>.nodes` (see the module docs).
pub async fn nodes_plan(state: &SessionState, def: &GraphDef) -> Result<LogicalPlan> {
    let edges = provider_as_source(graph_table(state, &def.name, &def.edges.table).await?);
    let vertices = match &def.vertices {
        Some(v) => Some(provider_as_source(
            graph_table(state, &def.name, &v.table).await?,
        )),
        None => None,
    };
    nodes_plan_from(def, edges, vertices)
}

/// [`nodes_plan`], with the tables' sources already resolved. `vertices` must
/// be given when the graph has a vertex table.
pub fn nodes_plan_from(
    def: &GraphDef,
    edges: Arc<dyn TableSource>,
    vertices: Option<Arc<dyn TableSource>>,
) -> Result<LogicalPlan> {
    let endpoint_type = column_type(&edges.schema(), &def.edges.table, &def.edges.source)?;
    let (vertex_def, vertices) = match (&def.vertices, vertices) {
        (None, _) => {
            return endpoints(def, &edges)?.build();
        }
        (Some(v), Some(source)) => (v, source),
        (Some(v), None) => {
            return Err(DataFusionError::Internal(format!(
                "nodes_plan_from: graph {} has vertex table {} but no source was given for it",
                def.name, v.table
            )))
        }
    };

    let vertex_schema = vertices.schema();
    let key_type = column_type(&vertex_schema, &vertex_def.table, &vertex_def.key)?;
    let id_type = comparison_coercion(&key_type, &endpoint_type).ok_or_else(|| {
        DataFusionError::Plan(format!(
            "the key of vertex table '{}' is {key_type} and the edge endpoints are \
             {endpoint_type}; they cannot be compared",
            vertex_def.table
        ))
    })?;
    // The key and the endpoints differ (say Int32 and Int64): `id` takes their
    // common type, a dictionary as its values.
    let id_type = match (key_type == endpoint_type, id_type) {
        (true, _) => key_type.clone(),
        (false, DataType::Dictionary(_, values)) => *values,
        (false, other) => other,
    };

    // The vertex table: `key AS id`, then its other columns.
    let others: Vec<_> = vertex_schema
        .fields()
        .iter()
        .filter(|f| f.name() != &vertex_def.key)
        .cloned()
        .collect();
    let mut vertex_exprs = vec![cast_to(
        Expr::Column(Column::new(Some(vertex_def.table.clone()), &vertex_def.key)),
        &key_type,
        &id_type,
    )
    .alias(ID)];
    vertex_exprs.extend(
        others
            .iter()
            .map(|f| Expr::Column(Column::new(Some(vertex_def.table.clone()), f.name()))),
    );
    let vertex_rows = LogicalPlanBuilder::scan(vertex_def.table.clone(), vertices, None)?
        .project(vertex_exprs)?
        .build()?;

    // The endpoints that are not keys of the vertex table, with NULL attributes.
    let keys = LogicalPlanBuilder::from(vertex_rows.clone())
        .project(vec![col(ID)])?
        .alias(VERTICES)?
        .build()?;
    let mut missing_exprs = vec![col(Column::new(Some(ENDPOINTS), ID)).alias(ID)];
    for f in &others {
        missing_exprs.push(lit(ScalarValue::try_from(f.data_type())?).alias(f.name()));
    }
    let missing = endpoints(def, &edges)?
        .project(vec![cast_to(col(ID), &endpoint_type, &id_type).alias(ID)])?
        .alias(ENDPOINTS)?
        .join(
            keys,
            JoinType::LeftAnti,
            (
                vec![Column::new(Some(ENDPOINTS), ID)],
                vec![Column::new(Some(VERTICES), ID)],
            ),
            None,
        )?
        .project(missing_exprs)?
        .build()?;

    LogicalPlanBuilder::from(vertex_rows)
        .union(missing)?
        .build()
}

/// The distinct non-null endpoints of the edges, as one column `id`.
fn endpoints(def: &GraphDef, edges: &Arc<dyn TableSource>) -> Result<LogicalPlanBuilder> {
    let side = |column: &str| -> Result<LogicalPlan> {
        LogicalPlanBuilder::scan(def.edges.table.clone(), Arc::clone(edges), None)?
            .project(vec![Expr::Column(Column::new(
                Some(def.edges.table.clone()),
                column,
            ))
            .alias(ID)])?
            .build()
    };
    LogicalPlanBuilder::from(side(&def.edges.source)?)
        .union_distinct(side(&def.edges.destination)?)?
        .filter(col(ID).is_not_null())
}

fn cast_to(expr: Expr, from: &DataType, to: &DataType) -> Expr {
    if from == to {
        expr
    } else {
        cast(expr, to.clone())
    }
}

fn column_type(schema: &Schema, table: &TableReference, column: &str) -> Result<DataType> {
    schema
        .field_with_name(column)
        .map(|f| f.data_type().clone())
        .map_err(|_| {
            DataFusionError::Plan(format!("column '{column}' not found in table '{table}'"))
        })
}

#[cfg(test)]
mod tests {
    use datafusion::arrow::array::{DictionaryArray, Int64Array, RecordBatch, StringArray};
    use datafusion::arrow::datatypes::Int32Type;
    use datafusion::datasource::MemTable;

    use super::*;
    use crate::graph::{create_graph, drop_graph, EdgeTable, VertexTable};

    fn table(batch: RecordBatch) -> Arc<MemTable> {
        Arc::new(MemTable::try_new(batch.schema(), vec![vec![batch]]).unwrap())
    }

    #[tokio::test]
    async fn dictionary_ids_and_a_vertex_table_without_every_node() {
        let ctx = crate::new_session();
        let ids = |values: Vec<&str>| -> Arc<dyn datafusion::arrow::array::Array> {
            Arc::new(values.into_iter().collect::<DictionaryArray<Int32Type>>())
        };
        let edges = RecordBatch::try_from_iter(vec![
            ("a", ids(vec!["x", "y", "z", "z"])),
            ("b", ids(vec!["y", "z", "x", "w"])),
        ])
        .unwrap();
        let people = RecordBatch::try_from_iter(vec![
            (
                "name",
                Arc::new(StringArray::from(vec!["x", "y", "z"])) as _,
            ),
            ("age", Arc::new(Int64Array::from(vec![30, 40, 50])) as _),
        ])
        .unwrap();
        ctx.register_table("follows", table(edges)).unwrap();
        ctx.register_table("people", table(people)).unwrap();

        let def = GraphDef {
            name: "social".into(),
            edges: EdgeTable::new(TableReference::bare("follows"), "a", "b"),
            vertices: Some(VertexTable::new(TableReference::bare("people"), "name")),
        };
        create_graph(&ctx, def.clone(), false, false).await.unwrap();

        // The plans the algorithms can reuse.
        let state = ctx.state();
        let edges = edges_plan(&state, &def).await.unwrap();
        assert_eq!(edges.schema().fields().len(), 2);
        let nodes = nodes_plan(&state, &def).await.unwrap();
        let columns: Vec<_> = nodes
            .schema()
            .fields()
            .iter()
            .map(|f| (f.name().clone(), f.data_type().clone()))
            .collect();
        assert_eq!(
            columns,
            [
                (ID.to_string(), DataType::Utf8),
                ("age".to_string(), DataType::Int64)
            ]
        );

        let batches = ctx
            .sql("SELECT id, age FROM social.nodes ORDER BY id")
            .await
            .unwrap()
            .collect()
            .await
            .unwrap();
        let formatted = datafusion::arrow::util::pretty::pretty_format_batches(&batches)
            .unwrap()
            .to_string();
        assert_eq!(
            formatted,
            "\
+----+-----+
| id | age |
+----+-----+
| w  |     |
| x  | 30  |
| y  | 40  |
| z  | 50  |
+----+-----+"
        );

        drop_graph(&ctx, "social", false).unwrap();
        assert!(ctx.sql("SELECT * FROM social.nodes").await.is_err());
    }
}
