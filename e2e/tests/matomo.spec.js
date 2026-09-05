const { test, expect } = require('@playwright/test');

const username = process.env.MATOMO_E2E_USERNAME || 'root';
const password = process.env.MATOMO_E2E_PASSWORD || 'ChangeMe123!';

async function loginIfRequired(page) {
  await page.goto('/');
  const login = page.locator('input[name="login"]');
  if (await login.isVisible().catch(() => false)) {
    await login.fill(username);
    await page.locator('input[name="password"]').fill(password);
    await page.locator('button[type="submit"], input[type="submit"]').first().click();
    await page.waitForLoadState('domcontentloaded');
  }
  await expect(page).not.toHaveURL(/module=Installation/);
}

test('administrator can reach the Matomo dashboard', async ({ page }) => {
  await loginIfRequired(page);
  await expect(page).toHaveTitle(/Matomo|Dashboard/i);
  await expect(page.locator('body')).toContainText(/Dashboard|Matomo/i);
});

test('browser can send a tracking request to Matomo', async ({ page }) => {
  await loginIfRequired(page);
  const trackerResponse = page.waitForResponse((response) => {
    return /\/(?:matomo|piwik)\.php(?:\?|$)/.test(response.url()) && response.status() < 500;
  });
  await page.evaluate(() => {
    const image = new Image();
    image.src = `${window.location.origin}/matomo.php?idsite=1&rec=1&url=${encodeURIComponent(window.location.href)}&action_name=Playwright%20E2E&rand=${Date.now()}`;
    document.body.appendChild(image);
  });
  const response = await trackerResponse;
  expect(response.status()).toBeLessThan(400);
});
