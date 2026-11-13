use crate::settings::ThemeChoice;
use crate::theme;
use crate::video_widget::video;
use crate::{App, Dialog, Message, MyShare, SessionView, Watch};
use iced::widget::{
    Column, button, center, checkbox, column, container, image, mouse_area, opaque, row, rule,
    scrollable, slider, space, stack, text, text_input,
};
use iced::{Alignment, ContentFit, Element, Length, Theme};
use screenshare_capture::SourceKind;
use screenshare_net::ShareKind;

type El<'a> = Element<'a, Message>;

fn label(s: &str) -> El<'_> {
    text(s).size(12).style(theme::muted_text).into()
}

fn muted<'a>(s: impl text::IntoFragment<'a>) -> text::Text<'a> {
    text(s).style(theme::muted_text)
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}

fn encoding_label(backend: &str) -> String {
    match backend {
        "" => String::new(),
        "software" => "CPU encode".into(),
        gpu => gpu.to_string(),
    }
}

fn decoding_label(backend: &str) -> String {
    match backend {
        "software" => "CPU decode".into(),
        gpu => gpu.to_string(),
    }
}

fn btn<'a>(label: &'a str) -> button::Button<'a, Message> {
    button(text(label).size(14)).padding([7, 14])
}

fn hairline<'a>() -> El<'a> {
    rule::horizontal(1)
        .style(|t: &Theme| rule::Style {
            color: iced::Color {
                a: 0.2,
                ..t.palette().text
            },
            radius: 0.0.into(),
            fill_mode: rule::FillMode::Full,
            snap: true,
        })
        .into()
}

fn vline<'a>() -> El<'a> {
    rule::vertical(1)
        .style(|t: &Theme| rule::Style {
            color: iced::Color {
                a: 0.2,
                ..t.palette().text
            },
            radius: 0.0.into(),
            fill_mode: rule::FillMode::Full,
            snap: true,
        })
        .into()
}

fn theme_label(choice: ThemeChoice) -> &'static str {
    match choice {
        ThemeChoice::Light => "Dark theme",
        ThemeChoice::Dark => "Light theme",
    }
}

impl App {
    pub fn view(&self) -> El<'_> {
        let base = match &self.session {
            Some(session) => self.session_view(session),
            None => self.home(),
        };
        match &self.dialog {
            Some(dialog) => {
                let content = match dialog {
                    Dialog::Sources { sources, selected } => {
                        self.sources_dialog(sources.as_deref(), *selected)
                    }
                    Dialog::AudioApp { apps, selected } => audio_dialog(apps.as_deref(), *selected),
                    Dialog::Settings => self.settings_dialog(),
                };
                let modal = container(opaque(content)).style(theme::panel).padding(24);
                stack![
                    base,
                    opaque(
                        mouse_area(center(modal).padding(32).style(theme::scrim))
                            .on_press(Message::CloseDialog)
                    )
                ]
                .into()
            }
            None => base,
        }
    }

    fn home(&self) -> El<'_> {
        let busy = self.busy;
        let top = row![
            space::horizontal(),
            btn("Settings")
                .style(theme::ghost)
                .on_press(Message::OpenSettings),
            btn(theme_label(self.settings.theme))
                .style(theme::ghost)
                .on_press(Message::ToggleTheme),
        ]
        .spacing(4)
        .padding(12);

        let mut form = column![
            text("screenshare").size(34),
            muted("Peer-to-peer screen sharing. No accounts, no servers."),
            space().height(12),
            label("NICKNAME"),
            text_input("How others will see you", &self.settings.nickname)
                .on_input(Message::NicknameChanged)
                .on_submit(Message::Create)
                .style(theme::input)
                .padding(10),
            button(center(text("Create session")).height(Length::Shrink))
                .style(theme::primary)
                .padding(10)
                .width(Length::Fill)
                .on_press_maybe((!busy).then_some(Message::Create)),
            row![hairline(), muted("or join").size(12), hairline()]
                .spacing(10)
                .align_y(Alignment::Center),
            text_input("Paste an invite code", &self.ticket_input)
                .on_input(Message::TicketChanged)
                .on_submit(Message::Join)
                .style(theme::input)
                .padding(10),
            button(center(text("Join session")).height(Length::Shrink))
                .style(theme::secondary)
                .padding(10)
                .width(Length::Fill)
                .on_press_maybe((!busy).then_some(Message::Join)),
        ]
        .spacing(10)
        .max_width(380);

        if busy {
            form = form.push(muted("Connecting…"));
        }
        if let Some(err) = &self.error {
            form = form.push(text(err).style(theme::live_text));
        }

        column![top, center(form).padding(24)].into()
    }

    fn session_view<'a>(&'a self, session: &'a SessionView) -> El<'a> {
        let main = self.main_area(session);
        if self.fullscreen {
            return main;
        }
        row![
            container(self.sidebar(session)).width(270).padding(16),
            vline(),
            main
        ]
        .into()
    }

    fn sidebar<'a>(&'a self, session: &'a SessionView) -> El<'a> {
        let status = if session.connected {
            "Connected"
        } else if session.peers.is_empty() {
            "Waiting for others to join"
        } else {
            "Reconnecting…"
        };

        let mut people = Column::new().spacing(10);
        let my_share = match &session.my_share {
            MyShare::Live { title, .. } => Some(title.as_str()),
            _ => None,
        };
        people = people.push(person_row(
            format!("{} (you)", truncate(self.settings.nickname.trim(), 24)),
            my_share,
            None,
        ));
        let mut others: Vec<_> = session.peers.iter().collect();
        others.sort_by_key(|(_, p)| p.nickname.to_lowercase());
        for (id, peer) in others {
            let watching = session.watching.iter().any(|w| w.peer == *id);
            let action = peer.share.as_ref().map(|_| {
                if watching {
                    ("Hide", Message::Unwatch(*id))
                } else {
                    ("Watch", Message::Watch(*id))
                }
            });
            people = people.push(person_row(
                truncate(&peer.nickname, 28),
                peer.share.as_ref().map(|s| s.title.as_str()),
                action,
            ));
        }

        let share_button: El<'a> = match &session.my_share {
            MyShare::Idle => button(center(text("Share screen")).height(Length::Shrink))
                .style(theme::primary)
                .padding(10)
                .width(Length::Fill)
                .on_press(Message::ShareClicked)
                .into(),
            MyShare::Starting => button(center(text("Starting…")).height(Length::Shrink))
                .style(theme::primary)
                .padding(10)
                .width(Length::Fill)
                .into(),
            MyShare::Live { kind, encoder, .. } => {
                let what = match kind {
                    ShareKind::Monitor => "screen",
                    ShareKind::Window => "window",
                };
                column![
                    row![
                        text("●").size(10).style(theme::live_text),
                        text(format!("Sharing your {what}")).size(13),
                        space::horizontal(),
                        muted(encoding_label(encoder)).size(12),
                    ]
                    .spacing(6)
                    .align_y(Alignment::Center),
                    button(center(text("Stop sharing")).height(Length::Shrink))
                        .style(theme::secondary)
                        .padding(10)
                        .width(Length::Fill)
                        .on_press(Message::StopShare),
                ]
                .spacing(8)
                .into()
            }
        };

        let live = !matches!(session.my_share, MyShare::Idle);
        let audio_toggle = checkbox(session.share_audio)
            .label("Include audio")
            .text_size(13)
            .style(theme::check)
            .on_toggle_maybe((!live).then_some(Message::ShareAudioToggled));

        column![
            text("Session").size(20),
            row![
                text(if session.connected { "●" } else { "○" })
                    .size(10)
                    .style(theme::muted_text),
                muted(status).size(13)
            ]
            .spacing(6)
            .align_y(Alignment::Center),
            button(
                center(text(if session.copied {
                    "Copied"
                } else {
                    "Copy invite code"
                }))
                .height(Length::Shrink)
            )
            .style(theme::secondary)
            .padding(8)
            .width(Length::Fill)
            .on_press(Message::CopyInvite),
            space().height(6),
            label("PEOPLE"),
            scrollable(people).height(Length::Fill),
            audio_toggle,
            share_button,
            hairline(),
            row![
                btn("Settings")
                    .style(theme::ghost)
                    .on_press(Message::OpenSettings),
                btn(if self.settings.theme == ThemeChoice::Dark {
                    "Light"
                } else {
                    "Dark"
                })
                .style(theme::ghost)
                .on_press(Message::ToggleTheme),
                space::horizontal(),
                btn("Leave")
                    .style(theme::secondary)
                    .on_press(Message::Leave),
            ]
            .spacing(2)
            .align_y(Alignment::Center),
        ]
        .spacing(10)
        .into()
    }

    fn main_area<'a>(&'a self, session: &'a SessionView) -> El<'a> {
        let mut col = Column::new()
            .spacing(12)
            .padding(if self.fullscreen { 0 } else { 16 });
        if let Some(err) = &self.error
            && !self.fullscreen
        {
            col = col.push(
                container(
                    row![
                        text(err).size(13).width(Length::Fill),
                        btn("Dismiss")
                            .style(theme::ghost)
                            .on_press(Message::DismissError)
                    ]
                    .spacing(8)
                    .align_y(Alignment::Center),
                )
                .style(theme::panel)
                .padding([6, 12]),
            );
        }

        let focused = session
            .focus
            .and_then(|p| session.watching.iter().find(|w| w.peer == p));
        let body: El<'a> = if let Some(w) = focused {
            self.tile(session, w, true)
        } else if session.watching.is_empty() {
            let anyone = session.peers.values().any(|p| p.share.is_some());
            center(
                column![
                    text("Nothing on screen yet").size(18),
                    muted(if anyone {
                        "Pick a live share from the list to watch it."
                    } else {
                        "When someone shares their screen, it shows up here."
                    })
                    .size(13),
                ]
                .spacing(6)
                .align_x(Alignment::Center),
            )
            .into()
        } else {
            let n = session.watching.len();
            let cols = (n as f32).sqrt().ceil() as usize;
            let mut grid = Column::new().spacing(12).height(Length::Fill);
            for chunk in session.watching.chunks(cols) {
                let mut r = iced::widget::Row::new().spacing(12).height(Length::Fill);
                for w in chunk {
                    r = r.push(self.tile(session, w, false));
                }
                for _ in chunk.len()..cols {
                    r = r.push(space::horizontal());
                }
                grid = grid.push(r);
            }
            grid.into()
        };
        col.push(body).into()
    }

    fn tile<'a>(&'a self, session: &'a SessionView, w: &'a Watch, focused: bool) -> El<'a> {
        let peer = session.peers.get(&w.peer);
        let name = peer.map(|p| p.nickname.as_str()).unwrap_or("?");
        let share = peer.and_then(|p| p.share.as_ref());
        let title = share.map(|s| s.title.as_str()).unwrap_or_default();
        let decoder = w.slot.decoder();
        let pipeline = match (share.map(|s| s.codec.as_str()), decoder.is_empty()) {
            (Some(codec), false) => {
                format!("{} → {}", encoding_label(codec), decoding_label(&decoder))
            }
            (Some(codec), true) => encoding_label(codec),
            (None, _) => String::new(),
        };

        let picture: El<'a> = if w.slot.has_frame() {
            video(w.stream_id, w.slot.clone())
        } else {
            center(muted("Connecting…").size(13)).into()
        };
        let picture = mouse_area(
            container(picture)
                .width(Length::Fill)
                .height(Length::Fill)
                .style(theme::video_area),
        )
        .on_double_click(Message::Focus(if focused { None } else { Some(w.peer) }));

        if self.fullscreen && focused {
            return picture.into();
        }

        let peer_id = w.peer;
        let volume = if w.muted { 0.0 } else { w.volume };
        let caption = container(
            row![
                text(truncate(name, 24))
                    .size(14)
                    .wrapping(text::Wrapping::None),
                muted(truncate(title, 40))
                    .size(12)
                    .wrapping(text::Wrapping::None),
                muted(pipeline).size(11).wrapping(text::Wrapping::None),
            ]
            .spacing(6)
            .align_y(Alignment::Center),
        )
        .width(Length::Fill)
        .clip(true);
        let mut footer = row![
            caption,
            slider(0.0..=1.0, volume, move |v| Message::Volume(peer_id, v))
                .step(0.01_f32)
                .width(70)
                .style(theme::slider),
            btn(if w.muted { "Unmute" } else { "Mute" })
                .style(theme::ghost)
                .on_press(Message::ToggleMute(w.peer)),
            btn(if focused { "Grid" } else { "Focus" })
                .style(theme::ghost)
                .on_press(Message::Focus(if focused { None } else { Some(w.peer) })),
        ]
        .spacing(6)
        .padding([4, 8])
        .align_y(Alignment::Center);
        if focused {
            footer = footer.push(
                btn(if self.fullscreen {
                    "Exit fullscreen"
                } else {
                    "Fullscreen"
                })
                .style(theme::ghost)
                .on_press(Message::ToggleFullscreen),
            );
        }
        footer = footer.push(
            btn("Close")
                .style(theme::ghost)
                .on_press(Message::Unwatch(w.peer)),
        );

        container(column![picture, footer])
            .style(theme::panel)
            .width(Length::Fill)
            .height(Length::Fill)
            .padding(1)
            .into()
    }

    fn sources_dialog<'a>(
        &'a self,
        sources: Option<&'a [crate::PickerSource]>,
        selected: Option<usize>,
    ) -> El<'a> {
        let body: El<'a> = match sources {
            None => center(muted("Looking for screens and windows…"))
                .height(200)
                .into(),
            Some([]) => center(muted("Nothing to share was found."))
                .height(200)
                .into(),
            Some(sources) => {
                let mut list = Column::new().spacing(14);
                for (heading, kind) in [
                    ("SCREENS", SourceKind::Monitor),
                    ("WINDOWS", SourceKind::Window),
                ] {
                    let items: Vec<_> = sources
                        .iter()
                        .enumerate()
                        .filter(|(_, s)| s.source.kind == kind)
                        .collect();
                    if items.is_empty() {
                        continue;
                    }
                    list = list.push(label(heading));
                    for chunk in items.chunks(3) {
                        let mut r = iced::widget::Row::new().spacing(10);
                        for (i, s) in chunk {
                            let thumb: El<'a> = match &s.thumbnail {
                                Some(h) => image(h.clone())
                                    .width(208)
                                    .height(117)
                                    .content_fit(ContentFit::Contain)
                                    .into(),
                                None => container(space())
                                    .width(208)
                                    .height(117)
                                    .style(theme::video_area)
                                    .into(),
                            };
                            r = r.push(
                                button(
                                    column![thumb, text(truncate(&s.source.title, 30)).size(12)]
                                        .spacing(6)
                                        .width(208),
                                )
                                .style(theme::choice(selected == Some(*i)))
                                .padding(6)
                                .on_press(Message::PickSource(*i)),
                            );
                        }
                        list = list.push(r);
                    }
                }
                scrollable(list).height(Length::Fill).into()
            }
        };

        let audio_note = selected
            .and_then(|i| sources?.get(i))
            .map(|s| match s.source.kind {
                SourceKind::Monitor => "All desktop audio is shared, except excluded apps.",
                SourceKind::Window => "Only this app's audio is shared.",
            })
            .unwrap_or("");
        let audio_note = if self.session.as_ref().is_some_and(|s| !s.share_audio) {
            "Audio is off."
        } else {
            audio_note
        };

        column![
            text("Share your screen").size(20),
            body,
            row![
                muted(audio_note).size(12),
                space::horizontal(),
                btn("Cancel")
                    .style(theme::secondary)
                    .on_press(Message::CloseDialog),
                btn("Share")
                    .style(theme::primary)
                    .on_press_maybe(selected.map(|_| Message::ConfirmSource)),
            ]
            .spacing(8)
            .align_y(Alignment::Center),
        ]
        .spacing(14)
        .width(712)
        .height(560)
        .into()
    }

    fn settings_dialog(&self) -> El<'_> {
        let s = &self.settings;
        let choices = |options: &[(u32, &'static str)], current: u32, msg: fn(u32) -> Message| {
            let mut r = iced::widget::Row::new().spacing(6);
            for &(value, name) in options {
                r = r.push(
                    btn(name)
                        .style(theme::choice(current == value))
                        .on_press(msg(value)),
                );
            }
            r
        };
        column![
            text("Settings").size(20),
            label("THEME"),
            row![
                btn("Light")
                    .style(theme::choice(s.theme == ThemeChoice::Light))
                    .on_press(Message::SetTheme(ThemeChoice::Light)),
                btn("Dark")
                    .style(theme::choice(s.theme == ThemeChoice::Dark))
                    .on_press(Message::SetTheme(ThemeChoice::Dark)),
            ]
            .spacing(6),
            label("NEVER SHARE AUDIO FROM"),
            text_input("discord, spotify", &self.exclude_input)
                .on_input(Message::ExcludeChanged)
                .style(theme::input)
                .padding(8),
            muted("Comma-separated app names. Applies when sharing a whole screen.").size(12),
            label("FRAME RATE"),
            choices(
                &[(15, "15 fps"), (30, "30 fps"), (60, "60 fps")],
                s.max_fps,
                Message::SetFps
            ),
            label("MAX RESOLUTION"),
            choices(
                &[
                    (720, "720p"),
                    (1080, "1080p"),
                    (1440, "1440p"),
                    (2160, "4K")
                ],
                s.max_height,
                Message::SetMaxHeight
            ),
            label("MAX BITRATE"),
            choices(
                &[
                    (2000, "2 Mbps"),
                    (4000, "4 Mbps"),
                    (6000, "6 Mbps"),
                    (10000, "10 Mbps"),
                    (16000, "16 Mbps")
                ],
                s.max_bitrate_kbps,
                Message::SetBitrate
            ),
            checkbox(s.hardware_video)
                .label("Use the GPU to encode and decode video")
                .text_size(13)
                .style(theme::check)
                .on_toggle(Message::SetHardwareVideo),
            muted("Video changes apply to your next share or newly opened streams.").size(12),
            row![
                space::horizontal(),
                btn("Done")
                    .style(theme::primary)
                    .on_press(Message::CloseDialog)
            ],
        ]
        .spacing(10)
        .width(460)
        .into()
    }
}

fn person_row<'a>(
    name: String,
    share: Option<&'a str>,
    action: Option<(&'a str, Message)>,
) -> El<'a> {
    let mut top = row![text(name).size(14)]
        .spacing(6)
        .align_y(Alignment::Center);
    if share.is_some() {
        top = top
            .push(space::horizontal())
            .push(text("● live").size(11).style(theme::live_text));
    }
    let mut col = column![top].spacing(2);
    if let Some(title) = share {
        let mut r = row![muted(truncate(title, 26)).size(12), space::horizontal()]
            .spacing(6)
            .align_y(Alignment::Center);
        if let Some((name, msg)) = action {
            r = r.push(
                button(text(name).size(12))
                    .padding([2, 8])
                    .style(theme::secondary)
                    .on_press(msg),
            );
        }
        col = col.push(r);
    }
    col.into()
}

fn audio_dialog(apps: Option<&[screenshare_capture::AudioApp]>, selected: Option<usize>) -> El<'_> {
    let mut list = Column::new().spacing(6);
    match apps {
        None => list = list.push(muted("Looking for apps playing audio…")),
        Some([]) => list = list.push(muted("No apps are playing audio right now.")),
        Some(apps) => {
            for (i, app) in apps.iter().enumerate() {
                let detail = app.binary.clone().unwrap_or_default();
                list = list.push(
                    button(row![
                        text(truncate(&app.name, 36)).size(14),
                        space::horizontal(),
                        text(detail).size(12)
                    ])
                    .style(theme::choice(selected == Some(i)))
                    .width(Length::Fill)
                    .padding([8, 12])
                    .on_press(Message::PickAudioApp(Some(i))),
                );
            }
        }
    }
    list = list.push(
        button(text("No audio").size(14))
            .style(theme::choice(selected.is_none()))
            .width(Length::Fill)
            .padding([8, 12])
            .on_press(Message::PickAudioApp(None)),
    );
    column![
        text("Which app's audio?").size(20),
        muted("The system picker doesn't say which app owns the window. Pick the app whose sound you want to share.")
            .size(13),
        scrollable(list).height(Length::Shrink),
        row![
            space::horizontal(),
            btn("Continue").style(theme::primary).on_press(Message::ConfirmAudioApp)
        ],
    ]
    .spacing(12)
    .width(460)
    .into()
}
