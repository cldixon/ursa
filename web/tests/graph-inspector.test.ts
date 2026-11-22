// The inspector's text (public/graph-inspector.js), without a browser: the
// commands its buttons run and the WHERE clause it offers to copy.
//
//   bun test web/tests/graph-inspector.test.ts

import { expect, test } from "bun:test";
import { identifier, literal, pathCommand, whereOf, withClause } from "../public/graph-inspector.js";

test("a name is bare when SQL reads it as typed, and quoted otherwise", () => {
  expect(identifier("club")).toBe("club");
  expect(identifier("page_rank2")).toBe("page_rank2");
  expect(identifier("Club")).toBe('"Club"');
  expect(identifier("first name")).toBe('"first name"');
  expect(identifier('say "hi"')).toBe('"say ""hi"""');
  expect(identifier("2nd")).toBe('"2nd"');
});

test("a key is a number for an int label, and a string otherwise", () => {
  expect(literal("33", "int")).toBe("33");
  expect(literal("-4", "int")).toBe("-4");
  expect(literal("33", "text")).toBe("'33'");
  expect(literal("O'Brien", "text")).toBe("'O''Brien'");
  // Never SQL of its own, whatever the label says.
  expect(literal("1; DROP TABLE t", "int")).toBe("'1; DROP TABLE t'");
});

test("colour by and size by change the clause, or add it", () => {
  expect(withClause(".graph karate", "COLOR BY", "club")).toBe(".graph karate COLOR BY club");
  expect(withClause(".graph karate SIZE BY pagerank", "COLOR BY", "club")).toBe(".graph karate SIZE BY pagerank COLOR BY club");
  expect(withClause(".graph karate SIZE BY pagerank COLOR BY louvain", "COLOR BY", "club")).toBe(".graph karate SIZE BY pagerank COLOR BY club");
  expect(withClause(".graph karate size by pagerank color by louvain;", "SIZE BY", "Age")).toBe('.graph karate size by "Age" color by louvain');
  expect(withClause('.graph karate COLOR BY "first name"', "COLOR BY", "club")).toBe(".graph karate COLOR BY club");
  expect(withClause(".graph", "SIZE BY", "id")).toBe(".graph SIZE BY id");
});

test("several nodes are a WHERE clause on their keys", () => {
  expect(
    whereOf([
      { key: "33", type: "int" },
      { key: "0", type: "int" },
    ]),
  ).toBe("WHERE id IN (33, 0)");
  expect(whereOf([{ key: "a'b", type: "text" }])).toBe("WHERE id IN ('a''b')");
});

test("a path between two nodes is a .graph of shortest_path", () => {
  expect(pathCommand("karate", { key: "16", type: "int" }, { key: "26", type: "int" })).toBe(".graph SELECT * FROM shortest_path(karate, source => 16, target => 26, direction => 'both')");
  expect(pathCommand("My Graph", { key: "a", type: "text" }, { key: "b", type: "text" })).toBe(`.graph SELECT * FROM shortest_path("My Graph", source => 'a', target => 'b', direction => 'both')`);
});
