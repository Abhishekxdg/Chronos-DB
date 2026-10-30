// Docs site over ../docs. The pages stay plain Markdown so GitHub renders them too.
module.exports = {
  title: 'Chronos DB',
  tagline: 'Fork the world, try everything, merge what works.',
  // GitHub Pages (.github/workflows/docs.yml); a custom domain later changes these two lines
  url: 'https://abhishekxdg.github.io',
  baseUrl: '/Chronos-DB/',
  onBrokenLinks: 'throw',
  markdown: { format: 'detect', hooks: { onBrokenMarkdownLinks: 'warn' } },
  presets: [
    ['classic', {
      docs: {
        path: '../docs',
        exclude: ['discovery/**', 'designs/**'], // internal product and design notes, not developer docs
        routeBasePath: '/',
        sidebarPath: require.resolve('./sidebars.js'),
      },
      blog: false,
      theme: { customCss: require.resolve('./src/css/custom.css') },
    }],
  ],
  // search built into the site at build time: no outside service
  themes: [[require.resolve('@easyops-cn/docusaurus-search-local'), { hashed: true, docsRouteBasePath: '/', indexBlog: false, highlightSearchTermsOnTargetPage: true }]],
  themeConfig: {
    colorMode: { respectPrefersColorScheme: true },
    navbar: {
      title: 'Chronos DB',
      items: [
        { to: '/quickstart', label: 'Quickstart', position: 'left' },
        { to: '/guides/agent-sandbox', label: 'Guides', position: 'left' },
        { to: '/sql', label: 'Reference', position: 'left' },
        { href: 'https://github.com/Abhishekxdg/Chronos-DB', label: 'GitHub', position: 'right' },
      ],
    },
    prism: { additionalLanguages: ['rust', 'bash', 'sql', 'toml', 'json'] },
  },
};
