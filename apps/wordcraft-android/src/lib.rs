//! WordCraft for Android, aimed at tablets and foldables.
//!
//! Runs the same [`wordcraft_ui_egui::WordApp`] as the desktop and web builds, through eframe on
//! a `NativeActivity`. Differences from the other shells:
//!
//! * No native file dialogs: Open, Save and Insert › Pictures go through the Storage Access
//!   Framework, which only Kotlin can reach, so the two sides talk through [`bridge`] files, which
//!   [`files::AndroidFiles`] presents to the framework host as its file service.
//! * No control channel, printing or clipboard hooks yet (egui's own text paste still works).
//!
//! `android_main` is the entry point `NativeActivity` looks for. Everything else compiles on the
//! host too, so `cargo test -p wordcraft-android` covers the bridge without a device.
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

pub mod bridge;
pub mod files;
pub mod touch;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use craft::host::{Host, Wake};
use craft::platform::open::{FileIntake, IntakeSender};
use craft::settings::{Location, OpenOptions, SettingsStore, SharedSettings};
use craft::ui::InstallSpec;
use craft::ui::shell::embed;
use craft::window::system::NoWindows;
use wordcraft_engine::Session;
use wordcraft_ui_egui::prefs::Prefs;
use wordcraft_ui_egui::{Services, WordApp};

use bridge::Bridge;
use files::AndroidFiles;

/// How often the bridge folder is checked while a picker or a save is open.
const POLL_EVERY: Duration = Duration::from_millis(200);

/// Wraps the app to feed what the bridge delivers into the file service and the intake.
struct AndroidShell {
    app: WordApp,
    bridge: Arc<Bridge>,
    files: Arc<AndroidFiles>,
    intake: IntakeSender,
    settings: SharedSettings<Prefs>,
    touch: touch::TouchScroll,
    insets: [f32; 4],
    insets_read: f64,
}

impl AndroidShell {
    /// Keeps the UI clear of the status bar, navigation bar, cutout and keyboard. Targeting API 35+
    /// draws apps edge to edge, so the system bars would otherwise cover the ribbon and status line.
    fn apply_insets(&mut self, raw: &mut egui::RawInput, now: f64) {
        if now - self.insets_read >= 0.25 || self.insets_read > now {
            self.insets_read = now;
            self.insets = self.bridge.insets();
        }
        let ppp = raw.viewports.get(&raw.viewport_id).and_then(|v| v.native_pixels_per_point).unwrap_or(1.0).max(0.1);
        if let Some(full) = raw.screen_rect {
            let [l, t, r, b] = self.insets.map(|px| px / ppp);
            let min = full.min + egui::vec2(l, t);
            let max = full.max - egui::vec2(r, b);
            if max.x - min.x > 100.0 && max.y - min.y > 100.0 {
                raw.screen_rect = Some(egui::Rect::from_min_max(min, max));
            }
        }
    }
}

impl eframe::App for AndroidShell {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if self.files.pump(&self.intake) {
            ctx.request_repaint_after(POLL_EVERY);
        }
        self.app.logic(ctx);
        self.settings.lock().tick(std::time::Instant::now());
    }

    fn raw_input_hook(&mut self, ctx: &egui::Context, raw: &mut egui::RawInput) {
        let now = raw.time.unwrap_or(0.0);
        self.apply_insets(raw, now);
        self.touch.filter(raw, now);
        if self.touch.is_pending() {
            ctx.request_repaint_after(Duration::from_millis(100)); // catches a long press
        }
        embed::before_input(ctx, |out| self.app.frame_input(out));
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.app.ui(ui);
    }
}

/// Opens the preferences store in the app's private storage, or in memory when that fails.
fn open_settings(dir: PathBuf) -> SharedSettings<Prefs> {
    let location = match std::fs::create_dir_all(&dir) {
        Ok(()) => Location::Dir(dir),
        Err(e) => {
            log::warn!("preferences stay in memory: {e}");
            Location::Memory
        }
    };
    let (store, report) = SettingsStore::<Prefs>::open_with(OpenOptions::new(location));
    if report.has_problems() {
        log::warn!("preferences: {report:?}");
    }
    SharedSettings::new(store)
}

/// Starts the app. `bridge_root` is a folder in the app's private storage shared with the Kotlin side.
pub fn run(options: eframe::NativeOptions, bridge_root: PathBuf) -> eframe::Result {
    eframe::run_native(
        "WordCraft",
        options,
        Box::new(move |cc| {
            let ctx = cc.egui_ctx.clone();
            let bridge = Arc::new(Bridge::new(&bridge_root).map_err(|e| format!("bridge folder {}: {e}", bridge_root.display()))?);
            let settings = open_settings(bridge_root.parent().map(|p| p.join("settings")).unwrap_or_else(|| bridge_root.join("settings")));
            let lang = wordcraft_ui_egui::i18n::Lang::from_pref(&settings.lock().get().language);
            let files = Arc::new(AndroidFiles::new(bridge.clone()));
            let host = Arc::new(
                Host::builder(wordcraft_ui_egui::APP)
                    .i18n(wordcraft_ui_egui::i18n::catalogs())
                    .files(files.clone())
                    .settings(Arc::new(settings.clone()))
                    .lang(lang.craft_lang())
                    .build(),
            );
            let spec = InstallSpec::new(craft::host::target::ShellInfo::current(), Arc::new(NoWindows)).app_id("wordcraft");
            embed::install(&ctx, spec, &wordcraft_ui_egui::ui_fonts(lang)).map_err(|e| format!("the runtime could not be installed: {e:?}"))?;
            let wake_ctx = ctx.clone();
            let (intake, sender) = FileIntake::new(Wake::new(move || wake_ctx.request_repaint()));
            let region = wordcraft_ui_egui::i18n::system_region();
            let mut session = Session::new(wordcraft_engine::blank_document(&region));
            session.system_region = region;
            let services = Services { intake: Some(intake), ..Default::default() };
            let mut app = WordApp::new(session, services).with_host(host).with_settings(settings.clone());
            // No folder the app may write to behind the user's back: saving always asks (SAF).
            app.autosave = false;
            app.start_screen();
            Ok(Box::new(AndroidShell {
                app,
                bridge,
                files,
                intake: sender,
                settings,
                touch: touch::TouchScroll::default(),
                insets: [0.0; 4],
                insets_read: -1.0,
            }))
        }),
    )
}

/// The entry point `NativeActivity` calls (named in `AndroidManifest.xml` as `libwordcraft_android.so`).
#[cfg(target_os = "android")]
#[allow(unsafe_code)]
#[unsafe(no_mangle)]
pub fn android_main(app: android_activity::AndroidApp) {
    android_logger::init_once(android_logger::Config::default().with_max_level(log::LevelFilter::Info).with_tag("WordCraft"));
    let data = app.internal_data_path().unwrap_or_else(|| PathBuf::from("/data/local/tmp"));
    install_panic_report(data.join("crash.txt"));
    let root = data.join("bridge");
    let options = eframe::NativeOptions { android_app: Some(app), ..Default::default() };
    if let Err(e) = run(options, root) {
        log::error!("WordCraft stopped: {e}");
    }
}

/// Records a panic where the Kotlin side can show it on the next launch (there is no console on a tablet).
#[cfg(target_os = "android")]
fn install_panic_report(path: PathBuf) {
    std::panic::set_hook(Box::new(move |info| {
        let report = format!("{info}\n\n{}", std::backtrace::Backtrace::force_capture());
        log::error!("panic: {report}");
        let _ = std::fs::write(&path, report);
    }));
}
