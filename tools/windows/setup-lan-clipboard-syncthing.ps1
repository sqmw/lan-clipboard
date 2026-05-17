param(
    [string]$ConfigPath = "$env:LOCALAPPDATA\Syncthing\config.xml",
    [string]$FolderId = "lan-clipboard-src",
    [string]$FolderLabel = "lan-clipboard",
    [string]$FolderPath = "F:\language\rust\code\lan-clipboard",
    [string]$FolderType = "receiveonly",
    [string]$MacDeviceId = "LNTEQPR-BYQRI4J-MPO573U-WOJHQVZ-RNSSOM7-H3NFZSM-PSW4ZDL-RMKREAX"
)

$ErrorActionPreference = "Stop"

if (-not (Test-Path -LiteralPath $ConfigPath)) {
    throw "Syncthing config not found: $ConfigPath"
}

$configDir = Split-Path -Parent $ConfigPath
$backupPath = Join-Path $configDir ("config.xml.bak-" + (Get-Date -Format "yyyyMMdd-HHmmss"))

Copy-Item -LiteralPath $ConfigPath -Destination $backupPath -Force
[xml]$config = Get-Content -LiteralPath $ConfigPath

$folder = $config.configuration.folder | Where-Object { $_.id -eq $FolderId } | Select-Object -First 1
$allDeviceIds = @($config.configuration.device | ForEach-Object { $_.id } | Where-Object { $_ })

if (-not $allDeviceIds) {
    throw "No Syncthing devices found in config."
}

if ($allDeviceIds -notcontains $MacDeviceId) {
    throw "Mac device $MacDeviceId is not present in Syncthing config."
}

if (-not (Test-Path -LiteralPath $FolderPath)) {
    New-Item -ItemType Directory -Path $FolderPath -Force | Out-Null
}

if (-not $folder) {
    $folder = $config.CreateElement("folder")
    $config.configuration.AppendChild($folder) | Out-Null
}

$folder.SetAttribute("id", $FolderId)
$folder.SetAttribute("label", $FolderLabel)
$folder.SetAttribute("path", $FolderPath)
$folder.SetAttribute("type", $FolderType)
$folder.SetAttribute("rescanIntervalS", "3600")
$folder.SetAttribute("fsWatcherEnabled", "true")
$folder.SetAttribute("fsWatcherDelayS", "10")
$folder.SetAttribute("fsWatcherTimeoutS", "0")
$folder.SetAttribute("ignorePerms", "false")
$folder.SetAttribute("autoNormalize", "true")

foreach ($childName in @("filesystemType", "device", "minDiskFree", "versioning", "copiers", "pullerMaxPendingKiB", "hashers", "order", "ignoreDelete", "scanProgressIntervalS", "pullerPauseS", "maxConflicts", "disableSparseFiles", "disableTempIndexes", "paused", "weakHashThresholdPct", "markerName", "copyOwnershipFromParent", "modTimeWindowS", "maxConcurrentWrites", "disableFsync")) {
    @($folder.SelectNodes($childName)) | ForEach-Object {
        $folder.RemoveChild($_) | Out-Null
    }
}

$filesystemType = $config.CreateElement("filesystemType")
$filesystemType.InnerText = "basic"
$folder.AppendChild($filesystemType) | Out-Null

foreach ($deviceId in $allDeviceIds) {
    $deviceNode = $config.CreateElement("device")
    $deviceNode.SetAttribute("id", $deviceId)
    $deviceNode.SetAttribute("introducedBy", "")
    $folder.AppendChild($deviceNode) | Out-Null
}

$minDiskFree = $config.CreateElement("minDiskFree")
$minDiskFree.SetAttribute("unit", "%")
$minDiskFree.InnerText = "1"
$folder.AppendChild($minDiskFree) | Out-Null

$versioning = $config.CreateElement("versioning")
$versioning.SetAttribute("type", "")
$folder.AppendChild($versioning) | Out-Null

$scalarValues = @{
    copiers = "0"
    pullerMaxPendingKiB = "0"
    hashers = "0"
    order = "random"
    ignoreDelete = "false"
    scanProgressIntervalS = "0"
    pullerPauseS = "0"
    maxConflicts = "10"
    disableSparseFiles = "false"
    disableTempIndexes = "false"
    paused = "false"
    weakHashThresholdPct = "25"
    markerName = ".stfolder"
    copyOwnershipFromParent = "false"
    modTimeWindowS = "0"
    maxConcurrentWrites = "2"
    disableFsync = "false"
}

foreach ($entry in $scalarValues.GetEnumerator()) {
    $node = $config.CreateElement($entry.Key)
    $node.InnerText = $entry.Value
    $folder.AppendChild($node) | Out-Null
}

$config.Save($ConfigPath)

$apiKey = $config.configuration.gui.apikey
if ($apiKey) {
    try {
        Invoke-RestMethod `
            -Method Post `
            -Uri "http://127.0.0.1:8384/rest/system/restart" `
            -Headers @{ "X-API-Key" = [string]$apiKey } | Out-Null
        $restartMode = "rest"
    }
    catch {
        $restartMode = "manual"
    }
}
else {
    $restartMode = "manual"
}

Write-Output ("UPDATED|" + $FolderId + "|" + $FolderPath + "|" + $FolderType)
Write-Output ("BACKUP|" + $backupPath)
Write-Output ("DEVICES|" + ($allDeviceIds -join ","))
Write-Output ("RESTART|" + $restartMode)
