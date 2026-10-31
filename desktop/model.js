// Keep machine-local PIDs separate: a remote PID can name an unrelated local
// process. Sub-agents without space labels inherit their parent's space.
export function sampleSpaces(spaces, terminals, machine = null) {
  const onDevice = spaces.filter((space) => (space.machine || null) === machine);
  const unqualified = (id) => machine && id?.startsWith(`${machine}/`) ? id.slice(machine.length + 1) : id;
  const ids = new Map(onDevice.map((space) => [unqualified(space.id), space.id]));
  const onMachine = terminals.filter((terminal) => {
    const prefix = terminal.address?.includes("/") ? terminal.address.split("/")[0] : null;
    return (terminal.machine || prefix) === machine && (!prefix || prefix === machine);
  });
  const byAddress = new Map(onMachine.map((terminal) => [unqualified(terminal.address), terminal]));
  const roots = new Map(onDevice.map((space) => [space.id, new Set()]));
  for (const terminal of onMachine) {
    if (terminal.state !== "live") continue;
    if (!Number.isInteger(terminal.pid) || terminal.pid <= 0) continue;
    let current = terminal;
    const seen = new Set();
    while (current && !seen.has(current.address)) {
      seen.add(current.address);
      const id = unqualified(current.labels?.["herdr.space"]);
      if (id) {
        if (ids.has(id)) roots.get(ids.get(id)).add(terminal.pid);
        break;
      }
      current = byAddress.get(unqualified(current.parent));
    }
  }
  return onDevice.map((space) => ({ id: space.id, label: space.name, roots: [...roots.get(space.id)] }));
}

export function sampleTargets(spaces, terminals) {
  return [...new Set([null, ...spaces.map((space) => space.machine || null)])]
    .map((machine) => ({ machine, input: sampleSpaces(spaces, terminals, machine) }));
}

export function parseReading(result) {
  if (result.code !== 0) throw new Error(result.stderr.trim() || "Cannot read space usage.");
  const reading = JSON.parse(result.stdout);
  if (!Array.isArray(reading.spaces) || typeof reading.totals !== "string") {
    throw new Error("Invalid space usage response.");
  }
  const system = reading.system;
  if (!system || typeof system.text !== "string" || !Number.isFinite(system.cpu_percent)
      || system.cpu_percent < 0 || system.cpu_percent > 100 || !Number.isFinite(system.ram_mb)
      || !Number.isFinite(system.total_ram_mb) || system.total_ram_mb <= 0
      || system.ram_mb < 0 || system.ram_mb > system.total_ram_mb) {
    throw new Error("Device readings unavailable. Update space-usage on this device.");
  }
  return reading;
}
