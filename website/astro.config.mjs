import { defineConfig } from 'astro/config';
import starlight from '@astrojs/starlight';

export default defineConfig({
  site: 'https://chmouel.github.io',
  base: '/raffi',
  integrations: [
    starlight({
      title: 'Raffi',
      logo: { src: './src/assets/logo.png' },
      social: [
        { icon: 'github', label: 'GitHub', href: 'https://github.com/chmouel/raffi' },
      ],
      sidebar: [
        {
          label: 'Getting Started',
          items: [
            { label: 'Introduction', slug: '' },
            { label: 'Installation', slug: 'installation' },
            { label: 'Quick Start', slug: 'quickstart' },
          ],
        },
        {
          label: 'Configuration',
          items: [
            { label: 'Overview', slug: 'configuration/overview' },
            { label: 'Launcher Entries', slug: 'configuration/entries' },
            { label: 'General Settings', slug: 'configuration/general-settings' },
            { label: 'Scripts', slug: 'configuration/scripts' },
            { label: 'Conditions', slug: 'configuration/conditions' },
            { label: 'Icons', slug: 'configuration/icons' },
          ],
        },
        {
          label: 'Features',
          items: [
            { label: 'UI Modes', slug: 'features/ui-modes' },
            { label: 'Calculator', slug: 'features/calculator' },
            { label: 'Currency Converter', slug: 'features/currency-converter' },
            { label: 'File Browser', slug: 'features/file-browser' },
            { label: 'Script Filters', slug: 'features/script-filters' },
            { label: 'Text Snippets', slug: 'features/text-snippets' },
            { label: 'Themes', slug: 'features/themes' },
            { label: 'Web Search', slug: 'features/web-search' },
          ],
        },
        {
          label: 'Integration',
          items: [
            { label: 'Sway', slug: 'integration/sway' },
            { label: 'Hyprland', slug: 'integration/hyprland' },
            { label: 'Fuzzel', slug: 'integration/fuzzel' },
          ],
        },
        {
          label: 'Reference',
          items: [
            { label: 'CLI Options', slug: 'reference/cli-options' },
            { label: 'YAML Schema', slug: 'reference/yaml-schema' },
            { label: 'Addon Configuration', slug: 'reference/addon-configuration' },
          ],
        },
      ],
    }),
  ],
});
