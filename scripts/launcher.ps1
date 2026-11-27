$ErrorActionPreference = 'Stop'

Add-Type -AssemblyName System.Windows.Forms
Add-Type -AssemblyName System.Drawing

$root = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$playScript = Join-Path $PSScriptRoot 'play.ps1'
$exampleConfig = Join-Path $root 'victory.example.toml'
$config = Join-Path $root 'victory.toml'
$logDir = Join-Path $root 'logs'
$logPath = Join-Path $logDir ("launcher-{0}.log" -f (Get-Date -Format 'yyyyMMdd-HHmmss'))

function Show-Error([string]$message) {
    [System.Windows.Forms.MessageBox]::Show(
        $message, 'VIctory - erreur',
        [System.Windows.Forms.MessageBoxButtons]::OK,
        [System.Windows.Forms.MessageBoxIcon]::Error
    ) | Out-Null
}

try {
    if (-not (Test-Path $playScript)) { throw "Lanceur absent : $playScript" }
    if (-not (Test-Path $exampleConfig)) { throw "Configuration modèle absente : $exampleConfig" }

    $victory = Join-Path $root 'victory.exe'
    if (-not (Test-Path $victory)) {
        $victory = Join-Path $root 'target\release\victory.exe'
    }
    if (-not (Test-Path $victory)) {
        throw "VIctory.exe est introuvable. Télécharge et extrais le paquet Windows complet, puis relance VIctory-Launcher.cmd."
    }

    if (-not (Test-Path $config)) {
        Copy-Item -LiteralPath $exampleConfig -Destination $config
        [System.Windows.Forms.MessageBox]::Show(
            ("Le fichier victory.toml a été créé automatiquement." + [Environment]::NewLine + "Le lanceur transmettra le dossier du jeu sélectionné à VIctory pour cette session."),
            'Configuration initialisée',
            [System.Windows.Forms.MessageBoxButtons]::OK,
            [System.Windows.Forms.MessageBoxIcon]::Information
        ) | Out-Null
    }

    $picker = New-Object System.Windows.Forms.OpenFileDialog
    $picker.Title = 'Sélectionne le fichier exécutable de ton jeu'
    $picker.Filter = 'Exécutable Windows (*.exe)|*.exe|Tous les fichiers (*.*)|*.*'
    $picker.CheckFileExists = $true
    $picker.Multiselect = $false
    $picker.InitialDirectory = [Environment]::GetFolderPath('UserProfile')
    if ($picker.ShowDialog() -ne [System.Windows.Forms.DialogResult]::OK) {
        exit 0
    }

    $gameExe = (Resolve-Path -LiteralPath $picker.FileName).Path
    if ([IO.Path]::GetExtension($gameExe) -ine '.exe') {
        throw 'Le fichier sélectionné doit être un exécutable .exe.'
    }
    if ($gameExe.StartsWith($root, [StringComparison]::OrdinalIgnoreCase)) {
        throw "Garde les fichiers du jeu en dehors du dossier VIctory, puis sélectionne l'exécutable depuis son dossier d'origine."
    }

    $choice = [System.Windows.Forms.MessageBox]::Show(
        ("Jeu sélectionné :" + [Environment]::NewLine + $gameExe + [Environment]::NewLine + [Environment]::NewLine + "Oui : profil normal (recommandé)" + [Environment]::NewLine + "Non : profil sécurisé (désactive les optimisations expérimentales)" + [Environment]::NewLine + "Annuler : quitter"),
        'VIctory - mode de lancement',
        [System.Windows.Forms.MessageBoxButtons]::YesNoCancel,
        [System.Windows.Forms.MessageBoxIcon]::Question
    )
    if ($choice -eq [System.Windows.Forms.DialogResult]::Cancel) { exit 0 }
    $safe = $choice -eq [System.Windows.Forms.DialogResult]::No

    New-Item -ItemType Directory -Force -Path $logDir | Out-Null
    $gpuInfo = @()
    try {
        $gpuInfo = @(Get-CimInstance Win32_VideoController -ErrorAction Stop |
            ForEach-Object { "$($_.Name) (pilote $($_.DriverVersion))" })
    } catch {
        $gpuInfo = @('GPU non détecté automatiquement')
    }
    $cpuInfo = 'CPU non détecté automatiquement'
    $ramInfo = 'RAM non détectée automatiquement'
    $windowsInfo = [Environment]::OSVersion.VersionString
    try {
        $cpuInfo = (Get-CimInstance Win32_Processor -ErrorAction Stop |
            Select-Object -First 1 -ExpandProperty Name).Trim()
        $computer = Get-CimInstance Win32_ComputerSystem -ErrorAction Stop
        $ramInfo = '{0:N1} Go' -f ($computer.TotalPhysicalMemory / 1GB)
        $os = Get-CimInstance Win32_OperatingSystem -ErrorAction Stop
        $windowsInfo = "$($os.Caption) $($os.Version) (build $($os.BuildNumber))"
    } catch {}

    Start-Transcript -LiteralPath $logPath -Force | Out-Null
    Write-Host '=== VIctory Windows launcher ==='
    Write-Host ("Date : {0}" -f (Get-Date -Format 'yyyy-MM-dd HH:mm:ss'))
    Write-Host ("Windows : {0}" -f $windowsInfo)
    Write-Host ("CPU : {0}" -f $cpuInfo)
    Write-Host ("RAM : {0}" -f $ramInfo)
    Write-Host ("GPU / pilote : {0}" -f ($gpuInfo -join '; '))
    Write-Host ("Jeu : {0}" -f $gameExe)
    Write-Host ("Mode : {0}" -f $(if ($safe) { 'sécurisé' } else { 'normal' }))
    Write-Host ("Journal : {0}" -f $logPath)

    # This variable applies only to this launcher process and the child game.
    # It overrides the template path without editing the user's config.
    $env:VICTORY_GAME_DIR = Split-Path -Parent $gameExe

    Write-Host ''
    Write-Host 'Vérification de la configuration VIctory...'
    & $victory config
    if ($LASTEXITCODE -ne 0) {
        throw "La vérification de configuration a échoué (code $LASTEXITCODE)."
    }

    Write-Host ''
    Write-Host 'Démarrage du jeu...'
    if ($safe) {
        & $playScript -Exe $gameExe -Build release -Safe
    } else {
        & $playScript -Exe $gameExe -Build release
    }
    $gameExit = $LASTEXITCODE
    Write-Host ''
    Write-Host ("Le processus VIctory s'est terminé avec le code {0}." -f $gameExit)
    Stop-Transcript | Out-Null

    $summary = "Journal enregistré ici :" + [Environment]::NewLine + $logPath
    if ($gameExit -eq 0) {
        [System.Windows.Forms.MessageBox]::Show(
            "VIctory s'est terminé." + [Environment]::NewLine + [Environment]::NewLine + $summary,
            'VIctory - terminé',
            [System.Windows.Forms.MessageBoxButtons]::OK,
            [System.Windows.Forms.MessageBoxIcon]::Information
        ) | Out-Null
    } else {
        [System.Windows.Forms.MessageBox]::Show(
            ("VIctory a retourné le code {0}." -f $gameExit) + [Environment]::NewLine + "Consulte le journal pour le diagnostic." + [Environment]::NewLine + [Environment]::NewLine + $summary,
            'VIctory - résultat',
            [System.Windows.Forms.MessageBoxButtons]::OK,
            [System.Windows.Forms.MessageBoxIcon]::Warning
        ) | Out-Null
    }
    exit $gameExit
}
catch {
    $message = $_.Exception.Message
    try {
        if (-not (Test-Path $logDir)) { New-Item -ItemType Directory -Force -Path $logDir | Out-Null }
        $message | Add-Content -LiteralPath $logPath -Encoding UTF8
        Write-Host $message -ForegroundColor Red
    } catch {}
    Show-Error ($message + [Environment]::NewLine + [Environment]::NewLine + "Si le problème persiste, envoie le message et le journal au support du projet.")
    exit 1
}
