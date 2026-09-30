import assert from "node:assert/strict";
import test from "node:test";
import { applyProviderRowOrder, moveProviderRow, orderProviderRows, providerRowKey } from "../src/providerRowOrder.ts";

const official = { id: "official", isCurrent: false };
const detected = { id: "detected", isCurrent: true };

test("a copied provider is last while the selected saved ID stays current", () => {
  const rows = orderProviderRows(official, [detected], [
    { id: "older", isCurrent: false },
    { id: "selected", isCurrent: true },
    { id: "copy", isCurrent: false },
  ]);

  assert.deepEqual(rows.map((row) => row.id), ["official", "older", "selected", "copy"]);
  assert.deepEqual(rows.filter((row) => row.isCurrent).map((row) => row.id), ["selected"]);
});

test("an unresolved detected current remains visible before saved rows", () => {
  const rows = orderProviderRows(official, [detected], [
    { id: "original", isCurrent: false },
    { id: "copy", isCurrent: false },
  ]);

  assert.deepEqual(rows.map((row) => row.id), ["official", "detected", "original", "copy"]);
  assert.deepEqual(rows.filter((row) => row.isCurrent).map((row) => row.id), ["detected"]);
});

test("custom order crosses official/local boundaries and appends new cards", () => {
  const rows = [{ source: "official", id: "default" }, { source: "local", id: "one" }, { source: "local", id: "new" }];
  assert.deepEqual(applyProviderRowOrder(rows, ["local:one", "official:default"]).map(providerRowKey), ["local:one", "official:default", "local:new"]);
});

test("missing and duplicate stored keys never hide or duplicate cards", () => {
  const rows = [{ source: "official", id: "same" }, { source: "local", id: "same" }];
  assert.deepEqual(applyProviderRowOrder(rows, ["detected:missing", "local:same", "local:same"]).map(providerRowKey), ["local:same", "official:same"]);
});

test("moving up and down inserts at the intended edge without losing entries", () => {
  const original = ["official:default", "local:one", "local:two", "local:three"];
  assert.deepEqual(moveProviderRow(original, "local:three", "official:default", "before"), ["local:three", "official:default", "local:one", "local:two"]);
  assert.deepEqual(moveProviderRow(original, "official:default", "local:three", "after"), ["local:one", "local:two", "local:three", "official:default"]);
  assert.deepEqual(moveProviderRow(original, "local:one", "local:two", "before"), original);
  assert.deepEqual(moveProviderRow(original, "missing", "local:one", "after"), original);
  assert.deepEqual(original, ["official:default", "local:one", "local:two", "local:three"]);
});
