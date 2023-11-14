use eframe::egui;
use waveform::{Timescale, TimescaleUnit};
use num::{BigInt, BigRational, ToPrimitive};

use crate::Message;

pub fn timescale_menu(ui: &mut egui::Ui, msgs: &mut Vec<Message>, wanted_timescale: &TimescaleUnit) {
    let timescales = [
        TimescaleUnit::FemtoSeconds,
        TimescaleUnit::PicoSeconds,
        TimescaleUnit::NanoSeconds,
        TimescaleUnit::MicroSeconds,
        TimescaleUnit::MilliSeconds,
        TimescaleUnit::Seconds,
    ];
    for timescale in timescales {
        ui.radio(*wanted_timescale == timescale, timescale.to_string())
            .clicked()
            .then(|| {
                ui.close_menu();
                msgs.push(Message::SetTimeScale(timescale));
            });
    }
}

pub fn time_string(time: &BigInt, data_timescale: &Timescale, wanted_timescale: &Timescale) -> String {
    let wanted_exponent = wanted_timescale.unit.to_exponent().unwrap();
    let data_exponent = data_timescale.unit.to_exponent().unwrap();
    let exponent_diff = wanted_exponent - data_exponent;
    if exponent_diff >= 0 {
        let precision = exponent_diff as usize;
        format!(
            "{scaledtime:.precision$} {wanted_timescale}",
            scaledtime = BigRational::new(
                time * data_timescale.factor,
                (BigInt::from(10)).pow(exponent_diff as u32)
            )
            .to_f64()
            .unwrap_or(f64::NAN)
        )
    } else {
        format!(
            "{scaledtime} {wanted_timescale}",
            scaledtime = time
                * data_timescale.factor
                * (BigInt::from(10)).pow(-exponent_diff as u32)
        )
    }
}