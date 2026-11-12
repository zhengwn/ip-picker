use tauri::{
    menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem},
    AppHandle, Manager,
};

const QUIT_ID: &str = "ip-picker-quit";

pub fn menu(app: &AppHandle) -> tauri::Result<Menu<tauri::Wry>> {
    let menu = Menu::default(app)?;
    let menus = menu.items()?;
    // Tauri puts the application submenu first. Keep its localized Quit label
    // and other default items, but route the action through CloseRequested.
    let quit_label = PredefinedMenuItem::quit(app, None)?.text()?;
    if let Some(application_menu) = menus.first().and_then(|item| item.as_submenu()) {
        for (index, item) in application_menu.items()?.iter().enumerate() {
            if let Some(predefined) = item.as_predefined_menuitem() {
                if predefined.text()? == quit_label {
                    let quit = MenuItem::with_id(app, QUIT_ID, &quit_label, true, Some("Cmd+Q"))?;
                    application_menu.remove(predefined)?;
                    application_menu.insert(&quit, index)?;
                    return Ok(menu);
                }
            }
        }
    }
    Err(std::io::Error::other("无法找到 macOS 退出菜单项").into())
}

pub fn on_menu_event(app: &AppHandle, event: MenuEvent) {
    if event.id() == QUIT_ID {
        if let Err(error) = request_close(app) {
            eprintln!("无法请求退出 IP Picker：{error}");
        }
    }
}

fn request_close(app: &AppHandle) -> tauri::Result<()> {
    if let Some(window) = app.get_webview_window("main") {
        window.close()?;
    }
    Ok(())
}

#[cfg(feature = "desktop-smoke")]
pub fn exercise_quit_menu(app: &AppHandle) -> tauri::Result<()> {
    let quit = app
        .menu()
        .and_then(|menu| menu.items().ok())
        .and_then(|menus| menus.first()?.as_submenu()?.get(QUIT_ID))
        .filter(|item| item.as_menuitem().is_some())
        .ok_or_else(|| std::io::Error::other("missing custom macOS Quit item"))?;
    on_menu_event(
        app,
        MenuEvent {
            id: quit.id().clone(),
        },
    );
    Ok(())
}
