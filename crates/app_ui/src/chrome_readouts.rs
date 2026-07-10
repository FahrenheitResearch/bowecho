//! AWIPS-style chrome readout formatting (ui-refresh spec §5): the pure
//! string builders behind the top-bar information strip and the status-bar
//! readout cluster.
//!
//! The split rule these encode: the TOP bar shows current-scan facts
//! (site, product, VCP, scan time + age, loop state); the BOTTOM bar shows
//! transient messages plus cursor/map readouts and the frame details the
//! top bar does not carry (frame status, live-chunk progress, source
//! attribution). Everything here formats state the app already computed —
//! nothing is re-derived.

/// Top-bar current-scan readout: `{site}  {product}  VCP {n}  {clock}  age
/// {age}` with two-space separators (AWIPS strip convention, monospace at
/// the call site). The VCP segment drops out when the volume carries none
/// (international, mobile, and synthetic-WRF feeds have no VCP).
pub(crate) fn top_bar_scan_readout(
    site: &str,
    product: &str,
    vcp: Option<u16>,
    clock: &str,
    age: &str,
) -> String {
    let vcp = vcp.map(|p| format!("VCP {p}  ")).unwrap_or_default();
    format!("{site}  {product}  {vcp}{clock}  age {age}")
}

/// Top-bar loop-state readout: `{i}/{N} PLAYING|PAUSED` (1-based frame
/// index, clamped into range so a mid-trim cursor can never print `13/12`).
/// `None` with no frames — the strip shows nothing instead of `0/0`.
pub(crate) fn top_bar_loop_readout(
    frame_index: usize,
    frame_count: usize,
    playing: bool,
) -> Option<String> {
    (frame_count > 0).then(|| {
        format!(
            "{}/{} {}",
            (frame_index + 1).min(frame_count),
            frame_count,
            if playing { "PLAYING" } else { "PAUSED" }
        )
    })
}

/// Status-bar condensed frame detail: the parts of the old full
/// frame-status line the top bar does NOT now show — frame status label,
/// the live-chunk readout when present, and the source attribution.
/// Site/scan-time/age are deliberately absent (they live in the top bar;
/// the caller puts the full line in the hover instead).
pub(crate) fn status_bar_frame_detail(
    status_label: &str,
    live_chunk: Option<&str>,
    source_label: &str,
) -> String {
    let chunk = live_chunk
        .map(|chunk| format!(" {chunk}"))
        .unwrap_or_default();
    format!("{status_label}{chunk} ({source_label})")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_readout_formats_awips_segments_and_drops_missing_vcp() {
        assert_eq!(
            top_bar_scan_readout("KEAX", "REF", Some(212), "2026-07-10 18:32:05Z", "2m14s"),
            "KEAX  REF  VCP 212  2026-07-10 18:32:05Z  age 2m14s"
        );
        // No VCP (international/mobile/synthetic volumes): segment gone,
        // spacing intact.
        assert_eq!(
            top_bar_scan_readout("ES-MAD", "REF", None, "2026-07-10 18:32:05Z", "4m"),
            "ES-MAD  REF  2026-07-10 18:32:05Z  age 4m"
        );
    }

    #[test]
    fn loop_readout_is_one_based_clamped_and_absent_without_frames() {
        assert_eq!(top_bar_loop_readout(0, 0, false), None);
        assert_eq!(
            top_bar_loop_readout(0, 1, false).as_deref(),
            Some("1/1 PAUSED")
        );
        assert_eq!(
            top_bar_loop_readout(2, 12, true).as_deref(),
            Some("3/12 PLAYING")
        );
        // Cursor past the end (mid-trim frame): clamps, never 13/12.
        assert_eq!(
            top_bar_loop_readout(12, 12, false).as_deref(),
            Some("12/12 PAUSED")
        );
    }

    #[test]
    fn frame_detail_carries_status_chunk_and_source_only() {
        assert_eq!(
            status_bar_frame_detail(
                "Live (partial)",
                Some("last chunk 18:32:41Z age 9s chunks 24 id 24 int"),
                "realtime L2",
            ),
            "Live (partial) last chunk 18:32:41Z age 9s chunks 24 id 24 int (realtime L2)"
        );
        assert_eq!(
            status_bar_frame_detail("Archive", None, "AWS archive"),
            "Archive (AWS archive)"
        );
    }
}
