mod benchmark;
mod clock_highlighting;
mod command_prompt;
mod commands;
mod config;
mod cursor;
mod displayed_item;
mod help;
mod keys;
mod menus;
mod message;
mod mousegestures;
mod signal_canvas;
mod signal_filter;
mod signal_name_type;
#[cfg(test)]
mod tests;
mod time;
mod translation;
mod util;
mod view;
mod viewport;
mod wasm_util;
mod wave_container;
mod wave_source;

use camino::Utf8PathBuf;
#[cfg(not(target_arch = "wasm32"))]
use clap::Parser;
use color_eyre::eyre::Context;
use color_eyre::Result;
use config::SurferConfig;
use displayed_item::DisplayedCursor;
use displayed_item::DisplayedDivider;
use displayed_item::DisplayedItem;
use displayed_item::DisplayedSignal;
#[cfg(not(target_arch = "wasm32"))]
use eframe::egui;
use eframe::egui::style::Selection;
use eframe::egui::style::WidgetVisuals;
use eframe::egui::style::Widgets;
use eframe::egui::Visuals;
use eframe::emath;
use eframe::epaint::Rect;
use eframe::epaint::Rounding;
use eframe::epaint::Stroke;
use eframe::epaint::Vec2;
use waveform::{Hierarchy, ScopeRef, Timescale, Var, Waveform};
#[cfg(not(target_arch = "wasm32"))]
use fern::colors::ColoredLevelConfig;
use log::error;
use log::info;
use log::trace;
use log::warn;
use message::Message;
use num::bigint::ToBigInt;
use num::BigInt;
use num::FromPrimitive;
use num::ToPrimitive;
use signal_filter::SignalFilterType;
use signal_name_type::SignalNameType;
use translation::all_translators;
use translation::spade::SpadeTranslator;
use translation::TranslationPreference;
use translation::Translator;
use translation::TranslatorList;
use viewport::Viewport;
use wasm_util::perform_work;
use wave_container::FieldRef;
use wave_container::ScopeName;
use wave_container::VarName;
use wave_source::LoadProgress;
use wave_source::WaveSource;

use std::cell::RefCell;
use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::mpsc::channel;
use std::sync::mpsc::{Receiver, Sender};

#[derive(clap::Parser, Default)]
struct Args {
    vcd_file: Option<Utf8PathBuf>,
    #[clap(long)]
    spade_state: Option<Utf8PathBuf>,
    #[clap(long)]
    spade_top: Option<String>,
}

struct StartupParams {
    pub spade_state: Option<Utf8PathBuf>,
    pub spade_top: Option<String>,
    pub waves: Option<WaveSource>,
}

impl StartupParams {
    #[allow(dead_code)] // NOTE: Only used in wasm version
    pub fn empty() -> Self {
        Self {
            spade_state: None,
            spade_top: None,
            waves: None,
        }
    }

    #[allow(dead_code)] // NOTE: Only used in wasm version
    pub fn vcd_from_url(url: Option<String>) -> Self {
        Self {
            spade_state: None,
            spade_top: None,
            waves: url.map(WaveSource::Url),
        }
    }

    #[allow(dead_code)] // NOTE: Only used in desktop version
    pub fn from_args(args: Args) -> Self {
        Self {
            spade_state: args.spade_state,
            spade_top: args.spade_top,
            waves: args.vcd_file.map(WaveSource::File),
        }
    }
}

// When compiling natively:
#[cfg(not(target_arch = "wasm32"))]
fn main() -> Result<()> {
    color_eyre::install()?;

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

    // https://tokio.rs/tokio/topics/bridging
    // We want to run the gui in the main thread, but some long running tasks like
    // laoading VCDs should be done asynchronously. We can't just use std::thread to
    // do that due to wasm support, so we'll start a tokio runtime
    let runtime = tokio::runtime::Builder::new_current_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .unwrap();

    let _enter = runtime.enter();

    std::thread::spawn(move || {
        runtime.block_on(async {
            loop {
                tokio::time::sleep(tokio::time::Duration::from_secs(3600)).await;
            }
        })
    });

    let args = Args::parse();
    let mut state = State::new(StartupParams::from_args(args))?;

    let options = eframe::NativeOptions {
        initial_window_size: Some(egui::vec2(
            state.config.layout.window_width as f32,
            state.config.layout.window_height as f32,
        )),
        ..Default::default()
    };

    eframe::run_native(
        "Surfer",
        options,
        Box::new(|cc| {
            state.context = Some(cc.egui_ctx.clone());
            cc.egui_ctx.set_visuals(state.get_visuals());
            Box::new(state)
        }),
    )
    .unwrap();

    Ok(())
}

// When compiling to web using trunk:
#[cfg(target_arch = "wasm32")]
fn main() -> Result<()> {
    console_error_panic_hook::set_once();
    color_eyre::install()?;
    // Redirect `log` message to `console.log` and friends:
    eframe::WebLogger::init(log::LevelFilter::Debug).ok();

    let web_options = eframe::WebOptions::default();

    let url = wasm_util::vcd_from_url();

    let state = State::new(StartupParams::vcd_from_url(url))?;

    wasm_bindgen_futures::spawn_local(async {
        eframe::WebRunner::new()
            .start(
                "the_canvas_id", // hardcode it
                web_options,
                Box::new(|cc| {
                    cc.egui_ctx.set_visuals(state.get_visuals());
                    Box::new(state)
                }),
            )
            .await
            .expect("failed to start eframe");
    });

    Ok(())
}

pub struct WaveData {
    waveform: Waveform,
    source: WaveSource,
    active_module: Option<ScopeRef>,
    /// Root items (signals, dividers, ...) to display
    displayed_items: Vec<DisplayedItem>,
    viewport: Viewport,
    num_timestamps: BigInt,
    /// Name of the translator used to translate this trace
    signal_format: HashMap<FieldRef, String>,
    cursor: Option<BigInt>,
    cursors: HashMap<u8, BigInt>,
    focused_item: Option<usize>,
    default_signal_name_type: SignalNameType,
    scroll: usize,
}

impl WaveData {
    pub fn update_with(
        mut self,
        new_waves: Box<Waveform>,
        source: WaveSource,
        num_timestamps: BigInt,
        wave_viewport: Viewport,
        translators: &TranslatorList,
    ) -> WaveData {
        let active_module = self
            .active_module
            .take()
            .filter(|m| new_waves.module_exists(m));
        let display_items = self
            .displayed_items
            .drain(..)
            .filter(|i| match i {
                DisplayedItem::Signal(s) => new_waves.signal_exists(&s.signal_ref),
                DisplayedItem::Divider(_) => true,
                DisplayedItem::Cursor(_) => true,
            })
            .collect::<Vec<_>>();
        let mut nested_format = self
            .signal_format
            .iter()
            .filter(|&(field_ref, _)| !field_ref.field.is_empty())
            .map(|(x, y)| (x.clone(), y.clone()))
            .collect::<HashMap<_, _>>();
        let signal_format = self
            .signal_format
            .drain()
            .filter(|(field_ref, candidate)| {
                display_items.iter().any(|di| match di {
                    DisplayedItem::Signal(DisplayedSignal { signal_ref, .. }) => {
                        let Ok(meta) = new_waves.signal_meta(signal_ref) else {
                            return false;
                        };
                        field_ref.field.is_empty()
                            && *signal_ref == field_ref.root
                            && translators.is_valid_translator(&meta, candidate.as_str())
                    }
                    _ => false,
                })
            })
            .collect();
        let mut new_wave = WaveData {
            inner: *new_waves,
            source,
            active_module,
            displayed_items: display_items,
            viewport: self.viewport.clone().clip_to(&wave_viewport),
            signal_format,
            num_timestamps,
            cursor: self.cursor.clone(),
            cursors: self.cursors.clone(),
            focused_item: self.focused_item,
            default_signal_name_type: self.default_signal_name_type,
            scroll: self.scroll,
        };
        nested_format.retain(|nested, _| {
            let Some(signal_ref) = new_wave.displayed_items.iter().find_map(|di| match di {
                DisplayedItem::Signal(DisplayedSignal { signal_ref, .. }) => Some(signal_ref),
                _ => None,
            }) else {
                return false;
            };
            let meta = new_wave.inner.signal_meta(&nested.root).unwrap();
            new_wave
                .signal_translator(
                    &FieldRef {
                        root: signal_ref.clone(),
                        field: vec![],
                    },
                    translators,
                )
                .signal_info(&meta)
                .map(|info| info.has_subpath(&nested.field))
                .unwrap_or(false)
        });
        new_wave.signal_format.extend(nested_format);
        new_wave
    }
}

type CommandCount = usize;

#[derive(Debug)]
pub enum MoveDir {
    Up,
    Down,
}

pub enum ColorSpecifier {
    Index(usize),
    Name(String),
}

struct CachedDrawData {
    pub draw_commands: HashMap<FieldRef, signal_canvas::DrawingCommands>,
    pub clock_edges: Vec<f32>,
}

pub struct State {
    config: config::SurferConfig,
    waves: Option<WaveData>,
    /// Count argument for movements
    count: Option<String>,
    /// Which translator to use for each signal
    translators: TranslatorList,

    /// Receiver for messages generated by other threads
    msg_sender: Sender<Message>,
    msg_receiver: Receiver<Message>,

    /// The number of bytes loaded from the vcd file
    vcd_progress: Option<LoadProgress>,

    // Vector of translators which have failed at the `translates` function for a signal.
    blacklisted_translators: HashSet<(VarName, String)>,
    /// Buffer for the command input
    command_prompt: command_prompt::CommandPrompt,

    /// The context to egui, we need this to change the visual settings when the config is reloaded
    context: Option<eframe::egui::Context>,

    show_about: bool,
    show_keys: bool,
    show_gestures: bool,
    /// Hide the wave source. For now, this is only used in shapshot tests to avoid problems
    /// with absolute path diffs
    show_wave_source: bool,
    wanted_timescale: Timescale,
    gesture_start_location: Option<emath::Pos2>,
    show_url_entry: bool,
    signal_filter_focused: bool,
    signal_filter_type: SignalFilterType,
    rename_target: Option<usize>,

    /// The draw commands for every signal currently selected
    // For performance reasons, these need caching so we have them in a RefCell for interior
    // mutability
    draw_data: RefCell<Option<CachedDrawData>>,

    // Egui requires a place to store text field content between frames
    url: RefCell<String>,
    command_prompt_text: RefCell<String>,
    last_canvas_rect: RefCell<Option<Rect>>,
    signal_filter: RefCell<String>,
    item_renaming_string: RefCell<String>,
}

impl State {
    fn new(args: StartupParams) -> Result<State> {
        let (sender, receiver) = channel();

        // Basic translators that we can load quickly
        let translators = all_translators();

        // Long running translators which we load in a thread
        {
            let sender = sender.clone();
            perform_work(move || {
                if let (Some(top), Some(state)) = (args.spade_top, args.spade_state) {
                    let t = SpadeTranslator::new(&top, &state);
                    match t {
                        Ok(result) => sender
                            .send(Message::TranslatorLoaded(Box::new(result)))
                            .unwrap(),
                        Err(e) => sender.send(Message::Error(e)).unwrap(),
                    }
                } else {
                    info!("spade-top and spade-state not set, not loading spade translator");
                }
            });
        }

        // load config
        let config = config::SurferConfig::new().with_context(|| "Failed to load config file")?;
        let mut result = State {
            config,
            waves: None,
            count: None,
            translators,
            msg_sender: sender,
            msg_receiver: receiver,
            vcd_progress: None,
            blacklisted_translators: HashSet::new(),
            command_prompt: command_prompt::CommandPrompt {
                visible: false,
                expanded: String::from(""),
                suggestions: vec![],
            },
            context: None,
            show_about: false,
            show_keys: false,
            show_gestures: false,
            wanted_timescale: Timescale::Unit,
            gesture_start_location: None,
            show_url_entry: false,
            rename_target: None,
            show_wave_source: true,
            signal_filter_focused: false,
            signal_filter_type: SignalFilterType::Fuzzy,
            url: RefCell::new(String::new()),
            command_prompt_text: RefCell::new(String::new()),
            draw_data: RefCell::new(None),
            last_canvas_rect: RefCell::new(None),
            signal_filter: RefCell::new(String::new()),
            item_renaming_string: RefCell::new(String::new()),
        };

        match args.waves {
            Some(WaveSource::Url(url)) => result.load_vcd_from_url(url, false),
            Some(WaveSource::File(file)) => result.load_vcd_from_file(file, false).unwrap(),
            Some(WaveSource::DragAndDrop(_)) => {
                error!("Attempted to load from drag and drop at startup (how?)")
            }
            None => {}
        }

        Ok(result)
    }

    pub fn update(&mut self, message: Message) {
        match message {
            Message::SetActiveScope(module) => {
                let Some(waves) = self.waves.as_mut() else {
                    return;
                };
                if waves.inner.has_module(&module) {
                    waves.active_module = Some(module)
                } else {
                    warn!("Setting active scope to {module} which does not exist")
                }
            }
            Message::AddSignal(sig) => {
                self.invalidate_draw_commands();
                let Some(waves) = self.waves.as_mut() else {
                    return;
                };
                waves.add_signal(&self.translators, &sig)
            }
            Message::AddDivider(name) => {
                let Some(waves) = self.waves.as_mut() else {
                    return;
                };
                waves
                    .displayed_items
                    .push(DisplayedItem::Divider(DisplayedDivider {
                        color: None,
                        background_color: None,
                        name,
                    }));
            }
            Message::AddModule(module) => {
                let Some(waves) = self.waves.as_mut() else {
                    warn!("Adding module without waves loaded");
                    return;
                };

                let signals = waves.inner.signals_in_module(&module);
                for signal in signals {
                    waves.add_signal(&self.translators, &signal);
                }
                self.invalidate_draw_commands();
            }
            Message::AddCount(digit) => {
                if let Some(count) = &mut self.count {
                    count.push(digit);
                } else {
                    self.count = Some(digit.to_string())
                }
            }
            Message::InvalidateCount => self.count = None,
            Message::FocusItem(idx) => {
                let Some(waves) = self.waves.as_mut() else {
                    return;
                };

                let visible_signals_len = waves.displayed_items.len();
                if visible_signals_len > 0 && idx < visible_signals_len {
                    waves.focused_item = Some(idx);
                } else {
                    error!(
                        "Can not focus signal {idx} because only {visible_signals_len} signals are visible.",
                    );
                }
            }
            Message::UnfocusItem => {
                let Some(waves) = self.waves.as_mut() else {
                    return;
                };
                waves.focused_item = None;
            }
            Message::RenameItem(vidx) => {
                let Some(waves) = self.waves.as_mut() else {
                    return;
                };
                self.rename_target = Some(vidx);
                *self.item_renaming_string.borrow_mut() =
                    waves.displayed_items.get(vidx).unwrap().name();
            }
            Message::MoveFocus(direction, count) => {
                let Some(waves) = self.waves.as_mut() else {
                    return;
                };
                let visible_signals_len = waves.displayed_items.len();
                if visible_signals_len > 0 {
                    self.count = None;
                    match direction {
                        MoveDir::Up => {
                            waves.focused_item = waves
                                .focused_item
                                .map_or(Some(visible_signals_len - 1), |focused| {
                                    Some(focused - count.clamp(0, focused))
                                })
                        }
                        MoveDir::Down => {
                            waves.focused_item = waves.focused_item.map_or(
                                Some(waves.scroll + (count - 1).clamp(0, visible_signals_len - 1)),
                                |focused| Some((focused + count).clamp(0, visible_signals_len - 1)),
                            );
                        }
                    }
                }
            }
            Message::SetVerticalScroll(position) => {
                if let Some(waves) = &mut self.waves {
                    waves.scroll = position.clamp(0, waves.displayed_items.len() - 1);
                }
            }
            Message::VerticalScroll(direction, count) => {
                let Some(waves) = self.waves.as_mut() else {
                    return;
                };
                match direction {
                    MoveDir::Down => {
                        if waves.scroll + count < waves.displayed_items.len() {
                            waves.scroll += count;
                        } else {
                            waves.scroll = waves.displayed_items.len() - 1;
                        }
                    }
                    MoveDir::Up => {
                        if waves.scroll > count {
                            waves.scroll -= count;
                        } else {
                            waves.scroll = 0;
                        }
                    }
                }
            }
            Message::RemoveItem(idx, count) => {
                self.invalidate_draw_commands();

                let Some(waves) = self.waves.as_mut() else {
                    return;
                };
                for _ in 0..count {
                    let visible_signals_len = waves.displayed_items.len();
                    if let Some(DisplayedItem::Cursor(cursor)) = waves.displayed_items.get(idx) {
                        waves.cursors.remove(&cursor.idx);
                    }
                    if visible_signals_len > 0 && idx <= (visible_signals_len - 1) {
                        waves.displayed_items.remove(idx);
                        if let Some(focused) = waves.focused_item {
                            if focused == idx {
                                if (idx > 0) && (idx == (visible_signals_len - 1)) {
                                    // if the end of list is selected
                                    waves.focused_item = Some(idx - 1);
                                }
                            } else {
                                if idx < focused {
                                    waves.focused_item = Some(focused - 1)
                                }
                            }
                            if waves.displayed_items.is_empty() {
                                waves.focused_item = None;
                            }
                        }
                    }
                }
                waves.compute_signal_display_names();
            }
            Message::MoveFocusedItem(direction, count) => {
                self.invalidate_draw_commands();
                let Some(waves) = self.waves.as_mut() else {
                    return;
                };
                if let Some(idx) = waves.focused_item {
                    let visible_signals_len = waves.displayed_items.len();
                    if visible_signals_len > 0 {
                        match direction {
                            MoveDir::Up => {
                                for i in (idx
                                    .saturating_sub(count - 1)
                                    .clamp(1, visible_signals_len - 1)
                                    ..=idx)
                                    .rev()
                                {
                                    waves.displayed_items.swap(i, i - 1);
                                    waves.focused_item = Some(i - 1);
                                }
                            }
                            MoveDir::Down => {
                                for i in idx..(idx + count).clamp(0, visible_signals_len - 1) {
                                    waves.displayed_items.swap(i, i + 1);
                                    waves.focused_item = Some(i + 1);
                                }
                            }
                        }
                    }
                }
            }
            Message::CanvasScroll { delta } => {
                self.invalidate_draw_commands();
                self.handle_canvas_scroll(delta);
            }
            Message::CanvasZoom {
                delta,
                mouse_ptr_timestamp,
            } => {
                self.invalidate_draw_commands();
                self.waves
                    .as_mut()
                    .map(|waves| waves.handle_canvas_zoom(mouse_ptr_timestamp, delta as f64));
            }
            Message::ZoomToFit => {
                self.invalidate_draw_commands();
                self.zoom_to_fit();
            }
            Message::GoToEnd => {
                self.invalidate_draw_commands();
                self.go_to_end();
            }
            Message::GoToStart => {
                self.invalidate_draw_commands();
                self.go_to_start();
            }
            Message::SetTimeScale(timescale) => {
                self.invalidate_draw_commands();
                self.wanted_timescale = timescale;
            }
            Message::ZoomToRange { start, end } => {
                if let Some(waves) = &mut self.waves {
                    waves.viewport.curr_left = start;
                    waves.viewport.curr_right = end;
                }
                self.invalidate_draw_commands();
            }
            Message::SignalFormatChange(field, format) => {
                let Some(waves) = self.waves.as_mut() else {
                    return;
                };

                if self.translators.all_translator_names().contains(&&format) {
                    *waves.signal_format.entry(field.clone()).or_default() = format;

                    if field.field.is_empty() {
                        let Ok(meta) = waves
                            .inner
                            .signal_meta(&field.root)
                            .map_err(|e| warn!("{e:#?}"))
                        else {
                            return;
                        };
                        let translator = waves.signal_translator(&field, &self.translators);
                        let new_info = translator.signal_info(&meta).unwrap();

                        for item in &mut waves.displayed_items {
                            match item {
                                DisplayedItem::Signal(disp) => {
                                    if &disp.signal_ref == &field.root {
                                        disp.info = new_info;
                                        break;
                                    }
                                }
                                DisplayedItem::Cursor(_) => {}
                                DisplayedItem::Divider(_) => {}
                            }
                        }
                    }
                    self.invalidate_draw_commands();
                } else {
                    warn!("No translator {format}")
                }
            }
            Message::ItemColorChange(vidx, color_name) => {
                let Some(waves) = self.waves.as_mut() else {
                    return;
                };

                if let Some(idx) = vidx.or(waves.focused_item) {
                    waves.displayed_items[idx].set_color(color_name);
                };
            }
            Message::ItemNameChange(vidx, name) => {
                let Some(waves) = self.waves.as_mut() else {
                    return;
                };

                if let Some(idx) = vidx.or(waves.focused_item) {
                    waves.displayed_items[idx].set_name(name);
                };
            }
            Message::ItemBackgroundColorChange(vidx, color_name) => {
                let Some(waves) = self.waves.as_mut() else {
                    return;
                };

                if let Some(idx) = vidx.or(waves.focused_item) {
                    waves.displayed_items[idx].set_background_color(color_name)
                };
            }
            Message::ResetSignalFormat(idx) => {
                self.invalidate_draw_commands();
                self.waves
                    .as_mut()
                    .map(|waves| waves.signal_format.remove(&idx));
            }
            Message::CursorSet(new) => {
                if let Some(waves) = self.waves.as_mut() {
                    waves.cursor = Some(new)
                }
            }
            Message::LoadVcd(filename) => {
                self.load_vcd_from_file(filename, false).ok();
            }
            Message::LoadVcdFromUrl(url) => {
                self.load_vcd_from_url(url, false);
            }
            Message::FileDropped(dropped_file) => {
                self.load_vcd_from_dropped(dropped_file, false)
                    .map_err(|e| error!("{e:#?}"))
                    .ok();
            }
            Message::WavesLoaded(filename, new_waves, keep_signals) => {
                info!("VCD file loaded");
                let num_timestamps = new_waves
                    .max_timestamp()
                    .as_ref()
                    .map(|t| t.to_bigint().unwrap())
                    .unwrap_or(BigInt::from_u32(1).unwrap());
                let viewport = Viewport::new(0., num_timestamps.clone().to_f64().unwrap());

                let new_wave = if keep_signals && self.waves.is_some() {
                    self.waves.take().unwrap().update_with(
                        new_waves,
                        filename,
                        num_timestamps,
                        viewport,
                        &self.translators,
                    )
                } else {
                    WaveData {
                        inner: *new_waves,
                        source: filename,
                        active_module: None,
                        displayed_items: vec![],
                        viewport,
                        signal_format: HashMap::new(),
                        num_timestamps,
                        cursor: None,
                        cursors: HashMap::new(),
                        focused_item: None,
                        default_signal_name_type: self.config.default_signal_name_type,
                        scroll: 0,
                    }
                };
                self.invalidate_draw_commands();

                // Must clone timescale before consuming new_vcd
                self.wanted_timescale = new_wave.inner.metadata().timescale.1;
                self.waves = Some(new_wave);
                self.vcd_progress = None;
                info!("Done setting up VCD file");
            }
            Message::BlacklistTranslator(idx, translator) => {
                self.blacklisted_translators.insert((idx, translator));
            }
            Message::Error(e) => {
                error!("{e:?}")
            }
            Message::TranslatorLoaded(t) => {
                info!("Translator {} loaded", t.name());
                self.translators.add(t)
            }
            Message::ToggleSidePanel => {
                self.config.layout.show_hierarchy = !self.config.layout.show_hierarchy;
            }
            Message::ToggleMenu => self.config.layout.show_menu = !self.config.layout.show_menu,
            Message::ShowCommandPrompt(new_visibility) => {
                if !new_visibility {
                    *self.command_prompt_text.borrow_mut() = "".to_string();
                    self.command_prompt.suggestions = vec![];
                    self.command_prompt.expanded = "".to_string();
                }
                self.command_prompt.visible = new_visibility;
            }
            Message::FileDownloaded(url, bytes, keep_signals) => {
                let size = bytes.len() as u64;
                self.load_vcd_from_bytes(
                    WaveSource::Url(url),
                    bytes.as_ref(),
                    Some(size),
                    keep_signals,
                )
            }
            Message::ReloadConfig => {
                // FIXME think about a structured way to collect errors
                if let Ok(config) =
                    SurferConfig::new().with_context(|| "Failed to load config file")
                {
                    self.config = config;
                    if let Some(ctx) = &self.context {
                        ctx.set_visuals(self.get_visuals())
                    }
                }
            }
            Message::ReloadWaveform => {
                let Some(waves) = &self.waves else { return };
                match &waves.source {
                    WaveSource::File(filename) => {
                        self.load_vcd_from_file(filename.clone(), true).ok()
                    }
                    WaveSource::DragAndDrop(filename) => filename
                        .clone()
                        .and_then(|filename| self.load_vcd_from_file(filename, true).ok()),
                    WaveSource::Url(url) => {
                        self.load_vcd_from_url(url.clone(), true);
                        Some(())
                    }
                };
            }
            Message::SetClockHighlightType(new_type) => {
                self.config.default_clock_highlight_type = new_type
            }
            Message::SetCursorPosition(idx) => {
                let Some(waves) = self.waves.as_mut() else {
                    return;
                };
                let Some(location) = &waves.cursor else {
                    return;
                };
                if waves
                    .displayed_items
                    .iter()
                    .find_map(|item| match item {
                        DisplayedItem::Cursor(cursor) => {
                            if cursor.idx == idx {
                                Some(cursor)
                            } else {
                                None
                            }
                        }
                        _ => None,
                    })
                    .is_none()
                {
                    let cursor = DisplayedCursor {
                        color: None,
                        background_color: None,
                        name: format!("Cursor"),
                        idx,
                    };
                    waves.displayed_items.push(DisplayedItem::Cursor(cursor));
                }
                waves.cursors.insert(idx, location.clone());
            }
            Message::GoToCursorPosition(idx) => {
                let Some(waves) = self.waves.as_ref() else {
                    return;
                };

                if let Some(cursor) = waves.cursors.get(&idx) {
                    self.go_to_time(&cursor.clone());
                    self.invalidate_draw_commands();
                }
            }
            Message::ChangeSignalNameType(vidx, name_type) => {
                let Some(waves) = self.waves.as_mut() else {
                    return;
                };
                // checks if vidx is Some then use that, else try focused signal
                if let Some(idx) = vidx.or(waves.focused_item) {
                    if waves.displayed_items.len() > idx {
                        if let DisplayedItem::Signal(signal) = &mut waves.displayed_items[idx] {
                            signal.display_name_type = name_type;
                            waves.compute_signal_display_names();
                        }
                    }
                }
            }
            Message::ForceSignalNameTypes(name_type) => {
                let Some(vcd) = self.waves.as_mut() else {
                    return;
                };
                for signal in &mut vcd.displayed_items {
                    if let DisplayedItem::Signal(signal) = signal {
                        signal.display_name_type = name_type;
                    }
                }
                vcd.default_signal_name_type = name_type;
                vcd.compute_signal_display_names();
            }
            Message::CommandPromptClear => {
                *self.command_prompt_text.borrow_mut() = "".to_string();
                self.command_prompt.expanded = "".to_string();
                self.command_prompt.suggestions = vec![];
            }
            Message::CommandPromptUpdate {
                expanded,
                suggestions,
            } => {
                self.command_prompt.expanded = expanded;
                self.command_prompt.suggestions = suggestions;
            }
            Message::OpenFileDialog(mode) => {
                self.open_file_dialog(mode);
            }
            Message::SetAboutVisible(s) => self.show_about = s,
            Message::SetKeyHelpVisible(s) => self.show_keys = s,
            Message::SetGestureHelpVisible(s) => self.show_gestures = s,
            Message::SetUrlEntryVisible(s) => self.show_url_entry = s,
            Message::SetRenameItemVisible(_) => self.rename_target = None,
            Message::SetDragStart(pos) => self.gesture_start_location = pos,
            Message::SetFilterFocused(s) => self.signal_filter_focused = s,
            Message::SetSignalFilterType(signal_filter_type) => {
                self.signal_filter_type = signal_filter_type
            }
            Message::Exit | Message::ToggleFullscreen => {} // Handled in eframe::update
        }
    }

    fn handle_async_messages(&mut self) {
        let mut msgs = vec![];
        loop {
            match self.msg_receiver.try_recv() {
                Ok(msg) => msgs.push(msg),
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    trace!("Message sender disconnected");
                    break;
                }
            }
        }

        while let Some(msg) = msgs.pop() {
            self.update(msg);
        }
    }

    pub fn handle_canvas_scroll(
        &mut self,
        // Canvas relative
        delta: Vec2,
    ) {
        if let Some(waves) = &mut self.waves {
            // Scroll 5% of the viewport per scroll event.
            // One scroll event yields 50
            let scroll_step = -(waves.viewport.curr_right - waves.viewport.curr_left) / (50. * 20.);

            let target_left = &waves.viewport.curr_left + scroll_step * delta.y as f64;
            let target_right = &waves.viewport.curr_right + scroll_step * delta.y as f64;

            waves.viewport.curr_left = target_left;
            waves.viewport.curr_right = target_right;
        }
    }

    pub fn go_to_start(&mut self) {
        if let Some(waves) = &mut self.waves {
            let width = waves.viewport.curr_right - waves.viewport.curr_left;

            waves.viewport.curr_left = 0.0;
            waves.viewport.curr_right = width;
        }
    }

    pub fn go_to_end(&mut self) {
        if let Some(waves) = &mut self.waves {
            let end_point = waves.num_timestamps.clone().to_f64().unwrap();
            let width = waves.viewport.curr_right - waves.viewport.curr_left;

            waves.viewport.curr_left = end_point - width;
            waves.viewport.curr_right = end_point;
        }
    }

    pub fn go_to_time(&mut self, center: &BigInt) {
        if let Some(waves) = &mut self.waves {
            let center_point = center.to_f64().unwrap();
            let half_width = (waves.viewport.curr_right - waves.viewport.curr_left) / 2.;

            waves.viewport.curr_left = center_point - half_width;
            waves.viewport.curr_right = center_point + half_width;
        }
    }

    pub fn zoom_to_fit(&mut self) {
        if let Some(waves) = &mut self.waves {
            waves.viewport.curr_left = 0.0;
            waves.viewport.curr_right = waves.num_timestamps.clone().to_f64().unwrap();
        }
    }

    pub fn get_visuals(&self) -> Visuals {
        let widget_style = WidgetVisuals {
            bg_fill: self.config.theme.secondary_ui_color.background,
            fg_stroke: Stroke {
                color: self.config.theme.secondary_ui_color.foreground,
                width: 1.0,
            },
            weak_bg_fill: self.config.theme.secondary_ui_color.background,
            bg_stroke: Stroke {
                color: self.config.theme.border_color,
                width: 1.0,
            },
            rounding: Rounding::same(2.),
            expansion: 0.0,
        };

        Visuals {
            override_text_color: Some(self.config.theme.foreground),
            extreme_bg_color: self.config.theme.secondary_ui_color.background,
            panel_fill: self.config.theme.secondary_ui_color.background,
            window_fill: self.config.theme.primary_ui_color.background,
            window_rounding: Rounding::ZERO,
            menu_rounding: Rounding::ZERO,
            window_stroke: Stroke {
                width: 1.0,
                color: self.config.theme.border_color,
            },
            selection: Selection {
                bg_fill: self.config.theme.selected_elements_colors.background,
                stroke: Stroke {
                    color: self.config.theme.selected_elements_colors.foreground,
                    width: 1.0,
                },
            },
            widgets: Widgets {
                noninteractive: widget_style,
                inactive: widget_style,
                hovered: widget_style,
                active: widget_style,
                open: widget_style,
                ..Default::default()
            },
            ..Visuals::dark()
        }
    }
}

impl WaveData {
    pub fn select_preferred_translator(
        &self,
        hierarchy: &Hierarchy,
        var: &Var,
        translators: &TranslatorList,
    ) -> String {
        translators
            .all_translators()
            .iter()
            .filter_map(|t| match t.translates(hierarchy, var) {
                Ok(TranslationPreference::Prefer) => Some(t.name()),
                Ok(TranslationPreference::Yes) => None,
                Ok(TranslationPreference::No) => None,
                Err(e) => {
                    error!(
                        "Failed to check if {} translates {}\n{e:#?}",
                        t.name(),
                        sig.sig.full_path_string()
                    );
                    None
                }
            })
            .next()
            .unwrap_or(translators.default.clone())
    }

    pub fn signal_translator<'a>(
        &'a self,
        field: &FieldRef,
        translators: &'a TranslatorList,
    ) -> &'a dyn Translator {
        let translator_name = self.signal_format.get(&field).cloned().unwrap_or_else(|| {
            if field.field.is_empty() {
                self.inner
                    .signal_meta(&field.root)
                    .map(|meta| self.select_preferred_translator(meta, translators))
                    .unwrap_or_else(|e| {
                        warn!("{e:#?}");
                        translators.default.clone()
                    })
            } else {
                translators.default.clone()
            }
        });
        let translator = translators.get_translator(&translator_name);
        translator
    }

    pub fn handle_canvas_zoom(
        &mut self,
        // Canvas relative
        mouse_ptr_timestamp: Option<f64>,
        delta: f64,
    ) {
        // Zoom or scroll
        let Viewport {
            curr_left: left,
            curr_right: right,
            ..
        } = &self.viewport;

        let (target_left, target_right) = match mouse_ptr_timestamp {
            Some(mouse_location) => (
                (left - mouse_location) / delta + mouse_location,
                (right - mouse_location) / delta + mouse_location,
            ),
            None => {
                let mid_point = (right + left) * 0.5;
                let offset = (right - left) * delta * 0.5;

                (mid_point - offset, mid_point + offset)
            }
        };

        self.viewport.curr_left = target_left;
        self.viewport.curr_right = target_right;
    }

    pub fn add_signal(&mut self, translators: &TranslatorList, sig: &VarName) {
        let Ok(meta) = self
            .inner
            .signal_meta(&sig)
            .context("When adding signal")
            .map_err(|e| error!("{e:#?}"))
        else {
            return;
        };

        let translator =
            self.signal_translator(&FieldRef::without_fields(sig.clone()), translators);
        let info = translator.signal_info(&meta).unwrap();

        self.displayed_items
            .push(DisplayedItem::Signal(DisplayedSignal {
                signal_ref: sig.clone(),
                info,
                color: None,
                background_color: None,
                display_name: sig.name.clone(),
                display_name_type: self.default_signal_name_type,
            }));
        self.compute_signal_display_names();
    }
}
