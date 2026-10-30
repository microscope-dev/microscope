import { defineConfig } from "vitepress";

export default defineConfig({
  title: "microscope",
  description: "Debug and profile native programs on Linux",
  cleanUrls: true,
  lastUpdated: true,
  ignoreDeadLinks: false,
  // Vite clears the terminal when a build starts, erasing the output of the
  // `just` recipes that ran before this one.
  vite: { clearScreen: false },
  head: [
    ["meta", { name: "theme-color", content: "#3157c8" }],
  ],
  themeConfig: {
    siteTitle: "microscope",
    search: { provider: "local" },
    nav: [
      { text: "Debug", link: "/debugging" },
      { text: "Profile", link: "/profiling" },
      { text: "Reference", link: "/reference/command-line" },
    ],
    sidebar: [
      {
        text: "Start",
        items: [
          { text: "Getting started", link: "/getting-started" },
          { text: "Debugging", link: "/debugging" },
          { text: "Browser", link: "/browser" },
          { text: "Editors", link: "/editors" },
          { text: "Profiling", link: "/profiling" },
          { text: "System calls", link: "/syscalls" },
        ],
      },
      {
        text: "Values and languages",
        items: [
          { text: "Expressions", link: "/expressions" },
          { text: "Views", link: "/views" },
          { text: "Writing views", link: "/writing-views" },
          { text: "Visualizers", link: "/visualizers" },
          { text: "Languages", link: "/languages" },
        ],
      },
      {
        text: "Reference",
        items: [
          { text: "Command line", link: "/reference/command-line" },
          { text: "Configuration", link: "/reference/configuration" },
          { text: "Troubleshooting", link: "/reference/troubleshooting" },
          { text: "Known limits", link: "/reference/limitations" },
        ],
      },
    ],
    socialLinks: [
      { icon: "github", link: "https://github.com/microscope-dev/microscope" },
    ],
    editLink: {
      pattern: "https://github.com/microscope-dev/microscope/edit/main/docs/:path",
      text: "Edit this page on GitHub",
    },
    outline: { level: [2, 3] },
    docFooter: {
      prev: "Previous",
      next: "Next",
    },
  },
  markdown: {
    lineNumbers: true,
    theme: { light: "github-light", dark: "github-dark" },
    config(markdown) {
      const renderFence = markdown.renderer.rules.fence;
      markdown.renderer.rules.fence = (tokens, index, options, env, self) => {
        const token = tokens[index];
        const info = token.info;
        const language = info.split(/\s+/, 1)[0];
        if (language.startsWith("microscope-")) {
          token.info = `text${info.slice(language.length)}`;
        }
        const rendered = renderFence!(tokens, index, options, env, self);
        token.info = info;
        return rendered;
      };
    },
  },
});
