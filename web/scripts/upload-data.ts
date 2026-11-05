// Stage the sample datasets in .data/ and upload them to the bucket under
// datasets/. Each of the library's bundled CSVs goes up as is (the Getting
// Started SQL reads them by name) and as Parquet, written by the native `ursa`
// binary, so the shell can show range reads of Parquet. A manifest lists them
// with their sources and licence.
//
//   bun run upload:data [--local] [--dry]   (builds the native ursa binary if needed)
import { copyFileSync, mkdirSync, readdirSync, rmSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { cargoTargetDir, DATA_DIR, put, REPO, run } from "./common";

const local = Bun.argv.includes("--local");
const dry = Bun.argv.includes("--dry");
const source = join(REPO, "python", "ursa", "datasets", "data");

const DATASETS = [
  {
    name: "karate",
    title: "Zachary's karate club",
    files: ["karate_edges", "karate_nodes"],
    source: "W. W. Zachary, An Information Flow Model for Conflict and Fission in Small Groups, Journal of Anthropological Research 33 (1977).",
  },
  {
    name: "lesmis",
    title: "Les Misérables co-appearances (weighted)",
    files: ["lesmis_edges"],
    source: "D. E. Knuth, The Stanford GraphBase (1993).",
  },
  {
    name: "florentine",
    title: "Florentine families marriage ties",
    files: ["florentine_edges"],
    source: "J. F. Padgett & C. K. Ansell, Robust Action and the Rise of the Medici (1993).",
  },
  {
    name: "kite",
    title: "Krackhardt kite",
    files: ["kite_edges"],
    source: "D. Krackhardt, Assessing the Political Landscape (1990).",
  },
];
const LICENCE =
  "Redistributed from NetworkX (3-clause BSD licence, https://github.com/networkx/networkx/blob/main/LICENSE.txt) " +
  "as plain CSV, as in Ursa's python/ursa/datasets/LICENSES.md; the Parquet files are the same rows.";

run("cargo", ["build", "-p", "ursa-cli", "--release", "--locked"], { cwd: REPO });
const ursa = join(cargoTargetDir(), "release", "ursa");

rmSync(DATA_DIR, { recursive: true, force: true });
mkdirSync(DATA_DIR, { recursive: true });
for (const d of DATASETS) {
  for (const f of d.files) {
    copyFileSync(join(source, `${f}.csv`), join(DATA_DIR, `${f}.csv`));
    run(ursa, ["-c", `COPY (SELECT * FROM '${f}.csv') TO '${f}.parquet'`], { cwd: DATA_DIR });
  }
}
const manifest = {
  licence: LICENCE,
  datasets: DATASETS.map((d) => ({ ...d, files: d.files.flatMap((f) => [`${f}.parquet`, `${f}.csv`]) })),
};
writeFileSync(join(DATA_DIR, "manifest.json"), JSON.stringify(manifest, null, 2) + "\n");

const TYPES: Record<string, string> = {
  parquet: "application/vnd.apache.parquet",
  csv: "text/csv; charset=utf-8",
  json: "application/json",
};
for (const file of readdirSync(DATA_DIR).sort()) {
  const type = TYPES[file.slice(file.lastIndexOf(".") + 1)];
  if (dry) console.log(`would upload datasets/${file} (${type})`);
  else put(`datasets/${file}`, join(DATA_DIR, file), type, local);
}
