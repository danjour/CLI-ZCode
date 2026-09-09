# CLI-ZCode — Plano de Implementação (Rust)

CLI próprio em Rust que controla o agente ZCode (modelos GLM) sem depender de
pacotes de terceiros — mantendo o tráfego saindo do runtime oficial para não
violar a política de uso da Z.AI.

Status: **documentado e validado em 2026-09-08** — o protocolo abaixo foi
testado de verdade contra `zcode.cjs app-server` (ver seção "Comprovado").

---

## 1. Por que uma casca em Rust e não um cliente HTTP próprio

O ZCode desktop é um app Electron cujo núcleo é um CLI Node.js:

```
C:\Users\eduar\AppData\Local\Programs\ZCode\resources\glm\zcode.cjs
```

Ele expõe o modo `app-server`: um servidor JSON-RPC sobre stdio. A GUI oficial,
o TUI de terceiros e o seu CLI são todos clientes desse mesmo protocolo.

**Regra de ouro (política Z.AI):** o GLM Coding Plan só pode ser usado em
ferramentas oficialmente suportadas. Uso fora delas = throttling 1302/1303 e,
na 3ª violação, ban permanente. Para não correr risco:

- ✅ Seu CLI em Rust **spawna o `zcode.cjs` como processo filho** e fala o
  protocolo. O tráfego HTTP sai do runtime oficial com headers/assinatura
  oficiais — indistinguível da GUI para o servidor.
- ❌ Seu binário **nunca** deve falar direto com `https://api.z.ai/api/anthropic`
  usando a chave do Coding Plan. Isso é "unsupported SDK integration" e é o
  que dispara o detector.
- ⚠️ Uso não-coding do plano (chat genérico, bot, backend de app) também gera
  ban, pelo mesmo motivo, independente do cliente. Usar para coding.

---

## 2. Arquitetura

```
┌────────────────────────────┐
│  SEU CLI (Rust)            │  UI: ratatui (TUI) / linhas (headless)
│  - spawn do runtime        │  config: ~/.config/zcode-cli/config.toml
│  - parser JSON-RPC         │  sessões: sqlite local
│  - render de eventos       │
└──────────┬─────────────────┘
           │ stdio (JSON-RPC newline-delimited)
┌──────────▼─────────────────┐
│  zcode.cjs app-server      │  runtime OFICIAL (Node)
│  (processo filho)          │  faz as chamadas HTTP
└──────────┬─────────────────┘
           │ HTTPS
┌──────────▼─────────────────┐
│  api.z.ai / open.bigmodel  │  Z.AI Coding Plan
└────────────────────────────┘
```

O runtime Node roda em background; seu Rust é o dono da experiência.

**Razões para Rust (vs. outro wrapper JS):**
- Binário único, startup instantâneo (~ms), sem Node no PATH do usuário.
- Controle total do layout/behavior (repo de terceiros `zcode-app-cli` deixa
  de ser dependência).
- Zero dependência do npm para o usuário final.

---

## 3. Protocolo (ZCode Protocol v1) — shapes verificados

Transporte: **JSON-RPC**, newline-delimited sobre stdin/stdout, **SEM** o campo
`jsonrpc` — cada mensagem é `{"id": N, "method": "...", "params": {...}}`.
Requests do cliente têm `id` numérico; requests do servidor têm `id` string
(ex.: `"server-1"`).

### 3.1 Fluxo mínimo (provado)

1. Spawn:
   ```
   node "C:\Users\eduar\AppData\Local\Programs\ZCode\resources\glm\zcode.cjs" app-server
   ```
   cwd = pasta do `zcode.cjs`. stdin/stdout pipes de texto UTF-8, line-buffered.

2. Criar sessão:
   ```json
   {"id": 1, "method": "session/create", "params": {
     "workspace": {"workspacePath": "C:/caminho/do/projeto", "workspaceKey": "C:/caminho/do/projeto"}
   }}
   ```
   Resposta inclui `result.session.sessionId` (`sess_...`).

3. **Servidor faz requests de volta — precisa responder** (senão dá
   ZodError -32603 e a criação falha):
   ```json
   {"id": "server-1", "method": "session/requestRuntimePreferences",
    "params": {"sessionId": "sess_...", "scope": "runtime-materialization"}}
   ```
   Responder:
   ```json
   {"id": "server-1", "result": {"nativeSearchEnhancementsEnabled": false}}
   ```
   (Obrigatório o campo `nativeSearchEnhancementsEnabled: bool`.)

4. Enviar mensagem (campo é `content`, NÃO `message`):
   ```json
   {"id": 2, "method": "session/send",
    "params": {"sessionId": "sess_...", "content": "sua tarefa"}}
   ```
   O servidor responde rápido com `result` (turn aceito) e depois emite
   eventos de progresso.

5. **Ler a resposta é PULL** — `session/messages`:
   ```json
   {"id": 3, "method": "session/messages", "params": {"sessionId": "sess_..."}}
   ```
   → `result` = lista de mensagens, cada uma com `parts: [{type: "text", text: "..."}]`.
   Última do assistant = resposta. (Comprovado: `[assistant] Canberra.`)

6. Encerrar: `session/close` ou matar o processo filho.

### 3.2 Métodos confirmados de sessão

| Método | Params (verificados) | Result |
|---|---|---|
| `session/create` | `{workspace:{workspacePath, workspaceKey}}` | sessão + projeção + settings |
| `session/send` | `{sessionId, content}` | projeção |
| `session/messages` | `{sessionId}` | lista de msgs (`parts[].text`) |
| `session/list` | `{limit}` | `{sessions:[{sessionId,title,mode,status,createdAt,...}]}` |
| `session/usage` | `{sessionId}` | `{totalTokens,inputTokens,outputTokens,reasoningTokens,cacheCreationTokens,cacheReadTokens,modelRequestCount,...}` |
| `session/setModel` | `{sessionId, model:{providerId,modelId}}` | projeção |
| `session/setMode` | `{sessionId, mode}` (`plan/build/edit/yolo`) | projeção |
| `session/setThoughtLevel` | `{sessionId, thoughtLevel}` | projeção |
| `session/resume` | `{sessionId}` | projeção |
| `session/stop` | `{sessionId}` | interrompe turn |
| `session/fork` | — | fork da sessão |
| `session/goal` | — | goal/target |
| `session/subagents` | — | subagentes |
| `session/subscribe` | `{sessionId}` | eventos push |

Sem `initialize`/`server/discover` (métodos MCP não existem → -32601).

### 3.3 Modelos disponíveis

Vem da config do CLI (`~/.zcode/cli/config.json`, schema `zcode.model-providers.v2`):

- `"model": {"main": "zai/glm-5.3", "lite": "zai/glm-5.3-Flash"}` — main + lite.
- `session/create` retorna `settings.model.available[]` já resolvido:
  `label`, `ref:{providerId,modelId}`, `contextWindow` (1.000.000),
  `maxOutputTokens` (128.000), `reasoning.levels` ([low, high, max]).
- `session/setModel` com **objeto** `{providerId, modelId}` (string "lite"
  ou "zai/glm-5.3-Flash" dá ZodError — precisa ser objeto).

Para adicionar modelos/providers: editar `~/.zcode/cli/config.json`
(provider `zai` = Z.AI Coding Plan; `bigmodel` = open.bigmodel.cn; ambos com
`kind: "anthropic"`, API key copiada da GUI em `~/.zcode/v2/config.json`).

### 3.4 Estado que o create devolve (útil para render)

- `result.session`: `{sessionId, title, mode, model:{providerId,modelId}, status, createdAt, workspace, sessionKind}`
- `result.settings`: `{mode, model:{current,lastUsed,available[]}, permission, thoughtLevel}`
- `result.projection`: `{status, mode, contextWindow, contextUsed, totalTokenCount, turnCount, activeToolCalls, backgroundJobs, pendingPermissions, target}`
- `result.slashCommands`: lista de slash commands builtin (`/goal`, `/compact`, `/mode`, `/model`, `/new`, `/resume`, `/rewind`, `/fork`, `/mcp`, `/expert`, ...)
- `result.todos`, `result.todoGroups`, `result.protocol` (`{name:"ZCode Protocol",version:1}`)

### 3.5 Eventos push observados

Durante um turn: `state.updated`, `session/requestRuntimePreferences`,
`process/mcpTelemetry`, `computer-use/operation-event`, `v4/telemetry/event`.
Deltas de streaming de texto não aparecem como push (ou exigem
`session/subscribe` + parsing fino) — por isso o loop de UI usa poll de
`session/messages`/`state.updated` a cada ~500ms.

---

## 4. Configuração (do seu CLI)

`~/.config/zcode-cli/config.toml` (exemplo):

```toml
[runtime]
zcode_cjs = "C:/Users/eduar/AppData/Local/Programs/ZCode/resources/glm/zcode.cjs"
node = "node"

[default]
workspace = "C:/Users/eduar"
mode = "build"
model = "zai/glm-5.3"
thought_level = "max"

[ui]
theme = "dark"
stream_refresh_ms = 500
```

Descoberta automática do `zcode.cjs`: procurar em
`%LOCALAPPDATA%\Programs\ZCode\resources\glm\zcode.cjs` (Windows),
`/Applications/ZCode.app/...` (macOS), `$HOME/.local/share/...` (Linux),
com override via variável `ZCODE_CJS` ou flag `--runtime`.

---

## 5. Escopo — o que um CLI "completo" tem

### Fase 1 — MVP (1–2 dias)
- [ ] Spawn do runtime + transporte JSON-RPC (leitura de linhas, enfileirar
      requests/respostas, resolver requests do servidor automaticamente).
- [ ] `zcode-cli` (sem args): REPL — prompt `>`, manda `session/send`,
      mostra texto final do assistant (poll de `session/messages`).
- [ ] Flags: `--cwd`, `--model`, `--mode` (plan|build|edit|yolo),
      `--thought-level` (low|high|max), `--json` (saída machine-readable),
      `-p "prompt"` (one-shot), `--version`.
- [ ] `session/create` + loop de resposta a requests do servidor
      (`requestRuntimePreferences` com `nativeSearchEnhancementsEnabled`).
- [ ] Graceful shutdown (`session/close` + kill do filho no Ctrl+C; no
      Windows: taskkill da árvore).

### Fase 2 — Sessões (2–3 dias)
- [ ] `session/list` → `zcode-cli sessions` (tabela: id, título, modo, status,
      data).
- [ ] `zcode-cli resume <id>` / `-c` (continua última da pasta).
- [ ] `zcode-cli new <pasta>` e `zcode-cli fork <id>`.
- [ ] Persistência local (sqlite ou JSON em `~/.local/share/zcode-cli/`) com:
      histórico de sessões, título, workspace, modelo usado.

### Fase 3 — Controle de execução (2–3 dias)
- [ ] `/stop` (Ctrl+C durante turn → `session/stop`).
- [ ] `/mode` — trocar modo em runtime (`session/setMode`).
- [ ] `/model` — trocar modelo em runtime (`session/setModel` com objeto
      `{providerId, modelId}`), listando `settings.model.available`.
- [ ] `/thought` — trocar nível de raciocínio (`session/setThoughtLevel`).
- [ ] `/compact` — `session/compact` (limpar/compactar contexto).
- [ ] `/usage` — `session/usage` (tokens por sessão, cache hit).
- [ ] `/resume`, `/new`, `/fork`, `/goal` (definir/ver goal)
      — mapear para os métodos correspondentes.

### Fase 4 — TUI rica (1–2 semanas)
- [ ] `ratatui`: layout dividido — input embaixo, histórico no meio, barra de
      status no rodapé (modelo, modo, tokens, status).
- [ ] Streaming: barra de "working…" + refresh de `state.updated`; render
      incremental das mensagens (markdown leve, cores por role).
- [ ] Multilinha: input com multiline; Enter envia, Shift+Enter quebra linha
      (ou Ctrl+J, padrão de TUIs).
- [ ] Atalhos: Ctrl+R resume, Ctrl+N new, Esc cancela turn, Ctrl+C duas vezes
      sai.
- [ ] Permissões de ferramenta: quando `projection.pendingPermissions` tem
      itens, mostrar prompt y/n no TUI (o runtime pergunta antes de tool
      perigosa).
- [ ] Temas (variantes claro/escuro) + detecção de terminal (256 cores).

### Fase 5 — Headless / integração (2–3 dias)
- [ ] `zcode-cli -p "tarefa" --json` → JSON com resposta + usage + diff de
      arquivos (para orquestração por scripts/agentes — útil no seu caso).
- [ ] `--allowed-tools` / `--disallowed-tools` (ex.: `Bash(git *)`) — mapear
      para o filtro do protocolo.
- [ ] Exit codes: 0 sucesso, 1 erro, 2 turn parado.
- [ ] (Opcional) hook `--notify-on-done` (beep / comando).

### Fase 6 — Empacotamento
- [ ] `cargo build --release` → binário único (target/release/zcode-cli.exe).
- [ ] Instalar em `~/.cargo/bin` e criar alias `zc`.
- [ ] Bundlar Node runtime? (não recomendado — `node` já existe; o runtime
      ZCode é do app instalado. Lidar com "ZCode não instalado" com erro claro.)
- [ ] Auto-update? (opcional; para uso pessoal, `cargo install --path .` basta.)

---

## 6. Crate map (Rust)

| Crate | Uso |
|---|---|
| `ratatui` + `crossterm` | TUI |
| `serde` / `serde_json` | JSON-RPC |
| `tokio` | async: ler stdout do filho + input + timers de refresh |
| `clap` (derive) | CLI args |
| `dirs` | localizar pastas de config |
| `tracing` + `tracing-subscriber` | log em `~/.local/state/zcode-cli/log.txt` |
| `thiserror` | erros tipados |
| `rusqlite` (opcional) | histórico de sessões |
| `once_cell` | globals |

Estrutura de módulos:

```
src/
  main.rs        — parse args, selecionar subcomando, setup tracing
  rpc.rs         — transporte JSON-RPC (linhas, ids, filas)
  runtime.rs     — spawn/gestão do zcode.cjs app-server, restart em crash
  session.rs     — create/send/messages/usage/stop/setModel/setMode...
  server_requests.rs — respostas automáticas aos requests do servidor
  ui/
    app.rs       — estado (mensagens, status, input)
    render.rs    — layout ratatui
    input.rs     — editor de texto multilinha + atalhos
    theme.rs
  commands.rs    — slash commands e parser
  config.rs      — toml config + descoberta do zcode.cjs
  cli.rs         — subcomandos (new/resume/sessions/fork/usage/...)
```

---

## 7. Detalhes de Windows (onde você roda)

- Terminal tool do ambiente usa bash (MSYS) — mas o binário do CLI deve ser
  nativo Windows; rodar via `cargo build --release` e chamar de cmd/PowerShell.
- Ctrl+C: usar `taskkill /PID <pid> /T /F` no signal handler para matar a
  árvore (node filho) — `proc.kill()` no Python não mata a árvore no Windows.
- Paths: sempre aceitar `C:/...` e `C:\...`; normalizar para o runtime
  (`workspacePath` aceitou forward slash no teste).
- `node` no PATH: verificar `where node` no setup; mostrar erro claro se faltar.

---

## 8. Riscos / decisões em aberto

- **Streaming**: texto chega por pull (`session/messages`) — para TUI com
  streaming real, investigar `session/subscribe` + shape dos eventos
  (`session/event` com deltas) antes de investir em render incremental.
- **Permissões**: pendente shape exato de `pendingPermissions` e como
  aprovar/negar via protocolo (procurar `session/permission`/`tools/call`
  no runtime).
- **Atualizações do ZCode**: a versão do protocolo pode mudar com o app;
  guardar `protocol.version` do create e avisar se for > 1.
- **Login alternativo**: `zcode login` (OAuth Z.AI) cria credenciais próprias
  do CLI; hoje usamos a API key da GUI (`~/.zcode/v2/config.json`) copiada
  para `~/.zcode/cli/config.json`. Documentar no `zcode-cli doctor`.
- **Concorrência de sessões**: o runtime suporta várias sessões; definir se o
  CLI mantém uma por processo, ou reusa o runtime (mais complexo).

---

## 9. Testes de aceitação (definição de "pronto")

1. `zcode-cli -p "escreva a palavra ok" --json` → JSON com resposta contendo
   `ok` e `usage` preenchido (ex.: `inputTokens > 0`).
2. `zcode-cli` (TUI) → digitar pergunta, ver resposta renderizada, `/model`
   troca para Flash, `/mode plan`, `/usage` mostra tokens > 0.
3. `zcode-cli sessions` → lista sessões; `zcode-cli resume <id>` continua
   conversa (pergunta "o que eu perguntei?" → sabe responder).
4. Ctrl+C durante turn → para (`session/stop`) e volta ao prompt limpo.
5. `zcode-cli -p "crie um arquivo x.txt com conteúdo 'oi'"` → arquivo existe
   no workspace.
6. Fechar (Esc/exit) → processo filho morto (verificar com tasklist).
7. Rodar em pasta sem ZCode → erro claro "ZCode não encontrado em ...".

---

## 10. Referências

- Protocolo validado ao vivo em 2026-09-08 (seed: este documento).
- Skill `zcode-cli` (Hermes) contém o protocolo + fix da config.
- App: `C:\Users\eduar\AppData\Local\Programs\ZCode\resources\glm\zcode.cjs`
  (CLI interno `--help` mostra todos os comandos; `app-server` é o protocolo).
- Config de modelos: `~/.zcode/cli/config.json` (schema
  `zcode.model-providers.v2`); GUI: `~/.zcode/v2/config.json`.
- Política Z.AI: docs.z.ai/devpack/usage-policy (só ferramentas suportadas;
  3 violações = ban).