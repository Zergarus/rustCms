// Скриншоты админки для дизайн-аудита и визуальных проверок.
// Требуется playwright: `npm i playwright && npx playwright install chromium`.
// Запуск: BASE_URL=http://127.0.0.1:3010 ADMIN_LOGIN=designer ADMIN_PASSWORD=… node scripts/screens.mjs [каталог]
import { chromium } from "playwright";

const base = process.env.BASE_URL || "http://127.0.0.1:3010";
const login = process.env.ADMIN_LOGIN || "designer";
const password = process.env.ADMIN_PASSWORD || "";
const out = process.argv[2] || "design-shots";
const pages = [
  ["dashboard", "/admin"],
  ["collections", "/admin/collections"],
  ["orders", "/admin/shop/orders"],
  ["item", process.env.ITEM_PATH || "/admin/items/1"],
  ["settings", "/admin/settings"],
  ["mail", "/admin/mail"],
  ["queue", "/admin/mail/queue"],
];
if (process.env.ORDER_PATH) pages.splice(3, 0, ["order", process.env.ORDER_PATH]);

if (!password) {
  console.error("нужен ADMIN_PASSWORD");
  process.exit(2);
}

const browser = await chromium.launch();
const context = await browser.newContext({ viewport: { width: 1440, height: 900 } });
const page = await context.newPage();
await page.goto(base + "/admin/login");
await page.fill('input[name="login"]', login);
await page.fill('input[name="password"]', password);
await Promise.all([page.waitForNavigation(), page.click('button[type="submit"]')]);
await import("node:fs").then((fs) => fs.mkdirSync(out, { recursive: true }));
for (const [name, path] of pages) {
  await page.goto(base + path, { waitUntil: "networkidle" });
  await page.screenshot({ path: `${out}/${name}.png`, fullPage: true });
  console.log("shot", name);
}
await browser.close();
