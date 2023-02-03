// @ts-check
// Note: type annotations allow type checking and IDEs autocompletion

const lightCodeTheme = require('prism-react-renderer/themes/github');
const darkCodeTheme = require('prism-react-renderer/themes/dracula');

/** @type {import('@docusaurus/types').Config} */
const config = {
  title: 'heph',
  tagline: 'Efficient build system',
  url: 'https://hephbuild.github.io',
  baseUrl: '/heph/',
  onBrokenLinks: 'throw',
  onBrokenMarkdownLinks: 'warn',
  favicon: 'img/favicon.ico',

  organizationName: 'hephbuild',
  projectName: 'heph',

  // Even if you don't use internalization, you can use this field to set useful
  // metadata like html lang. For example, if your site is Chinese, you may want
  // to replace "en" with "zh-Hans".
  i18n: {
    defaultLocale: 'en',
    locales: ['en'],
  },

  presets: [
    [
      'classic',
      /** @type {import('@docusaurus/preset-classic').Options} */
      ({
        docs: {
          sidebarPath: require.resolve('./sidebars.js'),
        },
        theme: {
          customCss: [
            require.resolve('./src/css/custom.css'),
            require.resolve('./src/css/custom.scss')
          ],
        },
      }),
    ],
  ],

  themeConfig:
    /** @type {import('@docusaurus/preset-classic').ThemeConfig} */
    ({
      navbar: {
        title: 'heph',
        logo: {
          alt: 'heph Logo',
          src: 'img/logo.png',
        },
        hideOnScroll: true,
        items: [
          {
            type: 'doc',
            docId: 'getting-started',
            position: 'left',
            label: 'Docs',
          },
          {
            href: 'https://github.com/hephbuild/heph',
            'aria-label': 'GitHub repository',
            position: 'right',
            className: 'navbar-github-link',
          },
        ]
      },
      footer: {
        style: 'dark',
        links: [
          {
            title: 'Docs',
            items: [
              {
                label: 'Docs',
                to: '/docs/getting-started',
              },
            ],
          },
          {
            title: 'More',
            items: [
              {
                label: 'GitHub',
                href: 'https://github.com/hephbuild/heph',
              },
            ],
          },
        ],
        copyright: `Copyright © ${new Date().getFullYear()} heph. Built with Docusaurus.`,
      },
      prism: {
        theme: lightCodeTheme,
        darkTheme: darkCodeTheme,
      },
      algolia: {
        // The application ID provided by Algolia
        appId: 'STE8BMOSCB',

        // Public API key: it is safe to commit it
        apiKey: '84ed2e7e95755632c38773d0374dafcb',

        indexName: 'heph',

        // Optional: see doc section below
        contextualSearch: true,

        // Optional: Algolia search parameters
        searchParameters: {},

        // Optional: path for search page that enabled by default (`false` to disable it)
        searchPagePath: 'search',
      },
    }),
  plugins: [
      // require.resolve("@cmfcmf/docusaurus-search-local"),
      'docusaurus-plugin-sass',
  ],
};

module.exports = config;
