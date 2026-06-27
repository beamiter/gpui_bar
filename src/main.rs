// gpui_bar — gpui port of iced_bar.
//
// Feature parity with iced_bar:
//   * 9 nerd-font workspace tag buttons with selected/filled/urgent/occupied visuals
//   * Layout toggle + 3-option selector
//   * Pills: CPU, memory, battery, brightness, volume, screenshot, time, monitor, scale
//   * Click semantics: tag → view-tag command; volume/brightness left/right click;
//     screenshot pill spawns `flameshot gui`; clock toggles seconds
//   * Background thread watches SharedRingBuffer and posts updates via
//     `cx.spawn`; a 1Hz timer drives the clock + system-monitor refresh
//
// gpui's render model: a `Render` impl returns an `Element` tree built from
// `div()` with tailwind-like styling. State mutation happens via
// `cx.listener(|this, ev, window, cx| { ... cx.notify(); })`.

use std::env;
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use chrono::Local;
use log::{error, info, warn};

use shared_structures::{CommandType, MonitorInfo, SharedCommand, SharedMessage, SharedRingBuffer};
use xbar_core::audio_manager::AudioManager;
use xbar_core::brightness::BrightnessManager;
use xbar_core::initialize_logging;
use xbar_core::system_monitor::SystemMonitor;

use gpui::{
    App, Bounds, Context, Entity, IntoElement, MouseButton, ParentElement, Pixels, Render, Rgba,
    SharedString, Styled, Task, Window, WindowBackgroundAppearance, WindowBounds, WindowDecorations,
    WindowKind, WindowOptions, div, prelude::*, px, rgb, rgba, size,
};
use gpui_platform::application;

// -------- Constants (mirror iced_bar) ----------------------------------------

const NERD_FONT: &str = "JetBrainsMono Nerd Font";

const TAG_ICONS: [&str; 9] = [
    "\u{F0A1E}", "\u{F0239}", "\u{F0A1B}", "\u{F0B79}", "\u{F024B}", "\u{F0388}", "\u{F0567}",
    "\u{F01F0}", "\u{F0297}",
];

const ICON_CPU: &str = "\u{F4BC}";
const ICON_MEM: &str = "\u{F035B}";
const ICON_BAT_FULL: &str = "\u{F0079}";
const ICON_BAT_CHG: &str = "\u{F0084}";
const ICON_VOL_HIGH: &str = "\u{F057E}";
const ICON_VOL_MID: &str = "\u{F0580}";
const ICON_VOL_LOW: &str = "\u{F057F}";
const ICON_VOL_MUTE: &str = "\u{F075F}";
const ICON_BRIGHT: &str = "\u{F00DE}";
const ICON_SHOT: &str = "\u{F0104}";
const ICON_TIME: &str = "\u{F0954}";
const ICON_MON: &str = "\u{F0379}";
const ICON_M0: &str = "\u{F02DA}";
const ICON_M1: &str = "\u{F02DB}";

const TAG_COLORS: [u32; 9] = [
    0xFF6B6B, 0x4ECDC4, 0x45B7D1, 0x96CEB4, 0xFECA57, 0xFF9FF3, 0x54A0FF, 0x5F27CD, 0x00D2D3,
];

// -------- App state ----------------------------------------------------------

struct GpuiBar {
    active_tab: usize,
    shared_buffer_rc: Option<Arc<SharedRingBuffer>>,

    monitor_info_opt: Option<MonitorInfo>,
    formated_now: String,
    show_seconds: bool,
    layout_symbol: String,
    monitor_num: i32,
    layout_selector_open: bool,

    audio_manager: AudioManager,
    system_monitor: SystemMonitor,
    brightness_manager: BrightnessManager,

    last_clock_update: Instant,
    last_monitor_update: Instant,

    stop_flag: Arc<AtomicBool>,
    _timer_task: Option<Task<()>>,
}

impl GpuiBar {
    fn new(cx: &mut Context<Self>) -> Self {
        let args: Vec<String> = env::args().collect();
        let shared_path = args.iter().skip(1).last().cloned().unwrap_or_default();
        let shared_buffer_rc =
            SharedRingBuffer::create_shared_ring_buffer_aux(&shared_path).map(Arc::new);

        let mut this = Self {
            active_tab: 0,
            shared_buffer_rc,
            monitor_info_opt: None,
            formated_now: String::new(),
            show_seconds: true,
            layout_symbol: "[]=".to_string(),
            monitor_num: 0,
            layout_selector_open: false,
            audio_manager: AudioManager::new(),
            system_monitor: SystemMonitor::new(5),
            brightness_manager: BrightnessManager::new(),
            last_clock_update: Instant::now(),
            last_monitor_update: Instant::now(),
            stop_flag: Arc::new(AtomicBool::new(false)),
            _timer_task: None,
        };
        this.spawn_clock(cx);
        this.spawn_shared_watcher(cx);
        this
    }

    fn spawn_clock(&mut self, cx: &mut Context<Self>) {
        let task = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_secs(1))
                    .await;
                let _ = this.update(cx, |this, cx| {
                    this.tick();
                    cx.notify();
                });
            }
        });
        self._timer_task = Some(task);
    }

    fn spawn_shared_watcher(&mut self, cx: &mut Context<Self>) {
        let Some(buf) = self.shared_buffer_rc.clone() else {
            warn!("No shared buffer; skipping watcher thread");
            return;
        };

        let (tx, mut rx) = futures::channel::mpsc::channel::<SharedMessage>(64);
        let stop = self.stop_flag.clone();
        std::thread::spawn(move || {
            let mut prev_ts: u128 = 0;
            let mut tx = tx;
            while !stop.load(Ordering::Relaxed) {
                match buf.wait_for_message(Some(Duration::from_secs(2))) {
                    Ok(true) => {
                        if let Ok(Some(msg)) = buf.try_read_latest_message() {
                            let ts = msg.timestamp as u128;
                            if prev_ts != ts {
                                prev_ts = ts;
                                if tx.try_send(msg).is_err() {
                                    break;
                                }
                            }
                        }
                    }
                    Ok(false) => {}
                    Err(e) => {
                        warn!("wait_for_message failed: {e}");
                        break;
                    }
                }
            }
        });

        cx.spawn(async move |this, cx| {
            use futures::StreamExt;
            while let Some(msg) = rx.next().await {
                let _ = this.update(cx, |this, cx| {
                    this.apply_shared(msg);
                    cx.notify();
                });
            }
        })
        .detach();
    }

    fn tick(&mut self) {
        if self.last_clock_update.elapsed() >= Duration::from_millis(900) {
            let fmt = if self.show_seconds {
                "%Y-%m-%d %H:%M:%S"
            } else {
                "%Y-%m-%d %H:%M"
            };
            self.formated_now = Local::now().format(fmt).to_string();
            self.last_clock_update = Instant::now();
        }
        if self.last_monitor_update.elapsed() >= Duration::from_secs(2) {
            self.system_monitor.update_if_needed();
            self.audio_manager.update_if_needed();
            self.brightness_manager.update_if_needed();
            self.last_monitor_update = Instant::now();
        }
    }

    fn apply_shared(&mut self, msg: SharedMessage) {
        self.monitor_info_opt = Some(msg.monitor_info);
        if let Some(mi) = &self.monitor_info_opt {
            self.layout_symbol = mi.get_ltsymbol();
            self.monitor_num = mi.monitor_num;
            for (idx, ts) in mi.tag_status_vec.iter().enumerate() {
                if ts.is_selected {
                    self.active_tab = idx;
                }
            }
        }
    }

    fn send_tag_command(&mut self, is_view: bool) {
        let tag_bit = 1 << self.active_tab;
        let command = if is_view {
            SharedCommand::view_tag(tag_bit, self.monitor_num)
        } else {
            SharedCommand::toggle_tag(tag_bit, self.monitor_num)
        };
        if let Some(buf) = &self.shared_buffer_rc {
            match buf.send_command(command) {
                Ok(true) => info!("Sent command: {:?}", command),
                Ok(false) => warn!("Command buffer full, command dropped"),
                Err(e) => error!("Failed to send command: {}", e),
            }
        }
    }

    fn send_layout_command(&mut self, layout_index: u32) {
        let cmd = SharedCommand::new(CommandType::SetLayout, layout_index, self.monitor_num);
        if let Some(buf) = &self.shared_buffer_rc {
            let _ = buf.send_command(cmd);
        }
    }
}

impl Drop for GpuiBar {
    fn drop(&mut self) {
        self.stop_flag.store(true, Ordering::Relaxed);
    }
}

// -------- Color helpers ------------------------------------------------------

fn rgba_alpha(hex: u32, alpha: f32) -> Rgba {
    let r = ((hex >> 16) & 0xFF) as f32 / 255.0;
    let g = ((hex >> 8) & 0xFF) as f32 / 255.0;
    let b = (hex & 0xFF) as f32 / 255.0;
    Rgba { r, g, b, a: alpha }
}

fn usage_colors(u: f32) -> (Rgba, Rgba) {
    if u <= 30.0 {
        (rgba_alpha(0x1FBF51, 0.9), rgba_alpha(0xFFFFFF, 1.0))
    } else if u <= 60.0 {
        (rgba_alpha(0xF4C20D, 0.9), rgba_alpha(0x000000, 1.0))
    } else if u <= 80.0 {
        (rgba_alpha(0xFF8C1A, 0.9), rgba_alpha(0xFFFFFF, 1.0))
    } else {
        (rgba_alpha(0xE53935, 0.9), rgba_alpha(0xFFFFFF, 1.0))
    }
}

fn battery_colors(pct: f32) -> (Rgba, Rgba) {
    if pct > 50.0 {
        (rgba_alpha(0x1FBF51, 0.9), rgba_alpha(0xFFFFFF, 1.0))
    } else if pct > 20.0 {
        (rgba_alpha(0xF4C20D, 0.9), rgba_alpha(0x000000, 1.0))
    } else {
        (rgba_alpha(0xE53935, 0.9), rgba_alpha(0xFFFFFF, 1.0))
    }
}

fn volume_icon(volume: i32, muted: bool, has_device: bool) -> &'static str {
    if !has_device || muted || volume <= 0 {
        ICON_VOL_MUTE
    } else if volume < 34 {
        ICON_VOL_LOW
    } else if volume < 67 {
        ICON_VOL_MID
    } else {
        ICON_VOL_HIGH
    }
}

fn monitor_num_to_icon(n: i32) -> String {
    match n {
        0 => ICON_M0.to_string(),
        1 => ICON_M1.to_string(),
        n => format!("M{}", n),
    }
}

// -------- Render -------------------------------------------------------------

impl GpuiBar {
    fn render_tag(
        &self,
        index: usize,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let icon = TAG_ICONS[index];
        let tag_color = TAG_COLORS[index];

        let (bg, border_c, is_active) = if let Some(monitor) = &self.monitor_info_opt {
            if let Some(s) = monitor.tag_status_vec.get(index) {
                if s.is_urg {
                    (rgba_alpha(0xDB3645, 1.0), rgba_alpha(0xBC2130, 1.0), true)
                } else if s.is_filled {
                    (rgba_alpha(tag_color, 1.0), rgba_alpha(tag_color, 1.0), true)
                } else if s.is_selected {
                    (rgba_alpha(tag_color, 0.7), rgba_alpha(tag_color, 1.0), true)
                } else if s.is_occ {
                    (rgba_alpha(tag_color, 0.3), rgba_alpha(tag_color, 0.6), false)
                } else {
                    (rgba_alpha(0xFFFFFF, 0.9), rgba_alpha(0xDEE2E6, 1.0), false)
                }
            } else {
                (rgba_alpha(0xFFFFFF, 0.9), rgba_alpha(0xDEE2E6, 1.0), false)
            }
        } else {
            (rgba_alpha(0xFFFFFF, 0.9), rgba_alpha(0xDEE2E6, 1.0), false)
        };

        let text_color = if is_active {
            if index == 4 {
                rgba_alpha(0x333333, 1.0)
            } else {
                rgba_alpha(0xFFFFFF, 1.0)
            }
        } else {
            rgba_alpha(0x333333, 1.0)
        };

        div()
            .id(SharedString::from(format!("tag-{}", index)))
            .w(px(38.))
            .h(px(32.))
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(6.))
            .bg(bg)
            .border_color(border_c)
            .border_2()
            .text_color(text_color)
            .text_size(px(18.))
            .child(icon)
            .hover(|s| s.opacity(0.85))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _ev, _window, cx| {
                    this.active_tab = index;
                    this.send_tag_command(true);
                    cx.notify();
                }),
            )
    }

    fn render_pill(
        &self,
        id: &'static str,
        bg: Rgba,
        border_c: Rgba,
        fg: Rgba,
        content: impl Into<SharedString>,
    ) -> impl IntoElement {
        div()
            .id(id)
            .h(px(26.))
            .px(px(10.))
            .py(px(3.))
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(12.))
            .bg(bg)
            .border_1()
            .border_color(border_c)
            .text_color(fg)
            .text_size(px(14.))
            .child(content.into())
    }

    fn render_usage_pill(&self, id: &'static str, icon: &str, value: f32) -> impl IntoElement {
        let (bg, fg) = usage_colors(value);
        self.render_pill(id, bg, bg, fg, format!("{}  {:.0}%", icon, value))
    }

    fn render_battery_pill(&self) -> impl IntoElement {
        let (pct, charging) = self
            .system_monitor
            .get_snapshot()
            .map(|s| (s.battery_percent, s.is_charging))
            .unwrap_or((0.0, false));
        let icon = if charging { ICON_BAT_CHG } else { ICON_BAT_FULL };
        let (bg, fg) = battery_colors(pct);
        self.render_pill("battery", bg, bg, fg, format!("{}  {:.0}%", icon, pct))
    }

    fn render_brightness_pill(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let label = match self.brightness_manager.percent() {
            Some(p) => format!("{}  {}%", ICON_BRIGHT, p),
            None => format!("{}  --", ICON_BRIGHT),
        };
        let bg = rgba_alpha(0xFDE047, 0.92);
        let border = rgba_alpha(0xFACC15, 1.0);
        let fg = rgba_alpha(0x1F2937, 1.0);

        div()
            .id("brightness")
            .h(px(26.))
            .px(px(10.))
            .py(px(3.))
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(12.))
            .bg(bg)
            .border_1()
            .border_color(border)
            .text_color(fg)
            .text_size(px(14.))
            .child(label)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _ev, _w, cx| {
                    this.brightness_manager.adjust(5);
                    cx.notify();
                }),
            )
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(|this, _ev, _w, cx| {
                    this.brightness_manager.adjust(-5);
                    cx.notify();
                }),
            )
    }

    fn render_volume_pill(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let master = self.audio_manager.get_master_device();
        let (vol, muted, has_dev) = if let Some(d) = master {
            (d.volume.clamp(0, 100), d.is_muted, true)
        } else {
            (0, true, false)
        };
        let icon = volume_icon(vol, muted, has_dev);
        let label = if has_dev {
            format!("{}  {}%", icon, vol)
        } else {
            format!("{}  --", icon)
        };
        let (bg, border, fg) = if muted || !has_dev {
            (
                rgba_alpha(0x787878, 0.85),
                rgba_alpha(0x888888, 1.0),
                rgba_alpha(0xEEEEEE, 1.0),
            )
        } else {
            (
                rgba_alpha(0x14B8A6, 0.9),
                rgba_alpha(0x14B8A6, 1.0),
                rgba_alpha(0xFFFFFF, 1.0),
            )
        };

        div()
            .id("volume")
            .h(px(26.))
            .px(px(10.))
            .py(px(3.))
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(12.))
            .bg(bg)
            .border_1()
            .border_color(border)
            .text_color(fg)
            .text_size(px(14.))
            .child(label)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _ev, _w, cx| {
                    if let Some(d) = this.audio_manager.get_master_device().cloned() {
                        let _ = this.audio_manager.toggle_mute(&d.name);
                    }
                    cx.notify();
                }),
            )
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(|this, _ev, _w, cx| {
                    if let Some(d) = this.audio_manager.get_master_device().cloned() {
                        let new_v = (d.volume - 5).clamp(0, 100);
                        let _ = this.audio_manager.set_volume(&d.name, new_v, d.is_muted);
                    }
                    cx.notify();
                }),
            )
    }

    fn render_screenshot_pill(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let bg = rgba_alpha(0x00CCCC, 0.90);
        let hover_bg = rgba_alpha(0xFF8800, 0.95);
        div()
            .id("screenshot")
            .h(px(26.))
            .px(px(10.))
            .py(px(3.))
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(12.))
            .bg(bg)
            .border_1()
            .border_color(bg)
            .text_color(rgba_alpha(0xFFFFFF, 1.0))
            .text_size(px(15.))
            .child(ICON_SHOT)
            .hover(move |s| s.bg(hover_bg).border_color(hover_bg))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|_this, _ev, _w, _cx| {
                    if let Err(e) = Command::new("flameshot").arg("gui").spawn() {
                        warn!("Failed to spawn flameshot: {e}");
                    }
                }),
            )
    }

    fn render_time_pill(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let bg = rgba_alpha(0x4DA3FF, 0.9);
        let label = format!("{}  {}", ICON_TIME, self.formated_now);
        div()
            .id("time")
            .h(px(26.))
            .px(px(10.))
            .py(px(3.))
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(12.))
            .bg(bg)
            .border_1()
            .border_color(bg)
            .text_color(rgba_alpha(0xFFFFFF, 1.0))
            .text_size(px(14.))
            .child(label)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _ev, _w, cx| {
                    this.show_seconds = !this.show_seconds;
                    cx.notify();
                }),
            )
    }

    fn render_layout_button(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let open = self.layout_selector_open;
        let pill_color = if open { 0x3CB371 } else { 0xD35400 };
        let bg = rgba_alpha(pill_color, 0.85);
        let border = rgba_alpha(pill_color, 1.0);
        div()
            .id("layout-toggle")
            .h(px(26.))
            .px(px(10.))
            .py(px(3.))
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(12.))
            .bg(bg)
            .border_1()
            .border_color(border)
            .text_color(rgba_alpha(0xFFFFFF, 1.0))
            .text_size(px(14.))
            .child(self.layout_symbol.clone())
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _ev, _w, cx| {
                    this.layout_selector_open = !this.layout_selector_open;
                    cx.notify();
                }),
            )
    }

    fn render_layout_options(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let opts: [(&str, u32); 3] = [("[]=", 0), ("><>", 1), ("[M]", 2)];
        let current = self.layout_symbol.clone();

        let mut row = div().flex().flex_row().gap(px(6.));
        for (sym, idx) in opts {
            let is_current = sym == current.as_str();
            let base = if is_current { 0x3CB371 } else { 0x4169E1 };
            let bg = rgba_alpha(base, 0.85);
            let border = rgba_alpha(base, 1.0);
            let item = div()
                .id(SharedString::from(format!("layout-opt-{}", idx)))
                .h(px(26.))
                .px(px(10.))
                .py(px(3.))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(12.))
                .bg(bg)
                .border_1()
                .border_color(border)
                .text_color(rgba_alpha(0xFFFFFF, 1.0))
                .text_size(px(14.))
                .child(sym)
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _ev, _w, cx| {
                        this.send_layout_command(idx);
                        this.layout_selector_open = false;
                        cx.notify();
                    }),
                );
            row = row.child(item);
        }
        row
    }

    fn render_monitor_pill(&self) -> impl IntoElement {
        let bg = rgba_alpha(0x9B59B6, 0.9);
        self.render_pill(
            "monitor",
            bg,
            bg,
            rgba_alpha(0xFFFFFF, 1.0),
            format!("{}  {}", ICON_MON, monitor_num_to_icon(self.monitor_num)),
        )
    }

    fn render_scale_pill(&self) -> impl IntoElement {
        let bg = rgba_alpha(0x787878, 0.88);
        self.render_pill(
            "scale",
            bg,
            bg,
            rgba_alpha(0xFFFFFF, 1.0),
            "s: 1.00".to_string(),
        )
    }
}

impl Render for GpuiBar {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let snapshot = self.system_monitor.get_snapshot();
        let cpu = snapshot.map(|s| s.cpu_average).unwrap_or(0.0);
        let mem = snapshot.map(|s| s.memory_usage_percent).unwrap_or(0.0);

        // Workspace row
        let mut tags = div().flex().flex_row().gap(px(4.));
        for i in 0..9 {
            tags = tags.child(self.render_tag(i, cx));
        }

        // Layout selector area
        let layout_btn = self.render_layout_button(cx);
        let layout_options_el = if self.layout_selector_open {
            Some(self.render_layout_options(cx))
        } else {
            None
        };

        // Right side pills
        let right_pills = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(6.))
            .child(self.render_usage_pill("cpu", ICON_CPU, cpu))
            .child(self.render_usage_pill("mem", ICON_MEM, mem))
            .child(self.render_battery_pill())
            .child(self.render_brightness_pill(cx))
            .child(self.render_volume_pill(cx))
            .child(self.render_screenshot_pill(cx))
            .child(self.render_time_pill(cx))
            .child(self.render_monitor_pill())
            .child(self.render_scale_pill());

        // Left cluster: tags + spacing + layout toggle (+ optional options)
        let mut left = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(6.))
            .child(tags)
            .child(layout_btn);
        if let Some(opts) = layout_options_el {
            left = left.child(opts);
        }

        div()
            .w_full()
            .h_full()
            .p(px(4.))
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .font_family(NERD_FONT)
            .text_color(rgba_alpha(0xFFFFFF, 1.0))
            .bg(rgba(0x00000000))
            .child(left)
            .child(right_pills)
    }
}

// -------- main ---------------------------------------------------------------

fn main() {
    let args: Vec<String> = env::args().collect();
    let shared_path = args.iter().skip(1).last().cloned().unwrap_or_default();
    let _ = initialize_logging("gpui_bar", &shared_path);

    application().run(|cx: &mut App| {
        let width: Pixels = px(800.);
        let height: Pixels = px(40.);
        let bounds = Bounds::centered(None, size(width, height), cx);

        let opts = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            titlebar: None,
            window_background: WindowBackgroundAppearance::Transparent,
            window_decorations: Some(WindowDecorations::Client),
            kind: WindowKind::PopUp,
            is_resizable: false,
            is_minimizable: false,
            window_min_size: Some(size(width, height)),
            app_id: Some("dev.gpui.bar".into()),
            ..Default::default()
        };

        cx.open_window(opts, |_w, cx| cx.new(|cx| GpuiBar::new(cx)))
            .expect("failed to open window");
        cx.activate(true);
    });
}
