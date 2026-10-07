// The admin web UI in a real (headless) browser, driven the way an operator
// uses it, against a running mint with a node and channels:
//
//   node e2e/admin_ui.mjs <admin url> <admin token> [screenshot dir]
//
// e2e/regtest.py runs it with UI=1. Exits non-zero on the first failed check,
// or on any console error but the expected 401s and refusals (CSP violations
// included).

import assert from 'node:assert/strict';
import {mkdirSync} from 'node:fs';

import {chromium} from 'playwright';

const [base, token, shots] = process.argv.slice(2);
if (!base || !token) {
  console.error('usage: node e2e/admin_ui.mjs <admin url> <admin token> [screenshot dir]');
  process.exit(2);
}
if (shots) mkdirSync(shots, {recursive: true});

const problems = [];
// refusals the test provokes on purpose, each answered 400 once
let refusals = 0;
const watch = (page) => {
  page.on('console', (m) => {
    if (m.type() !== 'error') return;
    // the probe before login, a wrong token, and after logout are 401s
    if (m.text().includes('401')) return;
    if (m.text().includes('400') && refusals > 0) {
      refusals--;
      return;
    }
    problems.push(m.text());
  });
  page.on('pageerror', (e) => problems.push(`page error: ${e.message}`));
};
const shot = (page, name) => shots && page.screenshot({path: `${shots}/${name}.png`, fullPage: true});
const log = (msg) => console.log(`[admin-ui] ${msg}`);

const browser = await chromium.launch();
try {
  const context = await browser.newContext({viewport: {width: 1280, height: 860}});
  const page = await context.newPage();
  watch(page);

  // ---- login ----
  await page.goto(base);
  await page.waitForSelector('#login:not([hidden])');
  await page.fill('#token', 'wrong');
  await page.click('#login-form button[type=submit]');
  await page.waitForSelector('#login-error:not([hidden])');
  assert.equal(await page.textContent('#login-error'), 'Wrong token.');
  await page.fill('#token', token);
  await page.click('#login-form button[type=submit]');
  await page.waitForSelector('#view .card');
  assert.match(await page.textContent('#title'), /^mint@/);
  log('a wrong token is refused, the right one signs in');

  // ---- overview ----
  await page.waitForSelector('#view .card :text("ready")');
  const overview = await page.textContent('#view');
  assert.match(overview, /Outstanding/);
  assert.match(overview, /\d+ usable/);
  await shot(page, 'overview');
  log('overview shows the node ready, notes and liquidity');

  // ---- channels ----
  await page.click('[data-tab=channels]');
  await page.waitForSelector('#view table tbody tr');
  const rows = await page.locator('#view table tbody tr').count();
  assert.ok(rows >= 1, 'a channel is listed');
  await shot(page, 'channels');
  log(`channels list ${rows} channel(s)`);

  // ---- payments: an operator invoice with its QR code ----
  await page.click('[data-tab=payments]');
  await page.fill('input[name=amount]', '2500');
  await page.click('text=Create invoice');
  const qr = page.locator('.result img.qr').first();
  await qr.waitFor();
  await page.waitForFunction(() => document.querySelector('.result img.qr')?.naturalWidth > 0);
  assert.match(await page.locator('.result code').first().getAttribute('title'), /^lnbcrt/);
  await shot(page, 'invoice');
  log('an invoice is created and its QR code renders');

  // a bootstrap invoice needs an LSP, and A has none: the reason is shown
  await page.locator('input[name=amount]').nth(1).fill('50000');
  refusals++;
  await page.click('text=Create bootstrap invoice');
  await page.waitForSelector('#toast.error:not([hidden])');
  assert.match(await page.textContent('#toast'), /no LSP configured/);
  log('a bootstrap invoice without an LSP is refused, with the reason');

  // ---- wallet: a fresh address with its QR code ----
  await page.click('[data-tab=wallet]');
  await page.click('text=New receive address');
  await page.waitForFunction(() => document.querySelector('.result img.qr')?.naturalWidth > 0);
  assert.match(await page.textContent('.result code'), /^bcrt1q/);
  log('a fresh address is shown with its QR code');

  // ---- notes ----
  await page.click('[data-tab=notes]');
  await page.waitForSelector('#view :text("Pending melts")');
  log('notes tab loads');

  // ---- the session survives a reload; the tab is in the URL ----
  await page.reload();
  await page.waitForSelector('#app:not([hidden])');
  assert.equal(await page.getAttribute('[aria-selected=true]', 'data-tab'), 'notes');
  log('a reload keeps the session and the tab');

  // ---- a phone, in dark mode ----
  const phone = await browser.newContext({
    viewport: {width: 390, height: 844},
    colorScheme: 'dark',
    storageState: await context.storageState(),
  });
  const small = await phone.newPage();
  watch(small);
  for (const tab of ['overview', 'channels', 'payments', 'wallet', 'notes']) {
    await small.goto(`${base}#${tab}`);
    await small.waitForSelector('#view .card');
    await small.waitForTimeout(300);
    const overflow = await small.evaluate(() => document.documentElement.scrollWidth - innerWidth);
    assert.equal(overflow, 0, `${tab} scrolls sideways on a phone`);
    if (tab === 'channels') await shot(small, 'phone-channels');
  }
  await phone.close();
  log('every tab fits a phone screen');

  // ---- logout ends the session ----
  await page.click('#logout');
  await page.waitForSelector('#login:not([hidden])');
  assert.equal(await page.evaluate(() => fetch('info').then((r) => r.status)), 401);
  log('signing out ends the session');

  assert.deepEqual(problems, [], 'console errors');
  log('PASS');
} finally {
  await browser.close();
}
