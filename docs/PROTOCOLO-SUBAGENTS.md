# ZCode Protocol — session/subagents (probe ao vivo 2026-09-09)

Resultado de sondagem contra o runtime oficial (`zcode.cjs app-server`),
mesma política do resto do CLI: tráfego sai só do runtime oficial.

## Método

| | |
|---|---|
| Method | `session/subagents` |
| Params (`.strict()`) | `{ sessionId: string, endedCursor?: string, endedLimit?: int 1–100 (default 20) }` |
| Extras | `endedCursor`/`endedLimit` não são aceitos como extras livres; só esses. `limit` simples → `-32602`. |

### Params verificados ao vivo

```
{"id":N,"method":"session/subagents","params":{"sessionId":"sess_..."}}
```

- `sessionId` ausente → `-32602` ZodError (`expected string`).
- Chave desconhecida (`limit`) → `-32602` `unrecognized_keys`.
- Sessão criada mas **ainda não persistida** no store (sem `send`) → `-32004 Session not found`.
  Resolver com `session/resume` de sessão existente, ou após materialização.

## Result (shape ao vivo)

```json
{
  "revision": 0,
  "childSessionIds": ["sess_subagent_agent_<uuid>", "..."],
  "running": [
    {
      "childSessionId": "sess_subagent_agent_<uuid>",
      "agentId": "agent_<uuid>",
      "toolCallId": "call_<hex>",
      "subagentType": "general-purpose",
      "title": "Finalizar PR2 job manager",
      "summary": "opcional enquanto roda",
      "status": "running | waiting | blocked",
      "startedAt": 1789001152433
    }
  ],
  "ended": {
    "total": 5,
    "items": [
      {
        "childSessionId": "sess_subagent_agent_<uuid>",
        "agentId": "agent_<uuid>",
        "toolCallId": "call_<hex>",
        "subagentType": "general-purpose",
        "title": "…",
        "startedAt": 1789001152433,
        "status": "success | failed | cancelled",
        "summary": "texto final do agente…"
      }
    ],
    "nextCursor": "opcional (paginação)"
  }
}
```

Schema do bundle (Zod): running item = `childSessionId, agentId?, toolCallId?,
subagentType, title, summary?, status(running|waiting|blocked), startedAt?`.
O handler também devolve `ended.items[]` com `status: success|failed|cancelled`
e `summary`.

## Como um subagent nasce (não é RPC externo)

Não há `session/launchSubagent` exposto ao cliente. O nascimento é **tool do
modelo** num turn:

```json
{
  "name": "Agent",
  "input": {
    "description": "curto (3–5 words)",
    "prompt": "tarefa completa",
    "subagent_type": "general-purpose",
    "run_in_background": true
  }
}
```

- Tipos conhecidos no bundle: `general-purpose` (default), `Explore`.
- IDs: `agent_<uuid>`, sessão filha `sess_subagent_agent_<uuid>`.
- Flag de feature: `features.subagent` (default `true`).
- `session/cancelBackgroundTask` params: `{ taskId: string }` (Zod exige `taskId`).

## Implicação p/ o “conselho” (3 agentes + 1 chefe)

| Caminho | Viável? | Nota |
|---------|---------|------|
| Listar nativos via `session/subagents` | **Sim** | p/ observar filhos de um turn com Agent tool |
| Lançar 3 agentes nativos sem o modelo | **Não** | launch é tool interno (`subagentPort`) |
| Orquestrar 3–4 sessões top-level no CLI | **Sim** | `session/create`+`send`+`messages` por agente; memória em arquivo |
| Chair num turn que usa Agent tool | **Sim** | custa plano; filhos aparecem em `session/subagents` |

**Decisão do scaffold `zcode-cli council`:** orquestração top-level no CLI
(chair + N workers), protocolo fixo de rodadas, memória por arquivo. O
`session/subagents` fica para *inspeção* de filhos nativos (`council status`
futuro / `/subagents` na TUI).

## Probes

- `tools/probe-subagents.js` — create + probes de params.
- `tools/probe-subagents-existing.js` — resume de sessões reais + dump de `ended`.
- `tools/probe-send.js` / `tools/probe-parts.js` — shape de `session/messages`
  pós-send (inclui `info.error` em rate limit / cota).

Não enviar turnos nos probes de listagem (zero gasto de plano além de
create/resume/list). Os probes de send **consomem plano** se o modelo
responder.

## Scaffold no CLI

`zcode-cli council "<pergunta>" --agents 3 --rounds 2` — orquestra workers +
chefe em sessões top-level, memória em `%APPDATA%/zcode-cli/council/<id>/`.
Workers/chefe em `--mode plan` (sem approve de tools em headless).
`send_and_wait` fail-fast em `info.error` do assistant (ex.: cota 429).
