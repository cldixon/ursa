//! Tests for the catalog: the specs, resolution, and the dispatch.

use std::collections::HashSet;
use std::sync::Arc;

use arrow::array::{Array, ArrayRef, Float32Array, Float64Array, Int64Array, UInt32Array};
use arrow::datatypes::DataType;

use super::stats::{self, bfs_source_scan, touched_nodes};
use super::*;
use crate::algo::{
    betweenness, clustering_coefficient, neighbor_aggregate, pagerank, triangle_count, AggKind,
    PageRankParams,
};
use crate::topology::{Direction, EdgeMask, Topology};

fn topo(n: usize, src: &[u32], dst: &[u32]) -> Topology {
    Topology::build(n, src.to_vec(), dst.to_vec())
}

fn diamond() -> Topology {
    topo(3, &[0, 0, 1, 2], &[1, 2, 2, 0])
}

/// The triangle 0-1-2, plus 3 hanging off 0 and the chord 1-3.
fn triangly() -> Topology {
    topo(4, &[0, 1, 2, 0, 1], &[1, 2, 0, 3, 3])
}

fn call(name: &str, args: &[(&str, Arg)]) -> Result<Resolved, ParamError> {
    lookup(name).unwrap().resolve(args.iter().cloned())
}

fn s(v: &str) -> Arg {
    Arg::Str(v.into())
}

fn f64s(a: &ArrayRef) -> Vec<f64> {
    a.as_any()
        .downcast_ref::<Float64Array>()
        .unwrap()
        .values()
        .to_vec()
}

fn u32s(a: &ArrayRef) -> Vec<u32> {
    a.as_any()
        .downcast_ref::<UInt32Array>()
        .unwrap()
        .values()
        .to_vec()
}

fn pagerank_call() -> Call {
    Call::PageRank {
        damping: 0.85,
        max_iter: 30,
        tol: 1e-6,
        direction: Direction::Out,
    }
}

// --- the specs ----------------------------------------------------------------

#[test]
fn names_are_unique_and_every_call_finds_its_spec() {
    let names: HashSet<&str> = names().collect();
    assert_eq!(names.len(), FUNCTIONS.len());
    for spec in FUNCTIONS {
        let resolved = spec.resolve(Vec::<(&str, Arg)>::new()).unwrap();
        assert_eq!(resolved.call.name(), spec.name);
        assert!(std::ptr::eq(resolved.call.spec(), spec));
        assert_eq!(resolved.dtype, OutputDtype::F64);
        assert!(!spec.outputs.is_empty(), "{}", spec.name);
        assert!(!spec.summary.is_empty(), "{}", spec.name);
    }
}

#[test]
fn the_catalog_covers_the_python_api() {
    let mut got: Vec<&str> = names().collect();
    got.sort_unstable();
    assert_eq!(
        got,
        [
            "avg_path_length",
            "betweenness",
            "closeness",
            "clustering_coefficient",
            "connected_components",
            "degree",
            "density",
            "describe",
            "diameter",
            "label_propagation",
            "layout_circle",
            "layout_fr",
            "layout_random",
            "louvain",
            "neighbors_agg",
            "pagerank",
            "triangle_count",
        ]
    );
}

#[test]
fn defaults_are_the_python_apis() {
    let defaults = |name: &str| -> Vec<(&str, Value)> {
        lookup(name)
            .unwrap()
            .params
            .iter()
            .map(|p| (p.name, p.default))
            .collect()
    };
    use Value::*;
    assert_eq!(
        defaults("pagerank"),
        [
            ("damping", Float(0.85)),
            ("max_iter", UInt(30)),
            ("tol", Float(1e-6)),
            ("weight", Null),
            ("dtype", Str("f64")),
            ("direction", Str("out")),
        ]
    );
    assert_eq!(
        defaults("betweenness"),
        [
            ("sample", Null),
            ("weight", Null),
            ("seed", Null),
            ("dtype", Str("f64")),
            ("direction", Str("out")),
        ]
    );
    assert_eq!(
        defaults("louvain"),
        [("weight", Null), ("resolution", Float(1.0)), ("seed", Null)]
    );
    assert_eq!(
        defaults("layout_fr"),
        [
            ("iterations", UInt(300)),
            ("k", Float(1.0)),
            ("gravity", Float(0.02)),
            ("seed", Null),
        ]
    );
    assert_eq!(
        defaults("label_propagation"),
        [("max_iter", UInt(20)), ("seed", Null)]
    );
    assert_eq!(defaults("connected_components"), [("mode", Str("weak"))]);
    assert_eq!(
        defaults("diameter"),
        [("approximate", Bool(true)), ("direction", Str("out"))]
    );
    assert_eq!(
        defaults("describe"),
        [
            ("full", Bool(false)),
            ("direction", Str("out")),
            ("nodes", Str("all")),
        ]
    );
}

#[test]
fn weights_directions_and_dtypes_are_where_python_has_them() {
    let with = |keep: fn(&FunctionSpec) -> bool| -> Vec<&str> {
        let mut v: Vec<&str> = FUNCTIONS
            .iter()
            .filter(|s| keep(s))
            .map(|s| s.name)
            .collect();
        v.sort_unstable();
        v
    };
    assert_eq!(
        with(|s| s.weighted()),
        ["betweenness", "closeness", "louvain", "pagerank"]
    );
    assert_eq!(
        with(|s| s.narrows()),
        [
            "betweenness",
            "closeness",
            "clustering_coefficient",
            "neighbors_agg",
            "pagerank"
        ]
    );
    assert_eq!(with(|s| s.reads_attribute()), ["neighbors_agg"]);
    assert_eq!(lookup("pagerank").unwrap().directions(), OUT_OR_BOTH);
    assert_eq!(lookup("betweenness").unwrap().directions(), OUT_OR_BOTH);
    for name in [
        "degree",
        "closeness",
        "neighbors_agg",
        "density",
        "describe",
    ] {
        assert_eq!(lookup(name).unwrap().directions(), ALL_DIRECTIONS, "{name}");
    }
    for name in [
        "triangle_count",
        "louvain",
        "layout_fr",
        "connected_components",
    ] {
        assert!(lookup(name).unwrap().directions().is_empty(), "{name}");
    }
}

#[test]
fn outputs_and_their_types() {
    for spec in FUNCTIONS {
        // A narrowable function has one Float64 output to narrow.
        if spec.narrows() {
            assert_eq!(spec.outputs.len(), 1, "{}", spec.name);
            assert_eq!(spec.outputs[0].ty, ColumnType::Float64, "{}", spec.name);
        }
        // A single-output function's column is named after it.
        if spec.outputs.len() == 1 {
            assert_eq!(spec.outputs[0].name, spec.name);
        }
        // Statistics are one row; everything else is per node.
        assert_eq!(
            spec.shape == Shape::OneRow,
            spec.family == Family::Statistic,
            "{}",
            spec.name
        );
        // A choice's default is one of its choices.
        for p in spec.params {
            if let Value::Str(d) = p.default {
                assert!(p.ty.choices().contains(&d), "{}.{}", spec.name, p.name);
            }
        }
    }
    let types = |name: &str| -> Vec<(&str, DataType)> {
        lookup(name)
            .unwrap()
            .outputs
            .iter()
            .map(|o| (o.name, o.ty.data_type()))
            .collect()
    };
    assert_eq!(types("degree"), [("degree", DataType::UInt32)]);
    assert_eq!(
        types("layout_fr"),
        [("x", DataType::Float32), ("y", DataType::Float32)]
    );
    assert_eq!(types("diameter"), [("diameter", DataType::Int64)]);
    let describe = lookup("describe").unwrap().outputs;
    assert_eq!(describe.len(), 5);
    assert!(describe[4].nullable && !describe[0].nullable);
}

// --- resolution ---------------------------------------------------------------

#[test]
fn resolves_arguments_and_fills_defaults() {
    let r = call(
        "pagerank",
        &[
            ("damping", Arg::Float(0.9)),
            ("max_iter", Arg::Int(100)),
            ("direction", s("both")),
            ("dtype", s("f32")),
        ],
    )
    .unwrap();
    assert_eq!(
        r.call,
        Call::PageRank {
            damping: 0.9,
            max_iter: 100,
            tol: 1e-6,
            direction: Direction::Both,
        }
    );
    assert_eq!(r.dtype, OutputDtype::F32);
    // An integer is a number; a null seed is the default.
    assert_eq!(
        call(
            "louvain",
            &[("resolution", Arg::Int(2)), ("seed", Arg::Null)]
        )
        .unwrap()
        .call,
        Call::Louvain {
            resolution: 2.0,
            seed: None
        }
    );
    assert_eq!(
        call("connected_components", &[("mode", s("strong"))])
            .unwrap()
            .call,
        Call::ConnectedComponents { strong: true }
    );
    assert_eq!(
        call("neighbors_agg", &[("agg", s("n_unique"))])
            .unwrap()
            .call,
        Call::NeighborAgg {
            direction: Direction::Out,
            agg: AggKind::NUnique
        }
    );
    assert_eq!(
        call(
            "describe",
            &[("nodes", s("touched")), ("full", Arg::Bool(true))]
        )
        .unwrap()
        .call,
        Call::Describe {
            full: true,
            direction: Direction::Out,
            touched: true
        }
    );
}

#[test]
fn values_round_trip_through_resolve() {
    for spec in FUNCTIONS {
        let call = spec.resolve(Vec::<(&str, Arg)>::new()).unwrap().call;
        let args: Vec<(&str, Arg)> = call
            .values()
            .into_iter()
            .map(|(p, v)| {
                let arg = match v {
                    Value::Null => Arg::Null,
                    Value::Bool(b) => Arg::Bool(b),
                    Value::UInt(u) => Arg::Int(u.into()),
                    Value::Float(f) => Arg::Float(f),
                    Value::Str(s) => Arg::Str(s.into()),
                };
                (p.name, arg)
            })
            .collect();
        assert_eq!(spec.resolve(args).unwrap().call, call, "{}", spec.name);
    }
}

fn kind(r: Result<Resolved, ParamError>) -> (String, ParamErrorKind) {
    let e = r.unwrap_err();
    (e.param, e.kind)
}

#[test]
fn reports_bad_arguments_structurally() {
    use ParamErrorKind::*;
    assert_eq!(
        kind(call("pagerank", &[("dampng", Arg::Float(0.9))])),
        ("dampng".into(), Unknown)
    );
    assert_eq!(
        kind(call(
            "pagerank",
            &[("tol", Arg::Float(0.1)), ("tol", Arg::Float(0.2))]
        )),
        ("tol".into(), Duplicate)
    );
    assert_eq!(
        kind(call("pagerank", &[("damping", s("high"))])),
        ("damping".into(), Expected(super::Expected::Number))
    );
    assert_eq!(
        kind(call("pagerank", &[("damping", Arg::Float(f64::NAN))])),
        ("damping".into(), Expected(super::Expected::FiniteNumber))
    );
    for bad in [Arg::Int(-1), Arg::Float(2.5), Arg::Int(1 << 32), Arg::Null] {
        assert_eq!(
            kind(call("pagerank", &[("max_iter", bad)])),
            ("max_iter".into(), Expected(super::Expected::Count))
        );
    }
    assert_eq!(
        kind(call("louvain", &[("seed", Arg::Int(-3))])),
        ("seed".into(), Expected(super::Expected::Seed))
    );
    assert_eq!(
        kind(call("betweenness", &[("sample", s("x"))])),
        ("sample".into(), Expected(super::Expected::NumberOrNull))
    );
    assert_eq!(
        kind(call("diameter", &[("approximate", Arg::Int(1))])),
        ("approximate".into(), Expected(super::Expected::Bool))
    );
    assert_eq!(
        kind(call("pagerank", &[("direction", s("in"))])),
        ("direction".into(), NotOneOf(OUT_OR_BOTH))
    );
    assert_eq!(
        kind(call("degree", &[("direction", Arg::Int(1))])),
        ("direction".into(), NotOneOf(ALL_DIRECTIONS))
    );
    assert_eq!(
        kind(call("connected_components", &[("mode", Arg::Null)])),
        ("mode".into(), NotOneOf(&["weak", "strong"]))
    );
    assert_eq!(
        kind(call("pagerank", &[("dtype", s("f16"))])),
        ("dtype".into(), NotOneOf(&["f64", "f32"]))
    );
    assert_eq!(
        kind(call("pagerank", &[("weight", s("w"))])),
        ("weight".into(), Expression)
    );
    // A null weight is unweighted.
    assert!(call("pagerank", &[("weight", Arg::Null)]).is_ok());
}

#[test]
fn the_first_bad_argument_in_the_order_given_is_reported() {
    let bad_seed = ("seed", Arg::Int(-1));
    let bad_max = ("max_iter", Arg::Int(-1));
    assert_eq!(
        call("label_propagation", &[bad_seed.clone(), bad_max.clone()])
            .unwrap_err()
            .param,
        "seed"
    );
    assert_eq!(
        call("label_propagation", &[bad_max, bad_seed])
            .unwrap_err()
            .param,
        "max_iter"
    );
}

#[test]
fn param_errors_have_a_plain_message() {
    let e = call("pagerank", &[("direction", s("in"))]).unwrap_err();
    assert_eq!(
        e.to_string(),
        "pagerank(): 'direction' must be one of 'out', 'both'"
    );
}

#[test]
fn calls_compare_by_their_parameters() {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let hash = |c: &Call| {
        let mut h = DefaultHasher::new();
        c.hash(&mut h);
        h.finish()
    };
    let a = pagerank_call();
    assert_eq!(a, pagerank_call());
    assert_eq!(hash(&a), hash(&pagerank_call()));
    let other = Call::PageRank {
        damping: 0.5,
        max_iter: 30,
        tol: 1e-6,
        direction: Direction::Out,
    };
    assert_ne!(a, other);
    // A null seed never collides with a zero one.
    assert_ne!(
        Call::LabelPropagation {
            max_iter: 20,
            seed: None
        },
        Call::LabelPropagation {
            max_iter: 20,
            seed: Some(0)
        }
    );
    // Different functions with the same parameters differ.
    assert_ne!(
        Call::Degree {
            direction: Direction::Out
        },
        Call::Closeness {
            direction: Direction::Out
        }
    );
    assert!(a.cmp(&other).is_ne());
}

// --- dispatch -----------------------------------------------------------------

#[test]
fn a_repeated_call_runs_once() {
    let t = diamond();
    let mut eval = Evaluator::new(&t, None);
    let a = eval
        .column(&pagerank_call(), Inputs::default(), 0, OutputDtype::F64)
        .unwrap();
    let b = eval
        .column(&pagerank_call(), Inputs::default(), 0, OutputDtype::F64)
        .unwrap();
    assert_eq!(eval.memo.len(), 1);
    // The same allocation: the second cannot have re-run the kernel.
    assert!(Arc::ptr_eq(&a, &b));
}

#[test]
fn the_dtype_narrows_after_the_shared_run() {
    let t = diamond();
    let mut eval = Evaluator::new(&t, None);
    let wide = eval
        .column(&pagerank_call(), Inputs::default(), 0, OutputDtype::F64)
        .unwrap();
    let narrow = eval
        .column(&pagerank_call(), Inputs::default(), 0, OutputDtype::F32)
        .unwrap();
    assert_eq!(eval.memo.len(), 1, "narrowing happens after the memo");
    assert_eq!(narrow.data_type(), &DataType::Float32);
    let narrow = narrow.as_any().downcast_ref::<Float32Array>().unwrap();
    for (i, w) in f64s(&wide).into_iter().enumerate() {
        assert_eq!(narrow.value(i), w as f32);
    }
}

#[test]
fn differing_calls_do_not_share() {
    let t = diamond();
    let mut eval = Evaluator::new(&t, None);
    let other = Call::PageRank {
        damping: 0.5,
        max_iter: 30,
        tol: 1e-6,
        direction: Direction::Out,
    };
    let degree = Call::Degree {
        direction: Direction::Out,
    };
    for c in [pagerank_call(), other, degree] {
        eval.evaluate(&c, Inputs::default()).unwrap();
    }
    assert_eq!(eval.memo.len(), 3);
}

#[test]
fn a_layout_runs_once_for_both_columns() {
    let t = diamond();
    let mut eval = Evaluator::new(&t, None);
    let layout = Call::LayoutFr {
        iterations: 20,
        k: 1.0,
        gravity: 0.02,
        seed: Some(1),
    };
    let x = eval
        .column(&layout, Inputs::default(), 0, OutputDtype::F64)
        .unwrap();
    let y = eval
        .column(&layout, Inputs::default(), 1, OutputDtype::F64)
        .unwrap();
    assert_eq!(eval.memo.len(), 1, "x and y share one run");
    assert_eq!(x.data_type(), &DataType::Float32);
    assert!(!Arc::ptr_eq(&x, &y));
    eval.evaluate(&Call::LayoutCircle { k: 1.0 }, Inputs::default())
        .unwrap();
    assert_eq!(eval.memo.len(), 2, "a different layout is a different run");
    assert_eq!(
        eval.column(&layout, Inputs::default(), 2, OutputDtype::F64)
            .unwrap_err(),
        DispatchError::NoSuchOutput {
            function: "layout_fr",
            output: 2
        }
    );
}

#[test]
fn the_triangle_family_shares_one_pass_and_matches_the_kernels() {
    let t = triangly();
    let mask = EdgeMask::from_bools(&[true, true, true, false, true]);
    for mask in [None, Some(&mask)] {
        let mut eval = Evaluator::new(&t, mask);
        let tri = eval
            .evaluate(&Call::TriangleCount, Inputs::default())
            .unwrap();
        let first = eval.triangles.as_ref().unwrap().1.as_ptr();
        let cc = eval
            .evaluate(&Call::ClusteringCoefficient, Inputs::default())
            .unwrap();
        let second = eval.triangles.as_ref().unwrap().1.as_ptr();
        assert_eq!(first, second, "clustering re-ran the triangle pass");
        assert_eq!(u32s(&tri[0]), triangle_count(&t, mask));
        // Exact: the shared path runs the identical float operations.
        assert_eq!(f64s(&cc[0]), clustering_coefficient(&t, mask));
    }
}

#[test]
fn the_triangle_family_survives_an_empty_graph() {
    let t = topo(0, &[], &[]);
    let mut eval = Evaluator::new(&t, None);
    let tri = eval
        .evaluate(&Call::TriangleCount, Inputs::default())
        .unwrap();
    let cc = eval
        .evaluate(&Call::ClusteringCoefficient, Inputs::default())
        .unwrap();
    assert_eq!((tri[0].len(), tri[0].data_type()), (0, &DataType::UInt32));
    assert_eq!((cc[0].len(), cc[0].data_type()), (0, &DataType::Float64));
}

#[test]
fn both_directions_read_the_symmetric_view() {
    let t = triangly();
    let mut eval = Evaluator::new(&t, None);
    let both = |direction| Call::PageRank {
        damping: 0.85,
        max_iter: 30,
        tol: 1e-6,
        direction,
    };
    let params = PageRankParams {
        damping: 0.85,
        max_iter: 30,
        tol: 1e-6,
    };
    let pr = eval
        .evaluate(&both(Direction::Both), Inputs::default())
        .unwrap();
    assert_eq!(f64s(&pr[0]), pagerank(t.symmetric(), None, params));
    let bc = eval
        .evaluate(
            &Call::Betweenness {
                sample: None,
                seed: None,
                direction: Direction::Both,
            },
            Inputs::default(),
        )
        .unwrap();
    let full = betweenness(t.symmetric(), None, None, None);
    let halved: Vec<f64> = full.into_iter().map(|x| x * 0.5).collect();
    assert_eq!(f64s(&bc[0]), halved);
}

#[test]
fn weights_key_the_memo_and_are_checked() {
    let t = triangly();
    let w1 = vec![1.0, 2.0, 1.0, 3.0, 1.0];
    let w2 = w1.clone();
    let mut eval = Evaluator::new(&t, None);
    let weighted = |w| Inputs {
        weights: Some(w),
        attribute: None,
    };
    let a = eval.evaluate(&pagerank_call(), weighted(&w1)).unwrap();
    let b = eval.evaluate(&pagerank_call(), weighted(&w1)).unwrap();
    assert!(Arc::ptr_eq(&a, &b));
    // Equal values in another column are another computation (by address).
    let c = eval.evaluate(&pagerank_call(), weighted(&w2)).unwrap();
    assert!(!Arc::ptr_eq(&a, &c));
    assert_eq!(f64s(&a[0]), f64s(&c[0]));
    eval.evaluate(&pagerank_call(), Inputs::default()).unwrap();
    assert_eq!(eval.memo.len(), 3);

    let degree = Call::Degree {
        direction: Direction::Out,
    };
    assert_eq!(
        eval.evaluate(&degree, weighted(&w1)).unwrap_err(),
        DispatchError::UnexpectedWeights { function: "degree" }
    );
    assert_eq!(
        eval.evaluate(&pagerank_call(), weighted(&w1[..2]))
            .unwrap_err(),
        DispatchError::WeightCount {
            expected: 5,
            got: 2
        }
    );
}

#[test]
fn a_neighbour_aggregation_reads_its_attribute() {
    let t = triangly();
    let attr = vec![Some(1.0), None, Some(3.0), Some(4.0)];
    let agg = Call::NeighborAgg {
        direction: Direction::Both,
        agg: AggKind::Mean,
    };
    let mut eval = Evaluator::new(&t, None);
    assert_eq!(
        eval.evaluate(&agg, Inputs::default()).unwrap_err(),
        DispatchError::MissingAttribute {
            function: "neighbors_agg"
        }
    );
    let inputs = Inputs {
        weights: None,
        attribute: Some(&attr),
    };
    let got = eval.column(&agg, inputs, 0, OutputDtype::F32).unwrap();
    let want = neighbor_aggregate(&t, &attr, None, Direction::Both, AggKind::Mean);
    let got = got.as_any().downcast_ref::<Float32Array>().unwrap();
    for (i, w) in want.iter().enumerate() {
        assert_eq!(got.is_null(i), w.is_none());
        if let Some(w) = w {
            assert_eq!(got.value(i), *w as f32);
        }
    }
    assert_eq!(
        eval.evaluate(&Call::TriangleCount, inputs).unwrap_err(),
        DispatchError::UnexpectedAttribute {
            function: "triangle_count"
        }
    );
}

#[test]
fn only_a_float_output_narrows() {
    let t = diamond();
    let mut eval = Evaluator::new(&t, None);
    let degree = Call::Degree {
        direction: Direction::Out,
    };
    assert_eq!(
        eval.column(&degree, Inputs::default(), 0, OutputDtype::F32)
            .unwrap_err(),
        DispatchError::NotNarrowable { function: "degree" }
    );
}

#[test]
fn statistics_are_one_row_of_their_spec_types() {
    let t = topo(3, &[0, 1, 2], &[1, 2, 0]);
    let mut eval = Evaluator::new(&t, None);
    for spec in FUNCTIONS.iter().filter(|s| s.shape == Shape::OneRow) {
        let call = spec.resolve(Vec::<(&str, Arg)>::new()).unwrap().call;
        let arrays = eval.evaluate(&call, Inputs::default()).unwrap();
        assert_eq!(arrays.len(), spec.outputs.len(), "{}", spec.name);
        for (a, o) in arrays.iter().zip(spec.outputs) {
            assert_eq!(a.len(), 1, "{}", spec.name);
            assert_eq!(a.data_type(), &o.ty.data_type(), "{}.{}", spec.name, o.name);
        }
    }
    let describe = Call::Describe {
        full: true,
        direction: Direction::Out,
        touched: false,
    };
    let row = eval.evaluate(&describe, Inputs::default()).unwrap();
    let i = |k: usize| {
        row[k]
            .as_any()
            .downcast_ref::<Int64Array>()
            .unwrap()
            .value(0)
    };
    assert_eq!((i(0), i(1), i(4)), (3, 3, 1));
    assert_eq!(f64s(&row[2]), [0.5]);
    assert_eq!(f64s(&row[3]), [1.0]);
}

// --- statistics ---------------------------------------------------------------

#[test]
fn density_of_a_directed_triangle() {
    let t = topo(3, &[0, 1, 2], &[1, 2, 0]);
    assert!((stats::density(&t, None, Direction::Out) - 0.5).abs() < 1e-12);
    assert_eq!(
        stats::density(&topo(1, &[0], &[0]), None, Direction::Out),
        0.0
    );
}

#[test]
fn describe_gates_n_components_behind_full() {
    let t = topo(3, &[0, 1, 2], &[1, 2, 0]);
    let d = stats::describe(&t, None, true, Direction::Out, false);
    assert_eq!((d.n_nodes, d.n_edges, d.n_components), (3, 3, Some(1)));
    assert_eq!((d.density, d.avg_degree), (0.5, 1.0));
    assert_eq!(
        stats::describe(&t, None, false, Direction::Out, false).n_components,
        None
    );
}

#[test]
fn describe_over_a_view_reads_only_the_kept_edges() {
    // 0->1, 0->2, 1->2, 2->3 with 0->2 and 1->2 kept.
    let t = topo(4, &[0, 0, 1, 2], &[1, 2, 2, 3]);
    let mask = EdgeMask::from_bools(&[false, true, true, false]);
    let row = |dir, touched| {
        let d = stats::describe(&t, Some(&mask), true, dir, touched);
        (
            d.n_nodes,
            d.n_edges,
            d.density,
            d.avg_degree,
            d.n_components,
        )
    };
    assert_eq!(row(Direction::Out, false), (4, 2, 2.0 / 12.0, 0.5, Some(2)));
    assert_eq!(row(Direction::Both, false), (4, 2, 2.0 / 6.0, 1.0, Some(2)));
    assert_eq!(
        row(Direction::Out, true),
        (3, 2, 2.0 / 6.0, 2.0 / 3.0, Some(1))
    );
    // Over the whole graph every node is touched.
    assert_eq!(
        stats::describe(&t, None, true, Direction::Out, true),
        stats::describe(&t, None, true, Direction::Out, false)
    );
}

#[test]
fn path_statistics() {
    let tri = topo(3, &[0, 1, 2], &[1, 2, 0]);
    // 6 reachable ordered pairs, distances {1,2} x3 -> mean 1.5
    assert!((stats::avg_path_length(&tri, None, None, Direction::Out) - 1.5).abs() < 1e-12);
    assert_eq!(stats::diameter(&tri, None, false, Direction::Out), 2);
    assert!(stats::diameter(&tri, None, true, Direction::Out) <= 2);
    // line 0->1->2->3
    assert_eq!(
        stats::diameter(
            &topo(4, &[0, 1, 2], &[1, 2, 3]),
            None,
            false,
            Direction::Out
        ),
        3
    );
    // two disjoint edges: only within-component pairs are reachable
    let t = topo(4, &[0, 2], &[1, 3]);
    assert!((stats::avg_path_length(&t, None, None, Direction::Out) - 1.0).abs() < 1e-12);
    assert_eq!(stats::diameter(&t, None, false, Direction::Out), 1);
}

#[test]
fn undirected_statistics_read_the_simple_undirected_view() {
    // A path 0-1-2-3 entered with a reciprocal pair, a parallel edge and a
    // self-loop: undirected it is 3 edges.
    let t = topo(4, &[0, 1, 1, 1, 2, 3], &[1, 0, 2, 2, 3, 3]);
    assert!((stats::density(&t, None, Direction::Both) - 0.5).abs() < 1e-12);
    assert!((stats::density(&t, None, Direction::Out) - 0.5).abs() < 1e-12);
    assert_eq!(
        stats::density(&t, None, Direction::In),
        stats::density(&t, None, Direction::Out)
    );
    assert_eq!(stats::diameter(&t, None, false, Direction::Both), 3);
    assert!((stats::avg_path_length(&t, None, None, Direction::Both) - 10.0 / 6.0).abs() < 1e-12);
    let d = stats::describe(&t, None, true, Direction::Both, false);
    assert_eq!((d.n_nodes, d.n_edges, d.n_components), (4, 3, Some(1)));
    assert_eq!((d.density, d.avg_degree), (0.5, 1.5));
}

#[test]
fn in_matches_out_over_the_whole_graph() {
    let t = topo(4, &[0, 1, 2, 2], &[1, 2, 3, 0]);
    assert_eq!(
        stats::diameter(&t, None, false, Direction::In),
        stats::diameter(&t, None, false, Direction::Out)
    );
    assert!(
        (stats::avg_path_length(&t, None, None, Direction::In)
            - stats::avg_path_length(&t, None, None, Direction::Out))
        .abs()
            < 1e-12
    );
}

#[test]
fn masked_statistics_read_only_the_kept_edges() {
    let t = topo(4, &[0, 0, 1, 2], &[1, 2, 2, 3]);
    let mask = EdgeMask::from_bools(&[false, true, true, false]);
    let m = Some(&mask);
    assert_eq!(stats::density(&t, m, Direction::Out), 2.0 / 12.0);
    assert_eq!(stats::diameter(&t, m, false, Direction::Out), 1);
    assert_eq!(stats::diameter(&t, m, false, Direction::Both), 2);
    assert_eq!(stats::avg_path_length(&t, m, None, Direction::Out), 1.0);
}

#[test]
fn a_masked_estimate_samples_the_touched_nodes() {
    // A 1,000-node path with a 2-edge view far from node 0: the strided sample
    // over all nodes would miss it.
    let n = 1000u32;
    let src: Vec<u32> = (0..n - 1).collect();
    let dst: Vec<u32> = (1..n).collect();
    let t = topo(n as usize, &src, &dst);
    let keep: Vec<bool> = (0..n - 1).map(|i| i == 700 || i == 701).collect();
    let mask = EdgeMask::from_bools(&keep);
    assert_eq!(touched_nodes(&t, &mask), vec![700, 701, 702]);
    assert_eq!(stats::diameter(&t, Some(&mask), true, Direction::Out), 2);
}

#[test]
fn the_path_scan_matches_a_full_distance_fold() {
    // The scratch-reusing scan must fold to exactly what a fresh `bfs_distances`
    // per source, scanned over all `n`, gives — sparse and dense, every direction.
    use crate::algo::bfs_distances;
    for (n, m) in [(3000u32, 2500usize), (300, 3000)] {
        let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
        let mut next = move || {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (state >> 33) as u32
        };
        let (mut src, mut dst) = (Vec::new(), Vec::new());
        for _ in 0..m {
            src.push(next() % n);
            dst.push(next() % n);
        }
        let t = topo(n as usize, &src, &dst);
        let sources: Vec<u32> = (0..n).collect();
        for dir in [Direction::Out, Direction::In, Direction::Both] {
            let (mut sum, mut count, mut max) = (0.0f64, 0u64, 0i32);
            for &s in &sources {
                let mut ps = 0.0f64;
                for d in bfs_distances(&t, s, dir) {
                    if d >= 1 {
                        ps += d as f64;
                        count += 1;
                        max = max.max(d);
                    }
                }
                sum += ps;
            }
            let got = bfs_source_scan(&t, &sources, dir, None);
            assert_eq!(got.0.to_bits(), sum.to_bits(), "{dir:?}");
            assert_eq!((got.1, got.2), (count, max), "{dir:?}");
        }
    }
}
