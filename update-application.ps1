[CmdletBinding()]
param(
    [Parameter(Mandatory = $false)]
    [string]$Version,

    [Parameter(Mandatory = $false)]
    [string]$Notes,

    [switch]$Force,

    [switch]$DryRun
)

$ErrorActionPreference = "Stop"

function Write-Info([string]$Message) {
    Write-Host "[INFO] $Message" -ForegroundColor Cyan
}

function Write-Success([string]$Message) {
    Write-Host "[OK]   $Message" -ForegroundColor Green
}

function Write-WarnLine([string]$Message) {
    Write-Host "[WARN] $Message" -ForegroundColor Yellow
}

function Write-ErrorLine([string]$Message) {
    Write-Host "[ERR]  $Message" -ForegroundColor Red
}

function Invoke-Git([string[]]$Arguments) {
    & git @Arguments
    if ($LASTEXITCODE -ne 0) {
        throw "git $($Arguments -join ' ') failed"
    }
}

function Test-IsGitRepository {
    & git rev-parse --is-inside-work-tree *> $null
    return ($LASTEXITCODE -eq 0)
}

function Get-WorkspaceRoot {
    $scriptDir = Split-Path -Parent $PSCommandPath
    return (Resolve-Path $scriptDir).Path
}

function Get-CurrentWorkspaceVersion([string]$CargoTomlPath) {
    $content = Get-Content -Path $CargoTomlPath -Raw
    $match = [regex]::Match($content, '(?ms)^\[workspace\.package\].*?^version\s*=\s*"(?<version>\d+\.\d+\.\d+)"')
    if (-not $match.Success) {
        throw "Could not locate [workspace.package] version in $CargoTomlPath"
    }
    return $match.Groups['version'].Value
}

function Compare-SemVer([string]$Left, [string]$Right) {
    $leftParts = $Left.Split('.') | ForEach-Object { [int]$_ }
    $rightParts = $Right.Split('.') | ForEach-Object { [int]$_ }
    for ($i = 0; $i -lt 3; $i++) {
        if ($leftParts[$i] -gt $rightParts[$i]) { return 1 }
        if ($leftParts[$i] -lt $rightParts[$i]) { return -1 }
    }
    return 0
}

function Update-WorkspaceVersion([string]$CargoTomlPath, [string]$OldVersion, [string]$NewVersion) {
    $content = Get-Content -Path $CargoTomlPath -Raw
    $pattern = '(?m)^(version\s*=\s*")' + [regex]::Escape($OldVersion) + '("\s*$)'
    $replacement = '${1}' + $NewVersion + '${2}'
    $updated = $content -replace $pattern, $replacement
    if ($updated -eq $content) {
        throw "Workspace version replacement did not change $CargoTomlPath"
    }
    Set-Content -Path $CargoTomlPath -Value $updated -Encoding UTF8 -NoNewline
}

function Get-RemoteHttpsUrl {
    $remote = (& git config --get remote.origin.url)
    if ($LASTEXITCODE -ne 0 -or [string]::IsNullOrWhiteSpace($remote)) {
        return $null
    }

    $remote = $remote.Trim()
    if ($remote -match '^https://') {
        return $remote -replace '\.git$', ''
    }

    if ($remote -match '^git@github\.com:(?<slug>[^\s]+?)(\.git)?$') {
        return "https://github.com/$($Matches['slug'])"
    }

    return $null
}

$workspaceRoot = Get-WorkspaceRoot
Set-Location $workspaceRoot

Write-Info "ClipRelay release automation starting in $workspaceRoot"
if ($DryRun) {
    Write-WarnLine "Dry-run mode enabled. No files, commits, tags, pushes, or releases will be changed."
}

$cargoToml = Join-Path $workspaceRoot "Cargo.toml"
$currentVersion = Get-CurrentWorkspaceVersion -CargoTomlPath $cargoToml
Write-Host ""
Write-Host "Current version: " -NoNewline -ForegroundColor White
Write-Host "$currentVersion" -ForegroundColor Yellow
Write-Host ""

if (-not $Version) {
    $Version = Read-Host "Enter new semantic version (x.y.z)"
}
if ([string]::IsNullOrWhiteSpace($Version) -or ($Version -notmatch '^\d+\.\d+\.\d+$')) {
    throw "Version must match semantic versioning format x.y.z"
}

if (-not $Notes) {
    Write-Host "Enter release notes (end input with an empty line):" -ForegroundColor Cyan
    $noteLines = [System.Collections.Generic.List[string]]::new()
    while ($true) {
        $line = Read-Host
        if ([string]::IsNullOrWhiteSpace($line)) { break }
        $noteLines.Add($line)
    }
    $Notes = $noteLines -join [Environment]::NewLine
}
if ([string]::IsNullOrWhiteSpace($Notes)) {
    throw "Release notes are required and cannot be empty"
}

$isGitRepo = Test-IsGitRepository
if (-not $isGitRepo -and -not $DryRun) {
    throw "This script must be run inside a git repository."
}
if (-not $isGitRepo -and $DryRun) {
    Write-WarnLine "Git repository not detected. Git-dependent checks are skipped in dry-run mode."
}

if (-not $Force) {
    $cmp = Compare-SemVer -Left $Version -Right $currentVersion
    if ($cmp -le 0) {
        throw "New version ($Version) must be greater than current version ($currentVersion). Use -Force to override."
    }
}

$newTag = "v$Version"
$existingTag = $null
if ($isGitRepo) {
    $existingTag = (& git tag -l $newTag)
    if (($LASTEXITCODE -eq 0) -and -not [string]::IsNullOrWhiteSpace($existingTag) -and -not $Force) {
        throw "Tag $newTag already exists. Use -Force to overwrite."
    }
}

$status = $null
if ($isGitRepo) {
    $status = & git status --porcelain
    if ($LASTEXITCODE -ne 0) {
        throw "Failed to inspect git status"
    }
    if (-not [string]::IsNullOrWhiteSpace($status)) {
        Write-WarnLine "Working tree has uncommitted changes. They will be included only if staged by this script."
    }
}

$originalCargoToml = Get-Content -Path $cargoToml -Raw
$lockPath = Join-Path $workspaceRoot "Cargo.lock"
$lockExistedBefore = Test-Path $lockPath
$originalLock = $null
if ($lockExistedBefore) {
    $originalLock = Get-Content -Path $lockPath -Raw
}

$changedFiles = New-Object System.Collections.Generic.List[string]

try {
    if ($DryRun) {
        Write-Host ""
        Write-Host "Release Summary (Dry Run)" -ForegroundColor White
        Write-Host "-------------------------" -ForegroundColor White
        Write-Host "Current version : $currentVersion"
        Write-Host "New version     : $Version"
        Write-Host "Tag             : $newTag"
        Write-Host "Release notes:" -ForegroundColor White
        Write-Host $Notes
        Write-Host ""

        Write-Info "Planned actions"
        Write-Host "- Update Cargo.toml workspace version: $currentVersion -> $Version"
        Write-Host "- Run: cargo update --workspace"
        Write-Host "- Run: cargo build --release"
        Write-Host "- Run: cargo test"
        if ($isGitRepo) {
            Write-Host "- Run: git add Cargo.toml Cargo.lock"
            Write-Host "- Run: git commit -m \"chore: bump version to $Version\""
            Write-Host "- Run: git tag -a $newTag -m <notes>"
            Write-Host "- Run: git push origin HEAD"
            Write-Host "- Run: git push origin $newTag"
            Write-Host "- Prune older tags/releases except $newTag"
        } else {
            Write-Host "- Skip git actions (not in a git repository)"
        }

        Write-Success "Dry-run completed"
        exit 0
    }

    Write-Info "Updating workspace version in Cargo.toml"
    Update-WorkspaceVersion -CargoTomlPath $cargoToml -OldVersion $currentVersion -NewVersion $Version
    $changedFiles.Add("Cargo.toml") | Out-Null

    Write-Info "Updating lockfile via cargo update --workspace"
    & cargo update --workspace
    if ($LASTEXITCODE -ne 0) {
        throw "cargo update --workspace failed"
    }

    if (Test-Path $lockPath) {
        $changedFiles.Add("Cargo.lock") | Out-Null
    }

    Write-Host ""
    Write-Host "Release Summary" -ForegroundColor White
    Write-Host "--------------" -ForegroundColor White
    Write-Host "Current version : $currentVersion"
    Write-Host "New version     : $Version"
    Write-Host "Tag             : $newTag"
    Write-Host "Files to stage  : $($changedFiles -join ', ')"
    Write-Host "Release notes:" -ForegroundColor White
    Write-Host $Notes
    Write-Host ""

    if ($isGitRepo) {
        Write-Info "Diff summary"
        & git --no-pager diff -- Cargo.toml Cargo.lock
        if ($LASTEXITCODE -ne 0) {
            throw "Failed to produce diff summary"
        }
    } else {
        Write-WarnLine "Skipping git diff summary because no git repository was detected."
    }

    $confirm = Read-Host "Proceed with build/test/release steps? (y/N)"
    if ($confirm -notin @('y', 'Y', 'yes', 'YES')) {
        throw "Release cancelled by user"
    }

    Write-Info "Running pre-release validation build"
    & cargo build --release
    if ($LASTEXITCODE -ne 0) {
        throw "cargo build --release failed"
    }

    Write-Info "Running full test suite"
    & cargo test
    if ($LASTEXITCODE -ne 0) {
        throw "cargo test failed"
    }

    if ($Force -and -not [string]::IsNullOrWhiteSpace($existingTag)) {
        Write-WarnLine "Removing existing local tag $newTag due to -Force"
        Invoke-Git @('tag', '-d', $newTag)

        $remoteTagExists = & git ls-remote --tags origin $newTag
        if ($LASTEXITCODE -eq 0 -and -not [string]::IsNullOrWhiteSpace($remoteTagExists)) {
            Write-WarnLine "Removing existing remote tag $newTag due to -Force"
            Invoke-Git @('push', 'origin', '--delete', $newTag)
        }
    }

    Write-Info "Staging release files"
    Invoke-Git @('add', 'Cargo.toml')
    if (Test-Path $lockPath) {
        Invoke-Git @('add', 'Cargo.lock')
    }

    Write-Info "Creating version bump commit"
    Invoke-Git @('commit', '-m', "chore: bump version to $Version")

    Write-Info "Creating annotated tag $newTag"
    Invoke-Git @('tag', '-a', $newTag, '-m', $Notes)

    Write-Info "Pushing commit and tag"
    Invoke-Git @('push', 'origin', 'HEAD')
    Invoke-Git @('push', 'origin', $newTag)

    Write-Info "Cleaning up older release tags"
    $allReleaseTags = (& git tag -l 'v*.*.*') | Where-Object { $_ -ne $newTag }
    foreach ($oldTag in $allReleaseTags) {
        if ([string]::IsNullOrWhiteSpace($oldTag)) { continue }

        Invoke-Git @('tag', '-d', $oldTag)

        $remoteTagExists = & git ls-remote --tags origin $oldTag
        if ($LASTEXITCODE -eq 0 -and -not [string]::IsNullOrWhiteSpace($remoteTagExists)) {
            Invoke-Git @('push', 'origin', '--delete', $oldTag)
        }

        $ghAvailable = Get-Command gh -ErrorAction SilentlyContinue
        if ($ghAvailable) {
            & gh release delete $oldTag --yes
            if ($LASTEXITCODE -ne 0) {
                Write-WarnLine "Could not delete GitHub release for $oldTag (continuing)"
            }
        }
    }

    $repoUrl = Get-RemoteHttpsUrl
    if ($repoUrl) {
        Write-Success "Release submitted. Monitor CI/CD: $repoUrl/actions"
    } else {
        Write-Success "Release submitted. Monitor CI/CD in your remote repository Actions page."
    }
}
catch {
    Write-ErrorLine $_.Exception.Message

    if ($DryRun) {
        exit 1
    }

    Write-WarnLine "Rolling back version file changes"
    Set-Content -Path $cargoToml -Value $originalCargoToml -Encoding UTF8

    if ($lockExistedBefore) {
        Set-Content -Path $lockPath -Value $originalLock -Encoding UTF8
    } elseif (Test-Path $lockPath) {
        Remove-Item -Path $lockPath -Force
    }

    Write-WarnLine "Rollback completed"
    exit 1
}
