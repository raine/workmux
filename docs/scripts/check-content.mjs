import fs from "node:fs";
import path from "node:path";

const contentRoot = new URL("../src/content/docs/", import.meta.url);
const failures = [];

function walk(directory) {
  return fs.readdirSync(directory, { withFileTypes: true }).flatMap((entry) => {
    const location = path.join(directory, entry.name);
    return entry.isDirectory() ? walk(location) : [location];
  });
}

for (const file of walk(contentRoot.pathname).filter((name) =>
  /\.mdx?$/.test(name),
)) {
  const relative = path.relative(contentRoot.pathname, file);
  const source = fs.readFileSync(file, "utf8");
  const frontmatter = source.match(/^---\n([\s\S]*?)\n---\n/);
  if (!frontmatter) {
    failures.push(`${relative}: missing frontmatter`);
    continue;
  }
  if (!/^title:\s*\S/m.test(frontmatter[1])) {
    failures.push(`${relative}: missing title`);
  }
  if (!/^description:\s*(?:\S|$)/m.test(frontmatter[1])) {
    failures.push(`${relative}: missing description`);
  }

  let inFence = false;
  const body = source.slice(frontmatter[0].length);
  for (const [index, line] of body.split("\n").entries()) {
    if (/^\s*(```|~~~)/.test(line)) {
      inFence = !inFence;
      continue;
    }
    if (inFence) continue;

    const lineNumber = index + frontmatter[0].split("\n").length;
    if (/^#\s+/.test(line)) {
      failures.push(
        `${relative}:${lineNumber}: body H1 duplicates the page title`,
      );
    }
    if (/^::::?\s+(?:info|warning|code-group)\b/.test(line)) {
      failures.push(`${relative}:${lineNumber}: VitePress directive`);
    }
    if (/\]\((?:\.\/|\.\.\/)/.test(line)) {
      failures.push(`${relative}:${lineNumber}: relative documentation link`);
    }
    if (/\bv-(?:pre|if|html)\b|<script setup>|from ['"]vue['"]/.test(line)) {
      failures.push(`${relative}:${lineNumber}: Vue syntax`);
    }
  }
}

if (failures.length > 0) {
  console.error(failures.join("\n"));
  process.exit(1);
}

console.log("Documentation content checks passed.");
