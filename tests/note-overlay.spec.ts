import { expect, test, type Page } from '@playwright/test';
import { readFileSync } from 'node:fs';
import path from 'node:path';

const root = path.resolve(__dirname, '../src-tauri');
const surface = readFileSync(path.join(root, 'src/meeting/note_overlay.html'), 'utf8');
const localeScript = readFileSync(path.join(root, 'src/meeting/overlay-locale.js'), 'utf8');
const messages = JSON.parse(readFileSync(path.join(root, 'resources/i18n/zh.json'), 'utf8')).desktop.overlay;

async function openSurface(page: Page, phase = 'recording', paused = false, failControl = false, stallControl = false) {
  await page.setViewportSize({ width: 360, height: 200 });
  await page.addInitScript(({ phase, paused, failControl, stallControl }) => {
    const host = window as any;
    host.noteCalls = [];
    host.__TAURI__ = {
      core: { invoke: async (command: string, args: unknown) => {
        host.noteCalls.push({ command, args });
        if (command === 'meeting_get_note_overlay_state') return { recordingId: 'rec-a', resultAttempt: 'capture', phase, paused, elapsedMs: 743000, canDismiss: phase !== 'recording' };
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
  expect(box).toEqual({ x: 81, y: 100, width: 197, height: 72 });
  for (const phase of ['recording', 'transcribing', 'completed', 'failed', 'ended', 'recording_failed', 'transcription_paused']) {
    await page.evaluate((phase) => (window as any).noteStateHandler({ payload: { recordingId: 'rec-a', resultAttempt: 'capture', phase, paused: phase === 'recording', elapsedMs: 743000, canDismiss: phase !== 'recording' } }), phase);
    await expect(page.locator('#capsule')).toHaveAttribute('data-phase', phase);
    if (phase === 'recording') await expect(page.locator('#dismiss')).toBeHidden();
    else {
      await expect(page.locator('#dismiss')).toBeVisible();
      await expect(page.locator('#dismiss')).toHaveAccessibleName(messages.dismiss);
      expect(await page.locator('#dismiss').boundingBox()).toEqual({ x: 274, y: 91, width: 24, height: 24 });
    }
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
      expect(await page.locator('#result-open').boundingBox()).toEqual({ x: 246, y: 120, width: 20, height: 32 });
    }
    if (phase === 'failed') await expect(page.locator('#result-action')).toHaveText(messages.retryTranscription);
    const identity = await page.locator('.identity').boundingBox();
    expect(identity).toEqual(phase === 'recording'
      ? { x: 93, y: 115, width: 73, height: 41 }
      : { x: 97, y: 115, width: 78, height: 42 });
    if (phase === 'completed' || phase === 'failed')
      expect(await page.locator('#result-action').boundingBox()).toEqual({ x: 200, y: 119, width: 66, height: 34 });
    if (phase === 'transcribing') {
      const status = await page.locator('#status-line').boundingBox();
      expect(status).toMatchObject({ x: 97, y: 137, height: 20 });
      expect(Math.abs(status!.width - 102)).toBeLessThan(0.5);
    }
    await page.screenshot({ path: `test-results/note-overlay-${phase}.png` });
  }
});

test('every summary stage hides the capsule and its actions', async ({ page }) => {
  await openSurface(page);
  for (const phase of ['summary_generating', 'summary_completed', 'summary_failed']) {
    await page.evaluate((phase) => (window as any).noteStateHandler({ payload: { recordingId: 'rec-a', phase } }), phase);
    await expect(page.locator('#capsule')).toBeHidden();
    await expect(page.locator('#dismiss')).toBeHidden();
    await expect(page.locator('#handoff-notice')).toBeHidden();
    await expect(page.getByRole('button')).toHaveCount(0);
  }
  const calls = await page.evaluate(() => (window as any).noteCalls.filter((call: { command: string }) => call.command === 'meeting_note_overlay_action'));
  expect(calls).toEqual([]);
});

test('handoff notice lasts three seconds without restarting on native ticks', async ({ page }) => {
  await page.clock.install();
  await openSurface(page);
  const state = { recordingId: 'rec-b', resultAttempt: 'capture', phase: 'recording', handoffNoticeId: 'a-to-b', elapsedMs: 3000 };
  await page.evaluate((payload) => (window as any).noteStateHandler({ payload }), state);
  await expect(page.locator('#handoff-notice')).toHaveText(messages.backgroundTranscription);
  await expect(page.locator('#handoff-notice')).toBeVisible();
  const notice = await page.locator('#handoff-notice').boundingBox();
  const capsule = await page.locator('#capsule').boundingBox();
  expect(notice!.x).toBe(24);
  expect(notice!.width).toBe(312);
  expect(notice!.y + notice!.height + 16).toBeLessThanOrEqual(capsule!.y);
  await page.clock.runFor(2000);
  await page.evaluate((payload) => (window as any).noteStateHandler({ payload }), state);
  await page.clock.runFor(1001);
  await expect(page.locator('#handoff-notice')).toBeHidden();
  await page.evaluate((payload) => (window as any).noteStateHandler({ payload }), state);
  await expect(page.locator('#handoff-notice')).toBeHidden();
  await expect(page.locator('#duration')).toHaveText('00:00:03');
});

test('changing the binding clears the previous recording handoff notice', async ({ page }) => {
  await openSurface(page);
  await page.evaluate(() => (window as any).noteStateHandler({ payload: {
    recordingId: 'rec-b', phase: 'recording', handoffNoticeId: 'a-to-b',
  } }));
  await expect(page.locator('#handoff-notice')).toBeVisible();
  await page.evaluate(() => (window as any).noteStateHandler({ payload: { recordingId: 'rec-c', phase: 'recording' } }));
  await expect(page.locator('#handoff-notice')).toBeHidden();
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
  expect(calls).toContainEqual({ command: 'meeting_note_overlay_action', args: { expectedRecordingId: 'rec-a', expectedResultAttempt: 'capture', action: 'open' } });
  expect(calls).toContainEqual({ command: 'meeting_note_overlay_action', args: { expectedRecordingId: 'rec-a', expectedResultAttempt: 'capture', action: 'retry' } });
  for (const phase of ['completed', 'recording_failed', 'transcription_paused', 'ended'])
    expect(calls).toContainEqual({ command: 'meeting_note_overlay_action', args: { expectedRecordingId: `rec-${phase}`, expectedResultAttempt: 'capture', action: 'open' } });
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

test('dismiss only closes the reminder and carries both recording and result identity', async ({ page }) => {
  await openSurface(page, 'transcribing');
  await page.locator('#dismiss').click();
  const calls = await page.evaluate(() => (window as any).noteCalls);
  expect(calls).toContainEqual({ command: 'meeting_note_overlay_action', args: {
    expectedRecordingId: 'rec-a', expectedResultAttempt: 'capture', expectedPhase: 'transcribing', action: 'dismiss',
  } });
  expect(calls.some((call: { command: string }) => call.command === 'meeting_note_overlay_control')).toBe(false);
  await page.evaluate(() => (window as any).noteStateHandler({ payload: {
    recordingId: 'rec-b', resultAttempt: 'capture', phase: 'recording', paused: true, canDismiss: false,
  } }));
  await expect(page.locator('#dismiss')).toBeHidden();
});

test('a dismiss failure keeps the reminder actionable and reports the error', async ({ page }) => {
  await openSurface(page, 'failed');
  await page.evaluate(() => {
    const host = window as any;
    const original = host.__TAURI__.core.invoke;
    host.__TAURI__.core.invoke = (command: string, args: { action?: string }) =>
      args?.action === 'dismiss' ? Promise.reject(new Error('disk unavailable')) : original(command, args);
  });
  await page.locator('#dismiss').click();
  await expect(page.locator('#dismiss')).toBeVisible();
  await expect(page.locator('#dismiss')).toBeEnabled();
  await expect(page.locator('#result-action')).toBeEnabled();
  await expect(page.locator('#live-status')).toHaveText(messages.dismissFailed);
});

test('a stale dismiss response cannot affect a later attempt of the same recording', async ({ page }) => {
  await openSurface(page, 'failed');
  await page.evaluate(() => {
    const host = window as any;
    const original = host.__TAURI__.core.invoke;
    host.__TAURI__.core.invoke = (command: string, args: { action?: string }) => {
      if (args?.action === 'dismiss') return new Promise((_resolve, reject) => { host.rejectOldDismiss = reject; });
      return original(command, args);
    };
  });
  await page.locator('#dismiss').click();
  await expect(page.locator('#dismiss')).toBeDisabled();
  await page.evaluate(() => {
    const host = window as any;
    host.noteStateHandler({ payload: { recordingId: 'rec-a', resultAttempt: 'retry-2', phase: 'failed', paused: false, canDismiss: true } });
    host.rejectOldDismiss(new Error('old attempt rejected'));
  });
  await expect(page.locator('#dismiss')).toBeEnabled();
  await expect(page.locator('#capsule')).toHaveAttribute('title', '');
  await expect(page.locator('#live-status')).toHaveText(messages.transcriptionFailed);
});

test('a processing dismiss rejected after completion preserves the final result', async ({ page }) => {
  await openSurface(page, 'transcribing');
  await page.evaluate(() => {
    const host = window as any;
    host.__TAURI__.core.invoke = (command: string, args: { action?: string; expectedPhase?: string }) => {
      host.noteCalls.push({ command, args });
      host.noteStateHandler({ payload: {
        recordingId: 'rec-a', resultAttempt: 'capture', phase: 'completed', paused: false, canDismiss: true,
      } });
      return Promise.reject(new Error('phase changed before closing'));
    };
  });
  await page.locator('#dismiss').click();
  await expect(page.locator('#status-label')).toHaveText(messages.transcriptionCompleted);
  await expect(page.locator('#result-action')).toBeVisible();
  await expect(page.locator('#result-action')).toBeEnabled();
  const calls = await page.evaluate(() => (window as any).noteCalls);
  expect(calls).toContainEqual({ command: 'meeting_note_overlay_action', args: {
    expectedRecordingId: 'rec-a', expectedResultAttempt: 'capture', expectedPhase: 'transcribing', action: 'dismiss',
  } });
});
