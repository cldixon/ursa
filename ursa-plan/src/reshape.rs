//! Graph ops over reshaped edge sets (#191): the topology of an edge plan whose
//! rows a step reshapes (`distinct`, `sample`, `join`, `group_by().agg()`,
//! `sort`, `head`), and the cache that keeps it.
//!
//! A reshaped edge set has no frame of its own to cache an index on: its rows
//! are the output of a subplan. Its [`GraphIndex`] is built from those rows, as
//! a fresh frame over them would be (`ur.from_arrow(rows, src=, dst=)`), and
//! kept in one process-wide cache keyed by a fingerprint of the subplan
//! ([`fingerprint`]): the subplan as written, with each input it reads
//! replaced by that input's identity. Repeated collects over the same reshaped
//! edges then reuse the index, and with it the weight, mask and symmetric-view
//! caches it carries, as a source frame's index does (#179).
//!
//! The fingerprint never lets two different subplans share an entry: it holds
//! every argument of every step, an in-memory table's identity (a token its
//! host frame never reuses), a local scan's files with their sizes and
//! modification times (as the footer cache revalidates them, #183), and the
//! content of each seed set. A subplan with an input that has no such identity
//! (a remote scan, a step the host could not serialize) is not cached: its
//! index is built for the one query.

use std::sync::{Arc, LazyLock, Mutex, PoisonError};

use arrow::array::{Array, ArrayRef, RecordBatch};
use arrow::compute::cast;
use arrow::datatypes::DataType;

use crate::expr::GraphVerb;
use crate::plan::{
    array_digest, count_topology_build, EdgeInput, Expr, GraphIndex, Inputs, Plan, PlanError,
    PlanResult, ScanPath, Scratch,
};

/// How many reshaped-topology indexes the cache keeps, least recently used
/// evicted. Each is a whole graph index, so the bound is small.
pub const TOPOLOGY_CACHE_CAPACITY: usize = 4;

/// One cached index, built once by whoever needs it first.
type IndexSlot = Arc<Mutex<Option<GraphIndex>>>;

/// Reshaped-topology indexes by subplan fingerprint, most recently used last.
static CACHE: LazyLock<Mutex<Vec<(String, IndexSlot)>>> = LazyLock::new(Mutex::default);

/// The cached index for `key`, if one is built (never builds one).
pub fn lookup(key: &str) -> Option<GraphIndex> {
    let slots = CACHE.lock().unwrap_or_else(PoisonError::into_inner);
    let slot = slots.iter().find(|(k, _)| k == key)?.1.clone();
    drop(slots);
    let index = slot.lock().unwrap_or_else(PoisonError::into_inner);
    index.clone()
}

/// The index for `key`, built by `build` on a miss (`None`: an uncacheable
/// subplan, built every time). Concurrent queries over one subplan build it
/// once: each entry is locked while its index is built.
pub fn cached_index(
    key: Option<&str>,
    build: impl FnOnce() -> PlanResult<GraphIndex>,
) -> PlanResult<GraphIndex> {
    let Some(key) = key else {
        return build();
    };
    let slot = {
        let mut slots = CACHE.lock().unwrap_or_else(PoisonError::into_inner);
        let slot = match slots.iter().position(|(k, _)| k == key) {
            Some(i) => slots.remove(i).1,
            None => Arc::default(),
        };
        if slots.len() >= TOPOLOGY_CACHE_CAPACITY {
            slots.remove(0);
        }
        slots.push((key.to_string(), slot.clone()));
        slot
    };
    let mut index = slot.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(i) = index.as_ref() {
        return Ok(i.clone());
    }
    let built = build()?;
    *index = Some(built.clone());
    Ok(built)
}

/// How many indexes the cache holds (test/observability hook).
pub fn cached_count() -> usize {
    CACHE.lock().unwrap_or_else(PoisonError::into_inner).len()
}

/// Build the index of a reshaped edge set from its rows, as a fresh frame over
/// them builds it: the `src` / `dst` columns (`roles`) canonicalized like
/// `ur.from_arrow`'s (any integer type to `int64`, a dictionary decoded, strings
/// as they are), then interned in row order.
pub fn build_index(rows: &[RecordBatch], roles: &(String, String)) -> PlanResult<GraphIndex> {
    let mut pairs: Vec<(ArrayRef, ArrayRef)> = Vec::with_capacity(rows.len());
    for b in rows {
        let column = |name: &str| -> PlanResult<ArrayRef> {
            let i = b.schema().index_of(name).map_err(|_| {
                PlanError::ColumnNotFound(format!("the reshaped edge rows have no column {name:?}"))
            })?;
            canonical_ids(b.column(i))
        };
        pairs.push((column(&roles.0)?, column(&roles.1)?));
    }
    let refs: Vec<(&dyn Array, &dyn Array)> = pairs
        .iter()
        .map(|(s, d)| (s.as_ref(), d.as_ref()))
        .collect();
    let (topo, ids) = crate::topology::build_topology_batches(&refs)
        .map_err(|e| PlanError::Value(e.to_string()))?;
    count_topology_build();
    Ok(GraphIndex::new(topo, ids))
}

/// A node-id column as the topology build takes it (`_canonical_id_array`).
fn canonical_ids(a: &ArrayRef) -> PlanResult<ArrayRef> {
    let to = |a: &ArrayRef, t: &DataType| -> PlanResult<ArrayRef> { Ok(cast(a, t)?) };
    match a.data_type() {
        t if t.is_integer() => to(a, &DataType::Int64),
        DataType::Utf8 | DataType::LargeUtf8 | DataType::Utf8View => Ok(a.clone()),
        DataType::Dictionary(_, value)
            if value.is_integer()
                || matches!(
                    value.as_ref(),
                    DataType::Utf8 | DataType::LargeUtf8 | DataType::Utf8View
                ) =>
        {
            canonical_ids(&to(a, value)?)
        }
        other => Err(PlanError::Type(format!(
            "node ids must be an integer or string column; got {other}"
        ))),
    }
}

/// The fingerprint of a reshaped topology: the subplan `plan` with its inputs'
/// identities, and the columns that play `src` / `dst`. `None` when an input
/// has no identity (the index is then not cached). See the module docs.
pub fn fingerprint(
    plan: &Plan,
    roles: &(String, String),
    inputs: &dyn Inputs,
    scratch: &mut Scratch,
) -> PlanResult<Option<String>> {
    let mut fp = Fingerprint {
        out: format!("roles {:?} {:?};", roles.0, roles.1),
        ok: true,
        inputs,
        scratch,
    };
    fp.plan(plan)?;
    Ok(fp.ok.then_some(fp.out))
}

struct Fingerprint<'a> {
    out: String,
    /// Whether every input has an identity.
    ok: bool,
    inputs: &'a dyn Inputs,
    scratch: &'a mut Scratch,
}

impl Fingerprint<'_> {
    fn push(&mut self, s: impl AsRef<str>) {
        self.out.push_str(s.as_ref());
        self.out.push(';');
    }

    /// The content of a seed set.
    fn seeds(&mut self, slot: usize) -> PlanResult<()> {
        let set = self.scratch.seed_set(slot, self.inputs)?;
        let digest = array_digest(set.as_ref());
        self.push(format!("seeds {digest}"));
        Ok(())
    }

    fn scan(&mut self, frame: usize, path: &Option<ScanPath>) -> PlanResult<()> {
        let identity = match path {
            Some(ScanPath::One(p)) => {
                let opts = self.inputs.storage_options(frame)?;
                crate::scan::scan_identity(p, &opts)
            }
            _ => None,
        };
        match identity {
            Some(id) => self.push(format!("files {id}")),
            None => self.ok = false,
        }
        Ok(())
    }

    fn edges(&mut self, edges: &EdgeInput) -> PlanResult<()> {
        self.push("edges [");
        self.plan(&edges.plan)?;
        self.push("]");
        Ok(())
    }

    fn verb(&mut self, verb: &GraphVerb) -> PlanResult<()> {
        let mut args = verb.clone();
        args.edges = None;
        self.push(format!("verb {args:?}"));
        match &verb.edges {
            Some(e) => self.edges(e),
            None => Ok(()),
        }
    }

    fn plan(&mut self, plan: &Plan) -> PlanResult<()> {
        if plan.invalid() {
            // A step the host could not serialize: the plan raises its error.
            self.ok = false;
            return Ok(());
        }
        if let Some(input) = plan.input() {
            self.plan(input)?;
        }
        let op = plan.op();
        match plan {
            Plan::ScanEdges {
                frame,
                path,
                src,
                dst,
                on_null,
            } => {
                self.push(format!("{op} src={src:?} dst={dst:?} on_null={on_null:?}"));
                self.scan(*frame, path)?;
            }
            Plan::ScanNodes { frame, path, id } => {
                self.push(format!("{op} id={id:?}"));
                self.scan(*frame, path)?;
            }
            Plan::FromArrow {
                frame,
                src,
                dst,
                id,
            } => {
                self.push(format!("{op} src={src:?} dst={dst:?} id={id:?}"));
                match self.inputs.identity(*frame)? {
                    Some(token) => self.push(format!("table {token}")),
                    None => self.ok = false,
                }
            }
            Plan::Filter { predicate, .. } => match predicate {
                Expr::Scalar(v) => self.push(format!("{op} {v}")),
                Expr::Graph(_) => self.ok = false,
            },
            Plan::Select { columns, .. } => self.push(format!("{op} {columns:?}")),
            Plan::WithColumns { columns, .. } => {
                self.push(op);
                for c in columns {
                    self.push(format!("column {:?}", c.name));
                    match &c.expr {
                        Expr::Scalar(v) => self.push(v.to_string()),
                        Expr::Graph(verb) => self.verb(verb)?,
                    }
                }
            }
            Plan::Rename { mapping, .. } => self.push(format!("{op} {mapping:?}")),
            Plan::Sort { by, .. } => self.push(format!("{op} {by:?}")),
            Plan::Head { n, .. } => self.push(format!("{op} {}", n.0)),
            Plan::Sample { n, seed, .. } => {
                let seed = seed.as_ref().map(|s| s.0.to_string());
                self.push(format!("{op} n={} seed={seed:?}", n.0));
            }
            Plan::Distinct { .. } | Plan::Nodes { .. } | Plan::Reverse { .. } => self.push(op),
            Plan::Join { right, on, how, .. } => {
                self.push(format!("{op} on={on:?} how={how:?} right ["));
                self.plan(right)?;
                self.push("]");
            }
            Plan::GroupByAgg { keys, aggs, .. } => {
                self.push(format!("{op} {keys:?} {aggs:?}"));
            }
            Plan::Hop {
                edges,
                n,
                direction,
                seeds,
            } => {
                self.push(format!("{op} n={} direction={direction:?}", n.0));
                self.seeds(*seeds)?;
                self.edges(edges)?;
            }
            Plan::ShortestPath {
                edges,
                source,
                target,
                weight,
                direction,
            } => {
                let weight = weight.as_ref().map(|w| w.to_string());
                self.push(format!("{op} weight={weight:?} direction={direction:?}"));
                self.seeds(*source)?;
                self.seeds(*target)?;
                self.edges(edges)?;
            }
            Plan::RandomWalk {
                edges,
                start,
                steps,
                walks_per_node,
                seed,
            } => {
                let seed = seed.as_ref().map(|s| s.0.to_string());
                self.push(format!(
                    "{op} steps={} walks_per_node={} seed={seed:?}",
                    steps.0, walks_per_node.0
                ));
                self.seeds(*start)?;
                self.edges(edges)?;
            }
            Plan::Describe {
                edges,
                full,
                direction,
            } => {
                self.push(format!("{op} full={full} direction={direction:?}"));
                self.edges(edges)?;
            }
            Plan::Invalid { .. } => self.ok = false,
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    use arrow::array::{DictionaryArray, Float64Array, Int32Array, Int64Array, StringArray};
    use arrow::datatypes::{Field, Int8Type, Schema};

    use crate::plan::{HostRows, Rows, Slot};
    use crate::scan::NodeTableCache;

    fn batch(src: ArrayRef, dst: ArrayRef) -> RecordBatch {
        let schema = Schema::new(vec![
            Field::new("a", src.data_type().clone(), true),
            Field::new("b", dst.data_type().clone(), true),
        ]);
        RecordBatch::try_new(Arc::new(schema), vec![src, dst]).unwrap()
    }

    fn roles() -> (String, String) {
        ("a".into(), "b".into())
    }

    #[test]
    fn builds_like_a_fresh_frame_over_the_rows() {
        // int32 ids canonicalize to int64; the dense order is first-seen, across
        // batches, as a fresh frame's build gives it.
        let b0 = batch(
            Arc::new(Int32Array::from(vec![7, 3])),
            Arc::new(Int32Array::from(vec![3, 9])),
        );
        let b1 = batch(
            Arc::new(Int32Array::from(vec![9])),
            Arc::new(Int32Array::from(vec![7])),
        );
        let index = build_index(&[b0, b1], &roles()).unwrap();
        let (topo, ids) = crate::topology::build_topology(
            &Int64Array::from(vec![7, 3, 9]),
            &Int64Array::from(vec![3, 9, 7]),
        )
        .unwrap();
        assert_eq!(index.topo.n_edges(), topo.n_edges());
        assert_eq!(
            index.ids.user_id_array().as_ref(),
            ids.user_id_array().as_ref()
        );
    }

    #[test]
    fn decodes_dictionary_ids_and_rejects_other_types() {
        let dict: DictionaryArray<Int8Type> = vec!["x", "y", "x"].into_iter().collect();
        let strings: ArrayRef = Arc::new(StringArray::from(vec!["y", "z", "z"]));
        let index = build_index(&[batch(Arc::new(dict), strings)], &roles()).unwrap();
        assert_eq!(index.topo.n_nodes(), 3);
        let floats: ArrayRef = Arc::new(Float64Array::from(vec![1.0]));
        let err = build_index(&[batch(floats.clone(), floats)], &roles());
        assert!(matches!(err, Err(PlanError::Type(_))));
    }

    fn index_of(n: i64) -> GraphIndex {
        build_index(
            &[batch(
                Arc::new(Int64Array::from(vec![0])),
                Arc::new(Int64Array::from(vec![n])),
            )],
            &roles(),
        )
        .unwrap()
    }

    #[test]
    fn caches_by_key_least_recently_used_evicted() {
        let first = cached_index(Some("test lru 0"), || Ok(index_of(1))).unwrap();
        let again = cached_index(Some("test lru 0"), || panic!("rebuilt")).unwrap();
        assert!(Arc::ptr_eq(&first.topo, &again.topo));
        // No key: never cached.
        let a = cached_index(None, || Ok(index_of(2))).unwrap();
        let b = cached_index(None, || Ok(index_of(2))).unwrap();
        assert!(!Arc::ptr_eq(&a.topo, &b.topo));
        // Past capacity the oldest goes; other tests share the cache, so only
        // this test's keys are checked.
        for i in 1..=TOPOLOGY_CACHE_CAPACITY {
            cached_index(Some(&format!("test lru {i}")), || Ok(index_of(3))).unwrap();
        }
        assert!(lookup("test lru 0").is_none());
        assert!(cached_count() <= TOPOLOGY_CACHE_CAPACITY);
    }

    /// A host with one in-memory table per slot (its token is the slot) and seed
    /// sets.
    struct Host(HashMap<Slot, Option<String>>);

    impl Inputs for Host {
        fn index(&self, _: Slot) -> PlanResult<GraphIndex> {
            unreachable!()
        }
        fn built_index(&self, _: Slot) -> PlanResult<Option<GraphIndex>> {
            unreachable!()
        }
        fn put_index(&self, _: Slot, _: GraphIndex) -> PlanResult<GraphIndex> {
            unreachable!()
        }
        fn table(&self, _: Slot) -> PlanResult<Option<Rows>> {
            unreachable!()
        }
        fn storage_options(&self, _: Slot) -> PlanResult<HashMap<String, String>> {
            Ok(HashMap::new())
        }
        fn node_cache(&self, _: Slot) -> PlanResult<Arc<NodeTableCache>> {
            unreachable!()
        }
        fn seeds(&self, slot: Slot) -> PlanResult<ArrayRef> {
            Ok(Arc::new(Int64Array::from(vec![slot as i64])))
        }
        fn sample(&self, _: Slot, _: usize) -> PlanResult<Option<Vec<usize>>> {
            unreachable!()
        }
        fn batches(&self, _: HostRows) -> PlanResult<Vec<RecordBatch>> {
            unreachable!()
        }
        fn step_error(&self, _: Slot) -> PlanError {
            unreachable!()
        }
        fn warn_dropped(&self, _: usize) -> PlanResult<()> {
            Ok(())
        }
        fn identity(&self, frame: Slot) -> PlanResult<Option<String>> {
            Ok(self.0.get(&frame).cloned().flatten())
        }
    }

    fn key(plan: serde_json::Value, host: &Host) -> Option<String> {
        let plan = Plan::from_json(&plan.to_string()).unwrap();
        fingerprint(&plan, &roles(), host, &mut Scratch::default()).unwrap()
    }

    fn source(frame: usize) -> serde_json::Value {
        serde_json::json!({"op": "from_arrow", "frame": frame, "src": "a", "dst": "b"})
    }

    #[test]
    fn fingerprints_tell_subplans_and_inputs_apart() {
        let host = Host(HashMap::from([
            (0, Some("t1".to_string())),
            (1, Some("t1".to_string())),
            (2, Some("t2".to_string())),
            (3, None),
        ]));
        let head = |frame, n, step| {
            serde_json::json!({"op": "head", "input": source(frame), "n": n, "step": step})
        };
        // The step handle and the slot don't matter; the table and the args do.
        assert_eq!(key(head(0, 5, 0), &host), key(head(1, 5, 9), &host));
        assert_ne!(key(head(0, 5, 0), &host), key(head(0, 6, 0), &host));
        assert_ne!(key(head(0, 5, 0), &host), key(head(2, 5, 0), &host));
        // A table with no identity is never cached.
        assert_eq!(key(head(3, 5, 0), &host), None);
        // A join's right side is part of the key.
        let join = |right| {
            serde_json::json!({"op": "join", "input": source(0), "right": source(right),
                "on": ["a"], "how": "semi"})
        };
        assert_ne!(key(join(0), &host), key(join(2), &host));
        assert_eq!(key(join(3), &host), None);
        // So is a seed set's content.
        let hop = |seeds| {
            serde_json::json!({"op": "distinct", "input": {"op": "hop", "n": 1,
                "direction": "out", "seeds": seeds,
                "edges": {"plan": source(0), "frame": 0}}})
        };
        assert_ne!(key(hop(4), &host), key(hop(5), &host));
        // So are the roles.
        let plan = Plan::from_json(&head(0, 5, 0).to_string()).unwrap();
        let swapped = ("b".to_string(), "a".to_string());
        let k = fingerprint(&plan, &swapped, &host, &mut Scratch::default()).unwrap();
        assert_ne!(k, key(head(0, 5, 0), &host));
    }
}
