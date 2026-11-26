// The parts of CodeMirror 6 the console's SQL editor (web/console/editor.js)
// uses, bundled by scripts/console.ts into one ES module,
// dist-console/console/vendor/codemirror.mjs, so every part shares one copy
// of @codemirror/state (CodeMirror breaks with two). The versions are pinned
// in package.json; every package bundled is MIT (their LICENSE files are
// copied beside it).
//
// No language package: the colours are the session's (`highlight`), as the
// shell's terminal colours them, applied as decorations.
export { EditorState, EditorSelection, StateEffect, StateField, Compartment, RangeSetBuilder, Prec } from "@codemirror/state";
export {
  EditorView,
  Decoration,
  ViewPlugin,
  WidgetType,
  keymap,
  lineNumbers,
  highlightActiveLine,
  highlightActiveLineGutter,
  highlightSpecialChars,
  drawSelection,
  dropCursor,
  rectangularSelection,
  crosshairCursor,
  placeholder,
} from "@codemirror/view";
export { defaultKeymap, history, historyKeymap, indentWithTab, insertNewlineAndIndent, toggleComment } from "@codemirror/commands";
export {
  autocompletion,
  completionKeymap,
  closeBrackets,
  closeBracketsKeymap,
  startCompletion,
  closeCompletion,
  acceptCompletion,
  completionStatus,
  currentCompletions,
  selectedCompletion,
} from "@codemirror/autocomplete";
export { setDiagnostics, lintGutter, forEachDiagnostic, diagnosticCount, openLintPanel } from "@codemirror/lint";
export { search, searchKeymap, highlightSelectionMatches } from "@codemirror/search";
