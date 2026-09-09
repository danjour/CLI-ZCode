# CLI-ZCode — Resumo do que foi feito

CLI próprio em Rust que controla o agente ZCode (modelos GLM) **sem depender de
pacotes de terceiros para o tráfego**: ele spawna o runtime oficial
`zcode.cjs app-server` como processo filho e fala JSON-RPC newline-delimited.
Todo o tráfego HTTPS sai do runtime oficial — indistinguível da GUI para o
servidor (regra de ouro da política Z.AI).

Detalhe do protocolo validado ao vivo: `docs/PLANO-CLI-ZCODE.md`. Uso: `README.md`.

---

## Estado atual (2026-09-09)

- **214 testes verdes** (`cargo test` e `--release`), build release sem warnings,
  gerando `zcode-cli.exe`, instalável em `~/.cargo/bin` (comando
  `zcode-cli`, alias sugerido `zc`). Distribuição pública:
  `github.com/danjour/CLI-ZCode` (one-liners `install.ps1`/`install.sh`,
  releases com binários Win/Linux/macOS via CI).
- **Daemon residente** (v0.3.0): `zcode-cli daemon` mantém runtimes e sessões
  quentes entre comandos — `resume` volta a aceitar turno quando a sessão está
  quente nele (R1 contornado); auto-start transparente com fallback silencioso
  para runtime embutido (`--no-daemon` força modo antigo); TCP local + token.
- **`zcode-cli doctor`** — diagnóstico local zero-custo (node, `zcode.cjs`,
  config, modelos, binário), com `--json` para scripts.
- **TUI** (`zcode-cli tui` ou `--tui`), re-arquitetada nas rodadas V1 e V2
  Windows (`DECISAO-ORQUESTRADOR-TUI-WINDOWS-V1/V2.md`):
  - **Turno vivo**: reasoning do assistente visível (badge THINK), ctx%
    atualizado a cada turno, resumo de tools, notice de arquivos editados
    (snapshot + `git diff --stat`), banner/trava honesta se o runtime Node
    morrer (`/help`, `/diff` incluídos).
  - **Scroll correto**: auto-follow no fim, PageUp/PageDown por página real,
    Home/End, roda do mouse, scrollbar; merge incremental de mensagens (sem reset
    de scroll nem perda de mensagens locais no poll).
  - **Fluidez**: rede fora do caminho do draw (poller em task dedicada + canal
    `UiUpdate`, RPCs de tecla spawnados com estado otimista, gatilho por
    notificação do runtime), animação a 100 ms, dirty-flag com coalescing, cache
    de transcript por `(rev, largura, altura)`.
  - **Input**: paste multilinha (bracketed paste em Unix; detector por timing
    no Windows), histórico de prompts ↑/↓ com draft, input multilinha
    (Ctrl+J), markdown rico (fences com fundo, listas, títulos).
  - **Splash half-block truecolor**: olho anime da referência (fundo magenta,
    íris dourada, pupila azul em coração, cílios pretos, janela retrô) via
    `src/ui/art.rs` (formato FMT1) + painel do olho na sidebar; fallback 256
    cores; ANSI truecolor no REPL/headless.
  - statusbar rica, slash commands, temas dark/light/**retro** (opt-in via
    `[ui] theme = "retro"`).
- **Headless**: `-p "tarefa" --json` → `{response, usage, tools, diff}` + exit
  codes 0/1/2. REPL linhas preservado.

## O que cada fase entregou

| Fase | Entrega | Status |
|---|---|---|
| 1+2 Núcleo | RPC, spawn, session create/send/messages/list/usage, setModel/setMode/setThoughtLevel, config TOML, persistência JSON, graceful shutdown (taskkill) | Aprovada c/ ressalvas |
| 3 Controle | `/stop /mode /model /thought /compact(c/ confirmação) /usage /goal /resume /new /fork`; `session/compact` e `session/goal` sondados ao vivo (existem; `goal set/replace` recusados — shape não verificado) | Aprovada c/ ressalvas |
| 4 TUI rica | ratatui: layout, poll 500ms, multilinha, atalhos, `subscribe` best-effort, permissões ocultas (sem método RPC — documentado) | Aprovada c/ ressalvas |
| 5 Headless | exit codes, `--allowed/--disallowed-tools` (aviso honesto — sem setter no protocolo), JSON estendido + diff, `--notify-on-done`, contrato subscribe/permissões | Aprovada c/ ressalvas |
| 6 Empacotamento | `cargo install` verificado, alias documentado, `doctor`, README; sem bundle de Node, sem auto-update | **Aprovada** (sem achados) |
| Visual V2 | dashboard com header/sidebar/transcript/prompt/rodapé, breakpoints determinísticos, fixtures `TestBackend` | Aprovada c/ ressalvas |
| **V1 Windows (2026-09-09)** | scroll real (follow+merge incremental), re-arquitetura de fluidez (poller/canal/streaming-trigger/cache), arte half-block truecolor do olho (painel na sidebar), dead-code 15→0, `decide_palette`/COLORTERM, aviso `--tui`×`-p` | Aprovada c/ ressalvas (`PARECER-REVISOR-TUI-WINDOWS-V1.md`) |
| **V2 Windows (2026-09-09)** | paridade com CLIs de referência: turno vivo (reasoning/tools/ctx%/diff/banner de runtime morto), `/help`+`/diff`, paste multilinha (gate por plataforma + detector por timing), histórico de prompts ↑/↓, markdown rico (fences/listas/títulos), `code_bg` com contraste | Aprovada c/ ressalvas (`PARECER-REVISOR-TUI-WINDOWS-V2.md`) |
| **V3 Windows (2026-09-09)** | painéis `/context` (barra+quebra+cache hit) e `/usage` (cartões) estilo Claude Code; M-3 memoização do total de linhas; backoff do poller; panic hook que restaura o terminal; rotação de log 1 MiB | Aprovada c/ ressalvas (`PARECER-REVISOR-TUI-WINDOWS-V3.md`) |
| **V4 Windows (2026-09-09)** | Tier 1: startup instantâneo (TUI desenha antes da sessão), `/todos` (checklist do agente), busca Ctrl+F no transcript, `/export md|json`, gate do panic hook por ThreadId + JoinError do turno; Tier 2: **daemon/broker** (TCP local + token, runtimes por workspace, auto-start c/ single-flight e fallback embutido, `--no-daemon`, doctor check, R1 condicional — resume gravável com sessão quente) | Aprovada c/ ressalvas (`PARECER-REVISOR-TUI-WINDOWS-V4.md`) |

Processo: cada fase teve handoff de implementação, revisão independente e
decisão registrada em `.maestri/` (handoffs, pareceres, decisões, contratos).
A rodada V1 Windows foi executada como enxame multi-agente ZCode com auditoria
independente pré e pós-correção.

## Limitações vigentes (documentadas, não são bugs)

- **R1 — resume condicionado ao daemon (v0.3.0)**: com o daemon (padrão), o
  `resume` tenta novo turno quando a sessão está quente nele; sem daemon ou
  com sessão fria naquele runtime, falha server-side (`-32031`) e o CLI mantém
  o modo leitura com aviso honesto.
- Servidor ao vivo só tem `zai/glm-5.3-Flash`; `--mode build` bloqueia
  ferramentas headless (use `--mode yolo`); `fork` exige checkpoint.
- `approve/deny` de permissões não existe via RPC (broker interno) — prompt y/n
  não aprova server-side; `ctx%` é snapshot do `create`.
- TUI validada em fixtures/TestBackend e auditoria de código — **sem validação
  visual em terminal real** (ambiente de build sem terminal interativo); o
  streaming usa notificações como *gatilho de fetch* (shape de `stream.chunk`
  não confirmado no protocolo).

## Mapa rápido do código

`src/main.rs` (args/dispatch) · `rpc.rs` (transporte) · `runtime.rs` (filho +
shutdown + canal de notificações) · `session.rs` (protocolo + `TurnStats`) ·
`server_requests.rs` (auto-respostas) · `config.rs` · `cli.rs` · `commands.rs`
(REPL/TUI/headless + loop de eventos da TUI) · `doctor.rs` · `ui/` (`art.rs`
arte half-block FMT1, `tui.rs` layout/estado/caching, `theme.rs` paletas,
`render.rs`, `input.rs`).

## Futuro registrado

Permissões approve/deny via broker (o daemon existe; a intercepção de
permissões é a próxima peça), streaming `stream.chunk` end-to-end,
`goal set/replace` (sondar shape do texto), cota do plano + categorias de
contexto nos painéis (sonda ao vivo), auto-update, validação visual em
terminal real; da auditoria V1/V2: sondar `role=system` em `session/messages`,
estabilidade de itens de reasoning entre polls; da auditoria V3/V4: Job Object
no Windows p/ node órfão de daemon morto cru, fixtures de overlay em áreas
pequenas, unificar path do log via `log_file_path()`.
