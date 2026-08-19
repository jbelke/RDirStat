#!/usr/bin/env node

import fs from "node:fs";
import path from "node:path";
import process from "node:process";
import { fileURLToPath } from "node:url";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const failures = [];

function relative(file) {
  return path.relative(root, file) || ".";
}

function fail(file, line, message) {
  failures.push(`${relative(file)}:${line}: ${message}`);
}

function markdownFilesUnder(directory) {
  if (!fs.existsSync(directory)) return [];

  const files = [];
  for (const entry of fs.readdirSync(directory, { withFileTypes: true })) {
    const target = path.join(directory, entry.name);
    if (entry.isSymbolicLink()) continue;
    if (entry.isDirectory()) files.push(...markdownFilesUnder(target));
    if (entry.isFile() && entry.name.endsWith(".md")) files.push(target);
  }
  return files;
}

const markdownFiles = [
  path.join(root, "AGENTS.md"),
  path.join(root, "CLAUDE.md"),
  path.join(root, "README.md"),
  path.join(root, "CHANGELOG.md"),
  path.join(root, "LICENSING.md"),
  path.join(root, "NOTICE"),
  ...markdownFilesUnder(path.join(root, "crates")),
  ...markdownFilesUnder(path.join(root, "src-tauri")),
  ...markdownFilesUnder(path.join(root, "src")),
  ...markdownFilesUnder(path.join(root, "skills")),
].filter((file, index, files) => fs.existsSync(file) && files.indexOf(file) === index);

function slugHeadings(file) {
  const slugs = new Set();
  const occurrences = new Map();
  const lines = fs.readFileSync(file, "utf8").split(/\r?\n/u);

  for (const line of lines) {
    const heading = /^(?:#{1,6})\s+(.+?)\s*#*\s*$/u.exec(line);
    if (!heading) continue;

    const base = heading[1]
      .toLocaleLowerCase("en-US")
      .replace(/<[^>]*>/gu, "")
      .replace(/[`*_~]/gu, "")
      .replace(/[^\p{Letter}\p{Number}\p{Mark}\s_-]/gu, "")
      .trim()
      .replace(/\s+/gu, "-");
    const duplicate = occurrences.get(base) ?? 0;
    occurrences.set(base, duplicate + 1);
    slugs.add(duplicate === 0 ? base : `${base}-${duplicate}`);
  }
  return slugs;
}

const headingCache = new Map();
function headings(file) {
  if (!headingCache.has(file)) headingCache.set(file, slugHeadings(file));
  return headingCache.get(file);
}

function isLocalOnlyPath(rel) {
  const top = rel.split(path.sep)[0];
  return top === ".settings" || top === "docs" || top === "reference-code";
}

function checkLinks(file) {
  const lines = fs.readFileSync(file, "utf8").split(/\r?\n/u);
  let inFence = false;

  lines.forEach((line, index) => {
    if (/^\s*```/u.test(line)) {
      inFence = !inFence;
      return;
    }
    if (inFence) return;

    const links = line.matchAll(/!?\[[^\]]*\]\(([^)]+)\)/gu);
    for (const match of links) {
      let target = match[1].trim().replace(/^<|>$/gu, "");
      if (/^(?:https?:|mailto:)/u.test(target)) continue;
      target = target.split(/\s+(?=["'])/u, 1)[0];

      const [rawPath, anchor] = target.split("#", 2);
      let decodedPath;
      try {
        decodedPath = decodeURIComponent(rawPath);
      } catch {
        fail(file, index + 1, `invalid URL encoding in local link: ${target}`);
        continue;
      }

      const destination = decodedPath
        ? path.resolve(path.dirname(file), decodedPath)
        : file;
      const rel = relative(destination);

      if (!fs.existsSync(destination)) {
        if (isLocalOnlyPath(rel)) {
          fail(
            file,
            index + 1,
            `public markdown must not link to local-only path: ${target}`,
          );
        } else {
          fail(file, index + 1, `missing local link target: ${target}`);
        }
        continue;
      }
      if (anchor && fs.statSync(destination).isFile() && !headings(destination).has(anchor)) {
        fail(file, index + 1, `missing anchor #${anchor} in ${rel}`);
      }
    }
  });
}

for (const file of markdownFiles) checkLinks(file);

const lockFile = path.join(root, "skills-lock.json");
const skillLock = fs.existsSync(lockFile)
  ? JSON.parse(fs.readFileSync(lockFile, "utf8")).skills ?? {}
  : {};

const skillsRoot = path.join(root, "skills");
if (fs.existsSync(skillsRoot)) {
  for (const entry of fs.readdirSync(skillsRoot, { withFileTypes: true })) {
    const directory = path.join(skillsRoot, entry.name);
    if (entry.isSymbolicLink()) {
      if (!Object.hasOwn(skillLock, entry.name)) {
        fail(lockFile, 1, `installed skill ${entry.name} has no lock entry`);
      }
      continue;
    }
    if (!entry.isDirectory()) continue;

    const skillFile = path.join(directory, "SKILL.md");
    if (!fs.existsSync(skillFile)) {
      fail(directory, 1, "skill directory has no SKILL.md");
      continue;
    }
    const name = /^name:\s*(.+?)\s*$/mu.exec(fs.readFileSync(skillFile, "utf8"))?.[1];
    if (name !== entry.name) {
      fail(skillFile, 1, `frontmatter name must be ${entry.name} (found ${name ?? "none"})`);
    }
  }
}

const staleClaims = new Map([
  [path.join(root, "AGENTS.md"), [
    "There is no application code in this repository yet",
    "no build system, test suite, linter, or gate exists",
  ]],
  [path.join(root, "README.md"), ["Nothing to build yet", "No `Cargo.toml`"]],
]);

for (const [file, claims] of staleClaims) {
  if (!fs.existsSync(file)) continue;
  const text = fs.readFileSync(file, "utf8");
  for (const claim of claims) {
    if (text.toLocaleLowerCase("en-US").includes(claim.toLocaleLowerCase("en-US"))) {
      fail(file, 1, `stale phase-0 claim remains: ${claim}`);
    }
  }
}

if (failures.length > 0) {
  console.error(failures.join("\n"));
  console.error(`\nDocumentation checks failed (${failures.length}).`);
  process.exitCode = 1;
} else {
  console.log(`Documentation checks passed (${markdownFiles.length} files).`);
}
