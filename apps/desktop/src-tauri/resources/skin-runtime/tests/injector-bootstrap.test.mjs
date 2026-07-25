import assert from "node:assert/strict";
import fs from "node:fs/promises";
import path from "node:path";
import vm from "node:vm";
import { fileURLToPath } from "node:url";
import {
  earlyPayloadFor,
  probeSession,
  verifySession,
  waitForCodexProbe,
} from "../scripts/injector.mjs";

const here = path.dirname(fileURLToPath(import.meta.url));
const injectorPath = path.resolve(here, "../scripts/injector.mjs");
const source = await fs.readFile(injectorPath, "utf8");

function createFixture(protocol = "app:") {
  const observers = [];
  const timers = new Map();
  let nextTimer = 1;
  const markers = { shell: false, sidebar: false, home: false, settings: false, themePreview: false };
  const context = {
    window: { installs: [] },
    location: { protocol },
    document: {
      documentElement: {},
      querySelector(selector) {
        if (selector === "main.main-surface") return markers.shell ? {} : null;
        if (selector === "aside.app-shell-left-panel") return markers.sidebar ? {} : null;
        if (selector === '[role="main"]:has([data-testid="home-icon"])') return markers.home ? {} : null;
        if (selector === 'input[name="appearance-theme"]') return markers.settings ? {} : null;
        if (selector === '[data-testid="theme-preview"]') return markers.themePreview ? {} : null;
        return null;
      },
    },
    MutationObserver: class {
      constructor(callback) {
        this.callback = callback;
        this.connected = true;
        observers.push(this);
      }
      observe() {}
      disconnect() { this.connected = false; }
    },
    setTimeout(callback) {
      const id = nextTimer++;
      timers.set(id, callback);
      return id;
    },
    clearTimeout(id) { timers.delete(id); },
  };
  return { context, markers, observers };
}

const guarded = createFixture();
vm.runInNewContext(earlyPayloadFor('window.installs.push("guarded")', "guarded"), guarded.context);
assert.deepEqual(guarded.context.window.installs, [], "Auxiliary app targets must remain untouched.");
guarded.markers.shell = true;
guarded.observers[0].callback([]);
assert.deepEqual(guarded.context.window.installs, [], "A main surface without the Codex sidebar is not sufficient.");

const home = createFixture();
home.markers.home = true;
vm.runInNewContext(earlyPayloadFor('window.installs.push("home")', "home"), home.context);
assert.deepEqual(home.context.window.installs, ["home"], "The explicit Codex home route must support early injection.");

const settings = createFixture();
settings.markers.settings = true;
vm.runInNewContext(earlyPayloadFor('window.installs.push("settings")', "settings"), settings.context);
assert.deepEqual(
  settings.context.window.installs,
  ["settings"],
  "The Codex appearance route must support early injection.",
);

const web = createFixture("https:");
web.markers.shell = true;
web.markers.sidebar = true;
vm.runInNewContext(earlyPayloadFor('window.installs.push("web")', "web"), web.context);
assert.deepEqual(web.context.window.installs, [], "Web pages must remain untouched even with spoofed shell markers.");

const generations = createFixture();
vm.runInNewContext(earlyPayloadFor('window.installs.push("old")', "old"), generations.context);
vm.runInNewContext(earlyPayloadFor('window.installs.push("new")', "new"), generations.context);
generations.markers.shell = true;
generations.markers.sidebar = true;
for (const observer of generations.observers) observer.callback([]);
assert.deepEqual(
  generations.context.window.installs,
  ["new"],
  "A stale early script must yield to the newest watcher generation.",
);
assert.equal(generations.context.window.__CODEX_DREAM_SKIN_EARLY_APPLIED__, "new");

function createProbeSession(frames) {
  let callCount = 0;
  return {
    get callCount() { return callCount; },
    async evaluate(expression) {
      const frame = frames[Math.min(callCount, frames.length - 1)];
      callCount += 1;
      if (frame instanceof Error) throw frame;
      const marker = (key) => frame[key] ? {} : null;
      return vm.runInNewContext(expression, {
        location: { protocol: frame.protocol ?? "app:", href: `${frame.protocol ?? "app:"}//codex` },
        document: {
          title: "Codex",
          querySelector(selector) {
            if (selector === "main.main-surface") return marker("shell");
            if (selector === "aside.app-shell-left-panel") return marker("sidebar");
            if (selector === ".composer-surface-chrome") return marker("composer");
            if (selector === '[role="main"]:has([data-testid="home-icon"])') return marker("home");
            if (selector === 'input[name="appearance-theme"]') return marker("settings");
            if (selector === '[data-testid="theme-preview"]') return marker("themePreview");
            return null;
          },
        },
      });
    },
  };
}

assert.equal((await probeSession(createProbeSession([{ shell: true, sidebar: true }]))).codex, true);
assert.equal((await probeSession(createProbeSession([{ home: true }]))).codex, true);
assert.equal((await probeSession(createProbeSession([{ settings: true }]))).codex, true);
assert.equal((await probeSession(createProbeSession([{}]))).codex, false, "An unmarked app page must be rejected.");
assert.equal(
  (await probeSession(createProbeSession([{ protocol: "https:", shell: true, sidebar: true }]))).codex,
  false,
  "A non-app page must be rejected even when it spoofs the standard shell.",
);

const delayed = createProbeSession([
  new Error("Execution context was destroyed"),
  {},
  { home: true },
]);
assert.equal((await waitForCodexProbe(delayed, 300))?.codex, true);
assert.equal(delayed.callCount, 3, "Probe waiting must survive document replacement and delayed DOM markers.");

function createVerificationSession(route) {
  const node = (width = 640, height = 480) => ({
    getBoundingClientRect: () => ({ x: 0, y: 0, width, height }),
    querySelector: () => null,
  });
  const body = node();
  const homeHero = node(560, 240);
  const home = node();
  home.firstElementChild = { firstElementChild: { firstElementChild: homeHero } };
  const homeIndicator = { closest: () => home };
  const settingsAnchor = node(320, 48);
  const chrome = node();
  return {
    async evaluate(expression) {
      return vm.runInNewContext(expression, {
        innerWidth: 1280,
        innerHeight: 800,
        window: { __CODEX_DREAM_SKIN_STATE__: { version: "1.2.4" } },
        getComputedStyle(target) {
          return { display: "block", visibility: "visible", pointerEvents: target === chrome ? "none" : "auto" };
        },
        document: {
          body,
          documentElement: {
            classList: { contains: () => true },
            scrollWidth: 1280,
            clientWidth: 1280,
            scrollHeight: 800,
            clientHeight: 800,
          },
          getElementById(id) {
            if (id === "codex-dream-skin-style") return {};
            if (id === "codex-dream-skin-chrome" && route === "home") return chrome;
            return null;
          },
          querySelector(selector) {
            if (selector === '[data-testid="home-icon"]' && route === "home") return homeIndicator;
            if (selector === '[role="main"].dream-skin-home' && route === "home") return home;
            if (selector === 'input[name="appearance-theme"]' && route === "settings") return settingsAnchor;
            return null;
          },
        },
      });
    },
  };
}

assert.equal(
  (await verifySession(createVerificationSession("home"))).pass,
  true,
  "A verified Codex home route must not require the legacy shell/sidebar pair.",
);
assert.equal(
  (await verifySession(createVerificationSession("settings"))).pass,
  true,
  "A verified appearance route must accept the installed root theme before the main shell mounts.",
);
assert.equal(
  (await verifySession(createVerificationSession("auxiliary"))).pass,
  false,
  "An auxiliary app page must not pass post-install verification.",
);

const discoveryStart = source.indexOf("record.earlyScriptId = await registerEarly");
const probeStart = source.indexOf("const probe = await waitForCodexProbe", discoveryStart);
assert.ok(discoveryStart >= 0 && probeStart > discoveryStart, "Early registration must happen before full shell probing.");
assert.match(
  source,
  /connectCodexTargets[\s\S]*const probe = await waitForCodexProbe/,
  "One-shot discovery must wait for a progressively loaded Codex renderer.",
);
assert.match(
  source,
  /finally\s*\{[\s\S]*Promise\.all\(\[\.\.\.sessions\.values\(\)\][\s\S]*removeEarly\(record\)/,
  "Watcher shutdown must unregister persistent Page scripts before closing CDP sessions.",
);
assert.match(
  source,
  /const earlyApplied = await session\.evaluate\([\s\S]*if \(!earlyApplied\) \{[\s\S]*applyToSession/,
  "The watcher must not run the full payload twice after a successful early install.",
);
assert.match(
  source,
  /await runOneShot\(options\);[\s\S]{0,160}await flushStandardStreams\(\);[\s\S]{0,100}process\.exit\(process\.exitCode \?\? 0\)/,
  "One-shot commands must exit after flushing output so a lingering CDP close handshake cannot block Codex-X.",
);

console.log("PASS: renderer probing is route-aware, delayed-load safe, generation-safe, and guarded.");
