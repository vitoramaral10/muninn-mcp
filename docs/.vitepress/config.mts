import { defineConfig } from 'vitepress'

const repo = 'https://github.com/vitoramaral10/muninn-mcp'

export default defineConfig({
  title: 'Muninn',
  description: 'Long-term memory for AI assistants, kept as plain Markdown files. An MCP server in one static Rust binary.',
  // Served from GitHub Pages under the repository's name.
  base: '/muninn-mcp/',
  cleanUrls: true,
  lastUpdated: true,
  markdown: {
    config(md) {
      // tools.md is generated from the tool descriptions, which may say '<project>'
      // outside code spans; Vue would read that as a tag, so escape it there.
      md.core.ruler.before('block', 'escape-generated-angles', (state) => {
        if (state.env.relativePath !== 'tools.md') return
        state.src = state.src
          .split(/(`[^`\n]*`)/)
          .map((part) => (part.startsWith('`') ? part : part.replaceAll('<', '&lt;')))
          .join('')
      })
    },
  },
  head: [
    ['link', { rel: 'icon', type: 'image/svg+xml', href: '/muninn-mcp/logo.svg' }],
    ['meta', { name: 'theme-color', content: '#6d7bff' }],
    ['meta', { property: 'og:type', content: 'website' }],
    ['meta', { property: 'og:title', content: 'Muninn: long-term memory for AI assistants' }],
    ['meta', { property: 'og:description', content: 'An MCP server over a folder of Markdown notes: hybrid search, a knowledge graph and safe writes.' }],
  ],
  themeConfig: {
    logo: '/logo.svg',
    nav: [
      { text: 'Guide', link: '/getting-started', activeMatch: '^/(getting-started|usage)' },
      { text: 'Reference', link: '/tools', activeMatch: '^/(tools|operations|architecture)' },
      { text: 'Changelog', link: `${repo}/blob/main/CHANGELOG.md` },
    ],
    sidebar: [
      {
        text: 'Guide',
        items: [
          { text: 'Getting started', link: '/getting-started' },
          { text: 'Using Muninn', link: '/usage' },
        ],
      },
      {
        text: 'Reference',
        items: [
          { text: 'Tools', link: '/tools' },
          { text: 'Running Muninn', link: '/operations' },
          { text: 'Architecture', link: '/architecture' },
        ],
      },
      {
        text: 'Project',
        items: [
          { text: 'Contributing', link: `${repo}/blob/main/CONTRIBUTING.md` },
          { text: 'Security', link: `${repo}/blob/main/SECURITY.md` },
          { text: 'Changelog', link: `${repo}/blob/main/CHANGELOG.md` },
        ],
      },
    ],
    socialLinks: [{ icon: 'github', link: repo }],
    search: { provider: 'local' },
    outline: { level: [2, 3] },
    editLink: { pattern: `${repo}/edit/main/docs/:path`, text: 'Edit this page on GitHub' },
    footer: {
      message: 'Released under the MIT License.',
      copyright: 'Copyright © 2026 Vitor Amaral',
    },
  },
})
