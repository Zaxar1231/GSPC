//! Окно Win32 с экраном гостя: создание, цикл сообщений, мышь и клавиатура.
//!
//! Окно живёт в UI-потоке; композитор рисует в него из своего потока (swapchain привязан к
//! HWND), поэтому UI никогда не блокирует вывод кадра. События ввода отдаются обработчику
//! сразу из оконной процедуры, без ожидания кадра.

use std::cell::RefCell;

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{ReleaseCapture, SetCapture};
use windows::Win32::UI::WindowsAndMessaging::{
    AdjustWindowRectEx, CS_HREDRAW, CS_VREDRAW, CW_USEDEFAULT, CreateWindowExW, DefWindowProcW,
    DestroyWindow, DispatchMessageW, GetClientRect, GetMessageW, IDC_ARROW, LoadCursorW, MSG,
    PostMessageW, PostQuitMessage, RegisterClassExW, SPI_GETWORKAREA, SW_SHOW,
    SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, ShowWindow, SystemParametersInfoW, TranslateMessage,
    WM_CLOSE, WM_DESTROY, WM_KEYDOWN, WM_KEYUP, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEMOVE,
    WM_SYSKEYDOWN, WM_SYSKEYUP, WNDCLASSEXW, WS_EX_APPWINDOW, WS_OVERLAPPEDWINDOW,
};
use windows::core::{PCWSTR, w};

/// Событие окна.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum WindowEvent {
    /// Указатель (левая кнопка мыши): координаты в долях клиентской области 0..1.
    Pointer {
        /// X.
        x: f32,
        /// Y.
        y: f32,
        /// Кнопка нажата.
        down: bool,
    },
    /// Клавиша: скан-код набора 1 и флаг E0.
    Key {
        /// Скан-код.
        scancode: u16,
        /// Префикс E0.
        extended: bool,
        /// Нажата.
        pressed: bool,
    },
    /// Пользователь закрывает окно.
    Close,
}

/// Дескриптор окна, который можно передать в другой поток (композитору).
#[derive(Debug, Clone, Copy)]
pub struct WindowHandle(pub HWND);

// SAFETY: HWND — идентификатор окна; Win32 разрешает передавать его между потоками
// (создание swapchain и PostMessage из другого потока допустимы).
unsafe impl Send for WindowHandle {}

impl WindowHandle {
    /// Просит окно закрыться (из любого потока).
    pub fn close(self) {
        // SAFETY: PostMessage безопасен для любого HWND; если окна уже нет, вернётся ошибка.
        let _ = unsafe { PostMessageW(Some(self.0), WM_CLOSE, WPARAM(0), LPARAM(0)) };
    }
}

type Handler = Box<dyn FnMut(WindowEvent)>;

thread_local! {
    static HANDLER: RefCell<Option<Handler>> = const { RefCell::new(None) };
}

fn emit(e: WindowEvent) {
    HANDLER.with(|h| {
        if let Ok(mut h) = h.try_borrow_mut()
            && let Some(f) = h.as_mut()
        {
            f(e);
        }
    });
}

/// Включает DPI awareness v2 для процесса (вызывать до создания окон).
pub fn enable_dpi_awareness() {
    // SAFETY: простой вызов настройки процесса.
    let r = unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
    if let Err(e) = r {
        tracing::debug!(error = %e, "DPI awareness уже задана");
    }
}

fn client_point(hwnd: HWND, lparam: LPARAM) -> (f32, f32) {
    #[allow(
        clippy::cast_possible_truncation,
        reason = "координаты мыши — младшие/старшие 16 бит"
    )]
    let (x, y) = (
        i32::from(lparam.0 as i16),
        i32::from((lparam.0 >> 16) as i16),
    );
    let mut r = RECT::default();
    // SAFETY: окно валидно, выходная структура локальная.
    let _ = unsafe { GetClientRect(hwnd, &raw mut r) };
    #[allow(clippy::cast_precision_loss, reason = "пиксели окна")]
    let (w, h) = (
        ((r.right - r.left).max(1)) as f32,
        ((r.bottom - r.top).max(1)) as f32,
    );
    #[allow(clippy::cast_precision_loss, reason = "пиксели окна")]
    (
        (x as f32 / w).clamp(0.0, 1.0),
        (y as f32 / h).clamp(0.0, 1.0),
    )
}

extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_LBUTTONDOWN => {
            // SAFETY: захват мыши окном из его же потока.
            unsafe { SetCapture(hwnd) };
            let (x, y) = client_point(hwnd, lparam);
            emit(WindowEvent::Pointer { x, y, down: true });
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            // MK_LBUTTON = 1: перемещение с нажатой кнопкой — движение пальца.
            if wparam.0 & 1 != 0 {
                let (x, y) = client_point(hwnd, lparam);
                emit(WindowEvent::Pointer { x, y, down: true });
            }
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            // SAFETY: освобождение захвата.
            let _ = unsafe { ReleaseCapture() };
            let (x, y) = client_point(hwnd, lparam);
            emit(WindowEvent::Pointer { x, y, down: false });
            LRESULT(0)
        }
        WM_KEYDOWN | WM_KEYUP | WM_SYSKEYDOWN | WM_SYSKEYUP => {
            let pressed = msg == WM_KEYDOWN || msg == WM_SYSKEYDOWN;
            let repeat = pressed && (lparam.0 >> 30) & 1 != 0;
            #[allow(clippy::cast_possible_truncation, reason = "биты 16..23 lParam")]
            let scancode = ((lparam.0 >> 16) & 0xFF) as u16;
            let extended = (lparam.0 >> 24) & 1 != 0;
            // Автоповтор Android делает сам.
            if !repeat {
                emit(WindowEvent::Key {
                    scancode,
                    extended,
                    pressed,
                });
            }
            if msg == WM_SYSKEYDOWN || msg == WM_SYSKEYUP {
                // Alt+F4 и системное меню продолжают работать.
                // SAFETY: обработка по умолчанию.
                return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) };
            }
            LRESULT(0)
        }
        WM_CLOSE => {
            emit(WindowEvent::Close);
            // SAFETY: окно принадлежит этому потоку.
            let _ = unsafe { DestroyWindow(hwnd) };
            LRESULT(0)
        }
        WM_DESTROY => {
            // SAFETY: завершение цикла сообщений этого потока.
            unsafe { PostQuitMessage(0) };
            LRESULT(0)
        }
        // SAFETY: обработка по умолчанию.
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

/// Размер клиентской области: экран гостя, уменьшенный до 85% рабочей области монитора.
fn fit_client(width: u32, height: u32) -> (i32, i32) {
    let mut work = RECT::default();
    // SAFETY: выходная структура локальная.
    let ok = unsafe {
        SystemParametersInfoW(
            SPI_GETWORKAREA,
            0,
            Some((&raw mut work).cast()),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )
    }
    .is_ok();
    let (ww, wh) = if ok {
        (work.right - work.left, work.bottom - work.top)
    } else {
        (1920, 1080)
    };
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        reason = "пиксели окна"
    )]
    {
        let (gw, gh) = (width.max(1) as f32, height.max(1) as f32);
        let scale = (ww as f32 * 0.85 / gw).min(wh as f32 * 0.85 / gh).min(1.0);
        ((gw * scale) as i32, (gh * scale) as i32)
    }
}

/// Создаёт окно и регистрирует обработчик событий (в текущем, UI-потоке).
///
/// # Errors
/// Ошибка регистрации класса или создания окна.
pub fn create(
    title: &str,
    width: u32,
    height: u32,
    handler: impl FnMut(WindowEvent) + 'static,
) -> windows::core::Result<WindowHandle> {
    // SAFETY: модуль текущего процесса.
    let instance = unsafe { GetModuleHandleW(None) }?;
    let class = w!("RustDroidWindow");
    let wc = WNDCLASSEXW {
        cbSize: size_of::<WNDCLASSEXW>() as u32,
        style: CS_HREDRAW | CS_VREDRAW,
        lpfnWndProc: Some(wndproc),
        hInstance: instance.into(),
        // SAFETY: системный курсор.
        hCursor: unsafe { LoadCursorW(None, IDC_ARROW) }?,
        lpszClassName: class,
        ..Default::default()
    };
    // SAFETY: структура класса локальная и корректно заполнена; повторная регистрация вернёт 0.
    unsafe { RegisterClassExW(&raw const wc) };

    let (cw, ch) = fit_client(width, height);
    let mut rect = RECT {
        left: 0,
        top: 0,
        right: cw,
        bottom: ch,
    };
    // SAFETY: структура локальная.
    unsafe { AdjustWindowRectEx(&raw mut rect, WS_OVERLAPPEDWINDOW, false, WS_EX_APPWINDOW) }?;
    HANDLER.with(|h| *h.borrow_mut() = Some(Box::new(handler)));
    let title: Vec<u16> = title.encode_utf16().chain(std::iter::once(0)).collect();
    // SAFETY: класс зарегистрирован, строки живут до конца вызова.
    let hwnd = unsafe {
        CreateWindowExW(
            WS_EX_APPWINDOW,
            class,
            PCWSTR(title.as_ptr()),
            WS_OVERLAPPEDWINDOW,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            rect.right - rect.left,
            rect.bottom - rect.top,
            None,
            None,
            Some(instance.into()),
            None,
        )
    }?;
    // SAFETY: окно только что создано.
    let _ = unsafe { ShowWindow(hwnd, SW_SHOW) };
    Ok(WindowHandle(hwnd))
}

/// Цикл сообщений до закрытия окна.
pub fn run_message_loop() {
    let mut msg = MSG::default();
    // SAFETY: стандартный цикл сообщений текущего потока.
    while unsafe { GetMessageW(&raw mut msg, None, 0, 0) }.as_bool() {
        // SAFETY: сообщение получено из GetMessageW.
        let _ = unsafe { TranslateMessage(&raw const msg) };
        // SAFETY: как выше.
        unsafe { DispatchMessageW(&raw const msg) };
    }
    HANDLER.with(|h| *h.borrow_mut() = None);
}
