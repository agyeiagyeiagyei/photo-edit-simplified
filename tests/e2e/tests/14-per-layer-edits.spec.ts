import { test, expect, Page } from '@playwright/test';
import {
  openApp, importFiles, clickTab, canvas, setRange, trackErrors, PNG_SMALL, RING,
} from './helpers';

// Issue #36 phase D: per-layer edits. Transform handles and the Rotate tab
// turn or scale text, path, brush and image layers; Heal and Clone act on an
// image layer's pixels; Color on an image layer edits an adjustment clipped to
// it. None of these may change the photo underneath.

const snapshot = (page: Page, key = '__before') =>
  canvas(page).evaluate((c: HTMLCanvasElement, k: string) => {
    (window as any)[k] = c.getContext('2d')!.getImageData(0, 0, c.width, c.height).data;
  }, key);

const diffSinceSnapshot = (page: Page, key = '__before') =>
  canvas(page).evaluate((c: HTMLCanvasElement, k: string) => {
    const d = c.getContext('2d')!.getImageData(0, 0, c.width, c.height).data;
    const base = (window as any)[k] as Uint8ClampedArray;
    if (!base || base.length !== d.length) return -1;
    let n = 0;
    for (let i = 0; i < d.length; i += 4) {
      if (
        Math.abs(d[i] - base[i]) > 8 ||
        Math.abs(d[i + 1] - base[i + 1]) > 8 ||
        Math.abs(d[i + 2] - base[i + 2]) > 8
      ) {
        n++;
      }
    }
    return n;
  }, key);

// A rebuild leaves the previous service worker armed, and its "New version
// ready" toast is fixed over the bottom of the preview, where it swallows
// presses on a layer's south-east handle. Remove it as it appears.
async function open(page: Page) {
  await page.addInitScript(() => {
    const kill = () => document.getElementById('pes-update')?.remove();
    // Init scripts run before the document element exists; observing the
    // document itself works from the first line of the page.
    new MutationObserver(kill).observe(document, { childList: true, subtree: true });
    kill();
  });
  return openApp(page);
}

async function drag(
  page: Page,
  from: { x: number; y: number },
  to: { x: number; y: number },
  steps = 12,
) {
  await page.mouse.move(from.x, from.y);
  await page.mouse.down();
  await page.mouse.move(to.x, to.y, { steps });
  await page.mouse.up();
}

async function addText(page: Page, text: string) {
  await page.getByRole('button', { name: '＋ Text' }).click();
  await page.waitForTimeout(400);
  await page.locator('.text-editor input[type=text]').fill(text);
  await page.locator('.text-editor input[type=range]').nth(0).fill('0.25');
  await page.waitForTimeout(900);
}

const layerRow = (page: Page, label: string) =>
  page.locator('.layer-row').filter({ hasText: label });

const visibleToggle = (page: Page, label: string) =>
  layerRow(page, label).locator('button.icon-btn').first();

const span = (page: Page, re: RegExp) =>
  page.locator('label.slider', { hasText: re }).locator('span').first();

test('text layer: handle drags and the Rotate tab act on the layer, not the photo', async ({ page }) => {
  test.setTimeout(180_000);
  const errors = trackErrors(page);
  await open(page);
  await importFiles(page, [PNG_SMALL]);
  await page.waitForTimeout(1200);
  await clickTab(page, 'Layers');
  await addText(page, 'TURN');
  await expect(page.locator('.layer-box.text-box')).toBeVisible();
  await page.waitForTimeout(600);

  // Photo-only baseline: the layer hidden.
  await visibleToggle(page, 'T: TURN').click();
  await page.waitForTimeout(700);
  await snapshot(page, '__photo');
  await visibleToggle(page, 'T: TURN').click();
  await page.waitForTimeout(600);

  // Move: drag inside the box.
  await snapshot(page);
  let box = (await page.locator('.layer-box').boundingBox())!;
  await drag(
    page,
    { x: box.x + box.width / 2, y: box.y + box.height / 2 },
    { x: box.x + box.width / 2 + 120, y: box.y + box.height / 2 + 60 },
  );
  await page.waitForTimeout(600);
  expect(await diffSinceSnapshot(page), 'the text did not move').toBeGreaterThan(200);

  // Rotate: drag the knob above the box.
  await snapshot(page);
  const knob = (await page.locator('.xform-rotate').boundingBox())!;
  await drag(
    page,
    { x: knob.x + knob.width / 2, y: knob.y + knob.height / 2 },
    { x: knob.x + knob.width / 2 + 150, y: knob.y + knob.height / 2 + 40 },
  );
  await page.waitForTimeout(600);
  expect(await diffSinceSnapshot(page), 'the text did not rotate').toBeGreaterThan(200);

  // Scale: drag the south-east corner outward.
  await snapshot(page);
  const se = (await page.locator('.xform-handle.se').boundingBox())!;
  await drag(
    page,
    { x: se.x + se.width / 2, y: se.y + se.height / 2 },
    { x: se.x + se.width / 2 + 70, y: se.y + se.height / 2 + 50 },
  );
  await page.waitForTimeout(600);
  expect(await diffSinceSnapshot(page), 'the text did not scale').toBeGreaterThan(200);

  // Hiding the layer gives back exactly the photo from before the gestures.
  await visibleToggle(page, 'T: TURN').click();
  await page.waitForTimeout(800);
  expect(
    await diffSinceSnapshot(page, '__photo'),
    'the photo changed when the text layer moved, turned or scaled',
  ).toBeLessThanOrEqual(10);
  await visibleToggle(page, 'T: TURN').click();
  await page.waitForTimeout(500);

  // The Rotate tab targets the layer: no photo controls, non-zero values.
  await clickTab(page, 'Rotate');
  await expect(page.locator('.editing-line')).toContainText('Editing:');
  await expect(page.locator('.editing-line')).toContainText('T: TURN');
  await expect(page.getByRole('button', { name: '⟲ 90°' })).toHaveCount(0);
  expect(await span(page, /^Angle:/).textContent()).toMatch(/Angle: -?[1-9][0-9]*°/);
  const sizeText = (await span(page, /^Size:/).textContent())!;
  expect(parseFloat(sizeText.replace('Size: ', ''))).toBeGreaterThan(25);

  // Back to the photo: it was never rotated.
  await page.locator('.editing-line button', { hasText: 'Edit the photo instead' }).click();
  await expect(page.locator('.editing-line')).toHaveText('Editing: Photo');
  await expect(page.getByRole('button', { name: '⟲ 90°' })).toHaveCount(1);
  await expect(span(page, /^Straighten:/)).toHaveText('Straighten: 0.0°');
  expect(errors, errors.join('\n')).toEqual([]);
});

test('image layer: colour, heal, clone and transforms stay on the layer', async ({ page }) => {
  test.setTimeout(240_000);
  const errors = trackErrors(page);
  await open(page);
  await importFiles(page, [PNG_SMALL]);
  await page.waitForTimeout(1200);
  await clickTab(page, 'Layers');
  await page.locator('.layers-tab input[type=file]').setInputFiles(RING);
  await expect(layerRow(page, 'Image')).toHaveCount(1, { timeout: 60_000 });
  await expect(page.locator('.layer-box')).toBeVisible({ timeout: 30_000 });
  await page.waitForTimeout(800);

  // Photo-only baseline: the image layer hidden.
  await visibleToggle(page, 'Image').click();
  await page.waitForTimeout(700);
  await snapshot(page, '__photo');
  await visibleToggle(page, 'Image').click();
  await page.waitForTimeout(600);

  // Color on an image layer edits a clipped adjustment.
  await clickTab(page, 'Color');
  await expect(page.locator('.editing-line')).toContainText('Editing: Image');
  await expect(page.locator('.adjust-editor > .panel-title')).toHaveText('Clipped to this image layer');
  await snapshot(page);
  const sat = (await page
    .locator('.adjust-editor label.slider', { hasText: /^Saturation/ })
    .locator('input')
    .boundingBox())!;
  await drag(
    page,
    { x: sat.x + sat.width / 2, y: sat.y + sat.height / 2 },
    { x: sat.x - 10, y: sat.y + sat.height / 2 },
  );
  await page.waitForTimeout(1000);
  await expect(
    page.locator('.adjust-editor label.slider', { hasText: /^Saturation/ }).locator('span').first(),
  ).toHaveText('Saturation: -100%');
  expect(await diffSinceSnapshot(page), 'the clipped colour did not change the image layer').toBeGreaterThan(100);

  // The clip lives in the layer stack, directly above its target.
  await clickTab(page, 'Layers');
  await expect(page.locator('.layer-row')).toHaveCount(2);
  await expect(page.locator('.layer-row .layer-label').first()).toHaveText('Adjust');
  await layerRow(page, 'Image').locator('.layer-label').click();

  // Heal edits the layer's own pixels.
  await clickTab(page, 'Heal');
  await expect(page.locator('.editing-line')).toContainText('Editing: Image');
  await snapshot(page);
  let c = (await canvas(page).boundingBox())!;
  await drag(
    page,
    { x: c.x + c.width / 2 - 60, y: c.y + c.height / 2 - 20 },
    { x: c.x + c.width / 2 + 60, y: c.y + c.height / 2 + 20 },
  );
  await page.waitForTimeout(1000);
  expect(await diffSinceSnapshot(page), 'healing did not change the image layer').toBeGreaterThan(20);

  // Clone stamps from the layer's own pixels.
  await clickTab(page, 'Layers');
  await layerRow(page, 'Image').locator('.layer-label').click();
  await clickTab(page, 'Clone');
  await expect(page.locator('.editing-line')).toContainText('Editing: Image');
  await page.getByRole('button', { name: 'Set source' }).click();
  c = (await canvas(page).boundingBox())!;
  await page.mouse.click(c.x + c.width / 2 - 80, c.y + c.height / 2);
  await page.waitForTimeout(400);
  await snapshot(page);
  await drag(
    page,
    { x: c.x + c.width / 2 + 40, y: c.y + c.height / 2 },
    { x: c.x + c.width / 2 + 120, y: c.y + c.height / 2 + 20 },
  );
  await page.waitForTimeout(1000);
  expect(await diffSinceSnapshot(page), 'cloning did not change the image layer').toBeGreaterThan(20);

  // Transform handles and the Rotate tab act on the image layer.
  await clickTab(page, 'Layers');
  await layerRow(page, 'Image').locator('.layer-label').click();
  await expect(page.locator('.layer-box')).toBeVisible();
  const knob = (await page.locator('.xform-rotate').boundingBox())!;
  await drag(
    page,
    { x: knob.x + knob.width / 2, y: knob.y + knob.height / 2 },
    { x: knob.x + knob.width / 2 + 140, y: knob.y + knob.height / 2 + 40 },
  );
  await page.waitForTimeout(600);
  await clickTab(page, 'Rotate');
  await expect(page.locator('.editing-line')).toContainText('Editing: Image');
  expect(await span(page, /^Angle:/).textContent()).toMatch(/Angle: -?[1-9][0-9]*°/);
  await clickTab(page, 'Layers');
  const se = (await page.locator('.xform-handle.se').boundingBox())!;
  await drag(
    page,
    { x: se.x + se.width / 2, y: se.y + se.height / 2 },
    { x: se.x + se.width / 2 + 80, y: se.y + se.height / 2 + 60 },
  );
  await page.waitForTimeout(600);
  await clickTab(page, 'Rotate');
  const scaleText = (await span(page, /^Scale:/).textContent())!;
  expect(parseFloat(scaleText.replace('Scale: ', ''))).toBeGreaterThan(50);

  // Every one of those edits lived on the layer: hiding it restores the photo.
  await clickTab(page, 'Layers');
  await visibleToggle(page, 'Image').click();
  await page.waitForTimeout(900);
  expect(
    await diffSinceSnapshot(page, '__photo'),
    'the photo changed under the layer colour, healing, cloning or transform',
  ).toBeLessThanOrEqual(10);
  expect(errors, errors.join('\n')).toEqual([]);
});

test('brush layer: the Rotate tab turns the stroke without turning the photo', async ({ page }) => {
  test.setTimeout(120_000);
  const errors = trackErrors(page);
  await open(page);
  await importFiles(page, [PNG_SMALL]);
  await page.waitForTimeout(1200);
  await clickTab(page, 'Layers');
  await snapshot(page, '__photo');

  await page.locator('.toolbar .chip', { hasText: 'Brush' }).click();
  await page.waitForTimeout(500);
  const c = (await canvas(page).boundingBox())!;
  await drag(
    page,
    { x: c.x + c.width * 0.4, y: c.y + c.height * 0.5 },
    { x: c.x + c.width * 0.6, y: c.y + c.height * 0.5 },
  );
  await page.waitForTimeout(600);
  await page.locator('.toolbar .chip', { hasText: 'Select' }).click();
  await expect(page.locator('.layer-box')).toBeVisible();
  await page.waitForTimeout(400);

  await snapshot(page);
  await clickTab(page, 'Rotate');
  await expect(page.locator('.editing-line')).toContainText('Editing: Brush');
  await expect(span(page, /^Angle:/)).toHaveText('Angle: 0°');
  await setRange(page, /^Angle:/, 40);
  await page.waitForTimeout(900);
  expect(await diffSinceSnapshot(page), 'the brush stroke did not turn').toBeGreaterThan(100);

  await clickTab(page, 'Layers');
  await visibleToggle(page, 'Brush').click();
  await page.waitForTimeout(800);
  expect(
    await diffSinceSnapshot(page, '__photo'),
    'the photo changed when the brush layer turned',
  ).toBeLessThanOrEqual(10);
  expect(errors, errors.join('\n')).toEqual([]);
});

test('layer transforms and the clipped adjustment survive a reload', async ({ page }) => {
  test.setTimeout(180_000);
  const errors = trackErrors(page);
  await open(page);
  await importFiles(page, [PNG_SMALL]);
  await page.waitForTimeout(1200);
  await clickTab(page, 'Layers');
  await page.locator('.layers-tab input[type=file]').setInputFiles(RING);
  await expect(layerRow(page, 'Image')).toHaveCount(1, { timeout: 60_000 });
  await page.waitForTimeout(800);

  await clickTab(page, 'Rotate');
  await setRange(page, /^Angle:/, 45);
  await setRange(page, /^Scale:/, 0.7);
  await page.waitForTimeout(400);
  await clickTab(page, 'Color');
  await expect(page.locator('.adjust-editor > .panel-title')).toHaveText('Clipped to this image layer');
  const sat = (await page
    .locator('.adjust-editor label.slider', { hasText: /^Saturation/ })
    .locator('input')
    .boundingBox())!;
  await drag(
    page,
    { x: sat.x + sat.width / 2, y: sat.y + sat.height / 2 },
    { x: sat.x - 10, y: sat.y + sat.height / 2 },
  );
  await page.waitForTimeout(600);
  await expect(
    page.locator('.adjust-editor label.slider', { hasText: /^Saturation/ }).locator('span').first(),
  ).toHaveText('Saturation: -100%');

  // Wait for a persisted session that includes the edits (debounce + PNG of
  // the image layer), then reload. The debounce may already have passed while
  // the drag settled, so nudge the stack: the toggle schedules a fresh pass
  // after t0 that necessarily carries the edits.
  await page.waitForTimeout(500);
  const t0 = await page.evaluate(() => performance.now());
  await clickTab(page, 'Layers');
  await visibleToggle(page, 'Image').click();
  await page.waitForTimeout(300);
  await visibleToggle(page, 'Image').click();
  await page.waitForFunction(
    (t) => {
      const p = (window as any).__pes?.perf?.() || [];
      return p.some(
        (e: any) => e.name === 'session.persist' && e.detail === '1 items' && e.at > t,
      );
    },
    t0,
    { timeout: 60_000, polling: 250 },
  );
  await page.reload();
  await expect(page.locator('header h1')).toBeVisible();
  await expect(page.locator('.overlay')).toHaveCount(0, { timeout: 60_000 });
  await expect(page.locator('.filmstrip .thumb')).toHaveCount(1, { timeout: 60_000 });
  await page.locator('.filmstrip .thumb').first().click();
  await page.waitForTimeout(800);

  await clickTab(page, 'Layers');
  await expect(page.locator('.layer-row')).toHaveCount(2);
  await layerRow(page, 'Image').locator('.layer-label').click();
  await clickTab(page, 'Rotate');
  await expect(span(page, /^Angle:/)).toHaveText('Angle: 45°');
  await expect(span(page, /^Scale:/)).toHaveText('Scale: 70%');
  await clickTab(page, 'Color');
  await expect(page.locator('.adjust-editor > .panel-title')).toHaveText('Clipped to this image layer');
  await expect(
    page.locator('.adjust-editor label.slider', { hasText: /^Saturation/ }).locator('span').first(),
  ).toHaveText('Saturation: -100%');
  expect(errors, errors.join('\n')).toEqual([]);
});
