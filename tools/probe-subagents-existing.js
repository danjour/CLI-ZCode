/**
 * Probe session/subagents on an EXISTING session from session/list.
 * Avoids "Session not found" after fresh create (store not materialized).
 */
const { spawn } = require("child_process");
const path = require("path");
const readline = require("readline");

const CJS =
  process.env.ZCODE_CJS ||
  path.join(process.env.LOCALAPPDATA, "Programs/ZCode/resources/glm/zcode.cjs");

function startServer() {
  return new Promise((resolve) => {
    const child = spawn("node", [CJS, "app-server"], {
      cwd: path.dirname(CJS),
      stdio: ["pipe", "pipe", "pipe"],
    });
    const pending = new Map();
    let nextId = 1;
    const rl = readline.createInterface({ input: child.stdout });
    rl.on("line", (line) => {
      let v;
      try {
        v = JSON.parse(line);
      } catch {
        return;
      }
      if (typeof v.id === "number" && pending.has(v.id)) {
        pending.get(v.id)(v);
        pending.delete(v.id);
        return;
      }
      if (typeof v.id === "string" && v.method) {
        if (v.method === "session/requestRuntimePreferences") {
          child.stdin.write(
            JSON.stringify({
              id: v.id,
              result: { nativeSearchEnhancementsEnabled: false },
            }) + "\n"
          );
        } else {
          child.stdin.write(JSON.stringify({ id: v.id, result: {} }) + "\n");
        }
      }
    });
    const call = (method, params) =>
      new Promise((res) => {
        const id = nextId++;
        const t = setTimeout(() => res({ __timeout: true, method, params }), 12000);
        pending.set(id, (v) => {
          clearTimeout(t);
          res(v);
        });
        child.stdin.write(JSON.stringify({ id, method, params }) + "\n");
      });
    setTimeout(
      () => resolve({ call, stop: () => child.kill() }),
      400
    );
  });
}

async function main() {
  const { call, stop } = await startServer();
  const list = await call("session/list", { limit: 5 });
  const sessions = list?.result?.sessions || [];
  console.log("sessions found:", sessions.length);
  if (!sessions.length) {
    console.log("no sessions");
    stop();
    return;
  }

  for (const s of sessions.slice(0, 3)) {
    console.log("\n========", s.sessionId, s.title?.slice(0, 40), "========");
    // resume first (may materialize in store)
    const resumed = await call("session/resume", { sessionId: s.sessionId });
    console.log(
      "resume:",
      resumed?.error?.message || "ok",
      "keys",
      Object.keys(resumed?.result || {})
    );

    const sub = await call("session/subagents", { sessionId: s.sessionId });
    if (sub?.error) {
      console.log("subagents ERROR:", JSON.stringify(sub.error));
    } else {
      console.log("subagents result:", JSON.stringify(sub.result, null, 2).slice(0, 2000));
    }

    // projection might carry subagents
    const proj = await call("session/read", { sessionId: s.sessionId }).catch(() => null);
    // session/read might need more params; also try messages to see subagent parts
    const msgs = await call("session/messages", { sessionId: s.sessionId });
    const raw = JSON.stringify(msgs?.result || msgs?.error || {});
    const hit = raw.match(/.{0,40}subagent.{0,80}/gi);
    console.log("messages subagent hits:", hit ? hit.slice(0, 5) : "none");
  }

  // Also try creating a session, sending a cheap prompt? Too expensive.
  // Try create + wait + subagents
  const WS = "C:/Users/eduar/Desktop/GitHub/CLI-ZCode";
  const created = await call("session/create", {
    workspace: { workspacePath: WS, workspaceKey: WS },
  });
  const sid = created?.result?.session?.sessionId;
  console.log("\n=== fresh create ===", sid);
  await new Promise((r) => setTimeout(r, 1500));
  const sub2 = await call("session/subagents", { sessionId: sid });
  console.log(
    "fresh subagents:",
    JSON.stringify(sub2?.error || sub2.result).slice(0, 800)
  );
  await call("session/close", { sessionId: sid });
  stop();
}

main().catch((e) => {
  console.error(e);
  process.exit(1);
});
