import { View, div } from "gpui-kit";
import { h_flex, v_flex } from "gpui-base";
import { appHome, exec, run, spaces, terminals, theme } from "herdr";
import { sampleSpaces, parseReading } from "./model.js";

export default class SpaceUsage extends View {
  init(_props, cx) {
    this.reading = null;
    this.error = null;
    this.busy = false;
    this.nextRefresh = 0;
    cx.timer.every(1000, (cx) => {
      if (Date.now() >= this.nextRefresh) this.refresh(cx);
    });
    this.refresh(cx);
  }

  refresh(cx) {
    if (this.busy) return;
    this.busy = true;
    cx.spawn(async (cx) => {
      try {
        const input = sampleSpaces(spaces(), terminals());
        this.reading = parseReading(await exec(
          `${appHome()}/mods/space-usage/@SPACE_USAGE_BINARY@`,
          ["--desktop-json", JSON.stringify(input)],
        ));
        this.error = null;
      } catch (error) {
        this.reading = null;
        this.error = String(error.message || error);
      } finally {
        this.busy = false;
        const interval = Math.max(1, Number(this.reading?.interval_seconds) || 5);
        this.nextRefresh = Date.now() + interval * 1000;
        cx.notify();
      }
    });
  }

  render() {
    const t = theme();
    const readings = new Map((this.reading?.spaces || []).map((row) => [row.workspace_id, row]));
    return v_flex().size_full().bg(t.surface).text_color(t.foreground).text_size(12).p(12).gap(8)
      .child(h_flex().gap(12)
        .child(div().child("SPACE USAGE"))
        .child(div().text_color(t.muted).child(this.reading?.totals || "")))
      .child(v_flex().id("space-usage-list").flex_1().overflow_y_scroll().gap(4).children(
        this.error
          ? [div().text_color(t.muted).child(this.error)]
          : spaces().map((space) =>
            h_flex().id(`usage-${space.id}`).gap(12).p(6).rounded(4)
              .bg(space.active ? t.raised : t.surface).hover((style) => style.bg(t.raised))
              .on_click(() => run("space.select", { id: space.id }))
              .child(div().flex_1().child(space.name))
              .child(div().text_color(t.dim).child(space.branch || ""))
              .child(div().text_color(t.muted).child(
                space.machine ? "Remote · not sampled" : readings.get(space.id)?.text || "Measuring…",
              )),
          ),
      ));
  }
}
