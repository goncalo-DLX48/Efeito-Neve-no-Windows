<#
.SYNOPSIS
    Compila o SnowHush em modo Release.

.DESCRIPTION
    Compila com `cargo build --release`. Se a toolchain ativa for a GNU
    (x86_64-pc-windows-gnu), adiciona automaticamente ao PATH as ferramentas
    que o rustup distribui em `lib\rustlib\<host>\bin\self-contained` (dlltool,
    gcc, ld) e avisa se faltar o assembler (`as.exe`, parte do mingw-w64).

.PARAMETER Test
    Corre também `cargo test` antes de compilar.

.PARAMETER Run
    Inicia a aplicação no fim da compilação.

.EXAMPLE
    .\build.ps1 -Test -Run
#>
[CmdletBinding()]
param(
    [switch]$Test,
    [switch]$Run
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $MyInvocation.MyCommand.Path
Push-Location $root
try {
    if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
        throw "cargo não encontrado. Instale o Rust (https://rustup.rs) e reabra o terminal."
    }

    $hostLine = (rustc -vV | Select-String '^host:').Line
    $hostTriple = ($hostLine -replace '^host:\s*', '').Trim()
    if ($hostTriple -like '*windows-gnu') {
        $sysroot = (rustc --print sysroot).Trim()
        $bin = Join-Path $sysroot "lib\rustlib\$hostTriple\bin\self-contained"
        if (Test-Path -LiteralPath $bin) {
            $env:PATH = "$bin;$env:PATH"
        }
        if (-not (Get-Command as.exe -ErrorAction SilentlyContinue)) {
            Write-Warning ("as.exe não encontrado. A toolchain GNU precisa do mingw-w64 " +
                "(https://winlibs.com) no PATH para gerar as import libraries das DLLs do Windows.")
        }
    }

    if ($Test) {
        Write-Host "== cargo test ==" -ForegroundColor Cyan
        cargo test
        if ($LASTEXITCODE -ne 0) { throw "Os testes falharam." }
    }

    Write-Host "== cargo build --release ==" -ForegroundColor Cyan
    cargo build --release
    if ($LASTEXITCODE -ne 0) { throw "A compilação falhou." }

    $exe = Join-Path $root "target\release\snowhush.exe"
    $size = [math]::Round((Get-Item -LiteralPath $exe).Length / 1MB, 2)
    Write-Host ""
    Write-Host "Compilado: $exe ($size MB)" -ForegroundColor Green
    Write-Host "Execute com: .\target\release\snowhush.exe" -ForegroundColor Green

    if ($Run) {
        Start-Process -FilePath $exe
    }
}
finally {
    Pop-Location
}
