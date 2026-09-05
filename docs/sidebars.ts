import type {SidebarsConfig} from '@docusaurus/plugin-content-docs';

// This runs in Node.js - Don't use client-side code here (browser APIs, JSX...)

/**
 * Creating a sidebar enables you to:
 - create an ordered group of docs
 - render a sidebar for each doc of that group
 - provide next/previous navigation

 The sidebars can be generated from the filesystem, or explicitly defined here.

 Create as many sidebars as you want.
 */
const sidebars: SidebarsConfig = {
  tutorialSidebar: [
    'get-started',
    'concepts/editing-model',
    'concepts/architecture',
    {
      type: 'category',
      label: 'MMFX',
      link: {
        type: 'doc',
        id: 'concepts/mmfx',
      },
      items: ['mmfx/scene-language', 'mmfx/examples'],
    },
    'project-status',
    'roadmap',
  ],
};

export default sidebars;
