$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$ToolRoot = (Get-Item -LiteralPath $PSScriptRoot).FullName
$RepoRoot = (Get-Item -LiteralPath (Join-Path $ToolRoot '../../../..')).FullName
$Image = 'bowecho-pytmatrix:0.3.3-research'
$ContainerTool = 'crates/radar_scattering/tools/pytmatrix-0.3.3'
$AssetRoot = 'research_only_assets/tmatrix/pytmatrix-0.3.3'
$ValidationRoot = 'validation/tmatrix'

Push-Location $RepoRoot
try {
    docker build --progress=plain --platform linux/amd64 `
        -f "$ContainerTool/Dockerfile" -t $Image .
    if ($LASTEXITCODE -ne 0) { throw 'Docker build failed' }

    $ImageId = docker image inspect $Image --format '{{.Id}}'
    if ($LASTEXITCODE -ne 0 -or [string]::IsNullOrWhiteSpace($ImageId)) {
        throw 'Could not inspect built image ID'
    }

    docker run --rm --platform linux/amd64 `
        -e "BRSLUT_CONTAINER_IMAGE_ID=$ImageId" `
        -v "${RepoRoot}:/workspace" $Image `
        python "$ContainerTool/generate_lut.py" environment `
        --output "$AssetRoot/environment.json"
    if ($LASTEXITCODE -ne 0) { throw 'Environment capture failed' }

    $Tables = @(
        'conventional_liquid_rain_sband_unvalidated',
        'conventional_dry_ice_spheroids_sband_unvalidated',
        'conventional_wet_hail_sband_unvalidated'
    )
    foreach ($Table in $Tables) {
        docker run --rm --platform linux/amd64 `
            -v "${RepoRoot}:/workspace" $Image `
            python "$ContainerTool/generate_lut.py" generate `
            --config "$AssetRoot/$Table/config.json" `
            --output "$AssetRoot/$Table/table.lut" `
            --manifest "$AssetRoot/$Table/manifest.json" `
            --environment-report "$AssetRoot/environment.json" `
            --overwrite
        if ($LASTEXITCODE -ne 0) { throw "Generation failed for $Table" }
    }

    docker run --rm --platform linux/amd64 `
        -v "${RepoRoot}:/workspace" $Image `
        python "$ValidationRoot/run_validation.py" sanity `
        --tool-root $ContainerTool `
        --dry-config "$AssetRoot/conventional_dry_ice_spheroids_sband_unvalidated/config.json" `
        --wet-config "$AssetRoot/conventional_wet_hail_sband_unvalidated/config.json" `
        --environment-report "$AssetRoot/environment.json" `
        --output "$ValidationRoot/sanity_report.json"
    if ($LASTEXITCODE -ne 0) { throw 'Sanity checks failed to execute' }

    docker run --rm --platform linux/amd64 `
        -v "${RepoRoot}:/workspace" $Image `
        python "$ValidationRoot/select_held_out_nodes.py" `
        --tool-root $ContainerTool `
        --asset-root $AssetRoot `
        --output "$ValidationRoot/held_out_nodes.json"
    if ($LASTEXITCODE -ne 0) { throw 'Held-out node selection failed' }

    docker run --rm --platform linux/amd64 `
        -v "${RepoRoot}:/workspace" $Image `
        python "$ValidationRoot/run_validation.py" heldout `
        --tool-root $ContainerTool `
        --asset-root $AssetRoot `
        --nodes "$ValidationRoot/held_out_nodes.json" `
        --environment-report "$AssetRoot/environment.json" `
        --output "$ValidationRoot/held_out_interpolation_report.json"
    if ($LASTEXITCODE -ne 0) { throw 'Held-out interpolation checks failed to execute' }

    docker run --rm --platform linux/amd64 `
        -v "${RepoRoot}:/workspace" $Image `
        python -m unittest validation.tmatrix.test_generator -v
    if ($LASTEXITCODE -ne 0) { throw 'Generator contract tests failed' }

    $SanityReport = Get-Content -Raw -LiteralPath `
        (Join-Path $RepoRoot "$ValidationRoot/sanity_report.json") | ConvertFrom-Json
    $HeldOutReport = Get-Content -Raw -LiteralPath `
        (Join-Path $RepoRoot "$ValidationRoot/held_out_interpolation_report.json") | ConvertFrom-Json
    $Run = [ordered]@{
        schema = 1
        command = 'powershell -ExecutionPolicy Bypass -File crates/radar_scattering/tools/pytmatrix-0.3.3/run_all.ps1'
        image_id = $ImageId.Trim()
        build_status = 'passed'
        upstream_unittest_status = 'passed_during_image_build'
        generated_tables = $Tables
        solver_failure_count = 0
        sanity_checks_passed = [bool]$SanityReport.sanity_checks_passed
        interpolation_check_passed = [bool]$HeldOutReport.interpolation_check_passed
        environment_sha256 = (Get-FileHash -Algorithm SHA256 -LiteralPath `
            (Join-Path $RepoRoot "$AssetRoot/environment.json")).Hash.ToLowerInvariant()
        sanity_report_sha256 = (Get-FileHash -Algorithm SHA256 -LiteralPath `
            (Join-Path $RepoRoot "$ValidationRoot/sanity_report.json")).Hash.ToLowerInvariant()
        held_out_interpolation_report_sha256 = (Get-FileHash -Algorithm SHA256 -LiteralPath `
            (Join-Path $RepoRoot "$ValidationRoot/held_out_interpolation_report.json")).Hash.ToLowerInvariant()
        table_validation_status = 'research_only_unvalidated'
        production_activation = $false
        historical_failures = @('See crates/radar_scattering/tools/pytmatrix-0.3.3/FAILURE_RECORD.md')
    }
    $RunJson = ($Run | ConvertTo-Json -Depth 8) + "`n"
    [System.IO.File]::WriteAllText(
        (Join-Path $RepoRoot "$AssetRoot/reproduction_run.json"),
        $RunJson,
        [System.Text.UTF8Encoding]::new($false)
    )
}
finally {
    Pop-Location
}
