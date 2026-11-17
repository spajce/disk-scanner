# Runs cargo / the Tauri CLI inside a Visual Studio developer environment.
#
# This machine has more than one Visual Studio install and the newest one
# (Visual Studio 18 Insiders) ships an incomplete C++ toolset: no headers, no
# desktop import libraries. rustc and the `cc` crate both pick the newest
# install, so a plain `cargo build` fails with LNK1104 on msvcrt.lib.
#
# The script looks for the first install whose MSVC toolset is actually
# complete, imports the environment from its vcvars64.bat, and only then starts
# cargo. On a normal machine with a single working install this is equivalent to
# calling cargo from a "Developer PowerShell" prompt.
#
# Usage:
#   pwsh -File scripts/rust-env.ps1 cargo build
#   pwsh -File scripts/rust-env.ps1 cargo test
#   pwsh -File scripts/rust-env.ps1 tauri dev     (proxies to `pnpm exec tauri`)
param(
  [string]$Tool = "cargo",
  [Parameter(ValueFromRemainingArguments = $true)]
  [string[]]$Rest
)

$ErrorActionPreference = "Stop"

function Find-VcVars64 {
  $vswhere = Join-Path ${env:ProgramFiles(x86)} "Microsoft Visual Studio\Installer\vswhere.exe"
  if (-not (Test-Path $vswhere)) { return $null }

  $roots = & $vswhere -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath 2>$null
  foreach ($root in $roots) {
    $bat = Join-Path $root "VC\Auxiliary\Build\vcvars64.bat"
    if (-not (Test-Path $bat)) { continue }
    $tools = Get-ChildItem (Join-Path $root "VC\Tools\MSVC") -Directory -ErrorAction SilentlyContinue |
      Sort-Object Name -Descending
    foreach ($tool in $tools) {
      $hasHeaders = Test-Path (Join-Path $tool.FullName "include\excpt.h")
      $hasLibs = Test-Path (Join-Path $tool.FullName "lib\x64\msvcrt.lib")
      if ($hasHeaders -and $hasLibs) { return $bat }
    }
  }
  return $null
}

$bat = Find-VcVars64
if ($bat) {
  $lines = & cmd /c "`"$bat`" >nul 2>&1 && set"
  foreach ($line in $lines) {
    $index = $line.IndexOf("=")
    if ($index -le 0) { continue }
    $name = $line.Substring(0, $index)
    if ($name -notmatch '^[A-Za-z_][A-Za-z0-9_]*$') { continue }
    $value = $line.Substring($index + 1)
    Set-Item -Path "env:$name" -Value $value
  }
} else {
  Write-Warning "No complete MSVC toolset found; falling back to the ambient environment."
}

$cargoBin = Join-Path $env:USERPROFILE ".cargo\bin"
if ((Test-Path $cargoBin) -and ($env:PATH -notlike "*$cargoBin*")) {
  $env:PATH = "$cargoBin;$env:PATH"
}

switch ($Tool) {
  "tauri" {
    & pnpm exec tauri @Rest
  }
  "cargo" {
    & cargo @Rest
  }
  default {
    & $Tool @Rest
  }
}

exit $LASTEXITCODE
