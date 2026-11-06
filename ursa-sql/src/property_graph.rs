//! The table clauses of `CREATE PROPERTY GRAPH`: SQL/PGQ's element tables
//! (#295).
//!
//! ```text
//! CREATE [OR REPLACE] PROPERTY GRAPH [IF NOT EXISTS] name
//!   [VERTEX TABLES ( vertex_table [, …] )]
//!   [EDGE TABLES ( edge_table [, …] )]
//!
//! vertex_table := table [AS name] [KEY (column)] [labels]
//! edge_table   := table [AS name] [KEY (column)]
//!                 SOURCE KEY (column) [REFERENCES vertex (column)]
//!                 DESTINATION KEY (column) [REFERENCES vertex (column)]
//!                 [labels]
//! labels       := properties                       -- the default label
//!               | DEFAULT LABEL [properties]
//!               | LABEL name [properties] [LABEL name [properties] …]
//!               | properties LABEL name [LABEL name …]   (DuckPGQ's order)
//! properties   := PROPERTIES (column [AS name], …)
//!               | PROPERTIES [ARE] ALL COLUMNS [EXCEPT (column, …)]
//!               | NO PROPERTIES
//! ```
//!
//! At least one of the two clauses is needed. Ursa's relaxation stays: a graph
//! may have no vertex tables, and then its nodes are its edges' endpoints. A
//! vertex `KEY` may be left out: the engine takes the table's primary key, or
//! the column the edges' `REFERENCES` name, as DuckPGQ does. `REFERENCES`
//! names a vertex table by its element name (its `AS` name, or else the table's
//! name). Still refused, with a clear error: multi-column keys, `SOURCE vertex`
//! without `KEY`, and property expressions other than a column. `NODE TABLES`
//! and `RELATIONSHIP TABLES`, SQL/PGQ's synonyms, are accepted.

use std::fmt;

use datafusion::error::{DataFusionError, Result};
use datafusion::sql::sqlparser::ast::{Ident, ObjectName};
use datafusion::sql::sqlparser::parser::Parser;
use datafusion::sql::sqlparser::tokenizer::Token;

/// One entry of `VERTEX TABLES (…)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VertexTableDef {
    pub table: ObjectName,
    /// `AS name`.
    pub alias: Option<Ident>,
    /// `KEY (column)`; `None` when left out.
    pub key: Option<Ident>,
    pub labels: Vec<LabelDef>,
}

/// One entry of `EDGE TABLES (…)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EdgeTableDef {
    pub table: ObjectName,
    pub alias: Option<Ident>,
    /// The edge table's own `KEY (column)`, if given.
    pub key: Option<Ident>,
    pub source: EndpointDef,
    pub destination: EndpointDef,
    pub labels: Vec<LabelDef>,
}

/// `KEY (key) [REFERENCES table (column)]`, after `SOURCE` or `DESTINATION`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EndpointDef {
    pub key: Ident,
    pub references: Option<References>,
}

/// `REFERENCES vertex (column)`: a vertex table's element name or table name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct References {
    pub table: ObjectName,
    pub column: Ident,
}

/// One label of an element table and its properties. `name: None` is the
/// default label (written `DEFAULT LABEL`, or implied by a bare `PROPERTIES`
/// clause), which is the element table's name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LabelDef {
    pub name: Option<Ident>,
    pub properties: PropertiesDef,
}

/// A label's properties.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PropertiesDef {
    /// No properties clause: every column, as `ALL COLUMNS`.
    Default,
    /// `PROPERTIES [ARE] ALL COLUMNS [EXCEPT (…)]`.
    AllColumns { except: Vec<Ident> },
    /// `PROPERTIES (column [AS name], …)`.
    List(Vec<(Ident, Option<Ident>)>),
    /// `NO PROPERTIES`.
    None,
}

fn comma_list<T: fmt::Display>(items: &[T]) -> String {
    items
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

fn write_labels(f: &mut fmt::Formatter<'_>, labels: &[LabelDef]) -> fmt::Result {
    for label in labels {
        match &label.name {
            Some(name) => write!(f, " LABEL {name}")?,
            None if label.properties == PropertiesDef::Default => write!(f, " DEFAULT LABEL")?,
            None => {}
        }
        write!(f, "{}", label.properties)?;
    }
    Ok(())
}

impl fmt::Display for PropertiesDef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PropertiesDef::Default => Ok(()),
            PropertiesDef::AllColumns { except } if except.is_empty() => {
                write!(f, " PROPERTIES ARE ALL COLUMNS")
            }
            PropertiesDef::AllColumns { except } => {
                write!(
                    f,
                    " PROPERTIES ARE ALL COLUMNS EXCEPT ({})",
                    comma_list(except)
                )
            }
            PropertiesDef::List(columns) => {
                let items: Vec<String> = columns
                    .iter()
                    .map(|(column, alias)| match alias {
                        Some(alias) => format!("{column} AS {alias}"),
                        None => column.to_string(),
                    })
                    .collect();
                write!(f, " PROPERTIES ({})", items.join(", "))
            }
            PropertiesDef::None => write!(f, " NO PROPERTIES"),
        }
    }
}

impl fmt::Display for VertexTableDef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.table)?;
        if let Some(alias) = &self.alias {
            write!(f, " AS {alias}")?;
        }
        if let Some(key) = &self.key {
            write!(f, " KEY ({key})")?;
        }
        write_labels(f, &self.labels)
    }
}

impl fmt::Display for EdgeTableDef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.table)?;
        if let Some(alias) = &self.alias {
            write!(f, " AS {alias}")?;
        }
        if let Some(key) = &self.key {
            write!(f, " KEY ({key})")?;
        }
        write!(
            f,
            " SOURCE {} DESTINATION {}",
            self.source, self.destination
        )?;
        write_labels(f, &self.labels)
    }
}

impl fmt::Display for EndpointDef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "KEY ({})", self.key)?;
        if let Some(r) = &self.references {
            write!(f, " REFERENCES {} ({})", r.table, r.column)?;
        }
        Ok(())
    }
}

/// `VERTEX TABLES (…)` and `EDGE TABLES (…)`, as written after the graph's name.
pub(crate) fn write_tables(
    f: &mut fmt::Formatter<'_>,
    vertices: &[VertexTableDef],
    edges: &[EdgeTableDef],
) -> fmt::Result {
    if !vertices.is_empty() {
        write!(f, " VERTEX TABLES ({})", comma_list(vertices))?;
    }
    if !edges.is_empty() {
        write!(f, " EDGE TABLES ({})", comma_list(edges))?;
    }
    Ok(())
}

/// What a feature beyond the supported grammar is refused with.
const NOT_SUPPORTED: &str = "not supported yet";

/// Parse the table clauses, after the graph's name.
pub(crate) fn parse_tables(p: &mut Parser) -> Result<(Vec<VertexTableDef>, Vec<EdgeTableDef>)> {
    let vertices =
        if consume_words(p, &["VERTEX", "TABLES"]) || consume_words(p, &["NODE", "TABLES"]) {
            parse_list(p, parse_vertex_table)?
        } else {
            Vec::new()
        };
    let edges =
        if consume_words(p, &["EDGE", "TABLES"]) || consume_words(p, &["RELATIONSHIP", "TABLES"]) {
            parse_list(p, parse_edge_table)?
        } else if vertices.is_empty() {
            return Ok(p.expected_ref("VERTEX TABLES or EDGE TABLES", p.peek_token_ref())?);
        } else {
            Vec::new()
        };
    if word(p, 0, "VERTEX") || word(p, 0, "NODE") {
        return unsupported(p, "VERTEX TABLES after EDGE TABLES is");
    }
    if word(p, 0, "EDGE") || word(p, 0, "RELATIONSHIP") {
        return unsupported(p, "a second EDGE TABLES clause is");
    }
    Ok((vertices, edges))
}

/// `( item [, item …] )`.
fn parse_list<T>(p: &mut Parser, item: fn(&mut Parser) -> Result<T>) -> Result<Vec<T>> {
    p.expect_token(&Token::LParen)?;
    let mut items = vec![item(p)?];
    while p.consume_token(&Token::Comma) {
        items.push(item(p)?);
    }
    p.expect_token(&Token::RParen)?;
    Ok(items)
}

/// `table [AS name]`.
fn parse_element_name(p: &mut Parser) -> Result<(ObjectName, Option<Ident>)> {
    let table = p.parse_object_name(false)?;
    let alias = if consume_words(p, &["AS"]) {
        Some(p.parse_identifier()?)
    } else {
        None
    };
    Ok((table, alias))
}

fn parse_vertex_table(p: &mut Parser) -> Result<VertexTableDef> {
    let (table, alias) = parse_element_name(p)?;
    let key = if consume_words(p, &["KEY"]) {
        Some(parse_key(p)?)
    } else {
        None
    };
    let labels = parse_labels(p)?;
    if !matches!(p.peek_token_ref().token, Token::RParen | Token::Comma) {
        return Ok(p.expected_ref("KEY, LABEL, PROPERTIES, ',' or ')'", p.peek_token_ref())?);
    }
    Ok(VertexTableDef {
        table,
        alias,
        key,
        labels,
    })
}

fn parse_edge_table(p: &mut Parser) -> Result<EdgeTableDef> {
    let (table, alias) = parse_element_name(p)?;
    let key = if consume_words(p, &["KEY"]) {
        Some(parse_key(p)?)
    } else {
        None
    };
    expect_word(p, "SOURCE")?;
    let source = parse_endpoint(p, "SOURCE")?;
    expect_word(p, "DESTINATION")?;
    let destination = parse_endpoint(p, "DESTINATION")?;
    let labels = parse_labels(p)?;
    Ok(EdgeTableDef {
        table,
        alias,
        key,
        source,
        destination,
        labels,
    })
}

fn parse_endpoint(p: &mut Parser, side: &str) -> Result<EndpointDef> {
    if !consume_words(p, &["KEY"]) {
        if matches!(p.peek_token_ref().token, Token::Word(_)) {
            return unsupported(
                p,
                &format!(
                    "{side} without KEY is (name the column with {side} KEY (column) \
                     REFERENCES vertex_table (key))"
                ),
            );
        }
        return Ok(p.expected_ref("KEY", p.peek_token_ref())?);
    }
    let key = parse_key(p)?;
    let references = if consume_words(p, &["REFERENCES"]) {
        let table = p.parse_object_name(false)?;
        let column = parse_key(p)?;
        Some(References { table, column })
    } else {
        None
    };
    Ok(EndpointDef { key, references })
}

/// The label and property clauses of one element table.
fn parse_labels(p: &mut Parser) -> Result<Vec<LabelDef>> {
    if consume_words(p, &["DEFAULT", "LABEL"]) {
        let properties = parse_properties(p)?;
        return Ok(vec![LabelDef {
            name: None,
            properties,
        }]);
    }
    // DuckPGQ writes the properties first: `PROPERTIES (id, name) LABEL
    // person` gives `person` those properties (#307). A label with its own
    // properties clause keeps it.
    let leading = parse_properties(p)?;
    let mut labels = Vec::new();
    while consume_words(p, &["LABEL"]) {
        let name = p.parse_identifier()?;
        let properties = match parse_properties(p)? {
            PropertiesDef::Default => leading.clone(),
            own => own,
        };
        labels.push(LabelDef {
            name: Some(name),
            properties,
        });
    }
    if labels.is_empty() && leading != PropertiesDef::Default {
        labels.push(LabelDef {
            name: None,
            properties: leading,
        });
    }
    if word(p, 0, "LABELS") || word(p, 0, "DEFAULT") {
        return unsupported(p, "this label clause is (write LABEL name, once per label)");
    }
    Ok(labels)
}

/// A properties clause, or `Default` when there is none.
fn parse_properties(p: &mut Parser) -> Result<PropertiesDef> {
    if consume_words(p, &["NO", "PROPERTIES"]) {
        return Ok(PropertiesDef::None);
    }
    if !consume_words(p, &["PROPERTIES"]) {
        return Ok(PropertiesDef::Default);
    }
    if consume_words(p, &["ARE", "ALL", "COLUMNS"]) || consume_words(p, &["ALL", "COLUMNS"]) {
        let except = if consume_words(p, &["EXCEPT"]) {
            parse_list(p, |p| Ok(p.parse_identifier()?))?
        } else {
            Vec::new()
        };
        return Ok(PropertiesDef::AllColumns { except });
    }
    let columns = parse_list(p, |p| {
        let column = match &p.peek_token_ref().token {
            Token::Word(_) => p.parse_identifier()?,
            _ => {
                return unsupported(p, "a property that is not a column is");
            }
        };
        if !matches!(
            p.peek_token_ref().token,
            Token::Comma | Token::RParen | Token::Word(_)
        ) {
            return unsupported(p, "a property that is not a column is");
        }
        let alias = if consume_words(p, &["AS"]) {
            Some(p.parse_identifier()?)
        } else {
            None
        };
        Ok((column, alias))
    })?;
    Ok(PropertiesDef::List(columns))
}

/// `(column)`: a key of exactly one column.
fn parse_key(p: &mut Parser) -> Result<Ident> {
    p.expect_token(&Token::LParen)?;
    let column = p.parse_identifier()?;
    if p.peek_token_ref().token == Token::Comma {
        return unsupported(
            p,
            "multi-column keys are (KEY, SOURCE KEY, DESTINATION KEY and REFERENCES take one column)",
        );
    }
    p.expect_token(&Token::RParen)?;
    Ok(column)
}

/// The error for a SQL/PGQ feature beyond what Ursa supports, at the next token.
fn unsupported<T>(p: &Parser, what: &str) -> Result<T> {
    Err(DataFusionError::NotImplemented(format!(
        "{what} {NOT_SUPPORTED}{}",
        p.peek_token_ref().span.start
    )))
}

/// Whether the `n`th token ahead is the unquoted word `word`, in any case.
pub(crate) fn word(p: &Parser, n: usize, word: &str) -> bool {
    match &p.peek_nth_token_ref(n).token {
        Token::Word(w) => w.quote_style.is_none() && w.value.eq_ignore_ascii_case(word),
        _ => false,
    }
}

/// Consume `words` if they are next.
pub(crate) fn consume_words(p: &mut Parser, words: &[&str]) -> bool {
    let found = words.iter().enumerate().all(|(i, w)| word(p, i, w));
    if found {
        for _ in words {
            p.next_token();
        }
    }
    found
}

fn expect_word(p: &mut Parser, expected: &str) -> Result<()> {
    if consume_words(p, &[expected]) {
        Ok(())
    } else {
        Ok(p.expected_ref(expected, p.peek_token_ref())?)
    }
}

#[cfg(test)]
mod tests {
    use datafusion::sql::sqlparser::ast::ObjectNamePart;

    use super::*;
    use crate::{parse_sql, CreatePropertyGraph, UrsaStatement};

    fn create(sql: &str) -> CreatePropertyGraph {
        let mut statements = parse_sql(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
        assert_eq!(statements.len(), 1, "{sql}");
        match statements.pop().unwrap() {
            UrsaStatement::CreatePropertyGraph(g) => *g,
            other => panic!("{sql} parsed as {other:?}"),
        }
    }

    fn error(sql: &str) -> String {
        match parse_sql(sql) {
            Ok(statements) => panic!("{sql} should not parse; got {statements:?}"),
            Err(e) => ursa_engine::user_message(&e),
        }
    }

    fn name(parts: &[&str]) -> ObjectName {
        ObjectName(
            parts
                .iter()
                .map(|p| ObjectNamePart::Identifier(Ident::new(*p)))
                .collect(),
        )
    }

    fn endpoint(key: &str, references: Option<(&str, &str)>) -> EndpointDef {
        EndpointDef {
            key: Ident::new(key),
            references: references.map(|(table, column)| References {
                table: name(&[table]),
                column: Ident::new(column),
            }),
        }
    }

    const GETTING_STARTED: &str = "CREATE PROPERTY GRAPH karate
  VERTEX TABLES (members KEY (id))
  EDGE TABLES (
    ties SOURCE KEY (src) REFERENCES members (id)
         DESTINATION KEY (dst) REFERENCES members (id)
  )";

    #[test]
    fn the_getting_started_graph() {
        assert_eq!(
            create(GETTING_STARTED),
            CreatePropertyGraph {
                name: name(&["karate"]),
                or_replace: false,
                if_not_exists: false,
                vertices: vec![VertexTableDef {
                    table: name(&["members"]),
                    alias: None,
                    key: Some(Ident::new("id")),
                    labels: Vec::new(),
                }],
                edges: vec![EdgeTableDef {
                    table: name(&["ties"]),
                    alias: None,
                    key: None,
                    source: endpoint("src", Some(("members", "id"))),
                    destination: endpoint("dst", Some(("members", "id"))),
                    labels: Vec::new(),
                }],
            }
        );
    }

    #[test]
    fn duckpgqs_graph_without_a_vertex_key() {
        // DuckPGQ's own test graph: the key is inferred from the REFERENCES.
        let g = create(
            "CREATE PROPERTY GRAPH pg
               VERTEX TABLES (Student)
               EDGE TABLES (know SOURCE KEY (src) REFERENCES Student (id)
                                 DESTINATION KEY (dst) REFERENCES Student (id))",
        );
        assert_eq!(g.vertices[0].key, None);
        assert_eq!(g.vertices[0].table, name(&["Student"]));
        // Vertex tables alone are a graph too.
        let g = create("CREATE PROPERTY GRAPH v VERTEX TABLES (Student KEY (id))");
        assert!(g.edges.is_empty());
    }

    #[test]
    fn several_tables_aliases_labels_and_properties() {
        let g = create(
            "CREATE PROPERTY GRAPH social
               VERTEX TABLES (
                 people AS person KEY (id) LABEL person PROPERTIES (id, name AS full_name),
                 posts KEY (pid) DEFAULT LABEL,
                 tags NO PROPERTIES
               )
               EDGE TABLES (
                 follows KEY (fid) SOURCE KEY (a) REFERENCES person (id)
                                   DESTINATION KEY (b) REFERENCES person (id)
                         LABEL knows LABEL follows PROPERTIES ARE ALL COLUMNS EXCEPT (fid),
                 wrote SOURCE KEY (author) REFERENCES person (id)
                       DESTINATION KEY (post) REFERENCES posts (pid)
                       PROPERTIES ALL COLUMNS
               )",
        );
        assert_eq!(g.vertices.len(), 3);
        assert_eq!(g.edges.len(), 2);
        let person = &g.vertices[0];
        assert_eq!(person.alias, Some(Ident::new("person")));
        assert_eq!(
            person.labels,
            vec![LabelDef {
                name: Some(Ident::new("person")),
                properties: PropertiesDef::List(vec![
                    (Ident::new("id"), None),
                    (Ident::new("name"), Some(Ident::new("full_name"))),
                ]),
            }]
        );
        assert_eq!(
            g.vertices[1].labels,
            vec![LabelDef {
                name: None,
                properties: PropertiesDef::Default
            }]
        );
        assert_eq!(
            g.vertices[2].labels,
            vec![LabelDef {
                name: None,
                properties: PropertiesDef::None
            }]
        );
        let follows = &g.edges[0];
        assert_eq!(follows.key, Some(Ident::new("fid")));
        assert_eq!(follows.labels.len(), 2);
        assert_eq!(
            follows.labels[1].properties,
            PropertiesDef::AllColumns {
                except: vec![Ident::new("fid")]
            }
        );
        assert_eq!(
            g.edges[1].labels,
            vec![LabelDef {
                name: None,
                properties: PropertiesDef::AllColumns { except: Vec::new() }
            }]
        );
    }

    #[test]
    fn accepted_forms() {
        let g = create(
            "CREATE PROPERTY GRAPH karate_edges_only
               EDGE TABLES (ties SOURCE KEY (src) DESTINATION KEY (dst))",
        );
        assert!(g.vertices.is_empty());
        assert_eq!(g.edges[0].source, endpoint("src", None));
        assert_eq!(g.edges[0].destination, endpoint("dst", None));

        // Keywords in any case; qualified and quoted names; REFERENCES on one side.
        let g = create(
            r#"create or replace property graph if not exists "Social"
                 node tables (db.public."People" key ("Id"))
                 relationship tables (db.follows source key (a) references db.public."People" ("Id")
                                                  destination key (b))"#,
        );
        assert!(g.or_replace && g.if_not_exists);
        assert_eq!(g.name.to_string(), r#""Social""#);
        assert_eq!(g.vertices[0].table.to_string(), r#"db.public."People""#);
        assert_eq!(g.vertices[0].key, Some(Ident::with_quote('"', "Id")));
        assert_eq!(g.edges[0].table.to_string(), "db.follows");
        assert_eq!(
            g.edges[0]
                .source
                .references
                .as_ref()
                .unwrap()
                .table
                .to_string(),
            r#"db.public."People""#
        );
        assert_eq!(g.edges[0].destination.references, None);
    }

    #[test]
    fn display_round_trips() {
        for sql in [
            GETTING_STARTED,
            "CREATE PROPERTY GRAPH g EDGE TABLES (e SOURCE KEY (a) DESTINATION KEY (b))",
            "CREATE PROPERTY GRAPH v VERTEX TABLES (s)",
            r#"CREATE OR REPLACE PROPERTY GRAPH IF NOT EXISTS "G" VERTEX TABLES (s.v KEY ("K"))
               EDGE TABLES (s.e SOURCE KEY (a) DESTINATION KEY (b) REFERENCES s.v ("K"))"#,
            "CREATE PROPERTY GRAPH g VERTEX TABLES (p AS person KEY (id) LABEL person PROPERTIES \
             (id, n AS name), q DEFAULT LABEL, r NO PROPERTIES, t PROPERTIES ARE ALL COLUMNS \
             EXCEPT (x)) EDGE TABLES (e KEY (k) SOURCE KEY (a) REFERENCES person (id) \
             DESTINATION KEY (b) REFERENCES q (id) LABEL knows LABEL likes NO PROPERTIES)",
            // DuckPGQ's order: the properties, then the label they belong to.
            "CREATE PROPERTY GRAPH g VERTEX TABLES (s PROPERTIES (id, name) LABEL person, \
             t PROPERTIES (x) LABEL a LABEL b NO PROPERTIES)",
        ] {
            let parsed = UrsaStatement::CreatePropertyGraph(Box::new(create(sql)));
            let printed = parsed.to_string();
            let mut again = parse_sql(&printed).unwrap();
            assert_eq!(again.pop().unwrap(), parsed, "{printed}");
        }
        assert_eq!(
            UrsaStatement::CreatePropertyGraph(Box::new(create(GETTING_STARTED))).to_string(),
            "CREATE PROPERTY GRAPH karate VERTEX TABLES (members KEY (id)) EDGE TABLES (ties \
             SOURCE KEY (src) REFERENCES members (id) DESTINATION KEY (dst) REFERENCES members (id))"
        );
    }

    #[test]
    fn the_statement_after_a_graph_still_parses() {
        let sql =
            "CREATE PROPERTY GRAPH g EDGE TABLES (e SOURCE KEY (a) DESTINATION KEY (b)); SELECT 1";
        assert_eq!(parse_sql(sql).unwrap().len(), 2);
    }

    #[test]
    fn what_is_still_refused() {
        let multi_column =
            "multi-column keys are (KEY, SOURCE KEY, DESTINATION KEY and REFERENCES take one column)";
        for (sql, what) in [
            (
                "CREATE PROPERTY GRAPH g EDGE TABLES (e SOURCE KEY (s) DESTINATION KEY (d))
                   EDGE TABLES (f SOURCE KEY (s) DESTINATION KEY (d))",
                "a second EDGE TABLES clause is",
            ),
            (
                "CREATE PROPERTY GRAPH g EDGE TABLES (e SOURCE KEY (s) DESTINATION KEY (d))
                   VERTEX TABLES (v KEY (id))",
                "VERTEX TABLES after EDGE TABLES is",
            ),
            (
                "CREATE PROPERTY GRAPH g VERTEX TABLES (v KEY (a, b))
                   EDGE TABLES (e SOURCE KEY (s) DESTINATION KEY (d))",
                multi_column,
            ),
            (
                "CREATE PROPERTY GRAPH g EDGE TABLES (e SOURCE KEY (s1, s2) DESTINATION KEY (d))",
                multi_column,
            ),
            (
                "CREATE PROPERTY GRAPH g VERTEX TABLES (v KEY (id))
                   EDGE TABLES (e SOURCE KEY (s) REFERENCES v (id, x) DESTINATION KEY (d))",
                multi_column,
            ),
            (
                "CREATE PROPERTY GRAPH g VERTEX TABLES (v KEY (id))
                   EDGE TABLES (e SOURCE v DESTINATION v)",
                "SOURCE without KEY is (name the column with SOURCE KEY (column) REFERENCES \
                 vertex_table (key))",
            ),
            (
                "CREATE PROPERTY GRAPH g VERTEX TABLES (v KEY (id) PROPERTIES (upper(name)))",
                "a property that is not a column is",
            ),
            (
                "CREATE PROPERTY GRAPH g VERTEX TABLES (v KEY (id) PROPERTIES (a + b))",
                "a property that is not a column is",
            ),
            (
                "CREATE PROPERTY GRAPH g VERTEX TABLES (v KEY (id) LABEL a DEFAULT LABEL)",
                "this label clause is (write LABEL name, once per label)",
            ),
        ] {
            let message = error(sql);
            assert!(
                message.starts_with(&format!("{what} not supported yet at Line: ")),
                "{sql}\n{message}"
            );
        }
    }

    #[test]
    fn syntax_errors_read_like_datafusions() {
        for (sql, expected) in [
            (
                "CREATE PROPERTY GRAPH g",
                "syntax error: Expected: VERTEX TABLES or EDGE TABLES, found: EOF",
            ),
            (
                "CREATE PROPERTY GRAPH g EDGE TABLES e SOURCE KEY (s) DESTINATION KEY (d)",
                "syntax error: Expected: (, found: e at Line: 1, Column: 37",
            ),
            (
                "CREATE PROPERTY GRAPH g EDGE TABLES (e SOURCE KEY (s))",
                "syntax error: Expected: DESTINATION, found: ) at Line: 1, Column: 54",
            ),
            (
                "CREATE PROPERTY GRAPH g EDGE TABLES (e DESTINATION KEY (d) SOURCE KEY (s))",
                "syntax error: Expected: SOURCE, found: DESTINATION at Line: 1, Column: 40",
            ),
            (
                "CREATE PROPERTY GRAPH g EDGE TABLES (e SOURCE KEY s DESTINATION KEY (d))",
                "syntax error: Expected: (, found: s at Line: 1, Column: 51",
            ),
            (
                "CREATE PROPERTY GRAPH g EDGE TABLES (e SOURCE KEY (s) DESTINATION KEY (d)",
                "syntax error: Expected: ), found: EOF",
            ),
            (
                "CREATE PROPERTY GRAPH g VERTEX TABLES (v ID (id)) EDGE TABLES (e SOURCE KEY (s) DESTINATION KEY (d))",
                "syntax error: Expected: KEY, LABEL, PROPERTIES, ',' or ')', found: ID at Line: 1, Column: 42",
            ),
        ] {
            assert_eq!(error(sql), expected, "{sql}");
        }
    }
}
