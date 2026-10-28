//! Lowering a plan's relational steps to DataFusion, in written order, with the
//! order guarantees implemented once (#191).
//!
//! Every frame a plan produces (a source's rows, a graph or traversal leaf, a
//! join) becomes a [`Frame`]: a DataFusion `DataFrame` plus what is known about
//! its row order. Each relational step (`filter`, `select`, `with_columns` of
//! scalar expressions, `rename`, `sort`, `head`, `distinct`, `sample`,
//! `group_by().agg()`) maps onto stock DataFusion operators on top of it, in the
//! order the steps are written, and [`Frame::collect`] runs the result.
//!
//! ## Row order
//!
//! A frame has a logical row order, and the guarantees are stated against it:
//!
//! - unsorted output follows input order (#168): a source's row order, a graph
//!   leaf's node order, a traversal's own output order, a join's left-major order;
//! - `sort` is stable: rows that tie on every key keep their input order (#165);
//! - `distinct` and `group_by` emit one row per group at its first input row;
//! - `sample` keeps the picked rows in input order;
//! - `head(n)` takes the first `n` rows of that order.
//!
//! Order is a property the lowering tracks rather than a column every plan pays
//! for. The session never round-robin repartitions (as in a scan, #182), so a
//! frame read as one partition stays one partition through `filter`, projections
//! and `limit`, which keep their input's order: such a frame is `in_order`, and
//! its rows come out in logical order with no extra work. Only the operators
//! that do not keep order need more:
//!
//! - `sort` (DataFusion's is not stable) and the hash aggregate behind
//!   `group_by` and `distinct` (which repartitions by hash) number the rows with
//!   a hidden ordinal first. The sort orders by it as its last key; the aggregate
//!   carries `min(ordinal)`, a group's first position, and the frame is then no
//!   longer `in_order`.
//! - A frame that is not `in_order` is put back in order by its ordinal only
//!   where order is observed: at the end, before a `head` (a top-k on the
//!   ordinal), and before a step that reads positions (`sample`) or sums floats
//!   (`group_by`, so a sum adds in input order, as pyarrow's did).
//!
//! ## Joins
//!
//! A join ([`join`]) runs both sides as frames of the one plan. Its hash join
//! emits rows in build/probe order, so each side's rows are put in logical order
//! and numbered densely first, and the joined rows carry one ordinal folded from
//! the two positions, which orders them as the join guarantees:
//!
//! - `inner`, `left`, `semi`, `anti`: left rows in order, each followed by its
//!   matches in right-frame order (an unmatched `left` row once, with nulls);
//! - `right`: the mirror image, right rows in order, each followed by its
//!   matches in left-frame order;
//! - `outer`: `left`'s order, then the unmatched right rows in right-frame order.

use std::collections::BTreeSet;
use std::sync::Arc;

use arrow::array::{Array, ArrayRef, Float64Array, Int64Array, RecordBatch, UInt64Array};
use arrow::compute::cast;
use arrow::datatypes::{DataType, Field, Schema, SchemaRef};
use datafusion::common::{Column, DFSchema, DFSchemaRef};
use datafusion::error::{DataFusionError, Result};
use datafusion::functions_aggregate::expr_fn::min;
use datafusion::logical_expr::{lit, JoinType};
use datafusion::logical_expr::{Expr, ExprSchemable};
use datafusion::prelude::{coalesce, DataFrame, SessionContext};

/// The session relational steps run on: the graph session, which plans Ursa's
/// extension nodes and never round-robin repartitions, so a single-partition
/// frame keeps its row order through `filter` and projections.
pub fn session() -> SessionContext {
    #[cfg(test)]
    if let Some(p) = TEST_PARTITIONS.with(|c| c.get()) {
        return crate::planner::graph_session_with_config(
            datafusion::prelude::SessionConfig::new().with_target_partitions(p),
        );
    }
    crate::planner::graph_session()
}

#[cfg(test)]
thread_local! {
    /// Pins the session's `target_partitions` in unit tests.
    pub(crate) static TEST_PARTITIONS: std::cell::Cell<Option<usize>> =
        const { std::cell::Cell::new(None) };
}

/// The base name of the hidden ordinal column.
const ORD: &str = "__ursa_ord";

/// Rows per batch a numbered frame is cut into (DataFusion's batch size), so a
/// sort over it can spread across partitions.
const CHUNK: usize = 8192;

fn arrow_err(e: arrow::error::ArrowError) -> DataFusionError {
    DataFusionError::ArrowError(Box::new(e), None)
}

/// An unqualified column reference that takes the name verbatim (no `a.b` parse).
pub fn col_named(name: &str) -> Expr {
    Expr::Column(Column::from_name(name))
}

/// A relational frame being lowered: a DataFusion plan and its row order.
///
/// A frame whose rows are materialized (a source's rows, an attached or sorted
/// result) holds them as batches and builds a plan over them only when a step
/// needs one: `filter`, `head` and column projections run on the batches
/// directly, with the same compiled expressions, so a cheap step on a small frame
/// pays no planning.
pub struct Frame {
    ctx: SessionContext,
    /// The plan, unless the frame is only `batches` so far.
    df: Option<DataFrame>,
    /// The rows, while they are materialized (`df`, if any, reads exactly these).
    batches: Option<Vec<RecordBatch>>,
    /// The frame's schema (the plan's, or the batches').
    schema: DFSchemaRef,
    /// The hidden ordinal column, when the rows carry one: their position in the
    /// frame's logical order (unique per row; for a group, its first row's).
    ord: Option<String>,
    /// Whether the plan emits the rows in logical order (one partition,
    /// order-keeping operators only). A frame that is not `in_order` has `ord`.
    in_order: bool,
    /// Columns the result reports as nullable whatever the plan infers: the
    /// output of `group_by`, as it has always been (the plan marks a `count`, or
    /// a key that was never null, non-nullable).
    nullable: BTreeSet<String>,
}

impl Frame {
    /// A frame over `batches` (at least one), in their order.
    pub fn from_batches(ctx: SessionContext, batches: Vec<RecordBatch>) -> Result<Frame> {
        let mut frame = Frame {
            ctx,
            df: None,
            batches: None,
            schema: Arc::new(DFSchema::empty()),
            ord: None,
            in_order: true,
            nullable: BTreeSet::new(),
        };
        frame.set(batches)?;
        Ok(frame)
    }

    /// A frame over a single-partition plan that emits its rows in order (a
    /// graph or traversal leaf).
    pub fn from_ordered(ctx: SessionContext, df: DataFrame) -> Frame {
        Frame {
            ctx,
            schema: Arc::new(df.schema().clone()),
            df: Some(df),
            batches: None,
            ord: None,
            in_order: true,
            nullable: BTreeSet::new(),
        }
    }

    /// A frame whose rows carry the ordinal `ord` but come out in any order (a
    /// hash join's output).
    pub fn from_numbered(ctx: SessionContext, df: DataFrame, ord: String) -> Frame {
        Frame {
            ctx,
            schema: Arc::new(df.schema().clone()),
            df: Some(df),
            batches: None,
            ord: Some(ord),
            in_order: false,
            nullable: BTreeSet::new(),
        }
    }

    pub fn ctx(&self) -> &SessionContext {
        &self.ctx
    }

    /// The user-visible columns (the ordinal hidden), as an Arrow schema.
    pub fn schema(&self) -> SchemaRef {
        let s = self.schema.as_arrow();
        let keep: Vec<usize> = (0..s.fields().len())
            .filter(|&i| Some(s.field(i).name()) != self.ord.as_ref())
            .collect();
        Arc::new(s.project(&keep).expect("indices are in range"))
    }

    /// The user-visible column names, in order.
    pub fn names(&self) -> Vec<String> {
        self.schema()
            .fields()
            .iter()
            .map(|f| f.name().clone())
            .collect()
    }

    pub fn has_column(&self, name: &str) -> bool {
        Some(name) != self.ord.as_deref() && self.schema.as_arrow().index_of(name).is_ok()
    }

    /// The type of an expression over this frame.
    pub fn type_of(&self, e: &Expr) -> Result<DataType> {
        e.get_type(self.schema.as_ref())
    }

    /// The frame's schema, for lowering expressions against.
    pub fn df_schema(&self) -> &DFSchema {
        &self.schema
    }

    /// The plan (built over the batches if there is none yet), moved out: the
    /// caller continues from what it builds on it (cloning a plan clones its
    /// session state).
    fn plan(&mut self) -> Result<DataFrame> {
        match self.df.take() {
            Some(df) => Ok(df),
            None => {
                let batches = self.batches.clone().expect("a frame has a plan or rows");
                read_ordered(&self.ctx, batches)
            }
        }
    }

    /// Continue from `df`.
    fn replace(&mut self, df: DataFrame) {
        self.schema = Arc::new(df.schema().clone());
        self.df = Some(df);
        self.batches = None;
    }

    /// Continue from materialized rows (at least one batch), in order.
    fn set(&mut self, batches: Vec<RecordBatch>) -> Result<()> {
        self.schema = Arc::new(DFSchema::try_from(batches[0].schema().as_ref().clone())?);
        self.df = None;
        self.batches = Some(batches);
        Ok(())
    }

    /// The rows, materialized (every partition of the plan, in partition order);
    /// the frame continues from them.
    fn run(&mut self) -> Result<Vec<RecordBatch>> {
        if let Some(b) = &self.batches {
            return Ok(b.clone());
        }
        let df = self.plan()?;
        let schema: SchemaRef = Arc::new(df.schema().as_arrow().clone());
        if crate::dry::active() {
            // A dry run records the plan instead of running it (an empty result
            // takes the plan's schema here too).
            crate::dry::record(df.logical_plan());
            let batches = vec![RecordBatch::new_empty(schema)];
            self.set(batches.clone())?;
            return Ok(batches);
        }
        let mut batches = crate::runtime::block_on(df.collect())??;
        batches.retain(|b| b.num_rows() > 0);
        if batches.is_empty() {
            batches.push(RecordBatch::new_empty(schema));
        }
        self.set(batches.clone())?;
        Ok(batches)
    }

    /// Number the rows with a hidden ordinal, if they don't carry one.
    fn number(&mut self) -> Result<String> {
        if let Some(ord) = &self.ord {
            return Ok(ord.clone());
        }
        debug_assert!(self.in_order, "a frame out of order carries its ordinal");
        let batches = self.run()?;
        let name = fresh_name(&batches[0].schema());
        self.set(attach_ordinal(batches, &name)?)?;
        self.ord = Some(name.clone());
        Ok(name)
    }

    /// Put the rows back in logical order (by the ordinal), materialized.
    fn reorder(&mut self) -> Result<()> {
        if self.in_order {
            return Ok(());
        }
        let ord = self
            .ord
            .clone()
            .expect("a frame out of order has an ordinal");
        let batches = self.run()?;
        self.set(sort_by_ordinal(&batches, &ord)?)?;
        self.in_order = true;
        Ok(())
    }

    /// Drop the ordinal (the frame must be in order).
    fn drop_ordinal(&mut self) -> Result<()> {
        if let Some(ord) = self.ord.take() {
            let keep: Vec<Expr> = self
                .schema
                .iter()
                .filter(|(_, f)| f.name() != &ord)
                .map(|(q, f)| Expr::Column(Column::from((q, f))))
                .collect();
            self.select_exprs(keep)?;
        }
        Ok(())
    }

    /// Every user column, as plan expressions.
    fn user_columns(&self) -> Vec<Expr> {
        self.schema
            .iter()
            .filter(|(_, f)| Some(f.name()) != self.ord.as_ref())
            .map(|(q, f)| Expr::Column(Column::from((q, f))))
            .collect()
    }

    /// A projection to `exprs`: on materialized rows, column references are
    /// picked straight from the batches.
    fn select_exprs(&mut self, exprs: Vec<Expr>) -> Result<()> {
        if let Some(batches) = &self.batches {
            let schema = batches[0].schema();
            let picks: Option<Vec<(usize, String)>> = exprs
                .iter()
                .map(|e| {
                    let (c, name) = match e {
                        Expr::Column(c) => (c, c.name.clone()),
                        Expr::Alias(a) => match a.expr.as_ref() {
                            Expr::Column(c) => (c, a.name.clone()),
                            _ => return None,
                        },
                        _ => return None,
                    };
                    Some((schema.index_of(&c.name).ok()?, name))
                })
                .collect();
            let unique = picks.as_ref().is_some_and(|p| {
                let names: BTreeSet<&String> = p.iter().map(|(_, n)| n).collect();
                names.len() == p.len()
            });
            if let (Some(picks), true) = (picks, unique) {
                let fields: Vec<Field> = picks
                    .iter()
                    .map(|(i, name)| schema.field(*i).clone().with_name(name))
                    .collect();
                let out = Arc::new(Schema::new_with_metadata(fields, schema.metadata().clone()));
                let projected = batches
                    .iter()
                    .map(|b| {
                        let cols = picks.iter().map(|(i, _)| b.column(*i).clone()).collect();
                        RecordBatch::try_new(out.clone(), cols).map_err(arrow_err)
                    })
                    .collect::<Result<Vec<_>>>()?;
                return self.set(projected);
            }
        }
        let df = self.plan()?.select(exprs)?;
        self.replace(df);
        Ok(())
    }

    // --- steps -------------------------------------------------------------

    /// `filter`: keep the rows `predicate` holds for (a null is not kept).
    pub fn filter(&mut self, predicate: Expr) -> Result<()> {
        if let Some(batches) = self.batches.take() {
            let compiled = crate::eval::physical(predicate, &self.schema)?;
            return self.set(crate::eval::filter_batches(batches, &[compiled])?);
        }
        let df = self.plan()?.filter(predicate)?;
        self.replace(df);
        Ok(())
    }

    /// A projection to `exprs` (each named), the ordinal carried along.
    pub fn project(&mut self, exprs: Vec<Expr>) -> Result<()> {
        let mut exprs = exprs;
        if let Some(ord) = &self.ord {
            exprs.push(col_named(ord));
        }
        let names: Vec<String> = exprs.iter().map(output_name).collect();
        self.nullable.retain(|n| names.contains(n));
        self.select_exprs(exprs)
    }

    /// `rename`: every `(old, new)` relabelled at once (all names checked).
    pub fn rename(&mut self, mapping: &[(String, String)]) -> Result<()> {
        let lookup = |name: &str| {
            mapping
                .iter()
                .find(|(old, _)| old == name)
                .map(|(_, new)| new.clone())
        };
        let exprs: Vec<Expr> = self
            .schema
            .iter()
            .filter(|(_, f)| Some(f.name()) != self.ord.as_ref())
            .map(|(q, f)| {
                let e = Expr::Column(Column::from((q, f)));
                match lookup(f.name()) {
                    Some(new) => e.alias(new),
                    None => e.alias(f.name()),
                }
            })
            .collect();
        self.nullable = self
            .nullable
            .iter()
            .map(|n| lookup(n).unwrap_or_else(|| n.clone()))
            .collect();
        self.project(exprs)
    }

    /// `sort`: stable, lexicographic over `keys` (`(expr, descending)`), nulls
    /// last for every key. A float key sorts canonical (#201): every NaN equal and
    /// above every number, `-0.0` equal to `0.0`. `limit` is a `head` written
    /// right after it (a top-k).
    pub fn sort(&mut self, keys: Vec<(Expr, bool)>, limit: Option<usize>) -> Result<()> {
        let ord = self.number()?;
        let mut sort_exprs = Vec::with_capacity(keys.len() + 1);
        let mut fast: Option<(String, bool)> = None;
        for (i, (e, descending)) in keys.iter().enumerate() {
            let dt = self.type_of(e)?;
            if let (0, Expr::Column(c), 1) = (i, e, keys.len()) {
                if is_fast_key(&dt) {
                    fast = Some((c.name.clone(), *descending));
                }
            }
            let e = match &dt {
                dt if dt.is_floating() => crate::compare::canonical(e.clone()),
                // A dictionary of floats sorts by its values: canonical too.
                DataType::Dictionary(_, value) if value.is_floating() => {
                    crate::compare::canonical(Expr::Cast(datafusion::logical_expr::Cast::new(
                        Box::new(e.clone()),
                        value.as_ref().clone(),
                    )))
                }
                _ => e.clone(),
            };
            sort_exprs.push(e.sort(!descending, false));
        }
        if let (Some((key, descending)), None) = (&fast, limit) {
            // A full sort by one primitive key: sort (key, ordinal) pairs. The
            // ordinal decides ties, so the frame's physical order doesn't matter.
            let batches = self.run()?;
            self.set(sort_by_key_then_ordinal(&batches, key, *descending, &ord)?)?;
        } else {
            sort_exprs.push(col_named(&ord).sort(true, false));
            let mut df = self.plan()?.sort(sort_exprs)?;
            if let Some(n) = limit {
                df = df.limit(0, Some(n))?;
            }
            self.replace(df.drop_columns(&[ord.as_str()])?);
        }
        // Sorted rows are in their new order; a later step that needs positions
        // numbers them afresh.
        self.ord = None;
        self.in_order = true;
        Ok(())
    }

    /// `head(n)`: the first `n` rows.
    pub fn head(&mut self, n: usize) -> Result<()> {
        if self.in_order {
            if let Some(batches) = self.batches.take() {
                return self.set(take_prefix(batches, n));
            }
        }
        let df = if self.in_order {
            self.plan()?.limit(0, Some(n))?
        } else {
            let ord = self
                .ord
                .clone()
                .expect("a frame out of order has an ordinal");
            self.in_order = true;
            self.plan()?
                .sort(vec![col_named(&ord).sort(true, false)])?
                .limit(0, Some(n))?
        };
        self.replace(df);
        Ok(())
    }

    /// The plan an aggregate reads: the numbered rows, spread over the session's
    /// partitions when the aggregate doesn't depend on their order (the ordinal
    /// restores it), else as one ordered partition.
    fn aggregate_input(&mut self, ordered: bool, keys: &[Expr]) -> Result<DataFrame> {
        let n = self.ctx.state().config().target_partitions().max(1);
        if ordered {
            // Hash-partitioned by the keys from one ordered partition: each
            // group's rows land in one partition, still in input order, so the
            // groups fold in parallel and each fold sees its values in order.
            let df = self.plan()?;
            if n == 1 || keys.is_empty() {
                return Ok(df);
            }
            return df.repartition(datafusion::logical_expr::Partitioning::Hash(
                keys.to_vec(),
                n,
            ));
        }
        let Some(batches) = &self.batches else {
            return self.plan();
        };
        let k = n.min(batches.len());
        let mut parts: Vec<Vec<RecordBatch>> = vec![vec![]; k];
        for (i, b) in batches.iter().enumerate() {
            parts[i % k].push(b.clone());
        }
        let table = datafusion::datasource::MemTable::try_new(batches[0].schema(), parts)?;
        self.ctx.read_table(Arc::new(table))
    }

    /// `distinct`: one row per distinct row, at its first occurrence.
    pub fn distinct(&mut self) -> Result<()> {
        let ord = self.number()?;
        let groups = self.user_columns();
        // The group's first position, under a new name (an aggregate over an
        // aggregate's output must not reuse its column names).
        let first = fresh_name(self.schema.as_arrow());
        let input = self.aggregate_input(false, &[])?;
        let df = aggregate(input, groups, vec![min(col_named(&ord)).alias(&first)])?;
        self.replace(df);
        self.ord = Some(first);
        self.in_order = false;
        Ok(())
    }

    /// `group_by(keys).agg(aggs)`: one row per group, at its first input row.
    /// When an aggregate depends on the order of its values (`ordered`: a float
    /// fold), the rows reach the aggregate in input order, as one partition, so a
    /// sum adds them in that order.
    pub fn group_by(&mut self, keys: Vec<Expr>, aggs: Vec<Expr>, ordered: bool) -> Result<()> {
        let ord = self.number()?;
        if ordered {
            self.reorder()?;
        }
        let names: Vec<String> = keys.iter().chain(aggs.iter()).map(output_name).collect();
        let mut aggs = aggs;
        let first = fresh_name(self.schema.as_arrow());
        aggs.push(min(col_named(&ord)).alias(&first));
        let input = self.aggregate_input(ordered, &keys)?;
        let df = aggregate(input, keys, aggs)?;
        self.replace(df);
        self.ord = Some(first);
        self.in_order = false;
        self.nullable.extend(names);
        Ok(())
    }

    /// `sample`: the rows at `positions` (ascending), or all of them for `None`.
    /// `pick` gets the row count.
    pub fn sample(
        &mut self,
        pick: impl FnOnce(usize) -> crate::plan::PlanResult<Option<Vec<usize>>>,
    ) -> crate::plan::PlanResult<()> {
        self.reorder()?;
        let batches = self.run()?;
        let r: usize = batches.iter().map(|b| b.num_rows()).sum();
        let Some(positions) = pick(r)? else {
            self.set(batches)?;
            return Ok(());
        };
        let schema = batches[0].schema();
        let mut starts = Vec::with_capacity(batches.len());
        let mut acc = 0usize;
        for b in &batches {
            starts.push(acc);
            acc += b.num_rows();
        }
        let picks: Vec<(usize, usize)> = positions
            .into_iter()
            .map(|g| {
                let b = starts.partition_point(|&s| s <= g) - 1;
                (b, g - starts[b])
            })
            .collect();
        let cols = (0..schema.fields().len())
            .map(|c| {
                let sources: Vec<&dyn Array> =
                    batches.iter().map(|b| b.column(c).as_ref()).collect();
                arrow::compute::interleave(&sources, &picks)
            })
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(arrow_err)?;
        self.set(vec![RecordBatch::try_new(schema, cols).map_err(arrow_err)?])?;
        Ok(())
    }

    /// The rows in logical order, materialized (for a step that needs them as
    /// batches, such as attaching graph columns by id). The ordinal stays.
    pub fn ordered_batches(&mut self) -> Result<Vec<RecordBatch>> {
        self.reorder()?;
        let batches = self.run()?;
        self.set(batches.clone())?;
        Ok(batches)
    }

    /// Replace the rows with `batches` (the same rows, in order, with columns
    /// added), keeping the ordinal if they carry it.
    pub fn set_batches(&mut self, batches: Vec<RecordBatch>) -> Result<()> {
        self.set(batches)?;
        self.in_order = true;
        Ok(())
    }

    /// Execute: the rows in logical order, ordinal dropped, as a batch list that
    /// is never empty (an empty result keeps its schema).
    pub fn collect(mut self) -> Result<Vec<RecordBatch>> {
        self.reorder()?;
        self.drop_ordinal()?;
        let batches = self.run()?;
        relax_nullability(batches, &self.nullable)
    }
}

/// `batches` with the columns named in `nullable` marked nullable.
fn relax_nullability(
    batches: Vec<RecordBatch>,
    nullable: &BTreeSet<String>,
) -> Result<Vec<RecordBatch>> {
    if nullable.is_empty() {
        return Ok(batches);
    }
    let schema = batches[0].schema();
    let fields: Vec<Field> = schema
        .fields()
        .iter()
        .map(|f| {
            let f = f.as_ref().clone();
            if nullable.contains(f.name()) {
                f.with_nullable(true)
            } else {
                f
            }
        })
        .collect();
    let schema = Arc::new(Schema::new_with_metadata(fields, schema.metadata().clone()));
    batches
        .into_iter()
        .map(|b| RecordBatch::try_new(schema.clone(), b.columns().to_vec()).map_err(arrow_err))
        .collect()
}

/// A join's `how`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum How {
    Inner,
    Left,
    Right,
    /// `outer` (`full`, Polars' name for it).
    Outer,
    /// The left rows that have a match, once each, with the left columns only.
    Semi,
    /// The left rows that have no match, with the left columns only.
    Anti,
}

impl How {
    /// The `how` a join step names, if it is one.
    pub fn parse(how: &str) -> Option<How> {
        Some(match how {
            "inner" => How::Inner,
            "left" => How::Left,
            "right" => How::Right,
            "outer" | "full" => How::Outer,
            "semi" => How::Semi,
            "anti" => How::Anti,
            _ => return None,
        })
    }

    /// Whether the output has the right frame's columns.
    pub fn keeps_right(self) -> bool {
        !matches!(self, How::Semi | How::Anti)
    }

    /// Whether a row can come from the right frame alone, so a key takes the
    /// right key's value where there is no left one.
    fn right_rows(self) -> bool {
        matches!(self, How::Right | How::Outer)
    }
}

/// Each side's position takes 32 bits of a joined row's ordinal.
const SPAN: u64 = 1 << 32;

/// The position of a side a joined row lacks: after every row's.
const ABSENT: u64 = SPAN - 1;

impl Frame {
    /// This frame as a join input: its rows in logical order, numbered `0..` by
    /// the ordinal `ord` (an ordinal the frame carries is dropped: it need not
    /// be dense), read as a plan the join spreads over the partitions. Columns
    /// the frame reports nullable are nullable in the rows, so what the join (and
    /// any expression over its output) infers matches.
    ///
    /// The rows are run first: numbering a side inside the join's plan (an
    /// operator appending positions to one ordered partition) measured slower
    /// than numbering its collected rows, which DataFusion then reads as several
    /// partitions.
    fn join_input(mut self, ord: &str) -> Result<DataFrame> {
        self.materialize()?;
        let batches = self.batches.take().expect("materialized");
        let rows: usize = batches.iter().map(|b| b.num_rows()).sum();
        if rows as u64 >= ABSENT {
            return Err(too_many_rows());
        }
        self.ctx.read_batches(attach_ordinal(batches, ord)?)
    }
}

impl Frame {
    /// Run the frame to its rows, in logical order, its ordinal dropped and the
    /// columns it reports nullable marked so (what `collect` returns, kept as
    /// the frame's rows).
    pub fn materialize(&mut self) -> Result<()> {
        self.reorder()?;
        self.drop_ordinal()?;
        let batches = relax_nullability(self.run()?, &self.nullable)?;
        self.set(batches)
    }
}

/// For a join whose keys can take the right key's value (`right`, `outer`): the
/// first key whose values in `right` the left key's type cannot hold, with the
/// two types. Each key keeps the left key's type in the output.
pub fn unfit_right_key(
    left: &Frame,
    right: &mut Frame,
    on: &[String],
) -> Result<Option<(String, DataType, DataType)>> {
    use arrow::compute::kernels::cmp::distinct;
    use arrow::compute::{cast_with_options, CastOptions};
    let (ls, rs) = (left.schema(), right.schema());
    let batches = right.run()?;
    for k in on {
        let lt = ls
            .field_with_name(k)
            .map_err(arrow_err)?
            .data_type()
            .clone();
        let rt = rs
            .field_with_name(k)
            .map_err(arrow_err)?
            .data_type()
            .clone();
        if lt == rt {
            continue;
        }
        // A value fits if it survives the cast to the left type and back.
        let safe = CastOptions {
            safe: true,
            ..Default::default()
        };
        for b in &batches {
            let values = b.column(b.schema().index_of(k).map_err(arrow_err)?);
            let fits = cast_with_options(values, &lt, &safe)
                .and_then(|as_left| cast_with_options(&as_left, &rt, &safe))
                .and_then(|back| distinct(&back, values))
                .map(|d| d.true_count() == 0)
                .unwrap_or(false);
            if !fits {
                return Ok(Some((k.clone(), lt, rt)));
            }
        }
    }
    Ok(None)
}

fn too_many_rows() -> DataFusionError {
    DataFusionError::NotImplemented(format!("a join input of more than {} rows", ABSENT - 1))
}

/// `left.join(right, on=, how=)`: an equi-join on the key columns `on`, which
/// both frames have (checked by the caller, with the column collisions). The
/// output has the left frame's columns, then the right frame's other columns
/// (none for `semi` and `anti`); each key appears once, holding the left key, or
/// the right key on a row with no left match. A null key matches nothing. The
/// rows come out in the order the module docs give.
pub fn join(left: Frame, right: Frame, on: &[String], how: How) -> Result<Frame> {
    if on.is_empty() {
        return Err(DataFusionError::Execution(
            "join() needs at least one key column (on=)".into(),
        ));
    }
    let ctx = left.ctx.clone();
    let (left_names, right_names) = (left.names(), right.names());
    let taken = Schema::new(
        left_names
            .iter()
            .chain(right_names.iter())
            .map(|n| Field::new(n, DataType::Null, true))
            .collect::<Vec<_>>(),
    );
    let l = fresh_name_with(&taken, "_l");
    let r = fresh_name_with(&taken, "_r");
    let combined = fresh_name(&taken);
    // Each right key under a private name, so the join keys don't collide; it is
    // dropped after the join (or read where a row has no left key).
    let sentinels: Vec<String> = (0..on.len())
        .map(|i| fresh_name_with(&taken, &format!("_rk{i}")))
        .collect();
    let left_types: Vec<DataType> = on
        .iter()
        .map(|k| Ok(left.schema().field_with_name(k)?.data_type().clone()))
        .collect::<std::result::Result<_, arrow::error::ArrowError>>()
        .map_err(arrow_err)?;
    let sentinel = |c: &str| on.iter().position(|k| k == c).map(|i| &sentinels[i]);

    let left_df = left.join_input(&l)?;
    let right_df = if how.keeps_right() {
        let df = right.join_input(&r)?;
        let mut cols: Vec<Expr> = right_names
            .iter()
            .map(|c| match sentinel(c) {
                Some(s) => col_named(c).alias(s),
                None => col_named(c),
            })
            .collect();
        cols.push(col_named(&r));
        df.select(cols)?
    } else {
        // Only whether a row has a match is read: the keys, in any order.
        let mut right = right;
        let keys: Vec<Expr> = on
            .iter()
            .zip(&sentinels)
            .map(|(k, s)| col_named(k).alias(s))
            .collect();
        right.plan()?.select(keys)?
    };
    let join_type = match how {
        How::Inner => JoinType::Inner,
        How::Left => JoinType::Left,
        How::Right => JoinType::Right,
        How::Outer => JoinType::Full,
        How::Semi => JoinType::LeftSemi,
        How::Anti => JoinType::LeftAnti,
    };
    let left_keys: Vec<&str> = on.iter().map(String::as_str).collect();
    let right_keys: Vec<&str> = sentinels.iter().map(String::as_str).collect();
    let joined = left_df.join(right_df, join_type, &left_keys, &right_keys, None)?;

    let mut out: Vec<Expr> = left_names
        .iter()
        .map(|c| match sentinel(c) {
            // The left key's value, or the right key's, as the left key's type
            // (the caller checked that the right key's values fit it).
            Some(s) if how.right_rows() => {
                let i = on.iter().position(|k| k == c).expect("a key");
                let right_key = Expr::Cast(datafusion::logical_expr::Cast::new(
                    Box::new(col_named(s)),
                    left_types[i].clone(),
                ));
                coalesce(vec![col_named(c), right_key]).alias(c.as_str())
            }
            _ => col_named(c),
        })
        .collect();
    if how.keeps_right() {
        out.extend(
            right_names
                .iter()
                .filter(|c| sentinel(c).is_none())
                .map(|c| col_named(c)),
        );
    }
    // One ordinal per joined row, the two positions folded into 64 bits so it
    // orders like the pair (an absent side last).
    let pos = |c: &str| col_named(c);
    let or_absent = |c: &str| coalesce(vec![col_named(c), lit(ABSENT)]);
    let ord = match how {
        How::Inner | How::Left => pos(&l) * lit(SPAN) + or_absent(&r),
        How::Right => pos(&r) * lit(SPAN) + or_absent(&l),
        How::Outer => or_absent(&l) * lit(SPAN) + or_absent(&r),
        How::Semi | How::Anti => pos(&l),
    };
    out.push(ord.alias(&combined));
    let df = joined.select(out)?;
    Ok(Frame::from_numbered(ctx, df, combined))
}

/// The first `n` rows of `batches` (zero-copy), keeping at least one batch so the
/// schema survives.
fn take_prefix(batches: Vec<RecordBatch>, n: usize) -> Vec<RecordBatch> {
    let mut left = n;
    let mut out = Vec::new();
    for b in batches {
        if left == 0 && !out.is_empty() {
            break;
        }
        let take = left.min(b.num_rows());
        out.push(b.slice(0, take));
        left -= take;
    }
    out
}

/// `GROUP BY groups` computing `aggs`, and only those columns: DataFusion adds
/// the columns a group key determines to the keys (the ordinal of an aggregate
/// below this one, say), and they are not this step's output.
fn aggregate(df: DataFrame, groups: Vec<Expr>, aggs: Vec<Expr>) -> Result<DataFrame> {
    let names: Vec<String> = groups.iter().chain(aggs.iter()).map(output_name).collect();
    let df = df.aggregate(groups, aggs)?;
    if df.schema().fields().len() == names.len() {
        return Ok(df);
    }
    let keep: Vec<Expr> = df
        .schema()
        .iter()
        .filter(|(_, f)| names.contains(f.name()))
        .map(|(q, f)| Expr::Column(Column::from((q, f))))
        .collect();
    df.select(keep)
}

/// The name an expression's output column takes.
pub fn output_name(e: &Expr) -> String {
    match e {
        Expr::Alias(a) => a.name.clone(),
        Expr::Column(c) => c.name.clone(),
        e => e.schema_name().to_string(),
    }
}

/// A hidden column name no column of `schema` uses.
pub fn fresh_name(schema: &Schema) -> String {
    fresh_name_with(schema, "")
}

/// [`fresh_name`] with a suffix.
pub fn fresh_name_with(schema: &Schema, suffix: &str) -> String {
    let mut name = format!("{ORD}{suffix}");
    while schema.fields().iter().any(|f| f.name() == &name) {
        name.push('_');
    }
    name
}

/// Append a `UInt64` column `name` numbering the rows `0..` across the batches,
/// cut into zero-copy slices of at most [`CHUNK`] rows.
pub fn attach_ordinal(batches: Vec<RecordBatch>, name: &str) -> Result<Vec<RecordBatch>> {
    let mut next = 0u64;
    let schema = batches[0].schema();
    let mut fields = schema.fields().to_vec();
    fields.push(Arc::new(Field::new(name, DataType::UInt64, false)));
    let out_schema = Arc::new(Schema::new_with_metadata(fields, schema.metadata().clone()));
    let mut out = Vec::with_capacity(batches.len());
    for b in batches {
        let n = b.num_rows();
        let starts: Vec<usize> = if n == 0 {
            vec![0]
        } else {
            (0..n).step_by(CHUNK).collect()
        };
        for s in starts {
            let part = b.slice(s, CHUNK.min(n - s));
            let len = part.num_rows() as u64;
            let ord: ArrayRef = Arc::new(UInt64Array::from_iter_values(next..next + len));
            next += len;
            let mut cols = part.columns().to_vec();
            cols.push(ord);
            out.push(RecordBatch::try_new(out_schema.clone(), cols).map_err(arrow_err)?);
        }
    }
    Ok(out)
}

/// `batches` (at least one) as a single partition the engine cannot split, so a
/// step that keeps order (a filter, a projection, a limit) keeps their order.
pub fn read_ordered(ctx: &SessionContext, batches: Vec<RecordBatch>) -> Result<DataFrame> {
    use datafusion::catalog::streaming::StreamingTable;
    use datafusion::execution::TaskContext;
    use datafusion::physical_plan::stream::RecordBatchStreamAdapter;
    use datafusion::physical_plan::streaming::PartitionStream;
    use datafusion::physical_plan::SendableRecordBatchStream;

    #[derive(Debug)]
    struct Ordered {
        schema: SchemaRef,
        batches: Vec<RecordBatch>,
    }
    impl PartitionStream for Ordered {
        fn schema(&self) -> &SchemaRef {
            &self.schema
        }
        fn execute(&self, _ctx: Arc<TaskContext>) -> SendableRecordBatchStream {
            let batches = self.batches.clone().into_iter().map(Ok);
            Box::pin(RecordBatchStreamAdapter::new(
                self.schema.clone(),
                futures::stream::iter(batches),
            ))
        }
    }
    let schema = batches[0].schema();
    let part = Arc::new(Ordered {
        schema: schema.clone(),
        batches,
    });
    ctx.read_table(Arc::new(StreamingTable::try_new(schema, vec![part])?))
}

/// Order `batches` by their unique, non-null `UInt64` ordinal `ord` (kept), as
/// one batch. Input already in order is passed through without a gather.
fn sort_by_ordinal(batches: &[RecordBatch], ord: &str) -> Result<Vec<RecordBatch>> {
    use arrow::array::UInt32Array;
    use rayon::prelude::*;
    let schema = batches[0].schema();
    let keep: Vec<usize> = (0..schema.fields().len()).collect();
    let r: usize = batches.iter().map(|b| b.num_rows()).sum();
    let ord_i = schema.index_of(ord).map_err(arrow_err)?;
    let mut keys: Vec<(u64, u64)> = Vec::with_capacity(r);
    let mut pos = 0u64;
    for b in batches {
        let ords = b
            .column(ord_i)
            .as_any()
            .downcast_ref::<UInt64Array>()
            .ok_or_else(|| DataFusionError::Internal("the ordinal is not u64".into()))?;
        keys.extend(ords.values().iter().zip(pos..).map(|(&o, p)| (o, p)));
        pos += b.num_rows() as u64;
    }
    // Count the ascending runs: one is input already in order; a few long ones
    // (a hash aggregate or join emits each partition in order) merge in
    // near-linear time with the adaptive merge sort; many short ones sort faster
    // unstable (the keys are unique, so the result is the same).
    let runs = 1 + keys.windows(2).filter(|w| w[0] > w[1]).count();
    let indices = if runs == 1 {
        None
    } else {
        if runs <= keys.len() / 1024 {
            keys.par_sort();
        } else {
            keys.par_sort_unstable();
        }
        if r > u32::MAX as usize {
            return Err(DataFusionError::NotImplemented(
                "ordering more than 2^32 rows".into(),
            ));
        }
        Some(UInt32Array::from_iter_values(
            keys.iter().map(|&(_, p)| p as u32),
        ))
    };
    drop(keys);
    emit_in_order(batches, schema, &keep, indices.as_ref())
}

/// The `keep` columns of `batches` as one batch, rows gathered by `indices`
/// (positions across the batches), or in their current order for `None`.
fn emit_in_order(
    batches: &[RecordBatch],
    schema: SchemaRef,
    keep: &[usize],
    indices: Option<&arrow::array::UInt32Array>,
) -> Result<Vec<RecordBatch>> {
    use arrow::compute::{concat, take};
    use rayon::prelude::*;
    let out_schema = Arc::new(schema.project(keep).map_err(arrow_err)?);
    if batches.iter().all(|b| b.num_rows() == 0) {
        return Ok(vec![RecordBatch::new_empty(out_schema)]);
    }
    let Some(indices) = indices else {
        let parts: Vec<RecordBatch> = batches
            .iter()
            .map(|b| b.project(keep))
            .collect::<std::result::Result<_, _>>()
            .map_err(arrow_err)?;
        return Ok(parts);
    };
    let cols = keep
        .par_iter()
        .map(|&i| {
            let parts: Vec<&dyn Array> = batches.iter().map(|b| b.column(i).as_ref()).collect();
            let c = concat(&parts).map_err(arrow_err)?;
            take(c.as_ref(), indices, None).map_err(arrow_err)
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(vec![
        RecordBatch::try_new(out_schema, cols).map_err(arrow_err)?
    ])
}

/// Whether [`normalized_key`] covers a key of this type.
fn is_fast_key(dt: &DataType) -> bool {
    dt.is_integer() || dt.is_floating() || *dt == DataType::Boolean
}

/// An order-preserving `u64` image of a primitive sort key: `a < b` in the sort's
/// order exactly when `norm(a) < norm(b)`. Integers flip the sign bit; floats are
/// made canonical (every NaN the positive NaN, `-0.0` as `0.0`, #201) and their
/// bits mapped to total order; `descending` inverts the image. Nulls are handled
/// by the caller.
fn normalized_key(array: &dyn Array, descending: bool) -> Option<Vec<u64>> {
    use arrow::array::BooleanArray;
    let dt = array.data_type();
    let keys: Vec<u64> = if dt.is_signed_integer() {
        let a = cast(array, &DataType::Int64).ok()?;
        let a = a.as_any().downcast_ref::<Int64Array>()?;
        a.values().iter().map(|&v| (v as u64) ^ (1 << 63)).collect()
    } else if dt.is_unsigned_integer() {
        let a = cast(array, &DataType::UInt64).ok()?;
        a.as_any().downcast_ref::<UInt64Array>()?.values().to_vec()
    } else if dt.is_floating() {
        let a = cast(array, &DataType::Float64).ok()?;
        let a = a.as_any().downcast_ref::<Float64Array>()?;
        a.values()
            .iter()
            .map(|&v| {
                let v = if v.is_nan() { f64::NAN } else { v + 0.0 };
                let b = v.to_bits();
                if b >> 63 == 1 {
                    !b
                } else {
                    b | (1 << 63)
                }
            })
            .collect()
    } else if *dt == DataType::Boolean {
        let a = array.as_any().downcast_ref::<BooleanArray>()?;
        (0..a.len()).map(|i| a.value(i) as u64).collect()
    } else {
        return None;
    };
    Some(if descending {
        keys.into_iter().map(|k| !k).collect()
    } else {
        keys
    })
}

/// Sort batches by one primitive key (nulls last), then by the ordinal `ord`,
/// and drop `ord`: the hot case of the stable sort (#165), a full sort by one
/// numeric column, often tie-heavy. DataFusion sorts a single primitive column
/// with a fast kernel but falls back to a generic lexicographic sort once the
/// ordinal is a second key; sorting `(key, ordinal)` integer pairs here keeps it
/// close to the single-key cost. Input already in order passes through.
fn sort_by_key_then_ordinal(
    batches: &[RecordBatch],
    key: &str,
    descending: bool,
    ord: &str,
) -> Result<Vec<RecordBatch>> {
    use arrow::array::UInt32Array;
    use rayon::prelude::*;

    let schema = batches[0].schema();
    let keep: Vec<usize> = (0..schema.fields().len())
        .filter(|&i| schema.field(i).name() != ord)
        .collect();
    let r: usize = batches.iter().map(|b| b.num_rows()).sum();
    if r > u32::MAX as usize {
        return Err(DataFusionError::NotImplemented(
            "sorting more than 2^32 rows".into(),
        ));
    }
    let key_i = schema.index_of(key).map_err(arrow_err)?;
    let ord_i = schema.index_of(ord).map_err(arrow_err)?;

    // Valid keys sort by (key, ordinal); the null keys all tie, so they follow in
    // ordinal order.
    let mut valid: Vec<(u64, u64, u32)> = Vec::with_capacity(r);
    let mut nulls: Vec<(u64, u32)> = Vec::new();
    let mut pos = 0u32;
    for b in batches {
        let key_col = b.column(key_i);
        let norm = normalized_key(key_col.as_ref(), descending)
            .ok_or_else(|| DataFusionError::Internal("not a fast sort key".into()))?;
        let ords = b
            .column(ord_i)
            .as_any()
            .downcast_ref::<UInt64Array>()
            .ok_or_else(|| DataFusionError::Internal("the ordinal is not u64".into()))?
            .values();
        match key_col.nulls().filter(|n| n.null_count() > 0) {
            None => valid.extend(
                norm.iter()
                    .zip(ords.iter())
                    .zip(pos..)
                    .map(|((&k, &o), p)| (k, o, p)),
            ),
            Some(mask) => {
                for (i, (&k, &o)) in norm.iter().zip(ords.iter()).enumerate() {
                    let p = pos + i as u32;
                    if mask.is_null(i) {
                        nulls.push((o, p));
                    } else {
                        valid.push((k, o, p));
                    }
                }
            }
        }
        pos += b.num_rows() as u32;
    }
    if nulls.is_empty() && valid.windows(2).all(|w| w[0] <= w[1]) {
        // Already in order: no sort or gather.
        return emit_in_order(batches, schema, &keep, None);
    }
    valid.par_sort_unstable();
    nulls.par_sort_unstable();
    let indices = UInt32Array::from_iter_values(
        valid
            .iter()
            .map(|&(_, _, p)| p)
            .chain(nulls.iter().map(|&(_, p)| p)),
    );
    drop((valid, nulls));
    emit_in_order(batches, schema, &keep, Some(&indices))
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow::array::Int64Array;

    fn ints(v: Vec<i64>) -> Vec<RecordBatch> {
        let schema = Arc::new(Schema::new(vec![Field::new("v", DataType::Int64, true)]));
        vec![RecordBatch::try_new(schema, vec![Arc::new(Int64Array::from(v))]).unwrap()]
    }

    #[test]
    fn distinct_twice() {
        let mut f = Frame::from_batches(session(), ints(vec![3, 1, 3, 2, 1])).unwrap();
        f.distinct().unwrap();
        f.distinct().unwrap();
        let out = f.collect().unwrap();
        let rows: Vec<i64> = out
            .iter()
            .flat_map(|b| {
                assert_eq!(b.num_columns(), 1, "{:?}", b.schema());
                let v = b.column(0).as_any().downcast_ref::<Int64Array>().unwrap();
                v.values().to_vec()
            })
            .collect();
        assert_eq!(rows, vec![3, 1, 2]);
    }

    type Row = (Option<i64>, Option<i64>, Option<i64>);

    /// A nested-loop join of `(key, value)` rows, in the order the join
    /// guarantees: `(key, left value, right value)` per output row.
    fn nested_loop(
        left: &[(Option<i64>, i64)],
        right: &[(Option<i64>, i64)],
        how: How,
    ) -> Vec<Row> {
        let eq = |a: Option<i64>, b: Option<i64>| a.is_some() && a == b;
        let mut out = vec![];
        if how == How::Right {
            for &(k, rv) in right {
                let m: Vec<_> = left.iter().filter(|(lk, _)| eq(*lk, k)).collect();
                if m.is_empty() {
                    out.push((k, None, Some(rv)));
                }
                out.extend(m.iter().map(|&&(_, lv)| (k, Some(lv), Some(rv))));
            }
            return out;
        }
        for &(k, lv) in left {
            let m: Vec<_> = right.iter().filter(|(rk, _)| eq(*rk, k)).collect();
            match how {
                How::Semi if !m.is_empty() => out.push((k, Some(lv), None)),
                How::Anti if m.is_empty() => out.push((k, Some(lv), None)),
                How::Semi | How::Anti => {}
                _ => {
                    if m.is_empty() && how != How::Inner {
                        out.push((k, Some(lv), None));
                    }
                    out.extend(m.iter().map(|&&(_, rv)| (k, Some(lv), Some(rv))));
                }
            }
        }
        if how == How::Outer {
            for &(k, rv) in right {
                if !left.iter().any(|(lk, _)| eq(*lk, k)) {
                    out.push((k, None, Some(rv)));
                }
            }
        }
        out
    }

    /// `(key, value)` rows as batches of `name_k`, `name_v`, 97 rows each.
    fn keyed(rows: &[(Option<i64>, i64)], value: &str) -> Vec<RecordBatch> {
        let schema = Arc::new(Schema::new(vec![
            Field::new("k", DataType::Int64, true),
            Field::new(value, DataType::Int64, false),
        ]));
        rows.chunks(97)
            .map(|c| {
                let k = Int64Array::from(c.iter().map(|r| r.0).collect::<Vec<_>>());
                let v = Int64Array::from(c.iter().map(|r| r.1).collect::<Vec<_>>());
                RecordBatch::try_new(schema.clone(), vec![Arc::new(k), Arc::new(v)]).unwrap()
            })
            .collect()
    }

    fn joined_rows(batches: &[RecordBatch], how: How) -> Vec<Row> {
        let mut out = vec![];
        for b in batches {
            let i64s = |name: &str| -> Option<Int64Array> {
                let i = b.schema().index_of(name).ok()?;
                Some(b.column(i).as_any().downcast_ref::<Int64Array>()?.clone())
            };
            let (k, l, r) = (i64s("k").unwrap(), i64s("a").unwrap(), i64s("b"));
            assert_eq!(r.is_some(), how.keeps_right(), "{:?}", b.schema());
            for i in 0..b.num_rows() {
                let get = |a: &Int64Array| a.is_valid(i).then(|| a.value(i));
                out.push((get(&k), get(&l), r.as_ref().and_then(get)));
            }
        }
        out
    }

    #[test]
    fn joins_follow_their_order_for_any_partition_count() {
        // Keys with duplicates on both sides, nulls, and keys on one side only.
        let key = |i: i64, m: i64| (i % 11 != 0).then_some((i * 7) % m);
        let left: Vec<_> = (0..1500).map(|i| (key(i, 400), i)).collect();
        let right: Vec<_> = (0..1200).map(|i| (key(i + 3, 523), 10_000 + i)).collect();
        let hows = [
            How::Inner,
            How::Left,
            How::Right,
            How::Outer,
            How::Semi,
            How::Anti,
        ];
        for p in [1, 3, 8] {
            TEST_PARTITIONS.with(|c| c.set(Some(p)));
            for how in hows {
                let want = nested_loop(&left, &right, how);
                // Each side materialized, or still a plan (run when joined).
                for lazy in [false, true] {
                    let ctx = session();
                    let side = |rows: &[(Option<i64>, i64)], v: &str| {
                        let b = keyed(rows, v);
                        if lazy {
                            Frame::from_ordered(ctx.clone(), read_ordered(&ctx, b).unwrap())
                        } else {
                            Frame::from_batches(ctx.clone(), b).unwrap()
                        }
                    };
                    let (l, r) = (side(&left, "a"), side(&right, "b"));
                    let j = join(l, r, &["k".to_string()], how).unwrap();
                    let got = joined_rows(&j.collect().unwrap(), how);
                    assert_eq!(got, want, "p={p} how={how:?} lazy={lazy}");
                }
            }
            TEST_PARTITIONS.with(|c| c.set(None));
        }
    }

    #[test]
    fn ordered_float_sums_match_a_sequential_fold_for_any_partition_count() {
        use arrow::array::Float64Array;
        // Values whose sum depends on the order they are added in.
        let n = 50_000usize;
        let k: Vec<i64> = (0..n).map(|i| (i * 7 % 13) as i64).collect();
        let v: Vec<f64> = (0..n)
            .map(|i| match i % 4 {
                0 => 1e16,
                1 => 1.0,
                2 => -1e16,
                _ => 0.1 * i as f64,
            })
            .collect();
        let mut want = vec![0.0f64; 13];
        for (g, x) in k.iter().zip(&v) {
            want[*g as usize] += x;
        }
        let schema = Arc::new(Schema::new(vec![
            Field::new("k", DataType::Int64, false),
            Field::new("v", DataType::Float64, false),
        ]));
        let batches: Vec<RecordBatch> = (0..n)
            .step_by(4096)
            .map(|s| {
                let e = (s + 4096).min(n);
                RecordBatch::try_new(
                    schema.clone(),
                    vec![
                        Arc::new(Int64Array::from(k[s..e].to_vec())),
                        Arc::new(Float64Array::from(v[s..e].to_vec())),
                    ],
                )
                .unwrap()
            })
            .collect();
        for p in [1, 3, 8] {
            TEST_PARTITIONS.with(|c| c.set(Some(p)));
            let mut f = Frame::from_batches(session(), batches.clone()).unwrap();
            let sum = crate::agg::float_agg(
                crate::agg::FloatAgg::Sum,
                &DataType::Float64,
                false,
                col_named("v"),
            );
            f.group_by(vec![col_named("k")], vec![sum.alias("s")], true)
                .unwrap();
            let out = f.collect().unwrap();
            TEST_PARTITIONS.with(|c| c.set(None));
            let mut got = vec![f64::NAN; 13];
            for b in &out {
                let ks = b.column(0).as_any().downcast_ref::<Int64Array>().unwrap();
                let ss = b.column(1).as_any().downcast_ref::<Float64Array>().unwrap();
                for r in 0..b.num_rows() {
                    got[ks.value(r) as usize] = ss.value(r);
                }
            }
            let bits = |x: &[f64]| x.iter().map(|f| f.to_bits()).collect::<Vec<_>>();
            assert_eq!(bits(&got), bits(&want), "p={p}");
        }
    }
}
