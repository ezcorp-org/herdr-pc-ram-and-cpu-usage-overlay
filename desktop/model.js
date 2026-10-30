// Keep machine-local PIDs separate: a remote PID can name an unrelated local
// process. Sub-agents without space labels inherit their parent's space.
export function sampleSpaces(spaces, terminals) {
  const local = spaces.filter((space) => !space.machine);
  const ids = new Set(local.map((space) => space.id));
  const byAddress = new Map(terminals.map((terminal) => [terminal.address, terminal]));
  const roots = new Map(local.map((space) => [space.id, new Set()]));
  for (const terminal of terminals) {
    if (terminal.machine || terminal.address?.includes("/") || terminal.state !== "live") continue;
    if (!Number.isInteger(terminal.pid) || terminal.pid <= 0) continue;
    let current = terminal;
    const seen = new Set();
    while (current && !seen.has(current.address)) {
      seen.add(current.address);
      const id = current.labels?.["herdr.space"];
      if (id) {
        if (ids.has(id)) roots.get(id).add(terminal.pid);
        break;
      }
      current = byAddress.get(current.parent);
    }
  }
  return local.map((space) => ({ id: space.id, label: space.name, roots: [...roots.get(space.id)] }));
}

export function parseReading(result) {
  if (result.code !== 0) throw new Error(result.stderr.trim() || "Cannot read space usage.");
  const reading = JSON.parse(result.stdout);
  if (!Array.isArray(reading.spaces) || typeof reading.totals !== "string") {
    throw new Error("Invalid space usage response.");
  }
  return reading;
}
