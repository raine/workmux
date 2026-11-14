import path from "node:path";

const releaseHeading = /^(.*?)\s*\((\d{4}-\d{2}-\d{2})\)\s*$/;

const groups = [
  { key: "breaking", label: "Breaking", prefixes: ["Breaking"] },
  { key: "new", label: "New", prefixes: [null, "New"] },
  { key: "improved", label: "Improved", prefixes: ["UX"] },
  { key: "perf", label: "Performance", prefixes: ["Perf"] },
  { key: "fix", label: "Fixes", prefixes: ["Fix"] },
];
const knownPrefixes = groups.flatMap((group) => group.prefixes).filter(Boolean);
const bulletPrefix = /^(\w+):\s+/;
const fixVerb = /^Fix(ed)?\b/;

// Renders each dated changelog release as a date line above a version-only
// heading, with its bullets grouped by their "Breaking:", "New:", "UX:",
// "Perf:" or "Fix:" prefixes. Bullets that open with the verb "Fix" or
// "Fixed" are fixes as written; other unprefixed bullets count as new.
export default function changelogReleases() {
  return (tree, file) => {
    if (!isChangelog(file.path)) return;

    const children = [];
    let latest = true;
    for (let i = 0; i < tree.children.length; i++) {
      const node = tree.children[i];
      const match =
        node.type === "heading" &&
        node.depth === 2 &&
        headingText(node).match(releaseHeading);
      if (!match) {
        children.push(node);
        continue;
      }

      const [, version, date] = match;
      const badge = latest
        ? '<span class="changelog-latest">Latest</span>'
        : "";
      latest = false;
      children.push({
        type: "html",
        value: `<p class="changelog-date not-content"><time datetime="${date}">${longDate(date)}</time>${badge}</p>`,
      });
      const id = "v" + version.replace(/^v/, "").replace(/\./g, "-");
      children.push({
        ...node,
        data: { ...node.data, hProperties: { ...node.data?.hProperties, id } },
        children: [{ type: "text", value: version }],
      });

      // The release's body runs to the next heading; its lists are merged
      // and regrouped, anything else stays in place ahead of them.
      const items = [];
      let list;
      while (
        i + 1 < tree.children.length &&
        tree.children[i + 1].type !== "heading"
      ) {
        const next = tree.children[++i];
        if (next.type === "list") {
          list ??= next;
          items.push(...next.children);
        } else {
          children.push(next);
        }
      }
      if (list) children.push(...groupItems({ ...list, children: items }));
    }
    tree.children = children;
  };
}

function groupItems(list) {
  const present = groups
    .map((group) => ({
      ...group,
      items: list.children.filter((item) =>
        group.prefixes.includes(prefixOf(item)),
      ),
    }))
    .filter((group) => group.items.length > 0);

  return present.flatMap((group) => {
    const nodes = [];
    if (present.length > 1 || group.key !== "new") {
      nodes.push({
        type: "html",
        value: `<p class="changelog-group changelog-group-${group.key} not-content">${group.label}</p>`,
      });
    }
    nodes.push({ ...list, children: group.items.map(stripPrefix) });
    return nodes;
  });
}

function firstText(item) {
  const paragraph = item.children[0];
  const text =
    paragraph?.type === "paragraph" ? paragraph.children[0] : undefined;
  return text?.type === "text" ? text : undefined;
}

function explicitPrefix(item) {
  const prefix = firstText(item)?.value.match(bulletPrefix)?.[1];
  return knownPrefixes.includes(prefix) ? prefix : null;
}

function prefixOf(item) {
  const prefix = explicitPrefix(item);
  if (prefix) return prefix;
  return fixVerb.test(firstText(item)?.value ?? "") ? "Fix" : null;
}

function stripPrefix(item) {
  const text = firstText(item);
  if (explicitPrefix(item)) text.value = text.value.replace(bulletPrefix, "");
  return item;
}

function longDate(iso) {
  return new Date(`${iso}T00:00:00Z`).toLocaleDateString("en-US", {
    timeZone: "UTC",
    month: "long",
    day: "numeric",
    year: "numeric",
  });
}

function isChangelog(filePath) {
  return path.basename(filePath ?? "").toLowerCase() === "changelog.md";
}

function headingText(node) {
  return node.children
    .map((child) => {
      if ("value" in child) return child.value;
      if ("children" in child) return headingText(child);
      return "";
    })
    .join("");
}
