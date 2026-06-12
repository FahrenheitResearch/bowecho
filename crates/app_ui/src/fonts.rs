//! CJK font fallback so Japanese text (JMA site names like 秋田, UI
//! strings like レーダー) renders as glyphs instead of tofu boxes.
//!
//! The embedded font is a Noto Sans JP Regular subset (SIL OFL 1.1 —
//! see assets/OFL-LICENSE.txt for the source URL and the exact kept
//! ranges: kana, katakana phonetic extensions, JP punctuation,
//! half/fullwidth forms, and the CJK Unified Ideographs block). It is
//! appended to the END of both egui font families, so it only resolves
//! glyphs no default font covers — Latin rendering stays pixel-identical.

use eframe::egui;

/// Noto Sans JP Regular, subset to U+3000-303F, U+3040-309F,
/// U+30A0-30FF, U+31F0-31FF, U+FF00-FFEF, U+4E00-9FFF (pyftsubset; no
/// Latin, deliberately — fallback only).
static NOTO_SANS_JP_SUBSET: &[u8] = include_bytes!("../assets/NotoSansJP-Subset.ttf");

const FALLBACK_FONT_NAME: &str = "NotoSansJP-Subset";

/// Registers the CJK fallback on `ctx`. Called once at app construction
/// (before the first frame); cheap to call again — egui diffs the
/// definitions and rebuilds only on change.
pub(crate) fn install_cjk_fallback(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    fonts.font_data.insert(
        FALLBACK_FONT_NAME.to_owned(),
        std::sync::Arc::new(egui::FontData::from_static(NOTO_SANS_JP_SUBSET)),
    );
    // LAST in both families: a fallback must never shadow the default
    // fonts' glyphs (Latin metrics stay byte-for-byte what they were).
    for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
        fonts
            .families
            .entry(family)
            .or_default()
            .push(FALLBACK_FONT_NAME.to_owned());
    }
    ctx.set_fonts(fonts);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The embedded bytes are a parseable font: FontData construction
    /// plus a full egui font build (Context::run forces epaint to parse
    /// every registered face) must not panic.
    #[test]
    fn embedded_font_bytes_parse() {
        let data = egui::FontData::from_static(NOTO_SANS_JP_SUBSET);
        assert!(!data.font.is_empty());
        // TrueType sfnt magic (0x00010000) — guards against an asset
        // swap accidentally embedding woff2/zip.
        assert_eq!(&data.font[..4], &[0x00, 0x01, 0x00, 0x00]);

        let ctx = egui::Context::default();
        install_cjk_fallback(&ctx);
        let _ = ctx.run_ui(egui::RawInput::default(), |_ui| {});
    }

    /// Japanese text lays out through real glyphs — coverage, not the
    /// tofu replacement (which would still have nonzero width, hence the
    /// has_glyphs checks and the no-fallback control).
    #[test]
    fn japanese_text_lays_out_with_glyph_coverage() {
        let font_id = egui::FontId::proportional(16.0);

        // Control: the default fonts have no kana/kanji coverage.
        let bare = egui::Context::default();
        let _ = bare.run_ui(egui::RawInput::default(), |ui| {
            ui.ctx().fonts_mut(|fonts| {
                assert!(
                    !fonts.has_glyphs(&font_id, "秋田"),
                    "default fonts unexpectedly cover kanji — fallback test is vacuous"
                );
            });
        });

        let ctx = egui::Context::default();
        install_cjk_fallback(&ctx);
        let _ = ctx.run_ui(egui::RawInput::default(), |ui| {
            ui.ctx().fonts_mut(|fonts| {
                // 秋田 — the Akita JMA site; レーダー — "radar" (katakana,
                // long-vowel mark); 「」。 — JP punctuation.
                for text in ["秋田", "レーダー", "「秋田レーダー」。"] {
                    assert!(fonts.has_glyphs(&font_id, text), "missing glyphs: {text}");
                    let galley = fonts.layout_no_wrap(
                        text.to_owned(),
                        font_id.clone(),
                        egui::Color32::WHITE,
                    );
                    assert!(galley.size().x > 0.0, "zero-width galley: {text}");
                }
                // Latin still resolves through the default fonts, not the
                // CJK subset (which carries no Latin glyphs at all).
                assert!(fonts.has_glyphs(&font_id, "KEAX 0.5deg"));
            });
        });
    }
}
