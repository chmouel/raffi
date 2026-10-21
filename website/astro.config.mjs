import { defineConfig } from 'astro/config';
import starlight from '@astrojs/starlight';

export default defineConfig({
  site: 'https://chmouel.github.io',
  base: '/raffi',
  redirects: {
    '/features/ui-modes': '/raffi/reference/keyboard-shortcuts/',
  },
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
            { label: 'General Settings', slug: 'configuration/general-settings' },
            { label: 'Custom Entries', slug: 'configuration/entries' },
            { label: 'Scripts', slug: 'configuration/scripts' },
            { label: 'Conditions', slug: 'configuration/conditions' },
            { label: 'Icons', slug: 'configuration/icons' },
          ],
        },
        {
          label: 'Features',
          items: [
            { label: 'Audio Switcher', slug: 'features/audio-switcher' },
            { label: 'Bitwarden', slug: 'features/bitwarden' },
            { label: 'Bluetooth', slug: 'features/bluetooth' },
            { label: 'Browser Tabs', slug: 'features/browser-tabs' },
            { label: 'Calculator', slug: 'features/calculator' },
            { label: 'Clipboard History', slug: 'features/clipboard-history' },
            { label: 'Color Converter', slug: 'features/color-converter' },
            { label: 'Currency Converter', slug: 'features/currency-converter' },
            { label: 'Developer Tools', slug: 'features/developer-tools' },
            { label: 'File Browser', slug: 'features/file-browser' },
            { label: 'Installed Apps', slug: 'features/installed-apps' },
            { label: 'Kill Process', slug: 'features/kill-process' },
            { label: 'Media Controls', slug: 'features/media-controls' },
            { label: 'Password Store', slug: 'features/password-store' },
            { label: 'Run in Terminal', slug: 'features/terminal' },
            { label: 'Script Filters', slug: 'features/script-filters' },
            { label: 'Slack', slug: 'features/slack' },
            { label: 'System Commands', slug: 'features/system-commands' },
            { label: 'Text Snippets', slug: 'features/text-snippets' },
            { label: 'Themes', slug: 'features/themes' },
            { label: 'Unit Conversion', slug: 'features/unit-conversion' },
            { label: 'Web Search', slug: 'features/web-search' },
            { label: 'Window Switcher', slug: 'features/window-switcher' },
            { label: 'World Clock', slug: 'features/world-clock' },
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
            { label: 'Keyboard Shortcuts', slug: 'reference/keyboard-shortcuts' },
          ],
        },
      ],
    }),
  ],
});
