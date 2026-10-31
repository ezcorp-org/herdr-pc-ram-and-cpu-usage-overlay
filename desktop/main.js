import { View, div } from "gpui-kit";
import { h_flex, v_flex } from "gpui-base";
import { appHome, exec, props, run, spaces, terminals, theme } from "herdr";
import { sampleTargets, parseReading } from "./model.js";

export default class SpaceUsage extends View {
  init(_props, cx) {
    this.devices = new Map();
    cx.timer.every(1000, (cx) => this.refresh(cx));
    this.refresh(cx);
  }

  refresh(cx) {
    for (const { machine, input } of sampleTargets(spaces(), terminals())) {
      let state = this.devices.get(machine);
      if (!state) {
        state = { reading: null, error: null, busy: false, nextRefresh: 0 };
        this.devices.set(machine, state);
      }
      if (state.busy || Date.now() < state.nextRefresh) continue;
      state.busy = true;
      cx.spawn(async (cx) => {
        try {
          // Each sampler runs on the device that owns its process IDs.
          const command = machine ? "space-usage" : `${appHome()}/mods/space-usage/@SPACE_USAGE_BINARY@`;
          state.reading = parseReading(await exec(command,
            ["--desktop-json", JSON.stringify(input)], { on: machine }));
          state.error = null;
        } catch (error) {
          state.reading = null;
          state.error = String(error.message || error);
        } finally {
          state.busy = false;
          const interval = Math.max(1, Number(state.reading?.interval_seconds) || 5);
          state.nextRefresh = Date.now() + interval * 1000;
          cx.notify();
        }
      });
    }
  }

  render() {
    const t = theme();
    const list = spaces();
    const machine = list.find((space) => space.active)?.machine || null;
    const name = machine || "This computer";
    const state = this.devices?.get(machine);
    const status = `${name} · ${state?.error ? "Usage unavailable" : state?.reading?.system.text || "Measuring…"}`;
    if (props()?.compact) {
      return h_flex().id("space-usage-status").size_full().items_center().justify_end().px(12)
        .bg(t.chrome).border_t(1).border_color(t.border).text_size(11.8).text_color(t.muted)
        .hover((style) => style.bg(t.raised))
        .on_click(() => run("mod.open", { name: "space-usage" }))
        .child(div().truncate().child(status));
    }
    const readings = new Map([...(this.devices?.values() || [])]
      .flatMap((device) => device.reading?.spaces || []).map((row) => [row.workspace_id, row]));
    return v_flex().size_full().bg(t.surface).text_color(t.foreground).text_size(12).p(12).gap(8)
      .child(h_flex().gap(12).items_center()
        .child(div().child("SPACE USAGE"))
        .child(div().flex_1().truncate().text_color(t.muted).child(status)))
      .when(Boolean(state?.error), (view) => view.child(div().text_color(t.muted).child(state.error)))
      .child(v_flex().id("space-usage-list").flex_1().overflow_y_scroll().gap(4).children(
        list.map((space) =>
          h_flex().id(`usage-${space.id}`).gap(12).p(6).rounded(4).items_center()
            .bg(space.active ? t.raised : t.surface).hover((style) => style.bg(t.raised))
            .on_click(() => run("space.select", { id: space.id }))
            .child(div().flex_1().min_w(0).truncate().child(space.name))
            .child(div().max_w(100).truncate().text_color(t.dim).child(space.machine || "This computer"))
            .child(div().max_w(180).truncate().text_color(t.dim).child(space.branch || ""))
            .child(div().flex_shrink_0().text_color(t.muted).child(readings.get(space.id)?.text
              || (this.devices?.get(space.machine || null)?.error ? "Unavailable" : "Measuring…"))),
        ),
      ));
  }
}
