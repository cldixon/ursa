//! SQL/PGQ pattern matching: `GRAPH_TABLE (g MATCH … COLUMNS (…))` (#296).
//!
//! Ursa's parser rewrites `GRAPH_TABLE ( body )` into `graph_table('body')`;
//! the relation planner hands the body here. [`parse`] reads it into a
//! [`Pattern`]; [`lower`] resolves each variable to one of the graph's element
//! tables and says what to join: a [`Lowered`] match, which the planner
//! builds as a logical plan (scans of the element tables, equality
//! conditions, the pattern's conditions and `COLUMNS`). DataFusion's
//! optimizer turns the equalities into hash joins.
//!
//! Supported:
//!
//! ```text
//! GRAPH_TABLE ( graph
//!   MATCH path [, path …]
//!   [WHERE condition]
//!   COLUMNS ( expression [AS name], … ) )
//!
//! path       := [var =] [selector] [mode [PATH | PATHS]] node (edge node)*
//! selector   := ANY SHORTEST | ALL SHORTEST | ANY [k] | SHORTEST k | ALL
//! mode       := WALK | TRAIL | SIMPLE | ACYCLIC
//! node       := ( [var] [:label | IS label] [WHERE condition] )
//! edge       := (-[ spec ]->  |  <-[ spec ]-  |  -[ spec ]-  |  ->  |  <-  |  -) [quantifier]
//! quantifier := * | + | ? | {n} | {m,n} | {m,} | {,n}
//! spec       := [var] [:label | IS label] [WHERE condition]
//! ```
//!
//! Conditions and `COLUMNS` are SQL expressions over the variables
//! (`a.name`); each variable is a relation of that name. A label names an element table: by its name or one of
//! its labels. A variable without one gets the only table it can be: from the
//! edges next to it and their `REFERENCES`, or because the graph has one table
//! of that kind; otherwise the pattern is refused, never guessed.
//!
//! Each variable reads its element table's properties: every column, or what
//! its labels' `PROPERTIES` clauses give, under their property names. An
//! undirected edge `-[e]-` matches both orientations of each edge (a self-loop
//! once): the match is a `UNION ALL` with one branch per orientation, so every
//! join stays an equality join.
//!
//! A quantified edge, and the edge of a path with a path variable, a selector
//! or a mode, is not joined: it is a [`Segment`], searched on the graph index
//! (see [`super::path_search`]), and joined to the nodes at its ends. A path
//! with a variable, a selector or a mode is one edge between two nodes for
//! now. Label expressions (`|`, `&`, `!`) and parenthesized path patterns are
//! refused with a message.
//!
//! ## Through the index
//!
//! A node is *bound* when a condition on it alone limits its key to values
//! the query names: `a.id = 7`, `a.id IN (1, 2)` or `a.id IN (SELECT …)`.
//! From a bound node, the fixed-length edges of its path are expanded
//! through the graph index too, one [`Segment`] of exactly one edge each,
//! outward from it: each segment starts from the keys the one before it
//! reached ([`Starts::After`]), so only the edges at the nodes reached are
//! read, never the whole edge table. A segment is a relation of edges as the
//! join's is, one row per edge row, so the rows are the same. A node whose
//! query reads only its key, at the end of a segment whose index's nodes are
//! that node's table's keys, is not joined to its table at all: its key is
//! the segment's column ([`Lowered::aliases`]). Edges stay joins when no node
//! is bound, when the query reads an edge's columns, or when
//! [`Usage::indexed`] does not list the edge table (its index could not be
//! kept between queries).

use std::collections::{BTreeMap, BTreeSet};

use datafusion::arrow::datatypes::SchemaRef;
use datafusion::common::TableReference;
use datafusion::error::{DataFusionError, Result};
use datafusion::sql::sqlparser::dialect::GenericDialect;
use datafusion::sql::sqlparser::parser::Parser;
use datafusion::sql::sqlparser::tokenizer::{Location, Token, TokenWithSpan, Tokenizer};

use super::provenance::{self, PatternEdge};
use super::{EdgeTable, Label, Properties, PropertyGraph, VertexTable};

/// The table function Ursa's parser rewrites `GRAPH_TABLE ( … )` into.
pub const FUNCTION: &str = "graph_table";

/// Undirected edges allowed in one pattern: each doubles the query's branches.
const MAX_UNDIRECTED: usize = 3;

/// A parsed `GRAPH_TABLE` body.
#[derive(Debug, Clone, PartialEq)]
pub struct Pattern {
    /// The graph's name, as written.
    pub graph: String,
    pub paths: Vec<PathPattern>,
    /// The pattern's `WHERE` condition, as written.
    pub condition: Option<String>,
    /// The `COLUMNS` list, as written (without its parentheses).
    pub columns: String,
}

/// One path of a `MATCH`: `[p =] [selector] [mode] (a)-[e]->…(b)`.
#[derive(Debug, Clone, PartialEq)]
pub struct PathPattern {
    /// The path variable, `p` in `p = …`.
    pub var: Option<String>,
    pub selector: Selector,
    pub mode: Mode,
    /// Nodes and edges alternate, starting and ending with a node.
    pub elements: Vec<Element>,
}

/// Which of a pair of nodes' paths a path keeps.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd)]
pub enum Selector {
    /// No selector (or `ALL`): every path.
    All,
    /// `ANY SHORTEST`, `ANY`, `ANY k` and `SHORTEST k`: the `k` shortest
    /// paths, the first by edge row among paths as short (see [`super::path_search`]).
    Shortest(u32),
    /// `ALL SHORTEST`: every path as short as the shortest.
    AllShortest,
}

/// Which walks a path may be (SQL/PGQ's path modes).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd)]
pub enum Mode {
    /// Any walk: nodes and edges may repeat (the default).
    Walk,
    /// No edge twice.
    Trail,
    /// No node twice, except that the last may be the first.
    Simple,
    /// No node twice.
    Acyclic,
}

/// An edge's quantifier: how many times it repeats, `{min,max}`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd)]
pub struct Quantifier {
    pub min: u32,
    /// `None`: no upper bound.
    pub max: Option<u32>,
}

impl Quantifier {
    /// An edge without a quantifier: exactly once.
    pub const ONCE: Quantifier = Quantifier {
        min: 1,
        max: Some(1),
    };
}

/// One element of a path: nodes and edges alternate, starting with a node.
#[derive(Debug, Clone, PartialEq)]
pub enum Element {
    Node(Spec),
    /// An edge, with its quantifier if it has one.
    Edge(Spec, Direction, Option<Quantifier>),
}

/// A node or edge: `[var] [:label] [WHERE condition]`.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Spec {
    pub var: Option<String>,
    pub label: Option<String>,
    pub condition: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd)]
pub enum Direction {
    /// `-[e]->`: from the node on the left to the node on the right.
    Right,
    /// `<-[e]-`: from the node on the right to the node on the left.
    Left,
    /// `-[e]-`: either way.
    Any,
}

fn pattern_err<T>(message: impl Into<String>) -> Result<T> {
    Err(DataFusionError::Plan(format!(
        "in GRAPH_TABLE: {}",
        message.into()
    )))
}

/// The body's tokens, without whitespace and comments, with their byte ranges.
struct Tokens<'a> {
    text: &'a str,
    tokens: Vec<(Token, usize, usize)>,
    at: usize,
}

impl<'a> Tokens<'a> {
    fn new(text: &'a str) -> Result<Self> {
        let tokens = Tokenizer::new(&GenericDialect {}, text)
            .tokenize_with_location()
            .map_err(|e| DataFusionError::Plan(format!("in GRAPH_TABLE: {e}")))?;
        let tokens = tokens
            .into_iter()
            .filter(|t| !matches!(t.token, Token::Whitespace(_)))
            .map(|TokenWithSpan { token, span }| {
                let start = offset(text, span.start).unwrap_or(text.len());
                let end = offset(text, span.end).unwrap_or(text.len());
                (token, start, end)
            })
            .filter(|(token, _, _)| *token != Token::EOF)
            .collect();
        Ok(Self {
            text,
            tokens,
            at: 0,
        })
    }

    fn peek(&self, n: usize) -> Option<&Token> {
        self.tokens.get(self.at + n).map(|(t, _, _)| t)
    }

    fn next(&mut self) -> Option<Token> {
        let token = self.tokens.get(self.at).map(|(t, _, _)| t.clone());
        self.at += 1;
        token
    }

    fn is_word(&self, n: usize, word: &str) -> bool {
        matches!(self.peek(n), Some(Token::Word(w)) if w.quote_style.is_none() && w.value.eq_ignore_ascii_case(word))
    }

    /// Whether a `KEEP` or `ONE ROW PER` clause starts at the next token: the
    /// word, not a property of that name (`a.keep`).
    fn starts_clause(&self) -> bool {
        let after_period = self.at > 0 && self.tokens[self.at - 1].0 == Token::Period;
        !after_period && (self.is_word(0, "KEEP") || (self.is_word(0, "ONE") && self.is_word(1, "ROW")))
    }

    /// The text of token `i`, as written.
    fn raw(&self, i: usize) -> &'a str {
        match self.tokens.get(i) {
            Some((_, start, end)) => &self.text[*start..*end],
            None => "",
        }
    }

    fn eat(&mut self, token: &Token) -> bool {
        if self.peek(0) == Some(token) {
            self.at += 1;
            true
        } else {
            false
        }
    }

    /// What the next token is, for an error: "'x' (at character 12)" or "the end".
    fn found(&self) -> String {
        match self.tokens.get(self.at) {
            Some((token, start, _)) => {
                format!("'{token}' (at character {} of the pattern)", start + 1)
            }
            None => "the end".to_string(),
        }
    }

    fn expect(&mut self, token: &Token, what: &str) -> Result<()> {
        if self.eat(token) {
            Ok(())
        } else {
            pattern_err(format!("expected {what}, found {}", self.found()))
        }
    }

    /// An identifier: an unquoted word (normalized as DataFusion normalizes
    /// identifiers: lower-cased) or a quoted one (kept).
    fn identifier(&mut self, what: &str) -> Result<String> {
        match self.peek(0) {
            Some(Token::Word(w)) => {
                let name = match w.quote_style {
                    Some(_) => w.value.clone(),
                    None => w.value.to_lowercase(),
                };
                self.at += 1;
                Ok(name)
            }
            _ => pattern_err(format!("expected {what}, found {}", self.found())),
        }
    }

    /// The text from the current token up to (not including) the first token
    /// at bracket depth 0 for which `stop` holds; the tokens are consumed.
    fn text_until(&mut self, stop: impl Fn(&Self, usize) -> bool) -> Option<String> {
        let start = self.at;
        let mut depth = 0i32;
        while self.at < self.tokens.len() {
            let token = &self.tokens[self.at].0;
            if depth == 0 && stop(self, 0) {
                break;
            }
            match token {
                Token::LParen | Token::LBracket => depth += 1,
                Token::RParen | Token::RBracket => {
                    if depth == 0 {
                        break;
                    }
                    depth -= 1;
                }
                _ => {}
            }
            self.at += 1;
        }
        if self.at == start {
            return None;
        }
        let from = self.tokens[start].1;
        let to = self.tokens[self.at - 1].2;
        Some(self.text[from..to].trim().to_string())
    }

    /// [`Self::text_until`], for text that is SQL: a condition or the
    /// `COLUMNS` list. Text left over after it is an error (#409): SQL's
    /// parser stops at the first token it cannot use, so `WHERE a.id = 0
    /// b.id = 3` would otherwise run as `WHERE a.id = 0`.
    fn sql_until(
        &mut self,
        sql: Sql,
        stop: impl Fn(&Self, usize) -> bool,
    ) -> Result<Option<String>> {
        let from = self.tokens.get(self.at).map(|(_, start, _)| *start);
        let text = self.text_until(stop).map(|t| graph_predicates(&t)).transpose()?;
        if let (Some(text), Some(from)) = (&text, from) {
            check_sql(text, from, sql)?;
        }
        Ok(text)
    }
}

/// The function `x IS [NOT] LABELED l` is rewritten to, as
/// `__ursa_is_labeled(x, 'l')`; the planner replaces it with `TRUE` or
/// `FALSE` once it knows `x`'s table.
pub const IS_LABELED: &str = "__ursa_is_labeled";
/// The function `a IS [NOT] SOURCE OF e` is rewritten to.
pub const IS_SOURCE: &str = "__ursa_is_source";
/// The function `a IS [NOT] DESTINATION OF e` is rewritten to.
pub const IS_DESTINATION: &str = "__ursa_is_destination";

/// `text`, a condition or `COLUMNS` list, with SQL/PGQ's predicates that SQL
/// cannot parse rewritten to function calls: `x IS [NOT] LABELED l`, `a IS
/// [NOT] SOURCE OF e` and `a IS [NOT] DESTINATION OF e`. The planner
/// replaces the calls, as it does `SAME` and `ALL_DIFFERENT`.
fn graph_predicates(text: &str) -> Result<String> {
    let Ok(tokens) = Tokenizer::new(&GenericDialect {}, text).tokenize_with_location() else {
        return Ok(text.to_string());
    };
    let tokens: Vec<(Token, usize, usize)> = tokens
        .into_iter()
        .filter(|t| !matches!(t.token, Token::Whitespace(_) | Token::EOF))
        .filter_map(|TokenWithSpan { token, span }| {
            Some((token, offset(text, span.start)?, offset(text, span.end)?))
        })
        .collect();
    let word = |i: usize, w: &str| matches!(tokens.get(i), Some((Token::Word(x), _, _)) if x.quote_style.is_none() && x.value.eq_ignore_ascii_case(w));
    let name = |i: usize| match tokens.get(i) {
        Some((Token::Word(w), start, end)) => Some((w, &text[*start..*end])),
        _ => None,
    };
    let mut out = String::new();
    let mut copied = 0;
    let mut i = 0;
    while i < tokens.len() {
        let variable = name(i).filter(|_| i == 0 || tokens[i - 1].0 != Token::Period);
        let (Some((_, var)), true) = (variable, word(i + 1, "IS")) else {
            i += 1;
            continue;
        };
        let mut j = i + 2;
        let negated = word(j, "NOT");
        if negated {
            j += 1;
        }
        let call = if word(j, "LABELED") {
            let Some((label, _)) = name(j + 1) else {
                return pattern_err(format!("{var} IS LABELED needs a label, as {var} IS LABELED person"));
            };
            if matches!(
                tokens.get(j + 2).map(|t| &t.0),
                Some(Token::Pipe | Token::Ampersand | Token::ExclamationMark | Token::Mod)
            ) {
                return pattern_err(LABEL_EXPRESSIONS);
            }
            let label = match label.quote_style {
                Some(_) => label.value.clone(),
                None => label.value.to_lowercase(),
            };
            j += 2;
            format!("{IS_LABELED}({var}, '{}')", label.replace('\'', "''"))
        } else if (word(j, "SOURCE") || word(j, "DESTINATION")) && word(j + 1, "OF") {
            let function = match word(j, "SOURCE") {
                true => IS_SOURCE,
                false => IS_DESTINATION,
            };
            let Some((_, edge)) = name(j + 2) else {
                return pattern_err(format!(
                    "IS SOURCE OF and IS DESTINATION OF take an edge variable, as {var} IS SOURCE OF e"
                ));
            };
            j += 3;
            format!("{function}({var}, {edge})")
        } else {
            i += 1;
            continue;
        };
        out.push_str(&text[copied..tokens[i].1]);
        match negated {
            true => out.push_str(&format!("(NOT {call})")),
            false => out.push_str(&call),
        }
        copied = tokens[j - 1].2;
        i = j;
    }
    out.push_str(&text[copied..]);
    Ok(out)
}

/// The refusal of a label expression (#412).
const LABEL_EXPRESSIONS: &str =
    "label expressions (|, &, !, %) are not supported yet; use one label (#412)";

/// What a piece of a pattern's text is, as SQL.
#[derive(Clone, Copy)]
enum Sql {
    /// A `WHERE` condition: one expression.
    Condition,
    /// The `COLUMNS` list: select items.
    Columns,
}

/// Refuse `text`, which starts at byte `from` of the pattern, when it is
/// `sql` followed by something else. A syntax error in the SQL itself is
/// left to the planner, which reports it when it parses the text.
fn check_sql(text: &str, from: usize, sql: Sql) -> Result<()> {
    let Ok(mut parser) = Parser::new(&GenericDialect {}).try_with_sql(text) else {
        return Ok(());
    };
    let parsed = match sql {
        Sql::Condition => parser.parse_expr().is_ok(),
        Sql::Columns => parser
            .parse_comma_separated(Parser::parse_select_item)
            .is_ok(),
    };
    if !parsed {
        return Ok(());
    }
    let next = parser.peek_token();
    if next.token == Token::EOF {
        return Ok(());
    }
    if let Some(clause) = unsupported_clause(&next.token, &parser.peek_nth_token(1).token) {
        return pattern_err(clause);
    }
    let read = offset(text, next.span.start).unwrap_or(text.len());
    let (after, hint) = match sql {
        Sql::Condition => ("the WHERE condition", "join conditions with AND or OR"),
        Sql::Columns => ("the COLUMNS item", "separate items with commas"),
    };
    pattern_err(format!(
        "unexpected '{}' (at character {} of the pattern) after {after} '{}'; {hint}",
        next.token,
        from + read + 1,
        text[..read].trim_end()
    ))
}

/// The refusal of a pattern clause written where it does not go, if `token`
/// (and the token after it, `next`) starts one: `KEEP …` or `ONE ROW PER …`
/// inside a node or an edge, or in `COLUMNS`.
fn unsupported_clause(token: &Token, next: &Token) -> Option<&'static str> {
    let is = |token: &Token, word: &str| matches!(token, Token::Word(w) if w.quote_style.is_none() && w.value.eq_ignore_ascii_case(word));
    if is(token, "KEEP") {
        Some(
            "KEEP goes after the whole pattern, before WHERE and COLUMNS, as MATCH (a)-[e]->*(b) KEEP ANY SHORTEST WHERE a.id = 0",
        )
    } else if is(token, "ONE") && is(next, "ROW") {
        Some(
            "ONE ROW PER goes after the pattern and its WHERE, before COLUMNS, as MATCH (a)-[e]->(b) ONE ROW PER MATCH COLUMNS (a.id)",
        )
    } else {
        None
    }
}

/// The byte offset of a tokenizer location in `text`.
fn offset(text: &str, at: Location) -> Option<usize> {
    if at.line == 0 || at.column == 0 {
        return None;
    }
    let mut line_start = 0;
    for _ in 1..at.line {
        line_start += text[line_start..].find('\n')? + 1;
    }
    let column = usize::try_from(at.column - 1).ok()?;
    let line = &text[line_start..];
    match line.char_indices().nth(column) {
        Some((o, _)) => Some(line_start + o),
        None if line.chars().count() == column => Some(text.len()),
        None => None,
    }
}

/// Parse a `GRAPH_TABLE` body: `graph MATCH … [WHERE …] COLUMNS (…)`.
pub fn parse(body: &str) -> Result<Pattern> {
    let mut t = Tokens::new(body)?;
    let graph = t.identifier("the graph's name")?;
    if !t.is_word(0, "MATCH") {
        return pattern_err(format!(
            "expected MATCH after the graph's name, found {}",
            t.found()
        ));
    }
    t.next();
    let mut paths = vec![parse_path(&mut t)?];
    while t.eat(&Token::Comma) {
        paths.push(parse_path(&mut t)?);
    }
    // KEEP, WHERE and ONE ROW PER, each at most once. The standard puts them
    // in that order; another order is read the same.
    let mut condition = None;
    let mut keep: Option<(Selector, Mode)> = None;
    let mut rows = false;
    loop {
        if t.is_word(0, "WHERE") {
            if condition.is_some() {
                return pattern_err("a pattern takes one WHERE; join its conditions with AND");
            }
            t.next();
            condition = t.sql_until(Sql::Condition, |t, _| {
                t.is_word(0, "COLUMNS") || t.starts_clause()
            })?;
            if condition.is_none() {
                return pattern_err("WHERE needs a condition");
            }
        } else if t.is_word(0, "KEEP") {
            if keep.is_some() {
                return pattern_err("a pattern takes one KEEP");
            }
            t.next();
            let before = t.at;
            let prefix = parse_prefix(&mut t)?;
            if t.at == before {
                return pattern_err(format!(
                    "KEEP takes a path selector or path mode, as KEEP ANY SHORTEST or KEEP TRAIL; found {}",
                    t.found()
                ));
            }
            keep = Some(prefix);
        } else if t.is_word(0, "ONE") && t.is_word(1, "ROW") {
            if rows {
                return pattern_err("a pattern takes one ONE ROW PER clause");
            }
            t.next();
            t.next();
            if !t.is_word(0, "PER") {
                return pattern_err(format!(
                    "expected PER after ONE ROW, found {}",
                    t.found()
                ));
            }
            t.next();
            if t.is_word(0, "VERTEX") || t.is_word(0, "STEP") {
                return pattern_err(ONE_ROW_PER_VERTEX);
            }
            if !t.is_word(0, "MATCH") {
                return pattern_err(format!(
                    "expected MATCH, VERTEX or STEP after ONE ROW PER, found {}",
                    t.found()
                ));
            }
            t.next();
            rows = true;
        } else {
            break;
        }
    }
    if let Some((selector, mode)) = keep {
        let [path] = paths.as_mut_slice() else {
            return pattern_err(
                "KEEP over several path patterns is not supported yet; put the selector or path mode before each path, as MATCH ANY SHORTEST (a)-[e]->*(b), TRAIL (c)-[f]->*(d)",
            );
        };
        if (selector != Selector::All && path.selector != Selector::All)
            || (mode != Mode::Walk && path.mode != Mode::Walk)
        {
            return pattern_err(
                "the path has a selector or path mode both before it and in KEEP; write it once",
            );
        }
        if selector != Selector::All {
            path.selector = selector;
        }
        if mode != Mode::Walk {
            path.mode = mode;
        }
    }
    let columns = if t.is_word(0, "COLUMNS") {
        t.next();
        t.expect(&Token::LParen, "'(' after COLUMNS")?;
        let Some(columns) = t.sql_until(Sql::Columns, |_, _| false)? else {
            return pattern_err("COLUMNS (…) lists nothing");
        };
        t.expect(&Token::RParen, "')' to close COLUMNS")?;
        if t.peek(0).is_some() {
            return pattern_err(format!("unexpected {} after COLUMNS (…)", t.found()));
        }
        match columns.as_str() {
            "*" => all_properties(&paths, "COLUMNS (*)")?,
            _ => columns,
        }
    } else if t.peek(0).is_none() {
        all_properties(&paths, "a GRAPH_TABLE without COLUMNS")?
    } else {
        if let Some(token) = t.peek(0) {
            if let Some(clause) = unsupported_clause(token, t.peek(1).unwrap_or(&Token::EOF)) {
                return pattern_err(clause);
            }
        }
        return pattern_err(format!(
            "expected ',', WHERE or COLUMNS after the pattern, found {}",
            t.found()
        ));
    };
    Ok(Pattern {
        graph,
        paths,
        condition,
        columns,
    })
}

/// The refusal of `ONE ROW PER VERTEX` and `ONE ROW PER STEP`.
const ONE_ROW_PER_VERTEX: &str = "ONE ROW PER VERTEX and ONE ROW PER STEP are not supported yet; return vertices(p) or edges(p) in COLUMNS and unnest them";

/// `COLUMNS (*)`, and `COLUMNS` left out: every property of each named node
/// and edge, in the order they first appear (`a.*, k.*, b.*`). An edge that
/// stands for several edges (quantified, or in a path searched as a whole)
/// has no properties to return, so it is left out. `what` names the form,
/// for the error when nothing is named.
fn all_properties(paths: &[PathPattern], what: &str) -> Result<String> {
    let mut vars: Vec<&str> = Vec::new();
    for path in paths {
        let whole = path.var.is_some() || path.selector != Selector::All || path.mode != Mode::Walk;
        for element in &path.elements {
            let var = match element {
                Element::Node(spec) => spec.var.as_deref(),
                Element::Edge(spec, _, quantifier) => match whole || quantifier.is_some() {
                    true => None,
                    false => spec.var.as_deref(),
                },
            };
            if let Some(var) = var {
                if !vars.contains(&var) {
                    vars.push(var);
                }
            }
        }
    }
    if vars.is_empty() {
        return pattern_err(format!(
            "{what} returns the properties of the pattern's named nodes and edges, and it names none; name one, as (a:person), or list what to return, as COLUMNS (a.id)"
        ));
    }
    Ok(vars
        .iter()
        .map(|v| format!("\"{}\".*", v.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(", "))
}

/// A path: `[p =] [selector] [mode [PATH | PATHS]]`, then a node, then edges
/// and nodes.
fn parse_path(t: &mut Tokens) -> Result<PathPattern> {
    let var = if matches!(t.peek(0), Some(Token::Word(_))) && t.peek(1) == Some(&Token::Eq) {
        let var = t.identifier("a path variable")?;
        t.next();
        Some(var)
    } else {
        None
    };
    let (selector, mode) = parse_prefix(t)?;
    if t.peek(0) == Some(&Token::LParen) && t.peek(1) == Some(&Token::LParen) {
        return pattern_err(
            "parenthesized path patterns, as ((a)-[e]->(b))*, are not supported yet; quantify an edge instead, as (a)-[e]->*(b)",
        );
    }
    let mut elements = vec![Element::Node(parse_node(t)?)];
    while let Some((spec, direction, quantifier)) = parse_edge(t)? {
        elements.push(Element::Edge(spec, direction, quantifier));
        elements.push(Element::Node(parse_node(t)?));
    }
    Ok(PathPattern {
        var,
        selector,
        mode,
        elements,
    })
}

/// A path's prefix, `[selector] [mode] [PATH | PATHS]`, every part optional;
/// also what `KEEP` takes.
fn parse_prefix(t: &mut Tokens) -> Result<(Selector, Mode)> {
    let selector = parse_selector(t)?;
    let mode = if t.is_word(0, "WALK") {
        Some(Mode::Walk)
    } else if t.is_word(0, "TRAIL") {
        Some(Mode::Trail)
    } else if t.is_word(0, "SIMPLE") {
        Some(Mode::Simple)
    } else if t.is_word(0, "ACYCLIC") {
        Some(Mode::Acyclic)
    } else {
        None
    };
    if mode.is_some() {
        t.next();
    }
    if t.is_word(0, "PATH") || t.is_word(0, "PATHS") {
        t.next();
    }
    Ok((selector, mode.unwrap_or(Mode::Walk)))
}

/// `ANY SHORTEST`, `ALL SHORTEST`, `ANY [k]`, `SHORTEST k`, `ALL`, or none.
fn parse_selector(t: &mut Tokens) -> Result<Selector> {
    let count = |t: &mut Tokens, what: &str| -> Result<Option<u32>> {
        let Some(Token::Number(n, _)) = t.peek(0) else {
            return Ok(None);
        };
        let k = n.parse::<u32>().ok().filter(|&k| k > 0).ok_or_else(|| {
            DataFusionError::Plan(format!(
                "in GRAPH_TABLE: {what} takes a whole number of paths above 0; got {n}"
            ))
        })?;
        t.next();
        Ok(Some(k))
    };
    if t.is_word(0, "ANY") && t.is_word(1, "SHORTEST") {
        t.next();
        t.next();
        if matches!(t.peek(0), Some(Token::Number(..))) {
            return pattern_err(
                "ANY SHORTEST keeps one path; for several, write SHORTEST k, as SHORTEST 3",
            );
        }
        return Ok(Selector::Shortest(1));
    }
    if t.is_word(0, "ALL") && t.is_word(1, "SHORTEST") {
        t.next();
        t.next();
        return Ok(Selector::AllShortest);
    }
    if t.is_word(0, "ANY") {
        t.next();
        return Ok(Selector::Shortest(count(t, "ANY")?.unwrap_or(1)));
    }
    if t.is_word(0, "SHORTEST") {
        t.next();
        let Some(k) = count(t, "SHORTEST")? else {
            return pattern_err(format!(
                "SHORTEST takes a number of paths, as SHORTEST 3; found {}",
                t.found()
            ));
        };
        if t.is_word(0, "GROUP") || t.is_word(0, "GROUPS") {
            return pattern_err(
                "SHORTEST k GROUP is not supported yet; use SHORTEST k or ALL SHORTEST",
            );
        }
        return Ok(Selector::Shortest(k));
    }
    if t.is_word(0, "ALL") {
        t.next();
    }
    Ok(Selector::All)
}

fn parse_node(t: &mut Tokens) -> Result<Spec> {
    t.expect(&Token::LParen, "'(' to start a node")?;
    let spec = parse_spec(t, None)?;
    t.expect(&Token::RParen, "')' to close the node")?;
    Ok(spec)
}

/// `[var] [:label | IS label] [WHERE condition]`, up to `close`.
/// `edge` is the edge's opening, `-` or `<-`, for an edge (closed by `]`);
/// `None` for a node (closed by `)`).
fn parse_spec(t: &mut Tokens, edge: Option<&str>) -> Result<Spec> {
    let close = &match edge {
        Some(_) => Token::RBracket,
        None => Token::RParen,
    };
    let start = t.at;
    let mut spec = Spec::default();
    if matches!(t.peek(0), Some(Token::Word(_))) && !t.is_word(0, "WHERE") && !t.is_word(0, "IS") {
        spec.var = Some(t.identifier("a variable")?);
    }
    if t.eat(&Token::Colon) || (t.is_word(0, "IS") && t.next().is_some()) {
        spec.label = Some(t.identifier("a label")?);
        if matches!(
            t.peek(0),
            Some(Token::Pipe | Token::Ampersand | Token::ExclamationMark | Token::Mod)
        ) {
            return pattern_err(LABEL_EXPRESSIONS);
        }
    }
    // Cypher habits: a property map, and a range inside an edge's brackets.
    let written = |t: &Tokens| {
        let last = t.at.checked_sub(1).and_then(|i| t.tokens.get(i));
        match (start < t.at, t.tokens.get(start), last) {
            (true, Some((_, from, _)), Some((_, _, to))) => t.text[*from..*to].to_string(),
            _ => String::new(),
        }
    };
    if t.peek(0) == Some(&Token::LBrace) {
        return pattern_err(property_map(t, &spec, &written(t), edge));
    }
    if let (Some(open), Some(Token::Mul)) = (edge, t.peek(0)) {
        return pattern_err(cypher_range(t, &written(t), open));
    }
    if t.is_word(0, "WHERE") {
        t.next();
        let close = close.clone();
        spec.condition = t.sql_until(Sql::Condition, move |t, n| t.peek(n) == Some(&close))?;
        if spec.condition.is_none() {
            return pattern_err("WHERE needs a condition");
        }
    }
    if t.peek(0) != Some(close) {
        return pattern_err(format!(
            "expected a variable, ':label', WHERE or '{close}', found {}",
            t.found()
        ));
    }
    Ok(spec)
}

/// The refusal of a Cypher property map, `(a:Person {name: 'Daniel'})`, at
/// the `{`: with the condition to write instead, when the map is simple.
/// `written` is the spec's text before the map; `edge`, the edge's opening.
fn property_map(t: &Tokens, spec: &Spec, written: &str, edge: Option<&str>) -> String {
    let var = spec
        .var
        .clone()
        .unwrap_or_else(|| if edge.is_some() { "e" } else { "a" }.to_string());
    let head = match spec.var {
        Some(_) => written.trim().to_string(),
        None => format!("{var}{}", written.trim()),
    };
    // The entries, `key: value`, up to the `}` that closes the map.
    let mut conditions = Vec::new();
    let mut i = t.at + 1;
    let simple = loop {
        let (Some(Token::Word(_)), Some(Token::Colon)) = (
            t.tokens.get(i).map(|x| &x.0),
            t.tokens.get(i + 1).map(|x| &x.0),
        ) else {
            break false;
        };
        let key = t.raw(i);
        let from = i + 2;
        let mut depth = 0i32;
        let mut j = from;
        while let Some((token, _, _)) = t.tokens.get(j) {
            match token {
                Token::LParen | Token::LBracket | Token::LBrace => depth += 1,
                Token::RParen | Token::RBracket => depth -= 1,
                Token::RBrace if depth > 0 => depth -= 1,
                Token::RBrace | Token::Comma if depth == 0 => break,
                _ => {}
            }
            j += 1;
        }
        if j == from || j >= t.tokens.len() {
            break false;
        }
        let value = &t.text[t.tokens[from].1..t.tokens[j - 1].2];
        conditions.push(format!("{var}.{key} = {value}"));
        if t.tokens[j].0 == Token::RBrace {
            break true;
        }
        i = j + 1;
    };
    let (open, close) = match edge {
        Some(_) => ("[", "]"),
        None => ("(", ")"),
    };
    let example = match simple && !conditions.is_empty() {
        true => format!("{open}{head} WHERE {}{close}", conditions.join(" AND ")),
        false => format!("{open}{head} WHERE {var}.name = 'Daniel'{close}"),
    };
    format!("property maps ({{…}}) are Cypher; write the condition with WHERE, as {example}")
}

/// The refusal of a Cypher range inside an edge's brackets,
/// `-[k:knows*1..3]->`, at the `*`: with the quantifier to write after the
/// edge instead. `written` is the spec's text before the `*`; `open`, the
/// edge's opening.
fn cypher_range(t: &Tokens, written: &str, open: &str) -> String {
    // The range's text, up to the `]`.
    let mut j = t.at;
    while j < t.tokens.len() && t.tokens[j].0 != Token::RBracket {
        j += 1;
    }
    let close = match t.tokens.get(j + 1).map(|x| &x.0) {
        Some(Token::Arrow) => "->",
        _ if open == "<-" => "-",
        _ => "-",
    };
    let range: String = match j > t.at {
        true => t.text[t.tokens[t.at].1..t.tokens[j - 1].2]
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect(),
        false => "*".to_string(),
    };
    let bounds = range.trim_start_matches('*');
    // Cypher's `*` is one or more edges; `*n` exactly n; `*m..n` m to n.
    let quantifier = match bounds.split_once("..") {
        None if bounds.is_empty() => Some("+".to_string()),
        None => bounds.parse::<u32>().ok().map(|n| format!("{{{n}}}")),
        Some((m, n)) => {
            let m = match m {
                "" => Some(1),
                m => m.parse::<u32>().ok(),
            };
            let n = match n {
                "" => Some(None),
                n => n.parse::<u32>().ok().map(Some),
            };
            match (m, n) {
                (Some(m), Some(Some(n))) => Some(format!("{{{m},{n}}}")),
                (Some(m), Some(None)) => Some(format!("{{{m},}}")),
                _ => None,
            }
        }
    };
    let quantifier = quantifier.unwrap_or_else(|| "{1,3}".to_string());
    format!(
        "{range} inside an edge's brackets is Cypher's range; in SQL/PGQ the quantifier follows the edge: write {open}[{}]{close}{quantifier}",
        written.trim()
    )
}

/// The refusal of an undirected edge written with `~`.
const TILDE_EDGES: &str =
    "Ursa's edges are directed; use -[e]- to follow them either way (~[e]~, <~[e]~, ~[e]~> and ~ are for undirected edges)";

/// An edge, if one is next: `-[…]->`, `<-[…]-`, `-[…]-`, `<-[…]->`, `->`,
/// `<-`, `-` or `<->`, and its quantifier, if it has one. `<-[…]->` and
/// `<->` are "left or right": every edge of an Ursa graph is directed, so
/// they mean `-[…]-` and `-`. Edges written with `~` (undirected) are
/// refused.
fn parse_edge(t: &mut Tokens) -> Result<Option<(Spec, Direction, Option<Quantifier>)>> {
    let edge = match (t.peek(0), t.peek(1)) {
        // Abbreviated edges.
        (Some(Token::Arrow), _) => {
            t.next();
            (Spec::default(), Direction::Right)
        }
        (Some(Token::TwoWayArrow), _) => {
            t.next();
            (Spec::default(), Direction::Any)
        }
        (Some(Token::Lt), Some(Token::Arrow)) => {
            t.next();
            t.next();
            (Spec::default(), Direction::Any)
        }
        (Some(Token::Lt), Some(Token::Minus)) if t.peek(2) != Some(&Token::LBracket) => {
            t.next();
            t.next();
            if t.eat(&Token::Gt) {
                (Spec::default(), Direction::Any)
            } else {
                (Spec::default(), Direction::Left)
            }
        }
        (Some(Token::Tilde), _) | (Some(Token::Lt), Some(Token::Tilde)) => {
            return pattern_err(TILDE_EDGES)
        }
        (Some(Token::Minus), Some(Token::LBracket)) => {
            t.next();
            t.next();
            let spec = parse_spec(t, Some("-"))?;
            t.expect(&Token::RBracket, "']' to close the edge")?;
            let direction = match t.next() {
                Some(Token::Arrow) => Direction::Right,
                // `- >`, as DuckPGQ also reads it.
                Some(Token::Minus) if t.peek(0) == Some(&Token::Gt) => {
                    t.next();
                    Direction::Right
                }
                Some(Token::Minus) => Direction::Any,
                Some(Token::LongArrow) => {
                    return pattern_err("expected '->' or '-' after ']', found '->>'")
                }
                Some(Token::Tilde) => return pattern_err(TILDE_EDGES),
                _ => {
                    t.at -= 1;
                    return pattern_err(format!(
                        "expected '->' or '-' after ']', found {}",
                        t.found()
                    ));
                }
            };
            (spec, direction)
        }
        (Some(Token::Lt), Some(Token::Minus)) => {
            t.next();
            t.next();
            t.expect(&Token::LBracket, "'[' after '<-'")?;
            let spec = parse_spec(t, Some("<-"))?;
            t.expect(&Token::RBracket, "']' to close the edge")?;
            let direction = match t.next() {
                // `<-[e]->`: left or right.
                Some(Token::Arrow) => Direction::Any,
                Some(Token::Minus) if t.eat(&Token::Gt) => Direction::Any,
                Some(Token::Minus) => Direction::Left,
                Some(Token::Tilde) => return pattern_err(TILDE_EDGES),
                _ => {
                    t.at -= 1;
                    return pattern_err(format!(
                        "expected '-' or '->' after ']', found {}",
                        t.found()
                    ));
                }
            };
            (spec, direction)
        }
        (Some(Token::Minus), _) => {
            t.next();
            (Spec::default(), Direction::Any)
        }
        _ => return Ok(None),
    };
    let quantifier = parse_quantifier(t)?;
    Ok(Some((edge.0, edge.1, quantifier)))
}

/// `*`, `+`, `?`, `{n}`, `{m,n}`, `{m,}`, `{,n}` or `{,}`, if one is next.
fn parse_quantifier(t: &mut Tokens) -> Result<Option<Quantifier>> {
    let q = |min, max| Ok(Some(Quantifier { min, max }));
    match t.peek(0) {
        Some(Token::Mul) => {
            t.next();
            q(0, None)
        }
        Some(Token::Plus) => {
            t.next();
            q(1, None)
        }
        Some(Token::Placeholder(p)) if p == "?" => {
            t.next();
            q(0, Some(1))
        }
        Some(Token::LBrace) => {
            t.next();
            let bound = |t: &mut Tokens| -> Result<Option<u32>> {
                match t.peek(0) {
                    Some(Token::Number(n, _)) => {
                        let n = n.parse::<u32>().map_err(|_| {
                            DataFusionError::Plan(format!(
                                "in GRAPH_TABLE: a quantifier's bounds are whole numbers from 0; got {n}"
                            ))
                        })?;
                        t.next();
                        Ok(Some(n))
                    }
                    _ => Ok(None),
                }
            };
            let min = bound(t)?;
            let max = if t.eat(&Token::Comma) {
                bound(t)?
            } else if min.is_some() {
                min
            } else {
                return pattern_err(format!(
                    "expected a number or ',' in the quantifier, found {}",
                    t.found()
                ));
            };
            t.expect(&Token::RBrace, "'}' to close the quantifier")?;
            let min = min.unwrap_or(0);
            if max.is_some_and(|max| max < min) {
                return pattern_err(format!(
                    "the quantifier's lower bound {min} is above its upper bound {}",
                    max.unwrap()
                ));
            }
            q(min, max)
        }
        _ => Ok(None),
    }
}

/// Where a vertex variable reads from.
#[derive(Debug, Clone)]
enum VertexSource<'g> {
    Table(&'g VertexTable),
    /// The graph's `nodes` table: a graph without vertex tables.
    Nodes,
}

/// One variable of the pattern.
#[derive(Debug, Default)]
struct Binding<'g> {
    is_edge: bool,
    label: Option<String>,
    vertex: Option<VertexSource<'g>>,
    edge: Option<&'g EdgeTable>,
    conditions: Vec<String>,
}

/// One edge of the pattern, with its two ends.
struct Step {
    left: String,
    edge: String,
    right: String,
    direction: Direction,
    quantifier: Option<Quantifier>,
    /// The path it is in.
    path: usize,
}

/// A column of a variable: (variable, column).
pub type VarColumn = (String, String);

/// What a variable reads: an element table (or the graph's `nodes` table),
/// all its columns or `columns` as (column, property name).
#[derive(Debug, Clone, PartialEq)]
pub struct Source {
    pub var: String,
    pub table: TableReference,
    /// `None`: every column, under its own name.
    pub columns: Option<Vec<(String, String)>>,
    /// Whether the match joins it: a node whose key is a segment's column
    /// ([`Lowered::aliases`]) is read only for its start keys, if at all.
    pub joined: bool,
    /// For a node of a vertex table: the table's element name and its key,
    /// which must identify one row (#411).
    pub vertex: Option<(String, String)>,
}

/// A variable's columns, as [`Source::columns`] lists them.
type Columns = Option<Vec<(String, String)>>;

/// A condition the lowering adds.
#[derive(Debug, Clone, PartialEq)]
pub enum Join {
    Equal(VarColumn, VarColumn),
    /// Not equal: keeps a self-loop out of an undirected edge's reverse branch.
    NotEqual(VarColumn, VarColumn),
}

/// An edge searched on the graph index rather than joined: a quantified
/// edge, or the edge of a path with a path variable, a selector or a mode.
/// It is a relation of paths, named `var`, with the columns of
/// [`super::path_search`]: the path's first and last node, its length, and
/// its nodes and edges.
#[derive(Debug, Clone, PartialEq)]
pub struct Segment {
    /// The relation's name: the path variable, or an unnamed one.
    pub var: String,
    /// The path variable, if the path has one.
    pub path: Option<String>,
    /// The edge variable it searches (an unnamed edge's is `__ursa_…`).
    pub edge: String,
    /// The edge table, and its source and destination columns.
    pub edges: TableReference,
    pub source: String,
    pub destination: String,
    /// The node variables at its two ends, and their keys.
    pub start: String,
    pub start_key: VarColumn,
    pub end: String,
    pub end_key: VarColumn,
    pub direction: Direction,
    pub quantifier: Quantifier,
    pub selector: Selector,
    pub mode: Mode,
    /// A fixed-length edge expanded through the index, not a searched path.
    pub fixed: bool,
    pub starts: Starts,
    /// Whether the index's nodes are the start node's table's keys, so a
    /// start that is not a node of the index is no node at all.
    pub known_only: bool,
    /// The keys the last node can have, when they are known: the values its
    /// conditions name ([`Starts::Node`]: it is bound, see [`Usage::bound`]),
    /// or the keys an earlier segment reached. The search's targets.
    pub targets: Option<Starts>,
}

/// What the query reads and binds, from its conditions and `COLUMNS`: what
/// decides which edges are expanded through the graph index (see the module
/// docs). The default plans joins, as a pattern was planned before.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Usage {
    /// Whether the rest was found: if not, every edge is joined and every
    /// node too.
    pub enabled: bool,
    /// Per variable, the properties the query reads of it: `None` for all of
    /// them (`v.*`). A variable that is not listed is not read.
    pub reads: BTreeMap<String, Option<BTreeSet<String>>>,
    /// Per variable with a condition on it alone that limits a property to
    /// values the query names (`v.p = 1`, `v.p IN (…)`), those properties.
    pub bound: BTreeMap<String, BTreeSet<String>>,
    /// The edge tables whose index is kept between queries: the ones fixed
    /// edges may be expanded on. Empty: every edge is joined.
    pub indexed: BTreeSet<TableReference>,
}

/// Where a segment's start nodes come from.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Starts {
    /// The start node's keys that the conditions on it alone keep.
    Node,
    /// The keys an earlier segment reached: its last node's (`end`), or its
    /// first's.
    After { segment: usize, end: bool },
}

/// A node not joined to its table: its key is a segment's column.
#[derive(Debug, Clone, PartialEq)]
pub struct Alias {
    pub var: String,
    /// The segment column its key is.
    pub column: VarColumn,
}

/// A pattern resolved against a graph: what to scan and join.
#[derive(Debug, Clone, PartialEq)]
pub struct Lowered {
    /// One per variable, in order of appearance, except the edges searched as
    /// segments.
    pub sources: Vec<Source>,
    /// The edges searched on the graph index, joined to their end nodes in
    /// every branch. A segment's [`Starts::After`] names an earlier one.
    pub segments: Vec<Segment>,
    /// The nodes whose sources are not joined: their keys are segment columns.
    pub aliases: Vec<Alias>,
    /// One per orientation of the undirected edges: the joins of each branch
    /// of the `UNION ALL` (one branch when every edge has a direction).
    pub branches: Vec<Vec<Join>>,
    /// Whether the match planned as joins would be a `UNION ALL`: it has an
    /// undirected fixed edge, joined or not.
    pub union: bool,
    /// The pattern's conditions as written: in elements, then after the paths.
    pub conditions: Vec<String>,
    /// The `COLUMNS` list as written.
    pub columns: String,
    /// What each variable is, for the result's field metadata
    /// ([`super::provenance`]): every named node, edge and path variable.
    pub variables: Vec<Variable>,
    /// The pattern's edges, in the order written, for the result's
    /// `ursa.pattern`.
    pub edges: Vec<PatternEdge>,
}

/// A pattern variable, as a result's field metadata describes it.
#[derive(Debug, Clone, PartialEq)]
pub struct Variable {
    pub var: String,
    pub kind: VariableKind,
}

/// What a pattern variable binds.
#[derive(Debug, Clone, PartialEq)]
pub enum VariableKind {
    /// A node: its vertex table (element name), if the graph has one, and
    /// the property names of its key.
    Node {
        label: Option<String>,
        keys: Vec<String>,
    },
    /// An edge of the edge table `label`.
    Edge { label: String },
    /// A path over the edge table `edges`, between nodes of the vertex table
    /// `vertices`, if the graph has one.
    Path {
        edges: String,
        vertices: Option<String>,
    },
}

impl Lowered {
    /// The field metadata of `column` of the relation `relation` (a
    /// variable), if it is something a result should say: a node's key, a
    /// property, or a path's nodes, edges or elements.
    pub fn provenance(
        &self,
        graph: &str,
        relation: &str,
        column: &str,
    ) -> Option<BTreeMap<String, String>> {
        use super::path_search::{EDGES, ELEMENTS, VERTICES};
        let variable = self.variables.iter().find(|v| v.var == relation)?;
        let (role, label) = match &variable.kind {
            VariableKind::Node { label, keys } => match keys.iter().any(|k| k == column) {
                true => (provenance::ROLE_NODE, label.as_deref()),
                false => (provenance::ROLE_PROPERTY, label.as_deref()),
            },
            VariableKind::Edge { label } => (provenance::ROLE_PROPERTY, Some(label.as_str())),
            VariableKind::Path { edges, vertices } => match column {
                VERTICES => (provenance::ROLE_PATH_NODES, vertices.as_deref()),
                EDGES => (provenance::ROLE_PATH_EDGES, Some(edges.as_str())),
                ELEMENTS => (provenance::ROLE_PATH_ELEMENTS, None),
                _ => return None,
            },
        };
        let mut metadata = provenance::metadata(role, graph, label);
        metadata.insert(provenance::VAR.to_string(), relation.to_string());
        Some(metadata)
    }
}

/// Resolve `pattern` against `graph`. `schema_of` gives a table's columns
/// (for properties).
pub fn lower(
    pattern: &Pattern,
    graph: &PropertyGraph,
    schema_of: &dyn Fn(&TableReference) -> Result<SchemaRef>,
    usage: &Usage,
) -> Result<Lowered> {
    // Collect the variables, naming anonymous ones.
    let mut bindings: BTreeMap<String, Binding> = BTreeMap::new();
    let mut order: Vec<String> = Vec::new();
    let mut steps = Vec::new();
    let mut anonymous = 0;
    for (path_index, path) in pattern.paths.iter().enumerate() {
        // (left node, direction, edge, quantifier)
        let mut previous: Option<(String, Direction, String, Option<Quantifier>)> = None;
        for element in &path.elements {
            let (spec, is_edge) = match element {
                Element::Node(spec) => (spec, false),
                Element::Edge(spec, _, _) => (spec, true),
            };
            let var = match &spec.var {
                Some(v) => v.clone(),
                None => {
                    anonymous += 1;
                    format!("__ursa_{anonymous}")
                }
            };
            let binding = bindings.entry(var.clone()).or_insert_with(|| {
                order.push(var.clone());
                Binding {
                    is_edge,
                    ..Binding::default()
                }
            });
            if binding.is_edge != is_edge {
                return pattern_err(format!("'{var}' is used for both a node and an edge"));
            }
            if is_edge
                && (binding.label.is_some()
                    || !binding.conditions.is_empty()
                    || steps.iter().any(|s: &Step| s.edge == var))
            {
                return pattern_err(format!(
                    "edge variable '{var}' appears twice; an edge variable binds one edge"
                ));
            }
            if let Some(label) = &spec.label {
                if binding.label.as_ref().is_some_and(|l| l != label) {
                    return pattern_err(format!(
                        "'{var}' has two labels, {} and {label}",
                        binding.label.as_ref().unwrap()
                    ));
                }
                binding.label = Some(label.clone());
            }
            binding.conditions.extend(spec.condition.clone());
            match element {
                Element::Node(_) => {
                    if let Some((left, direction, edge, quantifier)) = previous.take() {
                        steps.push(Step {
                            left,
                            edge,
                            right: var.clone(),
                            direction,
                            quantifier,
                            path: path_index,
                        });
                    }
                    previous = Some((var, Direction::Right, String::new(), None));
                }
                Element::Edge(_, direction, quantifier) => {
                    let (left, ..) = previous.take().expect("a path starts with a node");
                    previous = Some((left, *direction, var, *quantifier));
                }
            }
        }
    }

    // Labels as written.
    for (var, b) in bindings.iter_mut() {
        let Some(label) = &b.label else { continue };
        if b.is_edge {
            let tables: Vec<&EdgeTable> = graph
                .edge_tables
                .iter()
                .filter(|e| e.answers_to(label))
                .collect();
            match tables.as_slice() {
                [only] => b.edge = Some(only),
                [] => return pattern_err(format!(
                    "graph '{}' has no edge table or edge label '{label}' (for '{}'); its edge tables are {}",
                    graph.name, shown(var), names(graph.edge_tables.iter().map(|e| e.name.as_str()))
                )),
                _ => return pattern_err(format!(
                    "label '{label}' is on several edge tables; a label shared by several tables is not supported yet"
                )),
            }
        } else {
            let tables: Vec<&VertexTable> = graph
                .vertex_tables
                .iter()
                .filter(|v| v.answers_to(label))
                .collect();
            match tables.as_slice() {
                [only] => b.vertex = Some(VertexSource::Table(only)),
                [] => return pattern_err(format!(
                    "graph '{}' has no vertex table or vertex label '{label}' (for '{}'); its vertex tables are {}",
                    graph.name, shown(var), names(graph.vertex_tables.iter().map(|v| v.name.as_str()))
                )),
                _ => return pattern_err(format!(
                    "label '{label}' is on several vertex tables; a label shared by several tables is not supported yet"
                )),
            }
        }
    }

    // Infer the rest from the edges' REFERENCES, until nothing changes.
    loop {
        let mut changed = false;
        for step in &steps {
            let left = vertex_table(&bindings[&step.left]);
            let right = vertex_table(&bindings[&step.right]);
            let edge = bindings[&step.edge].edge;
            match edge {
                None => {
                    let candidates: Vec<&EdgeTable> = graph
                        .edge_tables
                        .iter()
                        .filter(|e| fits(graph, e, left, right, step.direction))
                        .collect();
                    if let [only] = candidates.as_slice() {
                        bindings.get_mut(&step.edge).unwrap().edge = Some(only);
                        changed = true;
                    }
                }
                Some(e) => {
                    let (from, to) = (
                        graph.endpoint_vertex(e, true),
                        graph.endpoint_vertex(e, false),
                    );
                    let (l, r) = match step.direction {
                        Direction::Right => (from, to),
                        Direction::Left => (to, from),
                        Direction::Any if from.map(|v| &v.name) == to.map(|v| &v.name) => {
                            (from, to)
                        }
                        Direction::Any => (None, None),
                    };
                    for (var, table) in [(&step.left, l), (&step.right, r)] {
                        let b = bindings.get_mut(var).unwrap();
                        if b.vertex.is_none() {
                            if let Some(table) = table {
                                b.vertex = Some(VertexSource::Table(table));
                                changed = true;
                            }
                        }
                    }
                }
            }
        }
        if !changed {
            break;
        }
    }

    // What is still open: the graph's only table of its kind, or a refusal.
    // Edges first: labelling an edge usually tells its nodes too.
    for (var, b) in bindings.iter_mut() {
        if b.is_edge && b.edge.is_none() {
            match graph.edge_tables.as_slice() {
                [only] => b.edge = Some(only),
                [] => return pattern_err(format!("graph '{}' has no edge tables", graph.name)),
                _ => return pattern_err(format!(
                    "which edge table {} is cannot be told from the pattern; give it a label, as -[{}:{}]->",
                    describe_var(var, true),
                    if var.starts_with("__ursa_") { "" } else { var.as_str() },
                    graph.edge_tables[0].labels()[0].name
                )),
            }
        }
    }
    for (var, b) in bindings.iter_mut() {
        if !b.is_edge && b.vertex.is_none() {
            match graph.vertex_tables.as_slice() {
                [only] => b.vertex = Some(VertexSource::Table(only)),
                [] if graph.default_pair().is_some() => b.vertex = Some(VertexSource::Nodes),
                [] => return pattern_err(format!(
                    "graph '{}' has no vertex tables and several edge tables, so its nodes cannot be matched",
                    graph.name
                )),
                _ => return pattern_err(format!(
                    "which vertex table {} is cannot be told from the pattern; give it a label, as ({}:{})",
                    describe_var(var, false),
                    if var.starts_with("__ursa_") { "" } else { var.as_str() },
                    graph.vertex_tables[0].labels()[0].name
                )),
            }
        }
    }

    // An edge binds only between the vertex tables its edge table REFERENCES
    // (#410): a node of another table is refused, not joined on key values
    // that happen to be equal.
    for step in &steps {
        let e = bindings[&step.edge].edge.expect("resolved");
        let left = vertex_table(&bindings[&step.left]);
        let right = vertex_table(&bindings[&step.right]);
        if !fits(graph, e, left, right, step.direction) {
            return pattern_err(contradiction(graph, e, step, &bindings));
        }
    }

    // The edges searched on the index: quantified edges, and the edge of a
    // path with a variable, a selector or a mode.
    let mut searched: Vec<(usize, Option<String>, Selector, Mode)> = Vec::new(); // (step, path var, …)
    for (i, path) in pattern.paths.iter().enumerate() {
        let edges: Vec<usize> = (0..steps.len()).filter(|&s| steps[s].path == i).collect();
        let whole = path.var.is_some() || path.selector != Selector::All || path.mode != Mode::Walk;
        if let Some(var) = &path.var {
            if bindings.contains_key(var)
                || pattern.paths[..i]
                    .iter()
                    .any(|p| p.var.as_ref() == Some(var))
            {
                return pattern_err(format!(
                    "'{var}' names a path and another variable; give the path its own name"
                ));
            }
        }
        if whole && edges.len() > 1 {
            return pattern_err(
                "a path with a path variable, ANY/ALL SHORTEST or a path mode must be one edge between two nodes for now, as p = ANY SHORTEST (a)-[e]->*(b)",
            );
        }
        if whole && edges.is_empty() {
            if path.var.is_some() {
                return pattern_err(
                    "a path variable needs a path with an edge, as p = (a)-[e]->*(b)",
                );
            }
            continue;
        }
        for s in edges {
            if whole {
                searched.push((s, path.var.clone(), path.selector, path.mode));
            } else if steps[s].quantifier.is_some() {
                searched.push((s, None, Selector::All, Mode::Walk));
            }
        }
    }
    for (s, _, selector, mode) in &searched {
        let step = &steps[*s];
        let quantifier = step.quantifier.unwrap_or(Quantifier::ONCE);
        let b = &bindings[&step.edge];
        let e = b.edge.expect("resolved");
        let edge = describe_var(&step.edge, true);
        if !b.conditions.is_empty() {
            return pattern_err(format!(
                "{edge} is quantified or in a searched path, so it stands for several edges; a condition on it is not supported yet"
            ));
        }
        if *selector == Selector::All && *mode == Mode::Walk && quantifier.max.is_none() {
            return pattern_err(format!(
                "{edge} has no upper bound, so its walks are endless; give it one, as {{1,3}}, keep the shortest with ANY SHORTEST, or use TRAIL, SIMPLE or ACYCLIC"
            ));
        }
        let ends = (
            graph.endpoint_vertex(e, true).map(|v| v.name.as_str()),
            graph.endpoint_vertex(e, false).map(|v| v.name.as_str()),
        );
        if ends.0 != ends.1
            && (quantifier.min != 1
                || quantifier.max != Some(1)
                || step.direction == Direction::Any)
        {
            return pattern_err(format!(
                "{edge} joins two vertex tables ({} and {}), so it cannot repeat, be undirected or match no edge; only a quantified edge within one vertex table can",
                ends.0.unwrap_or("?"),
                ends.1.unwrap_or("?")
            ));
        }
    }
    let is_searched = |i: usize| searched.iter().any(|(s, ..)| *s == i);

    // Each node variable's columns, and its key's property names.
    let mut node_columns: BTreeMap<&str, (TableReference, Columns)> = BTreeMap::new();
    for var in &order {
        let b = &bindings[var];
        if b.is_edge {
            continue;
        }
        let read = match b.vertex.as_ref().expect("resolved") {
            VertexSource::Table(v) => (
                v.table.clone(),
                properties(&v.table, &v.labels(), &[v.key.as_str()], schema_of)?,
            ),
            VertexSource::Nodes => (
                TableReference::partial(graph.name.as_str(), super::schema::NODES),
                None,
            ),
        };
        node_columns.insert(var.as_str(), read);
    }
    let key = |var: &str| -> VarColumn {
        match bindings[var].vertex.as_ref().expect("resolved") {
            VertexSource::Table(v) => (var.to_string(), v.key.clone()),
            VertexSource::Nodes => (var.to_string(), super::schema::ID.to_string()),
        }
    };
    let key_names = |var: &str| -> Vec<String> {
        let (_, column) = key(var);
        match &node_columns[var].1 {
            None => vec![column],
            Some(columns) => columns
                .iter()
                .filter(|(c, _)| *c == column)
                .map(|(_, n)| n.clone())
                .collect(),
        }
    };
    // Bound: a condition on the node alone names the values of its key.
    let is_bound = |var: &str| {
        usage
            .bound
            .get(var)
            .is_some_and(|ps| key_names(var).iter().any(|n| ps.contains(n)))
    };
    let reads_only_its_key = |var: &str| match usage.reads.get(var) {
        _ if !usage.enabled => false,
        None => true,
        Some(None) => false,
        Some(Some(ps)) => {
            let names = key_names(var);
            ps.iter().all(|p| names.contains(p))
        }
    };
    // Whether the index of `e` has the keys of `var`'s table as its nodes.
    let known = |e: &EdgeTable, var: &str| match bindings[var].vertex.as_ref().expect("resolved") {
        VertexSource::Table(v) => {
            graph
                .endpoint_vertex(e, true)
                .is_some_and(|t| t.name == v.name)
                && graph
                    .endpoint_vertex(e, false)
                    .is_some_and(|t| t.name == v.name)
        }
        VertexSource::Nodes => graph.vertex_tables.is_empty() && graph.edge_tables.len() == 1,
    };
    // Whether the index of `e` keeps every edge a join of `e` to `var`'s
    // table keeps: it drops edges to values that are no key of its ends'
    // tables, so `var` must read one of them.
    let in_index = |e: &EdgeTable, var: &str| match (
        graph.endpoint_vertex(e, true),
        graph.endpoint_vertex(e, false),
    ) {
        (Some(s), Some(d)) => {
            vertex_table(&bindings[var]).is_some_and(|v| v.name == s.name || v.name == d.name)
        }
        _ => true,
    };
    let expandable = |i: usize| {
        let step = &steps[i];
        let b = &bindings[&step.edge];
        let e = b.edge.expect("resolved");
        !is_searched(i)
            && b.conditions.is_empty()
            && !usage.reads.contains_key(&step.edge)
            && usage.indexed.contains(&e.table)
            && in_index(e, &step.left)
            && in_index(e, &step.right)
    };

    // From each bound node, the fixed edges of its path, outward, each from
    // the keys the one before reached; and the searched edges on the way,
    // from those keys too (see the module docs). (step, reversed, starts)
    let mut planned: Vec<(usize, bool, Starts)> = Vec::new();
    // Per node, where its keys come from, and the segment count when it
    // was reached.
    let mut reached: BTreeMap<String, (Starts, usize)> = BTreeMap::new();
    for var in &order {
        if !bindings[var].is_edge && is_bound(var) {
            reached.insert(var.clone(), (Starts::Node, 0));
        }
    }
    let mut done = vec![false; pattern.paths.len()];
    loop {
        let mut progress = false;
        for (i, finished) in done.iter_mut().enumerate() {
            if *finished {
                continue;
            }
            let path_steps: Vec<usize> = (0..steps.len()).filter(|&s| steps[s].path == i).collect();
            let Some(&first) = path_steps.first() else {
                *finished = true;
                continue;
            };
            let mut nodes = vec![steps[first].left.clone()];
            nodes.extend(path_steps.iter().map(|&s| steps[s].right.clone()));
            let Some(root) = nodes.iter().position(|n| reached.contains_key(n)) else {
                continue;
            };
            *finished = true;
            progress = true;
            for (j, &s) in path_steps.iter().enumerate().skip(root) {
                let Some(&(from, _)) = reached.get(&nodes[j]) else {
                    break;
                };
                if !is_searched(s) && !expandable(s) {
                    break;
                }
                planned.push((s, false, from));
                let segment = planned.len() - 1;
                reached
                    .entry(nodes[j + 1].clone())
                    .or_insert((Starts::After { segment, end: true }, planned.len()));
            }
            for j in (0..root).rev() {
                let s = path_steps[j];
                let Some(&(from, _)) = reached.get(&nodes[j + 1]) else {
                    break;
                };
                // A search runs from its first node: it cannot be reversed.
                if is_searched(s) || !expandable(s) {
                    break;
                }
                planned.push((s, true, from));
                let segment = planned.len() - 1;
                reached
                    .entry(nodes[j].clone())
                    .or_insert((Starts::After { segment, end: true }, planned.len()));
            }
        }
        if !progress {
            break;
        }
    }
    // The searched edges no bound node reached run from their first node.
    for (s, ..) in &searched {
        if !planned.iter().any(|(p, ..)| p == s) {
            planned.push((*s, false, Starts::Node));
        }
    }
    let is_expanded = |i: usize| planned.iter().any(|(s, ..)| *s == i);

    let reverse = |d: Direction| match d {
        Direction::Right => Direction::Left,
        Direction::Left => Direction::Right,
        Direction::Any => Direction::Any,
    };
    let mut segments: Vec<Segment> = Vec::new();
    for &(s, reversed, starts) in &planned {
        let step = &steps[s];
        let e = bindings[&step.edge].edge.expect("resolved");
        let (start, end) = match reversed {
            false => (step.left.clone(), step.right.clone()),
            true => (step.right.clone(), step.left.clone()),
        };
        let search = searched.iter().find(|(x, ..)| *x == s);
        let (path, selector, mode) = match search {
            Some((_, path, selector, mode)) => (path.clone(), *selector, *mode),
            None => (None, Selector::All, Mode::Walk),
        };
        let var = match &path {
            Some(p) => p.clone(),
            None => {
                anonymous += 1;
                format!("__ursa_{anonymous}")
            }
        };
        // The keys the last node can have, when they are known by now.
        let targets = match reached.get(&end) {
            _ if is_bound(&end) => Some(Starts::Node),
            Some(&(after @ Starts::After { .. }, when)) if when <= segments.len() => Some(after),
            _ => None,
        };
        segments.push(Segment {
            var,
            path,
            edge: step.edge.clone(),
            edges: e.table.clone(),
            source: e.source.clone(),
            destination: e.destination.clone(),
            known_only: known(e, &start),
            start_key: key(&start),
            start,
            end_key: key(&end),
            end,
            direction: if reversed {
                reverse(step.direction)
            } else {
                step.direction
            },
            quantifier: step.quantifier.unwrap_or(Quantifier::ONCE),
            selector,
            mode,
            fixed: search.is_none(),
            starts,
            targets,
        });
    }

    // Nodes the query reads only the key of, at the end of a segment whose
    // index's nodes are their table's keys: their key is that segment's column.
    let mut aliases: Vec<Alias> = Vec::new();
    for var in &order {
        if bindings[var].is_edge || !reads_only_its_key(var) {
            continue;
        }
        let at = segments
            .iter()
            .zip(&planned)
            .find_map(|(segment, &(s, ..))| {
                let e = bindings[&steps[s].edge].edge.expect("resolved");
                if !known(e, var) {
                    return None;
                }
                let column = if &segment.start == var {
                    super::path_search::START
                } else if &segment.end == var {
                    super::path_search::END
                } else {
                    return None;
                };
                Some((segment.var.clone(), column.to_string()))
            });
        if let Some(column) = at {
            aliases.push(Alias {
                var: var.clone(),
                column,
            });
        }
    }
    // Where a node's key is read: its table's key column, or a segment's.
    let at = |var: &str| -> VarColumn {
        match aliases.iter().find(|a| a.var == var) {
            Some(a) => a.column.clone(),
            None => key(var),
        }
    };

    let undirected: Vec<usize> = steps
        .iter()
        .enumerate()
        .filter(|(i, s)| s.direction == Direction::Any && !is_expanded(*i))
        .map(|(i, _)| i)
        .collect();
    if undirected.len() > MAX_UNDIRECTED {
        return pattern_err(format!(
            "a pattern may have at most {MAX_UNDIRECTED} undirected edges (-[e]-); give some a direction"
        ));
    }
    let mut sources = Vec::new();
    for var in &order {
        let b = &bindings[var];
        if b.is_edge && planned.iter().any(|(s, ..)| &steps[*s].edge == var) {
            continue;
        }
        let (table, columns) = if b.is_edge {
            let e = b.edge.expect("resolved");
            let needed = [e.source.as_str(), e.destination.as_str()];
            (
                e.table.clone(),
                properties(&e.table, &e.labels(), &needed, schema_of)?,
            )
        } else {
            node_columns[var.as_str()].clone()
        };
        sources.push(Source {
            var: var.clone(),
            table,
            columns,
            joined: !aliases.iter().any(|a| &a.var == var),
            vertex: match b.is_edge {
                true => None,
                false => vertex_table(b).map(|v| (v.name.clone(), v.key.clone())),
            },
        });
    }

    let mut branches = Vec::new();
    for mask in 0..(1usize << undirected.len()) {
        let mut joins = Vec::new();
        for segment in &segments {
            let column = |name: &str| (segment.var.clone(), name.to_string());
            for (name, var) in [
                (super::path_search::START, &segment.start),
                (super::path_search::END, &segment.end),
            ] {
                let (a, b) = (column(name), at(var));
                if a != b {
                    joins.push(Join::Equal(a, b));
                }
            }
        }
        for (i, step) in steps.iter().enumerate() {
            if is_expanded(i) {
                continue;
            }
            let e = bindings[&step.edge].edge.expect("resolved");
            let src = (step.edge.clone(), e.source.clone());
            let dst = (step.edge.clone(), e.destination.clone());
            let reversed = match step.direction {
                Direction::Right => false,
                Direction::Left => true,
                Direction::Any => {
                    let bit = undirected.iter().position(|&u| u == i).unwrap();
                    let reversed = mask & (1 << bit) != 0;
                    if reversed {
                        // A self-loop matches once, in the forward branch.
                        joins.push(Join::NotEqual(src.clone(), dst.clone()));
                    }
                    reversed
                }
            };
            let (from, to) = if reversed {
                (&step.right, &step.left)
            } else {
                (&step.left, &step.right)
            };
            joins.push(Join::Equal(src, at(from)));
            joins.push(Join::Equal(dst, at(to)));
        }
        branches.push(joins);
    }
    let mut conditions: Vec<String> = order
        .iter()
        .flat_map(|var| bindings[var].conditions.iter().cloned())
        .collect();
    conditions.extend(pattern.condition.clone());
    let union = steps
        .iter()
        .enumerate()
        .any(|(i, s)| s.direction == Direction::Any && !is_searched(i));
    // What the result says about its columns (`provenance`): the named
    // variables, and the pattern's edges.
    let named = |var: &str| (!var.starts_with("__ursa_")).then(|| var.to_string());
    let mut variables: Vec<Variable> = order
        .iter()
        .filter(|var| named(var).is_some())
        .map(|var| {
            let b = &bindings[var];
            let kind = match b.is_edge {
                true => VariableKind::Edge {
                    label: b.edge.expect("resolved").name.clone(),
                },
                false => VariableKind::Node {
                    label: vertex_table(b).map(|v| v.name.clone()),
                    keys: key_names(var),
                },
            };
            Variable {
                var: var.clone(),
                kind,
            }
        })
        .collect();
    for step in &steps {
        let Some(path) = &pattern.paths[step.path].var else {
            continue;
        };
        if variables.iter().any(|v| &v.var == path) {
            continue;
        }
        variables.push(Variable {
            var: path.clone(),
            kind: VariableKind::Path {
                edges: bindings[&step.edge].edge.expect("resolved").name.clone(),
                vertices: vertex_table(&bindings[&step.left]).map(|v| v.name.clone()),
            },
        });
    }
    let edges = steps
        .iter()
        .map(|step| {
            let (from, to) = match step.direction {
                Direction::Left => (&step.right, &step.left),
                Direction::Right | Direction::Any => (&step.left, &step.right),
            };
            PatternEdge {
                from: named(from),
                to: named(to),
                directed: step.direction != Direction::Any,
                edge: named(&step.edge),
                label: bindings[&step.edge].edge.expect("resolved").name.clone(),
                quantifier: step.quantifier.map(|q| (q.min, q.max)),
                path: pattern.paths[step.path].var.clone(),
            }
        })
        .collect();
    Ok(Lowered {
        sources,
        segments,
        aliases,
        branches,
        union,
        conditions,
        columns: pattern.columns.clone(),
        variables,
        edges,
    })
}

/// The vertex table a variable is bound to, if it is bound to one.
fn vertex_table<'g>(b: &Binding<'g>) -> Option<&'g VertexTable> {
    match b.vertex {
        Some(VertexSource::Table(v)) => Some(v),
        _ => None,
    }
}

/// Whether edge table `e` can join `left` to `right` in `direction`, as far
/// as the vertex tables already known say.
fn fits(
    graph: &PropertyGraph,
    e: &EdgeTable,
    left: Option<&VertexTable>,
    right: Option<&VertexTable>,
    direction: Direction,
) -> bool {
    let from = graph.endpoint_vertex(e, true).map(|v| v.name.as_str());
    let to = graph.endpoint_vertex(e, false).map(|v| v.name.as_str());
    let matches = |side: Option<&VertexTable>, end: Option<&str>| match (side, end) {
        (Some(side), Some(end)) => side.name == end,
        _ => true,
    };
    let forward = matches(left, from) && matches(right, to);
    let backward = matches(left, to) && matches(right, from);
    match direction {
        Direction::Right => forward,
        Direction::Left => backward,
        Direction::Any => forward || backward,
    }
}

/// The refusal of `step`, whose edge table `e` does not connect the vertex
/// tables its nodes are bound to.
fn contradiction(
    graph: &PropertyGraph,
    e: &EdgeTable,
    step: &Step,
    bindings: &BTreeMap<String, Binding>,
) -> String {
    let end = |source: bool| graph.endpoint_vertex(e, source).map(|v| v.name.as_str());
    let (from, to) = (end(true).unwrap_or("?"), end(false).unwrap_or("?"));
    // A node as written, and the vertex table it is bound to.
    let node = |var: &str| {
        let b = &bindings[var];
        let table = vertex_table(b).map(|v| v.name.as_str()).unwrap_or("?");
        let written = match (var.starts_with("__ursa_"), &b.label) {
            (true, Some(label)) => format!("(:{label})"),
            (true, None) => "()".to_string(),
            (false, Some(label)) => format!("({var}:{label})"),
            (false, None) => format!("({var})"),
        };
        (written, table)
    };
    let (left, left_table) = node(&step.left);
    let (right, right_table) = node(&step.right);
    let (arrow, written, either) = match step.direction {
        Direction::Right => ("->", format!("from '{left_table}' to '{right_table}'"), ""),
        Direction::Left => ("<-", format!("from '{right_table}' to '{left_table}'"), ""),
        Direction::Any => (
            "-",
            format!("between '{left_table}' and '{right_table}'"),
            " in either direction",
        ),
    };
    format!(
        "edge table '{}' of graph '{}' goes from vertex table '{from}' to '{to}', but {} is written {left}{arrow}{right}, {written}; no edge of '{}' connects them{either}, so the pattern can match nothing",
        e.name,
        graph.name,
        describe_var(&step.edge, true),
        e.name,
    )
}

/// The columns an element table exposes, as (column, property name): `None`
/// when every label exposes every column under its own name; else its
/// labels' properties, plus the `needed` key or endpoint columns.
fn properties(
    table: &TableReference,
    labels: &[Label],
    needed: &[&str],
    schema_of: &dyn Fn(&TableReference) -> Result<SchemaRef>,
) -> Result<Option<Vec<(String, String)>>> {
    let all = labels
        .iter()
        .all(|l| matches!(&l.properties, Properties::AllColumns { except } if except.is_empty()));
    if all {
        return Ok(None);
    }
    let schema = schema_of(table)?;
    let mut items: Vec<(String, String)> = Vec::new(); // (column, property)
    for label in labels {
        let columns: Vec<(String, String)> = match &label.properties {
            Properties::AllColumns { except } => schema
                .fields()
                .iter()
                .map(|f| f.name().clone())
                .filter(|c| !except.contains(c))
                .map(|c| (c.clone(), c))
                .collect(),
            Properties::Columns(columns) => columns.clone(),
            Properties::None => Vec::new(),
        };
        for item in columns {
            if !items.iter().any(|(_, name)| name == &item.1) {
                items.push(item);
            }
        }
    }
    for column in needed {
        if !items.iter().any(|(_, name)| name == column) {
            items.push((column.to_string(), column.to_string()));
        }
    }
    Ok(Some(items))
}

fn names<'a>(names: impl Iterator<Item = &'a str>) -> String {
    let names: Vec<&str> = names.collect();
    if names.is_empty() {
        "none".to_string()
    } else {
        names.join(", ")
    }
}

/// A variable as a message names it.
fn shown(var: &str) -> String {
    if var.starts_with("__ursa_") {
        "an unnamed element".to_string()
    } else {
        var.to_string()
    }
}

fn describe_var(var: &str, edge: bool) -> String {
    match (var.starts_with("__ursa_"), edge) {
        (true, true) => "an unnamed edge".to_string(),
        (true, false) => "an unnamed node".to_string(),
        (false, true) => format!("edge '{var}'"),
        (false, false) => format!("node '{var}'"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(var: Option<&str>, label: Option<&str>, condition: Option<&str>) -> Spec {
        Spec {
            var: var.map(str::to_string),
            label: label.map(str::to_string),
            condition: condition.map(str::to_string),
        }
    }

    fn edges(path: &PathPattern) -> Vec<(Direction, Option<Quantifier>)> {
        path.elements
            .iter()
            .filter_map(|e| match e {
                Element::Edge(_, d, q) => Some((*d, *q)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn parses_a_pattern() {
        let p = parse(
            "social MATCH (a:Person WHERE a.age > (1 + 2))-[k:knows WHERE k.since > 2000]->(b IS Person),
             (b)<-[w]-(c), (c)-(d)-[x]-(e)->(f)<-(g)
             WHERE a.name <> b.name COLUMNS (a.name AS a, concat(b.name, ')') AS b)",
        )
        .unwrap();
        assert_eq!(p.graph, "social");
        assert_eq!(p.condition.as_deref(), Some("a.name <> b.name"));
        assert_eq!(p.columns, "a.name AS a, concat(b.name, ')') AS b");
        assert_eq!(
            p.paths[0].elements,
            vec![
                Element::Node(spec(Some("a"), Some("person"), Some("a.age > (1 + 2)"))),
                Element::Edge(
                    spec(Some("k"), Some("knows"), Some("k.since > 2000")),
                    Direction::Right,
                    None
                ),
                Element::Node(spec(Some("b"), Some("person"), None)),
            ]
        );
        assert_eq!(
            p.paths[1].elements,
            vec![
                Element::Node(spec(Some("b"), None, None)),
                Element::Edge(spec(Some("w"), None, None), Direction::Left, None),
                Element::Node(spec(Some("c"), None, None)),
            ]
        );
        let directions: Vec<Direction> = edges(&p.paths[2]).into_iter().map(|e| e.0).collect();
        assert_eq!(
            directions,
            vec![
                Direction::Any,
                Direction::Any,
                Direction::Right,
                Direction::Left
            ]
        );
        assert_eq!(p.paths[0].selector, Selector::All);
        assert_eq!(p.paths[0].mode, Mode::Walk);
    }

    #[test]
    fn parses_quantifiers_selectors_and_modes() {
        let q = |min, max| Some(Quantifier { min, max });
        for (path, expected) in [
            ("(a)-[e]->*(b)", (Direction::Right, q(0, None))),
            ("(a)-[e]-> *(b)", (Direction::Right, q(0, None))),
            ("(a)-[e]- > *(b)", (Direction::Right, q(0, None))),
            ("(a)-[e]->+(b)", (Direction::Right, q(1, None))),
            ("(a)<-[e]-?(b)", (Direction::Left, q(0, Some(1)))),
            ("(a)-[e]-*(b)", (Direction::Any, q(0, None))),
            ("(a)->{2}(b)", (Direction::Right, q(2, Some(2)))),
            ("(a)-[e]->{1,3}(b)", (Direction::Right, q(1, Some(3)))),
            ("(a)-[e]->{2,}(b)", (Direction::Right, q(2, None))),
            ("(a)-[e]->{,3}(b)", (Direction::Right, q(0, Some(3)))),
            ("(a)-[e]->{,}(b)", (Direction::Right, q(0, None))),
            ("(a)-[e]->(b)", (Direction::Right, None)),
        ] {
            let p = parse(&format!("g MATCH {path} COLUMNS (a.id)")).unwrap();
            assert_eq!(edges(&p.paths[0]), vec![expected], "{path}");
        }
        for (path, var, selector, mode) in [
            (
                "p = ANY SHORTEST (a)-[e]->*(b)",
                Some("p"),
                Selector::Shortest(1),
                Mode::Walk,
            ),
            ("ANY (a)-[e]->*(b)", None, Selector::Shortest(1), Mode::Walk),
            (
                "ANY 2 TRAIL (a)-[e]->*(b)",
                None,
                Selector::Shortest(2),
                Mode::Trail,
            ),
            (
                "q = SHORTEST 3 PATHS (a)-[e]->*(b)",
                Some("q"),
                Selector::Shortest(3),
                Mode::Walk,
            ),
            (
                "ALL SHORTEST ACYCLIC PATH (a)-[e]->*(b)",
                None,
                Selector::AllShortest,
                Mode::Acyclic,
            ),
            (
                "ALL SIMPLE (a)-[e]->{1,2}(b)",
                None,
                Selector::All,
                Mode::Simple,
            ),
            (
                "p = WALK (a)-[e]->{1,2}(b)",
                Some("p"),
                Selector::All,
                Mode::Walk,
            ),
        ] {
            let p = parse(&format!("g MATCH {path} COLUMNS (a.id)")).unwrap();
            let path0 = &p.paths[0];
            assert_eq!(
                (path0.var.as_deref(), path0.selector, path0.mode),
                (var, selector, mode),
                "{path}"
            );
        }
    }

    #[test]
    fn refuses_what_is_not_supported_yet_and_reports_errors() {
        for (body, expected) in [
            (
                "g MATCH ANY SHORTEST 5 (a)-[e]->*(b) COLUMNS (a.id)",
                "for several, write SHORTEST k",
            ),
            (
                "g MATCH SHORTEST (a)-[e]->*(b) COLUMNS (a.id)",
                "SHORTEST takes a number",
            ),
            ("g MATCH SHORTEST 0 (a)-[e]->*(b) COLUMNS (a.id)", "above 0"),
            (
                "g MATCH (a)-[e]->{3,1}(b) COLUMNS (a.id)",
                "lower bound 3 is above its upper bound 1",
            ),
            (
                "g MATCH (a)-[e]->{x}(b) COLUMNS (a.id)",
                "expected a number or ','",
            ),
            (
                "g MATCH ((a)-[e]->(b))* COLUMNS (a.id)",
                "parenthesized path patterns",
            ),
            ("g MATCH (a:x|y) COLUMNS (a.id)", "label expressions"),
            (
                "g MATCH ()-[]->()",
                "a GRAPH_TABLE without COLUMNS returns the properties of the pattern's named nodes and edges",
            ),
            (
                "g MATCH ()-[]->() COLUMNS (*)",
                "COLUMNS (*) returns the properties of the pattern's named nodes and edges, and it names none",
            ),
            (
                "g (a) COLUMNS (a.id)",
                "expected MATCH after the graph's name, found '('",
            ),
            (
                "g MATCH (a)-[e]=>(b) COLUMNS (a.id)",
                "expected '->' or '-' after ']'",
            ),
            (
                "g MATCH (a b c) COLUMNS (a.id)",
                "expected a variable, ':label', WHERE or ')'",
            ),
            ("g MATCH (a) COLUMNS (a.id) extra", "unexpected 'extra'"),
            ("g MATCH (a) COLUMNS ()", "COLUMNS (…) lists nothing"),
        ] {
            let message = parse(body).unwrap_err().to_string();
            assert!(message.contains(expected), "{body}\n{message}");
        }
    }

    #[test]
    fn columns_left_out_or_star_are_every_named_element() {
        for body in [
            "g MATCH (a:x)-[k:y]->(b), (b)-[]->(\"C\")",
            "g MATCH (a:x)-[k:y]->(b), (b)-[]->(\"C\") COLUMNS (*)",
            "g MATCH (a:x)-[k:y]->(b), (b)-[]->(\"C\") COLUMNS ( * )",
        ] {
            assert_eq!(
                parse(body).unwrap().columns,
                r#""a".*, "k".*, "b".*, "C".*"#,
                "{body}"
            );
        }
        // An edge that stands for several edges has no properties to return.
        assert_eq!(
            parse("g MATCH (a)-[k]->{1,2}(b), p = (b)-[f]->(c)")
                .unwrap()
                .columns,
            r#""a".*, "b".*, "c".*"#
        );
        // Other uses of * are left to the planner.
        assert_eq!(
            parse("g MATCH (a) COLUMNS (*, a.id)").unwrap().columns,
            "*, a.id"
        );
    }

    #[test]
    fn either_direction_edges_and_tilde_edges() {
        for path in ["(a)<-[e]->(b)", "(a)<-[e]- >(b)", "(a)<->(b)", "(a)<- >(b)"] {
            let p = parse(&format!("g MATCH {path} COLUMNS (a.id)")).unwrap();
            assert_eq!(edges(&p.paths[0]), vec![(Direction::Any, None)], "{path}");
        }
        let p = parse("g MATCH (a)<-[e]->{1,3}(b) COLUMNS (a.id)").unwrap();
        assert_eq!(
            edges(&p.paths[0]),
            vec![(
                Direction::Any,
                Some(Quantifier {
                    min: 1,
                    max: Some(3)
                })
            )]
        );
        for path in [
            "(a)~[e]~(b)",
            "(a)<~[e]~(b)",
            "(a)~[e]~>(b)",
            "(a)~(b)",
            "(a)<~(b)",
            "(a)-[e]~(b)",
            "(a)<-[e]~(b)",
        ] {
            let message = parse(&format!("g MATCH {path} COLUMNS (a.id)"))
                .unwrap_err()
                .to_string();
            assert!(
                message.contains("Ursa's edges are directed; use -[e]- to follow them either way"),
                "{path}\n{message}"
            );
        }
    }

    #[test]
    fn keep_and_one_row_per_match() {
        let path = |body: &str| {
            let p = parse(body).unwrap();
            (p.paths[0].selector, p.paths[0].mode, p.condition)
        };
        assert_eq!(
            path("g MATCH (a)-[e]->*(b) KEEP ANY SHORTEST COLUMNS (a.id)"),
            (Selector::Shortest(1), Mode::Walk, None)
        );
        assert_eq!(
            path("g MATCH (a)-[e]->*(b) KEEP SHORTEST 2 TRAIL PATHS WHERE a.id = 1 COLUMNS (a.id)"),
            (Selector::Shortest(2), Mode::Trail, Some("a.id = 1".to_string()))
        );
        // After WHERE too, as #409 found it written.
        assert_eq!(
            path("g MATCH (a)-[e]->*(b) WHERE a.id = 1 KEEP ACYCLIC COLUMNS (a.id)"),
            (Selector::All, Mode::Acyclic, Some("a.id = 1".to_string()))
        );
        // A mode before the path and a selector in KEEP.
        assert_eq!(
            path("g MATCH TRAIL (a)-[e]->*(b) KEEP ALL SHORTEST COLUMNS (a.id)"),
            (Selector::AllShortest, Mode::Trail, None)
        );
        // ONE ROW PER MATCH is the default; a property named keep is not KEEP.
        assert_eq!(
            path("g MATCH (a)-[e]->(b) WHERE a.keep = 1 ONE ROW PER MATCH COLUMNS (a.id)"),
            (Selector::All, Mode::Walk, Some("a.keep = 1".to_string()))
        );
        assert_eq!(
            path("g MATCH (a)-[e]->(b) ONE ROW PER MATCH"),
            (Selector::All, Mode::Walk, None)
        );
        for (body, expected) in [
            (
                "g MATCH (a)-[e]->*(b), (b)-[f]->*(c) KEEP ANY SHORTEST COLUMNS (a.id)",
                "KEEP over several path patterns is not supported yet",
            ),
            (
                "g MATCH ANY SHORTEST (a)-[e]->*(b) KEEP ANY SHORTEST COLUMNS (a.id)",
                "both before it and in KEEP",
            ),
            (
                "g MATCH (a)-[e]->*(b) KEEP COLUMNS (a.id)",
                "KEEP takes a path selector or path mode",
            ),
            (
                "g MATCH (a)-[e]->*(b) ONE ROW PER STEP (v1, e, v2) COLUMNS (a.id)",
                "ONE ROW PER VERTEX and ONE ROW PER STEP are not supported yet; return vertices(p) or edges(p)",
            ),
            (
                "g MATCH (a)-[e]->*(b) ONE ROW PER VERTEX (v) COLUMNS (a.id)",
                "ONE ROW PER VERTEX and ONE ROW PER STEP are not supported yet",
            ),
            (
                "g MATCH (a WHERE a.id = 1 KEEP TRAIL)-[e]->*(b) COLUMNS (a.id)",
                "KEEP goes after the whole pattern",
            ),
        ] {
            let message = parse(body).unwrap_err().to_string();
            assert!(message.contains(expected), "{body}\n{message}");
        }
    }

    #[test]
    fn graph_predicates_are_rewritten_to_calls() {
        let p = parse(
            "g MATCH (a)-[k]->(b) WHERE k IS LABELED \"studyAt\" AND a IS NOT LABELED Person \
             AND a IS SOURCE OF k AND NOT b IS NOT DESTINATION OF k AND a.name IS NOT NULL \
             COLUMNS (a IS LABELED x AS l)",
        )
        .unwrap();
        assert_eq!(
            p.condition.as_deref(),
            Some(
                "__ursa_is_labeled(k, 'studyAt') AND (NOT __ursa_is_labeled(a, 'person')) \
                 AND __ursa_is_source(a, k) AND NOT (NOT __ursa_is_destination(b, k)) \
                 AND a.name IS NOT NULL"
            )
        );
        assert_eq!(p.columns, "__ursa_is_labeled(a, 'x') AS l");
        let message = parse("g MATCH (a) WHERE a IS LABELED x|y COLUMNS (a.id)")
            .unwrap_err()
            .to_string();
        assert!(message.contains("(#412)"), "{message}");
    }

    #[test]
    fn cypher_habits_are_refused_with_the_sql_pgq_form() {
        for (body, expected) in [
            (
                "g MATCH (a:Person {name: 'Daniel'}) COLUMNS (a.id)",
                "property maps ({…}) are Cypher; write the condition with WHERE, as (a:Person WHERE a.name = 'Daniel')",
            ),
            (
                "g MATCH (:Person {name: 'Daniel', age: f(1, 2)}) COLUMNS (a.id)",
                "as (a:Person WHERE a.name = 'Daniel' AND a.age = f(1, 2))",
            ),
            (
                "g MATCH (a)-[k:knows {since: 2020}]->(b) COLUMNS (a.id)",
                "as [k:knows WHERE k.since = 2020]",
            ),
            (
                "g MATCH (a)-[k:knows*1..3]->(b) COLUMNS (a.id)",
                "*1..3 inside an edge's brackets is Cypher's range; in SQL/PGQ the quantifier follows the edge: write -[k:knows]->{1,3}",
            ),
            ("g MATCH (a)-[k*]->(b) COLUMNS (a.id)", "write -[k]->+"),
            ("g MATCH (a)-[:knows*2]-(b) COLUMNS (a.id)", "write -[:knows]-{2}"),
            ("g MATCH (a)<-[k*..4]-(b) COLUMNS (a.id)", "write <-[k]-{1,4}"),
            ("g MATCH (a)-[k * 2..]->(b) COLUMNS (a.id)", "write -[k]->{2,}"),
        ] {
            let message = parse(body).unwrap_err().to_string();
            assert!(message.contains(expected), "{body}\n{message}");
        }
    }
}
