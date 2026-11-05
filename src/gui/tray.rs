//! System tray icon (Windows).
//!
//! The tray keeps Topebag reachable while its window is closed: click the
//! icon to open the window, or use the menu to print the link on your
//! clipboard, check Instapaper, or quit. Menu and click events arrive on the
//! UI thread's message loop; they're forwarded over a channel and the UI is
//! woken with `request_repaint()`.

use std::sync::mpsc::Sender;

use eframe::egui;

/// Something the user asked for from the tray.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayAction {
    Open,
    PrintClipboard,
    CheckInstapaper,
    Quit,
}

#[cfg(windows)]
mod imp {
    use super::*;
    use anyhow::Result;
    use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
    use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

    /// Keeps the tray icon alive; dropping it removes the icon.
    pub struct Tray {
        _icon: TrayIcon,
    }

    pub fn create(ctx: egui::Context, tx: Sender<TrayAction>) -> Result<Tray> {
        let open = MenuItem::new("Open Topebag", true, None);
        let clipboard = MenuItem::new("Print link from clipboard", true, None);
        let instapaper = MenuItem::new("Check Instapaper now", true, None);
        let quit = MenuItem::new("Quit Topebag", true, None);
        let menu = Menu::new();
        menu.append_items(&[
            &open,
            &PredefinedMenuItem::separator(),
            &clipboard,
            &instapaper,
            &PredefinedMenuItem::separator(),
            &quit,
        ])?;

        let ids = [
            (open.id().clone(), TrayAction::Open),
            (clipboard.id().clone(), TrayAction::PrintClipboard),
            (instapaper.id().clone(), TrayAction::CheckInstapaper),
            (quit.id().clone(), TrayAction::Quit),
        ];
        let menu_tx = tx.clone();
        let menu_ctx = ctx.clone();
        MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
            if let Some((_, action)) = ids.iter().find(|(id, _)| *id == event.id) {
                let _ = menu_tx.send(*action);
                menu_ctx.request_repaint();
            }
        }));
        TrayIconEvent::set_event_handler(Some(move |event: TrayIconEvent| {
            let open = matches!(
                event,
                TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. }
                    | TrayIconEvent::DoubleClick { .. }
            );
            if open {
                let _ = tx.send(TrayAction::Open);
                ctx.request_repaint();
            }
        }));

        let art = crate::gui::theme::icon();
        let icon = Icon::from_rgba(art.rgba, art.width, art.height)?;
        let tray = TrayIconBuilder::new()
            .with_icon(icon)
            .with_tooltip("Topebag — links in, paper out")
            .with_menu(Box::new(menu))
            .with_menu_on_left_click(false)
            .build()?;
        Ok(Tray { _icon: tray })
    }
}

#[cfg(not(windows))]
mod imp {
    use super::*;

    pub struct Tray;

    pub fn create(_ctx: egui::Context, _tx: Sender<TrayAction>) -> anyhow::Result<Tray> {
        anyhow::bail!("the tray icon is only implemented on Windows for now")
    }
}

pub use imp::{Tray, create};
