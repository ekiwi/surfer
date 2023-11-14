use std::io::Read;
use std::sync::atomic::AtomicU64;
use std::sync::Arc;

use crate::wasm_util::perform_work;
use camino::Utf8PathBuf;
use color_eyre::eyre::{anyhow, WrapErr};
use color_eyre::Result;
use eframe::egui::{self, DroppedFile};
use futures_util::FutureExt;
use futures_util::TryFutureExt;
use log::info;
#[cfg(not(target_arch = "wasm32"))]
use rfd::FileDialog;

use crate::{message::Message, State};

#[derive(Debug)]
pub enum WaveSource {
    File(Utf8PathBuf),
    DragAndDrop(Option<Utf8PathBuf>),
    Url(String),
}

impl std::fmt::Display for WaveSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WaveSource::File(file) => write!(f, "{file}"),
            WaveSource::DragAndDrop(None) => write!(f, "Dropped file"),
            WaveSource::DragAndDrop(Some(filename)) => write!(f, "Dropped file ({filename})"),
            WaveSource::Url(url) => write!(f, "{url}"),
        }
    }
}

#[derive(Debug)]
pub enum OpenMode {
    Open,
    Switch,
}

pub enum LoadProgress {
    Downloading(String),
    Loading(Option<u64>, Arc<AtomicU64>),
}

impl State {
    pub fn load_vcd_from_file(
        &mut self,
        vcd_filename: Utf8PathBuf,
        keep_signals: bool,
    ) -> Result<()> {
        info!("Load VCD: {vcd_filename}");
        let source = WaveSource::File(vcd_filename);
        let sender = self.msg_sender.clone();

        perform_work(move || {
            let result = waveform::vcd::read(vcd_filename.as_str())
                .map_err(|e| anyhow!("{e:?}"))
                .with_context(|| format!("Failed to parse VCD file: {source}"));

            match result {
                Ok(waves) => sender
                    .send(Message::WavesLoaded(
                        source,
                        Box::new(waves),
                        keep_signals,
                    ))
                    .unwrap(),
                Err(e) => sender.send(Message::Error(e)).unwrap(),
            }
        });

        Ok(())
    }

    pub fn load_vcd_from_dropped(&mut self, file: DroppedFile, keep_signals: bool) -> Result<()> {
        info!("Got a dropped file");

        let filename = file.path.and_then(|p| Utf8PathBuf::try_from(p).ok());
        let bytes = file
            .bytes
            .ok_or_else(|| anyhow!("Dropped a file with no bytes"))?;

        let total_bytes = bytes.len();

        self.load_vcd_from_bytes(
            WaveSource::DragAndDrop(filename),
            &bytes,
            Some(total_bytes as u64),
            keep_signals,
        );
        Ok(())
    }

    pub fn load_vcd_from_url(&mut self, url: String, keep_signals: bool) {
        let sender = self.msg_sender.clone();
        let url_ = url.clone();
        let task = async move {
            let bytes = reqwest::get(&url)
                .map(|e| e.with_context(|| format!("Failed fetch download {url}")))
                .and_then(|resp| {
                    resp.bytes()
                        .map(|e| e.with_context(|| format!("Failed to download {url}")))
                })
                .await;

            match bytes {
                Ok(b) => sender.send(Message::FileDownloaded(url, b, keep_signals)),
                Err(e) => sender.send(Message::Error(e)),
            }
            .unwrap();
        };
        #[cfg(not(target_arch = "wasm32"))]
        tokio::spawn(task);
        #[cfg(target_arch = "wasm32")]
        wasm_bindgen_futures::spawn_local(task);

        self.vcd_progress = Some(LoadProgress::Downloading(url_))
    }

    pub fn load_vcd_from_bytes(
        &mut self,
        source: WaveSource,
        bytes: &[u8],
        total_bytes: Option<u64>,
        keep_signals: bool,
    ) {
        // Progress tracking in bytes
        let progress_bytes = Arc::new(AtomicU64::new(0));
        // TODO: re-enable progress tracking with new waveform backend.

        // let reader = {
        //     info!("Creating progress reader");
        //     let progress_bytes = progress_bytes.clone();
        //     ProgressReader::new(reader, move |progress: usize| {
        //         progress_bytes.fetch_add(progress as u64, Ordering::SeqCst);
        //     })
        // };

        let sender = self.msg_sender.clone();

        perform_work(move || {
            let result = waveform::vcd::read_from_bytes(bytes)
                .map_err(|e| anyhow!("{e:?}"))
                .with_context(|| format!("Failed to parse VCD file: {source}"));

            match result {
                Ok(waves) => sender
                    .send(Message::WavesLoaded(
                        source,
                        Box::new(waves),
                        keep_signals,
                    ))
                    .unwrap(),
                Err(e) => sender.send(Message::Error(e)).unwrap(),
            }
        });

        info!("Setting VCD progress");
        self.vcd_progress = Some(LoadProgress::Loading(total_bytes, progress_bytes));
    }

    pub fn open_file_dialog(&mut self, mode: OpenMode) {
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(path) = FileDialog::new()
            .set_title("Open waveform file")
            .add_filter("VCD-files (*.vcd)", &["vcd"])
            .add_filter("All files", &["*"])
            .pick_file()
        {
            self.load_vcd_from_file(
                camino::Utf8PathBuf::from_path_buf(path).unwrap(),
                match mode {
                    OpenMode::Open => false,
                    OpenMode::Switch => true,
                },
            )
            .ok();
        }
    }
}

pub fn draw_progress_panel(ctx: &egui::Context, vcd_progress_data: &LoadProgress) {
    egui::TopBottomPanel::top("progress panel").show(ctx, |ui| {
        ui.vertical_centered_justified(|ui| match vcd_progress_data {
            LoadProgress::Downloading(url) => {
                ui.spinner();
                ui.monospace(format!("Downloading {url}"));
            }
            LoadProgress::Loading(total_bytes, bytes_done) => {
                let num_bytes = bytes_done.load(std::sync::atomic::Ordering::Relaxed);

                if let Some(total) = total_bytes {
                    ui.monospace(format!("Loading. {num_bytes}/{total} kb loaded"));
                    let progress = num_bytes as f32 / *total as f32;
                    let progress_bar = egui::ProgressBar::new(progress)
                        .show_percentage()
                        .desired_width(300.);

                    ui.add(progress_bar);
                } else {
                    ui.spinner();
                    ui.monospace(format!("Loading. {num_bytes} bytes loaded"));
                };
            }
        });
    });
}
