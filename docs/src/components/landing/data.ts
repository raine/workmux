import { getEntry } from "astro:content";
import { getGitHubStars } from "./stars";

export const starsPromise = getGitHubStars();

// The release pill reads CHANGELOG.md (the docs changelog page). A release can
// name its headline feature with a `<!-- highlight: ... -->` comment under its
// heading; the pill shows the newest release that has one, or the newest
// release with a generic label when none does.
async function latestRelease() {
  const body = (await getEntry("docs", "changelog"))?.body ?? "";
  const releases = body
    .split(/^## /m)
    .slice(1)
    .map((s) => ({
      version: s.match(/^v[\d.]+/)?.[0],
      highlight: s.match(/<!--\s*highlight:\s*(.+?)\s*-->/)?.[1],
    }))
    .filter(
      (r): r is { version: string; highlight: string | undefined } =>
        !!r.version,
    );
  const featured = releases.find((r) => r.highlight);
  return featured
    ? { version: featured.version, latest: featured.highlight! }
    : { version: releases[0]?.version ?? "", latest: "What’s new" };
}

export const { version, latest } = await latestRelease();

export const install = [
  {
    id: "curl",
    label: "curl",
    cmd: "curl -fsSL https://raw.githubusercontent.com/raine/workmux/main/scripts/install.sh | bash",
  },
  { id: "brew", label: "Homebrew", cmd: "brew install raine/workmux/workmux" },
  { id: "cargo", label: "Cargo", cmd: "cargo install workmux" },
];

export const testimonials = [
  {
    quote:
      "It's become my daily driver - the perfect level of abstraction over tmux + git, without getting in the way or obscuring the underlying tooling.",
    author: "@cisaacstern",
    source: "GitHub",
    href: "https://github.com/raine/workmux/issues/33",
  },
  {
    quote:
      "workmux is fantastic. nothing too fancy, solves a real problem and stays in TMUX ❤️",
    author: "@devopstoolbox",
    source: "X",
    href: "https://x.com/devopstoolbox/status/2094442083717390547",
  },
  {
    quote:
      "I have to mention workmux at every opportunity because it's the perfect glue between worktrees, agents and tmux windows.",
    author: "@dedbrizz",
    source: "Threads",
    href: "https://www.threads.com/@dedbrizz/post/DVt1DtLkr_l",
  },
  {
    quote:
      "Since I discovered workmux I'm cooking. Amazing parallel-agent driver if you like tmux and want to keep your config.",
    author: "@MatijaSosic",
    source: "X",
    href: "https://x.com/MatijaSosic/status/2080022087281127659",
  },
  {
    quote:
      "Thank you so much for your work with workmux! It's a tool I've been wanting to exist for a long time.",
    author: "@rstacruz",
    source: "GitHub",
    href: "https://github.com/raine/workmux/issues/2",
  },
  {
    quote:
      "I've been using (and loving) workmux which brings together tmux, git worktrees, and CLI agents into an opinionated workflow.",
    author: "@Coolin96",
    source: "Hacker News",
    href: "https://news.ycombinator.com/item?id=46029809",
  },
];

export const workflows = [
  {
    id: "solo",
    name: "Solo",
    command: 'workmux add -A -p "Add cursor-based pagination to /api/users"',
    href: "/reference/commands/add/",
    link: "workmux add",
    kind: "Command",
    prompt: "$",
    body: "One command creates a branch, worktree, and starts an agent with your prompt in a new window.",
  },
  {
    id: "delegated",
    name: "Delegated",
    command: "/worktree Implement the caching layer",
    href: "/guide/skills/#worktree",
    link: "/worktree",
    kind: "Skill",
    prompt: "❯",
    body: "From inside an agent, spin off a subtask to a new worktree with full context.",
  },
  {
    id: "coordinated",
    name: "Coordinated",
    command: "/coordinator Break down the auth refactor into parallel tasks",
    href: "/guide/skills/#coordinator",
    link: "/coordinator",
    kind: "Skill",
    prompt: "❯",
    body: "One agent spawns, monitors, and merges multiple worktree agents.",
  },
];

export const videos = [
  {
    title: "Tmux is Still Better Than Herdr (Here’s Why)",
    author: "Sam Natale",
    href: "https://www.youtube.com/watch?v=ww1wykxJps8",
  },
  {
    title: "I Don’t Want a New Terminal. I Want THIS for Tmux.",
    author: "DevOps Toolbox",
    href: "https://www.youtube.com/watch?v=7uumknh43rM",
  },
  {
    title: "Tmux Didn’t Need a Replacement, It Needed Workmux",
    author: "Seth Phaeno",
    href: "https://www.youtube.com/watch?v=0RGyt3EDP-E",
  },
];
