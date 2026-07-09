//! One debounced persistence lane for interactive settings/style edits.
//!
//! Widgets only mark a document dirty. The app loop coalesces drag/change
//! bursts, reports save failures visibly, and `on_exit` performs an explicit
//! final flush.

use std::time::{Duration, Instant};

use crate::ViewerApp;

const SAVE_DEBOUNCE: Duration = Duration::from_millis(800);
const SUCCESS_NOTICE_LIFETIME: Duration = Duration::from_secs(4);
const MAX_RETRY_DELAY: Duration = Duration::from_secs(30);

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) enum PersistenceNoticeLevel {
    Success,
    Warning,
    Error,
}

pub(crate) struct PersistenceStatusView<'a> {
    pub short: &'static str,
    pub detail: &'a str,
    pub level: PersistenceNoticeLevel,
}

#[derive(Debug)]
struct PersistenceNotice {
    detail: String,
    level: PersistenceNoticeLevel,
    recorded_at: Instant,
}

#[derive(Debug, Default)]
struct DebouncedDocument {
    due_at: Option<Instant>,
    retry_count: u8,
}

impl DebouncedDocument {
    fn mark(&mut self, now: Instant) {
        self.due_at = Some(now + SAVE_DEBOUNCE);
        self.retry_count = 0;
    }

    fn take_due(&mut self, now: Instant) -> bool {
        if self.due_at.is_some_and(|due_at| now >= due_at) {
            self.due_at = None;
            return true;
        }
        false
    }

    fn take_dirty(&mut self) -> bool {
        self.due_at.take().is_some()
    }

    fn is_dirty(&self) -> bool {
        self.due_at.is_some()
    }

    fn due_at(&self) -> Option<Instant> {
        self.due_at
    }

    fn finish_attempt(&mut self, succeeded: bool, now: Instant) {
        if succeeded {
            self.retry_count = 0;
            return;
        }
        let delay_seconds = 1_u64 << self.retry_count.min(5);
        let delay = Duration::from_secs(delay_seconds).min(MAX_RETRY_DELAY);
        self.retry_count = self.retry_count.saturating_add(1);
        self.due_at = Some(now + delay);
    }
}

#[derive(Debug, Default)]
pub(crate) struct SettingsPersistence {
    app: DebouncedDocument,
    styles: DebouncedDocument,
    load_notice: Option<PersistenceNotice>,
    save_notice: Option<PersistenceNotice>,
}

impl SettingsPersistence {
    pub(crate) fn from_load_statuses(
        app: settings::DocumentLoadStatus,
        styles: settings::DocumentLoadStatus,
        now: Instant,
    ) -> Self {
        let mut messages = Vec::new();
        let mut level = PersistenceNoticeLevel::Success;
        for (label, status) in [("settings", app), ("appearance settings", styles)] {
            if let Some(message) = status.user_message(label) {
                level = level.max(load_notice_level(&status));
                messages.push(message);
            }
        }
        Self {
            app: DebouncedDocument::default(),
            styles: DebouncedDocument::default(),
            load_notice: (!messages.is_empty()).then(|| PersistenceNotice {
                detail: messages.join(" "),
                level,
                recorded_at: now,
            }),
            save_notice: None,
        }
    }

    fn mark_app(&mut self, now: Instant) {
        self.app.mark(now);
    }

    fn mark_styles(&mut self, now: Instant) {
        self.styles.mark(now);
    }

    fn take_due(&mut self, now: Instant) -> (bool, bool) {
        (self.app.take_due(now), self.styles.take_due(now))
    }

    fn take_dirty(&mut self) -> (bool, bool) {
        (self.app.take_dirty(), self.styles.take_dirty())
    }

    fn next_due_in(&self, now: Instant) -> Option<Duration> {
        [self.app.due_at(), self.styles.due_at()]
            .into_iter()
            .flatten()
            .min()
            .map(|due_at| due_at.saturating_duration_since(now))
    }

    fn record_save_result(
        &mut self,
        attempted_app: bool,
        attempted_styles: bool,
        app_error: Option<String>,
        styles_error: Option<String>,
        now: Instant,
    ) {
        if attempted_app {
            self.app.finish_attempt(app_error.is_none(), now);
        }
        if attempted_styles {
            self.styles.finish_attempt(styles_error.is_none(), now);
        }
        let errors: Vec<_> = app_error.into_iter().chain(styles_error).collect();
        if !errors.is_empty() {
            self.save_notice = Some(PersistenceNotice {
                detail: format!("{} BowEcho will retry automatically.", errors.join(" ")),
                level: PersistenceNoticeLevel::Error,
                recorded_at: now,
            });
            return;
        }
        let detail = match (attempted_app, attempted_styles) {
            (true, true) => "Settings and appearance settings saved",
            (true, false) => "Settings saved",
            (false, true) => "Appearance settings saved",
            (false, false) => return,
        };
        self.save_notice = Some(PersistenceNotice {
            detail: detail.to_owned(),
            level: PersistenceNoticeLevel::Success,
            recorded_at: now,
        });
    }

    pub(crate) fn status_view(&self, now: Instant) -> Option<PersistenceStatusView<'_>> {
        if let Some(notice) = self.save_notice.as_ref()
            && notice.level == PersistenceNoticeLevel::Error
        {
            return Some(PersistenceStatusView {
                short: "SETTINGS ERROR",
                detail: &notice.detail,
                level: notice.level,
            });
        }
        if let Some(notice) = self.load_notice.as_ref() {
            return Some(PersistenceStatusView {
                short: if notice.level == PersistenceNoticeLevel::Error {
                    "SETTINGS DEFAULTS"
                } else {
                    "SETTINGS RECOVERED"
                },
                detail: &notice.detail,
                level: notice.level,
            });
        }
        if self.app.is_dirty() || self.styles.is_dirty() {
            return Some(PersistenceStatusView {
                short: "SETTINGS PENDING",
                detail: "Interactive changes are queued for a coalesced save",
                level: PersistenceNoticeLevel::Warning,
            });
        }
        self.save_notice.as_ref().and_then(|notice| {
            (notice.level != PersistenceNoticeLevel::Success
                || now.saturating_duration_since(notice.recorded_at) <= SUCCESS_NOTICE_LIFETIME)
                .then_some(PersistenceStatusView {
                    short: "SETTINGS SAVED",
                    detail: &notice.detail,
                    level: notice.level,
                })
        })
    }
}

fn load_notice_level(status: &settings::DocumentLoadStatus) -> PersistenceNoticeLevel {
    match status {
        settings::DocumentLoadStatus::DefaultsAfterError { .. } => PersistenceNoticeLevel::Error,
        settings::DocumentLoadStatus::RecoveredFromBackup { .. }
        | settings::DocumentLoadStatus::LoadedWithWarning { .. } => PersistenceNoticeLevel::Warning,
        settings::DocumentLoadStatus::Loaded | settings::DocumentLoadStatus::Missing => {
            PersistenceNoticeLevel::Success
        }
    }
}

impl ViewerApp {
    pub(crate) fn mark_app_settings_dirty(&mut self) {
        self.settings_persistence.mark_app(Instant::now());
    }

    pub(crate) fn mark_style_settings_dirty(&mut self) {
        if self.styles_newer_schema {
            return;
        }
        self.settings_persistence.mark_styles(Instant::now());
    }

    pub(crate) fn maybe_flush_settings_persistence(&mut self, ctx: &eframe::egui::Context) {
        let now = Instant::now();
        let (app, styles) = self.settings_persistence.take_due(now);
        self.persist_settings_documents(app, styles, now);
        if let Some(delay) = self.settings_persistence.next_due_in(Instant::now()) {
            ctx.request_repaint_after(delay);
        }
    }

    pub(crate) fn flush_settings_persistence(&mut self) {
        let now = Instant::now();
        let (app, styles) = self.settings_persistence.take_dirty();
        self.persist_settings_documents(app, styles, now);
    }

    fn persist_settings_documents(&mut self, app: bool, style_document: bool, now: Instant) {
        if !app && !style_document {
            return;
        }
        let app_error = if app {
            self.app_settings
                .save()
                .err()
                .map(|error| format!("Settings save failed: {error}"))
        } else {
            None
        };
        let attempted_styles = style_document && !self.styles_newer_schema;
        let styles_error = if attempted_styles {
            styles::save(&self.style_settings)
                .err()
                .map(|error| format!("Appearance settings save failed: {error}"))
        } else {
            None
        };
        self.settings_persistence.record_save_result(
            app,
            attempted_styles,
            app_error,
            styles_error,
            now,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debounce_coalesces_a_change_burst_into_one_due_save() {
        let start = Instant::now();
        let mut document = DebouncedDocument::default();
        document.mark(start);
        document.mark(start + Duration::from_millis(600));

        assert!(!document.take_due(start + Duration::from_millis(800)));
        assert!(document.take_due(start + Duration::from_millis(1_400)));
        assert!(!document.take_due(start + Duration::from_secs(2)));
    }

    #[test]
    fn explicit_flush_takes_pending_document_before_deadline() {
        let start = Instant::now();
        let mut document = DebouncedDocument::default();
        document.mark(start);
        assert!(document.take_dirty());
        assert!(!document.take_dirty());
    }

    #[test]
    fn failed_attempt_stays_dirty_and_retries_with_bounded_backoff() {
        let start = Instant::now();
        let mut document = DebouncedDocument::default();
        document.mark(start);
        assert!(document.take_due(start + SAVE_DEBOUNCE));

        document.finish_attempt(false, start + SAVE_DEBOUNCE);
        assert!(document.is_dirty());
        assert!(!document.take_due(start + SAVE_DEBOUNCE + Duration::from_millis(999)));
        assert!(document.take_due(start + SAVE_DEBOUNCE + Duration::from_secs(1)));

        document.finish_attempt(false, start + Duration::from_secs(2));
        assert!(!document.take_due(start + Duration::from_secs(3)));
        assert!(document.take_due(start + Duration::from_secs(4)));
        document.finish_attempt(true, start + Duration::from_secs(4));
        assert!(!document.is_dirty());
        assert_eq!(document.retry_count, 0);
    }

    #[test]
    fn next_due_delay_selects_the_earliest_dirty_document() {
        let start = Instant::now();
        let mut persistence = SettingsPersistence::default();
        persistence.styles.mark(start + Duration::from_millis(200));
        persistence.app.mark(start);

        assert_eq!(persistence.next_due_in(start), Some(SAVE_DEBOUNCE));
    }

    #[test]
    fn failed_app_result_is_requeued_without_requeueing_successful_styles() {
        let start = Instant::now();
        let mut persistence = SettingsPersistence::default();
        persistence.app.mark(start);
        persistence.styles.mark(start);
        assert_eq!(persistence.take_due(start + SAVE_DEBOUNCE), (true, true));

        persistence.record_save_result(
            true,
            true,
            Some("disk full".to_owned()),
            None,
            start + SAVE_DEBOUNCE,
        );
        assert!(persistence.app.is_dirty());
        assert!(!persistence.styles.is_dirty());
        assert_eq!(
            persistence.next_due_in(start + SAVE_DEBOUNCE),
            Some(Duration::from_secs(1))
        );
    }
}
