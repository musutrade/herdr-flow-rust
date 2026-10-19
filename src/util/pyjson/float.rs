//! Python's shortest, ties-to-even decimal digits come from serde_json's
//! formatter. Apply Python's fixed/scientific notation thresholds separately.
pub(super) fn format(text: &str) -> String {
    let (sign, text) = match text.strip_prefix('-') {
        Some(text) => ("-", text),
        None => ("", text),
    };
    let (mantissa, exponent) = split_exponent(text);
    let point = mantissa.find('.').unwrap_or(mantissa.len()) as i32;
    let digits = mantissa.replace('.', "");
    let leading = digits.bytes().take_while(|byte| *byte == b'0').count();
    let digits = digits[leading..].trim_end_matches('0');
    if digits.is_empty() {
        return format!("{sign}0.0");
    }
    let exponent = exponent + point - leading as i32 - 1;
    let value = if (-4..16).contains(&exponent) {
        fixed(digits, exponent + 1)
    } else {
        scientific(digits, exponent)
    };
    format!("{sign}{value}")
}

fn split_exponent(text: &str) -> (&str, i32) {
    match text.split_once('e') {
        Some((mantissa, exponent)) => (mantissa, exponent.parse().expect("JSON float exponent")),
        None => (text, 0),
    }
}

fn fixed(digits: &str, point: i32) -> String {
    if point <= 0 {
        return format!("0.{}{digits}", "0".repeat((-point) as usize));
    }
    let point = point as usize;
    if point >= digits.len() {
        return format!("{digits}{}.0", "0".repeat(point - digits.len()));
    }
    format!("{}.{}", &digits[..point], &digits[point..])
}

fn scientific(digits: &str, exponent: i32) -> String {
    let mantissa = if digits.len() == 1 {
        digits.to_string()
    } else {
        format!("{}.{}", &digits[..1], &digits[1..])
    };
    format!("{mantissa}e{exponent:+03}")
}
