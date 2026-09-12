/** Dump full assistant message shape after send. */
const { spawn } = require("child_process");
const path = require("path");
const readline = require("readline");
const CJS = path.join(process.env.LOCALAPPDATA, "Programs/ZCode/resources/glm/zcode.cjs");
const WS = "C:/Users/eduar/Desktop/GitHub/CLI-ZCode";
const child = spawn("node", [CJS, "app-server"], { cwd: path.dirname(CJS), stdio: ["pipe", "pipe", "pipe"] });
const pending = new Map(); let nextId = 1;
const rl = readline.createInterface({ input: child.stdout });
rl.on("line", (line) => {
  let v; try { v = JSON.parse(line); } catch { return; }
  if (typeof v.id === "number" && pending.has(v.id)) { pending.get(v.id)(v); pending.delete(v.id); return; }
  if (typeof v.id === "string" && v.method) {
    if (v.method === "session/requestRuntimePreferences")
      child.stdin.write(JSON.stringify({ id: v.id, result: { nativeSearchEnhancementsEnabled: false } }) + "\n");
    else child.stdin.write(JSON.stringify({ id: v.id, result: {} }) + "\n");
  }
});
const call = (m, p) => new Promise((res) => {
  const id = nextId++; const t = setTimeout(() => res({ __timeout: true }), 20000);
  pending.set(id, (v) => { clearTimeout(t); res(v); });
  child.stdin.write(JSON.stringify({ id, method: m, params: p }) + "\n");
});
(async () => {
  await new Promise((r) => setTimeout(r, 400));
  const c = await call("session/create", { workspace: { workspacePath: WS, workspaceKey: WS } });
  const sid = c.result.session.sessionId;
  await call("session/setMode", { sessionId: sid, mode: "plan" });
  await call("session/send", { sessionId: sid, content: "Diga apenas: ok" });
  for (let i = 0; i < 15; i++) {
    await new Promise((r) => setTimeout(r, 1500));
    const msgs = await call("session/messages", { sessionId: sid });
    const list = msgs.result?.messages || [];
    console.log("--- tick", i, "n=", list.length);
    for (const m of list) {
      const role = m?.info?.role || m?.role;
      if (role !== "assistant") continue;
      console.log(JSON.stringify({ info: m.info, parts: m.parts }, null, 1).slice(0, 1500));
    }
    const last = [...list].reverse().find((m) => (m?.info?.role) === "assistant");
    const textParts = (last?.parts || []).filter((p) => p.type === "text");
    if (textParts.some((p) => (p.text || "").trim())) { console.log("GOT TEXT"); break; }
  }
  await call("session/close", { sessionId: sid });
  child.kill();
  process.exit(0);
})();
