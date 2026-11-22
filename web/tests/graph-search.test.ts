// The search box's commands (public/graph-search.js), with no browser:
//   bun test tests/graph-search.test.ts
import { expect, test } from "bun:test";
import { findCommand, idWord } from "../public/graph-search.js";

test("the box runs .find with what was typed, on one line", () => {
  expect(findCommand("33")).toBe(".find 33");
  expect(findCommand("  Mr. Hi ")).toBe(".find Mr. Hi");
  expect(findCommand("WHERE club = 'Officer';")).toBe(".find WHERE club = 'Officer'");
  // A line break would be a second command.
  expect(findCommand("a\n.quit")).toBe(".find a .quit");
  expect(findCommand("")).toBeNull();
  expect(findCommand(" ; ")).toBeNull();
});

test("a key is an id an explore command can read", () => {
  expect(idWord("33")).toBe("33");
  expect(idWord("ann")).toBe("ann");
  expect(idWord("cy d")).toBe("'cy d'");
  expect(idWord("o'neil")).toBe("'o''neil'");
  expect(idWord("")).toBe("''");
});
