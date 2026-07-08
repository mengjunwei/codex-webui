#Requires -Version 5
# 编译前端 + 启动后端，先杀死所有 codex 相关进程。
# 用法（在项目根目录）：.\start.ps1
# 需在项目根 .env 文件配置 WEBUI_API_KEY（必填，>=16位）和 CODEX_BIN（codex.cmd 完整路径）。
$ErrorActionPreference = "Stop"
$root = $PSScriptRoot
Set-Location $root

Write-Host ""
Write-Host "=== [1/4] 杀死 codex / codex-webui 进程 ===" -ForegroundColor Cyan
Get-Process -Name codex, codex-webui -ErrorAction SilentlyContinue | ForEach-Object {
    Write-Host "  杀死 $($_.ProcessName) (PID $($_.Id))"
    Stop-Process -Id $_.Id -Force
}
Start-Sleep -Seconds 1
Write-Host "  done"

# 从 .env 读键值对到环境变量（避免 dotenvy 的 BOM/编码问题，统一用 PowerShell 读）
Write-Host ""
Write-Host "=== [2/4] 加载 .env ===" -ForegroundColor Cyan
if (Test-Path "$root\.env") {
    Get-Content "$root\.env" | ForEach-Object {
        $line = $_.Trim()
        if ($line -and -not $line.StartsWith("#") -and $line.Contains("=")) {
            $idx = $line.IndexOf("=")
            $k = $line.Substring(0, $idx).Trim()
            $v = $line.Substring($idx + 1).Trim()
            Set-Item -Path ("Env:" + $k) -Value $v
        }
    }
    Write-Host "  已加载 .env"
} else {
    Write-Host "  ⚠️ 根目录无 .env 文件" -ForegroundColor Yellow
}

# 校验 WEBUI_API_KEY
if (-not $env:WEBUI_API_KEY -or $env:WEBUI_API_KEY.Length -lt 16) {
    Write-Host "ERROR: WEBUI_API_KEY 未设置或长度 < 16。" -ForegroundColor Red
    Write-Host "  请在 $root\.env 写：WEBUI_API_KEY=至少16位的随机字符串" -ForegroundColor Yellow
    exit 1
}
Write-Host "  WEBUI_API_KEY 长度 $($env:WEBUI_API_KEY.Length)"

if ($env:CODEX_BIN) {
    Write-Host "  CODEX_BIN = $env:CODEX_BIN"
} else {
    Write-Host "  CODEX_BIN 未设（用默认 codex；若报 program not found，请在 .env 写 CODEX_BIN=codex.cmd 完整路径）" -ForegroundColor DarkGray
}

Write-Host ""
Write-Host "=== [3/4] 编译前端 (web → public/) ===" -ForegroundColor Cyan
pnpm --dir web build
if ($LASTEXITCODE -ne 0) { Write-Host "前端编译失败" -ForegroundColor Red; exit 1 }

Write-Host ""
Write-Host "=== [4/4] 启动后端 (http://localhost:8172) ===" -ForegroundColor Cyan
Write-Host "  提示：调用 /api/codex/sandbox-mode、/approval-policy 时，请确保 config.toml 没被编辑器(Notepad++等)打开。" -ForegroundColor DarkGray
Write-Host ""
cargo run --manifest-path "$root\backend-rs\Cargo.toml"
