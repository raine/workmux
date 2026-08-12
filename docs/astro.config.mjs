// @ts-check
import { defineConfig } from "astro/config";
import starlight from "@astrojs/starlight";
import starlightLinksValidator from "starlight-links-validator";
import starlightLlmsTxt from "starlight-llms-txt";

const site = "https://workmux.raine.dev";
const socialImage = `${site}/social-preview.png`;

export default defineConfig({
  site,
  trailingSlash: "always",
  build: { format: "directory" },
  image: {
    layout: "constrained",
    responsiveStyles: true,
  },
  integrations: [
    starlight({
      title: "workmux",
      description:
        "A CLI tool for parallel development with AI coding agents using git worktrees and tmux",
      locales: {
        root: { label: "English", lang: "en-US" },
      },
      logo: {
        dark: "./src/assets/icon-dark.svg",
        light: "./src/assets/icon.svg",
        alt: "workmux branch logo",
      },
      favicon: "/branch-icon.svg",
      lastUpdated: true,
      plugins: [
        starlightLlmsTxt(),
        starlightLinksValidator({
          errorOnRelativeLinks: true,
          errorOnInvalidHashes: true,
        }),
      ],
      editLink: {
        baseUrl: "https://github.com/raine/workmux/edit/main/docs/",
      },
      social: [
        {
          icon: "github",
          label: "GitHub",
          href: "https://github.com/raine/workmux",
        },
      ],
      components: {
        SocialIcons: "./src/components/HeaderLinks.astro",
        PageTitle: "./src/components/PageTitle.astro",
      },
      customCss: ["./src/styles/theme.css", "./src/styles/code.css"],
      head: [
        {
          tag: "meta",
          attrs: { property: "og:image", content: socialImage },
        },
        {
          tag: "meta",
          attrs: { property: "og:image:width", content: "1280" },
        },
        {
          tag: "meta",
          attrs: { property: "og:image:height", content: "640" },
        },
        {
          tag: "meta",
          attrs: {
            property: "og:image:alt",
            content: "workmux terminal workflow",
          },
        },
        {
          tag: "meta",
          attrs: { name: "twitter:image", content: socialImage },
        },
        {
          tag: "script",
          attrs: { src: "/image-zoom.js", defer: true },
        },
        {
          tag: "script",
          attrs: { src: "/video-player.js", defer: true },
        },
      ],
      sidebar: [
        {
          label: "Getting Started",
          items: [
            { label: "What is workmux?", slug: "guide" },
            { label: "Installation", slug: "guide/installation" },
            { label: "Quick start", slug: "guide/quick-start" },
            { label: "Configuration", slug: "guide/configuration" },
            { label: "Commands", slug: "reference/commands" },
          ],
        },
        {
          label: "AI Agents",
          items: [
            { label: "Overview", slug: "guide/agents" },
            { label: "Workflows", slug: "guide/workflows" },
            { label: "Claude Code", slug: "guide/claude-code" },
            { label: "Status tracking", slug: "guide/status-tracking" },
            { label: "Skills", slug: "guide/skills" },
          ],
        },
        {
          label: "Dashboard",
          items: [
            { label: "Overview", slug: "guide/dashboard" },
            { label: "Diff view", slug: "guide/dashboard/diff-view" },
            { label: "Patch mode", slug: "guide/dashboard/patch-mode" },
            { label: "Configuration", slug: "guide/dashboard/configuration" },
          ],
        },
        {
          label: "Sidebar",
          items: [
            { label: "Overview", slug: "guide/sidebar" },
            { label: "Customization", slug: "guide/sidebar/customization" },
            { label: "Remote agents", slug: "guide/sidebar/remote-agents" },
          ],
        },
        {
          label: "Sandbox",
          items: [
            { label: "Overview", slug: "guide/sandbox" },
            { label: "Container backend", slug: "guide/sandbox/container" },
            { label: "Lima VM backend", slug: "guide/sandbox/lima" },
            { label: "Shared features", slug: "guide/sandbox/features" },
            { label: "Alternatives", slug: "guide/sandbox/alternatives" },
          ],
        },
        {
          label: "Alternative backends",
          items: [
            { label: "kitty", slug: "guide/kitty" },
            { label: "WezTerm", slug: "guide/wezterm" },
            { label: "Zellij", slug: "guide/zellij" },
          ],
        },
        {
          label: "Guides",
          items: [
            { label: "Session mode", slug: "guide/session-mode" },
            { label: "direnv", slug: "guide/direnv" },
            { label: "Monorepos", slug: "guide/monorepos" },
            {
              label: "Git worktree caveats",
              slug: "guide/git-worktree-caveats",
            },
            { label: "Nix", slug: "guide/nix" },
            { label: "Changelog", slug: "changelog" },
          ],
        },
        {
          label: "Commands",
          items: [
            { label: "Overview", slug: "reference/commands" },
            { label: "add", slug: "reference/commands/add" },
            { label: "merge", slug: "reference/commands/merge" },
            { label: "rebase", slug: "reference/commands/rebase" },
            { label: "remove", slug: "reference/commands/remove" },
            { label: "rename", slug: "reference/commands/rename" },
            { label: "list", slug: "reference/commands/list" },
            { label: "status", slug: "reference/commands/status" },
            { label: "open", slug: "reference/commands/open" },
            { label: "close", slug: "reference/commands/close" },
            { label: "resurrect", slug: "reference/commands/resurrect" },
            { label: "sync-files", slug: "reference/commands/sync-files" },
            { label: "path", slug: "reference/commands/path" },
            { label: "dashboard", slug: "reference/commands/dashboard" },
            { label: "sidebar", slug: "reference/commands/sidebar" },
            { label: "reap-agents", slug: "reference/commands/reap-agents" },
            { label: "config edit", slug: "reference/commands/config" },
            { label: "init", slug: "reference/commands/init" },
            { label: "claude prune", slug: "reference/commands/claude" },
            { label: "sandbox", slug: "reference/commands/sandbox" },
            { label: "completions", slug: "reference/commands/completions" },
            { label: "docs", slug: "reference/commands/docs" },
            { label: "update", slug: "reference/commands/update" },
            { label: "last-done", slug: "reference/commands/last-done" },
          ],
        },
      ],
    }),
  ],
});
