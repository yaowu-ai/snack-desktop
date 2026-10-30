import { expect, test, type Page } from '@playwright/test';
import { readFileSync } from 'node:fs';
import path from 'node:path';

const root = path.resolve(__dirname, '../src-tauri');
const localeScript = readFileSync(path.join(root, 'src/meeting/overlay-locale.js'), 'utf8');

function payload(locale: 'en' | 'zh') {
  const chinese = JSON.parse(readFileSync(path.join(root, 'resources/i18n/zh.json'), 'utf8')).desktop.overlay;
  const desktop = JSON.parse(readFileSync(path.join(root, 'resources/i18n/en.json'), 'utf8')).desktop;
  const english = desktop.overlay;
  const effectiveLocale = locale === 'en' && hasTranslation(desktop) ? 'en' : 'zh';
  const messages = { ...chinese, ...(locale === 'en' ? english : {}) };
  return { locale: effectiveLocale, messages, reminderDescription: messages.reminderDescription.replace('{application}', 'Test Meeting') };
}

function hasTranslation(value: unknown): boolean {
  if (typeof value === 'string') return Boolean(value.trim());
  return Boolean(value && typeof value === 'object' && Object.values(value).some(hasTranslation));
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

async function openNoteDeck(page: Page, locale: 'en' | 'zh') {
  const html = readFileSync(path.join(root, 'src/meeting/note_overlay.html'), 'utf8').replace(
    '<!--SNACK_LOCALE-->', `<script>window.__SNACK_LOCALE__=${JSON.stringify(payload(locale))};</script><script>${localeScript}</script>`,
  );
  await page.setViewportSize({ width: 263, height: 192 });
  await page.emulateMedia({ reducedMotion: 'reduce' });
  await page.addInitScript(() => {
    const host = window as any;
    host.__TAURI__ = {
      core: { invoke: async () => ({
        recordingId: 'rec-b', resultAttempt: 'capture', phase: 'recording', paused: false,
        elapsedMs: 8000, capsuleTop: 13, stackAbove: false, handoffNoticeId: 'locale-handoff',
        previous: { recordingId: 'rec-a', resultAttempt: 'capture', phase: 'transcribing' },
      }) },
      event: { listen: (_name: string, handler: unknown) => { host.noteStateHandler = handler; return Promise.resolve(() => {}); } },
    };
  });
  await page.route('https://overlay.test/**', (route) => {
    const filename = path.basename(new URL(route.request().url()).pathname);
    if (filename === 'note-overlay-deck.js') return route.fulfill({ contentType: 'text/javascript', body: readFileSync(path.join(root, 'src/meeting', filename)) });
    if (filename.endsWith('.svg') || filename.endsWith('.png') || filename.endsWith('.woff2')) {
      return route.fulfill({ contentType: filename.endsWith('.woff2') ? 'font/woff2' : filename.endsWith('.png') ? 'image/png' : 'image/svg+xml', body: readFileSync(path.join(root, 'src/meeting/note-overlay-assets', filename)) });
    }
    return route.fulfill({ contentType: 'text/html', body: html });
  });
  await page.goto('https://overlay.test/note.html');
  await page.evaluate(() => document.fonts.ready);
}

async function expectPreviousRetired(page: Page) {
  for (const selector of ['#previous-card', '#previous-full', '#previous-peek', '#previous-open', '#recording-peek']) {
    const elements = page.locator(selector);
    await expect(elements).toBeHidden();
    expect(await elements.evaluateAll((items) => items.every((item) => Boolean(item.closest('[inert]'))))).toBe(true);
  }
  await expect(page.locator('#handoff-notice')).toBeHidden();
  await expect(page.locator('#current-content')).toHaveJSProperty('inert', false);
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
  await page.getByRole('button', { name: payload('en').messages.stop, exact: true }).click();
  await expect.poll(() => stopped).toBe(true);
  await expect(page.locator('#error')).toBeHidden();
});

for (const locale of ['en', 'zh'] as const) {
  test(`card deck localizes ${locale} and locale refresh does not extend its confirmation`, async ({ page }) => {
    await page.clock.install();
    await page.clock.pauseAt(new Date(Date.now() + 60_000));
    await openNoteDeck(page, locale);
    await page.clock.runFor(81);
    await expect(page.locator('#handoff-title')).toHaveText(payload(locale).messages.backgroundTranscriptionTitle);
    await expect(page.locator('#handoff-description')).toHaveText(payload(locale).messages.backgroundTranscriptionDescription);
    await expect(page.locator('#handoff-notice')).toBeVisible();
    await page.clock.runFor(1000);
    const other = locale === 'en' ? 'zh' : 'en';
    await page.evaluate((next) => (window as any).setSnackLocale(next), payload(other));
    await expect(page.locator('#handoff-title')).toHaveText(payload(other).messages.backgroundTranscriptionTitle);
    await expect(page.locator('#duration')).toHaveText('00:00:08');
    await page.clock.runFor(2201);
    await expectPreviousRetired(page);
    await expect(page.locator('#pause')).toHaveAccessibleName(payload(other).messages.pause);
    await page.evaluate((next) => (window as any).setSnackLocale(next), payload(locale));
    await expectPreviousRetired(page);
    await page.evaluate(() => (window as any).noteStateHandler({ payload: {
      recordingId: 'rec-b', resultAttempt: 'capture', phase: 'recording', elapsedMs: 41000,
      capsuleTop: 13, stackAbove: false,
      previous: { recordingId: 'rec-a', resultAttempt: 'capture', phase: 'failed' },
    } }));
    await page.clock.runFor(1000);
    await expectPreviousRetired(page);
    await expect(page.locator('#duration')).toHaveText('00:00:41');
  });
}
