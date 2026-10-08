import { expect, test, type Page } from '@playwright/test';
import { readFileSync } from 'node:fs';
import path from 'node:path';

const root = path.resolve(__dirname, '../src-tauri');
const localeScript = readFileSync(path.join(root, 'src/meeting/overlay-locale.js'), 'utf8');

function payload(locale: 'en' | 'zh') {
  const messages = JSON.parse(readFileSync(path.join(root, `resources/i18n/${locale}.json`), 'utf8')).desktop.overlay;
  return { locale, messages, reminderDescription: messages.reminderDescription.replace('{application}', 'Test Meeting') };
}

async function openSurface(page: Page, name: string, locale: 'en' | 'zh') {
  const html = readFileSync(path.join(root, `src/meeting/${name}.html`), 'utf8').replace(
    '<!--SNACK_LOCALE-->', `<script>window.__SNACK_LOCALE__=${JSON.stringify(payload(locale))};</script><script>${localeScript}</script>`,
  );
  await page.addInitScript(() => {
    const host = window as any;
    host.__TAURI__ = {
      core: { invoke: async (command: string) => {
        if (command === 'meeting_get_recording_overlay_state') return {
          recordingId: 'rec-test', elapsedMs: 95000, paused: true, displayName: 'My draft',
          transcriptProjectId: '42', transcriptProjectName: 'Project 42', projects: [],
        };
        if (command === 'set_overlay_expanded') return;
        throw new Error('Native action failed (test)');
      } },
      event: { listen: () => Promise.resolve(() => {}) },
    };
  });
  await page.route('https://overlay.test/**', (route) => route.fulfill({ contentType: 'text/html', body: html }));
  await page.goto('https://overlay.test/');
}

for (const locale of ['en', 'zh'] as const) {
  test(`recording card renders ${locale} offline and retains drafts on switch`, async ({ page }) => {
    const errors: string[] = [];
    page.on('pageerror', (error) => errors.push(error.message));
    await openSurface(page, 'overlay', locale);
    const messages = payload(locale).messages;
    await expect(page.locator('#title')).toHaveText(messages.paused);
    await expect(page.locator('#metric')).toHaveText('01:35');
    await page.getByRole('button', { name: messages.expand }).click();
    await expect(page.locator('#project-select')).toHaveValue('42');
    await page.locator('#filename-input').click();
    await page.locator('#filename-input').fill('Uncommitted edit');
    const other = locale === 'en' ? 'zh' : 'en';
    await page.evaluate((next) => (window as any).setSnackLocale(next), payload(other));
    await expect(page.locator('#filename-input')).toHaveValue('Uncommitted edit');
    await expect(page.locator('#title')).toHaveText(payload(other).messages.paused);
    await expect(page.locator('#metric')).toHaveText('01:35');
    await expect(page.locator('#project-select')).toHaveValue('42');
    await page.screenshot({ path: `test-results/recording-${other}.png` });
    expect(await page.locator('.setting-row').evaluateAll((rows) => rows.every((row) => row.scrollWidth <= row.clientWidth))).toBe(true);
    expect(errors).toEqual([]);
  });

  test(`reminder renders ${locale} and retranslates visible errors`, async ({ page }) => {
    await page.setViewportSize({ width: 326, height: 116 });
    await openSurface(page, 'reminder', locale);
    await expect(page.locator('#description')).toHaveText(payload(locale).reminderDescription);
    await page.locator('#start').click();
    await expect(page.locator('#error')).toHaveText(payload(locale).messages.reminderFailed);
    const other = locale === 'en' ? 'zh' : 'en';
    await page.evaluate((next) => (window as any).setSnackLocale(next), payload(other));
    await expect(page.locator('#start')).toHaveText(payload(other).messages.reminderRetry);
    await expect(page.locator('#error')).toHaveText(payload(other).messages.reminderFailed);
    await expect(page.locator('#description')).toHaveText(payload(other).reminderDescription);
    await page.screenshot({ path: `test-results/reminder-${other}.png` });
  });
}

test('native protocol can stop recording when IPC fails', async ({ page }) => {
  await openSurface(page, 'overlay', 'en');
  let stopped = false;
  await page.route('https://overlay.test/stop', (route) => {
    stopped = route.request().method() === 'POST';
    return route.fulfill({ status: 204 });
  });
  await page.getByRole('button', { name: 'Stop recording', exact: true }).click();
  await expect.poll(() => stopped).toBe(true);
  await expect(page.locator('#error')).toBeHidden();
});
