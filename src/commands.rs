use std::collections::BTreeMap;
use std::{fs, str::FromStr};

use crate::{
    clock_highlighting::ClockHighlightType,
    displayed_item::DisplayedItem,
    message::Message,
    signal_name_type::SignalNameType,
    util::{alpha_idx_to_uint_idx, uint_idx_to_alpha_idx},
    wave_container::{ScopeName, VarName},
    State,
};

use fzcmd::{expand_command, Command, FuzzyOutput, ParamGreed};
use itertools::Itertools;

pub fn get_parser(state: &State) -> Command<Message> {
    fn single_word(
        suggestions: Vec<String>,
        rest_command: Box<dyn Fn(&str) -> Option<Command<Message>>>,
    ) -> Option<Command<Message>> {
        Some(Command::NonTerminal(
            ParamGreed::Rest,
            suggestions,
            Box::new(move |query, _| rest_command(query)),
        ))
    }

    fn single_word_delayed_suggestions(
        suggestions: Box<dyn Fn() -> Vec<String>>,
        rest_command: Box<dyn Fn(&str) -> Option<Command<Message>>>,
    ) -> Option<Command<Message>> {
        Some(Command::NonTerminal(
            ParamGreed::Rest,
            suggestions(),
            Box::new(move |query, _| rest_command(query)),
        ))
    }

    let hierarchy = state.waves.map(|v| v.waveform.hierarchy());

    let modules = match hierarchy {
        Some(h) => h
            .iter_scopes()
            .map(|scope| scope.full_name(h))
            .collect(),
        None => vec![],
    };
    let signals = match hierarchy {
        Some(h) => h
            .iter_vars()
            .map(|var| var.full_name(h))
            .collect(),
        None => vec![],
    };
    let displayed_signals = match &state.waves {
        Some(v) => v
            .displayed_items
            .iter()
            .filter_map(|item| match item {
                DisplayedItem::Signal(idx) => Some(idx),
                _ => None,
            })
            .enumerate()
            .map(|(idx, s)| {
                format!(
                    "{}_{}",
                    uint_idx_to_alpha_idx(idx, v.displayed_items.len()),
                    s.signal_ref.full_path_string()
                )
            })
            .collect_vec(),
        None => vec![],
    };
    let signals_in_active_scope = state
        .waves
        .as_ref()
        .and_then(|waves| {
            match (waves.active_module, hierarchy) {
                (Some(scope), Some(h)) => Some(scope.vars()),
                _ => None
            }
        })
        .unwrap_or_default();

    let color_names = state.config.theme.colors.keys().cloned().collect_vec();

    let active_module = state.waves.as_ref().and_then(|w| w.active_module.clone());

    fn vcd_files() -> Vec<String> {
        if let Ok(res) = fs::read_dir(".") {
            res.map(|res| res.map(|e| e.path()).unwrap_or_default())
                .filter(|file| {
                    file.extension()
                        .map_or(false, |extension| extension.to_str().unwrap_or("") == "vcd")
                })
                .map(|file| file.into_os_string().into_string().unwrap())
                .collect::<Vec<String>>()
        } else {
            vec![]
        }
    }

    let cursors = if let Some(waves) = &state.waves {
        waves
            .displayed_items
            .iter()
            .filter_map(|item| match item {
                DisplayedItem::Cursor(tmp_cursor) => Some(tmp_cursor),
                _ => None,
            })
            .map(|cursor| (cursor.name.clone(), cursor.idx))
            .collect::<BTreeMap<_, _>>()
    } else {
        BTreeMap::new()
    };

    Command::NonTerminal(
        ParamGreed::Word,
        vec![
            "load_vcd",
            "load_url",
            "config_reload",
            "scroll_to_start",
            "scroll_to_end",
            "goto_start",
            "goto_end",
            "zoom_in",
            "zoom_out",
            "zoom_fit",
            "toggle_menu",
            "toggle_fullscreen",
            "module_add",
            "module_select",
            "reload",
            "signal_add",
            "signal_add_from_module",
            "signal_set_color",
            "signal_set_name_type",
            "signal_force_name_type",
            "signal_focus",
            "signal_unfocus",
            "signal_unset_color",
            "preference_set_clock_highlight",
            "divider_add",
            "goto_cursor",
        ]
        .into_iter()
        .map(|s| s.into())
        .collect(),
        Box::new(move |query, _| {
            let signals_in_active_scope = signals_in_active_scope.clone();
            let cursors = cursors.clone();
            let modules = modules.clone();
            let active_module = active_module.clone();
            match query {
                "load_vcd" => single_word_delayed_suggestions(
                    Box::new(vcd_files),
                    Box::new(|word| Some(Command::Terminal(Message::LoadVcd(word.into())))),
                ),
                "load_url" => Some(Command::NonTerminal(
                    ParamGreed::Rest,
                    vec![],
                    Box::new(|query, _| {
                        Some(Command::Terminal(Message::LoadVcdFromUrl(
                            query.to_string(),
                        )))
                    }),
                )),
                "config_reload" => Some(Command::Terminal(Message::ReloadConfig)),
                "scroll_to_start" | "goto_start" => Some(Command::Terminal(Message::GoToStart)),
                "scroll_to_end" | "goto_end" => Some(Command::Terminal(Message::GoToEnd)),
                "zoom_in" => Some(Command::Terminal(Message::CanvasZoom {
                    mouse_ptr_timestamp: None,
                    delta: 0.5,
                })),
                "zoom_out" => Some(Command::Terminal(Message::CanvasZoom {
                    mouse_ptr_timestamp: None,
                    delta: 2.0,
                })),
                "zoom_fit" => Some(Command::Terminal(Message::ZoomToFit)),
                "toggle_menu" => Some(Command::Terminal(Message::ToggleMenu)),
                "toggle_fullscreen" => Some(Command::Terminal(Message::ToggleFullscreen)),
                // Module commands
                "module_add" => single_word(
                    modules,
                    Box::new(|word| {
                        Some(Command::Terminal(Message::AddModule(
                            ScopeName::from_hierarchy_string(word),
                        )))
                    }),
                ),
                "module_select" => single_word(
                    modules.clone(),
                    Box::new(|word| {
                        Some(Command::Terminal(Message::SetActiveScope(
                            ScopeName::from_hierarchy_string(word),
                        )))
                    }),
                ),
                "reload" => Some(Command::Terminal(Message::ReloadWaveform)),
                // Signal commands
                "signal_add" => single_word(
                    signals.clone(),
                    Box::new(|word| {
                        Some(Command::Terminal(Message::AddSignal(
                            VarName::from_hierarchy_string(word),
                        )))
                    }),
                ),
                "signal_add_from_module" => single_word(
                    signals_in_active_scope
                        .into_iter()
                        .map(|s| s.name)
                        .collect(),
                    Box::new(move |name| {
                        active_module.as_ref().map(|module| {
                            Command::Terminal(Message::AddSignal(VarName::new(
                                module.clone(),
                                name.to_string(),
                            )))
                        })
                    }),
                ),
                "signal_set_color" => single_word(
                    color_names.clone(),
                    Box::new(|word| {
                        Some(Command::Terminal(Message::ItemColorChange(
                            None,
                            Some(word.to_string()),
                        )))
                    }),
                ),
                "signal_unset_color" => {
                    Some(Command::Terminal(Message::ItemColorChange(None, None)))
                }
                "signal_set_name_type" => single_word(
                    vec![
                        "Local".to_string(),
                        "Unique".to_string(),
                        "Global".to_string(),
                    ],
                    Box::new(|word| {
                        Some(Command::Terminal(Message::ChangeSignalNameType(
                            None,
                            SignalNameType::from_str(word).unwrap_or(SignalNameType::Local),
                        )))
                    }),
                ),
                "signal_force_name_type" => single_word(
                    vec![
                        "Local".to_string(),
                        "Unique".to_string(),
                        "Global".to_string(),
                    ],
                    Box::new(|word| {
                        Some(Command::Terminal(Message::ForceSignalNameTypes(
                            SignalNameType::from_str(word).unwrap_or(SignalNameType::Local),
                        )))
                    }),
                ),
                "signal_focus" => single_word(
                    displayed_signals.clone(),
                    Box::new(|word| {
                        // split off the idx which is always followed by an underscore
                        let alpha_idx: String = word.chars().take_while(|c| *c != '_').collect();
                        alpha_idx_to_uint_idx(alpha_idx)
                            .map(|idx| Command::Terminal(Message::FocusItem(idx)))
                    }),
                ),
                "preference_set_clock_highlight" => single_word(
                    ["Line", "Cycle", "None"]
                        .iter()
                        .map(|o| o.to_string())
                        .collect_vec(),
                    Box::new(|word| {
                        Some(Command::Terminal(Message::SetClockHighlightType(
                            ClockHighlightType::from_str(word).unwrap_or(ClockHighlightType::Line),
                        )))
                    }),
                ),
                "signal_unfocus" => Some(Command::Terminal(Message::UnfocusItem)),
                "divider_add" => single_word(
                    vec![],
                    Box::new(|word| Some(Command::Terminal(Message::AddDivider(word.into())))),
                ),
                "goto_cursor" => single_word(
                    cursors.keys().cloned().collect(),
                    Box::new(move |name| {
                        cursors
                            .get(name)
                            .map(|idx| Command::Terminal(Message::GoToCursorPosition(*idx)))
                    }),
                ),
                _ => None,
            }
        }),
    )
}

pub fn run_fuzzy_parser(input: &str, state: &State, msgs: &mut Vec<Message>) {
    let FuzzyOutput {
        expanded,
        suggestions,
    } = expand_command(input, get_parser(state));

    msgs.push(Message::CommandPromptUpdate {
        expanded,
        suggestions: suggestions.unwrap_or(vec![]),
    })
}
