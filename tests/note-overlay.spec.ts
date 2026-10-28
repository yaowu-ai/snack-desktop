import { expect, test, type Page } from '@playwright/test';
import { readFileSync } from 'node:fs';
import path from 'node:path';

const root = path.resolve(__dirname, '../src-tauri');
const surface = readFileSync(path.join(root, 'src/meeting/note_overlay.html'), 'utf8');
const localeScript = readFileSync(path.join(root, 'src/meeting/overlay-locale.js'), 'utf8');
const messages = JSON.parse(readFileSync(path.join(root, 'resources/i18n/zh.json'), 'utf8')).desktop.overlay;

async function openSurface(page: Page, phase = 'recording', paused = false, failControl = false, stallControl = false) {
  await page.setViewportSize({ width: 263, height: 148 });
  await page.addInitScript(({ phase, paused, failControl, stallControl }) => {
    const host = window as any;
    host.noteCalls = [];
    host.__TAURI__ = {
      core: { invoke: async (command: string, args: unknown) => {
        host.noteCalls.push({ command, args });
        if (command === 'meeting_get_note_overlay_state') return { recordingId: 'rec-a', phase, paused, elapsedMs: 743000 };
        if (failControl && command === 'meeting_note_overlay_control') throw new Error('IPC unavailable');
        if (stallControl && command === 'meeting_note_overlay_control') return new Promise(() => {});
      } },
      event: { listen: (_name: string, handler: unknown) => { host.noteStateHandler = handler; return Promise.resolve(() => {}); } },
    };
  }, { phase, paused, failControl, stallControl });
  await page.route('https://note-overlay.test/**', (route) => {
    const url = new URL(route.request().url());
    if (url.pathname.startsWith('/note-assets/')) {
      const filename = path.basename(url.pathname);
      return route.fulfill({ contentType: filename.endsWith('.woff2') ? 'font/woff2' : filename.endsWith('.png') ? 'image/png' : 'image/svg+xml',
        body: readFileSync(path.join(root, 'src/meeting/note-overlay-assets', filename)) });
    }
    return route.fulfill({ contentType: 'text/html', body: surface.replace('<!--SNACK_LOCALE-->',
      `<script>window.__SNACK_LOCALE__=${JSON.stringify({ locale: 'zh', messages })};</script><script>${localeScript}</script>`) });
  });
  await page.goto('https://note-overlay.test/note.html');
  await page.evaluate(() => document.fonts.ready);
  await expect(page.locator('#capsule')).toHaveAttribute('data-phase', phase);
}

test('Figma capsule retains its geometry and local asset slots across every state', async ({ page }) => {
  await openSurface(page);
  await expect(page.locator('#duration')).toHaveText('00:12:23');
  const box = await page.locator('#capsule').boundingBox();
  expect(box).toEqual({ x: 33, y: 13, width: 197, height: 72 });
  for (const phase of ['recording', 'transcribing', 'completed', 'failed', 'ended', 'recording_failed', 'transcription_paused',
    'summary_generating', 'summary_completed', 'summary_failed']) {
    await page.evaluate((phase) => (window as any).noteStateHandler({ payload: { recordingId: 'rec-a', phase, paused: phase === 'recording', elapsedMs: 743000 } }), phase);
    await expect(page.locator('#capsule')).toHaveAttribute('data-phase', phase);
    expect(await page.locator('img:visible').evaluateAll((images) => images.every((image) => (image as HTMLImageElement).complete && (image as HTMLImageElement).naturalWidth > 0))).toBe(true);
    if (phase === 'completed') {
      await expect(page.locator('#status-label')).toHaveText('转写完成');
      await expect(page.locator('#result-action')).toHaveText(messages.viewNote);
      await expect(page.locator('#result-action')).toBeVisible();
      await expect(page.locator('#result-open')).toBeHidden();
    }
    if (['ended', 'recording_failed', 'transcription_paused'].includes(phase)) {
      await expect(page.locator('#result-action')).toBeHidden();
      await expect(page.locator('#result-open')).toBeVisible();
      await expect(page.locator('#result-open')).toHaveAccessibleName(messages.viewNote);
      await expect(page.locator('#result-open img')).toHaveAttribute('src', '/note-assets/open-note-processing.svg');
      expect(await page.locator('#result-open').boundingBox()).toEqual({ x: 198, y: 33, width: 20, height: 32 });
    }
    if (phase === 'failed') await expect(page.locator('#result-action')).toHaveText(messages.retryTranscription);
    if (phase === 'summary_generating') {
      await expect(page.locator('#result-action')).toBeHidden();
      await expect(page.locator('#result-open')).toBeVisible();
      await expect(page.locator('#result-open')).toHaveAccessibleName(messages.viewSummary);
      expect(await page.locator('#result-open').boundingBox()).toEqual({ x: 198, y: 33, width: 20, height: 32 });
    }
    if (phase === 'summary_completed') await expect(page.locator('#result-action')).toHaveText('查看纪要');
    if (phase === 'summary_failed') await expect(page.locator('#result-action')).toHaveText('重新生成');
    const identity = await page.locator('.identity').boundingBox();
    expect(identity).toEqual(phase === 'recording'
      ? { x: 45, y: 28, width: 73, height: 41 }
      : { x: 49, y: 28, width: 78, height: 42 });
    if (phase === 'completed' || phase === 'failed')
      expect(await page.locator('#result-action').boundingBox()).toEqual({ x: 152, y: 32, width: 66, height: 34 });
    if (phase === 'transcribing') {
      const status = await page.locator('#status-line').boundingBox();
      expect(status).toMatchObject({ x: 49, y: 50, height: 20 });
      expect(Math.abs(status!.width - 102)).toBeLessThan(0.5);
    }
    if (phase.startsWith('summary_')) {
      const status = await page.locator('#status-line').boundingBox();
      const button = await page.locator(phase === 'summary_generating' ? '#result-open' : '#result-action').boundingBox();
      if (phase !== 'summary_generating') expect(button).toEqual({ x: 152, y: 32, width: 66, height: 34 });
      expect(status!.x + status!.width + 8).toBeLessThanOrEqual(button!.x);
      expect(await page.locator('#status-icon').boundingBox()).toMatchObject({ width: 16, height: 16 });
      if (phase !== 'summary_generating') await expect(page.locator('#result-open')).toBeHidden();
    }
    await page.screenshot({ path: `test-results/note-overlay-${phase}.png` });
  }
});

test('summary actions preserve note identity and distinguish retry from viewing', async ({ page }) => {
  await openSurface(page, 'summary_generating');
  await expect(page.locator('#status-label')).toHaveText(messages.summaryGenerating);
  await expect(page.locator('#status-icon')).toHaveClass('loading');
  await expect(page.getByRole('button', { name: '前往查看', exact: true })).toHaveCount(0);
  await page.getByRole('button', { name: messages.viewSummary, exact: true }).click();
  await page.evaluate(() => (window as any).noteStateHandler({ payload: { recordingId: 'rec-a', phase: 'summary_completed' } }));
  await expect(page.locator('#status-label')).toHaveText(messages.summaryCompleted);
  await expect(page.locator('#status-icon')).not.toHaveClass('loading');
  await page.getByRole('button', { name: '查看纪要', exact: true }).click();
  await page.evaluate(() => (window as any).noteStateHandler({ payload: { recordingId: 'rec-a', phase: 'summary_failed' } }));
  await expect(page.locator('#status-label')).toHaveText(messages.summaryFailed);
  await page.getByRole('button', { name: '重新生成', exact: true }).click();
  const calls = await page.evaluate(() => (window as any).noteCalls.filter((call: { command: string }) => call.command === 'meeting_note_overlay_action'));
  expect(calls.map((call: { args: unknown }) => call.args)).toEqual([
    { expectedRecordingId: 'rec-a', action: 'open' },
    { expectedRecordingId: 'rec-a', action: 'open' },
    { expectedRecordingId: 'rec-a', action: 'retry_summary' },
  ]);
});

test('controls carry the rendered recording identity and retry delegates to Web', async ({ page }) => {
  await openSurface(page);
  await page.locator('#pause').click();
  await page.locator('#open').click();
  await page.evaluate(() => (window as any).noteStateHandler({ payload: { recordingId: 'rec-a', phase: 'failed', paused: false } }));
  await page.locator('#result-action').click();
  for (const phase of ['completed', 'recording_failed', 'transcription_paused', 'ended']) {
    await page.evaluate((phase) => (window as any).noteStateHandler({ payload: { recordingId: `rec-${phase}`, phase } }), phase);
    if (phase === 'completed') await expect(page.locator('#result-action')).toBeVisible();
    else await expect(page.locator('#result-action')).toBeHidden();
    await page.getByRole('button', { name: messages.viewNote, exact: true }).click();
  }
  const calls = await page.evaluate(() => (window as any).noteCalls);
  expect(calls).toContainEqual({ command: 'meeting_note_overlay_control', args: { expectedRecordingId: 'rec-a', paused: true } });
  expect(calls).toContainEqual({ command: 'meeting_note_overlay_action', args: { expectedRecordingId: 'rec-a', action: 'open' } });
  expect(calls).toContainEqual({ command: 'meeting_note_overlay_action', args: { expectedRecordingId: 'rec-a', action: 'retry' } });
  for (const phase of ['completed', 'recording_failed', 'transcription_paused', 'ended'])
    expect(calls).toContainEqual({ command: 'meeting_note_overlay_action', args: { expectedRecordingId: `rec-${phase}`, action: 'open' } });
  expect(calls.some((call: { command: string }) => call.command === 'meeting_retranscribe')).toBe(false);
});

test('offline stop fallback includes the rendered recording identity', async ({ page }) => {
  await openSurface(page, 'recording', false, true);
  let stoppedRecording: string | null = null;
  await page.route('https://note-overlay.test/stop', (route) => {
    expect(route.request().method()).toBe('POST');
    stoppedRecording = route.request().postData();
    return route.fulfill({ status: 204 });
  });
  await page.locator('#stop').click();
  await expect.poll(() => stoppedRecording).toBe('rec-a');
  await expect(page.locator('#stop')).toBeEnabled();
});

test('a delayed stop fallback cannot target a newly rendered recording', async ({ page }) => {
  await page.clock.install();
  await openSurface(page, 'recording', false, false, true);
  let stoppedRecording: string | null = null;
  await page.route('https://note-overlay.test/stop', (route) => {
    stoppedRecording = route.request().postData();
    return route.fulfill({ status: 409, body: 'recording changed' });
  });
  await page.locator('#stop').click();
  await page.evaluate(() => (window as any).noteStateHandler({ payload: { recordingId: 'rec-b', phase: 'recording', paused: false, elapsedMs: 3000 } }));
  await page.clock.runFor(4001);
  await expect.poll(() => stoppedRecording).toBe('rec-a');
  await expect(page.locator('#duration')).toHaveText('00:00:03');
  await expect(page.locator('#capsule')).toHaveAttribute('title', '');
  await expect(page.locator('#stop')).toBeEnabled();
});
