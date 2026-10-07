<#
.SYNOPSIS
  在 Windows Server 上安装 rdep-service（或 rdep-forwarder）并注册为 Windows 服务。

.DESCRIPTION
  依赖：NSSM（把普通 exe 包装为 Windows 服务）。
  安装步骤：拷贝二进制 → 准备配置/证书 → 用 NSSM 注册服务 → 启动。

.EXAMPLE
  .\install.ps1 -Component service -Binary ..\..\target\release\rdep-service.exe -InstallDir C:\rdep
.EXAMPLE
  .\install.ps1 -Component forwarder -Binary ..\..\target\release\rdep-forwarder.exe -InstallDir C:\rdep
#>
[CmdletBinding()]
param(
    [ValidateSet('service', 'forwarder')]
    [string]$Component = 'service',

    [Parameter(Mandatory = $true)]
    [string]$Binary,                 # rdep-service.exe / rdep-forwarder.exe 路径

    [string]$InstallDir = 'C:\rdep',
    [string]$ServiceName = '',       # 默认 rdep-service / rdep-forwarder
    [string]$Nssm = 'nssm.exe',      # nssm 可执行文件（或其全路径）
    [string]$ListenPort = '',        # service: RDEP_LISTEN 端口；forwarder: client 端口
    [string]$WebPort = '',           # Web 管理端口（可选）
    [string]$RelayToken = 'change-me',
    [string]$ForwarderHost = '',     # service 经中转时用
    [string]$ServiceId = '',
    [switch]$Uninstall               # 卸载并删除服务
)

$ErrorActionPreference = 'Stop'

if (-not $ServiceName) { $ServiceName = "rdep-$Component" }
$dataDir = Join-Path $InstallDir 'data'
$certDir = Join-Path $InstallDir 'certs'
$scriptDir = Join-Path $dataDir 'scripts'

function Assert-Admin {
    $id = [Security.Principal.WindowsIdentity]::GetCurrent()
    if (-not ([Security.Principal.WindowsPrincipal]$id).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
        throw '需要管理员权限运行（请以管理员身份执行 PowerShell）。'
    }
}

# ---------- 卸载 ----------
if ($Uninstall) {
    Assert-Admin
    if (Get-Service -Name $ServiceName -ErrorAction SilentlyContinue) {
        Write-Host "停止并删除服务 $ServiceName ..."
        & $Nssm stop $ServiceName confirm | Out-Null
        & $Nssm remove $ServiceName confirm | Out-Null
    } else { Write-Host "服务 $ServiceName 不存在，跳过。" }
    exit 0
}

# ---------- 安装 ----------
Assert-Admin
if (-not (Test-Path $Binary)) { throw "找不到二进制：$Binary" }

Write-Host "== 准备目录 =="
New-Item -ItemType Directory -Force -Path $InstallDir, $dataDir, $certDir, $scriptDir | Out-Null
$dst = Join-Path $InstallDir (Split-Path $Binary -Leaf)
Copy-Item $Binary $dst -Force
Write-Host "已安装二进制 -> $dst"

Write-Host "== 检查证书 =="
$cert = Join-Path $certDir 'server.crt'
$key  = Join-Path $certDir 'server.key'
if (-not (Test-Path $cert) -or -not (Test-Path $key)) {
    Write-Warning "未找到 $cert / $key。请生成后放入 $certDir（可参考 deploy/certs/README.md）。"
}

Write-Host "== 注册 Windows 服务（经 NSSM）=="
if (-not (Get-Command $Nssm -ErrorAction SilentlyContinue) -and -not (Test-Path $Nssm)) {
    throw "找不到 NSSM（$Nssm）。请从 https://nssm.cc 下载并放到 PATH，或用 -Nssm 指定路径。"
}
if (Get-Service -Name $ServiceName -ErrorAction SilentlyContinue) {
    & $Nssm stop $ServiceName confirm | Out-Null
    & $Nssm remove $ServiceName confirm | Out-Null
}
& $Nssm install $ServiceName $dst | Out-Null
& $Nssm set $ServiceName DisplayName "rdep $Component" | Out-Null
& $Nssm set $ServiceName Description "rdep $Component" | Out-Null
& $Nssm set $ServiceName Start SERVICE_AUTO_START | Out-Null
# 以 LocalSystem 运行，日志落到 data 目录
& $Nssm set $ServiceName AppDirectory $InstallDir | Out-Null
& $Nssm set $ServiceName AppStdout (Join-Path $dataDir 'stdout.log') | Out-Null
& $Nssm set $ServiceName AppStderr (Join-Path $dataDir 'stderr.log') | Out-Null

Write-Host "== 配置环境变量 =="
if ($Component -eq 'service') {
    $env = @{
        RDEP_LISTEN   = if ($ListenPort) { "0.0.0.0:$ListenPort" } else { '0.0.0.0:8443' }
        RDEP_ROOT     = (Join-Path $dataDir 'root')
        RDEP_DB       = (Join-Path $dataDir 'rdep.db')
        RDEP_SCRIPTS  = $scriptDir
        RDEP_CERT     = $cert
        RDEP_KEY      = $key
        RDEP_BACKUP_KEEP = '10'
    }
    if ($WebPort)    { $env['RDEP_WEB_LISTEN'] = "0.0.0.0:$WebPort" }
    if ($ForwarderHost) {
        $env['RDEP_USE_FORWARDER'] = '1'
        $env['RDEP_FWD_HOST']      = $ForwarderHost
        $env['RDEP_FWD_CA']        = $cert
        $env['RDEP_RELAY_TOKEN']   = $RelayToken
        $env['RDEP_SERVICE_ID']    = if ($ServiceId) { $ServiceId } else { $env:COMPUTERNAME }
    }
} else {
    $env = @{
        RDEP_FWD_CLIENT_LISTEN   = if ($ListenPort) { "0.0.0.0:$ListenPort" } else { '0.0.0.0:9443' }
        RDEP_FWD_SERVICE_LISTEN  = '0.0.0.0:9444'
        RDEP_FWD_DB              = (Join-Path $dataDir 'forwarder.db')
        RDEP_FWD_CERT            = $cert
        RDEP_FWD_KEY             = $key
        RDEP_RELAY_TOKEN         = $RelayToken
    }
    if ($WebPort) { $env['RDEP_FWD_WEB_LISTEN'] = "0.0.0.0:$WebPort" }
}
foreach ($k in $env.Keys) {
    & $Nssm set $ServiceName Environment "$k=$($env[$k])" | Out-Null
    Write-Host "  $k = $($env[$k])"
}

Write-Host "== 启动服务 =="
& $Nssm start $ServiceName | Out-Null
Write-Host ""
Write-Host "完成。rdep $Component 已作为 Windows 服务 '$ServiceName' 运行。"
Write-Host "查看状态：Get-Service $ServiceName"
Write-Host "Web 管理： http://<host>:$(if($WebPort){$WebPort}else{'8080'})  （admin/admin）"
Write-Host "卸载： .\install.ps1 -Component $Component -Uninstall"
