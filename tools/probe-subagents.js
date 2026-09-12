/**
 * Probe live zcode.cjs app-server for session/subagents shapes.
 * Read-only where possible: create session, list, probe methods, close.
 * Does NOT send turns (no plan spend) unless PROBE_SEND=1.
 */
const { spawn } = require("child_process");
const path = require("path");
const readline = require("readline");

const CJS =
  process.env.ZCODE_CJS ||
  path.join(
    process.env.LOCALAPPDATA || "",
    "Programs/ZCode/resources/glm/zcode.cjs"
  );
const WS =
  process.env.ZCODE_WS ||
  "C:/Users/eduar/Desktop/GitHub/CLI-ZCode";

function startServer() {
  return new Promise((resolve, reject) => {
    const child = spawn("node", [CJS, "app-server"], {
      cwd: path.dirname(CJS),
      stdio: ["pipe", "pipe", "pipe"],
    });
    const pending = new Map();
    let nextId = 1;
    const notifications = [];
    const serverRequests = [];
    let stderr = "";

    child.stderr.on("data", (d) => {
      stderr += d.toString();
    });

    const rl = readline.createInterface({ input: child.stdout });
    rl.on("line", (line) => {
      let v;
      try {
        v = JSON.parse(line);
      } catch {
        console.error("NONJSON:", line.slice(0, 200));
        return;
      }
      if (typeof v.id === "number" && pending.has(v.id)) {
        pending.get(v.id)(v);
        pending.delete(v.id);
        return;
      }
      if (typeof v.id === "string" && v.method) {
        serverRequests.push(v);
        // auto-reply for known server requests
        if (v.method === "session/requestRuntimePreferences") {
          write({
            id: v.id,
            result: { nativeSearchEnhancementsEnabled: false },
          });
        } else {
          write({ id: v.id, result: {} });
        }
        return;
      }
      notifications.push(v);
    });

    function write(obj) {
      child.stdin.write(JSON.stringify(obj) + "\n");
    }
    function call(method, params, timeoutMs = 15000) {
      return new Promise((res) => {
        const id = nextId++;
        const t = setTimeout(() => {
          pending.delete(id);
          res({ id, method, params, __timeout: true });
        }, timeoutMs);
        pending.set(id, (v) => {
          clearTimeout(t);
          res(v);
        });
        write({ id, method, params });
      });
    }
    function stop() {
      try {
        child.kill();
      } catch {}
    }

    // wait a bit for boot
    setTimeout(() => resolve({ child, call, stop, notifications, serverRequests, getStderr: () => stderr }), 400);
  });
}

async function main() {
  console.log("spawning", CJS);
  const { call, stop, notifications, serverRequests, getStderr } = await startServer();

  const created = await call("session/create", {
    workspace: { workspacePath: WS, workspaceKey: WS },
  });
  console.log("\n=== session/create ===");
  const sid =
    created?.result?.session?.sessionId ||
    created?.result?.sessionId ||
    null;
  console.log(
    JSON.stringify(
      {
        hasSid: !!sid,
        sid,
        protocol: created?.result?.protocol,
        slashCommands: created?.result?.slashCommands?.map((c) =>
          typeof c === "string" ? c : c.name || c.command || c
        ),
        topKeys: Object.keys(created?.result || {}),
        settingsKeys: Object.keys(created?.result?.settings || {}),
        projectionKeys: Object.keys(created?.result?.projection || {}),
        subagentsField: created?.result?.subagents ?? created?.result?.projection?.subagents,
      },
      null,
      2
    )
  );

  if (!sid) {
    console.error("no session", JSON.stringify(created).slice(0, 500));
    stop();
    process.exit(1);
  }

  // Probe session/subagents with several param shapes
  const probes = [
    ["empty", {}],
    ["sessionId", { sessionId: sid }],
    ["limit", { sessionId: sid, limit: 20 }],
    ["no params object", undefined],
  ];
  console.log("\n=== session/subagents probes ===");
  for (const [label, params] of probes) {
    const r =
      params === undefined
        ? await call("session/subagents", {})
        : await call("session/subagents", params);
    console.log(
      label,
      JSON.stringify({
        error: r?.error,
        resultKeys: r?.result ? Object.keys(r.result) : null,
        result: r?.result,
      }).slice(0, 800)
    );
  }

  // Related methods that might list/launch agents
  const more = [
    ["session/list", { limit: 5 }],
    ["session/cancelBackgroundTask", { sessionId: sid }],
  ];
  console.log("\n=== related ===");
  for (const [m, p] of more) {
    const r = await call(m, p);
    console.log(
      m,
      JSON.stringify({ error: r?.error, result: r?.result }).slice(0, 600)
    );
  }

  // Slash commands that mention agent/subagent
  const sc = created?.result?.slashCommands || [];
  const scList = sc.map((c) => (typeof c === "string" ? c : JSON.stringify(c)));
  console.log("\n=== slashCommands (raw sample) ===");
  console.log(JSON.stringify(scList).slice(0, 2000));

  console.log("\n=== serverRequests seen ===");
  console.log(JSON.stringify(serverRequests.map((r) => r.method)));

  console.log("\n=== notifications sample ===");
  console.log(
    JSON.stringify(
      notifications.slice(0, 15).map((n) => ({ method: n.method, keys: Object.keys(n) }))
    )
  );

  const close = await call("session/close", { sessionId: sid });
  console.log("\n=== session/close ===", JSON.stringify(close.error || close.result).slice(0, 200));

  stop();
}

main().catch((e) => {
  console.error(e);
  process.exit(1);
});
