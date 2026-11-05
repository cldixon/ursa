//! Resolving a call's arguments into a validated [`Call`].

use std::cmp::Ordering;
use std::fmt;
use std::hash::{Hash, Hasher};

use super::eval::OutputDtype;
use super::spec::{lookup, parse_direction, FunctionSpec, ParamSpec, ParamType, Value, FUNCTIONS};
use crate::algo::AggKind;
use crate::topology::Direction;

/// An argument as a front end received it, before validation.
///
/// Each front end converts its own literals: Python's JSON values, SQL's
/// literals. An integer is `Int` whatever its width; a number with a fraction or
/// exponent that does not parse as an integer is `Float`.
#[derive(Debug, Clone, PartialEq)]
pub enum Arg {
    Null,
    Bool(bool),
    Int(i128),
    Float(f64),
    Str(String),
    /// Anything else (a list, an object, an expression): always the wrong type.
    Other,
}

/// What a parameter expected, for the message about an argument of the wrong
/// type or out of range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Expected {
    /// A number ([`ParamType::Float`]).
    Number,
    /// A number or null ([`ParamType::OptionalFloat`]).
    NumberOrNull,
    /// A finite number: the argument was a NaN or an infinity.
    FiniteNumber,
    /// A non-negative integer below 2^32 ([`ParamType::Count`]).
    Count,
    /// A non-negative integer below 2^64, or null ([`ParamType::Seed`]).
    Seed,
    /// A boolean ([`ParamType::Bool`]).
    Bool,
}

/// What is wrong with an argument.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParamErrorKind {
    /// The function has no parameter of this name.
    Unknown,
    /// The parameter was given more than once.
    Duplicate,
    /// The argument has the wrong type or is out of range.
    Expected(Expected),
    /// The argument is not one of these strings (a choice, a direction the
    /// function accepts, or a dtype).
    NotOneOf(&'static [&'static str]),
    /// A weight or attribute was passed as a literal: the front end evaluates
    /// these itself and passes them to the evaluator as [`Inputs`](super::Inputs).
    Expression,
}

/// A bad argument to a graph function. Front ends word the message; the
/// `Display` form is a plain fallback.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParamError {
    /// The function called.
    pub function: &'static str,
    /// The parameter's name, as given.
    pub param: String,
    pub kind: ParamErrorKind,
}

impl fmt::Display for ParamError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (function, param) = (self.function, &self.param);
        match self.kind {
            ParamErrorKind::Unknown => write!(f, "{function}() has no parameter '{param}'"),
            ParamErrorKind::Duplicate => {
                write!(f, "{function}() was given '{param}' more than once")
            }
            ParamErrorKind::Expected(e) => {
                let want = match e {
                    Expected::Number => "a number",
                    Expected::NumberOrNull => "a number or null",
                    Expected::FiniteNumber => "a finite number",
                    Expected::Count => "a non-negative integer below 2^32",
                    Expected::Seed => "a non-negative integer or null",
                    Expected::Bool => "a boolean",
                };
                write!(f, "{function}(): '{param}' must be {want}")
            }
            ParamErrorKind::NotOneOf(options) => {
                let options: Vec<String> = options.iter().map(|o| format!("'{o}'")).collect();
                write!(
                    f,
                    "{function}(): '{param}' must be one of {}",
                    options.join(", ")
                )
            }
            ParamErrorKind::Expression => write!(
                f,
                "{function}(): '{param}' takes an expression, which the caller evaluates"
            ),
        }
    }
}

impl std::error::Error for ParamError {}

/// A resolved call: what to compute, and the dtype to emit it as.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Resolved {
    pub call: Call,
    /// `F64` unless the function takes a `dtype` and it was `'f32'`.
    pub dtype: OutputDtype,
}

/// A validated graph-function call: which kernel, with every parameter set.
///
/// Equality, hashing and ordering compare the function and its parameters,
/// floats by their bit patterns, so two calls are equal exactly when they name
/// the same computation (as plan nodes and the evaluator's memo need).
#[derive(Debug, Clone, Copy)]
pub enum Call {
    Degree {
        direction: Direction,
    },
    PageRank {
        damping: f64,
        max_iter: u32,
        tol: f64,
        /// `Out` (the stored rows) or `Both` (the undirected view).
        direction: Direction,
    },
    ConnectedComponents {
        /// Strongly connected components; weak (undirected) otherwise.
        strong: bool,
    },
    TriangleCount,
    ClusteringCoefficient,
    Closeness {
        /// Which way distance is measured: from the node (`Out`), to it (`In`),
        /// or ignoring direction (`Both`).
        direction: Direction,
    },
    Betweenness {
        sample: Option<f64>,
        seed: Option<u64>,
        /// `Out` (directed) or `Both` (undirected view, halved).
        direction: Direction,
    },
    LabelPropagation {
        max_iter: u32,
        seed: Option<u64>,
    },
    Louvain {
        resolution: f64,
        seed: Option<u64>,
    },
    LayoutFr {
        iterations: u32,
        k: f64,
        gravity: f64,
        seed: Option<u64>,
    },
    LayoutRandom {
        k: f64,
        seed: Option<u64>,
    },
    LayoutCircle {
        k: f64,
    },
    NeighborAgg {
        direction: Direction,
        agg: AggKind,
    },
    Density {
        direction: Direction,
    },
    AvgPathLength {
        sample: Option<f64>,
        direction: Direction,
    },
    Diameter {
        approximate: bool,
        direction: Direction,
    },
    Describe {
        full: bool,
        direction: Direction,
        /// Describe only the nodes at an end of a kept edge (`nodes='touched'`).
        touched: bool,
    },
}

fn opt_u64(v: Option<u64>) -> Value {
    v.map_or(Value::Null, Value::UInt)
}

fn opt_f64(v: Option<f64>) -> Value {
    v.map_or(Value::Null, Value::Float)
}

fn dir(d: Direction) -> Value {
    Value::Str(super::spec::direction_name(d))
}

fn agg_name(a: AggKind) -> &'static str {
    match a {
        AggKind::Mean => "mean",
        AggKind::Sum => "sum",
        AggKind::Min => "min",
        AggKind::Max => "max",
        AggKind::Count => "count",
        AggKind::NUnique => "n_unique",
    }
}

impl Call {
    /// The function's name.
    pub fn name(&self) -> &'static str {
        match self {
            Call::Degree { .. } => "degree",
            Call::PageRank { .. } => "pagerank",
            Call::ConnectedComponents { .. } => "connected_components",
            Call::TriangleCount => "triangle_count",
            Call::ClusteringCoefficient => "clustering_coefficient",
            Call::Closeness { .. } => "closeness",
            Call::Betweenness { .. } => "betweenness",
            Call::LabelPropagation { .. } => "label_propagation",
            Call::Louvain { .. } => "louvain",
            Call::LayoutFr { .. } => "layout_fr",
            Call::LayoutRandom { .. } => "layout_random",
            Call::LayoutCircle { .. } => "layout_circle",
            Call::NeighborAgg { .. } => "neighbors_agg",
            Call::Density { .. } => "density",
            Call::AvgPathLength { .. } => "avg_path_length",
            Call::Diameter { .. } => "diameter",
            Call::Describe { .. } => "describe",
        }
    }

    /// The function's spec.
    pub fn spec(&self) -> &'static FunctionSpec {
        lookup(self.name()).expect("every call has a spec")
    }

    /// The value of the literal parameter `name` (`None` for a weight, an
    /// attribute, a dtype, or a name the function does not have).
    fn value(&self, name: &str) -> Option<Value> {
        Some(match (self, name) {
            (
                Call::Degree { direction }
                | Call::PageRank { direction, .. }
                | Call::Closeness { direction }
                | Call::Betweenness { direction, .. }
                | Call::NeighborAgg { direction, .. }
                | Call::Density { direction }
                | Call::AvgPathLength { direction, .. }
                | Call::Diameter { direction, .. }
                | Call::Describe { direction, .. },
                "direction",
            ) => dir(*direction),
            (Call::PageRank { damping, .. }, "damping") => Value::Float(*damping),
            (Call::PageRank { max_iter, .. }, "max_iter")
            | (Call::LabelPropagation { max_iter, .. }, "max_iter") => {
                Value::UInt(u64::from(*max_iter))
            }
            (Call::PageRank { tol, .. }, "tol") => Value::Float(*tol),
            (Call::ConnectedComponents { strong }, "mode") => {
                Value::Str(if *strong { "strong" } else { "weak" })
            }
            (Call::Betweenness { sample, .. }, "sample")
            | (Call::AvgPathLength { sample, .. }, "sample") => opt_f64(*sample),
            (
                Call::Betweenness { seed, .. }
                | Call::LabelPropagation { seed, .. }
                | Call::Louvain { seed, .. }
                | Call::LayoutFr { seed, .. }
                | Call::LayoutRandom { seed, .. },
                "seed",
            ) => opt_u64(*seed),
            (Call::Louvain { resolution, .. }, "resolution") => Value::Float(*resolution),
            (Call::LayoutFr { iterations, .. }, "iterations") => {
                Value::UInt(u64::from(*iterations))
            }
            (
                Call::LayoutFr { k, .. } | Call::LayoutRandom { k, .. } | Call::LayoutCircle { k },
                "k",
            ) => Value::Float(*k),
            (Call::LayoutFr { gravity, .. }, "gravity") => Value::Float(*gravity),
            (Call::NeighborAgg { agg, .. }, "agg") => Value::Str(agg_name(*agg)),
            (Call::Diameter { approximate, .. }, "approximate") => Value::Bool(*approximate),
            (Call::Describe { full, .. }, "full") => Value::Bool(*full),
            (Call::Describe { touched, .. }, "nodes") => {
                Value::Str(if *touched { "touched" } else { "all" })
            }
            _ => return None,
        })
    }

    /// Every literal parameter and its value, in the spec's order: what the
    /// call computes with, for `EXPLAIN` and messages. Weights, attributes and
    /// the dtype are not part of a call and are left out.
    pub fn values(&self) -> Vec<(&'static ParamSpec, Value)> {
        self.spec()
            .params
            .iter()
            .filter_map(|p| self.value(p.name).map(|v| (p, v)))
            .collect()
    }

    /// The call's identity: its function and its parameters, each as a
    /// presence-tagged bit pattern so a null never collides with a value.
    fn key(&self) -> (usize, Vec<u64>) {
        let index = FUNCTIONS
            .iter()
            .position(|f| f.name == self.name())
            .expect("every call has a spec");
        let mut bits = Vec::new();
        for (param, value) in self.values() {
            let (tag, v) = match value {
                Value::Null => (0, 0),
                Value::Bool(b) => (1, u64::from(b)),
                Value::UInt(u) => (2, u),
                Value::Float(f) => (3, f.to_bits()),
                Value::Str(s) => (
                    4,
                    param.ty.choices().iter().position(|c| *c == s).unwrap_or(0) as u64,
                ),
            };
            bits.extend([tag, v]);
        }
        (index, bits)
    }
}

impl PartialEq for Call {
    fn eq(&self, other: &Self) -> bool {
        self.key() == other.key()
    }
}

impl Eq for Call {}

impl Hash for Call {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.key().hash(state)
    }
}

impl PartialOrd for Call {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Call {
    fn cmp(&self, other: &Self) -> Ordering {
        self.key().cmp(&other.key())
    }
}

/// Validated values, one per parameter of a spec (defaults filled in), read by
/// a spec's builder.
pub(crate) struct Values {
    params: &'static [ParamSpec],
    values: Vec<Value>,
}

impl Values {
    fn get(&self, name: &str) -> Value {
        let i = self
            .params
            .iter()
            .position(|p| p.name == name)
            .unwrap_or_else(|| panic!("the spec has no parameter {name:?}"));
        self.values[i]
    }

    pub(crate) fn float(&self, name: &str) -> f64 {
        match self.get(name) {
            Value::Float(f) => f,
            other => panic!("{name} is not a float: {other:?}"),
        }
    }

    pub(crate) fn optional_float(&self, name: &str) -> Option<f64> {
        match self.get(name) {
            Value::Float(f) => Some(f),
            _ => None,
        }
    }

    pub(crate) fn count(&self, name: &str) -> u32 {
        match self.get(name) {
            Value::UInt(u) => u32::try_from(u).expect("a count is validated below 2^32"),
            other => panic!("{name} is not a count: {other:?}"),
        }
    }

    pub(crate) fn seed(&self, name: &str) -> Option<u64> {
        match self.get(name) {
            Value::UInt(u) => Some(u),
            _ => None,
        }
    }

    pub(crate) fn bool(&self, name: &str) -> bool {
        match self.get(name) {
            Value::Bool(b) => b,
            other => panic!("{name} is not a bool: {other:?}"),
        }
    }

    pub(crate) fn str(&self, name: &str) -> &'static str {
        match self.get(name) {
            Value::Str(s) => s,
            other => panic!("{name} is not a string: {other:?}"),
        }
    }

    pub(crate) fn direction(&self, name: &str) -> Direction {
        parse_direction(self.str(name)).expect("a direction is validated")
    }
}

/// Validate one argument against its parameter.
fn check(param: &ParamSpec, arg: Arg) -> Result<Value, ParamErrorKind> {
    let expected = |e| Err(ParamErrorKind::Expected(e));
    let number = |arg: Arg, want: Expected| match arg {
        Arg::Int(i) => Ok(Value::Float(i as f64)),
        Arg::Float(f) if f.is_finite() => Ok(Value::Float(f)),
        Arg::Float(_) => expected(Expected::FiniteNumber),
        _ => expected(want),
    };
    match param.ty {
        ParamType::Float => number(arg, Expected::Number),
        ParamType::OptionalFloat => match arg {
            Arg::Null => Ok(Value::Null),
            arg => number(arg, Expected::NumberOrNull),
        },
        ParamType::Count => match arg {
            Arg::Int(i) if (0..=i128::from(u32::MAX)).contains(&i) => Ok(Value::UInt(i as u64)),
            _ => expected(Expected::Count),
        },
        ParamType::Seed => match arg {
            Arg::Null => Ok(Value::Null),
            Arg::Int(i) if (0..=i128::from(u64::MAX)).contains(&i) => Ok(Value::UInt(i as u64)),
            _ => expected(Expected::Seed),
        },
        ParamType::Bool => match arg {
            Arg::Bool(b) => Ok(Value::Bool(b)),
            _ => expected(Expected::Bool),
        },
        ParamType::Choice(_) | ParamType::Direction(_) | ParamType::Dtype => {
            let choices = param.ty.choices();
            match arg {
                Arg::Str(s) => match choices.iter().find(|c| **c == s) {
                    Some(c) => Ok(Value::Str(c)),
                    None => Err(ParamErrorKind::NotOneOf(choices)),
                },
                _ => Err(ParamErrorKind::NotOneOf(choices)),
            }
        }
        ParamType::Weight | ParamType::Attribute => match arg {
            Arg::Null => Ok(Value::Null),
            _ => Err(ParamErrorKind::Expression),
        },
    }
}

impl FunctionSpec {
    /// Validate a call's named arguments and build the [`Call`].
    ///
    /// Arguments are checked in the order given, so the first bad one is the
    /// one reported; a parameter not given takes its default. A weight or an
    /// attribute may be given only as null: the front end evaluates those and
    /// passes them to the [`Evaluator`](super::Evaluator).
    pub fn resolve<S: AsRef<str>>(
        &'static self,
        args: impl IntoIterator<Item = (S, Arg)>,
    ) -> Result<Resolved, ParamError> {
        let params = self.params;
        let mut given: Vec<Option<Value>> = vec![None; params.len()];
        for (name, arg) in args {
            let name = name.as_ref();
            let error = |kind| ParamError {
                function: self.name,
                param: name.to_string(),
                kind,
            };
            let Some(i) = params.iter().position(|p| p.name == name) else {
                return Err(error(ParamErrorKind::Unknown));
            };
            if given[i].is_some() {
                return Err(error(ParamErrorKind::Duplicate));
            }
            given[i] = Some(check(&params[i], arg).map_err(error)?);
        }
        let values = Values {
            params,
            values: given
                .into_iter()
                .zip(params)
                .map(|(v, p)| v.unwrap_or(p.default))
                .collect(),
        };
        let dtype = match self.param("dtype") {
            Some(_) if values.str("dtype") == "f32" => OutputDtype::F32,
            _ => OutputDtype::F64,
        };
        Ok(Resolved {
            call: (self.build)(&values),
            dtype,
        })
    }
}
