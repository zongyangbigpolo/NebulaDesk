//! Native session controls, composited over the video on its existing GPU surface.

use std::time::{Duration, Instant};

use egui::{Color32, FontDefinitions, Pos2, Rect, RichText, Vec2};
use nebula_common::SessionPolicy;
use nebula_desktop_protocol::{ConnectionPath, Event, SessionState, TransferState};
use winit::{event::WindowEvent, window::Window};

use crate::input::Viewport;

pub const TOOLBAR_HEIGHT: f64 = 48.0;
pub const FOOTER_HEIGHT: f64 = 40.0;
const INK: Color32 = Color32::from_rgb(53, 68, 83);
const MUTED: Color32 = Color32::from_rgb(91, 113, 132);
const GREEN: Color32 = Color32::from_rgb(60, 118, 93);

fn status_colors(state: SessionState) -> (Color32, Color32) {
    match state {
        SessionState::Connected => (GREEN, Color32::from_rgb(228, 235, 231)),
        SessionState::Connecting => (
            Color32::from_rgb(133, 103, 51),
            Color32::from_rgb(245, 241, 232),
        ),
        SessionState::Failed => (
            Color32::from_rgb(146, 77, 72),
            Color32::from_rgb(248, 232, 230),
        ),
        SessionState::Disconnected => (MUTED, Color32::from_rgb(226, 232, 237)),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Audio(bool),
    Clipboard(bool),
    PickFiles,
    Fullscreen,
    Disconnect,
}

pub struct Chrome {
    pub context: egui::Context,
    state: Option<egui_winit::State>,
    resource_name: String,
    policy: SessionPolicy,
    audio: bool,
    clipboard: bool,
    panel_open: bool,
    physical_size: (u32, u32),
    connection: SessionState,
    path: Option<ConnectionPath>,
    rtt_ms: Option<f64>,
    resolution: (u32, u32),
    connected_at: Option<Instant>,
    elapsed: Duration,
    error: Option<String>,
    notice: Option<String>,
    transfer: Option<(String, u64, u64, TransferState, Option<String>)>,
    cjk: bool,
}

impl Chrome {
    pub fn new(resource_name: String, policy: SessionPolicy) -> Self {
        let context = egui::Context::default();
        context.set_theme(egui::Theme::Light);
        context.options_mut(|options| options.zoom_with_keyboard = false);
        let (fonts, cjk) = native_fonts();
        context.set_fonts(fonts);
        let mut style = (*context.style()).clone();
        style.visuals = egui::Visuals::light();
        style.visuals.override_text_color = Some(INK);
        style.visuals.selection.bg_fill = Color32::from_rgb(208, 221, 227);
        style.spacing.button_padding = Vec2::new(8.0, 5.0);
        style.spacing.item_spacing = Vec2::new(8.0, 10.0);
        style
            .text_styles
            .insert(egui::TextStyle::Body, egui::FontId::proportional(13.0));
        style
            .text_styles
            .insert(egui::TextStyle::Button, egui::FontId::proportional(12.0));
        context.set_style(style);
        Self {
            context,
            state: None,
            resource_name,
            audio: false,
            clipboard: false,
            policy,
            panel_open: true,
            physical_size: (0, 0),
            connection: SessionState::Connecting,
            path: None,
            rtt_ms: None,
            resolution: (0, 0),
            connected_at: None,
            elapsed: Duration::ZERO,
            error: None,
            notice: (!cjk).then(|| "CJK system font unavailable; controls use English.".into()),
            transfer: None,
            cjk,
        }
    }

    pub fn event(&mut self, event: &Event) {
        match event {
            Event::State { state, path, error } => {
                if self.path != *path || *state != SessionState::Connected {
                    self.rtt_ms = None;
                }
                if *state == SessionState::Connected {
                    if self.connected_at.is_none() {
                        self.connected_at = Some(Instant::now());
                    }
                } else if let Some(start) = self.connected_at.take() {
                    self.elapsed += start.elapsed();
                }
                self.connection = *state;
                self.path = *path;
                self.error.clone_from(error);
            }
            Event::Metrics {
                rtt_ms,
                width,
                height,
            } => {
                self.rtt_ms = rtt_ms.filter(|n| n.is_finite() && *n >= 0.0);
                self.resolution = (*width, *height);
            }
            Event::Transfer {
                name,
                transferred,
                total,
                state,
                error,
                ..
            } => {
                self.transfer = Some((name.clone(), *transferred, *total, *state, error.clone()));
            }
        }
        self.context.request_repaint();
    }

    pub fn set_audio(&mut self, enabled: bool) {
        self.audio = enabled && self.policy.audio;
        self.context.request_repaint();
    }

    pub fn set_clipboard(&mut self, enabled: bool) {
        self.clipboard = enabled && self.policy.clipboard;
        self.context.request_repaint();
    }

    /// Reflect capabilities actually opened by the session, not requested ones.
    pub fn settings(&mut self, audio: bool, clipboard: bool) {
        self.set_audio(audio);
        self.set_clipboard(clipboard);
    }

    pub fn notice(&mut self, message: String) {
        self.notice = Some(message);
        self.panel_open = true;
        self.context.request_repaint();
    }

    fn state(&mut self, window: &Window) -> &mut egui_winit::State {
        self.physical_size = window.inner_size().into();
        self.state.get_or_insert_with(|| {
            egui_winit::State::new(
                self.context.clone(),
                egui::ViewportId::ROOT,
                window,
                Some(window.scale_factor() as f32),
                window.theme(),
                None,
            )
        })
    }

    pub fn on_window_event(&mut self, window: &Window, event: &WindowEvent) -> bool {
        let response = self.state(window).on_window_event(window, event);
        if response.repaint {
            window.request_redraw();
        }
        response.consumed
    }

    pub fn wants_keyboard_input(&self) -> bool {
        self.context.wants_keyboard_input()
    }

    /// Both rendering and remote input use this physical-pixel viewport.
    pub fn viewport(&self, size: (u32, u32), scale: f64, picture: (u32, u32)) -> Viewport {
        video_viewport(size, scale, picture)
    }

    /// Geometry, not last-frame egui capture, decides whether input is remote.
    pub fn blocks_pointer(&self, x: f64, y: f64, scale: f64) -> bool {
        let scale = valid_scale(scale);
        let size = Vec2::new(
            self.physical_size.0 as f32 / scale as f32,
            self.physical_size.1 as f32 / scale as f32,
        );
        let point = Pos2::new((x / scale) as f32, (y / scale) as f32);
        !x.is_finite()
            || !y.is_finite()
            || x < 0.0
            || y < 0.0
            || x >= f64::from(self.physical_size.0)
            || y >= f64::from(self.physical_size.1)
            || point.y <= TOOLBAR_HEIGHT as f32
            || point.y >= size.y - FOOTER_HEIGHT as f32
            || (self.panel_open && panel_rect(size).contains(point))
    }

    pub(crate) fn frame(&mut self, window: &Window) -> (egui::FullOutput, Vec<Action>) {
        let input = self.state(window).take_egui_input(window);
        let context = self.context.clone();
        let mut actions = Vec::new();
        let output = context.run(input, |ctx| self.ui(ctx, &mut actions));
        self.state(window)
            .handle_platform_output(window, output.platform_output.clone());
        (output, actions)
    }

    fn text<'a>(&self, chinese: &'a str, english: &'a str) -> &'a str {
        if self.cjk {
            chinese
        } else {
            english
        }
    }

    fn path_label(&self) -> &str {
        match (self.connection, self.path) {
            (SessionState::Connected, Some(ConnectionPath::Direct)) => self.text("直连", "Direct"),
            (SessionState::Connected, Some(ConnectionPath::Relay)) => self.text("中继", "Relay"),
            (SessionState::Connected, None) => self.text("已连接", "Connected"),
            (SessionState::Connecting, _) => self.text("连接中", "Connecting"),
            (SessionState::Disconnected, _) => self.text("已断开", "Disconnected"),
            (SessionState::Failed, _) => self.text("连接失败", "Failed"),
        }
    }

    fn ui(&mut self, ctx: &egui::Context, actions: &mut Vec<Action>) {
        ctx.request_repaint_after(Duration::from_secs(1));
        egui::TopBottomPanel::top("session-toolbar")
            .exact_height(TOOLBAR_HEIGHT as f32)
            .frame(
                egui::Frame::new()
                    .fill(Color32::from_rgb(242, 244, 246))
                    .inner_margin(toolbar_margin()),
            )
            .show(ctx, |ui| {
                ui.horizontal_centered(|ui| {
                    let controls = 6.0 * 32.0 + 6.0 * 8.0 + 64.0;
                    let title_width = (ui.available_width() - controls).max(0.0);
                    ui.allocate_ui_with_layout(
                        Vec2::new(title_width, 32.0),
                        egui::Layout::left_to_right(egui::Align::Center),
                        |ui| {
                            ui.set_clip_rect(ui.max_rect());
                            let badge = match self.rtt_ms {
                                Some(rtt) => format!("{} · {rtt:.0} ms", self.path_label()),
                                None => format!("{} · — ms", self.path_label()),
                            };
                            let name_width = (title_width - 160.0).clamp(30.0, 240.0);
                            ui.add_sized(
                                [name_width, 28.0],
                                egui::Label::new(RichText::new(&self.resource_name).strong())
                                    .truncate(),
                            )
                            .on_hover_text(&self.resource_name);
                            if title_width > 360.0 {
                                ui.label(RichText::new("/ NebulaDesk").size(11.0).color(MUTED));
                            }
                            if title_width > 180.0 {
                                let (text, background) = status_colors(self.connection);
                                egui::Frame::new()
                                    .fill(background)
                                    .corner_radius(12)
                                    .inner_margin(egui::Margin::symmetric(9, 4))
                                    .show(ui, |ui| {
                                        ui.label(RichText::new(badge).size(11.0).color(text));
                                    });
                            }
                        },
                    );
                    self.toolbar_controls(ui, actions);
                });
            });
        egui::TopBottomPanel::bottom("session-footer")
            .exact_height(FOOTER_HEIGHT as f32)
            .frame(
                egui::Frame::new()
                    .fill(Color32::from_rgb(23, 32, 43))
                    .inner_margin(egui::Margin::symmetric(16, 9)),
            )
            .show(ctx, |ui| {
                ui.visuals_mut().override_text_color = Some(Color32::from_rgb(172, 187, 199));
                ui.horizontal(|ui| {
                    let elapsed = self.elapsed
                        + self
                            .connected_at
                            .map_or(Duration::ZERO, |start| start.elapsed());
                    ui.label(
                        RichText::new(format!(
                            "{} · {}",
                            self.path_label(),
                            duration_label(elapsed)
                        ))
                        .size(11.0),
                    );
                    ui.label(
                        RichText::new(if self.policy.input {
                            self.text("键鼠可用", "Keyboard & mouse")
                        } else {
                            self.text("仅查看", "View only")
                        })
                        .size(11.0),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.available_width() > 170.0 {
                            ui.label(
                                RichText::new(if self.policy.file_transfer {
                                    self.text("文件可直接拖入窗口", "Drop files into this window")
                                } else {
                                    self.text("文件传输已禁用", "File transfer disabled")
                                })
                                .size(11.0),
                            );
                        }
                    });
                });
            });
        if self.panel_open {
            let rect = panel_rect(ctx.screen_rect().size());
            egui::Area::new("connection-panel".into())
                .order(egui::Order::Foreground)
                .fixed_pos(rect.min)
                .movable(false)
                .show(ctx, |ui| {
                    ui.set_min_size(rect.size());
                    ui.set_max_size(rect.size());
                    ui.set_clip_rect(rect);
                    egui::Frame::new()
                        .fill(Color32::from_rgb(251, 252, 253))
                        .stroke(egui::Stroke::new(1.0_f32, Color32::from_rgb(223, 230, 236)))
                        .corner_radius(12)
                        .inner_margin(20)
                        .show(ui, |ui| {
                            ui.set_width((rect.width() - 40.0).max(0.0));
                            ui.set_min_height((rect.height() - 40.0).max(0.0));
                            egui::ScrollArea::vertical()
                                .max_height((rect.height() - 40.0).max(0.0))
                                .show(ui, |ui| self.panel(ui, actions));
                        });
                });
        }
    }

    fn toolbar_controls(&mut self, ui: &mut egui::Ui, actions: &mut Vec<Action>) {
        if icon_button(
            ui,
            Icon::Panel,
            self.panel_open,
            true,
            self.text("连接状态与设置", "Connection & settings"),
        )
        .clicked()
        {
            self.panel_open = !self.panel_open;
        }
        if icon_button(
            ui,
            Icon::Audio,
            self.audio,
            self.policy.audio,
            if self.audio {
                self.text("静音", "Mute audio")
            } else {
                self.text("开启声音", "Enable audio")
            },
        )
        .clicked()
        {
            actions.push(Action::Audio(!self.audio));
        }
        if icon_button(
            ui,
            Icon::Clipboard,
            self.clipboard,
            self.policy.clipboard,
            self.text("剪贴板同步", "Clipboard sync"),
        )
        .clicked()
        {
            actions.push(Action::Clipboard(!self.clipboard));
        }
        if icon_button(
            ui,
            Icon::File,
            false,
            self.policy.file_transfer,
            self.text("发送文件", "Send files"),
        )
        .clicked()
        {
            actions.push(Action::PickFiles);
        }
        if icon_button(
            ui,
            Icon::Fullscreen,
            false,
            true,
            self.text("切换全屏", "Toggle fullscreen"),
        )
        .clicked()
        {
            actions.push(Action::Fullscreen);
        }
        ui.separator();
        if ui
            .add(
                egui::Button::new(
                    RichText::new(self.text("断开", "Disconnect"))
                        .color(Color32::from_rgb(146, 77, 72)),
                )
                .min_size(Vec2::new(64.0, 30.0)),
            )
            .clicked()
        {
            actions.push(Action::Disconnect);
        }
    }

    fn panel(&mut self, ui: &mut egui::Ui, actions: &mut Vec<Action>) {
        ui.horizontal(|ui| {
            ui.label(
                RichText::new(self.text("连接状态", "Connection"))
                    .strong()
                    .size(15.0),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .button("×")
                    .on_hover_text(self.text("收起面板", "Hide panel"))
                    .clicked()
                {
                    self.panel_open = false;
                }
            });
        });
        ui.add_space(6.0);
        ui.label(RichText::new(self.path_label()).color(status_colors(self.connection).0));
        if let Some(error) = &self.error {
            ui.label(RichText::new(error).color(Color32::from_rgb(146, 77, 72)));
        }
        if let Some(notice) = &self.notice {
            ui.label(notice);
            if ui
                .small_button(self.text("关闭提示", "Dismiss notice"))
                .clicked()
            {
                self.notice = None;
            }
        }
        ui.separator();
        row(
            ui,
            self.text("往返延迟", "Round-trip latency"),
            &self
                .rtt_ms
                .map_or_else(|| "—".into(), |rtt| format!("{rtt:.0} ms")),
        );
        row(
            ui,
            self.text("分辨率", "Resolution"),
            &if self.resolution.0 == 0 {
                "—".into()
            } else {
                format!("{} × {}", self.resolution.0, self.resolution.1)
            },
        );
        row(
            ui,
            self.text("画面缩放", "Picture scaling"),
            self.text("适应窗口", "Fit window"),
        );
        ui.separator();
        let audio_label = self.text("系统声音", "System audio");
        let mut audio = self.audio;
        if ui
            .add_enabled(
                self.policy.audio,
                egui::Checkbox::new(&mut audio, audio_label),
            )
            .changed()
        {
            actions.push(Action::Audio(audio));
        }
        let clipboard_label = self.text("剪贴板同步", "Clipboard sync");
        let mut clipboard = self.clipboard;
        if ui
            .add_enabled(
                self.policy.clipboard,
                egui::Checkbox::new(&mut clipboard, clipboard_label),
            )
            .changed()
        {
            actions.push(Action::Clipboard(clipboard));
        }
        ui.separator();
        if ui
            .add_enabled(
                self.policy.file_transfer,
                egui::Button::new(self.text("发送文件…", "Send files…")),
            )
            .clicked()
        {
            actions.push(Action::PickFiles);
        }
        if let Some((name, sent, total, state, error)) = &self.transfer {
            ui.add(egui::Label::new(name).truncate())
                .on_hover_text(name);
            let label = match state {
                TransferState::Offered => self.text("等待接收", "Waiting"),
                TransferState::Transferring => self.text("传输中", "Transferring"),
                TransferState::Complete => self.text("已完成", "Complete"),
                TransferState::Failed => self.text("传输失败", "Failed"),
            };
            let progress = if *state == TransferState::Complete {
                1.0
            } else if *total == 0 {
                0.0
            } else {
                (*sent as f64 / *total as f64).clamp(0.0, 1.0) as f32
            };
            ui.add(egui::ProgressBar::new(progress).text(format!("{label} · {sent} / {total} B")));
            if let Some(error) = error {
                ui.label(RichText::new(error).color(Color32::from_rgb(146, 77, 72)));
            }
        }
        ui.label(
            RichText::new(self.text(
                "设置只影响当前连接",
                "Settings apply to this connection only",
            ))
            .size(10.0)
            .color(MUTED),
        );
    }
}

fn valid_scale(scale: f64) -> f64 {
    if scale.is_finite() && scale > 0.0 {
        scale
    } else {
        1.0
    }
}

fn toolbar_margin() -> egui::Margin {
    egui::Margin {
        left: if cfg!(target_os = "macos") { 96 } else { 12 },
        right: 12,
        top: 8,
        bottom: 8,
    }
}

pub(crate) fn video_viewport(size: (u32, u32), scale: f64, picture: (u32, u32)) -> Viewport {
    let top = (TOOLBAR_HEIGHT * valid_scale(scale)).min(f64::from(size.1));
    let height = (f64::from(size.1) - top - FOOTER_HEIGHT * valid_scale(scale)).max(0.0);
    if size.0 == 0 || height == 0.0 {
        return Viewport {
            x: 0.0,
            y: top,
            width: 0.0,
            height: 0.0,
        };
    }
    let mut viewport = Viewport::fit((f64::from(size.0), height), picture);
    viewport.y += top;
    viewport
}

fn panel_rect(size: Vec2) -> Rect {
    let width = 337.0_f32.min((size.x - 24.0).max(0.0));
    let height =
        437.0_f32.min((size.y - TOOLBAR_HEIGHT as f32 - FOOTER_HEIGHT as f32 - 32.0).max(0.0));
    Rect::from_min_size(
        Pos2::new(
            (size.x - width - 16.0).max(0.0),
            TOOLBAR_HEIGHT as f32 + 16.0,
        ),
        Vec2::new(width, height),
    )
}

fn duration_label(elapsed: Duration) -> String {
    let seconds = elapsed.as_secs();
    if seconds >= 3600 {
        format!(
            "{}:{:02}:{:02}",
            seconds / 3600,
            seconds / 60 % 60,
            seconds % 60
        )
    } else {
        format!("{:02}:{:02}", seconds / 60, seconds % 60)
    }
}

fn row(ui: &mut egui::Ui, label: &str, value: &str) {
    ui.horizontal(|ui| {
        ui.label(RichText::new(label).size(12.0).color(MUTED));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.label(value);
        });
    });
    ui.add_space(7.0);
}

#[derive(Clone, Copy)]
enum Icon {
    Panel,
    Audio,
    Clipboard,
    File,
    Fullscreen,
}

fn icon_button(
    ui: &mut egui::Ui,
    icon: Icon,
    selected: bool,
    enabled: bool,
    label: &str,
) -> egui::Response {
    let response = ui
        .add_enabled(
            enabled,
            egui::Button::new("")
                .min_size(Vec2::splat(32.0))
                .selected(selected),
        )
        .on_hover_text(label);
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, enabled, label));
    let painter = ui.painter_at(response.rect);
    let center = response.rect.center();
    let color = if enabled {
        MUTED
    } else {
        Color32::from_rgb(156, 167, 176)
    };
    let stroke = egui::Stroke::new(1.4_f32, color);
    let p = |x, y| center + Vec2::new(x, y);
    let line = |points: &[(f32, f32)]| {
        painter.add(egui::Shape::line(
            points.iter().map(|&(x, y)| p(x, y)).collect(),
            stroke,
        ));
    };
    match icon {
        Icon::Panel => {
            line(&[
                (-8.0, -6.0),
                (8.0, -6.0),
                (8.0, 5.0),
                (-8.0, 5.0),
                (-8.0, -6.0),
            ]);
            line(&[(-4.0, 8.0), (4.0, 8.0)]);
        }
        Icon::Audio => {
            line(&[
                (-8.0, -3.0),
                (-4.0, -3.0),
                (1.0, -7.0),
                (1.0, 7.0),
                (-4.0, 3.0),
                (-8.0, 3.0),
                (-8.0, -3.0),
            ]);
            if selected {
                line(&[(5.0, -5.0), (8.0, 0.0), (5.0, 5.0)]);
            } else {
                line(&[(4.0, -3.0), (9.0, 3.0)]);
                line(&[(9.0, -3.0), (4.0, 3.0)]);
            }
        }
        Icon::Clipboard => {
            line(&[
                (-3.0, -7.0),
                (-7.0, -7.0),
                (-7.0, 8.0),
                (7.0, 8.0),
                (7.0, -7.0),
                (3.0, -7.0),
            ]);
            line(&[
                (-3.0, -9.0),
                (3.0, -9.0),
                (3.0, -5.0),
                (-3.0, -5.0),
                (-3.0, -9.0),
            ]);
        }
        Icon::File => {
            line(&[(-7.0, 1.0), (-7.0, 8.0), (7.0, 8.0), (7.0, 1.0)]);
            line(&[(0.0, 4.0), (0.0, -8.0)]);
            line(&[(-4.0, -4.0), (0.0, -8.0), (4.0, -4.0)]);
        }
        Icon::Fullscreen => {
            for (x, y) in [(-1.0, -1.0), (1.0, -1.0), (-1.0, 1.0), (1.0, 1.0)] {
                line(&[(x * 3.0, y * 8.0), (x * 8.0, y * 8.0), (x * 8.0, y * 3.0)]);
            }
        }
    }
    response
}

/// Fonts remain on the user's OS; no proprietary font is redistributed.
fn native_fonts() -> (FontDefinitions, bool) {
    static FONTS: std::sync::OnceLock<(FontDefinitions, bool)> = std::sync::OnceLock::new();
    FONTS.get_or_init(load_native_fonts).clone()
}

fn load_native_fonts() -> (FontDefinitions, bool) {
    let mut fonts = FontDefinitions::default();
    let candidates: &[&str] = if cfg!(target_os = "macos") {
        &[
            "/System/Library/Fonts/PingFang.ttc",
            "/System/Library/Fonts/STHeiti Light.ttc",
            "/System/Library/Fonts/Supplemental/Arial Unicode.ttf",
            "/System/Library/Fonts/Supplemental/Songti.ttc",
        ]
    } else if cfg!(target_os = "windows") {
        &[
            "C:\\Windows\\Fonts\\msyh.ttc",
            "C:\\Windows\\Fonts\\msyh.ttf",
            "C:\\Windows\\Fonts\\simsun.ttc",
        ]
    } else {
        &[
            "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
            "/usr/share/fonts/truetype/noto/NotoSansCJK-Regular.ttc",
            "/usr/share/fonts/google-noto-cjk/NotoSansCJK-Regular.ttc",
            "/usr/share/fonts/truetype/wqy/wqy-microhei.ttc",
        ]
    };
    for path in candidates {
        let Ok(bytes) = std::fs::read(path) else {
            continue;
        };
        if !matches!(bytes.get(..4), Some(b"ttcf" | b"OTTO" | b"\0\x01\0\0")) {
            continue;
        }
        let mut data = egui::FontData::from_owned(bytes);
        // FontData's collection index is required for TTCs such as PingFang.
        data.index = 0;
        fonts.font_data.insert("native-cjk".into(), data.into());
        fonts
            .families
            .entry(egui::FontFamily::Proportional)
            .or_default()
            .push("native-cjk".into());
        return (fonts, true);
    }
    tracing::warn!("CJK system font unavailable; native session controls use English");
    (fonts, false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_connected_status_uses_the_ready_palette() {
        assert_eq!(status_colors(SessionState::Connected).0, GREEN);
        for state in [
            SessionState::Connecting,
            SessionState::Failed,
            SessionState::Disconnected,
        ] {
            assert_ne!(status_colors(state).0, GREEN);
            assert_ne!(
                status_colors(state).1,
                status_colors(SessionState::Connected).1
            );
        }
        assert_eq!(
            status_colors(SessionState::Failed).0,
            Color32::from_rgb(146, 77, 72)
        );
        assert_eq!(status_colors(SessionState::Disconnected).0, MUTED);
    }

    #[test]
    fn viewport_and_pointer_guards_follow_scale_and_panel_visibility() {
        let mut chrome = Chrome::new("Desktop".into(), SessionPolicy::full());
        chrome.physical_size = (2560, 1640);
        let viewport = chrome.viewport((2560, 1640), 2.0, (1280, 732));
        assert_eq!(
            viewport,
            Viewport {
                x: 0.0,
                y: 96.0,
                width: 2560.0,
                height: 1464.0
            }
        );
        assert!(chrome.blocks_pointer(100.0, 95.0, 2.0));
        assert!(chrome.blocks_pointer(100.0, 1560.0, 2.0));
        assert!(chrome.blocks_pointer(2500.0, 200.0, 2.0));
        assert!(!chrome.blocks_pointer(500.0, 800.0, 2.0));
        chrome.panel_open = false;
        assert!(!chrome.blocks_pointer(2500.0, 200.0, 2.0));
        assert_eq!(chrome.viewport((800, 50), 2.0, (1920, 1080)).height, 0.0);
    }

    #[test]
    fn native_toolbar_reserves_traffic_lights_without_changing_video_geometry() {
        let margin = toolbar_margin();
        assert_eq!(margin.left, if cfg!(target_os = "macos") { 96 } else { 12 });
        assert_eq!(margin.right, 12);
        assert_eq!(i16::from(margin.top) + i16::from(margin.bottom) + 32, 48);
        let chrome = Chrome::new("Desktop".into(), SessionPolicy::full());
        let viewport = chrome.viewport((1280, 820), 1.0, (1280, 732));
        assert_eq!(viewport.x, 0.0);
        assert_eq!(viewport.y, 48.0);
        assert_eq!(viewport.height, 732.0);
    }

    #[test]
    fn effective_settings_respect_policy_and_notices_reopen_panel() {
        let mut chrome = Chrome::new("Desktop".into(), SessionPolicy::full());
        assert!(!chrome.audio && !chrome.clipboard);
        chrome.settings(true, true);
        assert!(chrome.audio && chrome.clipboard);
        chrome.settings(false, false);
        assert!(!chrome.audio && !chrome.clipboard);
        chrome.panel_open = false;
        chrome.notice("Audio device unavailable".into());
        assert!(chrome.panel_open);
        assert_eq!(chrome.notice.as_deref(), Some("Audio device unavailable"));
        let mut restricted = Chrome::new("Desktop".into(), SessionPolicy::view_only());
        restricted.settings(true, true);
        assert!(!restricted.audio && !restricted.clipboard);
    }

    #[test]
    fn metrics_and_elapsed_are_real_not_demo_values() {
        let mut chrome = Chrome::new("Desktop".into(), SessionPolicy::view_only());
        assert!(chrome.rtt_ms.is_none());
        assert!(chrome.connected_at.is_none());
        chrome.set_audio(true);
        chrome.set_clipboard(true);
        assert!(!chrome.audio && !chrome.clipboard);
        chrome.event(&Event::State {
            state: SessionState::Connected,
            path: Some(ConnectionPath::Relay),
            error: None,
        });
        let start = chrome.connected_at;
        chrome.event(&Event::State {
            state: SessionState::Connected,
            path: Some(ConnectionPath::Direct),
            error: None,
        });
        assert_eq!(chrome.connected_at, start);
        chrome.event(&Event::Metrics {
            rtt_ms: Some(f64::NAN),
            width: 1920,
            height: 1080,
        });
        assert_eq!(chrome.resolution, (1920, 1080));
        assert!(chrome.rtt_ms.is_none());
        chrome.event(&Event::Metrics {
            rtt_ms: Some(3.0),
            width: 1920,
            height: 1080,
        });
        chrome.event(&Event::State {
            state: SessionState::Connected,
            path: Some(ConnectionPath::Relay),
            error: None,
        });
        assert!(
            chrome.rtt_ms.is_none(),
            "a new path must not show the old path's RTT"
        );
        assert_eq!(duration_label(Duration::from_secs(1458)), "24:18");
        assert_eq!(duration_label(Duration::from_secs(3601)), "1:00:01");
    }

    #[test]
    fn headless_layout_emits_native_shapes_at_multiple_sizes() {
        let mut chrome = Chrome::new(
            "办公室 Mac — a long resource title".into(),
            SessionPolicy::full(),
        );
        chrome.cjk = false;
        chrome.context.set_fonts(FontDefinitions::default());
        for size in [Vec2::new(1280.0, 820.0), Vec2::new(640.0, 480.0)] {
            let context = chrome.context.clone();
            let mut actions = Vec::new();
            let output = context.run(
                egui::RawInput {
                    screen_rect: Some(Rect::from_min_size(Pos2::ZERO, size)),
                    ..Default::default()
                },
                |ctx| chrome.ui(ctx, &mut actions),
            );
            assert!(!output.shapes.is_empty());
            assert!(actions.is_empty());
            assert!(panel_rect(size).max.y <= size.y - FOOTER_HEIGHT as f32);
        }
    }

    #[test]
    fn local_tab_navigates_native_controls_without_a_remote_key() {
        let focus = crate::input::KeyboardFocus::Chrome;
        let mut chrome = Chrome::new("Desktop".into(), SessionPolicy::full());
        chrome.context.set_fonts(FontDefinitions::default());
        chrome.cjk = false;
        let context = chrome.context.clone();
        for tab in [false, true] {
            let mut actions = Vec::new();
            let mut input = egui::RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(1280.0, 820.0))),
                ..Default::default()
            };
            if tab && focus.accepts_chrome_keyboard() {
                input.events.push(egui::Event::Key {
                    key: egui::Key::Tab,
                    physical_key: Some(egui::Key::Tab),
                    pressed: true,
                    repeat: false,
                    modifiers: egui::Modifiers::NONE,
                });
            }
            let _ = context.run(input, |ctx| chrome.ui(ctx, &mut actions));
            assert!(
                actions.is_empty(),
                "Tab navigates rather than activating a control"
            );
        }
        assert!(context.memory(|memory| memory.focused()).is_some());
        assert!(focus
            .remote_key(
                winit::keyboard::PhysicalKey::Code(winit::keyboard::KeyCode::Tab),
                winit::event::ElementState::Pressed,
                None,
                ndp_proto::Modifiers::NONE,
            )
            .is_none());
    }
}
