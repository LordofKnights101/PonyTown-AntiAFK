//! Iced application: top settings bar, live pony.town web view (wry child),
//! and a single-bar logging strip at the bottom - the Rust port of the
//! Python app's gui.py + Protection wiring.

use crate::cdp;
use crate::keepalive::{self, Engine, EngineParams, UiEvent};
use crate::settings::{self, Settings};
use crate::util;
use iced::widget::{button, checkbox, column, container, row, text};
use iced::{Color, Element, Length, Subscription, Task};
use raw_window_handle::{RawWindowHandle, WindowHandle};
use std::collections::VecDeque;
use std::num::NonZeroIsize;
use std::path::PathBuf;
use std::sync::mpsc::Receiver;
use std::time::Duration;

const TOP_BAR_H: f32 = 64.0;
const BOTTOM_BAR_H: f32 = 40.0;

// wry::WebView holds COM pointers (STA) and the event channel is drained on
// the UI thread only, so these wrappers are sound in practice.
struct WebViewBox(wry::WebView);
unsafe impl Send for WebViewBox {}

struct EngineRx(Receiver<UiEvent>);
unsafe impl Send for EngineRx {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Rect {
    x: i32,
    y: i32,
    w: i32,
    h: i32,
}

pub struct App {
    settings: Settings,
    status: &'static str,
    logs: VecDeque<String>,
    webview: Option<WebViewBox>,
    hwnd: Option<isize>,
    last_rect: Option<Rect>,
    port: u16,
    engine: Option<(Engine, EngineRx)>,
    window_id: Option<iced::window::Id>,
}

#[derive(Debug, Clone)]
pub enum Message {
    Startup,
    GotWindowId(Option<iced::window::Id>),
    GotHandle(isize),
    Poll,
    Resized,
    ToggleRun,
    CycleMode,
    IntervalDelta(i32),
    ToggleSkip(bool),
}

impl App {
    pub fn run() -> iced::Result {
        iced::application("Pony Town Anti-AFK", App::update, App::view)
            .theme(|_| iced::Theme::Dark)
            .subscription(App::subscription)
            .window(iced::window::Settings {
                size: iced::Size::new(1100.0, 720.0),
                min_size: Some(iced::Size::new(860.0, 540.0)),
                ..Default::default()
            })
            .run_with(App::new)
    }

    fn new() -> (Self, Task<Message>) {
        (
            App {
                settings: settings::load(),
                status: "off",
                logs: VecDeque::new(),
                webview: None,
                hwnd: None,
                last_rect: None,
                port: cdp::free_port(),
                engine: None,
                window_id: None,
            },
            Task::perform(std::future::ready(()), |_| Message::Startup),
        )
    }

    fn subscription(&self) -> Subscription<Message> {
        Subscription::batch([
            iced::time::every(Duration::from_millis(250)).map(|_| Message::Poll),
            iced::window::resize_events().map(|_| Message::Resized),
        ])
    }

    fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::Startup => iced::window::get_oldest().map(Message::GotWindowId),
            Message::GotWindowId(id) => match id {
                Some(id) => {
                    self.window_id = Some(id);
                    iced::window::run_with_handle(id, |handle| {
                        #[allow(deprecated)]
                        match raw_window_handle::HasRawWindowHandle::raw_window_handle(&handle) {
                            Ok(RawWindowHandle::Win32(w)) => w.hwnd.get(),
                            _ => 0,
                        }
                    })
                    .map(Message::GotHandle)
                }
                None => {
                    self.push_log("No window found - the game view can not start.");
                    Task::none()
                }
            },
            Message::GotHandle(hwnd) => {
                if hwnd == 0 {
                    self.push_log("Could not get the native window handle - the game view can not start.");
                    return Task::none();
                }
                self.hwnd = Some(hwnd);
                if let Err(e) = self.build_webview(hwnd) {
                    self.push_log(&format!("Failed to embed the game view: {}", e));
                }
                Task::none()
            }
            Message::Resized => {
                self.reposition();
                Task::none()
            }
            Message::Poll => {
                self.drain_events();
                self.reposition();
                Task::none()
            }
            Message::ToggleRun => {
                if self.engine.is_some() {
                    self.stop_engine();
                } else {
                    self.start_engine();
                }
                Task::none()
            }
            Message::CycleMode => {
                self.settings.mode = self.settings.mode.next();
                settings::save(&self.settings);
                Task::none()
            }
            Message::IntervalDelta(d) => {
                self.settings.interval_min = (self.settings.interval_min + d).clamp(2, 10);
                settings::save(&self.settings);
                Task::none()
            }
            Message::ToggleSkip(v) => {
                self.settings.skip_if_active = v;
                settings::save(&self.settings);
                Task::none()
            }
        }
    }

    // -- web view management -------------------------------------------------

    fn build_webview(&mut self, hwnd: isize) -> Result<(), String> {
        use wry::WebViewBuilderExtWindows;

        let port = cdp::free_port();
        self.port = port;
        let data_dir: PathBuf = util::base_dir().join("webview-data");
        std::fs::create_dir_all(&data_dir).map_err(|e| e.to_string())?;

        let raw = RawWindowHandle::Win32(wry_window_handle(hwnd)?);
        // SAFETY: the raw handle describes our own live top-level window and
        // is only used to parent the web view during the call below.
        let handle = unsafe { WindowHandle::borrow_raw(raw) };

        // Overriding browser args replaces wry's defaults, so re-add them
        // (per the wry docs) plus the DevTools port our engine talks to.
        let args = format!(
            "--disable-features=msWebOOUI,msPdfOOUI,msSmartScreenProtection \
             --autoplay-policy=no-user-gesture-required \
             --remote-debugging-port={}",
            port
        );

        let mut web_context = wry::WebContext::new(Some(data_dir));
        let builder = wry::WebViewBuilder::new_with_web_context(&mut web_context)
            .with_url(&self.settings.game_url)
            .with_devtools(true)
            .with_initialization_script(keepalive::PROBE_JS)
            .with_additional_browser_args(args);

        let webview = builder.build_as_child(&handle).map_err(|e| e.to_string())?;
        self.webview = Some(WebViewBox(webview));
        self.last_rect = None;
        self.reposition();
        self.push_log(&format!(
            "Game view loading {} (engine port {})",
            self.settings.game_url, port
        ));
        Ok(())
    }

    fn reposition(&mut self) {
        use windows::Win32::Foundation::{HWND, RECT};
        use windows::Win32::UI::HiDpi::GetDpiForWindow;
        use windows::Win32::UI::WindowsAndMessaging::GetClientRect;

        let (Some(wv), Some(hwnd)) = (&self.webview, self.hwnd) else {
            return;
        };
        let h = HWND(hwnd as *mut _);
        let mut rect = RECT::default();
        let ok = unsafe { GetClientRect(h, &mut rect) };
        if ok.is_err() {
            return;
        }
        let dpi = unsafe { GetDpiForWindow(h) } as f32 / 96.0;
        let top = (TOP_BAR_H * dpi).round() as i32;
        let bottom = (BOTTOM_BAR_H * dpi).round() as i32;
        let r = Rect {
            x: 0,
            y: top,
            w: (rect.right - rect.left).max(1),
            h: (rect.bottom - rect.top - top - bottom).max(1),
        };
        if self.last_rect != Some(r) {
            let _ = wv.0.set_bounds(wry::Rect {
                position: wry::dpi::PhysicalPosition::new(r.x, r.y).into(),
                size: wry::dpi::PhysicalSize::new(r.w, r.h).into(),
            });
            self.last_rect = Some(r);
        }
    }

    // -- engine management ----------------------------------------------------

    fn start_engine(&mut self) {
        let params = EngineParams {
            port: self.port,
            game_url: self.settings.game_url.clone(),
            domain: util::domain_of(&self.settings.game_url),
            interval_sec: self.settings.interval_min as f64 * 60.0,
            mode: self.settings.mode,
            skip_if_active: self.settings.skip_if_active,
        };
        let (tx, rx) = std::sync::mpsc::channel::<UiEvent>();
        let engine = keepalive::spawn(params, tx);
        self.engine = Some((engine, EngineRx(rx)));
        self.status = "starting";
        self.push_log(&format!(
            "Starting protection (every {} min, mode: {})",
            self.settings.interval_min,
            self.settings.mode.label()
        ));
    }

    fn stop_engine(&mut self) {
        if let Some((engine, _)) = self.engine.take() {
            engine.stop();
            self.push_log("Stopping protection.");
        }
        self.status = "off";
    }

    fn drain_events(&mut self) {
        // Collect pending engine events first so the immutable borrow on the
        // engine channel ends before we mutate app state below.
        let mut events = Vec::new();
        if let Some((_, rx)) = &self.engine {
            while let Ok(ev) = rx.0.try_recv() {
                events.push(ev);
            }
        }
        for ev in events {
            match ev {
                UiEvent::Log(line) => self.push_log(&line),
                UiEvent::Status(s) => self.status = s,
                UiEvent::WebViewDied => {
                    self.push_log("Game engine process died - recreating it ...");
                    self.status = "starting";
                    if let Some(hwnd) = self.hwnd {
                        self.webview = None;
                        if let Err(e) = self.build_webview(hwnd) {
                            self.push_log(&format!("Could not recreate the game view: {}", e));
                            self.status = "error";
                        } else if let Some((engine, _)) = &self.engine {
                            engine.restart(self.port);
                        }
                    }
                }
            }
        }
    }

    fn push_log(&mut self, line: &str) {
        self.logs.push_back(line.to_string());
        while self.logs.len() > 400 {
            self.logs.pop_front();
        }
    }

    // -- view -------------------------------------------------------------------

    fn view(&self) -> Element<'_, Message> {
        let running = self.engine.is_some();
        let toggle_label = if running { "Stop protection" } else { "Start protection" };

        let status_text = match self.status {
            "off" => "●  Protection is OFF",
            "starting" => "●  Starting engine ...",
            "running" => "●  Protection is ON - you can go AFK",
            "waiting" => "●  Waiting for the game page ...",
            "error" => "●  Error - check the log below",
            _ => "●",
        };
        let status_color = match self.status {
            "off" => Color::from_rgb(0.59, 0.65, 0.65),
            "running" => Color::from_rgb(0.18, 0.80, 0.44),
            "error" => Color::from_rgb(0.91, 0.30, 0.24),
            _ => Color::from_rgb(0.95, 0.61, 0.07),
        };

        let top_bar = row![
            button(text(toggle_label).size(14)).on_press(Message::ToggleRun),
            button(text(format!("Mode: {}", self.settings.mode.label())).size(13))
                .on_press(Message::CycleMode),
            button(text("−").size(14)).on_press(Message::IntervalDelta(-1)),
            text(format!("{} min", self.settings.interval_min)).size(13),
            button(text("+").size(14)).on_press(Message::IntervalDelta(1)),
            checkbox("Skip when I play", self.settings.skip_if_active)
                .on_toggle(Message::ToggleSkip),
            iced::widget::horizontal_space(),
            text(status_text).size(14).color(status_color),
        ]
        .spacing(10)
        .align_y(iced::Alignment::Center)
        .width(Length::Fill)
        .height(Length::Fixed(TOP_BAR_H))
        .padding([0, 12]);

        let bottom_bar = container(text(latest_log(&self.logs)).size(13).color(
            Color::from_rgb(0.85, 0.85, 0.85),
        ))
        .width(Length::Fill)
        .height(Length::Fixed(BOTTOM_BAR_H))
        .padding([0, 12])
        .align_y(iced::Alignment::Center);

        // The middle area is intentionally empty: the live pony.town web
        // view is a native child surface layered exactly over it.
        let middle = container(iced::widget::horizontal_space())
            .width(Length::Fill)
            .height(Length::Fill);

        column![top_bar, middle, bottom_bar]
            .spacing(0)
            .width(Length::Fill)
            .height(Length::Fill)
            .into()
    }
}

fn latest_log(logs: &VecDeque<String>) -> String {
    logs.back().map(|s| s.as_str()).unwrap_or("Ready.").to_string()
}

#[cfg(windows)]
fn wry_window_handle(
    hwnd: isize,
) -> Result<raw_window_handle::Win32WindowHandle, String> {
    let nz = NonZeroIsize::new(hwnd).ok_or("invalid window handle")?;
    Ok(raw_window_handle::Win32WindowHandle::new(nz))
}

#[cfg(not(windows))]
fn wry_window_handle(_: isize) -> Result<std::convert::Infallible, String> {
    Err("unsupported platform".into())
}
