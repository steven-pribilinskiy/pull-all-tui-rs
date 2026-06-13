// @ts-check
import { defineConfig } from 'astro/config';
import starlight from '@astrojs/starlight';
import remarkGfm from 'remark-gfm';

// Project site served from a subpath on GitHub Pages.
export default defineConfig({
  site: 'https://steven-pribilinskiy.github.io',
  base: '/polygit',
  // Astro applies GFM to `.md` automatically but NOT to `.mdx` tables — add remark-gfm
  // explicitly so the tables in our .mdx guides render (it extends to MDX by default).
  markdown: {
    remarkPlugins: [remarkGfm],
  },
  integrations: [
    starlight({
      title: 'polygit',
      description:
        'Interactive polyrepo git dashboard — a Rust/ratatui TUI that discovers every repo in a directory and pulls them in parallel.',
      // Show each page's git last-modified date in the footer — a visible staleness signal when
      // a page lags behind code churn. Needs full git history (fetch-depth: 0) in the deploy.
      lastUpdated: true,
      social: [
        {
          icon: 'github',
          label: 'GitHub',
          href: 'https://github.com/steven-pribilinskiy/polygit',
        },
      ],
      customCss: ['./src/styles/custom.css'],
      // Replace the native <select> theme switcher with a sun/moon toggle button.
      components: {
        ThemeSelect: './src/components/ThemeToggle.astro',
      },
      sidebar: [
        {
          label: 'Start here',
          items: [
            { label: 'What is polygit?', slug: 'index' },
            { label: 'Installation', slug: 'start/installation' },
            { label: 'Usage', slug: 'start/usage' },
          ],
        },
        {
          label: 'Guides',
          items: [
            { label: 'Keybindings', slug: 'guides/keybindings' },
            { label: 'Repo page & diff modal', slug: 'guides/repo-page' },
            { label: 'Columns & glyphs', slug: 'guides/columns-and-glyphs' },
            { label: 'Repo groups', slug: 'guides/groups' },
            { label: 'Directory tree', slug: 'guides/tree-view' },
          ],
        },
        {
          label: 'Reference',
          items: [
            { label: 'CLI flags & env', slug: 'reference/cli' },
            { label: 'Exit codes', slug: 'reference/exit-codes' },
            { label: 'Sibling builds', slug: 'reference/siblings' },
            { label: 'Architecture', slug: 'reference/architecture' },
          ],
        },
      ],
    }),
  ],
});
