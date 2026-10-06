#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use tauri::{
    menu::{Menu, MenuItem},
    tray::TrayIconBuilder,
    Manager,
};

/// Делаем окно tool window (WS_EX_TOOLWINDOW): такие окна Windows НЕ сворачивает
/// по «Показать рабочий стол» (Win+D / клик по углу таскбара).
#[cfg(windows)]
fn make_tool_window(win: &tauri::WebviewWindow) {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::*;

    if let Ok(raw) = win.hwnd() {
        let hwnd = HWND(raw.0 as _);
        unsafe {
            let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
            let new_ex = (ex | WS_EX_TOOLWINDOW.0 as isize) & !(WS_EX_APPWINDOW.0 as isize);
            SetWindowLongPtrW(hwnd, GWL_EXSTYLE, new_ex);
            let _ = SetWindowPos(
                hwnd,
                None,
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED,
            );
        }
    }
}

/// Сторожок Z-порядка и видимости: клик по пустому месту таскбара поднимает панель поверх
/// виджета, поэтому каждые 300 мс возвращаем окно наверх (HWND_TOPMOST, без активации).
/// При активном полноэкранном приложении виджет скрываем, при выходе из него — возвращаем.
/// HWND не Send — передаём сырой указатель как isize.
#[cfg(windows)]
fn spawn_zorder_keeper(win: &tauri::WebviewWindow) {
    use windows::Win32::Foundation::{HWND, POINT, RECT};
    use windows::Win32::UI::WindowsAndMessaging::*;

    if let Ok(raw) = win.hwnd() {
        let hwnd_raw = raw.0 as isize;
        std::thread::spawn(move || {
            let mut hidden = false;
            loop {
                std::thread::sleep(std::time::Duration::from_millis(300));
                let hwnd = HWND(hwnd_raw as _);
                unsafe {
                    let fs = fullscreen_app_running();
                    if fs && !hidden {
                        let _ = ShowWindow(hwnd, SW_HIDE);
                        hidden = true;
                    } else if !fs && hidden {
                        let _ = ShowWindow(hwnd, SW_SHOWNA);
                        hidden = false;
                    }
                    if !hidden {
                        // Слепой SetWindowPos каждые 300 мс ЛОМАЕТ клики WebView2
                        // (down теряется при реордере). Вместо этого проверяем, видимо ли
                        // окно в своей центральной точке, и поднимаем только при перекрытии.
                        let mut rc: RECT = std::mem::zeroed();
                        if GetWindowRect(hwnd, &mut rc).is_ok() {
                            let pt = POINT {
                                x: (rc.left + rc.right) / 2,
                                y: (rc.top + rc.bottom) / 2,
                            };
                            let hit = WindowFromPoint(pt);
                            let mut ours = hit == hwnd;
                            let mut h = hit;
                            for _ in 0..8 {
                                if ours {
                                    break;
                                }
                                match GetParent(h) {
                                    Ok(p) if p != HWND::default() => {
                                        ours = p == hwnd;
                                        h = p;
                                    }
                                    _ => break,
                                }
                            }
                            if !ours {
                                let _ = SetWindowPos(
                                    hwnd,
                                    Some(HWND_TOPMOST),
                                    0,
                                    0,
                                    0,
                                    0,
                                    SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
                                );
                            }
                        }
                    }
                }
            }
        });
    }
}

/// Есть ли на переднем плане полноэкранное приложение (видео, игра).
#[cfg(windows)]
fn fullscreen_app_running() -> bool {
    use windows::Win32::UI::Shell::{
        SHQueryUserNotificationState, QUNS_PRESENTATION_MODE, QUNS_RUNNING_D3D_FULL_SCREEN,
    };

    unsafe {
        SHQueryUserNotificationState()
            .map(|s| s == QUNS_RUNNING_D3D_FULL_SCREEN || s == QUNS_PRESENTATION_MODE)
            .unwrap_or(false)
    }
}

/// Автозапуск с Windows через HKCU\...\CurrentVersion\Run.
#[cfg(windows)]
mod autostart {
    use windows::core::w;
    use windows::Win32::System::Registry::*;

    pub fn enabled() -> bool {
        unsafe {
            let mut key = HKEY::default();
            if RegOpenKeyExW(
                HKEY_CURRENT_USER,
                w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run"),
                Some(0),
                KEY_READ,
                &mut key,
            )
            .is_err()
            {
                return false;
            }
            let mut size = 0u32;
            let r = RegQueryValueExW(key, w!("HungerTimer"), None, None, None, Some(&mut size));
            let _ = RegCloseKey(key);
            r.is_ok()
        }
    }

    pub fn set(enable: bool) {
        unsafe {
            let mut key = HKEY::default();
            if RegOpenKeyExW(
                HKEY_CURRENT_USER,
                w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run"),
                Some(0),
                KEY_WRITE,
                &mut key,
            )
            .is_err()
            {
                return;
            }
            if enable {
                if let Ok(exe) = std::env::current_exe() {
                    let path = exe.to_string_lossy();
                    let wide: Vec<u16> =
                        path.encode_utf16().chain(std::iter::once(0)).collect();
                    let bytes: &[u8] = std::slice::from_raw_parts(
                        wide.as_ptr() as *const u8,
                        wide.len() * 2,
                    );
                    let _ = RegSetValueExW(key, w!("HungerTimer"), Some(0), REG_SZ, Some(bytes));
                }
            } else {
                let _ = RegDeleteValueW(key, w!("HungerTimer"));
            }
            let _ = RegCloseKey(key);
        }
    }
}

/// Геометрия панели задач и левый край системного трея (TrayNotifyWnd).
/// Координаты физические (процесс DPI-aware). Возвращает (x_виджета, y_виджета):
/// виджет встаёт вплотную слева от трея, по вертикали центрируется в панели.
#[cfg(windows)]
fn widget_position_near_tray(win_w: i32, win_h: i32) -> Option<(i32, i32)> {
    use windows::Win32::Foundation::{HWND, RECT};
    use windows::Win32::UI::WindowsAndMessaging::{FindWindowExW, FindWindowW, GetWindowRect};
    use windows::core::w;

    unsafe {
        let tray = FindWindowW(w!("Shell_TrayWnd"), None).ok()?;
        let notify =
            FindWindowExW(Some(tray), Some(HWND::default()), w!("TrayNotifyWnd"), None).ok()?;
        let mut trc: RECT = std::mem::zeroed();
        GetWindowRect(tray, &mut trc).ok()?;
        let mut nrc: RECT = std::mem::zeroed();
        GetWindowRect(notify, &mut nrc).ok()?;
        let x = nrc.left - win_w - 8;
        let y = trc.top + (trc.bottom - trc.top - win_h) / 2;
        Some((x, y))
    }
}

fn main() {
    // Скрытый флаг самопроверки: реестр автозапуска (round-trip) + детектор полного экрана.
    // Только в debug-сборке есть консоль (release — windows subsystem).
    #[cfg(windows)]
    if std::env::args().any(|a| a == "--selftest") {
        println!("autostart before: {}", autostart::enabled());
        autostart::set(true);
        println!("autostart after set(true): {}", autostart::enabled());
        autostart::set(false);
        println!("autostart after set(false): {}", autostart::enabled());
        println!("fullscreen_app_running: {}", fullscreen_app_running());
        return;
    }

    tauri::Builder::default()
        // Single-instance guard: повторный запуск не плодит процесс,
        // а показывает и фокусирует уже существующее окно виджета.
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            if let Some(win) = app.get_webview_window("main") {
                let _ = win.unminimize();
                let _ = win.show();
                let _ = win.set_focus();
            }
        }))
        .setup(|app| {
            // Позиционируем виджет на панель задач вплотную слева от системного трея.
            // Fallback — левый нижний угол (место виджета погоды).
            if let Some(win) = app.get_webview_window("main") {
                #[cfg(windows)]
                let positioned = if let Ok(s) = win.outer_size() {
                    widget_position_near_tray(s.width as i32, s.height as i32)
                        .map(|(x, y)| {
                            let _ = win.set_position(tauri::Position::Physical(
                                tauri::PhysicalPosition { x, y },
                            ));
                        })
                        .is_some()
                } else {
                    false
                };
                #[cfg(not(windows))]
                let positioned = false;
                if !positioned {
                    if let Ok(Some(monitor)) = win.primary_monitor() {
                        let mon = monitor.size();
                        if let Ok(win_size) = win.outer_size() {
                            let x = 12i32;
                            let y = mon.height as i32 - win_size.height as i32;
                            let _ = win.set_position(tauri::Position::Physical(
                                tauri::PhysicalPosition { x, y },
                            ));
                        }
                    }
                }
                #[cfg(windows)]
                {
                    make_tool_window(&win);
                    spawn_zorder_keeper(&win);
                }
            }

            // Трей: единственный способ закрыть приложение и запасной канал управления.
            let quit = MenuItem::with_id(app, "quit", "Выход", true, None::<&str>)?;
            #[cfg(windows)]
            let autostart_item = {
                use tauri::menu::CheckMenuItemBuilder;
                CheckMenuItemBuilder::with_id("autostart", "Автозапуск с Windows")
                    .checked(autostart::enabled())
                    .build(app)?
            };
            #[cfg(windows)]
            let menu = Menu::with_items(app, &[&autostart_item, &quit])?;
            #[cfg(not(windows))]
            let menu = Menu::with_items(app, &[&quit])?;
            #[cfg(windows)]
            let autostart_handle = autostart_item.clone();
            let _tray = TrayIconBuilder::new()
                .icon(app.default_window_icon().unwrap().clone())
                .tooltip("Hunger Timer — 2 часа после голода")
                .menu(&menu)
                .on_menu_event(move |app, event| {
                    match event.id.as_ref() {
                        "quit" => app.exit(0),
                        #[cfg(windows)]
                        "autostart" => {
                            let new = !autostart::enabled();
                            autostart::set(new);
                            let _ = autostart_handle.set_checked(new);
                        }
                        _ => {}
                    }
                })
                .build(app)?;

            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running hunger-timer");
}
