use std::collections::HashMap;

use color_eyre::eyre::Context;
use eframe::egui::{self, Sense};
use eframe::emath::{self, Align2};
use eframe::epaint::{Color32, FontId, PathShape, Pos2, Rect, RectShape, Rounding, Stroke, Vec2};
use log::{error, warn};
use num::BigRational;
use num::ToPrimitive;

use crate::benchmark::{TimedRegion, TranslationTimings};
use crate::config::SurferTheme;
use crate::translation::{SignalInfo, ValueKind};
use crate::view::{DrawConfig, DrawingContext, ItemDrawingInfo};
use crate::wave_container::FieldRef;
use crate::{displayed_item::DisplayedItem, CachedDrawData, Message, State};

pub struct DrawnRegion {
    inner: Option<(String, ValueKind)>,
    /// True if a transition should be drawn even if there is no change in the value
    /// between the previous and next pixels. Only used by the bool drawing logic to
    /// draw draw a vertical line and prevent apparent aliasing
    force_anti_alias: bool,
}

/// List of values to draw for a signal. It is an ordered list of values that should
/// be drawn at the *start time* until the *start time* of the next value
pub struct DrawingCommands {
    is_bool: bool,
    values: Vec<(f32, DrawnRegion)>,
}

impl DrawingCommands {
    pub fn new_bool() -> Self {
        Self {
            values: vec![],
            is_bool: true,
        }
    }

    pub fn new_wide() -> Self {
        Self {
            values: vec![],
            is_bool: false,
        }
    }

    pub fn push(&mut self, val: (f32, DrawnRegion)) {
        self.values.push(val)
    }
}

impl State {
    pub fn invalidate_draw_commands(&mut self) {
        *self.draw_data.borrow_mut() = None;
    }

    pub fn generate_draw_commands(&self, cfg: &DrawConfig, width: f32, msgs: &mut Vec<Message>) {
        let mut draw_commands = HashMap::new();
        if let Some(waves) = &self.waves {
            let frame_width = width;
            let max_time = BigRational::from_integer(waves.num_timestamps.clone());
            let mut timings = TranslationTimings::new();
            let mut clock_edges = vec![];
            // Compute which timestamp to draw in each pixel. We'll draw from -transition_width to
            // width + transition_width in order to draw initial transitions outside the screen
            let timestamps = (-cfg.max_transition_width
                ..(frame_width as i32 + cfg.max_transition_width))
                .filter_map(|x| {
                    let time = waves.viewport.to_time(x as f64, frame_width);
                    if time < BigRational::from_float(0.).unwrap() || time > max_time {
                        None
                    } else {
                        Some((x as f32, time.to_integer().to_biguint().unwrap()))
                    }
                })
                .collect::<Vec<_>>();

            waves
                .displayed_items
                .iter()
                .filter_map(|item| match item {
                    DisplayedItem::Signal(signal_ref) => Some(signal_ref),
                    _ => None,
                })
                // Iterate over the signals, generating draw commands for all the
                // subfields
                .for_each(|displayed_signal| {
                    let meta = match waves
                        .inner
                        .signal_meta(&displayed_signal.signal_ref)
                        .context("failed to get signal meta")
                    {
                        Ok(meta) => meta,
                        Err(e) => {
                            warn!("{e:#?}");
                            return;
                        }
                    };

                    let translator = waves.signal_translator(
                        &FieldRef {
                            root: displayed_signal.signal_ref.clone(),
                            field: vec![],
                        },
                        &self.translators,
                    );
                    // we need to get the signal info here to get the correct info for aliases
                    let info = translator.signal_info(&meta).unwrap();

                    let mut local_commands: HashMap<Vec<_>, _> = HashMap::new();

                    let mut prev_values = HashMap::new();

                    // In order to insert a final draw command at the end of a trace,
                    // we need to know if this is the last timestamp to draw
                    let end_pixel = timestamps.iter().last().map(|t| t.0).unwrap_or_default();
                    // The first pixel we actually draw is the second pixel in the
                    // list, since we skip one pixel to have a previous value
                    let start_pixel = timestamps
                        .iter()
                        .skip(1)
                        .next()
                        .map(|t| t.0)
                        .unwrap_or_default();

                    // Iterate over all the time stamps to draw on
                    for ((_, prev_time), (pixel, time)) in
                        timestamps.iter().zip(timestamps.iter().skip(1))
                    {
                        let (change_time, val) =
                            match waves.inner.query_signal(&displayed_signal.signal_ref, time) {
                                Ok(Some(val)) => val,
                                Ok(None) => continue,
                                Err(e) => {
                                    error!("Signal query error {e:#?}");
                                    continue;
                                }
                            };

                        let is_last_timestep = pixel == &end_pixel;
                        let is_first_timestep = pixel == &start_pixel;

                        // Check if the value remains unchanged between this pixel
                        // and the last
                        if &change_time < prev_time && !is_first_timestep && !is_last_timestep {
                            continue;
                        }

                        // Perform the translation
                        let mut duration = TimedRegion::started();

                        let translation_result = match translator.translate(&meta, &val) {
                            Ok(result) => result,
                            Err(e) => {
                                error!(
                                    "{translator_name} for {sig_name} failed. Disabling:",
                                    translator_name = translator.name(),
                                    sig_name = displayed_signal.signal_ref.full_path_string()
                                );
                                error!("{e:#}");
                                msgs.push(Message::ResetSignalFormat(FieldRef {
                                    root: displayed_signal.signal_ref.clone(),
                                    field: vec![],
                                }));
                                return;
                            }
                        };

                        duration.stop();
                        timings.push_timing(&translator.name(), None, duration.secs());
                        let fields = translation_result
                            .flatten(
                                FieldRef {
                                    root: displayed_signal.signal_ref.clone(),
                                    field: vec![],
                                },
                                &waves.signal_format,
                                &self.translators,
                            )
                            .as_fields();

                        for (path, value) in fields {
                            let prev = prev_values.get(&path);

                            // If the value changed between this and the previous pixel, we want to
                            // draw a transition even if the translated value didn't change.  We
                            // only want to do this for root signals, because resolving when a
                            // sub-field change is tricky without more information from the
                            // translators
                            let anti_alias = &change_time > prev_time && path.is_empty();
                            let new_value = prev != Some(&value);

                            // This is not the value we drew last time
                            if new_value || is_last_timestep || anti_alias {
                                *prev_values.entry(path.clone()).or_insert(value.clone()) =
                                    value.clone();

                                if let SignalInfo::Clock = info.get_subinfo(&path) {
                                    match value.as_ref().map(|(val, _)| val.as_str()) {
                                        Some("1") => {
                                            if !is_last_timestep && !is_first_timestep {
                                                clock_edges.push(*pixel)
                                            }
                                        }
                                        Some(_) => {}
                                        None => {}
                                    }
                                }

                                local_commands
                                    .entry(path.clone())
                                    .or_insert_with(|| {
                                        if let SignalInfo::Bool | SignalInfo::Clock =
                                            info.get_subinfo(&path)
                                        {
                                            DrawingCommands::new_bool()
                                        } else {
                                            DrawingCommands::new_wide()
                                        }
                                    })
                                    .push((
                                        *pixel,
                                        DrawnRegion {
                                            inner: value,
                                            force_anti_alias: anti_alias && !new_value,
                                        },
                                    ))
                            }
                        }
                    }
                    // Append the signal index to the fields
                    local_commands.into_iter().for_each(|(path, val)| {
                        draw_commands.insert(
                            FieldRef {
                                root: displayed_signal.signal_ref.clone(),
                                field: path.clone(),
                            },
                            val,
                        );
                    });
                });

            *self.draw_data.borrow_mut() = Some(CachedDrawData {
                draw_commands,
                clock_edges,
            });
        }
    }

    pub fn draw_signals(
        &self,
        msgs: &mut Vec<Message>,
        item_offsets: &Vec<ItemDrawingInfo>,
        ui: &mut egui::Ui,
    ) {
        let (response, mut painter) = ui.allocate_painter(ui.available_size(), Sense::drag());

        let cfg = DrawConfig {
            canvas_height: response.rect.size().y,
            line_height: 16.,
            max_transition_width: 6,
        };
        // the draw commands have been invalidated, recompute
        if self.draw_data.borrow().is_none()
            || Some(response.rect) != *self.last_canvas_rect.borrow()
        {
            self.generate_draw_commands(&cfg, response.rect.width(), msgs);
            *self.last_canvas_rect.borrow_mut() = Some(response.rect);
        }

        let Some(vcd) = &self.waves else { return };
        let container_rect = Rect::from_min_size(Pos2::ZERO, response.rect.size());
        let to_screen = emath::RectTransform::from_to(container_rect, response.rect);
        let frame_width = response.rect.width();
        let pointer_pos_global = ui.input(|i| i.pointer.interact_pos());
        let pointer_pos_canvas = pointer_pos_global.map(|p| to_screen.inverse().transform_pos(p));

        if ui.ui_contains_pointer() {
            let pointer_pos = pointer_pos_global.unwrap();
            let scroll_delta = ui.input(|i| i.scroll_delta);
            let mouse_ptr_pos = to_screen.inverse().transform_pos(pointer_pos);
            if scroll_delta != Vec2::ZERO {
                msgs.push(Message::CanvasScroll {
                    delta: ui.input(|i| i.scroll_delta),
                })
            }

            if ui.input(|i| i.zoom_delta()) != 1. {
                let mouse_ptr_timestamp = vcd
                    .viewport
                    .to_time(mouse_ptr_pos.x as f64, frame_width)
                    .to_f64();

                msgs.push(Message::CanvasZoom {
                    mouse_ptr_timestamp,
                    delta: ui.input(|i| i.zoom_delta()),
                })
            }
        }

        response.dragged_by(egui::PointerButton::Primary).then(|| {
            let x = pointer_pos_canvas.unwrap().x;
            let timestamp = vcd.viewport.to_time(x as f64, frame_width);
            msgs.push(Message::CursorSet(timestamp.round().to_integer()));
        });

        painter.rect_filled(
            response.rect,
            Rounding::ZERO,
            self.config.theme.canvas_colors.background,
        );

        response
            .drag_started_by(egui::PointerButton::Middle)
            .then(|| msgs.push(Message::SetDragStart(pointer_pos_canvas)));

        let mut ctx = DrawingContext {
            painter: &mut painter,
            cfg: &cfg,
            // This 0.5 is very odd, but it fixes the lines we draw being smushed out across two
            // pixels, resulting in dimmer colors https://github.com/emilk/egui/issues/1322
            to_screen: &|x, y| to_screen.transform_pos(Pos2::new(x, y) + Vec2::new(0.5, 0.5)),
            theme: &self.config.theme,
        };

        let gap = self.get_item_gap(item_offsets, &ctx);
        for (idx, drawing_info) in item_offsets.iter().enumerate() {
            let default_background_color = self.get_default_alternating_background_color(idx);
            let background_color = *vcd
                .displayed_items
                .get(drawing_info.signal_list_idx())
                .and_then(|signal| signal.background_color())
                .and_then(|color| self.config.theme.colors.get(&color))
                .unwrap_or(&default_background_color);

            // We draw in absolute coords, but the signal offset in the y
            // direction is also in absolute coordinates, so we need to
            // compensate for that
            let y_offset = drawing_info.offset() - to_screen.transform_pos(Pos2::ZERO).y;
            let min = (ctx.to_screen)(0.0, y_offset - gap);
            let max = (ctx.to_screen)(frame_width, y_offset + ctx.cfg.line_height + gap);
            ctx.painter
                .rect_filled(Rect { min, max }, Rounding::ZERO, background_color);
        }

        self.draw_mouse_gesture_widget(vcd, pointer_pos_canvas, &response, msgs, &mut ctx);

        if let Some(draw_data) = &*self.draw_data.borrow() {
            let clock_edges = &draw_data.clock_edges;
            let draw_commands = &draw_data.draw_commands;
            let draw_clock_edges = match clock_edges.as_slice() {
                [] => false,
                [_single] => true,
                [first, second, ..] => second - first > 15.,
            };

            if draw_clock_edges {
                let mut last_edge = 0.0;
                let mut cycle = false;
                for current_edge in clock_edges {
                    self.draw_clock_edge(last_edge, *current_edge, cycle, &mut ctx);
                    cycle = !cycle;
                    last_edge = *current_edge;
                }
            }

            for drawing_info in item_offsets {
                // We draw in absolute coords, but the signal offset in the y
                // direction is also in absolute coordinates, so we need to
                // compensate for that
                let y_offset = drawing_info.offset() - to_screen.transform_pos(Pos2::ZERO).y;

                let color = *vcd
                    .displayed_items
                    .get(drawing_info.signal_list_idx())
                    .and_then(|signal| signal.color())
                    .and_then(|color| self.config.theme.colors.get(&color))
                    .unwrap_or(&self.config.theme.signal_default);
                match drawing_info {
                    ItemDrawingInfo::Signal(drawing_info) => {
                        if let Some(commands) = draw_commands.get(&drawing_info.field_ref) {
                            for (old, new) in
                                commands.values.iter().zip(commands.values.iter().skip(1))
                            {
                                if commands.is_bool {
                                    self.draw_bool_transition(
                                        (old, new),
                                        new.1.force_anti_alias,
                                        color,
                                        y_offset,
                                        &mut ctx,
                                    )
                                } else {
                                    self.draw_region((old, new), color, y_offset, &mut ctx)
                                }
                            }
                        }
                    }
                    ItemDrawingInfo::Divider(_) => {}
                    ItemDrawingInfo::Cursor(_) => {}
                }
            }
        }

        vcd.draw_cursor(
            &self.config.theme,
            &mut ctx,
            response.rect.size(),
            to_screen,
        );

        vcd.draw_cursors(
            &self.config.theme,
            &mut ctx,
            response.rect.size(),
            to_screen,
        );

        self.draw_cursor_boxes(ctx, item_offsets, to_screen, vcd, response, gap);
    }

    fn draw_region(
        &self,
        ((old_x, prev_region), (new_x, _)): (&(f32, DrawnRegion), &(f32, DrawnRegion)),
        user_color: Color32,
        offset: f32,
        ctx: &mut DrawingContext,
    ) {
        if let Some((prev_value, color)) = &prev_region.inner {
            let stroke = Stroke {
                color: color.color(user_color, ctx.theme),
                width: self.config.theme.linewidth,
            };

            let transition_width = (new_x - old_x).min(6.) as f32;

            let trace_coords = |x, y| (ctx.to_screen)(x, y * ctx.cfg.line_height + offset);

            ctx.painter.add(PathShape::line(
                vec![
                    trace_coords(*old_x, 0.5),
                    trace_coords(old_x + transition_width / 2., 1.0),
                    trace_coords(new_x - transition_width / 2., 1.0),
                    trace_coords(*new_x, 0.5),
                    trace_coords(new_x - transition_width / 2., 0.0),
                    trace_coords(old_x + transition_width / 2., 0.0),
                    trace_coords(*old_x, 0.5),
                ],
                stroke,
            ));

            let text_size = ctx.cfg.line_height - 5.;
            let char_width = text_size * (20. / 31.);

            let text_area = (new_x - old_x) as f32 - transition_width;
            let num_chars = (text_area / char_width).floor();
            let fits_text = num_chars >= 1.;

            if fits_text {
                let content = if prev_value.len() > num_chars as usize {
                    prev_value
                        .chars()
                        .take(num_chars as usize - 1)
                        .chain(['…'].into_iter())
                        .collect::<String>()
                } else {
                    prev_value.to_string()
                };

                ctx.painter.text(
                    trace_coords(*old_x + transition_width, 0.5),
                    Align2::LEFT_CENTER,
                    content,
                    FontId::monospace(text_size),
                    self.config.theme.foreground,
                );
            }
        }
    }

    fn draw_bool_transition(
        &self,
        ((old_x, prev_region), (new_x, new_region)): (&(f32, DrawnRegion), &(f32, DrawnRegion)),
        force_anti_alias: bool,
        color: Color32,
        offset: f32,
        ctx: &mut DrawingContext,
    ) {
        if let (Some((prev_value, prev_kind)), Some((new_value, new_kind))) =
            (&prev_region.inner, &new_region.inner)
        {
            let trace_coords = |x, y| (ctx.to_screen)(x, y * ctx.cfg.line_height + offset);

            let (mut old_height, old_color, old_bg) =
                prev_value.bool_drawing_spec(color, &self.config.theme, *prev_kind);
            let (mut new_height, _, _) =
                new_value.bool_drawing_spec(color, &self.config.theme, *new_kind);

            let stroke = Stroke {
                color: old_color,
                width: self.config.theme.linewidth,
            };

            if force_anti_alias {
                old_height = 0.;
                new_height = 1.;
            }

            ctx.painter.add(PathShape::line(
                vec![
                    trace_coords(*old_x, 1. - old_height),
                    trace_coords(*new_x, 1. - old_height),
                    trace_coords(*new_x, 1. - new_height),
                ],
                stroke,
            ));

            if let Some(old_bg) = old_bg {
                ctx.painter.add(RectShape {
                    fill: old_bg,
                    rect: Rect {
                        min: (ctx.to_screen)(*old_x, offset),
                        max: (ctx.to_screen)(*new_x, offset + ctx.cfg.line_height),
                    },
                    rounding: Rounding::ZERO,
                    stroke: Stroke {
                        width: 0.,
                        ..Default::default()
                    },
                    fill_texture_id: Default::default(),
                    uv: Rect::ZERO,
                });
            }
        }
    }
}

trait SignalExt {
    fn bool_drawing_spec(
        &self,
        user_color: Color32,
        theme: &SurferTheme,
        value_kind: ValueKind,
    ) -> (f32, Color32, Option<Color32>);
}

impl ValueKind {
    fn color(&self, user_color: Color32, theme: &SurferTheme) -> Color32 {
        match self {
            ValueKind::HighImp => theme.signal_highimp,
            ValueKind::Undef => theme.signal_undef,
            ValueKind::DontCare => theme.signal_dontcare,
            ValueKind::Warn => theme.signal_undef,
            ValueKind::Custom(custom_color) => custom_color.clone(),
            ValueKind::Weak => theme.signal_weak,
            ValueKind::Normal => user_color,
        }
    }
}

impl SignalExt for String {
    /// Return the height and color with which to draw this value if it is a boolean
    fn bool_drawing_spec(
        &self,
        user_color: Color32,
        theme: &SurferTheme,
        value_kind: ValueKind,
    ) -> (f32, Color32, Option<Color32>) {
        let color = value_kind.color(user_color, theme);
        let (height, background) = match (value_kind, self) {
            (ValueKind::HighImp, _) => (0.5, None),
            (ValueKind::Undef, _) => (0.5, None),
            (ValueKind::DontCare, _) => (0.5, None),
            (ValueKind::Warn, _) => (0.5, None),
            (ValueKind::Custom(_), _) => (0.5, None),
            (ValueKind::Weak, other) => {
                if other.to_lowercase() == "l" {
                    (0., None)
                } else {
                    (1., Some(color.gamma_multiply(0.2)))
                }
            }
            (ValueKind::Normal, other) => {
                if other == "0" {
                    (0., None)
                } else {
                    (1., Some(color.gamma_multiply(0.2)))
                }
            }
        };
        (height, color, background)
    }
}
