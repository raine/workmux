import { defineConfig } from "vitepress";
import fs from "fs";
import path from "path";
import { retinaImagesPlugin } from "./retina-images";

const publicDir = path.join(__dirname, "..", "public");

export default defineConfig({
  transformPageData(pageData) {
    const filePath = path.join(__dirname, "..", pageData.relativePath);
    if (fs.existsSync(filePath)) {
      const content = fs.readFileSync(filePath, "utf-8");
      (pageData as any).rawMarkdownBase64 =
        Buffer.from(content).toString("base64");
    }
  },

  title: "workmux",
  description:
    "A CLI tool for parallel development with AI coding agents using git worktrees and tmux",
  lang: "en-US",
  lastUpdated: true,
  cleanUrls: true,
  sitemap: {
    hostname: "https://workmux.raine.dev",
  },

  head: [
    ["link", { rel: "icon", href: "/branch-icon.svg" }],
    [
      "meta",
      { name: "algolia-site-verification", content: "3CFC51B41FBBDD13" },
    ],
  ],

  vite: {
    resolve: {
      preserveSymlinks: true,
    },
    server: {
      fs: {
        allow: [".."],
      },
    },
    plugins: [retinaImagesPlugin(publicDir)],
  },

  themeConfig: {
    logo: { light: "/icon.svg", dark: "/icon-dark.svg" },
    siteTitle: "workmux",

    search: {
      provider: "algolia",
      options: {
        appId: "LE5BQE6V5G",
        apiKey: "5155e711e5233eab82a26f248b60b61b",
        indexName: "Workmux website",
      },
    },

    nav: [
      { text: "Guide", link: "/guide/" },
      { text: "Changelog", link: "/changelog" },
    ],

    sidebar: [
      {
        text: "Getting Started",
        items: [
          { text: "What is workmux?", link: "/guide/" },
          { text: "Installation", link: "/guide/installation" },
          { text: "Quick start", link: "/guide/quick-start" },
          { text: "Configuration", link: "/guide/configuration" },
          { text: "Commands", link: "/reference/commands/" },
        ],
      },
      {
        text: "AI Agents",
        items: [
          { text: "Overview", link: "/guide/agents" },
          { text: "Workflows", link: "/guide/workflows" },
          { text: "Claude Code", link: "/guide/claude-code" },
          { text: "Status tracking", link: "/guide/status-tracking" },
          { text: "Skills", link: "/guide/skills" },
        ],
      },
      {
        text: "Dashboard",
        items: [
          { text: "Overview", link: "/guide/dashboard/" },
          { text: "Diff view", link: "/guide/dashboard/diff-view" },
          { text: "Patch mode", link: "/guide/dashboard/patch-mode" },
          { text: "Configuration", link: "/guide/dashboard/configuration" },
        ],
      },
      {
        text: "Sandbox",
        items: [
          { text: "Overview", link: "/guide/sandbox/" },
          { text: "Container backend", link: "/guide/sandbox/container" },
          { text: "Lima VM backend", link: "/guide/sandbox/lima" },
          { text: "Shared features", link: "/guide/sandbox/features" },
          { text: "Alternatives", link: "/guide/sandbox/alternatives" },
        ],
      },
      {
        text: "Alternative backends",
        items: [
          { text: "kitty", link: "/guide/kitty" },
          { text: "WezTerm", link: "/guide/wezterm" },
          { text: "Zellij", link: "/guide/zellij" },
        ],
      },
      {
        text: "Guides",
        items: [
          { text: "Session mode", link: "/guide/session-mode" },
          { text: "direnv", link: "/guide/direnv" },
          { text: "Monorepos", link: "/guide/monorepos" },
          { text: "Worktree caveats", link: "/guide/git-worktree-caveats" },
          { text: "Nix", link: "/guide/nix" },
        ],
      },
      {
        text: "Commands",
        items: [
          { text: "add", link: "/reference/commands/add" },
          { text: "merge", link: "/reference/commands/merge" },
          { text: "remove", link: "/reference/commands/remove" },
          { text: "list", link: "/reference/commands/list" },
          { text: "open", link: "/reference/commands/open" },
          { text: "close", link: "/reference/commands/close" },
          { text: "path", link: "/reference/commands/path" },
          { text: "dashboard", link: "/reference/commands/dashboard" },
          { text: "init", link: "/reference/commands/init" },
          { text: "claude prune", link: "/reference/commands/claude" },
          { text: "sandbox", link: "/reference/commands/sandbox" },
          { text: "completions", link: "/reference/commands/completions" },
          { text: "docs", link: "/reference/commands/docs" },
          { text: "update", link: "/reference/commands/update" },
        ],
      },
    ],

    socialLinks: [{ icon: "github", link: "https://github.com/raine/workmux" }],

    footer: {
      message: "Released under the MIT License.",
    },

    editLink: {
      pattern: "https://github.com/raine/workmux/edit/main/docs/:path",
      text: "Edit this page on GitHub",
    },
  },
});
