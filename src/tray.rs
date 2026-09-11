//! Значок в углу экрана.
//!
//! Нужен ради одного: крестик не должен рвать разговор. Человек играет,
//! окно ему мешает, он его закрывает — и комната при этом обязана остаться.
//! Поэтому крестик всегда прячет окно в значок, а закрыть приложение можно
//! только из меню значка. Дверь наружу одна, зато её ни с чем не спутать:
//! иначе вышло бы, что крестик то закрывает, то не закрывает, смотря в
//! комнате ты или нет, — и человек никогда не знал бы заранее, что будет.
//!
//! Значок есть только на Windows: там он и нужен, а на прочих системах
//! библиотека тянет за собой GTK. Всё остальное собирается и работает без
//! него — в этом случае крестик просто закрывает приложение, как раньше.

/// Очередь нажатий: её наполняет поток наблюдения, разбирает поток
/// отрисовки.
pub type Queue = std::sync::Arc<std::sync::Mutex<Vec<Cmd>>>;

/// Что человек попросил через значок.
// На системах без значка ничего из этого не создаётся — это нормально.
#[allow(dead_code)]
#[derive(PartialEq, Clone, Copy)]
pub enum Cmd {
    /// Показать окно обратно.
    Show,
    /// Переключить микрофон, не открывая окна.
    ToggleMute,
    /// Выйти совсем.
    Quit,
}

#[cfg(all(windows, feature = "tray"))]
mod imp {
    use super::Cmd;
    use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
    use tray_icon::{Icon, MouseButton, TrayIcon, TrayIconBuilder, TrayIconEvent};

    pub struct Tray {
        // Значок должен жить, пока живёт приложение: уронив его, мы уроним
        // и картинку в трее.
        _icon: TrayIcon,
    }

    impl Tray {
        pub fn new(rgba: Vec<u8>, w: u32, h: u32) -> Option<Self> {
            let icon = Icon::from_rgba(rgba, w, h).ok()?;

            let menu = Menu::new();
            let open = MenuItem::with_id("open", "Открыть окно", true, None);
            let mute = MenuItem::with_id("mute", "Микрофон вкл/выкл", true, None);
            let quit = MenuItem::with_id("quit", "Закрыть приложение", true, None);
            menu.append(&open).ok()?;
            menu.append(&mute).ok()?;
            menu.append(&PredefinedMenuItem::separator()).ok()?;
            menu.append(&quit).ok()?;

            let icon = TrayIconBuilder::new()
                .with_menu(Box::new(menu))
                // Меню — только по правой кнопке. Левая должна просто
                // открывать окно: человек тычет в значок, чтобы увидеть
                // приложение, а не чтобы прочитать список пунктов.
                .with_menu_on_left_click(false)
                .with_tooltip("voicechat")
                .with_icon(icon)
                .build()
                .ok()?;

            Some(Self { _icon: icon })
        }

    }

    /// Поток, который слушает значок и будит окно.
    ///
    /// Раньше события разбирались в отрисовке — а спрятанное окно
    /// перерисовывается раз в секунду, и нажатие столько же и ждало.
    /// Отсюда и «надо укликаться». Поток видит нажатие за сорок
    /// миллисекунд и сам просит окно проснуться.
    pub fn spawn_watcher(wake: impl Fn() + Send + 'static) -> super::Queue {
        let queue: super::Queue = Default::default();
        let mine = queue.clone();
        std::thread::spawn(move || loop {
            let mut got = Vec::new();

            // Нажатие по самому значку. Левая кнопка открывает окно;
            // правая сюда не доходит — её забирает меню.
            while let Ok(event) = TrayIconEvent::receiver().try_recv() {
                match event {
                    TrayIconEvent::Click { button: MouseButton::Left, .. }
                    | TrayIconEvent::DoubleClick { button: MouseButton::Left, .. } => {
                        got.push(Cmd::Show)
                    }
                    _ => {}
                }
            }

            while let Ok(event) = MenuEvent::receiver().try_recv() {
                match event.id.as_ref() {
                    "open" => got.push(Cmd::Show),
                    "mute" => got.push(Cmd::ToggleMute),
                    "quit" => got.push(Cmd::Quit),
                    _ => {}
                }
            }

            if !got.is_empty() {
                mine.lock().unwrap().extend(got);
                wake();
            }
            std::thread::sleep(std::time::Duration::from_millis(40));
        });
        queue
    }
}

#[cfg(not(all(windows, feature = "tray")))]
mod imp {
    /// Заглушка для систем без значка. Ничего не создаёт — и приложение
    /// ведёт себя как раньше: крестик закрывает.
    pub struct Tray;

    impl Tray {
        pub fn new(_rgba: Vec<u8>, _w: u32, _h: u32) -> Option<Self> {
            None
        }
    }

    pub fn spawn_watcher(_wake: impl Fn() + Send + 'static) -> super::Queue {
        Default::default()
    }
}

pub use imp::{spawn_watcher, Tray};
