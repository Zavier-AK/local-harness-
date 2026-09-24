// The voice agent's browser: its own Chrome window, with its own profile, driven by
// Playwright. Spoken to over stdin/stdout, one JSON object per line:
//   → {"id": 1, "op": "open", "url": "https://…"}
//   ← {"id": 1, "ok": true, "title": "…", "url": "…"}
//   ← {"id": 1, "ok": false, "error": "…"}
//
// It only does what it is told. Deciding what may be clicked without asking the person
// is the harness's job (harness_core::voice::browser), not this file's.
//
// Environment:
//   HARNESS_BROWSER_PROFILE     the profile folder (sign-ins are kept here)
//   HARNESS_BROWSER_EXECUTABLE  a Chrome/Chromium binary; default: installed Google Chrome
//   HARNESS_BROWSER_HEADLESS=1  no window (tests)

import { createInterface } from "node:readline";
import { chromium } from "playwright-core";

const PROFILE = process.env.HARNESS_BROWSER_PROFILE;
const EXECUTABLE = process.env.HARNESS_BROWSER_EXECUTABLE || null;
const HEADLESS = process.env.HARNESS_BROWSER_HEADLESS === "1";
/** Enough of a page for the agent to act on, without flooding it. */
const SNAPSHOT_CHARS = 14000;
const TEXT_CHARS = 12000;

let context = null;
let current = null;

async function browser() {
  if (context) return context;
  if (!PROFILE) throw new Error("HARNESS_BROWSER_PROFILE is not set");
  const options = {
    headless: HEADLESS,
    viewport: null,
    // Without these, some sites (Google sign-in among them) refuse an automated browser.
    ignoreDefaultArgs: ["--enable-automation"],
    args: ["--disable-blink-features=AutomationControlled", "--no-first-run", "--no-default-browser-check"],
  };
  if (EXECUTABLE) options.executablePath = EXECUTABLE;
  else options.channel = "chrome";
  try {
    context = await chromium.launchPersistentContext(PROFILE, options);
  } catch (error) {
    if (!EXECUTABLE && /chrome/i.test(String(error)) && /(not found|install|doesn't exist|executable)/i.test(String(error))) {
      throw new Error("Google Chrome isn't installed. Install it from google.com/chrome and try again.");
    }
    throw error;
  }
  context.on("close", () => {
    context = null;
    current = null;
  });
  context.on("page", (page) => {
    // A link that opens a new tab: follow it there.
    current = page;
    page.on("close", () => {
      if (current === page) current = context?.pages().at(-1) ?? null;
    });
  });
  current = context.pages()[0] ?? (await context.newPage());
  return context;
}

async function page() {
  const ctx = await browser();
  if (!current || current.isClosed()) current = ctx.pages().at(-1) ?? (await ctx.newPage());
  return current;
}

async function settle(p) {
  await p.waitForLoadState("domcontentloaded", { timeout: 10000 }).catch(() => {});
  await p.waitForTimeout(400);
}

async function where(p) {
  return { title: await p.title().catch(() => ""), url: p.url() };
}

function element(p, ref) {
  if (!/^(f\d+)?e\d+$/.test(String(ref || ""))) throw new Error(`"${ref}" is not an element ref like e12 — look at the page first`);
  return p.locator(`aria-ref=${ref}`);
}

/** What an element is, so the harness can decide whether acting on it needs a yes. */
async function describe(p, ref) {
  const handle = ref ? element(p, ref) : p.locator(":focus");
  if (!ref && (await handle.count()) === 0) return { role: "", name: "", tag: "body" };
  // Role and name as the agent saw them, from a fresh snapshot (which also keeps the
  // ref current). A second, non-AI snapshot here would reset the refs.
  let aria = "";
  if (ref) {
    const line = (await snapshot(p)).split("\n").find((l) => l.includes(`[ref=${ref}]`));
    if (!line) throw new Error(`${ref} is no longer on the page — look again`);
    aria = line;
  }
  const facts = await handle.first().evaluate((el) => {
    const form = el.closest("form");
    const searchy = (node) =>
      !!node &&
      (node.getAttribute("role") === "search" ||
        !!node.querySelector("input[type=search], [role=searchbox], [role=search]") ||
        [...node.querySelectorAll("input")].some((i) => /^(q|query|search|keywords?|field-keywords|search_query|s)$/i.test(i.name || "")) ||
        /search/i.test(`${node.getAttribute("action") || ""} ${node.id || ""} ${node.className || ""}`));
    return {
      tag: el.tagName.toLowerCase(),
      input_type: (el.getAttribute("type") || "").toLowerCase(),
      in_form: !!form,
      search: searchy(form) || !!el.closest("[role=search]") || el.getAttribute("type") === "search" || el.getAttribute("role") === "searchbox",
      editable: el.isContentEditable || el.tagName === "TEXTAREA",
      autocomplete: (el.getAttribute("autocomplete") || "").toLowerCase(),
      href: el.closest("a")?.href || "",
    };
  }, null, { timeout: 5000 });
  const match = aria.match(/^\s*-\s*([a-z]+)(?:\s+"((?:[^"\\]|\\.)*)")?/i);
  return { role: match?.[1] ?? "", name: match?.[2] ?? "", ...facts };
}

async function snapshot(p) {
  return p.ariaSnapshot({ mode: "ai", timeout: 15000 });
}

const ops = {
  async status() {
    return { running: !!context };
  },
  async open({ url }) {
    const p = await page();
    let target = String(url || "").trim();
    // The harness only sends http(s); bare addresses get https. Other schemes are for tests.
    if (!/^[a-z][a-z0-9+.-]*:/i.test(target) || /^[^:/]+:\d/.test(target)) target = `https://${target}`;
    await p.goto(target, { waitUntil: "domcontentloaded", timeout: 30000 });
    await settle(p);
    await p.bringToFront().catch(() => {});
    return where(p);
  },
  async look() {
    const p = await page();
    const ctx = await browser();
    let tree = await snapshot(p);
    const truncated = tree.length > SNAPSHOT_CHARS;
    if (truncated) tree = tree.slice(0, SNAPSHOT_CHARS);
    const tabs = await Promise.all(
      ctx.pages().map(async (t, i) => ({ index: i, title: await t.title().catch(() => ""), url: t.url(), current: t === p })),
    );
    return { ...(await where(p)), tabs, snapshot: tree, truncated };
  },
  async find({ text }) {
    const p = await page();
    const needle = String(text || "").toLowerCase();
    if (!needle) throw new Error("find what?");
    const lines = (await snapshot(p)).split("\n");
    const hits = [];
    lines.forEach((line, i) => {
      if (line.toLowerCase().includes(needle)) {
        // The line and the few after it: a product's price and button follow its name.
        hits.push(lines.slice(i, i + 4).join("\n"));
      }
    });
    return { ...(await where(p)), matches: hits.slice(0, 25), count: hits.length };
  },
  async describe({ ref }) {
    const p = await page();
    return { ...(await where(p)), element: await describe(p, ref) };
  },
  async click({ ref }) {
    const p = await page();
    await element(p, ref).click({ timeout: 10000 });
    await settle(await page());
    return where(await page());
  },
  async type({ ref, text, enter }) {
    const p = await page();
    const target = element(p, ref);
    await target.fill(String(text ?? ""), { timeout: 10000 });
    if (enter) {
      await target.press("Enter", { timeout: 10000 });
      await settle(await page());
    }
    return where(await page());
  },
  async press({ key, ref }) {
    const p = await page();
    if (ref) await element(p, ref).press(String(key || "Enter"), { timeout: 10000 });
    else await p.keyboard.press(String(key || "Enter"));
    await settle(await page());
    return where(await page());
  },
  async scroll({ direction }) {
    const p = await page();
    const up = String(direction || "down").toLowerCase() === "up";
    await p.mouse.wheel(0, up ? -700 : 700);
    await p.waitForTimeout(300);
    return where(p);
  },
  async back() {
    const p = await page();
    await p.goBack({ waitUntil: "domcontentloaded", timeout: 15000 }).catch(() => null);
    await settle(p);
    return where(p);
  },
  async read() {
    const p = await page();
    let text = await p.evaluate(() => (document.querySelector("main, [role=main], article") || document.body)?.innerText || "");
    text = text.replace(/\n{3,}/g, "\n\n").trim();
    const truncated = text.length > TEXT_CHARS;
    return { ...(await where(p)), text: truncated ? text.slice(0, TEXT_CHARS) : text, truncated };
  },
  async tab({ index }) {
    const ctx = await browser();
    const target = ctx.pages()[Number(index)];
    if (!target) throw new Error(`there is no tab ${index}`);
    current = target;
    await target.bringToFront().catch(() => {});
    return where(target);
  },
  async close() {
    if (context) await context.close().catch(() => {});
    context = null;
    current = null;
    return {};
  },
};

function reply(message) {
  process.stdout.write(JSON.stringify(message) + "\n");
}

// One request at a time, in order: the harness waits for each answer anyway.
let queue = Promise.resolve();
const lines = createInterface({ input: process.stdin });
lines.on("line", (line) => {
  let request;
  try {
    request = JSON.parse(line);
  } catch {
    return;
  }
  queue = queue.then(async () => {
    const op = ops[request.op];
    if (!op) return reply({ id: request.id, ok: false, error: `unknown op ${request.op}` });
    try {
      reply({ id: request.id, ok: true, ...(await op(request)) });
    } catch (error) {
      const message = String(error?.message || error).split("\n")[0];
      reply({ id: request.id, ok: false, error: message });
    }
  });
});
lines.on("close", async () => {
  await queue.catch(() => {});
  if (context) await context.close().catch(() => {});
  process.exit(0);
});
