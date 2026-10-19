mod transfer;

use std::path::PathBuf;

use gpui_kit::component::{
    ActiveTheme, Disableable,
    button::{Button, ButtonVariants},
};
use gpui_kit::*;
use transfer::TransferEvent;

#[derive(Default)]
struct Folders {
    source: Option<PathBuf>,
    destination: Option<PathBuf>,
    total_files: usize,
    moved_files: usize,
    transfer_status: TransferStatus,
    transfer_errors: Vec<String>,
    current_file: Option<PathBuf>,
    screen: Screen,
}

#[derive(Default)]
enum TransferStatus {
    #[default]
    Idle,
    Scanning,
    Moving,
    Finished,
}

impl TransferStatus {
    fn in_progress(&self) -> bool {
        matches!(self, Self::Scanning | Self::Moving)
    }
}

#[derive(Default)]
enum Screen {
    #[default]
    Selection,
    Transfer,
}

enum PickerTarget {
    Source,
    Destination,
}

impl Folders {
    async fn move_files(this: WeakEntity<Self>, cx: &mut AsyncApp) {
        let Ok(Some((source, destination))) = this.read_with(cx, |folders, _| {
            folders.source.clone().zip(folders.destination.clone())
        }) else {
            return;
        };

        let (sender, receiver) = async_channel::bounded(64);
        let worker = cx.background_executor().spawn(async move {
            transfer::run(&source, &destination, |event| {
                sender.send_blocking(event).is_ok()
            });
        });

        while let Ok(event) = receiver.recv().await {
            if this
                .update(cx, |folders, cx| {
                    match event {
                        TransferEvent::Scanned(total) => {
                            folders.total_files = total;
                            folders.transfer_status = TransferStatus::Moving;
                        }
                        TransferEvent::Moving(path) => folders.current_file = Some(path),
                        TransferEvent::Moved => folders.moved_files += 1,
                        TransferEvent::Error(error) => {
                            eprintln!("{error}");
                            folders.transfer_errors.push(error);
                        }
                        TransferEvent::Finished => {
                            folders.current_file = None;
                            folders.transfer_status = TransferStatus::Finished;
                        }
                    }
                    cx.notify();
                })
                .is_err()
            {
                receiver.close();
                break;
            }
        }
        worker.await;
    }

    fn can_continue(&self) -> bool {
        matches!(
            (&self.source, &self.destination),
            (Some(source), Some(destination)) if source != destination
        )
    }

    fn pick_path(&mut self, target: PickerTarget, cx: &mut Context<Self>) {
        let is_source = matches!(target, PickerTarget::Source);
        let picker = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some(
                if is_source {
                    "Select source directory"
                } else {
                    "Select destination directory"
                }
                .into(),
            ),
        });

        cx.spawn(async move |this, cx| {
            match picker.await {
                Ok(Ok(Some(paths))) => {
                    if let Some(path) = paths.into_iter().next() {
                        // The view may have closed while the picker was open.
                        let _ = this.update(cx, |this, cx| {
                            match target {
                                PickerTarget::Source => this.source = Some(path),
                                PickerTarget::Destination => this.destination = Some(path),
                            }
                            cx.notify();
                        });
                    }
                }
                Ok(Ok(None)) => {}
                Ok(Err(error)) => eprintln!("File picker failed: {error}"),
                Err(error) => eprintln!("File picker channel closed: {error}"),
            }
        })
        .detach();
    }
}

impl Render for Folders {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let source = self
            .source
            .as_ref()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "No source directory selected".into());
        let destination = self
            .destination
            .as_ref()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "No destination directory selected".into());

        if matches!(self.screen, Screen::Transfer) {
            let status = match self.transfer_status {
                TransferStatus::Idle => "Ready to transfer",
                TransferStatus::Scanning => "Scanning source directory...",
                TransferStatus::Moving => "Moving files...",
                TransferStatus::Finished if self.transfer_errors.is_empty() => "Transfer complete",
                TransferStatus::Finished => "Transfer finished with issues",
            };
            return div()
                .flex()
                .flex_col()
                .size_full()
                .justify_center()
                .p_6()
                .gap_4()
                .bg(cx.theme().background)
                .text_color(cx.theme().foreground)
                .child(div().text_2xl().child("File transfer"))
                .child(status)
                .child(format!(
                    "Files moved: {} of {}",
                    self.moved_files, self.total_files
                ))
                .children(self.current_file.as_ref().map(|path| {
                    div()
                        .text_sm()
                        .truncate()
                        .child(format!("Moving: {}", path.display()))
                }))
                .child(
                    div()
                        .text_sm()
                        .truncate()
                        .text_color(cx.theme().muted_foreground)
                        .child(format!("Source: {source}")),
                )
                .child(
                    div()
                        .text_sm()
                        .truncate()
                        .text_color(cx.theme().muted_foreground)
                        .child(format!("Destination: {destination}")),
                )
                .children(self.transfer_errors.last().map(|error| {
                    div()
                        .text_sm()
                        .child(format!("Issues: {}. {error}", self.transfer_errors.len()))
                }))
                .child(
                    Button::new("back")
                        .label("Back")
                        .disabled(self.transfer_status.in_progress())
                        .on_click(cx.listener(|this, _, _, cx| {
                            if !this.transfer_status.in_progress() {
                                this.screen = Screen::Selection;
                                cx.notify();
                            }
                        })),
                );
        }

        div()
            .flex()
            .flex_col()
            .size_full()
            .justify_center()
            .p_6()
            .gap_6()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child("Source directory")
                    .child(
                        Button::new("source")
                            .primary()
                            .label("Choose source directory")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.pick_path(PickerTarget::Source, cx);
                            })),
                    )
                    .child(
                        div()
                            .text_sm()
                            .truncate()
                            .text_color(cx.theme().muted_foreground)
                            .child(source),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child("Destination directory")
                    .child(
                        Button::new("destination")
                            .primary()
                            .label("Choose destination directory")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.pick_path(PickerTarget::Destination, cx);
                            })),
                    )
                    .child(
                        div()
                            .text_sm()
                            .truncate()
                            .text_color(cx.theme().muted_foreground)
                            .child(destination),
                    ),
            )
            .child(
                Button::new("continue")
                    .primary()
                    .label("Start transfer")
                    .disabled(!self.can_continue())
                    .on_click(cx.listener(|this, _, _, cx| {
                        if this.can_continue() {
                            this.total_files = 0;
                            this.moved_files = 0;
                            this.transfer_status = TransferStatus::Scanning;
                            this.transfer_errors.clear();
                            this.current_file = None;
                            this.screen = Screen::Transfer;
                            cx.notify();
                            cx.spawn(async move |this, cx| {
                                Self::move_files(this, cx).await;
                            })
                            .detach();
                        }
                    })),
            )
    }
}

fn main() {
    application().with_assets(assets::Assets).run(|cx| {
        init(cx);
        cx.on_window_closed(|cx, _| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();

        let options = WindowOptions {
            window_bounds: Some(WindowBounds::centered(size(px(560.0), px(520.0)), cx)),
            titlebar: Some(TitlebarOptions {
                title: Some("Folde".into()),
                ..Default::default()
            }),
            ..Default::default()
        };

        open_window(options, cx, |_, cx| cx.new(|_| Folders::default()))
            .expect("Failed to open the application window");
        cx.activate(true);
    });
}
