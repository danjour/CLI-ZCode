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

## Instalação

Pré-requisitos para **usar**: ZCode desktop instalado **e logado**, `node` no
PATH. (Sem ZCode, os comandos falham com erro claro — `ZCode não encontrado em
...`.) O toolchain do Rust só é preciso para compilar do código-fonte.

### Usuários — binário pronto (sem Rust)

```powershell
# Windows (PowerShell)
irm https://raw.githubusercontent.com/<repo>/main/install.ps1 | iex
```

```bash
# macOS / Linux
curl -fsSL https://raw.githubusercontent.com/<repo>/main/install.sh | bash
```

Os instaladores baixam o binário da última release para `~/.cargo/bin` (ou
`%USERPROFILE%\.cargo\bin`) e já rodam `--version` no final. O `<repo>` é o
slug do GitHub onde o projeto está publicado (ver `docs/DISTRIBUICAO.md`).

### Desenvolvedores — do código-fonte

```powershell
cargo install --git https://github.com/<repo>   # direto do repo
cargo install --path .                          # de um checkout local
zcode-cli --version   # 0.1.0
zcode-cli doctor      # diagnóstico local, sem gastar plano
```

Não é preciso (nem recomendado) bundlar o Node: o `node` do sistema é usado
para rodar o `zcode.cjs` do app instalado.

### Conexão com o ZCode (como funciona para cada usuário)

O CLI **não pede chave de API e não tem login próprio**: ele spawna o runtime
oficial do ZCode desktop já instalado na máquina e conversa com ele por
JSON-RPC local. A autenticação é a sessão do próprio ZCode desktop — cada
usuário consome o **seu próprio** plano. Passo a passo de quem vai usar:

1. Instale o ZCode desktop e faça login nele uma vez.
2. Tenha `node` no PATH (testado com v24).
3. Rode `zcode-cli doctor` — precisa **5/5 OK** (ele autodetecta o
   `zcode.cjs`, valida config e modelos). Tudo verde = pronto para `tui`,
   REPL ou headless.

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
```

Flags úteis: `--cwd`, `--model`, `--mode (plan|build|edit|yolo)`,
`--thought-level (low|high|max)`, `--json`, `--runtime`, `--allowed-tools`,
`--disallowed-tools`, `--notify-on-done [--notify-cmd "..."]`.

## Exit codes

`0` sucesso · `1` erro (inclui `doctor` com falha) · `2` turno parado
(timeout do turno sem resposta).

## Limitações vigentes (2026-09-08, verificadas contra o servidor ao vivo)

- **R1 — resume = leitura**: retomar sessão em processo novo funciona para
  leitura (`messages`/`usage`), mas um novo turno falha server-side
  (`-32031`). Novo turno só em sessão warm (mesmo processo) ou sessão nova.
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

## Futuro (fora de escopo)

Auto-update (use `cargo install --path .` de novo), daemon de runtime longevo,
broker de permissões.
