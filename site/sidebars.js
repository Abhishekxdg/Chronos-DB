module.exports = {
  docs: [
    'README',
    'quickstart',
    'migrating-from-postgres',
    'concepts',
    { type: 'category', label: 'Guides', items: ['guides/agent-sandbox', 'guides/agent-evaluation', 'guides/scenario-planning', 'guides/merge-policies', 'guides/merge-checks'] },
    { type: 'category', label: 'Reference', items: [
      'sql', 'reference/worlds', 'search', 'http-api', 'reference/mcp', 'reference/clients', 'reference/cli', 'reference/errors',
    ] },
    'postgres-compatibility',
    'security',
    'operations',
  ],
};
