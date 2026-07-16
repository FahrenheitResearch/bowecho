//! Lazy, UI-local loading for the official SPC mesoscale-discussion graphic.
//!
//! SPC publishes each text product as `mdNNNN.html` beside a matching
//! `mcdNNNN.png`.  Keep this presentation cache in egui memory rather than in
//! [`ViewerApp`](crate::ViewerApp): selecting an MD starts one bounded worker,
//! and switching away does not add another long-lived application field.

use std::sync::{Arc, Mutex};
use std::thread;

use eframe::egui;

const MAX_IMAGE_EDGE: u32 = 4_096;
const MAX_IMAGE_PIXELS: u64 = 16_000_000;
const MIN_VIEWER_ZOOM: f32 = 0.1;
const MAX_VIEWER_ZOOM: f32 = 4.0;

enum ImageState {
    Loading,
    Decoded(Option<egui::ColorImage>),
    Ready {
        texture: egui::TextureHandle,
        size: egui::Vec2,
    },
    Failed(String),
}

type SharedImageState = Arc<Mutex<ImageState>>;

#[derive(Clone, Copy, Debug)]
struct ImageViewerState {
    open: bool,
    zoom: f32,
}

impl Default for ImageViewerState {
    fn default() -> Self {
        Self {
            open: false,
            zoom: 1.0,
        }
    }
}

/// Draw the official SPC graphic for a selected mesoscale discussion.
///
/// The product text remains the authoritative fallback. A failed graphic
/// never hides it and can still be opened through the source hyperlink.
pub(crate) fn show(
    ui: &mut egui::Ui,
    event_family: &str,
    source_url: Option<&str>,
    event_id: &str,
) {
    if event_family != "mesoscale discussion" {
        return;
    }
    let Some(source_url) = source_url else {
        return;
    };
    let Some(image_url) = image_url_from_product_url(source_url) else {
        return;
    };

    ui.separator();
    ui.horizontal(|ui| {
        ui.strong("Official SPC graphic");
        ui.hyperlink_to("Open product", source_url);
    });

    let cache_id = egui::Id::new(("spc-md-image", event_id, image_url.as_str()));
    let (shared, start_worker) = ui.ctx().data_mut(|data| {
        if let Some(shared) = data.get_temp::<SharedImageState>(cache_id) {
            (shared, false)
        } else {
            let shared = Arc::new(Mutex::new(ImageState::Loading));
            data.insert_temp(cache_id, Arc::clone(&shared));
            (shared, true)
        }
    });

    if start_worker {
        let worker_state = Arc::clone(&shared);
        let worker_ctx = ui.ctx().clone();
        thread::spawn(move || {
            let result = fetch_color_image(&image_url);
            if let Ok(mut state) = worker_state.lock() {
                *state = match result {
                    Ok(image) => ImageState::Decoded(Some(image)),
                    Err(error) => ImageState::Failed(error),
                };
            }
            worker_ctx.request_repaint();
        });
    }

    let decoded = shared.lock().ok().and_then(|mut state| match &mut *state {
        ImageState::Decoded(image) => image.take(),
        _ => None,
    });
    if let Some(image) = decoded {
        let size = egui::vec2(image.size[0] as f32, image.size[1] as f32);
        let texture = ui.ctx().load_texture(
            format!("spc-md/{event_id}"),
            image,
            egui::TextureOptions::LINEAR,
        );
        if let Ok(mut state) = shared.lock() {
            *state = ImageState::Ready { texture, size };
        }
    }

    let snapshot = shared.lock().ok().map(|state| match &*state {
        ImageState::Loading | ImageState::Decoded(_) => ImageSnapshot::Loading,
        ImageState::Ready { texture, size } => ImageSnapshot::Ready(texture.clone(), *size),
        ImageState::Failed(error) => ImageSnapshot::Failed(error.clone()),
    });
    match snapshot {
        Some(ImageSnapshot::Ready(texture, source_size)) => {
            let max_width = ui.available_width().max(1.0);
            let max_height = 360.0;
            let scale = (max_width / source_size.x)
                .min(max_height / source_size.y)
                .min(1.0);
            let response = ui
                .add(
                    egui::Image::new((texture.id(), source_size * scale))
                        .sense(egui::Sense::click()),
                )
                .on_hover_cursor(egui::CursorIcon::ZoomIn)
                .on_hover_text("Click for a larger, zoomable view");
            let viewer_id = egui::Id::new(("spc-md-image-viewer", event_id));
            let mut viewer = ui
                .ctx()
                .data_mut(|data| data.get_temp::<ImageViewerState>(viewer_id))
                .unwrap_or_default();
            if response.clicked() {
                viewer.open = true;
            }
            if viewer.open {
                show_image_viewer(
                    ui.ctx(),
                    event_id,
                    source_url,
                    &texture,
                    source_size,
                    &mut viewer,
                );
            }
            ui.ctx()
                .data_mut(|data| data.insert_temp(viewer_id, viewer));
        }
        Some(ImageSnapshot::Failed(error)) => {
            ui.weak(format!(
                "Graphic unavailable; product text is still shown ({error})"
            ));
        }
        _ => {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.weak("Loading SPC graphic…");
            });
        }
    }
}

fn show_image_viewer(
    ctx: &egui::Context,
    event_id: &str,
    source_url: &str,
    texture: &egui::TextureHandle,
    source_size: egui::Vec2,
    state: &mut ImageViewerState,
) {
    let mut open = state.open;
    let mut zoom = state.zoom.clamp(MIN_VIEWER_ZOOM, MAX_VIEWER_ZOOM);
    egui::Window::new(format!("Official SPC graphic - {event_id}"))
        .id(egui::Id::new(("spc-md-image-window", event_id)))
        .open(&mut open)
        .default_size(egui::vec2(900.0, 700.0))
        .min_size(egui::vec2(420.0, 320.0))
        .resizable(true)
        .show(ctx, |ui| {
            let fit_available = ui.available_size() - egui::vec2(16.0, 48.0);
            ui.horizontal(|ui| {
                if ui.small_button("-").on_hover_text("Zoom out").clicked() {
                    zoom = (zoom / 1.25).max(MIN_VIEWER_ZOOM);
                }
                if ui.small_button("+").on_hover_text("Zoom in").clicked() {
                    zoom = (zoom * 1.25).min(MAX_VIEWER_ZOOM);
                }
                if ui.small_button("Fit").clicked() {
                    zoom = fit_zoom(source_size, fit_available);
                }
                if ui.small_button("100%").clicked() {
                    zoom = 1.0;
                }
                ui.add(
                    egui::Slider::new(&mut zoom, MIN_VIEWER_ZOOM..=MAX_VIEWER_ZOOM)
                        .logarithmic(true)
                        .suffix("x"),
                );
                ui.hyperlink_to("Open product", source_url);
            });
            ui.separator();
            egui::ScrollArea::both()
                .id_salt(("spc-md-image-scroll", event_id))
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    ui.add(egui::Image::new((texture.id(), source_size * zoom)));
                });
        });
    state.open = open;
    state.zoom = zoom;
}

fn fit_zoom(source_size: egui::Vec2, available_size: egui::Vec2) -> f32 {
    (available_size.x.max(1.0) / source_size.x.max(1.0))
        .min(available_size.y.max(1.0) / source_size.y.max(1.0))
        .clamp(MIN_VIEWER_ZOOM, MAX_VIEWER_ZOOM)
}

enum ImageSnapshot {
    Loading,
    Ready(egui::TextureHandle, egui::Vec2),
    Failed(String),
}

fn image_url_from_product_url(product_url: &str) -> Option<String> {
    let (base, filename) = product_url.rsplit_once('/')?;
    let number = filename.strip_prefix("md")?.strip_suffix(".html")?;
    (number.len() == 4 && number.bytes().all(|byte| byte.is_ascii_digit()))
        .then(|| format!("{base}/mcd{number}.png"))
}

fn fetch_color_image(url: &str) -> Result<egui::ColorImage, String> {
    let bytes = data_source::fetch_bytes(url).map_err(|error| error.to_string())?;
    let image =
        image::load_from_memory(&bytes).map_err(|error| format!("decode failed: {error}"))?;
    let width = image.width();
    let height = image.height();
    if width == 0
        || height == 0
        || width > MAX_IMAGE_EDGE
        || height > MAX_IMAGE_EDGE
        || u64::from(width) * u64::from(height) > MAX_IMAGE_PIXELS
    {
        return Err(format!("unsupported image dimensions {width}×{height}"));
    }
    let rgba = image.to_rgba8();
    Ok(egui::ColorImage::from_rgba_unmultiplied(
        [width as usize, height as usize],
        rgba.as_raw(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alert_qol_spc_product_url_maps_to_official_graphic() {
        assert_eq!(
            image_url_from_product_url("https://www.spc.noaa.gov/products/md/md1610.html"),
            Some("https://www.spc.noaa.gov/products/md/mcd1610.png".to_owned())
        );
    }

    #[test]
    fn alert_qol_noncanonical_product_urls_do_not_start_workers() {
        assert_eq!(
            image_url_from_product_url("https://example.com/md1610.html?download=1"),
            None
        );
        assert_eq!(image_url_from_product_url("md1610.html"), None);
        assert_eq!(
            image_url_from_product_url("https://example.com/md16.html"),
            None
        );
    }

    #[test]
    fn alert_qol_image_viewer_fit_zoom_preserves_aspect_and_clamps() {
        assert_eq!(
            fit_zoom(egui::vec2(1_200.0, 800.0), egui::vec2(600.0, 300.0)),
            0.375
        );
        assert_eq!(
            fit_zoom(egui::vec2(10_000.0, 10_000.0), egui::vec2(100.0, 100.0)),
            MIN_VIEWER_ZOOM
        );
        assert_eq!(
            fit_zoom(egui::vec2(100.0, 100.0), egui::vec2(1_000.0, 1_000.0)),
            MAX_VIEWER_ZOOM
        );
    }
}
