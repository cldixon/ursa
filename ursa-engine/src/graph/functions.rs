//! The graph functions: their names, parameters and defaults, and the
//! [`Algorithm`] a call resolves to.
//!
//! Parameter names, defaults and accepted values are those of the Python API
//! (`python/ursa/_graph.py`, `docs/reference/algorithms.md`), so a call means
//! the same thing in both. Each function returns `(id, <function name>)`.
//!
//! `weight =>` arrives with M2 (#236): it becomes one more parameter, and the
//! weight expression one more column of the edge plan under the graph node.

use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::fmt;
use std::hash::{Hash, Hasher};

use datafusion::arrow::datatypes::DataType;
use datafusion::common::utils::datafusion_strsim::levenshtein;
use datafusion::error::{DataFusionError, Result};
use ursa_core::Direction;

/// A graph function's call, resolved: which kernel and its parameters.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Algorithm {
    Degree {
        direction: Direction,
    },
    PageRank {
        damping: f64,
        max_iter: u32,
        tol: f64,
        /// `Out` or `Both`; `In` is rejected when the call is planned.
        direction: Direction,
    },
    ConnectedComponents {
        /// `mode => 'strong'`; `'weak'` (the default) is `false`.
        strong: bool,
    },
    TriangleCount,
    Louvain {
        resolution: f64,
        /// `None`: the kernel's fixed default seed, as Python's `seed=None`.
        seed: Option<u64>,
    },
}

impl Algorithm {
    /// The function's name, which is also the name of its value column.
    pub fn name(&self) -> &'static str {
        match self {
            Algorithm::Degree { .. } => "degree",
            Algorithm::PageRank { .. } => "pagerank",
            Algorithm::ConnectedComponents { .. } => "connected_components",
            Algorithm::TriangleCount => "triangle_count",
            Algorithm::Louvain { .. } => "louvain",
        }
    }

    /// The Arrow type of the value column, as the Python API emits it.
    pub fn output_type(&self) -> DataType {
        match self {
            Algorithm::PageRank { .. } => DataType::Float64,
            _ => DataType::UInt32,
        }
    }

    /// A canonical identity: the variant and its parameters, floats by their
    /// bits (so `Eq`, `Hash` and `Ord` agree, as plan nodes need).
    fn key(&self) -> (u8, [u64; 4]) {
        let dir = |d: &Direction| match d {
            Direction::Out => 0,
            Direction::In => 1,
            Direction::Both => 2,
        };
        match self {
            Algorithm::Degree { direction } => (0, [dir(direction), 0, 0, 0]),
            Algorithm::PageRank {
                damping,
                max_iter,
                tol,
                direction,
            } => (
                1,
                [
                    damping.to_bits(),
                    u64::from(*max_iter),
                    tol.to_bits(),
                    dir(direction),
                ],
            ),
            Algorithm::ConnectedComponents { strong } => (2, [u64::from(*strong), 0, 0, 0]),
            Algorithm::TriangleCount => (3, [0; 4]),
            Algorithm::Louvain { resolution, seed } => (
                4,
                [
                    resolution.to_bits(),
                    u64::from(seed.is_some()),
                    seed.unwrap_or(0),
                    0,
                ],
            ),
        }
    }
}

impl Eq for Algorithm {}

impl Hash for Algorithm {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.key().hash(state)
    }
}

impl PartialOrd for Algorithm {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Algorithm {
    fn cmp(&self, other: &Self) -> Ordering {
        self.key().cmp(&other.key())
    }
}

/// The call as SQL would spell it, every parameter shown: for `EXPLAIN`.
impl fmt::Display for Algorithm {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = self.name();
        match self {
            Algorithm::Degree { direction } => {
                write!(f, "{name}(direction => '{}')", direction_name(*direction))
            }
            Algorithm::PageRank {
                damping,
                max_iter,
                tol,
                direction,
            } => write!(
                f,
                "{name}(damping => {damping:?}, max_iter => {max_iter}, tol => {tol:?}, \
                 direction => '{}')",
                direction_name(*direction)
            ),
            Algorithm::ConnectedComponents { strong } => write!(
                f,
                "{name}(mode => '{}')",
                if *strong { "strong" } else { "weak" }
            ),
            Algorithm::TriangleCount => write!(f, "{name}()"),
            Algorithm::Louvain { resolution, seed } => {
                write!(f, "{name}(resolution => {resolution:?}, seed => ")?;
                match seed {
                    Some(seed) => write!(f, "{seed})"),
                    None => write!(f, "NULL)"),
                }
            }
        }
    }
}

fn direction_name(d: Direction) -> &'static str {
    match d {
        Direction::Out => "out",
        Direction::In => "in",
        Direction::Both => "both",
    }
}

/// A literal argument as written: a (possibly signed) number, a string, or
/// `NULL`.
#[derive(Debug, Clone, PartialEq)]
pub enum Literal {
    /// The number's text, sign included, so it parses exactly as Python
    /// parses the same text.
    Number(String),
    String(String),
    Null,
}

impl fmt::Display for Literal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Literal::Number(n) => write!(f, "{n}"),
            Literal::String(s) => write!(f, "'{}'", s.replace('\'', "''")),
            Literal::Null => write!(f, "NULL"),
        }
    }
}

/// What a parameter accepts.
#[derive(Debug, Clone, Copy)]
enum Kind {
    /// Any number (Python `float`, which takes an `int` too).
    Float,
    /// A non-negative integer below 2^32 (Python `int`, read as `u32`).
    Count,
    /// A non-negative integer, or `NULL` for the default (Python `int | None`).
    Seed,
    /// One of these strings.
    Choice(&'static [&'static str]),
}

/// One parameter: its name, what it accepts, and its default as SQL writes it.
#[derive(Debug, Clone, Copy)]
pub struct Param {
    pub name: &'static str,
    kind: Kind,
    pub default: &'static str,
}

/// One graph function.
#[derive(Debug)]
pub struct FunctionSpec {
    pub name: &'static str,
    pub params: &'static [Param],
    build: fn(&Args) -> Result<Algorithm>,
}

const ALL_DIRECTIONS: &[&str] = &["out", "in", "both"];

/// The graph functions, by name. M1: cldixon/ursa#235.
pub const FUNCTIONS: &[FunctionSpec] = &[
    FunctionSpec {
        name: "degree",
        params: &[Param {
            name: "direction",
            kind: Kind::Choice(ALL_DIRECTIONS),
            default: "'out'",
        }],
        build: |a| {
            Ok(Algorithm::Degree {
                direction: a.direction("direction")?,
            })
        },
    },
    FunctionSpec {
        name: "pagerank",
        params: &[
            Param {
                name: "damping",
                kind: Kind::Float,
                default: "0.85",
            },
            Param {
                name: "max_iter",
                kind: Kind::Count,
                default: "30",
            },
            Param {
                name: "tol",
                kind: Kind::Float,
                default: "1e-6",
            },
            Param {
                name: "direction",
                kind: Kind::Choice(&["out", "both"]),
                default: "'out'",
            },
        ],
        build: |a| {
            Ok(Algorithm::PageRank {
                damping: a.float("damping")?,
                max_iter: a.count("max_iter")?,
                tol: a.float("tol")?,
                direction: a.direction("direction")?,
            })
        },
    },
    FunctionSpec {
        name: "connected_components",
        params: &[Param {
            name: "mode",
            kind: Kind::Choice(&["weak", "strong"]),
            default: "'weak'",
        }],
        build: |a| {
            Ok(Algorithm::ConnectedComponents {
                strong: a.choice("mode")? == "strong",
            })
        },
    },
    FunctionSpec {
        name: "triangle_count",
        params: &[],
        build: |_| Ok(Algorithm::TriangleCount),
    },
    FunctionSpec {
        name: "louvain",
        params: &[
            Param {
                name: "resolution",
                kind: Kind::Float,
                default: "1.0",
            },
            Param {
                name: "seed",
                kind: Kind::Seed,
                default: "NULL",
            },
        ],
        build: |a| {
            Ok(Algorithm::Louvain {
                resolution: a.float("resolution")?,
                seed: a.seed("seed")?,
            })
        },
    },
];

/// The graph function called `name`.
pub fn lookup(name: &str) -> Option<&'static FunctionSpec> {
    FUNCTIONS.iter().find(|f| f.name == name)
}

/// The names of the graph functions.
pub fn names() -> impl Iterator<Item = &'static str> {
    FUNCTIONS.iter().map(|f| f.name)
}

impl FunctionSpec {
    /// Resolve a call's named arguments (`name => literal`, names already
    /// normalized) against this function's parameters.
    pub fn resolve(&'static self, args: Vec<(String, Literal)>) -> Result<Algorithm> {
        let mut given = BTreeMap::new();
        for (name, value) in args {
            let Some(param) = self.params.iter().find(|p| p.name == name) else {
                return Err(self.unknown_param(&name));
            };
            if given.insert(param.name, value).is_some() {
                return Err(plan(format!(
                    "{}() was given '{}' more than once",
                    self.name, param.name
                )));
            }
        }
        (self.build)(&Args {
            function: self,
            given,
        })
    }

    fn unknown_param(&self, name: &str) -> DataFusionError {
        if self.params.is_empty() {
            return plan(format!(
                "{}() takes no parameters besides the graph; got '{name}'",
                self.name
            ));
        }
        let names: Vec<&str> = self.params.iter().map(|p| p.name).collect();
        let hint = did_you_mean(name, names.iter().copied())
            .map(|s| format!("; did you mean '{s}'?"))
            .unwrap_or_else(|| ".".to_string());
        plan(format!(
            "{}() has no parameter '{name}'{hint} Its parameters are: {}",
            self.name,
            names.join(", ")
        ))
    }

    /// `pagerank(<graph>, damping => 0.85, ...)`: the call with every
    /// parameter at its default, for messages.
    pub fn signature(&self) -> String {
        let mut parts = vec!["<graph>".to_string()];
        parts.extend(self.params.iter().map(|p| format!("{} => {}", p.name, p.default)));
        format!("{}({})", self.name, parts.join(", "))
    }
}

/// A plan error.
pub(crate) fn plan(message: String) -> DataFusionError {
    DataFusionError::Plan(message)
}

/// The closest of `candidates` to `name`, if one is close enough to be a typo.
pub fn did_you_mean<'a>(
    name: &str,
    candidates: impl IntoIterator<Item = &'a str>,
) -> Option<&'a str> {
    let lower = name.to_lowercase();
    candidates
        .into_iter()
        .map(|c| (levenshtein(&lower, &c.to_lowercase()), c))
        .filter(|(d, c)| *d <= (c.len() / 3).max(1) || lower == c.to_lowercase())
        .min_by_key(|(d, _)| *d)
        .map(|(_, c)| c)
}

/// A call's arguments, read by parameter with its default.
struct Args {
    function: &'static FunctionSpec,
    given: BTreeMap<&'static str, Literal>,
}

impl Args {
    fn param(&self, name: &str) -> &'static Param {
        self.function
            .params
            .iter()
            .find(|p| p.name == name)
            .expect("a function reads only its own parameters")
    }

    /// The literal for `name`: as given, or its default.
    fn value(&self, name: &str) -> Literal {
        match self.given.get(name) {
            Some(v) => v.clone(),
            None => default_literal(self.param(name)),
        }
    }

    /// "pagerank(damping =>) must be a number; got 'x'", as Python words it.
    fn wrong(&self, name: &str, want: &str, got: &Literal) -> DataFusionError {
        plan(format!(
            "{}({name} =>) must be {want}; got {got}",
            self.function.name
        ))
    }

    fn float(&self, name: &str) -> Result<f64> {
        debug_assert!(matches!(self.param(name).kind, Kind::Float));
        match self.value(name) {
            Literal::Number(n) => n
                .parse::<f64>()
                .ok()
                .filter(|v| v.is_finite())
                .ok_or_else(|| self.wrong(name, "a finite number", &Literal::Number(n))),
            other => Err(self.wrong(name, "a number", &other)),
        }
    }

    fn integer(&self, name: &str, want: &str, max: u64) -> Result<Option<u64>> {
        match self.value(name) {
            Literal::Null => Ok(None),
            Literal::Number(n) => match n.parse::<i128>() {
                Ok(v) if (0..=i128::from(max)).contains(&v) => Ok(Some(v as u64)),
                _ => Err(self.wrong(name, want, &Literal::Number(n))),
            },
            other => Err(self.wrong(name, want, &other)),
        }
    }

    fn count(&self, name: &str) -> Result<u32> {
        debug_assert!(matches!(self.param(name).kind, Kind::Count));
        let want = "a non-negative integer below 2^32";
        match self.integer(name, want, u64::from(u32::MAX))? {
            Some(v) => Ok(v as u32),
            None => Err(self.wrong(name, want, &Literal::Null)),
        }
    }

    fn seed(&self, name: &str) -> Result<Option<u64>> {
        debug_assert!(matches!(self.param(name).kind, Kind::Seed));
        self.integer(name, "a non-negative integer or NULL", u64::MAX)
    }

    fn choice(&self, name: &str) -> Result<String> {
        let Kind::Choice(allowed) = self.param(name).kind else {
            unreachable!("{name} is not a choice")
        };
        let options: Vec<String> = allowed.iter().map(|a| format!("'{a}'")).collect();
        let value = self.value(name);
        match &value {
            Literal::String(s) if allowed.contains(&s.as_str()) => Ok(s.clone()),
            _ => Err(plan(format!(
                "{}({name} => {value}) is not supported; use one of {}",
                self.function.name,
                options.join(", ")
            ))),
        }
    }

    fn direction(&self, name: &str) -> Result<Direction> {
        Ok(match self.choice(name)?.as_str() {
            "out" => Direction::Out,
            "in" => Direction::In,
            _ => Direction::Both,
        })
    }
}

/// A parameter's default as a literal.
fn default_literal(param: &Param) -> Literal {
    match param.default {
        "NULL" => Literal::Null,
        d if d.starts_with('\'') => Literal::String(d.trim_matches('\'').to_string()),
        d => Literal::Number(d.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(name: &str, args: &[(&str, Literal)]) -> Result<Algorithm> {
        lookup(name).unwrap().resolve(
            args.iter()
                .map(|(n, v)| (n.to_string(), v.clone()))
                .collect(),
        )
    }

    fn num(n: &str) -> Literal {
        Literal::Number(n.into())
    }

    fn s(v: &str) -> Literal {
        Literal::String(v.into())
    }

    fn message(r: Result<Algorithm>) -> String {
        crate::user_message(&r.unwrap_err())
    }

    #[test]
    fn defaults_match_python() {
        assert_eq!(
            call("pagerank", &[]).unwrap(),
            Algorithm::PageRank {
                damping: 0.85,
                max_iter: 30,
                tol: 1e-6,
                direction: Direction::Out
            }
        );
        assert_eq!(
            call("degree", &[]).unwrap(),
            Algorithm::Degree {
                direction: Direction::Out
            }
        );
        assert_eq!(
            call("connected_components", &[]).unwrap(),
            Algorithm::ConnectedComponents { strong: false }
        );
        assert_eq!(
            call("louvain", &[]).unwrap(),
            Algorithm::Louvain {
                resolution: 1.0,
                seed: None
            }
        );
        assert_eq!(call("triangle_count", &[]).unwrap(), Algorithm::TriangleCount);
    }

    #[test]
    fn reads_arguments() {
        assert_eq!(
            call(
                "pagerank",
                &[
                    ("damping", num("0.9")),
                    ("max_iter", num("100")),
                    ("tol", num("1e-10")),
                    ("direction", s("both"))
                ]
            )
            .unwrap(),
            Algorithm::PageRank {
                damping: 0.9,
                max_iter: 100,
                tol: 1e-10,
                direction: Direction::Both
            }
        );
        // An integer is a number, as Python's float parameters take an int.
        assert!(call("pagerank", &[("damping", num("1"))]).is_ok());
        assert_eq!(
            call("louvain", &[("seed", num("7")), ("resolution", num("-0.5"))]).unwrap(),
            Algorithm::Louvain {
                resolution: -0.5,
                seed: Some(7)
            }
        );
        assert_eq!(
            call("louvain", &[("seed", Literal::Null)]).unwrap(),
            call("louvain", &[]).unwrap()
        );
        assert_eq!(
            call("connected_components", &[("mode", s("strong"))]).unwrap(),
            Algorithm::ConnectedComponents { strong: true }
        );
    }

    #[test]
    fn explains_bad_arguments() {
        assert_eq!(
            message(call("pagerank", &[("dampng", num("0.9"))])),
            "pagerank() has no parameter 'dampng'; did you mean 'damping'? Its parameters are: \
             damping, max_iter, tol, direction"
        );
        assert_eq!(
            message(call("pagerank", &[("weight", num("1"))])),
            "pagerank() has no parameter 'weight'. Its parameters are: damping, max_iter, tol, \
             direction"
        );
        assert_eq!(
            message(call("triangle_count", &[("direction", s("both"))])),
            "triangle_count() takes no parameters besides the graph; got 'direction'"
        );
        assert_eq!(
            message(call(
                "pagerank",
                &[("damping", num("0.9")), ("damping", num("0.8"))]
            )),
            "pagerank() was given 'damping' more than once"
        );
        assert_eq!(
            message(call("pagerank", &[("damping", s("high"))])),
            "pagerank(damping =>) must be a number; got 'high'"
        );
        assert_eq!(
            message(call("pagerank", &[("max_iter", num("-1"))])),
            "pagerank(max_iter =>) must be a non-negative integer below 2^32; got -1"
        );
        assert_eq!(
            message(call("pagerank", &[("max_iter", num("2.5"))])),
            "pagerank(max_iter =>) must be a non-negative integer below 2^32; got 2.5"
        );
        assert_eq!(
            message(call("pagerank", &[("max_iter", num("4294967296"))])),
            "pagerank(max_iter =>) must be a non-negative integer below 2^32; got 4294967296"
        );
        assert_eq!(
            message(call("louvain", &[("seed", num("-3"))])),
            "louvain(seed =>) must be a non-negative integer or NULL; got -3"
        );
        assert_eq!(
            message(call("pagerank", &[("direction", s("in"))])),
            "pagerank(direction => 'in') is not supported; use one of 'out', 'both'"
        );
        assert_eq!(
            message(call("degree", &[("direction", s("sideways"))])),
            "degree(direction => 'sideways') is not supported; use one of 'out', 'in', 'both'"
        );
        assert_eq!(
            message(call("connected_components", &[("mode", num("1"))])),
            "connected_components(mode => 1) is not supported; use one of 'weak', 'strong'"
        );
    }

    #[test]
    fn suggests_names() {
        assert_eq!(did_you_mean("pagerenk", names()), Some("pagerank"));
        assert_eq!(did_you_mean("PAGERANK", names()), Some("pagerank"));
        assert_eq!(did_you_mean("closeness", names()), None);
    }

    #[test]
    fn signatures_show_defaults() {
        assert_eq!(
            lookup("pagerank").unwrap().signature(),
            "pagerank(<graph>, damping => 0.85, max_iter => 30, tol => 1e-6, direction => 'out')"
        );
    }
}
