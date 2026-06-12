//! Live probe for the built-in international ODIM providers.
//!
//! For every provider in `data_source::international::intl_providers()`:
//! list sites, print the newest [`FramePlan`] for one site, then download
//! each plan part with `fetch_volume_bytes` and decode it through the
//! shared `nexrad_io::decode_supported_volume_bytes` router (ODIM_H5 per
//! EUMETNET OPERA Data Information Model; Michelson et al., OPERA WP
//! 2.1/2.2, v2.2-2.3), printing site id, cut count, and moment names.
//!
//! Usage:
//!   cargo run -p data_source --example intl_probe
//!   cargo run -p data_source --example intl_probe -- --plan-only
//!
//! `--plan-only` skips the download/decode stage (FMI volumes run ~16-24
//! MB, which is worth skipping on constrained links).

use std::collections::BTreeSet;

use data_source::international::{FramePlan, IntlProvider, intl_providers};

/// Per-provider preferred probe site (the ones validated in the
/// implementation dossier); falls back to the provider's first site.
const PREFERRED_SITES: &[(&str, &str)] = &[
    ("smhi", "angelholm"),
    ("dmi", "06177"),
    ("geosphere", "hochficht"),
    ("fmi", "fianj"),
];

fn main() {
    let plan_only = std::env::args().any(|arg| arg == "--plan-only");
    let mut failures = 0usize;

    for provider in intl_providers() {
        println!("== {} ({}) ==", provider.label(), provider.id());
        if let Err(message) = probe_provider(provider.as_ref(), plan_only) {
            failures += 1;
            println!("  FAILED: {message}");
        }
        println!();
    }

    if failures > 0 {
        eprintln!("{failures} provider(s) failed");
        std::process::exit(1);
    }
}

fn probe_provider(provider: &dyn IntlProvider, plan_only: bool) -> Result<(), String> {
    let sites = provider.list_sites()?;
    println!("  sites: {}", sites.len());
    for site in sites.iter().take(4) {
        println!("    {} ({})", site.site_id, site.label);
    }
    if sites.len() > 4 {
        println!("    ... and {} more", sites.len() - 4);
    }

    let preferred = PREFERRED_SITES
        .iter()
        .find(|(id, _)| *id == provider.id())
        .map(|(_, site)| *site);
    let site = preferred
        .and_then(|wanted| sites.iter().find(|site| site.site_id == wanted))
        .or_else(|| sites.first())
        .ok_or_else(|| "provider listed no sites".to_owned())?;
    println!("  probing site: {} ({})", site.site_id, site.label);

    let plan = provider.latest(&site.site_id)?;
    println!("  plan identity: {}", plan.identity);
    println!("  plan merge: {}", plan.merge);
    for part in &plan.parts {
        println!("  plan part: {}", part.url);
    }

    if plan_only {
        println!("  (plan-only: download/decode skipped)");
        return Ok(());
    }
    decode_plan(&plan)
}

fn decode_plan(plan: &FramePlan) -> Result<(), String> {
    for part in &plan.parts {
        let bytes = data_source::fetch_volume_bytes(&part.url)
            .map_err(|err| format!("download {}: {err}", part.url))?;
        println!("  downloaded: {} bytes", bytes.len());
        let volume = nexrad_io::decode_supported_volume_bytes(&bytes)
            .map_err(|err| format!("decode {}: {err}", part.url))?;
        let moments = volume
            .cuts
            .iter()
            .flat_map(|cut| cut.moments.keys())
            .map(|moment| moment.short_name().to_owned())
            .collect::<BTreeSet<_>>();
        println!(
            "  decoded: site={} name={} cuts={} moments=[{}]",
            volume.site.id,
            volume.site.name.as_deref().unwrap_or("-"),
            volume.cuts.len(),
            moments.into_iter().collect::<Vec<_>>().join(", ")
        );
    }
    Ok(())
}
