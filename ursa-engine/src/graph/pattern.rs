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
//! Supported (fixed-length patterns):
//!
//! ```text
//! GRAPH_TABLE ( graph
//!   MATCH path [, path …]
//!   [WHERE condition]
//!   COLUMNS ( expression [AS name], … ) )
//!
//! path    := node (edge node)*
//! node    := ( [var] [:label | IS label] [WHERE condition] )
//! edge    := -[ spec ]->  |  <-[ spec ]-  |  -[ spec ]-  |  ->  |  <-  |  -
//! spec    := [var] [:label | IS label] [WHERE condition]
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
//! Variable-length paths, path variables, `ANY SHORTEST` and label
//! expressions (`|`, `&`, `!`) are refused with a message (#297).

use std::collections::BTreeMap;

use datafusion::arrow::datatypes::SchemaRef;
use datafusion::common::TableReference;
use datafusion::error::{DataFusionError, Result};
use datafusion::sql::sqlparser::dialect::GenericDialect;
use datafusion::sql::sqlparser::tokenizer::{Location, Token, TokenWithSpan, Tokenizer};

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
    pub paths: Vec<Vec<Element>>,
    /// The pattern's `WHERE` condition, as written.
    pub condition: Option<String>,
    /// The `COLUMNS` list, as written (without its parentheses).
    pub columns: String,
}

/// One element of a path: nodes and edges alternate, starting with a node.
#[derive(Debug, Clone, PartialEq)]
pub enum Element {
    Node(Spec),
    Edge(Spec, Direction),
}

/// A node or edge: `[var] [:label] [WHERE condition]`.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Spec {
    pub var: Option<String>,
    pub label: Option<String>,
    pub condition: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
    let condition = if t.is_word(0, "WHERE") {
        t.next();
        let condition = t.text_until(|t, n| t.is_word(n, "COLUMNS"));
        if condition.is_none() {
            return pattern_err("WHERE needs a condition");
        }
        condition
    } else {
        None
    };
    if !t.is_word(0, "COLUMNS") {
        if t.peek(0).is_none() {
            return pattern_err(
                "a GRAPH_TABLE needs COLUMNS (…), listing what to return, such as COLUMNS (a.id AS a, b.id AS b)",
            );
        }
        return pattern_err(format!(
            "expected ',', WHERE or COLUMNS after the pattern, found {}",
            t.found()
        ));
    }
    t.next();
    t.expect(&Token::LParen, "'(' after COLUMNS")?;
    let Some(columns) = t.text_until(|_, _| false) else {
        return pattern_err("COLUMNS (…) lists nothing");
    };
    t.expect(&Token::RParen, "')' to close COLUMNS")?;
    if t.peek(0).is_some() {
        return pattern_err(format!("unexpected {} after COLUMNS (…)", t.found()));
    }
    Ok(Pattern {
        graph,
        paths,
        condition,
        columns,
    })
}

/// A path: a node, then edges and nodes.
fn parse_path(t: &mut Tokens) -> Result<Vec<Element>> {
    if matches!(t.peek(0), Some(Token::Word(_))) && t.peek(1) == Some(&Token::Eq) {
        return pattern_err(
            "path variables (p = …) are not supported yet; they come with shortest paths (#297)",
        );
    }
    if t.is_word(0, "ANY") || t.is_word(0, "ALL") || t.is_word(0, "SHORTEST") {
        return pattern_err("ANY/ALL SHORTEST paths are not supported yet (#297)");
    }
    let mut path = vec![Element::Node(parse_node(t)?)];
    while let Some(direction_and_spec) = parse_edge(t)? {
        let (spec, direction) = direction_and_spec;
        path.push(Element::Edge(spec, direction));
        path.push(Element::Node(parse_node(t)?));
    }
    Ok(path)
}

fn parse_node(t: &mut Tokens) -> Result<Spec> {
    t.expect(&Token::LParen, "'(' to start a node")?;
    let spec = parse_spec(t, &Token::RParen)?;
    t.expect(&Token::RParen, "')' to close the node")?;
    Ok(spec)
}

/// `[var] [:label | IS label] [WHERE condition]`, up to `close`.
fn parse_spec(t: &mut Tokens, close: &Token) -> Result<Spec> {
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
            return pattern_err(
                "label expressions (|, &, !, %) are not supported yet; use one label (#297)",
            );
        }
    }
    if t.is_word(0, "WHERE") {
        t.next();
        let close = close.clone();
        spec.condition = t.text_until(move |t, n| t.peek(n) == Some(&close));
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

/// An edge, if one is next: `-[…]->`, `<-[…]-`, `-[…]-`, `->`, `<-` or `-`.
fn parse_edge(t: &mut Tokens) -> Result<Option<(Spec, Direction)>> {
    let quantified = |t: &Tokens| {
        matches!(
            t.peek(0),
            Some(Token::Mul | Token::Plus | Token::Placeholder(_) | Token::LBrace)
        )
    };
    let refuse_quantifier = |t: &Tokens| -> Result<()> {
        if quantified(t) {
            return pattern_err(
                "quantified edges (*, +, ?, {m,n}) are not supported yet; variable-length paths come with #297",
            );
        }
        Ok(())
    };
    let edge = match (t.peek(0), t.peek(1)) {
        // Abbreviated edges.
        (Some(Token::Arrow), _) => {
            t.next();
            refuse_quantifier(t)?;
            (Spec::default(), Direction::Right)
        }
        (Some(Token::Lt), Some(Token::Minus)) if t.peek(2) != Some(&Token::LBracket) => {
            t.next();
            t.next();
            refuse_quantifier(t)?;
            (Spec::default(), Direction::Left)
        }
        (Some(Token::Minus), Some(Token::LBracket)) => {
            t.next();
            t.next();
            let spec = parse_spec(t, &Token::RBracket)?;
            t.expect(&Token::RBracket, "']' to close the edge")?;
            let direction = match t.next() {
                Some(Token::Arrow) => Direction::Right,
                Some(Token::Minus) => Direction::Any,
                Some(Token::LongArrow) => {
                    return pattern_err("expected '->' or '-' after ']', found '->>'")
                }
                _ => {
                    t.at -= 1;
                    return pattern_err(format!(
                        "expected '->' or '-' after ']', found {}",
                        t.found()
                    ));
                }
            };
            refuse_quantifier(t)?;
            (spec, direction)
        }
        (Some(Token::Lt), Some(Token::Minus)) => {
            t.next();
            t.next();
            t.expect(&Token::LBracket, "'[' after '<-'")?;
            let spec = parse_spec(t, &Token::RBracket)?;
            t.expect(&Token::RBracket, "']' to close the edge")?;
            t.expect(&Token::Minus, "'-' after ']'")?;
            refuse_quantifier(t)?;
            (spec, Direction::Left)
        }
        (Some(Token::Minus), _) => {
            t.next();
            refuse_quantifier(t)?;
            (Spec::default(), Direction::Any)
        }
        _ => return Ok(None),
    };
    Ok(Some(edge))
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
}

/// A condition the lowering adds.
#[derive(Debug, Clone, PartialEq)]
pub enum Join {
    Equal(VarColumn, VarColumn),
    /// Not equal: keeps a self-loop out of an undirected edge's reverse branch.
    NotEqual(VarColumn, VarColumn),
}

/// A pattern resolved against a graph: what to scan and join.
#[derive(Debug, Clone, PartialEq)]
pub struct Lowered {
    /// One per variable, in order of appearance.
    pub sources: Vec<Source>,
    /// One per orientation of the undirected edges: the joins of each branch
    /// of the `UNION ALL` (one branch when every edge has a direction).
    pub branches: Vec<Vec<Join>>,
    /// The pattern's conditions as written: in elements, then after the paths.
    pub conditions: Vec<String>,
    /// The `COLUMNS` list as written.
    pub columns: String,
}

/// Resolve `pattern` against `graph`. `schema_of` gives a table's columns
/// (for properties).
pub fn lower(
    pattern: &Pattern,
    graph: &PropertyGraph,
    schema_of: &dyn Fn(&TableReference) -> Result<SchemaRef>,
) -> Result<Lowered> {
    // Collect the variables, naming anonymous ones.
    let mut bindings: BTreeMap<String, Binding> = BTreeMap::new();
    let mut order: Vec<String> = Vec::new();
    let mut steps = Vec::new();
    let mut anonymous = 0;
    for path in &pattern.paths {
        let mut previous: Option<(String, Direction, String)> = None; // (left node, direction, edge)
        for element in path {
            let (spec, is_edge) = match element {
                Element::Node(spec) => (spec, false),
                Element::Edge(spec, _) => (spec, true),
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
                    if let Some((left, direction, edge)) = previous.take() {
                        steps.push(Step {
                            left,
                            edge,
                            right: var.clone(),
                            direction,
                        });
                    }
                    previous = Some((var, Direction::Right, String::new()));
                }
                Element::Edge(_, direction) => {
                    let (left, _, _) = previous.take().expect("a path starts with a node");
                    previous = Some((left, *direction, var));
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

    let undirected: Vec<usize> = steps
        .iter()
        .enumerate()
        .filter(|(_, s)| s.direction == Direction::Any)
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
        let (table, columns) = if b.is_edge {
            let e = b.edge.expect("resolved");
            let needed = [e.source.as_str(), e.destination.as_str()];
            (
                e.table.clone(),
                properties(&e.table, &e.labels(), &needed, schema_of)?,
            )
        } else {
            match b.vertex.as_ref().expect("resolved") {
                VertexSource::Table(v) => (
                    v.table.clone(),
                    properties(&v.table, &v.labels(), &[v.key.as_str()], schema_of)?,
                ),
                VertexSource::Nodes => (
                    TableReference::partial(graph.name.as_str(), super::schema::NODES),
                    None,
                ),
            }
        };
        sources.push(Source {
            var: var.clone(),
            table,
            columns,
        });
    }
    let key = |var: &str| -> VarColumn {
        match bindings[var].vertex.as_ref().expect("resolved") {
            VertexSource::Table(v) => (var.to_string(), v.key.clone()),
            VertexSource::Nodes => (var.to_string(), super::schema::ID.to_string()),
        }
    };

    let mut branches = Vec::new();
    for mask in 0..(1usize << undirected.len()) {
        let mut joins = Vec::new();
        for (i, step) in steps.iter().enumerate() {
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
            joins.push(Join::Equal(src, key(from)));
            joins.push(Join::Equal(dst, key(to)));
        }
        branches.push(joins);
    }
    let mut conditions: Vec<String> = order
        .iter()
        .flat_map(|var| bindings[var].conditions.iter().cloned())
        .collect();
    conditions.extend(pattern.condition.clone());
    Ok(Lowered {
        sources,
        branches,
        conditions,
        columns: pattern.columns.clone(),
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
            p.paths[0],
            vec![
                Element::Node(spec(Some("a"), Some("person"), Some("a.age > (1 + 2)"))),
                Element::Edge(
                    spec(Some("k"), Some("knows"), Some("k.since > 2000")),
                    Direction::Right
                ),
                Element::Node(spec(Some("b"), Some("person"), None)),
            ]
        );
        assert_eq!(
            p.paths[1],
            vec![
                Element::Node(spec(Some("b"), None, None)),
                Element::Edge(spec(Some("w"), None, None), Direction::Left),
                Element::Node(spec(Some("c"), None, None)),
            ]
        );
        let directions: Vec<Direction> = p.paths[2]
            .iter()
            .filter_map(|e| match e {
                Element::Edge(_, d) => Some(*d),
                _ => None,
            })
            .collect();
        assert_eq!(
            directions,
            vec![
                Direction::Any,
                Direction::Any,
                Direction::Right,
                Direction::Left
            ]
        );
    }

    #[test]
    fn refuses_what_is_not_supported_yet_and_reports_errors() {
        for (body, expected) in [
            (
                "g MATCH p = (a)-[e]->(b) COLUMNS (a.id)",
                "path variables (p = …) are not supported yet",
            ),
            (
                "g MATCH ANY SHORTEST (a)-[e]->(b) COLUMNS (a.id)",
                "ANY/ALL SHORTEST paths are not supported yet",
            ),
            ("g MATCH (a)-[e]->*(b) COLUMNS (a.id)", "quantified edges"),
            (
                "g MATCH (a)-[e]->{1,3}(b) COLUMNS (a.id)",
                "quantified edges",
            ),
            ("g MATCH (a:x|y) COLUMNS (a.id)", "label expressions"),
            ("g MATCH (a)-[e]->(b)", "a GRAPH_TABLE needs COLUMNS (…)"),
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
}
