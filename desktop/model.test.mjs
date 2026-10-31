import { test } from "node:test";
import assert from "node:assert/strict";
import { sampleSpaces, sampleTargets, parseReading } from "./model.js";

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
  const reading = { spaces: [], totals: "cpu 0% · ram 0%", interval_seconds: 5,
    system: { cpu_percent: 25, ram_mb: 8192, total_ram_mb: 16384, text: "cpu 25% · ram 50%" } };
  assert.deepEqual(parseReading({ code: 0, stdout: JSON.stringify(reading) }), reading);
  for (const system of [undefined, {}, { ...reading.system, ram_mb: 20000 },
    { ...reading.system, cpu_percent: 101 }]) {
    assert.throws(() => parseReading({ code: 0, stdout: JSON.stringify({ ...reading, system }) }), /Update/);
  }
});

test("remote spaces use only their device's PIDs and inherit unqualified parent links", () => {
  assert.deepEqual(sampleSpaces(spaces, [
    terminal("t1", 10, "s1"),
    terminal("remote/t1", 20, "s1"),
    terminal("remote/t2", 30, null, { parent: "t1" }),
    terminal("remote/t3", 40, null, { parent: "remote/t2" }),
    terminal("other/t1", 50, "s1"),
  ], "remote"), [{ id: "remote/s1", label: "Server", roots: [20, 30, 40] }]);
});

test("equal PIDs on different devices are kept in separate sampler requests", () => {
  const targets = sampleTargets(spaces, [
    terminal("t1", 10, "s1"), terminal("remote/t1", 10, "s1"),
  ]);
  assert.deepEqual(targets.map(({ machine }) => machine), [null, "remote"]);
  assert.deepEqual(targets[0].input[0].roots, [10]);
  assert.deepEqual(targets[1].input[0], { id: "remote/s1", label: "Server", roots: [10] });
});
