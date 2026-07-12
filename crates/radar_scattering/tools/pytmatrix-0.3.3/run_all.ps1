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
        'conventional_wet_hail_sband_unvalidated',
        'property_p3_ishmael_dry_oblate_sband_unvalidated',
        'property_p3_ishmael_dry_prolate_sband_unvalidated',
        'property_p3_ishmael_wet_oblate_sband_unvalidated',
        'property_p3_ishmael_wet_prolate_sband_unvalidated',
        'property_rain_sband_unvalidated'
    )

    $SolverReportPath = Join-Path $RepoRoot "$ValidationRoot/solver_refined_v9_convergence_report.json"
    $GridAuditPath = Join-Path $RepoRoot "$ValidationRoot/refined_grid_v9_full_axis_budget_report.json"
    if (-not (Test-Path -LiteralPath $SolverReportPath -PathType Leaf)) {
        throw 'Precomputed refined-v9 solver convergence report is missing'
    }
    if (-not (Test-Path -LiteralPath $GridAuditPath -PathType Leaf)) {
        throw 'Precomputed refined-v9 full grid-design audit is missing'
    }
    $SolverConvergenceReport = Get-Content -Raw -LiteralPath $SolverReportPath | ConvertFrom-Json
    if (-not [bool]$SolverConvergenceReport.solver_convergence_check_passed) {
        throw 'Solver convergence report failed'
    }
    $CurrentEnvironmentSha256 = (Get-FileHash -Algorithm SHA256 -LiteralPath `
        (Join-Path $RepoRoot "$AssetRoot/environment.json")).Hash.ToLowerInvariant()
    $CurrentGeneratorSha256 = (Get-FileHash -Algorithm SHA256 -LiteralPath `
        (Join-Path $RepoRoot "$ContainerTool/generate_lut.py")).Hash.ToLowerInvariant()
    $CurrentValidationSha256 = (Get-FileHash -Algorithm SHA256 -LiteralPath `
        (Join-Path $RepoRoot "$ValidationRoot/run_validation.py")).Hash.ToLowerInvariant()
    $CurrentGridAuditSha256 = (Get-FileHash -Algorithm SHA256 -LiteralPath `
        $GridAuditPath).Hash.ToLowerInvariant()
    if ($SolverConvergenceReport.environment_report_sha256 -ne $CurrentEnvironmentSha256) {
        throw 'Solver convergence environment hash is stale'
    }
    if ($SolverConvergenceReport.generator_source_sha256 -ne $CurrentGeneratorSha256) {
        throw 'Solver convergence generator hash is stale'
    }
    if ($SolverConvergenceReport.validation_source_sha256 -ne $CurrentValidationSha256) {
        throw 'Solver convergence validation-source hash is stale'
    }
    if ($SolverConvergenceReport.refined_grid_design_audit_sha256 -ne $CurrentGridAuditSha256) {
        throw 'Solver convergence grid-design audit hash is stale'
    }
    foreach ($Table in $Tables) {
        if ($Table -notlike 'property_*') { continue }
        $TableReport = $SolverConvergenceReport.tables | Where-Object asset_directory -eq $Table
        if ($null -eq $TableReport) { throw "Solver report omits $Table" }
        $CurrentConfigSha256 = (Get-FileHash -Algorithm SHA256 -LiteralPath `
            (Join-Path $RepoRoot "$AssetRoot/$Table/config.json")).Hash.ToLowerInvariant()
        if ($TableReport.config_sha256 -ne $CurrentConfigSha256) {
            throw "Solver convergence config hash is stale for $Table"
        }
    }

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

    $TableSha256 = [ordered]@{}
    $GridPointCounts = [ordered]@{}
    $ElevationNodeCounts = [ordered]@{}
    foreach ($Table in $Tables) {
        $LutPath = Join-Path $RepoRoot "$AssetRoot/$Table/table.lut"
        $ConfigPath = Join-Path $RepoRoot "$AssetRoot/$Table/config.json"
        $Digest = (Get-FileHash -Algorithm SHA256 -LiteralPath $LutPath).Hash.ToLowerInvariant()
        docker run --rm --platform linux/amd64 `
            -v "${RepoRoot}:/workspace" $Image `
            validate_tmatrix_lut `
            "$AssetRoot/$Table/table.lut" `
            "$AssetRoot/$Table/config.json" `
            $Digest
        if ($LASTEXITCODE -ne 0) { throw "Runtime loader smoke failed for $Table" }
        $Manifest = Get-Content -Raw -LiteralPath `
            (Join-Path $RepoRoot "$AssetRoot/$Table/manifest.json") | ConvertFrom-Json
        $Config = Get-Content -Raw -LiteralPath $ConfigPath | ConvertFrom-Json
        $ElevationAxis = $Config.axes | Where-Object kind -eq 'radar_elevation'
        $TableSha256[$Table] = $Digest
        $GridPointCounts[$Table] = [int64]$Manifest.grid_point_count
        $ElevationNodeCounts[$Table] = [int]$ElevationAxis.coordinates.Count
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
        python "$ValidationRoot/run_validation.py" property-sanity `
        --tool-root $ContainerTool `
        --asset-root $AssetRoot `
        --environment-report "$AssetRoot/environment.json" `
        --output "$ValidationRoot/property_sanity_report.json"
    if ($LASTEXITCODE -ne 0) { throw 'Property sanity checks failed to execute' }

    docker run --rm --platform linux/amd64 `
        -v "${RepoRoot}:/workspace" $Image `
        python "$ValidationRoot/select_held_out_nodes.py" `
        --tool-root $ContainerTool `
        --asset-root $AssetRoot `
        --asset-set all `
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
        python "$ValidationRoot/run_validation.py" property-view `
        --tool-root $ContainerTool `
        --asset-root $AssetRoot `
        --environment-report "$AssetRoot/environment.json" `
        --output "$ValidationRoot/property_view_interpolation_report.json"
    if ($LASTEXITCODE -ne 0) { throw 'Property view checks failed to execute' }

    docker run --rm --platform linux/amd64 `
        -v "${RepoRoot}:/workspace" $Image `
        python -m unittest validation.tmatrix.test_generator -v
    if ($LASTEXITCODE -ne 0) { throw 'Generator contract tests failed' }

    $SanityReport = Get-Content -Raw -LiteralPath `
        (Join-Path $RepoRoot "$ValidationRoot/sanity_report.json") | ConvertFrom-Json
    $HeldOutReport = Get-Content -Raw -LiteralPath `
        (Join-Path $RepoRoot "$ValidationRoot/held_out_interpolation_report.json") | ConvertFrom-Json
    $PropertySanityReport = Get-Content -Raw -LiteralPath `
        (Join-Path $RepoRoot "$ValidationRoot/property_sanity_report.json") | ConvertFrom-Json
    $PropertyViewReport = Get-Content -Raw -LiteralPath `
        (Join-Path $RepoRoot "$ValidationRoot/property_view_interpolation_report.json") | ConvertFrom-Json
    if (-not [bool]$SanityReport.sanity_checks_passed) {
        throw 'Conventional sanity report failed'
    }
    if (-not [bool]$PropertySanityReport.sanity_checks_passed) {
        throw 'Property sanity report failed'
    }
    if (-not [bool]$HeldOutReport.interpolation_check_passed) {
        throw 'Held-out interpolation report failed'
    }
    if (-not [bool]$PropertyViewReport.view_interpolation_check_passed) {
        throw 'Property view interpolation report failed'
    }
    $Run = [ordered]@{
        schema = 1
        command = 'powershell -ExecutionPolicy Bypass -File crates/radar_scattering/tools/pytmatrix-0.3.3/run_all.ps1'
        image_id = $ImageId.Trim()
        build_status = 'passed'
        upstream_unittest_status = 'passed_during_image_build'
        generated_tables = $Tables
        table_lut_sha256 = $TableSha256
        grid_point_counts = $GridPointCounts
        radar_elevation_node_counts = $ElevationNodeCounts
        solver_failure_count = 0
        runtime_loader_smoke_passed = $true
        solver_convergence_check_passed = [bool]$SolverConvergenceReport.solver_convergence_check_passed
        sanity_checks_passed = [bool]$SanityReport.sanity_checks_passed
        property_sanity_checks_passed = [bool]$PropertySanityReport.sanity_checks_passed
        interpolation_check_passed = [bool]$HeldOutReport.interpolation_check_passed
        property_view_interpolation_check_passed = [bool]$PropertyViewReport.view_interpolation_check_passed
        environment_sha256 = (Get-FileHash -Algorithm SHA256 -LiteralPath `
            (Join-Path $RepoRoot "$AssetRoot/environment.json")).Hash.ToLowerInvariant()
        sanity_report_sha256 = (Get-FileHash -Algorithm SHA256 -LiteralPath `
            (Join-Path $RepoRoot "$ValidationRoot/sanity_report.json")).Hash.ToLowerInvariant()
        held_out_interpolation_report_sha256 = (Get-FileHash -Algorithm SHA256 -LiteralPath `
            (Join-Path $RepoRoot "$ValidationRoot/held_out_interpolation_report.json")).Hash.ToLowerInvariant()
        property_sanity_report_sha256 = (Get-FileHash -Algorithm SHA256 -LiteralPath `
            (Join-Path $RepoRoot "$ValidationRoot/property_sanity_report.json")).Hash.ToLowerInvariant()
        property_view_interpolation_report_sha256 = (Get-FileHash -Algorithm SHA256 -LiteralPath `
            (Join-Path $RepoRoot "$ValidationRoot/property_view_interpolation_report.json")).Hash.ToLowerInvariant()
        solver_convergence_report_sha256 = (Get-FileHash -Algorithm SHA256 -LiteralPath `
            $SolverReportPath).Hash.ToLowerInvariant()
        refined_grid_design_audit_sha256 = (Get-FileHash -Algorithm SHA256 -LiteralPath `
            $GridAuditPath).Hash.ToLowerInvariant()
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
