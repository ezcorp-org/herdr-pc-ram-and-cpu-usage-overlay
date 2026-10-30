import { test } from "node:test";
import assert from "node:assert/strict";
import { sampleSpaces, parseReading } from "./model.js";

const spaces = [
  { id: "s1", name: "Project" },
  { id: "s2", name: "Empty", machine: null },
  { id: "remote/s1", name: "Server", machine: "remote" },
];

function terminal(address, pid, space, extra = {}) {
  return { address, pid, state: "live", labels: space ? { "herdr.space": space } : {}, ...extra };
}

test("sample local spaces, deduplicate PIDs, and include detached sub-agents", () => {
  assert.deepEqual(sampleSpaces(spaces, [
    terminal("t1", 10, "s1"),
    terminal("t2", 10, "s1"),
    terminal("t3", 30, null, { parent: "t1" }),
    terminal("t4", 40, null, { parent: "t3" }),
    terminal("t5", 50, "other-app"),
  ]), [
    { id: "s1", label: "Project", roots: [10, 30, 40] },
    { id: "s2", label: "Empty", roots: [] },
  ]);
});

test("remote, finished, and invalid PIDs cannot become local roots", () => {
  const result = sampleSpaces(spaces, [
    terminal("remote/t1", 10, "s1"),
    terminal("t2", 20, "s1", { machine: "remote" }),
    terminal("t3", 30, "s1", { state: "finished" }),
    ...[null, undefined, 0, -1, 1.5, "50"].map((pid, i) => terminal(`invalid-${i}`, pid, "s1")),
  ]);
  assert.ok(result.every((space) => space.roots.length === 0));
});

test("parent cycles and missing parents do not block a refresh", () => {
  const result = sampleSpaces(spaces, [
    terminal("t1", 10, null, { parent: "t2" }),
    terminal("t2", 20, null, { parent: "t1" }),
    terminal("t3", 30, null, { parent: "missing" }),
  ]);
  assert.ok(result.every((space) => space.roots.length === 0));
});

test("failed samples show an error instead of stale or false zero readings", () => {
  assert.throws(() => parseReading({ code: 1, stderr: "  Access denied\n" }), /Access denied/);
  assert.throws(() => parseReading({ code: 0, stdout: "not JSON" }));
  assert.throws(() => parseReading({ code: 0, stdout: "{}" }), /Invalid space usage/);
  const reading = { spaces: [], totals: "cpu 0% · ram 0%", interval_seconds: 5 };
  assert.deepEqual(parseReading({ code: 0, stdout: JSON.stringify(reading) }), reading);
});
