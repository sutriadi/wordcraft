//! WordCraft for Android, aimed at tablets and foldables.
//!
//! Runs the same [`wordcraft_ui_egui::WordApp`] as the desktop and web builds, through eframe on
//! a `NativeActivity`. Differences from the other shells:
//!
//! * No native file dialogs: Open, Save and Insert › Pictures go through the Storage Access
//!   Framework, which only Kotlin can reach, so the two sides talk through [`bridge`] files.
//! * No control channel, printing or clipboard hooks yet (egui's own text paste still works).
//!
//! `android_main` is the entry point `NativeActivity` looks for. Everything else compiles on the
//! host too, so `cargo test -p wordcraft-android` covers the bridge without a device.
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

pub mod bridge;
pub mod touch;

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

use bridge::{Bridge, SaveResult};
use wordcraft_engine::Session;
use wordcraft_ui_egui::async_saves::{AsyncSaves, SaveOutcome};
use wordcraft_ui_egui::{Inbox, Services, WordApp};

/// How often the bridge folder is checked while a picker or a save is open.
const POLL_EVERY: Duration = Duration::from_millis(200);

/// Wraps the app to move what the bridge delivers into the app's inbox and save tickets.
struct AndroidShell {
    app: WordApp,
    inbox: Inbox,
    bridge: Rc<Bridge>,
    saves: AsyncSaves,
    /// Bridge save id → ticket.
    tickets: Rc<RefCell<HashMap<u64, u64>>>,
    /// Open pickers shown and not yet answered.
    opens_open: Rc<RefCell<u32>>,
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
        let polled = self.bridge.poll();
        for file in polled.files {
            {
                let mut open = self.opens_open.borrow_mut();
                *open = open.saturating_sub(1);
            }
            self.inbox.lock().unwrap_or_else(|e| e.into_inner()).push(file);
        }
        for _ in polled.cancelled_opens {
            {
                let mut open = self.opens_open.borrow_mut();
                *open = open.saturating_sub(1);
            }
        }
        for (id, result) in polled.saves {
            if let Some(ticket) = self.tickets.borrow_mut().remove(&id) {
                let outcome = match result {
                    SaveResult::Saved => SaveOutcome::Saved,
                    SaveResult::Cancelled => SaveOutcome::Cancelled,
                    SaveResult::Failed(reason) => SaveOutcome::Failed(reason),
                };
                self.saves.finish(ticket, outcome);
            }
        }
        if *self.opens_open.borrow() > 0 || !self.tickets.borrow().is_empty() {
            ctx.request_repaint_after(POLL_EVERY);
        }
        self.app.logic(ctx);
    }

    fn raw_input_hook(&mut self, ctx: &egui::Context, raw: &mut egui::RawInput) {
        let now = raw.time.unwrap_or(0.0);
        self.apply_insets(raw, now);
        self.touch.filter(raw, now);
        if self.touch.is_pending() {
            ctx.request_repaint_after(Duration::from_millis(100)); // catches a long press
        }
        self.app.raw_input_hook(ctx, raw);
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.app.ui(ui);
    }
}

fn services(inbox: Inbox, bridge: Rc<Bridge>, saves: AsyncSaves, tickets: Rc<RefCell<HashMap<u64, u64>>>, opens_open: Rc<RefCell<u32>>) -> Services {
    let open_bridge = bridge.clone();
    let open_count = opens_open.clone();
    Services {
        open_async: Some(Box::new(move |purpose: &str| match open_bridge.request_open(purpose) {
            Ok(_) => *open_count.borrow_mut() += 1,
            Err(e) => log::error!("couldn't ask for the open picker: {e}"),
        })),
        // The suggested name goes to the system picker; the user chooses where it lands.
        pick_save: Some(Box::new(|name: &str| Some(name.to_string()))),
        async_saves: Some(saves.clone()),
        download: Some(Box::new(move |path: &str, bytes: &[u8]| {
            let ticket = saves.start();
            match bridge.request_save(path, bytes) {
                Ok(id) => {
                    tickets.borrow_mut().insert(id, ticket);
                    Ok(())
                }
                Err(e) => {
                    saves.finish(ticket, SaveOutcome::Failed(e.to_string()));
                    Err(format!("couldn't hand the file to Android: {e}"))
                }
            }
        })),
        inbox: Some(inbox),
        touch_ui: true,
        ..Default::default()
    }
}

/// Starts the app. `bridge_root` is a folder in the app's private storage shared with the Kotlin side.
pub fn run(options: eframe::NativeOptions, bridge_root: PathBuf) -> eframe::Result {
    eframe::run_native(
        "WordCraft",
        options,
        Box::new(move |cc| {
            let bridge = Rc::new(Bridge::new(&bridge_root).map_err(|e| format!("bridge folder {}: {e}", bridge_root.display()))?);
            let inbox: Inbox = Inbox::default();
            let saves = AsyncSaves::default();
            let tickets = Rc::new(RefCell::new(HashMap::new()));
            let opens_open = Rc::new(RefCell::new(0u32));
            let region = wordcraft_ui_egui::i18n::system_region();
            let mut session = Session::new(wordcraft_engine::blank_document(&region));
            session.system_region = region;
            let mut app = WordApp::new(session, services(inbox.clone(), bridge.clone(), saves.clone(), tickets.clone(), opens_open.clone()));
            // No folder the app may write to behind the user's back: saving always asks (SAF).
            app.autosave = false;
            app.start_screen();
            let _ = &cc.egui_ctx;
            Ok(Box::new(AndroidShell {
                app,
                inbox,
                bridge,
                saves,
                tickets,
                opens_open,
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
