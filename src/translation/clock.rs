use color_eyre::eyre::anyhow;
use waveform::{Hierarchy, SignalValue, Var};


use super::{BasicTranslator, BitTranslator, SignalInfo, Translator};

pub struct ClockTranslator {
    // In order to not duplicate logic, we'll re-use the bit translator internally
    inner: Box<dyn BasicTranslator>,
}

impl ClockTranslator {
    pub fn new() -> Self {
        Self {
            inner: Box::new(BitTranslator {}),
        }
    }
}

impl Translator for ClockTranslator {
    fn name(&self) -> String {
        "Clock".to_string()
    }

    fn translate(
        &self,
        hierarchy: &Hierarchy, var: &Var, value: &SignalValue
    ) -> color_eyre::Result<super::TranslationResult> {
        if var.is_1bit() {
            self.inner.translate(hierarchy, var, value)
        } else {
            Err(anyhow!(
                "Clock translator translates a signal which is not 1 bit wide"
            ))
        }
    }

    fn signal_info(&self, _hierarchy: &Hierarchy, _var: &Var) -> color_eyre::Result<super::SignalInfo> {
        Ok(SignalInfo::Clock)
    }

    fn translates(&self, _hierarchy: &Hierarchy, var: &Var) -> color_eyre::Result<super::TranslationPreference> {
        if var.is_1bit() {
            Ok(super::TranslationPreference::Yes)
        } else {
            Ok(super::TranslationPreference::No)
        }
    }
}
