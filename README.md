# zcode-cli

CLI em Rust que controla o agente ZCode (modelos GLM) sem depender de pacotes
de terceiros. Inclui modo headless/one-shot (`-p`), REPL, TUI (`tui`/`--tui`) e
subcomandos de sessão.

## Regra de ouro (política Z.AI — bloqueante)

O GLM Coding Plan só pode ser usado em ferramentas oficialmente suportadas.
Por isso este binário **nunca** fala direto com `https://api.z.ai`: ele sempre
spawna o runtime oficial (`zcode.cjs app-server`, do ZCode desktop instalado)
como processo filho e conversa JSON-RPC newline-delimited com ele. O tráfego
HTTPS sai do runtime oficial — indistinguível da GUI. Falar direto com a API
usando a chave do plano = throttling e risco de ban. Não faça isso.

## Instalar e conectar — passo a passo

Visão geral: **(1)** ZCode desktop instalado e logado → **(2)** `node` no PATH
→ **(3)** instalar o zcode-cli → **(4)** `zcode-cli doctor` com 5/5 OK →
**(5)** usar. O CLI **não tem login próprio e não usa chave de API**: a
autenticação é a sessão do seu ZCode desktop, e cada usuário consome o
**seu próprio** plano.

### Passo 1 — ZCode desktop instalado e logado (obrigatório)

- Instale o aplicativo **ZCode desktop** e faça login nele (uma vez basta; a
  sessão fica guardada no app).
- É de onde o CLI pega o runtime oficial (`zcode.cjs`) e a autenticação.
- Sem isso, os comandos falham com erro claro: `ZCode não encontrado em ...`.

### Passo 2 — Node no PATH (obrigatório)

- Confira com `node --version` (testado com v24; qualquer Node recente serve).
- Não é preciso (nem recomendado) bundlar Node: o CLI usa o do sistema para
  rodar o `zcode.cjs`.

### Passo 3 — Instalar o zcode-cli

**Usuários (binário pronto, sem Rust):**

```powershell
# Windows (PowerShell)
irm https://raw.githubusercontent.com/danjour/CLI-ZCode/main/install.ps1 | iex
```

```bash
# macOS / Linux
curl -fsSL https://raw.githubusercontent.com/danjour/CLI-ZCode/main/install.sh | bash
```

**Desenvolvedores (do código-fonte):**

```powershell
cargo install --git https://github.com/danjour/CLI-ZCode   # direto do repo
cargo install --path .                          # de um checkout local
```

> **O que o instalador faz por você:** consulta a última release do GitHub,
> baixa o binário da sua plataforma, instala em `~/.cargo/bin`
> (`%USERPROFILE%\.cargo\bin` no Windows), confere o PATH e roda `--version`
> no final. Você **não precisa baixar nem abrir** o
> `zcode-cli-...-tar.gz` da Release — ele é embalagem interna que o script
> consome sozinho.

### Passo 4 — Validar a conexão: `zcode-cli doctor`

```powershell
zcode-cli --version   # 0.1.0
zcode-cli doctor      # precisa 5/5 OK
```

O doctor autodetecta o `zcode.cjs` do desktop, valida config e modelos e
aponta exatamente o que resolver se algo falhar. Tudo verde = conectado e
pronto.

### Passo 5 — Usar

Veja [Comandos principais](#comandos-principais) abaixo: TUI (`zc tui`), REPL
(`zc`) ou headless (`zc -p "tarefa" --json`). Atalho opcional: alias `zc`.

## Alias `zc`

Sem tocar no ambiente além do `cargo install` acima. Exemplos (opcionais):

```powershell
# PowerShell ($PROFILE): function zc { zcode-cli @args }
```

```cmd
REM cmd (doskey): doskey zc=zcode-cli $*
```

## Comandos principais

```powershell
zcode-cli --cwd C:/meu/proj -p "explique este repo" --json
zcode-cli                                     # REPL (warm: envios funcionam)
zcode-cli tui                                 # TUI rica (ou --tui)
zcode-cli sessions                            # session/list (+ fallback local)
zcode-cli resume <id>                         # LEITURA (ver R1); -c = última da pasta
zcode-cli new <pasta>                         # cria sessão
zcode-cli fork <id>                           # exige checkpoint do servidor
zcode-cli usage [<id>]                        # tokens da sessão
zcode-cli doctor [--json]                     # checks locais, sem sessão
zcode-cli daemon                              # broker residente (sessões quentes)
zcode-cli daemon --stop                       # parada limpa do daemon
```

Flags úteis: `--cwd`, `--model`, `--mode (plan|build|edit|yolo)`,
`--thought-level (low|high|max)`, `--json`, `--runtime`, `--allowed-tools`,
`--disallowed-tools`, `--notify-on-done [--notify-cmd "..."]`, `--no-daemon`.

## Daemon (sessões quentes entre comandos)

A partir da v0.3.0, os comandos usam **automaticamente** um daemon residente
(`zcode-cli daemon` auto-iniciado sob demanda): ele mantém os runtimes Node e
as sessões **quentes** entre invocações — e o `resume` volta a aceitar **novo
turno** quando a sessão está quente nele (limitação R1 contornada; em sessão
fria o aviso honesto continua). Detalhes: TCP local + token (proteção contra
hijack), `daemon.json` em `%APPDATA%/zcode-cli`, fallback silencioso para
runtime embutido se o daemon não responder. `--no-daemon` força o modo antigo;
`zcode-cli doctor` mostra o estado do daemon (check informativo, nunca falha).
Sem daemon, tudo funciona como antes — o daemon é acréscimo, não requisito.

## Exit codes

`0` sucesso · `1` erro (inclui `doctor` com falha) · `2` turno parado
(timeout do turno sem resposta).

## Limitações vigentes (2026-09-08, verificadas contra o servidor ao vivo)

- **R1 — resume condicionado ao daemon**: sem daemon, retomar sessão é
  **leitura** (novo turno falha server-side `-32031`). Com o daemon (padrão
  desde v0.3.0), o `resume` tenta novo turno normalmente quando a sessão está
  quente nele; se a sessão nunca passou por aquele runtime, o erro persiste e
  o aviso honesto de leitura é mantido.
- **Permissões**: em `build`, ferramentas (Write/Bash) falham headless
  (`Permission request failed`); use `--mode yolo` para execução autônoma. Não
  há método RPC para aprovar/negar pedidos — prompt y/n é limitação
  documentada (pendente de daemon/broker).
- **Modelo**: o servidor só oferece `zai/glm-5.3-Flash` (+ alias `main`);
  `--model` só é aplicado se explícito.
- **Filtro de tools**: `--allowed-tools/--disallowed-tools` são parseados com
  aviso honesto de não-aplicado (sem setter RPC no servidor). Decisão do
  orquestrador: aviso, não bloqueio.
- `fork` exige workspace checkpoint do servidor. `subscribe` usa
  `deliveryKind: desktop-continuous`. Log: `%APPDATA%/zcode-cli/log.txt`;
  histórico: `%APPDATA%/zcode-cli/history.json`.

## Publicando uma nova versão (mantenedor)

Nova versão = nova tag, e a CI cuida do resto:

```bash
git tag v0.1.1 && git push origin v0.1.1
```

O workflow `.github/workflows/release.yml` compila Windows/Linux/macOS e
anexa os binários na GitHub Release automaticamente; os instaladores sempre
baixam a última (`releases/latest`). O `zcode-cli-...-tar.gz` anexado é
artefato interno do instalador — usuário final nunca o manipula. Observações:
o one-liner exige que o usuário consiga acessar o repo (público, ou
colaborador num privado); runbook completo em `docs/DISTRIBUICAO.md`.

## Futuro (fora de escopo)

Auto-update (use `cargo install --path .` de novo), permissões approve/deny
via broker (o daemon já existe; a intercepção de permissões é a próxima peça).
