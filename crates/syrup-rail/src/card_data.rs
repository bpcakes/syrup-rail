use std::ops::Range;

use icu_properties::{CodePointMapData, CodePointMapDataBorrowed, props::GeneralCategory};
use zeroize::{Zeroize, Zeroizing};

const UNICODE_GENERAL_CATEGORY: CodePointMapDataBorrowed<'static, GeneralCategory> =
    CodePointMapData::<GeneralCategory>::new();
const MIN_PAN_DIGITS: usize = 13;
const MAX_PAN_DIGITS: usize = 19;

struct PanDigitWindow {
    digits: Zeroizing<[u8; MAX_PAN_DIGITS]>,
    len: usize,
    has_luhn_window: bool,
}

impl PanDigitWindow {
    fn new() -> Self {
        Self {
            digits: Zeroizing::new([0; MAX_PAN_DIGITS]),
            len: 0,
            has_luhn_window: false,
        }
    }

    fn push(&mut self, digit: u8) {
        if self.len < MAX_PAN_DIGITS {
            self.digits[self.len] = digit;
            self.len += 1;
        } else {
            self.digits.copy_within(1..MAX_PAN_DIGITS, 0);
            self.digits[MAX_PAN_DIGITS - 1] = digit;
        }

        if !self.has_luhn_window {
            self.has_luhn_window = (MIN_PAN_DIGITS..=self.len)
                .any(|window_len| luhn_digits_pass(&self.digits[self.len - window_len..self.len]));
        }
    }

    fn has_luhn_window(&self) -> bool {
        self.has_luhn_window
    }

    fn reset(&mut self) {
        self.digits.zeroize();
        self.len = 0;
        self.has_luhn_window = false;
    }
}

pub fn string_contains_raw_card_data(value: &str) -> bool {
    !raw_card_data_ranges(value).is_empty()
}

/// Returns byte ranges containing Luhn-valid, PAN-shaped digit sequences.
///
/// Decimal digits contribute to a candidate, Unicode letters terminate it,
/// and every other scalar is treated as a separator. Percent-encoded scalars
/// are classified after decoding one layer. Returned ranges always refer to
/// the original input bytes.
pub fn raw_card_data_ranges(value: &str) -> Vec<Range<usize>> {
    let mut ranges = Vec::new();
    let mut digits = PanDigitWindow::new();
    let mut candidate_start = None;
    let mut candidate_end = 0;
    let mut index = 0;

    while index < value.len() {
        let (character, next_index) = next_card_scan_scalar(value, index);
        if let Some(digit) = decimal_digit_value(character) {
            candidate_start.get_or_insert(index);
            candidate_end = next_index;
            digits.push(digit);
        } else if character.is_alphabetic() {
            push_raw_card_range(
                value,
                &mut ranges,
                candidate_start.take(),
                candidate_end,
                digits.has_luhn_window(),
            );
            digits.reset();
        }
        index = next_index;
    }

    push_raw_card_range(
        value,
        &mut ranges,
        candidate_start,
        candidate_end,
        digits.has_luhn_window(),
    );
    ranges
}

fn push_raw_card_range(
    value: &str,
    ranges: &mut Vec<Range<usize>>,
    candidate_start: Option<usize>,
    candidate_end: usize,
    has_luhn_window: bool,
) {
    let Some(candidate_start) = candidate_start else {
        return;
    };
    if !has_luhn_window {
        return;
    }
    let candidate = &value[candidate_start..candidate_end];
    if candidate_is_compact_date_time(candidate) {
        return;
    }
    ranges.push(candidate_start..candidate_end);
}

fn next_card_scan_scalar(value: &str, index: usize) -> (char, usize) {
    if value.as_bytes()[index] == b'%'
        && let Some((decoded, next_index)) = percent_decoded_scalar(value, index)
    {
        return (decoded, next_index);
    }
    let character = value[index..]
        .chars()
        .next()
        .expect("scanner index must remain on a UTF-8 boundary");
    (character, index + character.len_utf8())
}

fn percent_decoded_scalar(value: &str, index: usize) -> Option<(char, usize)> {
    let first = percent_decoded_byte(value.as_bytes(), index)?;
    let width = match first {
        0x00..=0x7f => 1,
        0xc2..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf4 => 4,
        _ => return None,
    };
    let mut decoded = [0_u8; 4];
    decoded[0] = first;
    for (offset, byte) in decoded.iter_mut().enumerate().take(width).skip(1) {
        *byte = percent_decoded_byte(value.as_bytes(), index + offset * 3)?;
    }
    let decoded = std::str::from_utf8(&decoded[..width]).ok()?;
    let mut characters = decoded.chars();
    let character = characters.next()?;
    if characters.next().is_some() {
        return None;
    }
    Some((character, index + width * 3))
}

fn percent_decoded_byte(value: &[u8], index: usize) -> Option<u8> {
    if value.get(index) != Some(&b'%') {
        return None;
    }
    let high = char::from(*value.get(index + 1)?).to_digit(16)?;
    let low = char::from(*value.get(index + 2)?).to_digit(16)?;
    Some(((high << 4) | low) as u8)
}

fn candidate_is_compact_date_time(candidate: &str) -> bool {
    let bytes = candidate.as_bytes();
    if bytes.len() != 14 || !bytes.iter().all(u8::is_ascii_digit) {
        return false;
    }
    let year = parse_ascii_digits(&bytes[0..4]);
    let month = parse_ascii_digits(&bytes[4..6]);
    let day = parse_ascii_digits(&bytes[6..8]);
    let hour = parse_ascii_digits(&bytes[8..10]);
    let minute = parse_ascii_digits(&bytes[10..12]);
    let second = parse_ascii_digits(&bytes[12..14]);
    (2000..=2099).contains(&year)
        && (1..=12).contains(&month)
        && (1..=31).contains(&day)
        && hour <= 23
        && minute <= 59
        && second <= 59
}

fn parse_ascii_digits(bytes: &[u8]) -> u32 {
    bytes
        .iter()
        .fold(0, |value, byte| value * 10 + u32::from(byte - b'0'))
}

fn decimal_digit_value(character: char) -> Option<u8> {
    if UNICODE_GENERAL_CATEGORY.get(character) != GeneralCategory::DecimalNumber {
        return None;
    }

    let code_point = character as u32;
    let mut run_start = code_point;
    while run_start > 0
        && UNICODE_GENERAL_CATEGORY.get32(run_start - 1) == GeneralCategory::DecimalNumber
    {
        run_start -= 1;
    }
    Some(((code_point - run_start) % 10) as u8)
}

fn luhn_digits_pass(digits: &[u8]) -> bool {
    if !(MIN_PAN_DIGITS..=MAX_PAN_DIGITS).contains(&digits.len()) {
        return false;
    }
    let mut sum = 0;
    let mut double = false;
    for digit in digits.iter().rev() {
        let mut value = *digit;
        if double {
            value *= 2;
            if value > 9 {
                value -= 9;
            }
        }
        sum += value;
        double = !double;
    }
    sum % 10 == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_PAN: &str = "4111111111111111";

    fn decimal_text(zero: u32, ascii: &str) -> String {
        ascii
            .bytes()
            .map(|byte| char::from_u32(zero + u32::from(byte - b'0')).unwrap())
            .collect()
    }

    fn percent_encode_utf8(value: &str) -> String {
        value
            .as_bytes()
            .iter()
            .map(|byte| format!("%{byte:02X}"))
            .collect()
    }

    #[test]
    fn detects_encoded_unicode_and_unusual_separators() {
        for value in [
            "tok_4111(1111)1111 1111",
            "card 4111%281111%291111%201111",
            "card 4111\u{200b}1111\u{fe0f}1111\u{034f}1111",
            "card 4111%E2%80%8B1111%e2%80%8b1111%20%31%31%31%31",
            "card ４１１１１１１１１１１１１１１１",
            "41111111-1111-1111-1111-111111111111",
        ] {
            assert!(string_contains_raw_card_data(value), "missed {value}");
        }
    }

    #[test]
    fn returns_original_byte_ranges() {
        let encoded = "prefix 4111%E2%80%8B1111%281111%291111 suffix";
        let range = raw_card_data_ranges(encoded).pop().unwrap();
        assert_eq!(&encoded[range], "4111%E2%80%8B1111%281111%291111");

        let unicode = "prefix 4111\u{200b}1111\u{fe0f}1111 1111 suffix";
        let range = raw_card_data_ranges(unicode).pop().unwrap();
        assert_eq!(&unicode[range], "4111\u{200b}1111\u{fe0f}1111 1111");
    }

    #[test]
    fn preserves_boundaries_and_compact_date_time_exception() {
        const LUHN_VALID_TIMESTAMP: &str = "20260704120506";

        assert!(!string_contains_raw_card_data(&format!(
            "reference {LUHN_VALID_TIMESTAMP}"
        )));
        assert!(string_contains_raw_card_data(&decimal_text(
            0x0966,
            LUHN_VALID_TIMESTAMP
        )));
        assert!(string_contains_raw_card_data(&percent_encode_utf8(
            LUHN_VALID_TIMESTAMP
        )));
        assert!(!string_contains_raw_card_data("tok_card_change"));
        assert!(!string_contains_raw_card_data(
            "ffffffff-ffff-4fff-8fff-ffffffffffff"
        ));
        assert!(!string_contains_raw_card_data("41111111letters11111111"));
        assert!(!string_contains_raw_card_data("4111%E2broken111111111111"));
    }

    #[test]
    fn covers_every_icu_decimal_number_set() {
        let mut set_count = 0;
        for range in UNICODE_GENERAL_CATEGORY.iter_ranges_for_value(GeneralCategory::DecimalNumber)
        {
            let start = *range.start();
            let end = *range.end();
            assert_eq!((end - start + 1) % 10, 0);
            for zero in (start..=end).step_by(10) {
                set_count += 1;
                let pan = decimal_text(zero, TEST_PAN);
                assert!(string_contains_raw_card_data(&pan));
                assert!(string_contains_raw_card_data(&percent_encode_utf8(&pan)));
            }
        }
        assert!(set_count > 0);
    }

    #[test]
    fn fixed_window_detects_all_supported_lengths_and_zeroizes_on_reset() {
        for value in [
            "4222222222222",
            "378282246310005",
            TEST_PAN,
            "4000000000000000006",
            "4111 1111 1111 1111 1234",
        ] {
            assert!(string_contains_raw_card_data(value), "missed {value}");
        }

        let mut window = PanDigitWindow::new();
        for digit in TEST_PAN.bytes().map(|byte| byte - b'0') {
            window.push(digit);
        }
        assert!(window.has_luhn_window());
        window.reset();
        assert_eq!(window.len, 0);
        assert!(window.digits.iter().all(|digit| *digit == 0));
    }
}
