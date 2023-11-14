use color_eyre::Result;
use num::BigUint;
use waveform::{SignalValue, Var};

use super::{
    map_vector_signal, translates_all_bit_types, BasicTranslator, NumberParseResult,
    TranslationPreference, ValueKind,
};

pub trait NumericTranslator {
    fn name(&self) -> String;
    fn translate_biguint(&self, _: u64, _: BigUint) -> String;
    fn translates(&self, var: &Var) -> Result<TranslationPreference> {
        translates_all_bit_types(var)
    }
}

impl<T: NumericTranslator> BasicTranslator for T {
    fn name(&self) -> String {
        self.name()
    }

    fn basic_translate(&self, num_bits: u64, value: &SignalValue) -> (String, ValueKind) {
        match value {
            SignalValue::Binary(bytes) => {
                let v = BigUint::from_bytes_be(bytes);
                (
                self.translate_biguint(num_bits, v.clone()),
                ValueKind::Normal,
                )
            }
            SignalValue::String(s) => match map_vector_signal(s) {
                NumberParseResult::Unparsable(v, k) => (v, k),
                NumberParseResult::Numerical(v) => {
                    (self.translate_biguint(num_bits, v), ValueKind::Normal)
                }
            },
        }
    }

    fn translates(&self, var: &Var) -> Result<TranslationPreference> {
        self.translates(var)
    }
}
