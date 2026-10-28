//! An order-preserving parallel hash partition, the input of an aggregate whose
//! folds depend on the order of their values (#204).
//!
//! A float `sum` / `mean` / `min` / `max` folds each group's values in input
//! order ([`crate::agg`]), so the aggregate must see every group's rows in one
//! partition and in order. [`read_partitioned`] gives it that without a serial
//! pass over the rows:
//!
//! 1. the ordered rows are cut into chunks of [`CHUNK`] rows (a fixed size, not a
//!    function of the core count);
//! 2. each chunk's rows are routed to `n` partitions by the hash of their group
//!    key, all chunks in parallel, keeping each partition's rows in row order;
//! 3. partition `p` reads chunk 0's rows for `p`, then chunk 1's, and so on,
//!    gathered into batches as the aggregate pulls them.
//!
//! So each partition holds its groups' rows in input order, every group lies in
//! one partition, and the groups fold in parallel with each fold seeing exactly
//! the sequence a single-threaded fold would.
//!
//! The route is DataFusion's own hash repartition (`REPARTITION_RANDOM_STATE`,
//! `hash % n`), so the scan declares `Partitioning::Hash` honestly: the aggregate
//! over it needs no repartition, and a plan that meets it with another hash
//! partitioning of the same keys sees the same assignment.

use std::fmt;
use std::sync::Arc;

use arrow::array::{Array, ArrayRef, RecordBatch};
use arrow::datatypes::SchemaRef;
use async_trait::async_trait;
use datafusion::catalog::Session;
use datafusion::common::hash_utils::create_hashes;
use datafusion::datasource::{TableProvider, TableType};
use datafusion::error::{DataFusionError, Result};
use datafusion::execution::TaskContext;
use datafusion::logical_expr::Expr;
use datafusion::physical_expr::expressions::Column;
use datafusion::physical_expr::{EquivalenceProperties, PhysicalExpr};
use datafusion::physical_plan::execution_plan::{Boundedness, EmissionType};
use datafusion::physical_plan::repartition::REPARTITION_RANDOM_STATE;
use datafusion::physical_plan::stream::RecordBatchStreamAdapter;
use datafusion::physical_plan::{
    DisplayAs, DisplayFormatType, ExecutionPlan, Partitioning, PlanProperties,
    SendableRecordBatchStream,
};
use datafusion::prelude::{DataFrame, SessionContext};
use rayon::prelude::*;

/// Rows per chunk, and per batch a partition emits (DataFusion's batch size).
pub(crate) const CHUNK: usize = 8192;

fn arrow_err(e: arrow::error::ArrowError) -> DataFusionError {
    DataFusionError::ArrowError(Box::new(e), None)
}

/// One chunk of the input and where its rows go.
#[derive(Debug)]
struct Chunk {
    batch: RecordBatch,
    /// `rows[offsets[p]..offsets[p + 1]]` are the chunk's rows for partition
    /// `p`, ascending.
    offsets: Vec<u32>,
    rows: Vec<u16>,
}

impl Chunk {
    fn rows_for(&self, p: usize) -> &[u16] {
        &self.rows[self.offsets[p] as usize..self.offsets[p + 1] as usize]
    }
}

/// Route a chunk's rows to `n` partitions by the hash of its `keys` columns.
fn route(batch: RecordBatch, keys: &[usize], n: usize) -> Result<Chunk> {
    let r = batch.num_rows();
    let arrays: Vec<ArrayRef> = keys.iter().map(|&k| batch.column(k).clone()).collect();
    let mut hashes = vec![0u64; r];
    create_hashes(
        &arrays,
        REPARTITION_RANDOM_STATE.random_state(),
        &mut hashes,
    )?;
    // A counting sort by partition, stable, so each partition's rows ascend.
    let parts: Vec<u32> = hashes.iter().map(|h| (h % n as u64) as u32).collect();
    let mut offsets = vec![0u32; n + 1];
    for &p in &parts {
        offsets[p as usize + 1] += 1;
    }
    for p in 0..n {
        offsets[p + 1] += offsets[p];
    }
    let mut next = offsets.clone();
    let mut rows = vec![0u16; r];
    for (i, &p) in parts.iter().enumerate() {
        rows[next[p as usize] as usize] = i as u16;
        next[p as usize] += 1;
    }
    Ok(Chunk {
        batch,
        offsets,
        rows,
    })
}

/// `batches` (at least one, in order) as `n` partitions hashed by the columns at
/// `keys`, each holding its rows in input order (see the module docs).
pub fn read_partitioned(
    ctx: &SessionContext,
    batches: Vec<RecordBatch>,
    keys: &[usize],
    n: usize,
) -> Result<DataFrame> {
    let schema = batches[0].schema();
    let pieces: Vec<RecordBatch> = batches
        .iter()
        .flat_map(|b| {
            (0..b.num_rows())
                .step_by(CHUNK)
                .map(move |s| b.slice(s, CHUNK.min(b.num_rows() - s)))
        })
        .collect();
    let chunks = pieces
        .into_par_iter()
        .map(|b| route(b, keys, n))
        .collect::<Result<Vec<_>>>()?;
    let table = PartitionedTable {
        schema,
        chunks: Arc::new(chunks),
        keys: keys.to_vec(),
        n,
    };
    ctx.read_table(Arc::new(table))
}

#[derive(Debug)]
struct PartitionedTable {
    schema: SchemaRef,
    chunks: Arc<Vec<Chunk>>,
    keys: Vec<usize>,
    n: usize,
}

#[async_trait]
impl TableProvider for PartitionedTable {
    fn schema(&self) -> SchemaRef {
        self.schema.clone()
    }

    fn table_type(&self) -> TableType {
        TableType::Base
    }

    async fn scan(
        &self,
        _state: &dyn Session,
        projection: Option<&Vec<usize>>,
        _filters: &[Expr],
        _limit: Option<usize>,
    ) -> Result<Arc<dyn ExecutionPlan>> {
        let columns: Vec<usize> = match projection {
            Some(p) => p.clone(),
            None => (0..self.schema.fields().len()).collect(),
        };
        let schema = Arc::new(self.schema.project(&columns).map_err(arrow_err)?);
        // The keys as columns of the projected schema; without all of them the
        // partitioning is still by key, but can't be stated in its columns.
        let keys: Option<Vec<Arc<dyn PhysicalExpr>>> = self
            .keys
            .iter()
            .map(|k| {
                let i = columns.iter().position(|c| c == k)?;
                let e: Arc<dyn PhysicalExpr> = Arc::new(Column::new(schema.field(i).name(), i));
                Some(e)
            })
            .collect();
        let partitioning = match keys {
            Some(keys) => Partitioning::Hash(keys, self.n),
            None => Partitioning::UnknownPartitioning(self.n),
        };
        let properties = Arc::new(PlanProperties::new(
            EquivalenceProperties::new(schema.clone()),
            partitioning,
            EmissionType::Incremental,
            Boundedness::Bounded,
        ));
        Ok(Arc::new(PartitionedExec {
            chunks: self.chunks.clone(),
            columns,
            schema,
            properties,
        }))
    }
}

/// The scan of a [`PartitionedTable`]: partition `p` streams its rows, chunk by
/// chunk, gathered into batches of up to [`CHUNK`] rows.
#[derive(Debug)]
struct PartitionedExec {
    chunks: Arc<Vec<Chunk>>,
    /// The table's columns this scan emits.
    columns: Vec<usize>,
    schema: SchemaRef,
    properties: Arc<PlanProperties>,
}

impl DisplayAs for PartitionedExec {
    fn fmt_as(&self, _t: DisplayFormatType, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "OrderedHashPartitionExec: chunks={}, partitioning={}",
            self.chunks.len(),
            self.properties.partitioning
        )
    }
}

impl ExecutionPlan for PartitionedExec {
    fn name(&self) -> &str {
        "OrderedHashPartitionExec"
    }

    fn properties(&self) -> &Arc<PlanProperties> {
        &self.properties
    }

    fn children(&self) -> Vec<&Arc<dyn ExecutionPlan>> {
        vec![]
    }

    fn with_new_children(
        self: Arc<Self>,
        _children: Vec<Arc<dyn ExecutionPlan>>,
    ) -> Result<Arc<dyn ExecutionPlan>> {
        Ok(self)
    }

    fn execute(
        &self,
        partition: usize,
        _context: Arc<TaskContext>,
    ) -> Result<SendableRecordBatchStream> {
        let batches = Batches {
            chunks: self.chunks.clone(),
            columns: self.columns.clone(),
            schema: self.schema.clone(),
            p: partition,
            next: 0,
        };
        Ok(Box::pin(RecordBatchStreamAdapter::new(
            self.schema.clone(),
            futures::stream::iter(batches),
        )))
    }
}

/// Partition `p`'s rows from chunk `next` on, one gathered batch at a time.
struct Batches {
    chunks: Arc<Vec<Chunk>>,
    columns: Vec<usize>,
    schema: SchemaRef,
    p: usize,
    next: usize,
}

impl Batches {
    /// The rows of chunks `from..to` for this partition, as one batch.
    fn gather(&self, from: usize, to: usize, rows: usize) -> Result<RecordBatch> {
        let chunks = &self.chunks[from..to];
        // A whole chunk on its own (a partition holding all its rows) is emitted
        // as it is.
        if let [c] = chunks {
            if rows == c.batch.num_rows() {
                return c.batch.project(&self.columns).map_err(arrow_err);
            }
        }
        let mut picks = Vec::with_capacity(rows);
        for (i, c) in chunks.iter().enumerate() {
            picks.extend(c.rows_for(self.p).iter().map(|&r| (i, r as usize)));
        }
        let cols = self
            .columns
            .iter()
            .map(|&k| {
                let sources: Vec<&dyn Array> =
                    chunks.iter().map(|c| c.batch.column(k).as_ref()).collect();
                arrow::compute::interleave(&sources, &picks).map_err(arrow_err)
            })
            .collect::<Result<Vec<_>>>()?;
        RecordBatch::try_new(self.schema.clone(), cols).map_err(arrow_err)
    }
}

impl Iterator for Batches {
    type Item = Result<RecordBatch>;

    fn next(&mut self) -> Option<Self::Item> {
        // Skip the chunks with no rows here, then take chunks while the batch
        // stays within CHUNK rows (at least one chunk's rows).
        while self.next < self.chunks.len() && self.chunks[self.next].rows_for(self.p).is_empty() {
            self.next += 1;
        }
        if self.next == self.chunks.len() {
            return None;
        }
        let from = self.next;
        let mut rows = 0;
        while self.next < self.chunks.len() {
            let k = self.chunks[self.next].rows_for(self.p).len();
            if rows > 0 && rows + k > CHUNK {
                break;
            }
            rows += k;
            self.next += 1;
        }
        Some(self.gather(from, self.next, rows))
    }
}
