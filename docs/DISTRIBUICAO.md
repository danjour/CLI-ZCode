# Runbook de distribuição — publicar o CLI para outras pessoas

Estado atual: tudo pronto no repo (workflow de release, instaladores, README),
mas o git **não tem nenhum commit** e não há remote. Siga na ordem.

## 0. Único passo manual: autenticar no GitHub

```bash
gh auth login          # escolha GitHub.com → HTTPS → login no navegador
```

## 1. O que vai público (decisão aplicada no `.gitignore`)

O `.gitignore` exclui do repo os dados de processo interno — `.maestri/`,
`memoria/`, `PLANO-ORQUESTRADOR-*`, `DECISAO-ORQUESTRADOR-*`, `HANDOFF-*`,
`PARECER-*` — além de build (`/target`), lixo de OS/editor e possíveis
`.env`/logs. Ficam públicos: código, `README.md`, `docs/` (RESUMO,
DISTRIBUICAO e o detalhamento do protocolo `docs/PLANO-CLI-ZCODE.md`),
`assets/` e `tools/`. Esses arquivos internos **continuam locais** (o
`.gitignore` não apaga nada — só os tira do versionamento). Varredura de
segredos feita em 2026-09-09: nenhum chave/token no que sobe.

## 2. Commit baseline + criar o repo + push

```bash
git add -A
git commit -m "zcode-cli 0.1.0 — TUI, headless, REPL, doctor (rodadas V1+V2 Windows)"
gh repo create CLI-ZCode --public --source=. --push
#   (ou --private; o instalador e o cargo install --git funcionam iguais
#    para membros autenticados no repo privado)
```

## 3. Ajustar o slug nos artefatos (substituir SEU-USUARIO/<repo>)

- `install.ps1` → linha `$Repo = "SEU-USUARIO/CLI-ZCode"`
- `install.sh` → linha `REPO="SEU-USUARIO/CLI-ZCode"`
- `README.md` → `<repo>` nas URLs de instalação
- `Cargo.toml` → adicionar `repository = "https://github.com/<slug>"`
  (necessário também para o futuro `cargo publish` no crates.io)

Commit dos ajustes: `git add -A && git commit -m "dist: slug do repo nos instaladores" && git push`

## 4. Publicar a release (gera os binários automaticamente)

```bash
git tag v0.1.0
git push origin v0.1.0
```

O workflow `.github/workflows/release.yml` compila e anexa na Release os
binários de `x86_64-pc-windows-msvc`, `x86_64-unknown-linux-gnu` e
`aarch64-apple-darwin` (aba Actions → release). Acompanhe: `gh run watch`.

## 5. Como outras pessoas instalam e conectam

```powershell
# Windows (PowerShell)
irm https://raw.githubusercontent.com/<slug>/main/install.ps1 | iex
zcode-cli doctor        # 5/5 OK = pronto
```

```bash
# macOS / Linux
curl -fsSL https://raw.githubusercontent.com/<slug>/main/install.sh | bash
```

Alternativas: `cargo install --git https://github.com/<slug>` (com Rust).
Pré-requisitos do usuário final: **ZCode desktop instalado e logado** + `node`
no PATH — a autenticação é a sessão do próprio desktop (cada um usa o próprio
plano; o CLI nunca fala com a API). `doctor` falha com instrução clara se algo
faltar.

## 6. Opcional, depois: crates.io

```bash
cargo publish   # metadados já prontos (name/version/description/license)
```

Nota: crates.io publica o CÓDIGO (build do zero na máquina do usuário); os
binários da Release continuam sendo o caminho "sem Rust". Versionar com
`cargo release`/tags `vX.Y.Z` a cada entrega.

## Rollback / atualização

Nova versão = novo commit + nova tag (`v0.1.1`...); `releases/latest` sempre
aponta para a última, e os instaladores baixam dela. Para remover: `gh release
delete` + `git push --delete` (binários) ou `cargo uninstall zcode-cli` (local).
