import { test, expect, Page } from '@playwright/test';
import { openApp, importFiles, thumbs, clickTab, setRange, trackErrors, JPEG_A, JPEG_B, PNG_SMALL } from './helpers';

// The only way to remove a photo used to be the Drive star on its thumb, and
// that star only renders for a Drive file while this session is signed in to
// Drive. After a reload (signed out) no thumb had a star, and a photo opened
// from disk never had one, so nothing could be removed. Every thumb now has a
// remove button; a Drive photo is also unstarred, or queued to be unstarred
// when the Drive manifest next loads.

const removeBtn = (page: Page, i: number) => thumbs(page).nth(i).locator('.thumb-remove');
const names = (page: Page) => thumbs(page).evaluateAll((els) => els.map((e) => e.getAttribute('title')));

/** Set `dfid` on every stored item record, as a Drive import would have. */
async function markStoredAsDrive(page: Page, id: string) {
  await page.evaluate((id) => new Promise<void>((resolve, reject) => {
    const req = indexedDB.open('pes-session');
    req.onerror = () => reject(req.error);
    req.onsuccess = () => {
      const db = req.result;
      const tx = db.transaction('items', 'readwrite');
      const cur = tx.objectStore('items').openCursor();
      cur.onsuccess = () => {
        const c = cur.result;
        if (!c) return;
        c.update({ ...c.value, dfid: id, dpid: 'drive-parent' });
        c.continue();
      };
      tx.oncomplete = () => { db.close(); resolve(); };
      tx.onerror = () => reject(tx.error);
    };
  }), id);
}

test.describe('filmstrip remove', () => {
  test('an untouched photo is removed at once, and the neighbour is selected', async ({ page }) => {
    const errors = trackErrors(page);
    let dialogs = 0;
    page.on('dialog', (d) => { dialogs++; void d.dismiss(); });
    await openApp(page);
    await importFiles(page, [JPEG_A, JPEG_B, PNG_SMALL]);
    await thumbs(page).nth(1).click();
    await thumbs(page).nth(1).hover();
    await expect(removeBtn(page, 1)).toBeVisible();
    await expect(removeBtn(page, 1)).toHaveCSS('opacity', '1');
    const before = await names(page);
    await removeBtn(page, 1).click();
    await expect(thumbs(page)).toHaveCount(2);
    expect(dialogs, 'an unedited photo should not ask').toBe(0);
    expect(await names(page)).toEqual([before[0], before[2]]);
    // The removed photo was selected: its right-hand neighbour takes over.
    await expect(page.locator('.filmstrip .thumb.selected')).toHaveAttribute('title', before[2]!);
    await expect(page.locator('.canvas-wrap canvas')).toBeVisible();
    expect(errors, errors.join('\n')).toEqual([]);
  });

  test('an edited photo asks first; Cancel keeps it, OK removes it, and it stays removed after a reload', async ({ page }) => {
    const errors = trackErrors(page);
    await openApp(page);
    await importFiles(page, [JPEG_A, PNG_SMALL]);
    await thumbs(page).first().click();
    await clickTab(page, 'Color');
    await setRange(page, /^Brightness/, 0.2);
    await expect(thumbs(page).first().locator('.thumb-edited')).toBeVisible();

    let message = '';
    page.once('dialog', (d) => { message = d.message(); void d.dismiss(); });
    await thumbs(page).first().hover();
    await removeBtn(page, 0).click();
    await expect.poll(() => message).toContain('from the editor');
    expect(message).toContain('edits will be lost');
    await expect(thumbs(page)).toHaveCount(2);

    page.once('dialog', (d) => void d.accept());
    await removeBtn(page, 0).click();
    await expect(thumbs(page)).toHaveCount(1);
    await page.waitForTimeout(2000); // debounced persist
    await page.reload();
    await expect(page.locator('header h1')).toBeVisible();
    await expect(thumbs(page)).toHaveCount(1, { timeout: 60_000 });
    await page.waitForTimeout(1500);
    await expect(thumbs(page)).toHaveCount(1);
    expect(errors, errors.join('\n')).toEqual([]);
  });

  test('a Drive photo can be removed while signed out, and its unstar waits for the next Drive load', async ({ page }) => {
    const errors = trackErrors(page);
    await openApp(page);
    await importFiles(page, [PNG_SMALL]);
    await page.waitForTimeout(2000); // first persist writes the record
    await markStoredAsDrive(page, 'drive-file-123');
    await page.reload();
    await expect(page.locator('header h1')).toBeVisible();
    await expect(thumbs(page)).toHaveCount(1, { timeout: 60_000 });
    // Signed out after a reload: no star, which is what used to leave no way out.
    await expect(thumbs(page).first().locator('.thumb-star')).toHaveCount(0);
    await thumbs(page).first().hover();
    await removeBtn(page, 0).click();
    await expect(thumbs(page)).toHaveCount(0);
    expect(await page.evaluate(() => localStorage.getItem('pes-drive-unstar'))).toBe('drive-file-123');
    expect(errors, errors.join('\n')).toEqual([]);
  });
});
