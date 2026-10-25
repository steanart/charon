//! The menu-bar icon and its menu: the overall status at a glance, and the main actions.

use crate::desktop::Desktop;
use crate::quit::Quit;
use crate::state::AppState;
use crate::updates::{UpdateInfo, Updates};
use charon_core::Overview;
use tauri::image::Image;
use tauri::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::{AppHandle, Emitter, Manager, Wry};

/// macOS: black-on-transparent template, tinted by the system for light and dark menu bars.
#[cfg(target_os = "macos")]
const TRAY_ICON: &[u8] = include_bytes!("../icons/tray.png");
/// Windows and Linux: white boat, made for the usual dark taskbars and panels.
#[cfg(not(target_os = "macos"))]
const TRAY_ICON: &[u8] = include_bytes!("../icons/tray-white.png");

const TRAY_ID: &str = "charon";

const CHECK_FOR_UPDATES: &str = "Check for Updates…";

/// The menu items whose text changes with the status.
pub struct Tray {
    status: MenuItem<Wry>,
    waiting: MenuItem<Wry>,
    pause: MenuItem<Wry>,
    update: MenuItem<Wry>,
}

impl Tray {
    /// Puts the icon in the menu bar (macOS) or system tray (Windows, Linux).
    pub fn install(app: &AppHandle, overview: &Overview) -> tauri::Result<()> {
        let status = MenuItem::with_id(app, "status", overview.headline(), false, None::<&str>)?;
        let waiting = MenuItem::with_id(app, "waiting", "Review Waiting Jobs…", overview.lists_waiting() > 0, None::<&str>)?;
        let pause = MenuItem::with_id(app, "pause", Self::pause_label(overview), true, None::<&str>)?;
        let update = MenuItem::with_id(app, "update", CHECK_FOR_UPDATES, true, None::<&str>)?;
        let menu = Menu::with_items(
            app,
            &[
                &status,
                &PredefinedMenuItem::separator(app)?,
                &MenuItem::with_id(app, "open", "Open Charon", true, None::<&str>)?,
                &waiting,
                &MenuItem::with_id(app, "sync", "Sync All…", true, None::<&str>)?,
                &pause,
                &PredefinedMenuItem::separator(app)?,
                &update,
                &MenuItem::with_id(app, "quit", "Quit Charon", true, None::<&str>)?,
            ],
        )?;
        TrayIconBuilder::with_id(TRAY_ID)
            .icon(Image::from_bytes(TRAY_ICON)?)
            .icon_as_template(true)
            .tooltip(format!("Charon · {}", overview.headline()))
            .menu(&menu)
            .show_menu_on_left_click(true)
            .on_menu_event(Self::on_menu)
            .build(app)?;
        app.manage(Tray { status, waiting, pause, update });
        Self::badge(app, overview);
        Ok(())
    }

    /// Whether the icon is in the menu bar or system tray (it may not be, on a Linux desktop
    /// without a tray).
    pub fn is_installed(app: &AppHandle) -> bool {
        app.try_state::<Tray>().is_some()
    }

    /// Updates the status line, the waiting and pause items, the tooltip and the badge.
    pub fn refresh(app: &AppHandle, overview: &Overview) {
        let Some(tray) = app.try_state::<Tray>() else { return };
        let _ = tray.status.set_text(overview.headline());
        let _ = tray.waiting.set_enabled(overview.lists_waiting() > 0);
        let _ = tray.pause.set_text(Self::pause_label(overview));
        if let Some(icon) = app.tray_by_id(TRAY_ID) {
            let _ = icon.set_tooltip(Some(format!("Charon · {}", overview.headline())));
        }
        Self::badge(app, overview);
    }

    /// macOS: the Dock icon counts the jobs whose lists wait for the person (changes to
    /// confirm, or files to compare).
    fn badge(app: &AppHandle, overview: &Overview) {
        #[cfg(target_os = "macos")]
        if let Some(window) = app.get_webview_window("main") {
            let waiting = overview.lists_waiting();
            let count = (waiting > 0).then_some(waiting as i64);
            let _ = window.set_badge_count(count);
        }
        let _ = (app, overview);
    }

    /// With a new version out, the update item becomes its download link.
    pub fn show_update(app: &AppHandle, info: &UpdateInfo) {
        let Some(tray) = app.try_state::<Tray>() else { return };
        let text = if info.available {
            format!("Download Charon {}…", info.latest)
        } else {
            CHECK_FOR_UPDATES.to_string()
        };
        let _ = tray.update.set_text(text);
    }

    /// Opens the download of a version found earlier, or asks GitHub and says what it found.
    fn update_clicked(app: &AppHandle) {
        if let Some(info) = Updates::last(app).filter(|info| info.available) {
            if let Err(e) = Desktop::open_url(info.link()) {
                log::warn!("{e}");
            }
            return;
        }
        let app = app.clone();
        std::thread::spawn(move || match Updates::check(&app) {
            Ok(info) if info.available => Updates::notify(&app, &info, true),
            Ok(info) => Updates::show(&app, "Charon is up to date", &format!("You have the newest version, {}.", info.current)),
            Err(e) => Updates::show(&app, "Couldn't check for updates", &e),
        });
    }

    /// Brings the window up and asks it to do something.
    fn tell_window(app: &AppHandle, event: &str) {
        crate::show_main_window(app);
        let _ = app.emit(event, ());
    }

    fn pause_label(overview: &Overview) -> &'static str {
        if overview.paused_all {
            "Resume Everything"
        } else {
            "Pause Everything"
        }
    }

    /// Handles a click on one of the menu items.
    fn on_menu(app: &AppHandle, event: MenuEvent) {
        let engine = app.state::<AppState>().engine.clone();
        match event.id.as_ref() {
            "open" => crate::show_main_window(app),
            // The window runs these, so jobs that wait for an OK get their lists shown.
            "sync" => Self::tell_window(app, "sync-all"),
            "waiting" => Self::tell_window(app, "show-waiting"),
            // Saving the settings and redrawing every row asks the disk: not on the thread
            // that draws the menu bar.
            "pause" => {
                std::thread::spawn(move || {
                    let paused = engine.settings().paused_all;
                    if let Err(e) = engine.set_all_paused(!paused) {
                        log::error!("couldn't pause or resume everything: {e}");
                    }
                });
            }
            "update" => Self::update_clicked(app),
            "quit" => Quit::ask(app),
            _ => {}
        }
    }
}
