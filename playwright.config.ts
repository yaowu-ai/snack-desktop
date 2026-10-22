import { defineConfig } from '@playwright/test';

export default defineConfig({
  testDir: './tests',
  outputDir: './test-results',
  use: { browserName: 'webkit', viewport: { width: 240, height: 276 } },
  workers: 2,
});
