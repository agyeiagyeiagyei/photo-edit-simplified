import { test, expect, Page } from '@playwright/test';
import { openApp, importFiles, clickTab, canvas, trackErrors, PNG_SMALL } from './helpers';

const centerColors = (page: Page) =>
  canvas(page).evaluate((c: HTMLCanvasElement) => {
    const ctx = c.getContext('2d')!;
    const w = Math.max(1, Math.floor(c.width * 0.2));
    const h = Math.max(1, Math.floor(c.height * 0.2));
    const d = ctx.getImageData(
      Math.floor(c.width * 0.4),
      Math.floor(c.height * 0.4),
      w,
      h,
    ).data;
    let red = 0;
    let blue = 0;
    for (let i = 0; i < d.length; i += 4) {
      if (d[i] > 150 && d[i + 2] < 100) red++;
      if (d[i + 2] > 150 && d[i] < 100) blue++;
    }
    return { red, blue };
  });

const snapshot = (page: Page) =>
  canvas(page).evaluate((c: HTMLCanvasElement) => {
    (window as any).__layersBefore = c.getContext('2d')!.getImageData(0, 0, c.width, c.height).data;
  });

const diffSinceSnapshot = (page: Page) =>
  canvas(page).evaluate((c: HTMLCanvasElement) => {
    const d = c.getContext('2d')!.getImageData(0, 0, c.width, c.height).data;
    const base = (window as any).__layersBefore as Uint8ClampedArray;
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
  });

const addText = async (page: Page, text: string, color: string) => {
  await page.getByRole('button', { name: '＋ Text' }).click();
  await page.waitForTimeout(300);
  await page.locator('.text-editor input[type=text]').fill(text);
  await page.locator('.text-editor input[type=range]').nth(0).fill('0.3');
  await page.locator('.text-editor input[type=color]').nth(0).fill(color);
  await page.waitForTimeout(600);
};

test('layers panel lists topmost first and the arrows reorder the stack', async ({ page }) => {
  test.setTimeout(120_000);
  const errors = trackErrors(page);
  await openApp(page);
  await importFiles(page, [PNG_SMALL]);
  await page.waitForTimeout(1500);
  await clickTab(page, 'Layers');

  await addText(page, 'MMMMMMMM', '#ff0000');
  await addText(page, 'MMMMMMMM', '#0000ff');
  await page.waitForTimeout(800);

  const rows = page.locator('.layer-row');
  await expect(rows).toHaveCount(2);

  // The last added layer is on top of the picture and must be listed first.
  await rows.nth(0).locator('.layer-label').click();
  await expect(page.locator('.text-editor input[type=color]').nth(0)).toHaveValue('#0000ff');
  let c = await centerColors(page);
  expect(c.blue, 'the blue text layer is not composited on top').toBeGreaterThan(c.red + 500);

  // ↓ on the top row lowers blue; red becomes the top layer and first row.
  await rows.nth(0).locator('button', { hasText: '↓' }).click();
  await page.waitForTimeout(900);
  await rows.nth(0).locator('.layer-label').click();
  await expect(page.locator('.text-editor input[type=color]').nth(0)).toHaveValue('#ff0000');
  c = await centerColors(page);
  expect(c.red, '↓ did not lower the blue layer').toBeGreaterThan(c.blue + 500);

  // ↑ on the blue row (now second) raises it back to the top.
  await rows.nth(1).locator('button', { hasText: '↑' }).click();
  await page.waitForTimeout(900);
  await rows.nth(0).locator('.layer-label').click();
  await expect(page.locator('.text-editor input[type=color]').nth(0)).toHaveValue('#0000ff');
  c = await centerColors(page);
  expect(c.blue, '↑ did not raise the blue layer').toBeGreaterThan(c.red + 500);

  expect(errors, errors.join('\n')).toEqual([]);
});

test('Pen and Brush select or create a layer and draw', async ({ page }) => {
  test.setTimeout(180_000);
  const errors = trackErrors(page);
  await openApp(page);
  await importFiles(page, [PNG_SMALL]);
  await page.waitForTimeout(1500);
  await clickTab(page, 'Layers');

  const labels = () => page.locator('.layer-row .layer-label').allTextContents();
  await addText(page, 'X', '#ff0000');
  expect(await labels()).toEqual(['T: X']);

  // Pen with a text layer selected: it must create and select a Path layer,
  // not silently ignore the tap.
  await page.locator('.toolbar .chip', { hasText: 'Pen' }).click();
  await page.waitForTimeout(500);
  await expect(page.locator('.layer-row.selected .layer-label')).toHaveText('Path');
  expect(await labels()).toEqual(['Path', 'T: X']);

  const b = await canvas(page).boundingBox();
  await snapshot(page);
  await canvas(page).click({ position: { x: b!.width * 0.3, y: b!.height * 0.35 } });
  await canvas(page).click({ position: { x: b!.width * 0.7, y: b!.height * 0.65 } });
  await page.waitForTimeout(900);
  expect(await diffSinceSnapshot(page), 'Pen taps did not draw a path').toBeGreaterThan(50);

  // Brush with the Path selected: creates a Brush layer and draws.
  await page.locator('.toolbar .chip', { hasText: 'Brush' }).click();
  await page.waitForTimeout(500);
  await expect(page.locator('.layer-row.selected .layer-label')).toHaveText('Brush');
  expect(await labels()).toEqual(['Brush', 'Path', 'T: X']);

  await snapshot(page);
  await page.mouse.move(b!.x + b!.width * 0.2, b!.y + b!.height * 0.25);
  await page.mouse.down();
  await page.mouse.move(b!.x + b!.width * 0.6, b!.y + b!.height * 0.3, { steps: 8 });
  await page.mouse.up();
  await page.waitForTimeout(900);
  expect(await diffSinceSnapshot(page), 'Brush drag did not draw').toBeGreaterThan(50);

  // Pen again with the Brush selected reuses the existing Path layer instead
  // of stacking another empty one.
  await page.locator('.toolbar .chip', { hasText: 'Pen' }).click();
  await page.waitForTimeout(500);
  await expect(page.locator('.layer-row.selected .layer-label')).toHaveText('Path');
  expect(await labels()).toEqual(['Brush', 'Path', 'T: X']);

  expect(errors, errors.join('\n')).toEqual([]);
});

test('arrows disable at the overlay/adjustment group boundary', async ({ page }) => {
  test.setTimeout(120_000);
  const errors = trackErrors(page);
  await openApp(page);
  await importFiles(page, [PNG_SMALL]);
  await page.waitForTimeout(1500);
  await clickTab(page, 'Layers');

  // One text layer and one adjustment: each group has a single layer, so
  // neither can move and both arrow pairs must look inert.
  await addText(page, 'X', '#ff0000');
  await page.getByRole('button', { name: '＋ Adjust', exact: true }).click();
  await page.waitForTimeout(800);
  const rows = page.locator('.layer-row');
  await expect(rows).toHaveCount(2);
  for (let i = 0; i < 2; i++) {
    await expect(rows.nth(i).locator('button', { hasText: '↑' }), `row ${i} ↑`).toBeDisabled();
    await expect(rows.nth(i).locator('button', { hasText: '↓' }), `row ${i} ↓`).toBeDisabled();
  }

  // A second text layer makes the overlay pair movable, but the adjustment
  // still cannot cross the group boundary.
  await addText(page, 'Y', '#0000ff');
  await expect(rows).toHaveCount(3);
  await expect(rows.nth(0).locator('button', { hasText: '↑' })).toBeDisabled();
  await expect(rows.nth(0).locator('button', { hasText: '↓' })).toBeEnabled();
  await expect(rows.nth(1).locator('button', { hasText: '↑' })).toBeEnabled();
  await expect(rows.nth(1).locator('button', { hasText: '↓' })).toBeDisabled();
  await expect(rows.nth(2).locator('button', { hasText: '↑' })).toBeDisabled();
  await expect(rows.nth(2).locator('button', { hasText: '↓' })).toBeDisabled();

  expect(errors, errors.join('\n')).toEqual([]);
});
