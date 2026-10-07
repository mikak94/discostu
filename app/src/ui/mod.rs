//! The iced front end.

mod icons;
mod taskbar;
mod theme;
mod video;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use iced::font::Weight;
use iced::widget::{
    Space, button, center, column, container, image, mouse_area, opaque, pick_list, row, scrollable, shader,
    slider, stack, svg, text, text_input, toggler, tooltip,
};
use iced::{Alignment, Color, Element, Font, Length, Size, Subscription, Task, Theme, window};

use crate::audio::{DeviceSpec, Driver, device};
use crate::engine::{BrokerStatus, Engine, PeerView, Snapshot, Toggle};
use crate::net::portmap::PortMap;
use crate::protocol::{BLOCK, ChannelId, ChannelRef, PeerId, SAMPLE_RATE};
use crate::screen::{self, Source, VideoSink};
use icons::{Icon, icon};
use theme::{ACCENT, AMBER, FAINT, GOLD, GREEN, MUTED, PINK, RED, TEXT, Tone, alpha, avatar_color};

type El<'a> = Element<'a, Message>;

const BOLD: Font = Font { weight: Weight::Bold, ..Font::DEFAULT };
const SEMIBOLD: Font = Font { weight: Weight::Semibold, ..Font::DEFAULT };
const DEFAULT_DEVICE: &str = "System default";
/// Disco Stu on a lavender→pink badge: window and taskbar icon.
const APP_ICON: &[u8] = include_bytes!("../../assets/icon.png");
/// The same badge as a vector, crisp at any size (`cargo run --example make_art`).
const STU_BADGE: &[u8] = include_bytes!("../../assets/stu-badge.svg");
const NEW_CHANNEL_INPUT: &str = "new-channel";

pub fn run() -> iced::Result {
    iced::application(App::boot, App::update, App::view)
        .title(App::title)
        .subscription(App::subscription)
        .theme(App::theme)
        .window(window::Settings {
            size: Size::new(1200.0, 760.0),
            min_size: Some(Size::new(860.0, 540.0)),
            icon: window::icon::from_file_data(APP_ICON, None).ok(),
            ..Default::default()
        })
        .antialiasing(true)
        .run()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Overlay {
    None,
    Settings,
    SharePicker,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PickerTab {
    Screens,
    Windows,
}

/// Device choices for the settings panel.
#[derive(Debug, Clone, Default)]
struct DeviceLists {
    asio: Vec<String>,
    wasapi_in: Vec<String>,
    wasapi_out: Vec<String>,
}

/// A labelled `Option<u32>` for pick lists (buffer sizes, channels).
#[derive(Debug, Clone, PartialEq, Eq)]
struct Choice {
    value: Option<u32>,
    label: String,
}

impl std::fmt::Display for Choice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.label)
    }
}

fn source_key(s: &Source) -> String {
    format!("{:?}", s.kind)
}

struct App {
    engine: Option<Arc<Engine>>,
    fatal: Option<String>,
    snap: Snapshot,
    sink: Option<Arc<VideoSink>>,
    overlay: Overlay,
    expanded: Option<PeerId>,
    sources: Vec<Source>,
    thumbs: HashMap<String, image::Handle>,
    picker_tab: PickerTab,
    devices: DeviceLists,
    name_draft: String,
    connect_draft: String,
    /// Friends group code being typed or pasted.
    group_draft: String,
    broker_draft: String,
    toast: Option<(String, Instant)>,
    fullscreen: bool,
    /// Last mouse movement: fullscreen controls show for a moment after it.
    pointer_moved: Instant,
    logo: svg::Handle,
    /// Name being typed for a new channel.
    new_channel: Option<String>,
    /// Own channel whose delete button was clicked once (click again to confirm).
    armed_delete: Option<ChannelId>,
}

#[derive(Debug, Clone)]
enum Message {
    Tick,
    Frame,
    ToggleMute,
    ToggleDeafen,
    ShareClicked,
    Sources(Vec<Source>),
    Thumbs(Vec<(String, u32, u32, Vec<u8>)>),
    PickerTab(PickerTab),
    ShareSource(Source),
    ShareResult(Result<Option<String>, String>),
    ShareAudio(bool),
    OpenSettings,
    Devices(DeviceLists),
    CloseOverlay,
    Watch(PeerId),
    WatchResult(Result<(), String>),
    Unwatch,
    Expand(PeerId),
    Volume(PeerId, f32),
    StreamVolume(f32),
    JoinChannel(Option<ChannelRef>),
    NewChannel,
    NewChannelDraft(String),
    NewChannelSubmit,
    DeleteChannel(ChannelId),
    NameChanged(String),
    NameSubmit,
    ConnectChanged(String),
    ConnectSubmit,
    GroupDraft(String),
    GroupJoin,
    GroupNew,
    GroupLeave,
    CopyGroupCode,
    BrokerDraft(String),
    BrokerSubmit,
    ForgetBrokerKey,
    Driver(Driver),
    InputDevice(String),
    OutputDevice(String),
    AsioDriver(String),
    AsioBuffer(Choice),
    MicChannel(Choice),
    Exclusive(bool),
    Toggle(Toggle, bool),
    ResetProfile,
    SaveDiagnostics,
    ToggleFullscreen,
    PointerMoved,
    Escape,
    /// Native handle of our window, once it exists.
    WindowHandle(u64),
}

/// Runs blocking backend work off the UI thread.
fn blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> impl Future<Output = T> {
    let (tx, rx) = futures::channel::oneshot::channel();
    std::thread::spawn(move || {
        let _ = tx.send(f());
    });
    async move { rx.await.expect("worker thread finished") }
}

fn load_devices() -> DeviceLists {
    let (wasapi_in, wasapi_out) = device::list(Driver::Wasapi);
    let asio = if device::asio_available() { device::list(Driver::Asio).0 } else { Vec::new() };
    DeviceLists { asio, wasapi_in, wasapi_out }
}

impl App {
    fn boot() -> (Self, Task<Message>) {
        let (engine, fatal) = match Engine::start() {
            Ok(e) => (Some(e), None),
            Err(e) => (None, Some(e)),
        };
        let snap = engine.as_ref().map(|e| e.snapshot()).unwrap_or_default();
        let name_draft = snap.me.name.clone();
        (
            Self {
                engine,
                fatal,
                snap,
                sink: None,
                overlay: Overlay::None,
                expanded: None,
                sources: Vec::new(),
                thumbs: HashMap::new(),
                picker_tab: PickerTab::Screens,
                devices: DeviceLists::default(),
                name_draft,
                group_draft: String::new(),
                broker_draft: String::new(),
                connect_draft: String::new(),
                toast: None,
                fullscreen: false,
                pointer_moved: Instant::now(),
                logo: svg::Handle::from_memory(STU_BADGE),
                new_channel: None,
                armed_delete: None,
            },
            window::latest().and_then(window::raw_id::<Message>).map(Message::WindowHandle),
        )
    }

    fn title(&self) -> String {
        match self.current_channel() {
            Some(c) => format!("discostu · {}", c.name),
            None => "discostu".into(),
        }
    }

    fn current_channel(&self) -> Option<&crate::engine::ChannelView> {
        let at = self.snap.channel?;
        self.snap.channels.iter().find(|c| c.at == at)
    }

    /// Peers in `at` (`None` = lobby). Peers in a channel we can't see (its
    /// host isn't connected to us) show in the lobby.
    fn members(&self, at: Option<ChannelRef>) -> impl Iterator<Item = &PeerView> {
        self.snap.peers.iter().filter(move |p| {
            let theirs = p.channel.filter(|c| self.snap.channels.iter().any(|v| v.at == *c));
            theirs == at
        })
    }

    fn theme(&self) -> Theme {
        theme::theme()
    }

    fn subscription(&self) -> Subscription<Message> {
        let tick = iced::time::every(Duration::from_millis(50)).map(|_| Message::Tick);
        if self.sink.is_some() {
            // Redraw every display frame while a stream is up; the video
            // primitive uploads whatever arrived since the last one.
            let escape = iced::event::listen_with(|event, _, _| match event {
                iced::Event::Keyboard(iced::keyboard::Event::KeyPressed {
                    key: iced::keyboard::Key::Named(iced::keyboard::key::Named::Escape),
                    ..
                }) => Some(Message::Escape),
                _ => None,
            });
            Subscription::batch([tick, window::frames().map(|_| Message::Frame), escape])
        } else {
            tick
        }
    }

    fn toast(&mut self, msg: impl Into<String>) {
        self.toast = Some((msg.into(), Instant::now()));
    }

    fn refresh(&mut self) {
        if let Some(e) = &self.engine {
            self.snap = e.snapshot();
            self.sink = e.video_sink();
        }
    }

    fn update(&mut self, message: Message) -> Task<Message> {
        let Some(engine) = self.engine.clone() else { return Task::none() };
        let mut spec = engine.audio_spec();
        match message {
            Message::Tick => {
                self.refresh();
                if let Some(n) = engine.take_notice() {
                    self.toast(n);
                }
                match engine.take_diagnostics_result() {
                    Some(Ok(dir)) => self.toast(format!("Saved to {}", dir.display())),
                    Some(Err(e)) => self.toast(format!("Couldn't save diagnostics: {e}")),
                    None => {}
                }
                if self.toast.as_ref().is_some_and(|(_, t)| t.elapsed() > Duration::from_secs(5)) {
                    self.toast = None;
                }
                if self.fullscreen && self.sink.is_none() {
                    // The stream ended (sharer stopped or left): leave fullscreen.
                    return self.update(Message::ToggleFullscreen);
                }
            }
            Message::Frame => {}
            Message::ToggleMute => {
                let me = &self.snap.me;
                if me.deafened {
                    engine.set_deafened(false);
                    engine.set_muted(false);
                } else {
                    engine.set_muted(!me.muted);
                }
                self.refresh();
            }
            Message::ToggleDeafen => {
                engine.set_deafened(!self.snap.me.deafened);
                self.refresh();
            }
            Message::ShareClicked => {
                if self.snap.me.sharing.is_some() {
                    engine.stop_share();
                    self.refresh();
                } else {
                    self.overlay = Overlay::SharePicker;
                    return Task::perform(blocking(screen::sources), Message::Sources);
                }
            }
            Message::Sources(list) => {
                self.sources = list.clone();
                // Thumbnails are slow (one capture each): load them after.
                return Task::perform(
                    blocking(move || {
                        list.iter()
                            .filter_map(|s| screen::thumbnail(s).map(|(w, h, px)| (source_key(s), w, h, px)))
                            .collect()
                    }),
                    Message::Thumbs,
                );
            }
            Message::Thumbs(list) => {
                for (key, w, h, px) in list {
                    self.thumbs.insert(key, image::Handle::from_rgba(w, h, px));
                }
            }
            Message::PickerTab(t) => self.picker_tab = t,
            Message::ShareSource(s) => {
                self.overlay = Overlay::None;
                return Task::perform(blocking(move || engine.start_share(&s)), Message::ShareResult);
            }
            Message::ShareResult(r) => {
                match r {
                    Ok(Some(warning)) => self.toast(warning),
                    Ok(None) => {}
                    Err(e) => self.toast(format!("Couldn't start sharing: {e}")),
                }
                self.refresh();
            }
            Message::ShareAudio(on) => {
                engine.set_share_audio(on);
                self.refresh();
            }
            Message::OpenSettings => {
                self.overlay = Overlay::Settings;
                self.name_draft = self.snap.me.name.clone();
                self.broker_draft = self.snap.internet.broker.clone();
                return Task::perform(blocking(load_devices), Message::Devices);
            }
            Message::Devices(d) => self.devices = d,
            Message::CloseOverlay => self.overlay = Overlay::None,
            Message::Watch(id) => {
                return Task::perform(blocking(move || engine.watch(id)), Message::WatchResult);
            }
            Message::WatchResult(r) => {
                if let Err(e) = r {
                    self.toast(format!("Couldn't open stream: {e}"));
                }
                self.refresh();
            }
            Message::Unwatch => {
                engine.unwatch();
                self.refresh();
                if self.fullscreen {
                    return self.update(Message::ToggleFullscreen);
                }
            }
            Message::Expand(id) => {
                self.expanded = if self.expanded == Some(id) { None } else { Some(id) };
            }
            Message::Volume(id, v) => engine.set_volume(id, v),
            Message::StreamVolume(v) => {
                engine.set_stream_volume(v);
                self.refresh();
            }
            Message::JoinChannel(to) => {
                self.armed_delete = None;
                if let Err(e) = engine.join(to) {
                    self.toast(e);
                }
                self.refresh();
            }
            Message::NewChannel => {
                if self.new_channel.take().is_none() {
                    self.new_channel = Some(String::new());
                    return iced::widget::operation::focus(NEW_CHANNEL_INPUT);
                }
            }
            Message::NewChannelDraft(s) => self.new_channel = Some(s),
            Message::NewChannelSubmit => {
                let name = self.new_channel.take().unwrap_or_default();
                if let Err(e) = engine.create_channel(&name) {
                    self.toast(format!("Couldn't create the channel: {e}"));
                }
                self.refresh();
            }
            Message::DeleteChannel(id) => {
                if self.armed_delete == Some(id) {
                    self.armed_delete = None;
                    engine.delete_channel(id);
                    self.refresh();
                } else {
                    self.armed_delete = Some(id);
                    self.toast("Click the bin again to delete this channel for good");
                }
            }
            Message::NameChanged(s) => self.name_draft = s,
            Message::NameSubmit => {
                engine.set_name(self.name_draft.clone());
                self.refresh();
            }
            Message::GroupDraft(s) => self.group_draft = s,
            Message::GroupJoin => {
                let code = std::mem::take(&mut self.group_draft);
                if discostu_proto::identity::normalize(&code).len() < 8 {
                    self.toast("That doesn't look like a group code");
                } else {
                    engine.set_group_code(&code);
                    self.toast("Joined the group. Friends in it will show up as they come online.");
                    self.refresh();
                }
            }
            Message::GroupNew => {
                engine.new_group();
                self.refresh();
                self.toast("New group made. Copy its code and send it to your friends.");
            }
            Message::GroupLeave => {
                engine.set_group_code("");
                self.refresh();
                self.toast("Left the group: LAN only now");
            }
            Message::CopyGroupCode => {
                self.toast("Group code copied");
                return iced::clipboard::write(self.snap.internet.group_code.clone());
            }
            Message::BrokerDraft(s) => self.broker_draft = s,
            Message::BrokerSubmit => {
                engine.set_broker(&self.broker_draft);
                self.refresh();
            }
            Message::ForgetBrokerKey => {
                engine.forget_broker_key();
                self.toast("Forgot the broker's key: the next one it shows will be trusted");
            }
            Message::ConnectChanged(s) => self.connect_draft = s,
            Message::ConnectSubmit => {
                let target = std::mem::take(&mut self.connect_draft);
                if !target.trim().is_empty() {
                    match engine.connect_manual(&target) {
                        Ok(()) => self.toast(format!("Connecting to {}…", target.trim())),
                        Err(e) => self.toast(format!("{}: {e}", target.trim())),
                    }
                }
            }
            Message::Driver(d) => {
                if d != spec.driver {
                    spec = DeviceSpec { driver: d, ..Default::default() };
                    if d == Driver::Asio {
                        let first = self.devices.asio.first().cloned();
                        spec.input = first.clone();
                        spec.output = first;
                    }
                    engine.set_audio_spec(spec);
                }
            }
            Message::InputDevice(d) => {
                spec.input = (d != DEFAULT_DEVICE).then_some(d);
                spec.mic_channel = None;
                engine.set_audio_spec(spec);
            }
            Message::OutputDevice(d) => {
                spec.output = (d != DEFAULT_DEVICE).then_some(d);
                engine.set_audio_spec(spec);
            }
            Message::AsioDriver(d) => {
                spec.input = Some(d.clone());
                spec.output = Some(d);
                spec.mic_channel = None;
                spec.asio_buffer = None;
                engine.set_audio_spec(spec);
            }
            Message::AsioBuffer(c) => {
                spec.asio_buffer = c.value;
                engine.set_audio_spec(spec);
            }
            Message::MicChannel(c) => {
                spec.mic_channel = c.value.map(|v| v as u16);
                engine.set_audio_spec(spec);
            }
            Message::Exclusive(on) => {
                spec.exclusive = on;
                engine.set_audio_spec(spec);
            }
            Message::Toggle(t, on) => {
                engine.set_toggle(t, on);
                self.refresh();
            }
            Message::SaveDiagnostics => {
                engine.save_diagnostics();
                self.toast("Saving the last 10 seconds of audio…");
            }
            Message::ResetProfile => {
                engine.reset_voice_profile();
                self.toast("Voice profile reset — it will relearn as you talk");
            }
            Message::WindowHandle(hwnd) => taskbar::set_large_icon(hwnd),
            Message::PointerMoved => self.pointer_moved = Instant::now(),
            Message::Escape => {
                if self.fullscreen {
                    return self.update(Message::ToggleFullscreen);
                }
            }
            Message::ToggleFullscreen => {
                self.fullscreen = !self.fullscreen;
                self.pointer_moved = Instant::now();
                let mode = if self.fullscreen { window::Mode::Fullscreen } else { window::Mode::Windowed };
                return window::latest().and_then(move |id| window::set_mode(id, mode));
            }
        }
        Task::none()
    }

    // ------------------------------------------------------------------ view

    fn view(&self) -> El<'_> {
        if let Some(err) = &self.fatal {
            return container(center(
                column![
                    text("discostu couldn't start").size(22).font(BOLD),
                    text(err.clone()).color(MUTED),
                ]
                .spacing(8)
                .align_x(Alignment::Center),
            ))
            .style(theme::app)
            .into();
        }

        let watching = self.sink.is_some() && self.snap.watching.is_some();
        let body: El = if watching && self.fullscreen {
            self.stage()
        } else {
            row![self.sidebar(), self.stage()].into()
        };

        let backdrop = if watching && self.fullscreen { theme::fullscreen } else { theme::app };
        let mut layers = vec![container(body).width(Length::Fill).height(Length::Fill).style(backdrop).into()];
        match self.overlay {
            Overlay::Settings => layers.push(self.modal(self.settings(), 600.0)),
            Overlay::SharePicker => layers.push(self.modal(self.share_picker(), 720.0)),
            Overlay::None => {}
        }
        if let Some((msg, _)) = &self.toast {
            layers.push(
                container(
                    container(text(msg.clone()).size(13).color(TEXT))
                        .padding([10, 16])
                        .style(theme::dock),
                )
                .width(Length::Fill)
                .height(Length::Fill)
                .align_x(Alignment::Center)
                .align_y(Alignment::Start)
                .padding(20)
                .into(),
            );
        }
        stack(layers).into()
    }

    fn modal<'a>(&'a self, content: El<'a>, width: f32) -> El<'a> {
        stack![
            mouse_area(container(Space::new()).width(Length::Fill).height(Length::Fill).style(theme::scrim))
                .on_press(Message::CloseOverlay),
            center(opaque(
                container(content).max_width(width).max_height(680.0).padding(24).style(theme::modal)
            ))
            .padding(32),
        ]
        .into()
    }

    // --- sidebar ---

    fn sidebar(&self) -> El<'_> {
        let header = row![
            container(svg(self.logo.clone()).width(34).height(34)).style(theme::stu_badge),
            column![
                text("discostu").size(17).font(BOLD),
                text("Disco Stu doesn't advertise").size(11).color(FAINT),
            ]
            .spacing(1),
        ]
        .spacing(11)
        .align_y(Alignment::Center);

        let label = row![
            text("CHANNELS").size(11).font(SEMIBOLD).color(FAINT).width(Length::Fill),
            tooltip(
                button(icon(if self.new_channel.is_some() { Icon::Close } else { Icon::Plus }, 14.0, MUTED))
                    .padding(4)
                    .style(theme::ghost)
                    .on_press(Message::NewChannel),
                container(text("New channel").size(12)).padding([5, 9]).style(theme::dock),
                tooltip::Position::Bottom,
            )
            .gap(6),
        ]
        .align_y(Alignment::Center);

        let mut list = column![].spacing(4);
        if let Some(draft) = &self.new_channel {
            list = list.push(
                text_input("Channel name, then Enter", draft)
                    .id(NEW_CHANNEL_INPUT)
                    .on_input(Message::NewChannelDraft)
                    .on_submit(Message::NewChannelSubmit)
                    .padding([8, 12])
                    .size(13)
                    .style(theme::input),
            );
        }
        list = list.push(self.channel_block(None, "Lobby", None, false));
        for c in &self.snap.channels {
            let host = if c.mine { "made by you".to_string() } else { format!("made by {}", c.host) };
            list = list.push(self.channel_block(Some(c.at), &c.name, Some(host), c.mine));
        }
        let internet = &self.snap.internet;
        if internet.group_code.is_empty() && self.snap.channels.is_empty() {
            list = list.push(
                container(text("Channels and friends outside this network need a group: Settings → Internet.").size(11).color(FAINT))
                    .padding([6, 10]),
            );
        }
        if !internet.waiting.is_empty() {
            let mut waiting = column![text("NOT CONNECTED YET").size(10).font(SEMIBOLD).color(FAINT)].spacing(4);
            for w in &internet.waiting {
                waiting = waiting.push(
                    row![icon(Icon::Users, 12.0, FAINT), text(format!("{} · connecting…", w.name)).size(12).color(FAINT)]
                        .spacing(8)
                        .align_y(Alignment::Center),
                );
            }
            list = list.push(container(waiting).padding([10, 10]));
        }

        container(
            column![header, Space::new().height(8), label, scrollable(list).height(Length::Fill), self.me_card()]
                .spacing(10)
                .padding(16),
        )
        .width(272)
        .height(Length::Fill)
        .style(theme::sidebar)
        .into()
    }

    /// A channel header (click to join) with its members underneath.
    fn channel_block<'a>(&'a self, at: Option<ChannelRef>, name: &str, host: Option<String>, mine: bool) -> El<'a> {
        let current = self.snap.channel == at;
        let mut title = column![text(name.to_string()).size(14).font(SEMIBOLD)].spacing(1);
        if let Some(h) = host {
            title = title.push(text(h).size(11).color(if mine { GOLD } else { FAINT }));
        }
        let glyph = if at.is_some() { Icon::Music } else { Icon::Users };
        let mut head = row![icon(glyph, 15.0, if current { PINK } else { MUTED }), title.width(Length::Fill)]
            .spacing(10)
            .align_y(Alignment::Center);
        if let (Some(at), true) = (at, mine) {
            let armed = self.armed_delete == Some(at.id);
            head = head.push(
                button(icon(Icon::Trash, 13.0, if armed { RED } else { FAINT }))
                    .padding(4)
                    .style(theme::ghost)
                    .on_press(Message::DeleteChannel(at.id)),
            );
        }
        let head = button(head)
            .padding([7, 10])
            .width(Length::Fill)
            .style(theme::channel_row(current))
            .on_press_maybe((!current).then_some(Message::JoinChannel(at)));

        let mut members = column![].spacing(2).padding(iced::Padding { left: 14.0, ..iced::Padding::ZERO });
        if current {
            let me = &self.snap.me;
            members = members.push(
                container(
                    row![
                        avatar(me.id, &me.name, 34.0, me.speaking.then_some(GREEN)),
                        column![text(me.name.clone()).size(14).font(SEMIBOLD), text("you").size(12).color(FAINT)]
                            .spacing(3),
                    ]
                    .spacing(11)
                    .align_y(Alignment::Center),
                )
                .padding([8, 10]),
            );
        }
        for p in self.members(at) {
            members = members.push(self.peer_row(p));
        }
        column![head, members].spacing(2).into()
    }

    fn peer_row<'a>(&'a self, p: &'a PeerView) -> El<'a> {
        let expanded = self.expanded == Some(p.id);
        let ring = p.speaking.then_some(GREEN);
        let mut subtitle = row![].spacing(6).align_y(Alignment::Center);
        if p.sharing.is_some() {
            subtitle = subtitle.push(live_badge());
        }
        subtitle = subtitle.push(text(latency(p.rtt_us)).size(12).color(latency_color(p.rtt_us)));
        if p.colocated {
            subtitle = subtitle.push(icon(Icon::Home, 12.0, MUTED));
        }
        if p.deafened {
            subtitle = subtitle.push(icon(Icon::HeadphonesOff, 12.0, RED));
        } else if p.muted {
            subtitle = subtitle.push(icon(Icon::MicOff, 12.0, RED));
        }

        let main = button(
            row![
                avatar(p.id, &p.name, 34.0, ring),
                column![text(p.name.clone()).size(14).font(SEMIBOLD), subtitle].spacing(3),
            ]
            .spacing(11)
            .align_y(Alignment::Center),
        )
        .padding([8, 10])
        .width(Length::Fill)
        .style(theme::row_button(expanded))
        .on_press(Message::Expand(p.id));

        if !expanded {
            return main.into();
        }

        let id = p.id;
        let jitter_ms = p.jitter.target as f32 * BLOCK as f32 * 1000.0 / SAMPLE_RATE as f32;
        let mut details = column![
            row![
                icon(Icon::Headphones, 14.0, MUTED),
                slider(0.0..=2.0, p.volume, move |v| Message::Volume(id, v)).step(0.01f32).style(theme::volume),
                text(format!("{:>3.0}%", p.volume * 100.0)).size(12).color(MUTED).width(36),
            ]
            .spacing(10)
            .align_y(Alignment::Center),
            text(format!(
                "{} · jitter {:.2} ms → buffer {:.1} ms · lost {} of {} · late bursts {}",
                format!("{} {}", p.path, p.addr),
                p.jitter.jitter_us / 1000.0,
                jitter_ms,
                p.net.expected - p.net.received,
                p.net.expected,
                p.jitter.underruns,
            ))
            .size(11)
            .color(FAINT),
        ]
        .spacing(10)
        .padding([8, 12]);
        if p.sharing.is_some() && self.snap.watching != Some(p.id) {
            details = details.push(
                button(
                    row![icon(Icon::Eye, 14.0, Color::WHITE), text("Watch screen").size(13)]
                        .spacing(8)
                        .align_y(Alignment::Center),
                )
                .padding([7, 12])
                .style(theme::primary)
                .on_press(Message::Watch(p.id)),
            );
        }
        column![main, details].into()
    }

    fn me_card(&self) -> El<'_> {
        let me = &self.snap.me;
        let status = if me.deafened {
            "Deafened"
        } else if me.muted {
            "Muted"
        } else if me.sharing.is_some() {
            "Sharing screen"
        } else {
            "Connected"
        };
        let status_color = if me.deafened || me.muted { RED } else { MUTED };
        let level = level_norm(me.level);
        let meter_color = if me.speaking { GREEN } else { alpha(TEXT, 0.35) };
        let progress = me.profile_progress;

        container(
            column![
                row![
                    avatar(me.id, &me.name, 36.0, me.speaking.then_some(GREEN)),
                    column![
                        text(me.name.clone()).size(14).font(SEMIBOLD),
                        text(status).size(12).color(status_color),
                    ]
                    .spacing(2),
                ]
                .spacing(11)
                .align_y(Alignment::Center),
                meter(if me.muted || me.deafened { 0.0 } else { level }, meter_color),
                row![
                    text("Voice profile").size(11).color(FAINT).width(Length::Fill),
                    text(if progress >= 1.0 { "learned".to_string() } else { format!("{:.0}%", progress * 100.0) })
                        .size(11)
                        .color(if progress >= 1.0 { GREEN } else { FAINT }),
                ],
                meter(progress, alpha(ACCENT, 0.8)),
            ]
            .spacing(10),
        )
        .padding(14)
        .style(theme::card)
        .into()
    }

    // --- stage ---

    /// Fullscreen controls (title bar, dock, cursor) after recent mouse movement.
    fn controls_visible(&self) -> bool {
        !self.fullscreen || self.pointer_moved.elapsed() < Duration::from_millis(2500)
    }

    fn stage(&self) -> El<'_> {
        let content: El = match (&self.sink, self.snap.watching) {
            (Some(sink), Some(id)) => self.video_stage(sink.clone(), id),
            _ if self.snap.peers.is_empty() => self.empty_stage(),
            _ => self.room_stage(),
        };
        let visible = self.controls_visible();
        let mut layers = vec![content];
        if visible {
            layers.push(
                container(self.dock())
                    .width(Length::Fill)
                    .height(Length::Fill)
                    .align_x(Alignment::Center)
                    .align_y(Alignment::End)
                    .padding(22)
                    .into(),
            );
        }
        let stage = stack(layers).width(Length::Fill).height(Length::Fill);
        if !self.fullscreen {
            return stage.into();
        }
        // Any movement (over the video or the controls) keeps them up; the
        // cursor hides with them.
        mouse_area(stage)
            .on_move(|_| Message::PointerMoved)
            .interaction(if visible { iced::mouse::Interaction::None } else { iced::mouse::Interaction::Hidden })
            .into()
    }

    fn dock(&self) -> El<'_> {
        let me = &self.snap.me;
        let silenced = me.muted || me.deafened;
        let btn = |i: Icon, tone: Tone, fg: Color, tip: &'static str, msg: Message| -> El<'_> {
            tooltip(
                button(icon(i, 20.0, fg)).padding(13).style(theme::dock_button(tone)).on_press(msg),
                container(text(tip).size(12)).padding([5, 9]).style(theme::dock),
                tooltip::Position::Top,
            )
            .gap(8)
            .into()
        };
        let mic = if silenced {
            btn(Icon::MicOff, Tone::Danger, RED, "Unmute", Message::ToggleMute)
        } else {
            btn(Icon::Mic, Tone::Normal, TEXT, "Mute", Message::ToggleMute)
        };
        let deafen = if me.deafened {
            btn(Icon::HeadphonesOff, Tone::Danger, RED, "Undeafen", Message::ToggleDeafen)
        } else {
            btn(Icon::Headphones, Tone::Normal, TEXT, "Deafen", Message::ToggleDeafen)
        };
        let share = if me.sharing.is_some() {
            btn(Icon::MonitorUp, Tone::Active, Color::WHITE, "Stop sharing", Message::ShareClicked)
        } else {
            btn(Icon::Monitor, Tone::Normal, TEXT, "Share screen", Message::ShareClicked)
        };
        let settings = btn(Icon::Sliders, Tone::Normal, TEXT, "Settings", Message::OpenSettings);

        let mut items = row![mic, deafen, share, settings].spacing(10).align_y(Alignment::Center);
        if let Some(h) = &me.share {
            let audio = if h.audio { " · audio" } else { "" };
            items = items.push(
                container(
                    text(format!("{} watching · {} fps · {:.0} Mb/s{audio}", h.viewers, h.fps, h.mbps)).size(12).color(MUTED),
                )
                .padding([0, 8]),
            );
        }
        container(items).padding(8).style(theme::dock).into()
    }

    fn empty_stage(&self) -> El<'_> {
        let addrs = if self.snap.addresses.is_empty() {
            "no network".to_string()
        } else {
            self.snap.addresses.join("   ")
        };
        center(
            column![
                container(svg(self.logo.clone()).width(112).height(112)).style(theme::stu_badge),
                Space::new().height(4),
                text("Nobody's on the dance floor yet").size(22).font(BOLD),
                text("Anyone running discostu on this LAN shows up here automatically.").size(14).color(MUTED),
                text("Friends elsewhere: share a group code in Settings → Internet.").size(13).color(MUTED),
                Space::new().height(6),
                text(format!("You're reachable at  {addrs}")).size(13).color(FAINT),
                row![
                    text_input("Or connect by address — 192.168.1.20", &self.connect_draft)
                        .on_input(Message::ConnectChanged)
                        .on_submit(Message::ConnectSubmit)
                        .padding([10, 14])
                        .size(14)
                        .width(320)
                        .style(theme::input),
                    button(text("Connect").size(14)).padding([10, 16]).style(theme::primary).on_press(Message::ConnectSubmit),
                ]
                .spacing(8),
            ]
            .spacing(10)
            .align_x(Alignment::Center),
        )
        .padding(iced::Padding { bottom: 90.0, ..iced::Padding::ZERO })
        .into()
    }

    fn room_stage(&self) -> El<'_> {
        let me = &self.snap.me;
        let mut tiles: Vec<El> = vec![participant(
            me.id,
            &me.name,
            "You",
            me.speaking,
            me.muted || me.deafened,
            false,
            None,
        )];
        for p in self.members(self.snap.channel) {
            let sub = if p.colocated { "in this room".to_string() } else { latency(p.rtt_us) };
            let watch = p.sharing.is_some().then_some(Message::Watch(p.id));
            tiles.push(participant(p.id, &p.name, &sub, p.speaking, p.muted || p.deafened, p.sharing.is_some(), watch));
        }
        let alone = tiles.len() == 1;
        let (name, host) = match self.current_channel() {
            Some(c) => (c.name.clone(), if c.mine { "your channel".to_string() } else { format!("hosted by {}", c.host) }),
            None => ("Lobby".to_string(), "everyone who isn't in a channel".to_string()),
        };
        let mut body = column![
            column![text(name).size(26).font(BOLD), text(host).size(13).color(MUTED)]
                .spacing(4)
                .align_x(Alignment::Center),
            row(tiles).spacing(16).wrap().vertical_spacing(16),
        ]
        .spacing(28)
        .align_x(Alignment::Center);
        if alone {
            body = body.push(text("Nobody else here yet. Friends join from the sidebar.").size(13).color(FAINT));
        }
        container(body)
            .width(Length::Fill)
            .height(Length::Fill)
            .align_x(Alignment::Center)
            .align_y(Alignment::Center)
            .padding(iced::Padding { top: 32.0, right: 32.0, bottom: 110.0, left: 32.0 })
            .into()
    }

    fn video_stage(&self, sink: Arc<VideoSink>, id: PeerId) -> El<'_> {
        let peer = self.snap.peers.iter().find(|p| p.id == id);
        let name = peer.map(|p| p.name.clone()).unwrap_or_default();
        let streaming = peer.is_some_and(|p| p.streaming);
        let stats = self.snap.video.clone().unwrap_or_default();
        let has_frame = sink.has_frame.load(std::sync::atomic::Ordering::Relaxed);

        let stat_text = if stats.width > 0 {
            let breakdown = if stats.codec == "H.264" {
                format!(" (enc {:.1} · dec {:.1})", stats.encode_ms, stats.decode_ms)
            } else {
                String::new()
            };
            format!(
                "{} · {}×{} · {} fps · {:.1} ms{breakdown} · {:.0} Mb/s",
                stats.codec, stats.width, stats.height, stats.fps, stats.latency_ms, stats.mbps
            )
        } else {
            "connecting…".into()
        };
        let mut header = row![
            container(
                row![live_badge(), text(format!("{name}'s screen")).size(13).font(SEMIBOLD), text(stat_text).size(12).color(MUTED)]
                    .spacing(10)
                    .align_y(Alignment::Center),
            )
            .padding([7, 12])
            .style(theme::dock),
            Space::new().width(Length::Fill),
        ]
        .spacing(8)
        .align_y(Alignment::Center);
        if streaming {
            let v = self.snap.stream_volume;
            header = header.push(
                container(
                    row![
                        icon(Icon::Headphones, 14.0, MUTED),
                        slider(0.0..=1.5, v, Message::StreamVolume).step(0.01f32).width(110).style(theme::volume),
                    ]
                    .spacing(8)
                    .align_y(Alignment::Center),
                )
                .padding([8, 12])
                .style(theme::dock),
            );
        }
        header = header
            .push(button(icon(Icon::Maximize, 16.0, TEXT)).padding(9).style(theme::dock_button(Tone::Normal)).on_press(Message::ToggleFullscreen))
            .push(button(icon(Icon::Close, 16.0, TEXT)).padding(9).style(theme::dock_button(Tone::Normal)).on_press(Message::Unwatch));

        let radius = if self.fullscreen { 0.0 } else { 14.0 };
        let surface: El = if has_frame {
            shader(video::Video { sink, radius }).width(Length::Fill).height(Length::Fill).into()
        } else {
            center(text("Waiting for the first frame…").color(MUTED)).into()
        };
        // Double-click toggles fullscreen.
        let surface = mouse_area(surface).on_double_click(Message::ToggleFullscreen);
        if !self.fullscreen {
            return column![header, surface]
                .spacing(12)
                .padding(iced::Padding { top: 20.0, right: 20.0, bottom: 96.0, left: 20.0 })
                .into();
        }

        // Truly fullscreen: the picture edge to edge, controls float on top
        // and fade with the cursor after a moment without movement.
        let visible = self.controls_visible();
        let mut layers: Vec<El> = vec![surface.into()];
        if visible {
            layers.push(container(header).width(Length::Fill).padding(16).into());
        }
        stack(layers).width(Length::Fill).height(Length::Fill).into()
    }

    // --- overlays ---

    fn share_picker(&self) -> El<'_> {
        let windows_tab = self.picker_tab == PickerTab::Windows;
        let shown: Vec<&Source> = self.sources.iter().filter(|s| s.is_window() == windows_tab).collect();

        let tab = |label: &'static str, t: PickerTab| {
            button(text(label).size(13))
                .padding([6, 14])
                .style(theme::segment(self.picker_tab == t))
                .on_press(Message::PickerTab(t))
        };
        let tabs = container(row![tab("Screens", PickerTab::Screens), tab("Windows", PickerTab::Windows)].spacing(2))
            .padding(3)
            .style(theme::pill(theme::BG));

        let grid: El = if self.sources.is_empty() {
            container(text("Looking for things to share…").color(MUTED)).padding(30).into()
        } else if shown.is_empty() {
            container(text("Nothing here").color(MUTED)).padding(30).into()
        } else {
            let cards: Vec<El> = shown
                .into_iter()
                .map(|s| {
                    let preview: El = match self.thumbs.get(&source_key(s)) {
                        Some(h) => image(h.clone())
                            .width(Length::Fill)
                            .height(Length::Fill)
                            .content_fit(iced::ContentFit::Contain)
                            .into(),
                        None => center(icon(if s.is_window() { Icon::Eye } else { Icon::Monitor }, 26.0, FAINT)).into(),
                    };
                    button(
                        column![
                            container(preview).width(Length::Fill).height(118).style(theme::pill_rect(theme::BG)),
                            text(s.title.clone()).size(13).font(SEMIBOLD).wrapping(text::Wrapping::None),
                            text(s.subtitle.clone()).size(11).color(MUTED).wrapping(text::Wrapping::None),
                        ]
                        .spacing(6),
                    )
                    .padding(8)
                    .width(212)
                    .clip(true)
                    .style(theme::row_button(false))
                    .on_press(Message::ShareSource(s.clone()))
                    .into()
                })
                .collect();
            scrollable(row(cards).spacing(10).wrap().vertical_spacing(10)).height(Length::Fixed(420.0)).into()
        };

        let share_audio = self.snap.share_audio;
        let audio_note = if windows_tab {
            "Only that app's sound"
        } else {
            "Everything except discostu itself — no echo of the call"
        };
        column![
            row![
                text("Share").size(18).font(BOLD),
                Space::new().width(14),
                tabs,
                Space::new().width(Length::Fill),
                button(icon(Icon::Close, 16.0, MUTED)).padding(6).style(theme::ghost).on_press(Message::CloseOverlay),
            ]
            .align_y(Alignment::Center),
            grid,
            row![
                column![
                    text("Share audio").size(14).font(SEMIBOLD),
                    text(audio_note).size(12).color(MUTED),
                ]
                .spacing(2)
                .width(Length::Fill),
                toggler(share_audio).on_toggle(Message::ShareAudio).size(20).style(theme::switch),
            ]
            .align_y(Alignment::Center),
            text("Hardware H.264, low-latency mode, straight from the GPU.").size(11).color(FAINT),
        ]
        .spacing(14)
        .into()
    }

    fn audio_settings(&self) -> El<'_> {
        let d = &self.snap.devices;
        let spec = self.engine.as_ref().map(|e| e.audio_spec()).unwrap_or_default();
        let lists = &self.devices;

        let mut drivers = vec![Driver::Wasapi];
        if !lists.asio.is_empty() || spec.driver == Driver::Asio {
            drivers.push(Driver::Asio);
        }
        let pick = |options: Vec<String>, selected: Option<String>, msg: fn(String) -> Message| -> El<'_> {
            pick_list(options, selected, msg).padding([8, 12]).width(Length::Fill).style(theme::picker).into()
        };
        let mut col = column![
            section("Audio"),
            labeled(
                "Driver",
                pick_list(drivers, Some(spec.driver), Message::Driver)
                    .padding([8, 12])
                    .width(Length::Fill)
                    .style(theme::picker)
                    .into(),
            ),
        ]
        .spacing(10);

        match spec.driver {
            Driver::Asio => {
                col = col.push(labeled("ASIO device", pick(lists.asio.clone(), spec.input.clone(), Message::AsioDriver)));
                // Buffer sizes the driver accepts.
                let mut sizes = vec![Choice { value: None, label: "Auto (smallest glitch-free)".into() }];
                if let Some((lo, hi)) = d.buffer_range {
                    for n in [16u32, 32, 48, 64, 96, 128, 192, 256, 512] {
                        if (lo..=hi).contains(&n) {
                            sizes.push(Choice {
                                value: Some(n),
                                label: format!("{n} frames · {:.2} ms", n as f32 * 1000.0 / d.input_rate.max(1) as f32),
                            });
                        }
                    }
                }
                let current = sizes.iter().find(|c| c.value == spec.asio_buffer).cloned();
                col = col.push(labeled(
                    "Buffer",
                    pick_list(sizes, current, Message::AsioBuffer)
                        .padding([8, 12])
                        .width(Length::Fill)
                        .style(theme::picker)
                        .into(),
                ));
                let wrapper = spec.input.as_deref().is_some_and(|n| {
                    let n = n.to_lowercase();
                    ["fl studio", "asio4all", "flexasio", "generic low latency"].iter().any(|w| n.contains(w))
                });
                if wrapper {
                    col = col.push(
                        text(
                            "This ASIO driver is a wrapper around WASAPI: it only opens the devices picked in its own \
                             control panel and adds its own buffering. Use WASAPI instead — it's at least as fast.",
                        )
                        .size(11)
                        .color(AMBER),
                    );
                }
            }
            Driver::Wasapi => {
                let ins = std::iter::once(DEFAULT_DEVICE.to_string()).chain(lists.wasapi_in.iter().cloned()).collect();
                let outs = std::iter::once(DEFAULT_DEVICE.to_string()).chain(lists.wasapi_out.iter().cloned()).collect();
                col = col
                    .push(labeled(
                        "Microphone",
                        pick(ins, Some(spec.input.clone().unwrap_or_else(|| DEFAULT_DEVICE.into())), Message::InputDevice),
                    ))
                    .push(labeled(
                        "Speakers",
                        pick(outs, Some(spec.output.clone().unwrap_or_else(|| DEFAULT_DEVICE.into())), Message::OutputDevice),
                    ))
                    .push(
                        row![
                            column![
                                text("Exclusive mode").size(14).font(SEMIBOLD),
                                text("Bypasses Windows audio for the lowest latency. Games and other apps can't use your mic or speakers while it's on.")
                                    .size(12)
                                    .color(MUTED),
                            ]
                            .spacing(3)
                            .width(Length::Fill),
                            toggler(spec.exclusive).on_toggle(Message::Exclusive).size(20).style(theme::switch),
                        ]
                        .spacing(16)
                        .align_y(Alignment::Center),
                    );
            }
        }

        if d.input_channels > 1 {
            let mut chans = vec![Choice { value: None, label: "Mix all inputs".into() }];
            chans.extend((0..d.input_channels as u32).map(|c| Choice { value: Some(c), label: format!("Input {}", c + 1) }));
            let current = chans.iter().find(|c| c.value == spec.mic_channel.map(u32::from)).cloned();
            col = col.push(labeled(
                "Mic channel",
                pick_list(chans, current, Message::MicChannel)
                    .padding([8, 12])
                    .width(Length::Fill)
                    .style(theme::picker)
                    .into(),
            ));
        }

        let period = |frames: usize, rate: u32| -> String {
            if frames == 0 {
                "starting…".into()
            } else {
                format!("{frames} frames · {:.2} ms", frames as f32 * 1000.0 / rate.max(1) as f32)
            }
        };
        if let Some(n) = &d.input {
            col = col.push(
                text(format!("In: {n} · {} kHz · {} · {}", d.input_rate / 1000, d.input_mode, period(d.input_period, d.input_rate)))
                    .size(11)
                    .color(FAINT),
            );
        }
        if let Some(n) = &d.output {
            col = col.push(
                text(format!("Out: {n} · {} kHz · {} · {}", d.output_rate / 1000, d.output_mode, period(d.output_period, d.output_rate)))
                    .size(11)
                    .color(FAINT),
            );
        }
        if spec.driver == Driver::Wasapi && (d.input_mode == "shared" || d.output_mode == "shared") {
            col = col.push(
                text("This device's driver only supports Windows' standard 10 ms period. Exclusive mode goes lower.")
                    .size(11)
                    .color(AMBER),
            );
        }
        if let Some(e) = &d.error {
            col = col.push(text(e.clone()).size(12).color(RED));
        }
        col = col.push(
            row![
                text("Hear a click or crackle? Save right after it happens.").size(12).color(MUTED).width(Length::Fill),
                button(text("Save last 10 s").size(12)).padding([6, 12]).style(theme::ghost).on_press(Message::SaveDiagnostics),
            ]
            .spacing(10)
            .align_y(Alignment::Center),
        );
        col.into()
    }

    fn settings(&self) -> El<'_> {
        let s = &self.snap;
        let audio = self.audio_settings();

        let echo_note = match s.me.echo_delay_ms {
            Some(ms) => format!("Removes your speakers from your mic. Locked on at {ms:.0} ms."),
            None => "Removes your speakers from your mic.".into(),
        };
        let progress = s.me.profile_progress;
        let voice = column![
            section("Voice processing"),
            switch_row("Echo cancellation", echo_note, s.echo_cancel, Toggle::EchoCancel),
            switch_row(
                "Same-room separation",
                "While someone in your room talks, your mic stays closed, so they never hear themselves come back.".into(),
                s.crosstalk_cancel,
                Toggle::CrosstalkCancel,
            ),
            switch_row(
                "Voice gate",
                "Only transmit when it sounds like you, using your voice profile.".into(),
                s.noise_gate,
                Toggle::NoiseGate,
            ),
            container(
                column![
                    row![
                        text("Voice profile").size(14).font(SEMIBOLD).width(Length::Fill),
                        button(text("Reset").size(12)).padding([4, 10]).style(theme::ghost).on_press(Message::ResetProfile),
                    ]
                    .align_y(Alignment::Center),
                    meter(progress, ACCENT),
                    text(if progress >= 1.0 {
                        "Trained. Keeps adapting slowly as your voice, mic and room change.".to_string()
                    } else {
                        format!(
                            "{:.0}% — learns the shape of your voice whenever you talk, and remembers it.",
                            progress * 100.0
                        )
                    })
                    .size(12)
                    .color(MUTED),
                ]
                .spacing(8),
            )
            .padding(14)
            .style(theme::card),
        ]
        .spacing(12);

        let internet = self.internet_settings();

        let saved = self.engine.as_ref().map(|e| e.manual_peers()).unwrap_or_default();
        let network = column![
            section("Network"),
            text(format!(
                "Reachable at {}",
                if s.addresses.is_empty() { "—".into() } else { s.addresses.join(", ") }
            ))
            .size(13)
            .color(MUTED),
            row![
                text_input("Connect by address (host or host:port)", &self.connect_draft)
                    .on_input(Message::ConnectChanged)
                    .on_submit(Message::ConnectSubmit)
                    .padding([8, 12])
                    .size(14)
                    .style(theme::input),
                button(icon(Icon::Plus, 16.0, Color::WHITE)).padding(9).style(theme::primary).on_press(Message::ConnectSubmit),
            ]
            .spacing(8),
            text(if saved.is_empty() { String::new() } else { format!("Always dialing: {}", saved.join(", ")) })
                .size(11)
                .color(FAINT),
        ]
        .spacing(10);

        let name = column![
            section("Profile"),
            text_input("Your name", &self.name_draft)
                .on_input(Message::NameChanged)
                .on_submit(Message::NameSubmit)
                .padding([8, 12])
                .size(14)
                .style(theme::input),
        ]
        .spacing(10);

        let budget = self.latency_budget();

        column![
            row![
                text("Settings").size(20).font(BOLD).width(Length::Fill),
                button(icon(Icon::Close, 16.0, MUTED)).padding(6).style(theme::ghost).on_press(Message::CloseOverlay),
            ]
            .align_y(Alignment::Center),
            scrollable(column![name, audio, voice, internet, network, budget].spacing(26).padding(iced::Padding {
                right: 12.0,
                ..iced::Padding::ZERO
            }))
            .height(Length::Shrink),
        ]
        .spacing(18)
        .into()
    }

    /// Where the milliseconds go, mouth to ear, with the current devices.
    /// Friends group and broker: how people outside this LAN find us.
    fn internet_settings(&self) -> El<'_> {
        let i = &self.snap.internet;
        let mut col = column![section("Internet")].spacing(10);
        if i.group_code.is_empty() {
            col = col.push(
                text(
                    "To talk with friends outside this network, start a group and send them its code, \
                     or paste the code a friend sent you. Channels live in the group too.",
                )
                .size(13)
                .color(MUTED),
            );
            col = col.push(
                row![
                    text_input("Paste a group code", &self.group_draft)
                        .on_input(Message::GroupDraft)
                        .on_submit(Message::GroupJoin)
                        .padding([8, 12])
                        .size(14)
                        .style(theme::input),
                    button(text("Join").size(13)).padding([8, 14]).style(theme::primary).on_press(Message::GroupJoin),
                    button(text("New group").size(13)).padding([8, 14]).style(theme::ghost).on_press(Message::GroupNew),
                ]
                .spacing(8)
                .align_y(Alignment::Center),
            );
        } else {
            col = col.push(
                container(
                    column![
                        row![
                            text("Group code").size(12).color(FAINT).width(Length::Fill),
                            button(text("Copy").size(12)).padding([4, 10]).style(theme::ghost).on_press(Message::CopyGroupCode),
                            button(text("Leave").size(12)).padding([4, 10]).style(theme::ghost).on_press(Message::GroupLeave),
                        ]
                        .spacing(6)
                        .align_y(Alignment::Center),
                        text(i.group_code.clone()).size(22).font(iced::Font::MONOSPACE).color(GOLD),
                        text("Anyone with this code can join the group: send it only to friends.").size(11).color(FAINT),
                    ]
                    .spacing(6),
                )
                .padding(14)
                .style(theme::card),
            );
            let (dot, status) = match &i.status {
                BrokerStatus::Off => (FAINT, "Broker off: LAN only".to_string()),
                BrokerStatus::Connecting => (GOLD, "Connecting to the broker…".to_string()),
                BrokerStatus::Connected { observed } => {
                    (GREEN, format!("Online. The internet sees you at {observed}."))
                }
                BrokerStatus::Failed(e) => (RED, e.clone()),
            };
            col = col.push(
                row![container(Space::new()).width(8).height(8).style(move |_| theme::dot(dot)), text(status).size(13).color(MUTED)]
                    .spacing(8)
                    .align_y(Alignment::Center),
            );
            let upnp = match &i.portmap {
                PortMap::Trying => "Router port mapping: asking the router…".to_string(),
                PortMap::Mapped { addr, via } => format!("Router port mapping: open at {addr} ({via}), friends can always reach you."),
                PortMap::CarrierNat(ip) => format!(
                    "Router port mapping: your provider puts you behind a shared address ({ip}), so it can't help. \
                     Direct connections still work unless your friend has the same."
                ),
                PortMap::Unavailable(e) => format!("Router port mapping: unavailable ({e}). Hole punching usually works anyway."),
                PortMap::Disabled => "Router port mapping: off".to_string(),
            };
            col = col.push(text(upnp).size(12).color(FAINT));
        }
        let broker_failed_key = matches!(&i.status, BrokerStatus::Failed(e) if e.contains("identity changed"));
        let mut broker_row = row![
            text("Broker").size(13).color(MUTED).width(60),
            text_input(crate::config::DEFAULT_BROKER, &self.broker_draft)
                .on_input(Message::BrokerDraft)
                .on_submit(Message::BrokerSubmit)
                .padding([6, 10])
                .size(13)
                .style(theme::input),
        ]
        .spacing(8)
        .align_y(Alignment::Center);
        if self.broker_draft.trim() != i.broker.trim() {
            broker_row = broker_row.push(button(text("Save").size(12)).padding([6, 12]).style(theme::primary).on_press(Message::BrokerSubmit));
        }
        if broker_failed_key {
            broker_row = broker_row.push(
                button(text("Trust new key").size(12)).padding([6, 12]).style(theme::ghost).on_press(Message::ForgetBrokerKey),
            );
        }
        col.push(broker_row)
            .push(
                text("The broker only introduces you: voice and video always go straight between you and your friends, encrypted.")
                    .size(11)
                    .color(FAINT),
            )
            .into()
    }

    fn latency_budget(&self) -> El<'_> {
        let d = &self.snap.devices;
        let ms = |frames: usize, rate: u32| (frames > 0).then(|| frames as f32 * 1000.0 / rate.max(1) as f32);
        let cap = ms(d.input_period, d.input_rate);
        let play = ms(d.output_period, d.output_rate);
        let block = BLOCK as f32 * 1000.0 / SAMPLE_RATE as f32;
        let best = self.snap.peers.iter().map(|p| p.rtt_us).filter(|r| *r > 0).min();
        let net = best.map(|r| r as f32 / 2000.0);
        let jitter = self
            .snap
            .peers
            .iter()
            .map(|p| p.jitter.target as f32 * block)
            .fold(None, |a: Option<f32>, v| Some(a.map_or(v, |a| a.min(v))));
        let total = [cap, Some(block), net, jitter, play].iter().flatten().sum::<f32>();
        let fmt = |v: Option<f32>, fallback: &str| v.map(|v| format!("{v:.2} ms")).unwrap_or_else(|| fallback.into());
        let line = |label: &'static str, value: String| -> El<'_> {
            row![text(label).size(12).color(MUTED).width(Length::Fill), text(value).size(12).font(SEMIBOLD)].into()
        };
        column![
            section("Latency budget"),
            line("Mic buffer", fmt(cap, "—")),
            line("Processing block", format!("{block:.2} ms")),
            line("Network, one way", fmt(net, "no peers")),
            line("Jitter buffer", fmt(jitter, "—")),
            line("Speaker buffer", fmt(play, "—")),
            line("Total (plus converters)", format!("≈ {total:.1} ms")),
            line("DSP load", format!("{:.0}% of budget", self.snap.dsp_load * 100.0)),
            line("Glitches (xruns / underruns)", format!("{} / {}", d.xruns, self.snap.playback_underruns)),
        ]
        .spacing(6)
        .into()
    }
}

// --- small components -----------------------------------------------------

fn avatar<'a>(id: u64, name: &str, size: f32, ring: Option<Color>) -> El<'a> {
    let initial = name.chars().next().map(|c| c.to_uppercase().collect::<String>()).unwrap_or_else(|| "?".into());
    container(text(initial).size(size * 0.42).font(BOLD))
        .width(size)
        .height(size)
        .align_x(Alignment::Center)
        .align_y(Alignment::Center)
        .style(theme::avatar(avatar_color(id), ring, size))
        .into()
}

fn participant<'a>(
    id: u64,
    name: &str,
    subtitle: &str,
    speaking: bool,
    silenced: bool,
    live: bool,
    on_press: Option<Message>,
) -> El<'a> {
    let mut status = row![].spacing(6).align_y(Alignment::Center);
    if live {
        status = status.push(live_badge());
    }
    if silenced {
        status = status.push(icon(Icon::MicOff, 13.0, RED));
    }
    status = status.push(text(subtitle.to_string()).size(12).color(MUTED));

    let body = container(
        column![
            avatar(id, name, 76.0, speaking.then_some(GREEN)),
            Space::new().height(4),
            text(name.to_string()).size(15).font(SEMIBOLD),
            status,
        ]
        .spacing(6)
        .align_x(Alignment::Center),
    )
    .width(210)
    .height(186)
    .align_x(Alignment::Center)
    .align_y(Alignment::Center)
    .style(theme::tile(speaking));

    match on_press {
        Some(msg) => mouse_area(body).on_press(msg).interaction(iced::mouse::Interaction::Pointer).into(),
        None => body.into(),
    }
}

fn live_badge<'a>() -> El<'a> {
    container(text("LIVE").size(10).font(BOLD).color(Color::WHITE))
        .padding([1, 6])
        .style(theme::pill(RED))
        .into()
}

fn meter<'a>(value: f32, color: Color) -> El<'a> {
    let filled = (value.clamp(0.0, 1.0) * 1000.0).round() as u16;
    let mut r = row![].height(4);
    if filled > 0 {
        r = r.push(container(Space::new()).width(Length::FillPortion(filled)).height(4).style(theme::meter_fill(color)));
    }
    if filled < 1000 {
        r = r.push(container(Space::new()).width(Length::FillPortion(1000 - filled)).height(4).style(theme::meter_track));
    }
    r.into()
}

fn section<'a>(title: &'static str) -> El<'a> {
    column![
        text(title.to_uppercase()).size(11).font(SEMIBOLD).color(FAINT),
        container(Space::new()).width(Length::Fill).height(1).style(theme::divider),
    ]
    .spacing(8)
    .into()
}

fn labeled<'a>(label: &'static str, control: El<'a>) -> El<'a> {
    row![text(label).size(13).color(MUTED).width(110), control].spacing(12).align_y(Alignment::Center).into()
}

fn switch_row<'a>(title: &'static str, note: String, on: bool, t: Toggle) -> El<'a> {
    row![
        column![text(title).size(14).font(SEMIBOLD), text(note).size(12).color(MUTED)].spacing(3).width(Length::Fill),
        toggler(on).on_toggle(move |v| Message::Toggle(t, v)).size(20).style(theme::switch),
    ]
    .spacing(16)
    .align_y(Alignment::Center)
    .into()
}

fn level_norm(rms: f32) -> f32 {
    let db = 20.0 * rms.max(1e-6).log10();
    ((db + 60.0) / 54.0).clamp(0.0, 1.0)
}

fn latency(rtt_us: u64) -> String {
    if rtt_us == 0 {
        "measuring…".into()
    } else {
        format!("{:.2} ms", rtt_us as f32 / 2000.0)
    }
}

fn latency_color(rtt_us: u64) -> Color {
    match rtt_us {
        0 => FAINT,
        r if r < 2_000 => GREEN,
        r if r < 10_000 => AMBER,
        _ => RED,
    }
}
