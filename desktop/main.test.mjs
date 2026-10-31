import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { runInNewContext } from "node:vm";
import { sampleTargets, parseReading } from "./model.js";

const source = readFileSync(new URL("./main.js", import.meta.url), "utf8")
  .replace(/^import .*;\n/gm, "")
  .replace("export default class", "class") + "\nSpaceUsage;";

function reading(text) {
  return { spaces: [], totals: "Herdr processes only", interval_seconds: 5,
    system: { text, cpu_percent: 25, ram_mb: 8192, total_ram_mb: 16384 } };
}

function compactView(props = { compact: true }, overrides = {}) {
  const text = [];
  const actions = [];
  let click;
  function element() {
    const node = new Proxy({}, {
      get: (_, method) => (...args) => {
        if (method === "child" && typeof args[0] === "string") text.push(args[0]);
        if (method === "on_click") click = args[0];
        if (method === "when" && args[0]) args[1](node);
        return node;
      },
    });
    return node;
  }
  const View = runInNewContext(source, {
    View: class {}, div: element, h_flex: element, v_flex: element,
    props: () => props, theme: () => ({}), spaces: () => [], terminals: () => [],
    appHome: () => "/app", sampleTargets, parseReading,
    run: (command, args) => actions.push({ command, ...args }),
    ...overrides,
  });
  const view = new View();
  view.devices = new Map();
  return { view, text, actions, click: () => click() };
}

function setReading(view, machine, text, error = null) {
  view.devices.set(machine, { reading: text ? reading(text) : null, error });
}

function context() {
  const tasks = [];
  const cx = { timer: { every() {} }, notify() {}, spawn(task) { tasks.push(task(cx)); } };
  return { cx, tasks };
}

function deferred() {
  let resolve;
  const promise = new Promise((done) => { resolve = done; });
  return { promise, resolve: (text) => resolve({ code: 0, stdout: JSON.stringify(reading(text)) }) };
}

test("the full overlay accepts missing props from the native mod checker", () => {
  const ui = compactView(null);
  ui.view.render();
  assert.ok(ui.text.includes("SPACE USAGE"));
});

test("status bar shows whole-device readings rather than Herdr process totals", () => {
  const ui = compactView();
  setReading(ui.view, null, "cpu 25% · ram 50% · bat 100%");
  ui.view.render();
  assert.deepEqual(ui.text, ["This computer · cpu 25% · ram 50% · bat 100%"]);
  ui.click();
  assert.deepEqual(ui.actions, [{ command: "mod.open", name: "space-usage" }]);
});

test("status bar reports the first sample as pending on the selected device", () => {
  const ui = compactView();
  ui.view.render();
  assert.deepEqual(ui.text, ["This computer · Measuring…"]);
});

test("status bar reports a failed sample instead of stale readings", () => {
  const ui = compactView();
  setReading(ui.view, null, "cpu 25% · ram 50%", "sampler failed");
  ui.view.render();
  assert.deepEqual(ui.text, ["This computer · Usage unavailable"]);
  ui.click();
  assert.equal(ui.actions[0].command, "mod.open");
});

test("switching devices immediately selects that device's cached reading", () => {
  let machine = "nixos-amd";
  const ui = compactView({ compact: true }, {
    spaces: () => [{ id: "s1", active: true, machine }],
  });
  setReading(ui.view, null, "cpu 12% · ram 75%");
  setReading(ui.view, "nixos-amd", "cpu 64% · ram 32%");
  ui.view.render();
  assert.deepEqual(ui.text, ["nixos-amd · cpu 64% · ram 32%"]);
  machine = null;
  ui.text.length = 0;
  ui.view.render();
  assert.deepEqual(ui.text, ["This computer · cpu 12% · ram 75%"]);
});

test("a device without a sample never displays another device's reading", () => {
  const ui = compactView({ compact: true }, {
    spaces: () => [{ id: "remote/s1", active: true, machine: "remote" }],
  });
  setReading(ui.view, null, "cpu 12% · ram 75%");
  ui.view.render();
  assert.deepEqual(ui.text, ["remote · Measuring…"]);
});

test("the full list uses the reading from each space's owning device", () => {
  const ui = compactView(null, {
    spaces: () => [
      { id: "s1", name: "Mac project", active: true, machine: null },
      { id: "remote/s1", name: "Remote project", machine: "remote" },
    ],
  });
  setReading(ui.view, null, "cpu 12% · ram 75%");
  setReading(ui.view, "remote", "cpu 64% · ram 32%");
  ui.view.devices.get(null).reading.spaces = [{ workspace_id: "s1", text: "cpu 2% · ram 1%" }];
  ui.view.devices.get("remote").reading.spaces = [{ workspace_id: "remote/s1", text: "cpu 8% · ram 4%" }];
  ui.view.render();
  assert.ok(ui.text.includes("cpu 2% · ram 1%"));
  assert.ok(ui.text.includes("cpu 8% · ram 4%"));
});

test("late local replies cannot overwrite the selected remote device", async () => {
  const local = deferred();
  const remote = deferred();
  const calls = [];
  const ui = compactView({ compact: true }, {
    spaces: () => [{ id: "remote/s1", active: true, machine: "remote" }],
    exec: (command, args, options) => {
      calls.push({ command, on: options.on, input: JSON.parse(args[1]) });
      return options.on ? remote.promise : local.promise;
    },
  });
  const { cx, tasks } = context();
  ui.view.init({}, cx);
  ui.view.refresh(cx);
  assert.equal(calls.length, 2, "one in-flight request per device");
  assert.equal(calls[0].on, null);
  assert.match(calls[0].command, /^\/app\/mods\/space-usage\//);
  assert.equal(calls[1].command, "space-usage");
  assert.equal(calls[1].on, "remote");
  assert.equal(calls[1].input[0].id, "remote/s1");
  remote.resolve("cpu 64% · ram 32%");
  await tasks[1];
  local.resolve("cpu 12% · ram 75%");
  await tasks[0];
  ui.view.render();
  assert.deepEqual(ui.text, ["remote · cpu 64% · ram 32%"]);
});

test("an offline remote device fails separately while local readings keep working", async () => {
  let machine = "remote";
  const ui = compactView({ compact: true }, {
    spaces: () => [{ id: "remote/s1", active: true, machine }],
    exec: async (_command, _args, options) => {
      if (options.on) throw new Error("Device offline");
      return { code: 0, stdout: JSON.stringify(reading("cpu 12% · ram 75%")) };
    },
  });
  const { cx, tasks } = context();
  ui.view.init({}, cx);
  await Promise.all(tasks);
  ui.view.render();
  assert.deepEqual(ui.text, ["remote · Usage unavailable"]);
  machine = null;
  ui.text.length = 0;
  ui.view.render();
  assert.deepEqual(ui.text, ["This computer · cpu 12% · ram 75%"]);
});
