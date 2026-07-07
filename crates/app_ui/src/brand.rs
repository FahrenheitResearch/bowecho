//! egui/image adapter for the persisted App Identity / Brand Kit.
//!
//! Presets and validation live in `settings`; this module is the single place
//! that turns those toolkit-neutral values into runtime icons, textures,
//! previews, and screenshot/share-card chrome.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use eframe::egui;

pub(crate) fn window_title(brand: &settings::BrandConfig) -> String {
    brand.resolved_display_name().to_owned()
}

pub(crate) fn github_latest_release_api_url(repo_url: &str) -> Option<String> {
    let (owner, name) = github_repo_parts(repo_url)?;
    Some(format!(
        "https://api.github.com/repos/{owner}/{name}/releases/latest"
    ))
}

/// Canonical public repo — the update feed for stock builds and for brand
/// kits that leave `repo_url` empty.
pub(crate) const CANONICAL_REPO_URL: &str = "https://github.com/FahrenheitResearch/bowecho";

/// The repo a brand's `repo_url` actually means — the ONE place the
/// "empty means canonical" rule lives. An empty `repo_url` is the stock exe
/// wearing brand-kit assets, so it resolves to the canonical repo; anything
/// else is the distributor's own override, verbatim (trimmed). Every
/// consumer — the passive update check, the releases page, and the
/// self-update repo pin — must resolve through here so ".git"/trailing-slash
/// spellings behave identically across all of them.
pub(crate) fn effective_repo_url(repo_url: &str) -> &str {
    let trimmed = repo_url.trim();
    if trimmed.is_empty() {
        CANONICAL_REPO_URL
    } else {
        trimmed
    }
}

/// Endpoint for the passive update check. An empty `repo_url` still checks
/// the canonical feed (see [`effective_repo_url`]); only an explicit
/// non-GitHub override disables the check (the distributor is self-hosting
/// releases).
pub(crate) fn update_check_api_url(repo_url: &str) -> Option<String> {
    github_latest_release_api_url(effective_repo_url(repo_url))
}

pub(crate) fn releases_page_url(brand: &settings::BrandConfig) -> Option<String> {
    brand
        .valid_http_url(&brand.releases_url)
        .map(str::to_owned)
        .or_else(|| {
            let (owner, name) = github_repo_parts(effective_repo_url(&brand.repo_url))?;
            Some(format!("https://github.com/{owner}/{name}/releases"))
        })
}

pub(crate) fn github_repo_parts(repo_url: &str) -> Option<(&str, &str)> {
    let repo = repo_url.trim().trim_end_matches('/');
    let repo = repo.strip_suffix(".git").unwrap_or(repo);
    let repo = repo
        .strip_prefix("https://github.com/")
        .or_else(|| repo.strip_prefix("http://github.com/"))?;
    let mut parts = repo.split('/').filter(|part| !part.is_empty());
    let owner = parts.next()?;
    let name = parts.next()?;
    parts.next().is_none().then_some((owner, name))
}

pub(crate) fn configured_app_icon(brand: &settings::BrandConfig) -> Option<egui::IconData> {
    let path = settings::BrandAssets::existing_file(&brand.assets.app_icon_png)?;
    let bytes = std::fs::read(path).ok()?;
    eframe::icon_data::from_png_bytes(&bytes).ok()
}

#[derive(Default)]
pub(crate) struct BrandTextureCache {
    entries: BTreeMap<&'static str, CachedTexture>,
}

struct CachedTexture {
    path: PathBuf,
    size: [usize; 2],
    texture: egui::TextureHandle,
}

impl BrandTextureCache {
    pub(crate) fn clear(&mut self) {
        self.entries.clear();
    }

    pub(crate) fn texture(
        &mut self,
        ctx: &egui::Context,
        role: &'static str,
        configured_path: Option<&str>,
    ) -> Option<(egui::TextureHandle, [usize; 2])> {
        let path = configured_path
            .map(str::trim)
            .filter(|path| !path.is_empty())
            .map(PathBuf::from)?;
        if !path.is_file() {
            self.entries.remove(role);
            return None;
        }
        if let Some(cached) = self.entries.get(role)
            && cached.path == path
        {
            return Some((cached.texture.clone(), cached.size));
        }

        let image = load_color_image(&path)?;
        let size = image.size;
        let texture = ctx.load_texture(
            format!("brand/{role}/{}", stable_path_label(&path)),
            image,
            egui::TextureOptions::LINEAR,
        );
        self.entries.insert(
            role,
            CachedTexture {
                path,
                size,
                texture: texture.clone(),
            },
        );
        Some((texture, size))
    }
}

fn load_color_image(path: &Path) -> Option<egui::ColorImage> {
    let bytes = std::fs::read(path).ok()?;
    let image = image::load_from_memory(&bytes).ok()?;
    let image = if image.width().max(image.height()) > 2048 {
        image.thumbnail(2048, 2048)
    } else {
        image
    };
    let rgba = image.to_rgba8();
    let size = [rgba.width() as usize, rgba.height() as usize];
    Some(egui::ColorImage::from_rgba_unmultiplied(
        size,
        rgba.as_raw(),
    ))
}

fn stable_path_label(path: &Path) -> String {
    path.file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("asset")
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.') {
                character
            } else {
                '_'
            }
        })
        .collect()
}

pub(crate) struct ShareMetadata {
    pub(crate) site: String,
    pub(crate) time: String,
    pub(crate) product: String,
}

pub(crate) fn paint_capture_overlay(
    ctx: &egui::Context,
    map_rect: egui::Rect,
    brand: &settings::BrandConfig,
    metadata: &ShareMetadata,
    textures: &mut BrandTextureCache,
) {
    if !brand.sharing.watermark_enabled && !brand.sharing.card_enabled {
        return;
    }
    if map_rect.width() < 180.0 || map_rect.height() < 100.0 {
        return;
    }

    let palette = brand.resolved_palette();
    let painter = ctx.layer_painter(egui::LayerId::new(
        egui::Order::Foreground,
        egui::Id::new("brand_capture_overlay"),
    ));
    let safe_rect = map_rect.shrink(18.0);

    if brand.sharing.card_enabled {
        let card_width = safe_rect.width().min(620.0);
        let card_height = safe_rect.height().min(112.0);
        let card = egui::Rect::from_min_size(
            safe_rect.left_bottom() - egui::vec2(0.0, card_height),
            egui::vec2(card_width, card_height),
        );
        if let Some((texture, _)) = textures.texture(
            ctx,
            "share-card-background",
            brand.assets.share_card_background.as_deref(),
        ) {
            painter.image(
                texture.id(),
                card,
                egui::Rect::from_min_max(egui::Pos2::ZERO, egui::pos2(1.0, 1.0)),
                egui::Color32::WHITE,
            );
        }
        painter.rect_filled(
            card,
            6.0,
            color_with_alpha(
                palette.surface,
                if brand.assets.share_card_background.is_some() {
                    218
                } else {
                    236
                },
            ),
        );
        painter.rect_stroke(
            card,
            6.0,
            egui::Stroke::new(1.0, color32(palette.outline)),
            egui::StrokeKind::Inside,
        );

        let title = non_empty_or(&brand.sharing.title, brand.resolved_display_name());
        let subtitle = non_empty_or(&brand.sharing.subtitle, brand.resolved_tagline());
        let detail = [
            brand.sharing.site_label.trim(),
            metadata.site.trim(),
            metadata.time.trim(),
            metadata.product.trim(),
        ]
        .into_iter()
        .filter(|value| !value.is_empty())
        .collect::<Vec<_>>()
        .join(" · ");
        let footer = brand.sharing.source_footer.trim();
        let x = card.left() + 14.0;
        painter.text(
            egui::pos2(x, card.top() + 12.0),
            egui::Align2::LEFT_TOP,
            truncate(title, 58),
            egui::FontId::proportional(20.0),
            color32(palette.text),
        );
        painter.text(
            egui::pos2(x, card.top() + 40.0),
            egui::Align2::LEFT_TOP,
            truncate(subtitle, 82),
            egui::FontId::proportional(13.0),
            color32(palette.muted_text),
        );
        if !detail.is_empty() {
            painter.text(
                egui::pos2(x, card.top() + 62.0),
                egui::Align2::LEFT_TOP,
                truncate(&detail, 90),
                egui::FontId::monospace(12.0),
                color32(palette.accent),
            );
        }
        if !footer.is_empty() {
            painter.text(
                egui::pos2(x, card.bottom() - 10.0),
                egui::Align2::LEFT_BOTTOM,
                truncate(footer, 118),
                egui::FontId::proportional(9.0),
                color32(palette.muted_text),
            );
        }
    }

    if brand.sharing.watermark_enabled {
        let anchor = safe_rect.right_bottom();
        let path = brand
            .assets
            .social_watermark
            .as_deref()
            .or(brand.assets.header_logo.as_deref());
        if let Some((texture, size)) = textures.texture(ctx, "social-watermark", path) {
            let source_size = egui::vec2(size[0] as f32, size[1] as f32);
            let scale = (54.0 / source_size.y.max(1.0)).min(180.0 / source_size.x.max(1.0));
            let draw_size = source_size * scale;
            let rect = egui::Rect::from_min_size(anchor - draw_size, draw_size);
            painter.image(
                texture.id(),
                rect,
                egui::Rect::from_min_max(egui::Pos2::ZERO, egui::pos2(1.0, 1.0)),
                egui::Color32::from_white_alpha(230),
            );
        } else {
            let label = brand.resolved_short_name();
            let galley = painter.layout_no_wrap(
                label.to_owned(),
                egui::FontId::proportional(18.0),
                color32(palette.text),
            );
            let rect = egui::Rect::from_min_size(
                anchor - galley.size() - egui::vec2(18.0, 12.0),
                galley.size() + egui::vec2(18.0, 12.0),
            );
            painter.rect_filled(rect, 5.0, color_with_alpha(palette.surface, 220));
            painter.rect_stroke(
                rect,
                5.0,
                egui::Stroke::new(1.0, color32(palette.primary)),
                egui::StrokeKind::Inside,
            );
            painter.galley(
                rect.min + egui::vec2(9.0, 6.0),
                galley,
                color32(palette.text),
            );
        }
    }
}

pub(crate) fn preview_ui(
    ui: &mut egui::Ui,
    brand: &settings::BrandConfig,
    textures: &mut BrandTextureCache,
) {
    let palette = brand.resolved_palette();
    ui.label(egui::RichText::new("Preview").strong());
    ui.horizontal_wrapped(|ui| {
        ui.group(|ui| {
            ui.set_min_width(170.0);
            ui.horizontal(|ui| {
                if let Some((texture, size)) = textures.texture(
                    ui.ctx(),
                    "header-logo-preview",
                    brand.assets.header_logo.as_deref(),
                ) {
                    let source = egui::vec2(size[0] as f32, size[1] as f32);
                    let scale = (22.0 / source.y.max(1.0)).min(72.0 / source.x.max(1.0));
                    ui.add(egui::Image::new((texture.id(), source * scale)));
                }
                ui.label(
                    egui::RichText::new(brand.resolved_display_name())
                        .strong()
                        .color(color32(palette.text)),
                );
            });
            ui.weak(truncate(brand.resolved_tagline(), 42));
        });

        ui.group(|ui| {
            ui.set_min_width(116.0);
            ui.label("Watermark");
            ui.label(
                egui::RichText::new(brand.resolved_short_name())
                    .size(18.0)
                    .strong()
                    .color(color32(palette.primary)),
            );
        });

        let (rect, _) = ui.allocate_exact_size(egui::vec2(270.0, 152.0), egui::Sense::hover());
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 6.0, color32(palette.surface));
        painter.rect_stroke(
            rect,
            6.0,
            egui::Stroke::new(1.0, color32(palette.outline)),
            egui::StrokeKind::Inside,
        );
        if let Some((texture, _)) = textures.texture(
            ui.ctx(),
            "share-card-preview",
            brand.assets.share_card_background.as_deref(),
        ) {
            painter.image(
                texture.id(),
                rect,
                egui::Rect::from_min_max(egui::Pos2::ZERO, egui::pos2(1.0, 1.0)),
                egui::Color32::from_white_alpha(95),
            );
        }
        painter.text(
            rect.left_top() + egui::vec2(12.0, 12.0),
            egui::Align2::LEFT_TOP,
            truncate(
                non_empty_or(&brand.sharing.title, brand.resolved_display_name()),
                35,
            ),
            egui::FontId::proportional(17.0),
            color32(palette.text),
        );
        painter.text(
            rect.left_top() + egui::vec2(12.0, 39.0),
            egui::Align2::LEFT_TOP,
            truncate(
                non_empty_or(&brand.sharing.subtitle, brand.resolved_tagline()),
                46,
            ),
            egui::FontId::proportional(11.0),
            color32(palette.muted_text),
        );
        painter.text(
            rect.left_bottom() + egui::vec2(12.0, -12.0),
            egui::Align2::LEFT_BOTTOM,
            format!(
                "{} · 18:42Z · {}",
                non_empty_or(&brand.sharing.site_label, "SITE"),
                brand.features.map
            ),
            egui::FontId::monospace(10.0),
            color32(palette.accent),
        );
    });
}

pub(crate) fn color32(rgb: [u8; 3]) -> egui::Color32 {
    egui::Color32::from_rgb(rgb[0], rgb[1], rgb[2])
}

fn color_with_alpha(rgb: [u8; 3], alpha: u8) -> egui::Color32 {
    egui::Color32::from_rgba_unmultiplied(rgb[0], rgb[1], rgb[2], alpha)
}

fn non_empty_or<'a>(value: &'a str, fallback: &'a str) -> &'a str {
    let value = value.trim();
    if value.is_empty() { fallback } else { value }
}

fn truncate(value: &str, max_chars: usize) -> String {
    let mut chars = value.chars();
    let mut output = chars.by_ref().take(max_chars).collect::<String>();
    if chars.next().is_some() {
        output.push('…');
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn brand_window_title_helper_uses_display_name() {
        let mut brand = settings::BrandConfig::default();
        assert_eq!(window_title(&brand), "BowEcho");

        brand.display_name = "Generic Weather App".to_owned();
        assert_eq!(window_title(&brand), "Generic Weather App");
    }

    #[test]
    fn brand_release_api_follows_configured_github_repo() {
        assert_eq!(
            github_latest_release_api_url("https://github.com/example/custom-app"),
            Some("https://api.github.com/repos/example/custom-app/releases/latest".to_owned())
        );
        assert_eq!(github_latest_release_api_url("https://example.org/"), None);
        let brand = settings::BrandConfig {
            repo_url: "https://github.com/example/custom-app".to_owned(),
            releases_url: String::new(),
            ..settings::BrandConfig::default()
        };
        assert_eq!(
            releases_page_url(&brand),
            Some("https://github.com/example/custom-app/releases".to_owned())
        );
    }

    #[test]
    fn empty_brand_repo_falls_back_to_the_canonical_update_feed() {
        // A brand kit without a repo override is the stock exe with different
        // assets — it must keep checking (and reporting "up to date") against
        // the canonical releases instead of "not configured".
        assert_eq!(
            update_check_api_url(""),
            Some(
                "https://api.github.com/repos/FahrenheitResearch/bowecho/releases/latest"
                    .to_owned()
            )
        );
        assert_eq!(
            update_check_api_url("https://github.com/example/custom-app"),
            Some("https://api.github.com/repos/example/custom-app/releases/latest".to_owned())
        );
        // An explicit non-GitHub override means self-hosted releases: no check.
        assert_eq!(update_check_api_url("https://example.org/releases"), None);

        let brand = settings::BrandConfig {
            repo_url: String::new(),
            releases_url: String::new(),
            ..settings::BrandConfig::default()
        };
        assert_eq!(
            releases_page_url(&brand),
            Some("https://github.com/FahrenheitResearch/bowecho/releases".to_owned())
        );

        // ".git" / trailing-slash spellings of the canonical repo resolve to
        // the same feed as the empty fallback — every consumer goes through
        // `effective_repo_url` + `github_repo_parts`, so the update check
        // and the self-update pin cannot disagree over a suffix.
        for spelling in [
            "https://github.com/FahrenheitResearch/bowecho.git",
            "https://github.com/FahrenheitResearch/bowecho/",
            "  https://github.com/FahrenheitResearch/bowecho  ",
        ] {
            assert_eq!(
                update_check_api_url(spelling),
                update_check_api_url(""),
                "{spelling:?} must hit the canonical feed"
            );
            assert_eq!(
                github_repo_parts(effective_repo_url(spelling)),
                github_repo_parts(CANONICAL_REPO_URL),
                "{spelling:?} must parse to the canonical repo"
            );
        }
        assert_eq!(effective_repo_url(""), CANONICAL_REPO_URL);
        assert_eq!(effective_repo_url("   "), CANONICAL_REPO_URL);
        assert_eq!(
            effective_repo_url(" https://example.org/releases "),
            "https://example.org/releases"
        );
    }
}
