mod signal_canvas;
mod translation;
mod view;
mod viewport;

use camino::Utf8PathBuf;
use clap::Parser;
use color_eyre::eyre::anyhow;
use color_eyre::eyre::Context;
use color_eyre::Result;
use eframe::egui;
use eframe::epaint::Vec2;
use fastwave_backend::parse_vcd;
use fastwave_backend::ScopeIdx;
use fastwave_backend::SignalIdx;
use fastwave_backend::VCD;
use fern::colors::ColoredLevelConfig;
use log::debug;
use log::info;
use num::bigint::ToBigInt;
use num::BigInt;
use num::BigRational;
use num::FromPrimitive;
use num::ToPrimitive;
use progress_streams::ProgressReader;
use pyo3::append_to_inittab;

use translation::pytranslator::surfer;
use translation::SignalInfo;
use translation::Translator;
use translation::TranslatorList;
use viewport::Viewport;

use std::collections::HashMap;
use std::fs::File;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::sync::mpsc::channel;
use std::sync::mpsc::Receiver;

#[derive(clap::Parser)]
struct Args {
    vcd_file: Utf8PathBuf,
}

fn main() -> Result<()> {
    color_eyre::install()?;

    // Load python modules we deinfe in this crate
    append_to_inittab!(surfer);

    let args = Args::parse();

    let state = State::new(args)?;

    let options = eframe::NativeOptions {
        initial_window_size: Some(egui::vec2(1920., 1080.)),
        ..Default::default()
    };
    eframe::run_native("My egui App", options, Box::new(|_cc| Box::new(state)));

    Ok(())
}

struct VcdData {
    inner: VCD,
    active_scope: Option<ScopeIdx>,
    signals: Vec<(SignalIdx, SignalInfo)>,
    viewport: Viewport,
    num_timestamps: BigInt,
    signal_format: HashMap<SignalIdx, String>,
    cursor: Option<BigInt>,
}

struct State {
    vcd: Option<VcdData>,
    /// The offset of the left side of the wave window in signal timestamps.
    control_key: bool,
    /// Which translator to use for each signal
    translators: TranslatorList,

    /// Receiver for messages generated by other threads
    msg_receiver: Receiver<Message>,

    /// The number of bytes loaded from the vcd file
    vcd_progess: Arc<AtomicUsize>,
}

#[derive(Debug)]
enum Message {
    HierarchyClick(ScopeIdx),
    AddSignal(SignalIdx),
    SignalFormatChange(SignalIdx, String),
    CanvasScroll {
        delta: Vec2,
    },
    CanvasZoom {
        mouse_ptr_timestamp: BigRational,
        delta: f32,
    },
    CursorSet(BigInt),
    VcdLoaded(Box<VCD>),
    Error(color_eyre::eyre::Error),
}

impl State {
    fn new(args: Args) -> Result<State> {
        let vcd_filename = args.vcd_file;


        let colors = ColoredLevelConfig::new()
            .error(fern::colors::Color::Red)
            .warn(fern::colors::Color::Yellow)
            .info(fern::colors::Color::Green)
            .debug(fern::colors::Color::Blue)
            .trace(fern::colors::Color::White);

        let stdout_config = fern::Dispatch::new()
            .level(log::LevelFilter::Info)
            .format(move |out, message, record| {
                out.finish(format_args!(
                    "[{}] {}",
                    colors.color(record.level()),
                    message
                ))
            })
            .chain(std::io::stdout());

        fern::Dispatch::new().chain(stdout_config).apply()?;

        let translators = TranslatorList::new(vec![
            Box::new(translation::HexTranslator {}),
            Box::new(translation::UnsignedTranslator {}),
            Box::new(translation::HierarchyTranslator {}),
            // Box::new(PyTranslator::new("pytest", "translation_test.py").unwrap()),
        ]);

        let (sender, receiver) = channel();


        // We'll open the file to check if it exists here to panic the main thread if not.
        // Then we pass the file into the thread for parsing
        let file =
            File::open(&vcd_filename).with_context(|| format!("Failed to open {vcd_filename}"))?;

        // Progress tracking in bytes
        let progress_bytes = Arc::new(AtomicUsize::new(0));
        let reader = {
            let progress_bytes = progress_bytes.clone();
            ProgressReader::new(file, move |progress: usize| {
                progress_bytes.fetch_add(progress, Ordering::SeqCst);
            })
        };

        std::thread::spawn(move || {
            println!("Loading VCD");
            let result = parse_vcd(reader)
                .map_err(|e| anyhow!("{e}"))
                .with_context(|| format!("Failed to parse parse {vcd_filename}"));

            println!("Done loading");

            match result {
                Ok(vcd) => sender.send(Message::VcdLoaded(Box::new(vcd))),
                Err(e) => sender.send(Message::Error(e))
            }
        });

        Ok(State {
            vcd: None,
            control_key: false,
            translators,
            msg_receiver: receiver,
            vcd_progess: progress_bytes,
        })
    }

    // TODO: Rename to process_msg or something
    fn update(&mut self, message: Message) {
        match message {
            Message::HierarchyClick(scope) => {
                let mut vcd = self.vcd.as_mut().expect("HierarchyClick without vcd set");

                vcd.active_scope = Some(scope)
            }
            Message::AddSignal(s) => {
                let vcd = self.vcd.as_mut().expect("AddSignal without vcd set");

                let translator = vcd.signal_translator(s, &self.translators);
                let info = translator.signal_info(&vcd.signal_name(s)).unwrap();
                vcd.signals.push((s, info))
            }
            Message::CanvasScroll { delta } => self.handle_canvas_scroll(delta),
            Message::CanvasZoom {
                delta,
                mouse_ptr_timestamp,
            } => {
                self.vcd.as_mut().map(|vcd| {
                    vcd.handle_canvas_zoom(mouse_ptr_timestamp, delta as f64)
                });
            },
            Message::SignalFormatChange(idx, format) => {
                let vcd = self
                    .vcd
                    .as_mut()
                    .expect("Signal format change without vcd set");

                if self.translators.inner.contains_key(&format) {
                    *vcd.signal_format.entry(idx).or_default() = format;

                    let translator = vcd.signal_translator(idx, &self.translators);
                    let info = translator.signal_info(&vcd.signal_name(idx)).unwrap();
                    vcd.signals.retain(|(old_idx, _)| *old_idx != idx);
                    vcd.signals.push((idx, info));
                } else {
                    println!("WARN: No translator {format}")
                }
            }
            Message::CursorSet(new) => {
                self.vcd.as_mut().map(|vcd| vcd.cursor = Some(new));
            },
            Message::VcdLoaded(new_vcd_data) => {
                let num_timestamps = new_vcd_data
                    .max_timestamp()
                    .as_ref()
                    .map(|t| t.to_bigint().unwrap())
                    .unwrap_or(BigInt::from_u32(1).unwrap());

                let new_vcd = VcdData {
                    inner: *new_vcd_data,
                    active_scope: None,
                    signals: vec![],
                    viewport: Viewport::new(0., num_timestamps.clone().to_f64().unwrap()),
                    signal_format: HashMap::new(),
                    num_timestamps,
                    cursor: None,
                };

                self.vcd = Some(new_vcd);
            }
            Message::Error(e) => {
                eprintln!("{e}")
            }
        }
    }

    pub fn handle_canvas_scroll(
        &mut self,
        // Canvas relative
        delta: Vec2,
    ) {
        if let Some(vcd) = &mut self.vcd {
            // Scroll 5% of the viewport per scroll event.
            // One scroll event yields 50
            let scroll_step = (&vcd.viewport.curr_right - &vcd.viewport.curr_left) / (50. * 20.);

            let target_left = &vcd.viewport.curr_left + scroll_step * delta.y as f64;
            let target_right = &vcd.viewport.curr_right + scroll_step * delta.y as f64;

            vcd.viewport.curr_left = target_left;
            vcd.viewport.curr_right = target_right;
        }
    }

}

impl VcdData {
    pub fn signal_name(&self, idx: SignalIdx) -> String {
        self.inner.signal_from_signal_idx(idx).name()
    }

    pub fn signal_translator<'a>(
        &'a self,
        sig: SignalIdx,
        translators: &'a TranslatorList,
    ) -> &'a Box<dyn Translator> {
        let translator_name = self
            .signal_format
            .get(&sig)
            .unwrap_or_else(|| &translators.default);
        let translator = &translators.inner[translator_name];
        translator
    }

    pub fn handle_canvas_zoom(
        &mut self,
        // Canvas relative
        mouse_ptr_timestamp: BigRational,
        delta: f64,
    ) {
        // Zoom or scroll
        let Viewport {
            curr_left: left,
            curr_right: right,
            ..
        } = &self.viewport;

        let target_left = (left - mouse_ptr_timestamp.to_f64().unwrap()) / delta
            + &mouse_ptr_timestamp.to_f64().unwrap();
        let target_right = (right - mouse_ptr_timestamp.to_f64().unwrap()) / delta
            + &mouse_ptr_timestamp.to_f64().unwrap();

        // TODO: Do not just round here, this will not work
        // for small zoom levels
        self.viewport.curr_left = target_left;
        self.viewport.curr_right = target_right;
    }
}
