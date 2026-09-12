/** Minimal: create, send short prompt, poll messages. */
const { spawn } = require("child_process");
const path = require("path");
const readline = require("readline");
const CJS = path.join(process.env.LOCALAPPDATA, "Programs/ZCode/resources/glm/zcode.cjs");
const WS = "C:/Users/eduar/Desktop/GitHub/CLI-ZCode";

const child = spawn("node", [CJS, "app-server"], { cwd: path.dirname(CJS), stdio: ["pipe", "pipe", "pipe"] });
const pending = new Map();
let nextId = 1;
const events = [];
child.stderr.on("data", (d) => process.stderr.write("[err] " + d));
const rl = readline.createInterface({ input: child.stdout });
rl.on("line", (line) => {
  let v; try { v = JSON.parse(line); } catch { console.log("NONJSON", line.slice(0,150)); return; }
  if (typeof v.id === "number" && pending.has(v.id)) { pending.get(v.id)(v); pending.delete(v.id); return; }
  if (typeof v.id === "string" && v.method) {
    if (v.method === "session/requestRuntimePreferences")
      child.stdin.write(JSON.stringify({ id: v.id, result: { nativeSearchEnhancementsEnabled: false } }) + "\n");
    else child.stdin.write(JSON.stringify({ id: v.id, result: {} }) + "\n");
    return;
  }
  if (v.method) events.push(v.method);
});
const call = (m, p) => new Promise((res) => {
  const id = nextId++; const t = setTimeout(() => res({ __timeout: true, m }), 30000);
  pending.set(id, (v) => { clearTimeout(t); res(v); });
  child.stdin.write(JSON.stringify({ id, method: m, params: p }) + "\n");
});

(async () => {
  await new Promise((r) => setTimeout(r, 500));
  const c = await call("session/create", { workspace: { workspacePath: WS, workspaceKey: WS } });
  const sid = c.result.session.sessionId;
  console.log("sid", sid);
  const s = await call("session/send", { sessionId: sid, content: "Diga apenas: ok" });
  console.log("send", s.error || "accepted", Object.keys(s.result||{}));
  for (let i = 0; i < 20; i++) {
    await new Promise((r) => setTimeout(r, 2000));
    const msgs = await call("session/messages", { sessionId: sid });
    const list = msgs.result?.messages || [];
    const last = [...list].reverse().find((m) => (m.role || m.info?.role) === "assistant");
    console.log(i, "msgs", list.length, "last", last ? JSON.stringify(last).slice(0,200) : "none");
    if (last) break;
  }
  console.log("events", events.slice(0, 20));
  await call("session/close", { sessionId: sid });
  child.kill();
  process.exit(0);
})();
