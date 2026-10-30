import { expect, test, type Page } from '@playwright/test';
import { readFileSync } from 'node:fs';
import path from 'node:path';

const root = path.resolve(__dirname, '../src-tauri');
const surface = readFileSync(path.join(root, 'src/meeting/note_overlay.html'), 'utf8');
const localeScript = readFileSync(path.join(root, 'src/meeting/overlay-locale.js'), 'utf8');
const messages = JSON.parse(readFileSync(path.join(root, 'resources/i18n/zh.json'), 'utf8')).desktop.overlay;

interface SurfaceOptions {
  motion?: 'reduce' | 'no-preference';
  deferBootstrap?: boolean;
}

async function openSurface(page: Page, phase = 'recording', paused = false, failControl = false, stallControl = false, options: SurfaceOptions = {}) {
  await page.setViewportSize({ width: 263, height: 192 });
  await page.emulateMedia({ reducedMotion: options.motion ?? 'reduce' });
  await page.addInitScript(({ phase, paused, failControl, stallControl, deferBootstrap }) => {
    const host = window as any;
    host.noteCalls = [];
    host.__TAURI__ = {
      core: { invoke: async (command: string, args: unknown) => {
        host.noteCalls.push({ command, args });
        if (command === 'meeting_get_note_overlay_state') {
          if (deferBootstrap) return new Promise((resolve) => { host.resolveNoteBootstrap = resolve; });
          return { recordingId: 'rec-a', resultAttempt: 'capture', phase, paused, elapsedMs: 743000, canDismiss: phase !== 'recording', capsuleTop: 13, stackAbove: false };
        }
        if (failControl && command === 'meeting_note_overlay_control') throw new Error('IPC unavailable');
        if (stallControl && command === 'meeting_note_overlay_control') return new Promise(() => {});
      } },
      event: { listen: (_name: string, handler: unknown) => { host.noteStateHandler = handler; return Promise.resolve(() => {}); } },
    };
  }, { phase, paused, failControl, stallControl, deferBootstrap: options.deferBootstrap ?? false });
  await routeSurface(page, surface);
  await page.goto('https://note-overlay.test/note.html');
  await page.evaluate(() => document.fonts.ready);
  await expect(page.locator('#capsule')).toHaveAttribute('data-phase', phase);
}

async function routeSurface(page: Page, html: string) {
  await page.route('https://note-overlay.test/**', (route) => {
    const url = new URL(route.request().url());
    if (url.pathname === '/note-assets/note-overlay-deck.js') {
      return route.fulfill({ contentType: 'text/javascript', body: readFileSync(path.join(root, 'src/meeting/note-overlay-deck.js')) });
    }
    if (url.pathname.startsWith('/note-assets/')) {
      const filename = path.basename(url.pathname);
      return route.fulfill({ contentType: filename.endsWith('.woff2') ? 'font/woff2' : filename.endsWith('.png') ? 'image/png' : 'image/svg+xml',
        body: readFileSync(path.join(root, 'src/meeting/note-overlay-assets', filename)) });
    }
    return route.fulfill({ contentType: 'text/html', body: html.replace('<!--SNACK_LOCALE-->',
      `<script>window.__SNACK_LOCALE__=${JSON.stringify({ locale: 'zh', messages })};</script><script>${localeScript}</script>`) });
  });
}

function handoffState(overrides: Record<string, unknown> = {}) {
  return {
    recordingId: 'rec-b', resultAttempt: 'capture', phase: 'recording', elapsedMs: 3000,
    paused: false, canDismiss: false, capsuleTop: 13, stackAbove: false, handoffNoticeId: 'a-to-b',
    previous: { recordingId: 'rec-a', resultAttempt: 'capture', phase: 'transcribing' }, ...overrides,
  };
}

async function emitState(page: Page, state: Record<string, unknown>) {
  await page.evaluate((payload) => (window as any).noteStateHandler({ payload }), state);
}

async function expectPreviousRetired(page: Page) {
  for (const selector of ['#previous-card', '#previous-full', '#previous-peek', '#previous-open', '#recording-peek']) {
    const elements = page.locator(selector);
    await expect(elements).toBeHidden();
    expect(await elements.evaluateAll((items) => items.every((item) => Boolean(item.closest('[inert]'))))).toBe(true);
  }
  await expect(page.locator('#handoff-notice')).toBeHidden();
  await expect(page.locator('#current-content')).toHaveJSProperty('inert', false);
  await expect(page.locator('#deck')).toHaveAttribute('data-deck-view', 'recording');
}

test('Figma capsule retains its geometry and local asset slots across every state', async ({ page }) => {
  await openSurface(page);
  await expect(page.locator('#duration')).toHaveText('00:12:23');
  const box = await page.locator('#capsule').boundingBox();
  expect(box).toEqual({ x: 33, y: 13, width: 197, height: 72 });
  for (const phase of ['recording', 'transcribing', 'completed', 'failed', 'ended', 'recording_failed', 'transcription_paused']) {
    await page.evaluate((phase) => (window as any).noteStateHandler({ payload: { recordingId: 'rec-a', resultAttempt: 'capture', phase, paused: phase === 'recording', elapsedMs: 743000, canDismiss: phase !== 'recording' } }), phase);
    await expect(page.locator('#capsule')).toHaveAttribute('data-phase', phase);
    if (phase === 'recording') await expect(page.locator('#dismiss')).toBeHidden();
    else {
      await expect(page.locator('#dismiss')).toBeVisible();
      await expect(page.locator('#dismiss')).toHaveAccessibleName(messages.dismiss);
      expect(await page.locator('#dismiss').boundingBox()).toEqual({ x: 226, y: 4, width: 24, height: 24 });
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
      expect(await page.locator('#result-open').boundingBox()).toEqual({ x: 198, y: 33, width: 20, height: 32 });
    }
    if (phase === 'failed') await expect(page.locator('#result-action')).toHaveText(messages.retryTranscription);
    const identity = await page.locator('.identity').boundingBox();
    expect(identity).toEqual(phase === 'recording'
      ? { x: 45, y: 28, width: 73, height: 41 }
      : { x: 49, y: 28, width: 78, height: 42 });
    if (phase === 'completed' || phase === 'failed') {
      const action = await page.locator('#result-action').boundingBox();
      expect(action).toMatchObject({ x: 152, y: 32, height: 34 });
      expect(Math.abs(action!.width - 66)).toBeLessThanOrEqual(0.1);
    }
    if (phase === 'transcribing') {
      const status = await page.locator('#status-line').boundingBox();
      expect(status).toMatchObject({ x: 49, y: 50, height: 20 });
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

test('handoff shows a compact green confirmation for three seconds without replaying native ticks', async ({ page }) => {
  await page.clock.install();
  await page.clock.pauseAt(new Date(Date.now() + 60_000));
  await openSurface(page);
  const state = handoffState();
  await emitState(page, state);
  await page.clock.runFor(81);
  await expect(page.locator('#handoff-title')).toHaveText(messages.backgroundTranscriptionTitle);
  await expect(page.locator('#handoff-description')).toHaveText(messages.backgroundTranscriptionDescription);
  await expect(page.locator('#handoff-notice')).toBeVisible();
  const notice = await page.locator('#handoff-notice').boundingBox();
  expect(notice).toEqual({ x: 37.5, y: 97, width: 188, height: 64 });
  const style = await page.locator('#handoff-notice').evaluate((element) => ({
    radius: getComputedStyle(element).borderRadius, pointerEvents: getComputedStyle(element).pointerEvents,
  }));
  expect(style).toEqual({ radius: '36px', pointerEvents: 'none' });
  expect(await page.locator('#capsule').boundingBox()).toEqual({ x: 33, y: 13, width: 197, height: 72 });
  await page.screenshot({ path: 'test-results/recording-handoff-toast-visible.png' });
  await page.clock.runFor(2000);
  await emitState(page, { ...state, elapsedMs: 5000 });
  await page.clock.runFor(1001);
  await expectPreviousRetired(page);
  await page.screenshot({ path: 'test-results/recording-handoff-toast-expired.png' });
  await emitState(page, state);
  await expectPreviousRetired(page);
});

for (const previousPhase of ['completed', 'failed']) {
  test(`an old ${previousPhase} update keeps the neutral receipt until its original deadline`, async ({ page }) => {
    await page.clock.install();
    await page.clock.pauseAt(new Date(Date.now() + 60_000));
    await openSurface(page);
    const state = handoffState();
    await emitState(page, state);
    await page.clock.runFor(81);
    await expect(page.locator('#handoff-title')).toHaveText('上一条录音已保留');
    await page.clock.runFor(1000);
    const updated = { ...state, previous: { ...state.previous, phase: previousPhase } };
    await emitState(page, updated);
    await expect(page.locator('#handoff-notice')).toBeVisible();
    await expect(page.locator('#handoff-title')).toHaveText('上一条录音已保留');
    await expect(page.locator('#handoff-description')).toHaveText('首页「会议录音」可查看');
    await page.clock.runFor(1998);
    await expect(page.locator('#handoff-notice')).toBeVisible();
    await page.clock.runFor(2);
    await expectPreviousRetired(page);
    await emitState(page, updated);
    await page.clock.runFor(1000);
    await expectPreviousRetired(page);
  });
}

test('a new recording clears the previous animation and cannot show its stale confirmation', async ({ page }) => {
  await page.clock.install();
  await page.clock.pauseAt(new Date(Date.now() + 60_000));
  await openSurface(page, 'recording', false, false, false, { motion: 'no-preference' });
  await emitState(page, handoffState());
  await page.clock.runFor(200);
  await emitState(page, { recordingId: 'rec-c', resultAttempt: 'capture', phase: 'recording', elapsedMs: 7000, capsuleTop: 13, stackAbove: false });
  await page.clock.runFor(4000);
  await expect(page.locator('#previous-card')).toBeHidden();
  await expect(page.locator('#handoff-notice')).toBeHidden();
  await expect(page.locator('#deck')).toHaveAttribute('data-deck-view', 'recording');
  await expect(page.locator('#duration')).toHaveText('00:00:07');
});

test('expired handoff leaves only the current recording controls and never sends actions to the old recording', async ({ page }) => {
  await page.clock.install();
  await page.clock.pauseAt(new Date(Date.now() + 60_000));
  await openSurface(page);
  await emitState(page, handoffState());
  await page.clock.runFor(3300);
  await expectPreviousRetired(page);
  await emitState(page, handoffState({ elapsedMs: 9000, handoffNoticeId: undefined }));
  await expectPreviousRetired(page);
  await expect(page.locator('#duration')).toHaveText('00:00:09');
  await page.locator('#pause').click();
  await page.locator('#stop').click();
  const calls = await page.evaluate(() => (window as any).noteCalls);
  expect(calls).toContainEqual({ command: 'meeting_note_overlay_control', args: { expectedRecordingId: 'rec-b', paused: true } });
  expect(calls).toContainEqual({ command: 'meeting_note_overlay_control', args: { expectedRecordingId: 'rec-b' } });
  expect(calls.filter((call: { command: string; args?: { expectedRecordingId?: string } }) =>
    ['meeting_note_overlay_control', 'meeting_note_overlay_action'].includes(call.command) && call.args?.expectedRecordingId === 'rec-a')).toEqual([]);
});

test('leaving recording hides both deck cards and does not replay the consumed handoff', async ({ page }) => {
  await page.clock.install();
  await page.clock.pauseAt(new Date(Date.now() + 60_000));
  await openSurface(page);
  const state = handoffState();
  await emitState(page, state);
  await page.clock.runFor(81);
  await expect(page.locator('#handoff-notice')).toBeVisible();
  await emitState(page, { ...state, phase: 'summary_generating' });
  await expect(page.locator('#capsule')).toBeHidden();
  await expect(page.locator('#previous-card')).toBeHidden();
  await emitState(page, state);
  await page.clock.runFor(1000);
  await expect(page.locator('#handoff-notice')).toBeHidden();
});

for (const stackAbove of [false, true]) {
  test(`both cards remain inside the native window with the stack ${stackAbove ? 'above' : 'below'}`, async ({ page }) => {
    await page.clock.install();
    await page.clock.pauseAt(new Date(Date.now() + 60_000));
    await openSurface(page);
    const capsuleTop = stackAbove ? 97 : 13;
    await emitState(page, handoffState({ capsuleTop, stackAbove }));
    await page.clock.runFor(81);
    expect(await page.locator('#capsule').boundingBox()).toEqual({ x: 33, y: capsuleTop, width: 197, height: 72 });
    const notice = await page.locator('#handoff-notice').boundingBox();
    expect(notice).toEqual({ x: 37.5, y: stackAbove ? 13 : 97, width: 188, height: 64 });
    expect(notice!.y + notice!.height).toBeLessThanOrEqual(192);
    await page.clock.runFor(3300);
    await expectPreviousRetired(page);
    expect(await page.locator('#capsule').boundingBox()).toEqual({ x: 33, y: capsuleTop, width: 197, height: 72 });
  });
}

test('complete handoff uses one bounded flip and reduced motion uses a short transition', async ({ page }) => {
  await page.clock.install();
  await page.clock.pauseAt(new Date(Date.now() + 60_000));
  await openSurface(page, 'recording', false, false, false, { motion: 'no-preference' });
  await emitState(page, handoffState());
  await expect(page.locator('#deck')).toHaveAttribute('data-deck-view', 'handoff');
  await page.clock.runFor(559);
  await expect(page.locator('#deck')).toHaveAttribute('data-deck-view', 'handoff');
  await page.clock.runFor(2);
  await expect(page.locator('#deck')).toHaveAttribute('data-deck-view', 'recording');
  await expect(page.locator('#handoff-notice')).toBeVisible();
  await page.emulateMedia({ reducedMotion: 'reduce' });
  await emitState(page, handoffState({ recordingId: 'rec-c', handoffNoticeId: 'b-to-c', previous: { recordingId: 'rec-b', resultAttempt: 'capture', phase: 'transcribing' } }));
  await page.clock.runFor(81);
  await expect(page.locator('#deck')).toHaveAttribute('data-deck-view', 'recording');
  await expect(page.locator('#handoff-notice')).toBeVisible();
});

test('handoff makes moving controls inert and restores only the foreground card controls', async ({ page }) => {
  await page.clock.install();
  await page.clock.pauseAt(new Date(Date.now() + 60_000));
  await openSurface(page, 'recording', false, false, false, { motion: 'no-preference' });
  await emitState(page, handoffState());
  await expect(page.locator('#current-content')).toHaveJSProperty('inert', true);
  await expect(page.locator('#previous-full')).toHaveJSProperty('inert', true);
  await page.clock.runFor(561);
  await expect(page.locator('#current-content')).toHaveJSProperty('inert', false);
  await expect(page.locator('#previous-full')).toHaveJSProperty('inert', true);
  await expect(page.locator('#pause')).toBeEnabled();
  await page.clock.runFor(3300);
  await expectPreviousRetired(page);
  await expect(page.locator('#pause')).toBeEnabled();
  await expect(page.locator('#stop')).toBeEnabled();
});

test('native ticks near expiry cannot keep the previous receipt or controls alive', async ({ page }) => {
  await page.clock.install();
  await page.clock.pauseAt(new Date(Date.now() + 60_000));
  await openSurface(page, 'recording', false, false, false, { motion: 'no-preference' });
  const state = handoffState();
  await emitState(page, state);
  await page.clock.runFor(3500);
  await expect(page.locator('#handoff-notice')).toBeVisible();
  await emitState(page, { ...state, elapsedMs: 6500 });
  await page.clock.runFor(281);
  await expectPreviousRetired(page);
});

test('a late bootstrap response cannot replace a newer recording event or reset its notice', async ({ page }) => {
  await page.clock.install();
  await page.clock.pauseAt(new Date(Date.now() + 60_000));
  await openSurface(page, 'recording', false, false, false, { deferBootstrap: true });
  await emitState(page, handoffState());
  await page.clock.runFor(81);
  await page.evaluate(() => (window as any).resolveNoteBootstrap({ recordingId: 'rec-a', resultAttempt: 'capture', phase: 'transcribing' }));
  await expect(page.locator('#capsule')).toHaveAttribute('data-phase', 'recording');
  await expect(page.locator('#duration')).toHaveText('00:00:03');
  await expect(page.locator('#handoff-notice')).toBeVisible();
  await page.clock.runFor(3001);
  await expectPreviousRetired(page);
});

test('a previous card without a fresh handoff notice is never shown', async ({ page }) => {
  await page.clock.install();
  await page.clock.pauseAt(new Date(Date.now() + 60_000));
  await openSurface(page);
  await emitState(page, handoffState({ handoffNoticeId: undefined }));
  await expectPreviousRetired(page);
  await page.clock.runFor(5000);
  await expectPreviousRetired(page);
  await expect(page.locator('#pause')).toBeEnabled();
});

test('local expiry keeps the current capsule opaque until native restores its backdrop', async ({ page }) => {
  await page.clock.install();
  await page.clock.pauseAt(new Date(Date.now() + 60_000));
  await openSurface(page);
  await page.evaluate(() => { document.documentElement.dataset.backdrop = 'native'; });
  const state = handoffState();
  await emitState(page, state);
  await page.clock.runFor(3300);
  await expectPreviousRetired(page);
  await expect(page.locator('#capsule')).toHaveCSS('background-color', 'rgb(255, 255, 255)');
  await emitState(page, { ...state, previous: null, handoffNoticeId: undefined });
  await expectPreviousRetired(page);
  await expect(page.locator('#capsule')).toHaveCSS('background-color', 'rgba(0, 0, 0, 0)');
});

test('failed or completed old-state updates never resurrect an expired handoff', async ({ page }) => {
  await page.clock.install();
  await page.clock.pauseAt(new Date(Date.now() + 60_000));
  await openSurface(page);
  const state = handoffState();
  await emitState(page, state);
  await page.clock.runFor(3300);
  await expectPreviousRetired(page);
  for (const phase of ['failed', 'recording_failed', 'completed', 'transcribing']) {
    await emitState(page, { ...state, elapsedMs: 41000, previous: { ...state.previous, phase } });
    await expectPreviousRetired(page);
  }
  await page.clock.runFor(41000);
  await expectPreviousRetired(page);
  await expect(page.locator('#duration')).toHaveText('00:00:41');
});

test('changing stack direction cannot replay or extend the current confirmation', async ({ page }) => {
  await page.clock.install();
  await page.clock.pauseAt(new Date(Date.now() + 60_000));
  await openSurface(page, 'recording', false, false, false, { motion: 'no-preference' });
  const state = handoffState();
  await emitState(page, state);
  await page.clock.runFor(561);
  await expect(page.locator('#handoff-notice')).toBeVisible();
  await page.clock.runFor(2000);
  await emitState(page, { ...state, capsuleTop: 97, stackAbove: true });
  await page.clock.runFor(1220);
  await expectPreviousRetired(page);
  await emitState(page, state);
  await page.clock.runFor(1000);
  await expectPreviousRetired(page);
});

test('a late initial state cannot revive a confirmation after its deadline', async ({ page }) => {
  await page.clock.install();
  await page.clock.pauseAt(new Date(Date.now() + 60_000));
  await openSurface(page, 'recording', false, false, false, { deferBootstrap: true });
  const state = handoffState();
  await emitState(page, state);
  await page.clock.runFor(3300);
  await expectPreviousRetired(page);
  await page.evaluate((payload) => (window as any).resolveNoteBootstrap(payload), state);
  await page.clock.runFor(1000);
  await expectPreviousRetired(page);
});

test('a new handoff identity can show one new confirmation after an earlier one expires', async ({ page }) => {
  await page.clock.install();
  await page.clock.pauseAt(new Date(Date.now() + 60_000));
  await openSurface(page);
  await emitState(page, handoffState());
  await page.clock.runFor(3300);
  await expectPreviousRetired(page);
  const next = handoffState({ recordingId: 'rec-c', handoffNoticeId: 'b-to-c',
    previous: { recordingId: 'rec-b', resultAttempt: 'capture', phase: 'transcribing' } });
  await emitState(page, next);
  await page.clock.runFor(81);
  await expect(page.locator('#handoff-notice')).toBeVisible();
  await page.clock.runFor(3220);
  await expectPreviousRetired(page);
  await emitState(page, next);
  await page.clock.runFor(1000);
  await expectPreviousRetired(page);
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
  await page.clock.pauseAt(new Date(Date.now() + 60_000));
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
