use std::sync::mpsc::{self, Receiver, Sender};

use eframe::egui;
use global_hotkey::hotkey::{Code, HotKey, Modifiers};
use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState};
use tray_icon::menu::{Menu, MenuEvent, MenuItem};
use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

use crate::audio::AppResult;

pub const HOTKEY_LABEL: &str = "Ctrl+Alt+M";
/// Matches the icon id in assets/hum-anc.rc.
const APP_ICON_RESOURCE: u16 = 1;

/// What the tray icon, its menu and the hotkey ask the app to do.
pub enum Command {
    ToggleAudio,
    ToggleWindow,
    ShowWindow,
    Quit,
}

/// Tray icon and global hotkey; dropping this removes both.
pub struct Controls {
    _tray: TrayIcon,
    _hotkeys: GlobalHotKeyManager,
    toggle_item: MenuItem,
    pub commands: Receiver<Command>,
    pub hotkey_problem: Option<String>,
}

impl Controls {
    /// Each command wakes `ctx`, so a hidden window does no work until the tray or hotkey is used.
    pub fn new(ctx: &egui::Context) -> AppResult<Self> {
        let (sender, commands) = mpsc::channel();
        let toggle_item = MenuItem::new("Start", true, None);
        let show_item = MenuItem::new("Show window", true, None);
        let quit_item = MenuItem::new("Quit", true, None);
        let menu = Menu::with_items(&[&toggle_item, &show_item, &quit_item])?;
        let tray = TrayIconBuilder::new()
            .with_tooltip("hum-anc")
            .with_icon(tray_icon_image()?)
            .with_menu(Box::new(menu))
            .with_menu_on_left_click(false)
            .build()?;

        let hotkeys = GlobalHotKeyManager::new()?;
        let hotkey = HotKey::new(Some(Modifiers::CONTROL | Modifiers::ALT), Code::KeyM);
        let hotkey_problem = hotkeys
            .register(hotkey)
            .err()
            .map(|err| format!("{HOTKEY_LABEL} is unavailable ({err}); use the tray icon instead"));

        let hotkey_id = hotkey.id();
        let forward = forwarder(sender.clone(), ctx.clone());
        GlobalHotKeyEvent::set_event_handler(Some(move |event: GlobalHotKeyEvent| {
            if event.id == hotkey_id && matches!(event.state, HotKeyState::Pressed) {
                forward(Command::ToggleAudio);
            }
        }));

        let forward = forwarder(sender.clone(), ctx.clone());
        TrayIconEvent::set_event_handler(Some(move |event: TrayIconEvent| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                forward(Command::ToggleWindow);
            }
        }));

        let ids = (
            toggle_item.id().clone(),
            show_item.id().clone(),
            quit_item.id().clone(),
        );
        let forward = forwarder(sender, ctx.clone());
        MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
            let (toggle_id, show_id, quit_id) = &ids;
            let command = if event.id == *toggle_id {
                Command::ToggleAudio
            } else if event.id == *show_id {
                Command::ShowWindow
            } else if event.id == *quit_id {
                Command::Quit
            } else {
                return;
            };
            forward(command);
        }));

        Ok(Self {
            _tray: tray,
            _hotkeys: hotkeys,
            toggle_item,
            commands,
            hotkey_problem,
        })
    }

    pub fn show_running(&self, running: bool) {
        self.toggle_item
            .set_text(if running { "Stop" } else { "Start" });
    }
}

fn forwarder(
    sender: Sender<Command>,
    ctx: egui::Context,
) -> impl Fn(Command) + Send + Sync + 'static {
    move |command| {
        let _ = sender.send(command);
        ctx.request_repaint();
    }
}

/// The app icon that build.rs embeds in the exe.
fn tray_icon_image() -> AppResult<Icon> {
    Ok(Icon::from_resource(APP_ICON_RESOURCE, None)?)
}
