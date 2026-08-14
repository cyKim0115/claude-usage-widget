# Stops running widget, force-rebuilds release, reinstalls to %LOCALAPPDATA%, restarts.
# Use after implementation tasks so the installed copy matches the source.

$ErrorActionPreference = "Stop"
& (Join-Path $PSScriptRoot "launch-user.ps1") -ForceRebuild
exit $LASTEXITCODE
