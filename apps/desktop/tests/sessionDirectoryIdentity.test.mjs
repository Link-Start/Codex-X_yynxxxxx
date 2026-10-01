import assert from "node:assert/strict";
import { mkdirSync, mkdtempSync, realpathSync, rmSync, statSync, symlinkSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";
import {
  SESSION_FOLDER_UNKNOWN_KEY,
  mergeSessionDirectoryGroups,
  pinnedSessionDirectoryKeys,
  readSessionPinPreferences,
  sessionDirectoryKey,
  sessionPinStorageKey,
  sessionsForDirectoryGroup,
  toggleFolderPins,
} from "../src/sessionDirectoryIdentity.ts";

const sessionIds = (items) => items.map((item) => item.id);

test("case-sensitive directories have unique folder identities and accurate selection", () => {
  const upper = "/case-sensitive/Foo";
  const lower = "/case-sensitive/foo";
  const identities = { [upper]: "verified:upper", [lower]: "verified:lower" };
  const groups = mergeSessionDirectoryGroups([
    [upper, [{ id: "upper-one", cwd: upper }, { id: "upper-two", cwd: upper }]],
    [lower, [{ id: "lower-one", cwd: lower }, { id: "lower-two", cwd: lower }]],
  ], identities);

  assert.equal(groups.length, 2);
  assert.equal(new Set(groups.map((group) => group.folderKey)).size, 2, "React folder keys must be unique");
  assert.notEqual(sessionDirectoryKey(upper, identities), sessionDirectoryKey(lower, identities));
  assert.deepEqual(sessionIds(sessionsForDirectoryGroup(groups, sessionDirectoryKey(upper, identities))), ["upper-one", "upper-two"]);
  assert.deepEqual(sessionIds(sessionsForDirectoryGroup(groups, sessionDirectoryKey(lower, identities))), ["lower-one", "lower-two"]);
});

test("case-distinct directories stay separate before filesystem identities are available", () => {
  const upper = "/offline/Foo";
  const lower = "/offline/foo";
  const groups = mergeSessionDirectoryGroups([
    [upper, [{ id: "upper", cwd: upper }]],
    [lower, [{ id: "lower", cwd: lower }]],
  ]);

  assert.equal(groups.length, 2);
  assert.equal(new Set(groups.map((group) => group.folderKey)).size, 2);
  assert.deepEqual(sessionIds(sessionsForDirectoryGroup(groups, sessionDirectoryKey(lower))), ["lower"]);
  assert.deepEqual(sessionsForDirectoryGroup(groups, "unknown-selection"), []);
});

test("filesystem-verified aliases merge into one folder containing every spelling's sessions", () => {
  const root = mkdtempSync(join(tmpdir(), "codex-x-directory-identity-"));
  try {
    const target = join(root, "Workspace");
    const alias = join(root, "Workspace-alias");
    mkdirSync(target);
    symlinkSync(target, alias, process.platform === "win32" ? "junction" : "dir");
    const verifiedKey = (path) => {
      const metadata = statSync(path, { bigint: true });
      assert(metadata.isDirectory());
      return `verified:${metadata.dev}:${metadata.ino}:${realpathSync.native(path)}`;
    };
    const identities = { [target]: verifiedKey(target), [alias]: verifiedKey(alias) };
    assert.equal(identities[target], identities[alias], "the temporary filesystem must confirm the alias");

    const original = Object.freeze([
      Object.freeze([target, Object.freeze([{ id: "target-one", cwd: target }, { id: "target-two", cwd: target }])]),
      Object.freeze([alias, Object.freeze([{ id: "alias-one", cwd: alias }])]),
    ]);
    const groups = mergeSessionDirectoryGroups(original, identities);
    assert.equal(groups.length, 1);
    assert.equal(new Set(groups.map((group) => group.folderKey)).size, 1);
    assert.equal(groups[0].group, target, "the first folder label is retained");
    for (const spelling of [target, alias]) {
      assert.deepEqual(sessionIds(sessionsForDirectoryGroup(groups, sessionDirectoryKey(spelling, identities))), ["target-one", "target-two", "alias-one"]);
    }
    assert.equal(original[0][1].length, 2, "merging must not mutate parent session arrays");
    assert.equal(original[1][1].length, 1);
    assert.equal(sessionPinStorageKey(target, identities[target]), sessionPinStorageKey(alias, identities[alias]));
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

test("unresolved paths preserve case, backslashes, trailing spaces and roots", () => {
  const paths = [
    "/missing/Foo", "/missing/foo", "/missing/Foo/",
    "/missing/name", "/missing/name ", "/missing/name  ",
    "/missing/with\\backslash", "/missing/with/backslash",
    "/", "//", "C:\\", "C:/", " ",
    SESSION_FOLDER_UNKNOWN_KEY,
  ];
  const keys = paths.map((path) => sessionDirectoryKey(path));
  assert.equal(new Set(keys).size, paths.length);
  for (const path of paths) {
    assert.equal(sessionDirectoryKey(path), sessionDirectoryKey(path, {}));
    assert.notEqual(sessionDirectoryKey(path), SESSION_FOLDER_UNKNOWN_KEY);
  }
  for (const missing of [undefined, null, ""]) {
    assert.equal(sessionDirectoryKey(missing), SESSION_FOLDER_UNKNOWN_KEY);
  }
  assert.notEqual(sessionDirectoryKey("/"), sessionDirectoryKey(null));
});

test("a nonempty raw path cannot borrow an inherited identity-table property", () => {
  const identities = Object.create({ "offline/Foo": "verified:unrelated" });
  assert.equal(sessionDirectoryKey("offline/Foo", identities), sessionDirectoryKey("offline/Foo"));
  assert.notEqual(sessionDirectoryKey("offline/Foo", identities), "verified:unrelated");
});

test("directory pins remain independent for case-distinct folders", () => {
  const identities = { "/work/Foo": "verified:Foo", "/work/foo": "verified:foo" };
  const upper = sessionDirectoryKey("/work/Foo", identities);
  const lower = sessionDirectoryKey("/work/foo", identities);
  let stored = toggleFolderPins([], "/work/Foo", identities);
  assert.deepEqual(stored, ["path:/work/Foo"], "persistent pins must retain the original path");
  assert(pinnedSessionDirectoryKeys(stored, identities).has(upper));
  assert(!pinnedSessionDirectoryKeys(stored, identities).has(lower));
  stored = toggleFolderPins(stored, "/work/foo", identities);
  assert.deepEqual(stored, ["path:/work/Foo", "path:/work/foo"]);
  stored = toggleFolderPins(stored, "/work/Foo", identities);
  assert.deepEqual(stored, ["path:/work/foo"]);
  assert(!pinnedSessionDirectoryKeys(stored, identities).has(upper));
  assert(pinnedSessionDirectoryKeys(stored, identities).has(lower));
  assert.deepEqual(toggleFolderPins(stored, "/work/foo", identities), []);
});

test("persistent raw-path pins survive online, offline and recreated directory identities", () => {
  const path = "/work/Project ";
  const online = { [path]: "verified:original-inode" };
  const recreated = { [path]: "verified:new-inode" };
  const stored = Object.freeze(toggleFolderPins([], path, online));
  assert.deepEqual(stored, ["path:/work/Project "]);
  for (const identities of [online, undefined, {}, recreated]) {
    assert(pinnedSessionDirectoryKeys(stored, identities).has(sessionDirectoryKey(path, identities)));
    assert.deepEqual(toggleFolderPins(stored, path, identities), []);
  }
  assert(!pinnedSessionDirectoryKeys(stored, recreated).has(online[path]));
  assert.deepEqual(stored, ["path:/work/Project "], "resolving or toggling must not mutate the saved pins");
});

test("unpinning a verified alias removes every matching raw-path pin and keeps unrelated pins", () => {
  const target = "/work/Project";
  const alias = "/links/Project";
  const other = "/work/Other";
  const identities = { [target]: "verified:shared", [alias]: "verified:shared", [other]: "verified:other" };
  const stored = Object.freeze(["path:/work/Project", "path:/links/Project", "path:/work/Other"]);
  for (const spelling of [target, alias]) {
    const next = toggleFolderPins(stored, spelling, identities);
    assert.deepEqual(next, ["path:/work/Other"]);
    assert(!pinnedSessionDirectoryKeys(next, identities).has(identities[target]));
    assert(pinnedSessionDirectoryKeys(next, identities).has(identities[other]));
  }
  assert.deepEqual(toggleFolderPins([], alias, identities), ["path:/links/Project"]);
  assert.deepEqual(toggleFolderPins(["path:/work/Project"], alias, identities), []);
  assert.equal(stored.length, 3);
});

test("case-distinct CODEX_HOME values have independent v2 pin storage", () => {
  const upper = "/homes/Foo";
  const lower = "/homes/foo";
  for (const identities of [undefined, { [upper]: "verified:home-upper", [lower]: "verified:home-lower" }]) {
    const upperKey = sessionPinStorageKey(upper, identities?.[upper]);
    const lowerKey = sessionPinStorageKey(lower, identities?.[lower]);
    assert.notEqual(upperKey, lowerKey);
    assert(upperKey.startsWith("codexx.sessionPins.v2:"));
    assert(lowerKey.startsWith("codexx.sessionPins.v2:"));
    const cache = new Map([
      [upperKey, JSON.stringify({ folders: ["upper-folder"], sessions: ["upper-session"] })],
      [lowerKey, JSON.stringify({ folders: ["lower-folder"], sessions: ["lower-session"] })],
    ]);
    const storage = { getItem: (key) => cache.get(key) ?? null };
    assert.deepEqual(readSessionPinPreferences(upperKey, storage), { folders: ["upper-folder"], sessions: ["upper-session"] });
    assert.deepEqual(readSessionPinPreferences(lowerKey, storage), { folders: ["lower-folder"], sessions: ["lower-session"] });
  }
});

test("an unverified home scope falls back to case-preserving paths without borrowing snapshot IDs", () => {
  const upper = "/homes/Foo";
  const lower = "/homes/foo";
  const snapshotIds = { [upper]: "snapshot:reused-file-id", [lower]: "snapshot:reused-file-id" };
  assert.equal(sessionDirectoryKey(upper, snapshotIds), sessionDirectoryKey(lower, snapshotIds));
  const upperKey = sessionPinStorageKey(upper);
  const lowerKey = sessionPinStorageKey(lower);
  assert.notEqual(upperKey, lowerKey, "snapshot identities alone must not merge persistent home scopes");
  assert.equal(sessionPinStorageKey(upper, null), upperKey);
  assert.equal(sessionPinStorageKey(lower, null), lowerKey);
  const cache = new Map([[upperKey, JSON.stringify({ folders: ["path:/work/Foo"], sessions: ["upper-only"] })]]);
  assert.deepEqual(readSessionPinPreferences(lowerKey, { getItem: (key) => cache.get(key) ?? null }), { folders: [], sessions: [] });
});

test("reused physical home IDs with different verified birth scopes have isolated caches", () => {
  const home = "/homes/Project";
  const oldScope = "verified-home:device-1:file-42:birth-100";
  const newScope = "verified-home:device-1:file-42:birth-200";
  const oldKey = sessionPinStorageKey(home, oldScope);
  const newKey = sessionPinStorageKey(home, newScope);
  assert.notEqual(oldKey, newKey);
  const cache = new Map([[oldKey, JSON.stringify({ folders: ["path:/work/old"], sessions: ["old-generation"] })]]);
  const storage = { getItem: (key) => cache.get(key) ?? null };
  assert.deepEqual(readSessionPinPreferences(oldKey, storage), { folders: ["path:/work/old"], sessions: ["old-generation"] });
  assert.deepEqual(readSessionPinPreferences(newKey, storage), { folders: [], sessions: [] });
});

test("ambiguous legacy v1 pins never populate either case's v2 preferences", () => {
  const homes = ["/homes/Foo", "/homes/foo"];
  const legacyKey = "codexx.sessionPins.v1:%2Fhomes%2Ffoo";
  const cache = new Map([[legacyKey, JSON.stringify({ folders: ["/work/foo"], sessions: ["legacy-session"] })]]);
  const reads = [];
  const storage = { getItem: (key) => { reads.push(key); return cache.get(key) ?? null; } };
  for (const home of homes) {
    const currentKey = sessionPinStorageKey(home);
    assert.notEqual(currentKey, legacyKey);
    assert.deepEqual(readSessionPinPreferences(currentKey, storage), { folders: [], sessions: [] });
  }
  assert.equal(new Set(reads).size, 2);
  assert(reads.every((key) => key.startsWith("codexx.sessionPins.v2:")));
  assert(!reads.includes(legacyKey));
});

test("unavailable storage readers return empty pin preferences", () => {
  const storageKey = sessionPinStorageKey("/homes/Project");
  const denied = { getItem() { throw new Error("fixture storage unavailable"); } };
  assert.deepEqual(readSessionPinPreferences(storageKey, denied), { folders: [], sessions: [] });
  const deniedGetter = { get getItem() { throw new Error("fixture storage getter unavailable"); } };
  assert.deepEqual(readSessionPinPreferences(storageKey, deniedGetter), { folders: [], sessions: [] });
});

test("missing-workspace sessions and root-directory sessions select separate folders", () => {
  const groups = mergeSessionDirectoryGroups([
    ["No workspace", [{ id: "missing-one", cwd: null }, { id: "missing-two" }]],
    ["/", [{ id: "root", cwd: "/" }]],
  ]);
  assert.equal(new Set(groups.map((group) => group.folderKey)).size, 2);
  assert.deepEqual(sessionIds(sessionsForDirectoryGroup(groups, SESSION_FOLDER_UNKNOWN_KEY)), ["missing-one", "missing-two"]);
  assert.deepEqual(sessionIds(sessionsForDirectoryGroup(groups, sessionDirectoryKey("/"))), ["root"]);
});

test("missing-workspace and root-directory pins stay independent and persist through identity changes", () => {
  const rootIdentity = { "/": "verified:root" };
  let stored = toggleFolderPins([], null, rootIdentity);
  assert.deepEqual(stored, [SESSION_FOLDER_UNKNOWN_KEY]);
  stored = toggleFolderPins(stored, "/", rootIdentity);
  assert.deepEqual(stored, [SESSION_FOLDER_UNKNOWN_KEY, "path:/"]);
  assert(pinnedSessionDirectoryKeys(stored, rootIdentity).has(SESSION_FOLDER_UNKNOWN_KEY));
  assert(pinnedSessionDirectoryKeys(stored, rootIdentity).has(rootIdentity["/"]));
  assert(pinnedSessionDirectoryKeys(stored).has(sessionDirectoryKey("/")));
  stored = toggleFolderPins(stored, undefined, rootIdentity);
  assert.deepEqual(stored, ["path:/"]);
  assert.deepEqual(toggleFolderPins(stored, "/", rootIdentity), []);
});
