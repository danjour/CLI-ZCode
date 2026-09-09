# Instalador do zcode-cli (Windows) - baixa o binario da ultima release.
#
# Uso (apos publicar o repo, ajuste $Repo abaixo):
#   irm https://raw.githubusercontent.com/<repo>/main/install.ps1 | iex
# Ou: powershell -ExecutionPolicy Bypass -File install.ps1 [-Dir "C:\bin"]
#
# Pre-requisitos: ZCode desktop instalado e logado + node no PATH
# (o CLI usa o runtime oficial; nada de chave de API). Valide com: zcode-cli doctor
#
# NOTA: mantenha este arquivo em ASCII puro (PowerShell 5.1 le .ps1 sem BOM
# como ANSI e acentos/em-dash quebram o parse em maquinas de usuario).

param([string]$Dir = "")

$ErrorActionPreference = "Stop"
$Repo = "danjour/CLI-ZCode"

if (-not $Dir) { $Dir = "$env:USERPROFILE\.cargo\bin" }

# Arquitetura suportada nesta release
if ($env:PROCESSOR_ARCHITECTURE -ne "AMD64") {
    throw "Esta release publica binario x86_64 Windows; arquitetura detectada: $($env:PROCESSOR_ARCHITECTURE)."
}

Write-Host "zcode-cli installer - repo $Repo"
$rel = Invoke-RestMethod "https://api.github.com/repos/$Repo/releases/latest"
$asset = $rel.assets | Where-Object { $_.name -like "*x86_64-pc-windows-msvc.tar.gz" } | Select-Object -First 1
if (-not $asset) { throw "Nenhum binario Windows encontrado na release $($rel.tag_name)." }

Write-Host "Baixando $($asset.name) (release $($rel.tag_name))..."
$tmpDir = Join-Path $env:TEMP ("zcode-cli-" + [guid]::NewGuid().ToString("N"))
New-Item -ItemType Directory -Force -Path $tmpDir | Out-Null
$tmpArc = Join-Path $tmpDir $asset.name
Invoke-WebRequest -Uri $asset.browser_download_url -OutFile $tmpArc -UseBasicParsing
tar -xzf $tmpArc -C $tmpDir

$exe = Get-ChildItem -Path $tmpDir -Recurse -Filter "zcode-cli.exe" | Select-Object -First 1
if (-not $exe) { throw "zcode-cli.exe nao encontrado dentro do arquivo." }

New-Item -ItemType Directory -Force -Path $Dir | Out-Null
Copy-Item $exe.FullName (Join-Path $Dir "zcode-cli.exe") -Force
Remove-Item -Recurse -Force $tmpDir

# PATH: avisa se o destino nao estiver visivel
$inPath = ($env:PATH -split ";") -contains $Dir
if (-not $inPath) {
    Write-Warning ("O destino nao esta no PATH: " + $Dir)
    Write-Warning "Adicione com:"
    Write-Warning ("  [Environment]::SetEnvironmentVariable('Path', `$env:Path + ';" + $Dir + "', 'User')")
}

Write-Host "Instalado em $Dir\zcode-cli.exe - versao:"
& (Join-Path $Dir "zcode-cli.exe") --version
Write-Host ""
Write-Host "Validacao do ambiente (ZCode desktop + node): rode  zcode-cli doctor"
