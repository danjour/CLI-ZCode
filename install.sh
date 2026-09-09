#!/usr/bin/env bash
# Instalador do zcode-cli (macOS/Linux) — baixa o binário da última release.
#
# Uso (após publicar o repo, ajuste REPO abaixo):
#   curl -fsSL https://raw.githubusercontent.com/<repo>/main/install.sh | bash
#
# Pré-requisitos: ZCode desktop instalado e logado + node no PATH
# (o CLI usa o runtime oficial; nada de chave de API). Valide com: zcode-cli doctor

set -euo pipefail
REPO="danjour/CLI-ZCode"

case "$(uname -s)-$(uname -m)" in
    Linux-x86_64) TARGET="x86_64-unknown-linux-gnu" ;;
    Darwin-arm64) TARGET="aarch64-apple-darwin" ;;
    *) echo "Plataforma não suportada por esta release: $(uname -s)-$(uname -m). Use 'cargo install --git'." >&2; exit 1 ;;
esac

DIR="${ZCODE_CLI_INSTALL_DIR:-$HOME/.cargo/bin}"
mkdir -p "$DIR"

TAG=$(curl -fsSL "https://api.github.com/repos/$REPO/releases/latest" | sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p')
[ -n "$TAG" ] || { echo "Não achei a última release de $REPO." >&2; exit 1; }
ASSET="zcode-cli-${TAG#v}-$TARGET.tar.gz"
URL="https://github.com/$REPO/releases/download/$TAG/$ASSET"

echo "zcode-cli installer — $TAG ($TARGET)"
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT
curl -fsSL "$URL" -o "$TMP/$ASSET"
tar xzf "$TMP/$ASSET" -C "$TMP"
find "$TMP" -name zcode-cli -type f -exec install -m 0755 {} "$DIR/zcode-cli" \;

case ":$PATH:" in
    *":$DIR:"*) ;;
    *) echo "Aviso: \"$DIR\" não está no PATH. Adicione ao seu ~/.bashrc ou ~/.zshrc:" >&2
       echo "  export PATH=\"$DIR:\$PATH\"" >&2 ;;
esac

echo "Instalado em $DIR/zcode-cli — versão:"
"$DIR/zcode-cli" --version
echo ""
echo "Validação do ambiente (ZCode desktop + node): rode  zcode-cli doctor"
