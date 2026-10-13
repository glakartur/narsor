use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum DurationParseError {
    #[error("duration '{0}' is empty")]
    Empty(String),
    #[error("duration '{input}' is invalid: expected digits followed by a unit (s, m, h, d), e.g. '10s', '30m', '2h'; found '{found}'")]
    Invalid { input: String, found: String },
    #[error("duration '{input}' uses unknown unit '{unit}'; supported units are s, m, h, d")]
    UnknownUnit { input: String, unit: String },
    #[error("duration '{0}' overflows the supported range")]
    Overflow(String),
}

pub fn parse_duration(input: &str) -> Result<u64, DurationParseError> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err(DurationParseError::Empty(input.to_string()));
    }
    let mut total: u64 = 0;
    let mut digits = String::new();
    let mut matched_any = false;
    let mut chars = trimmed.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch.is_ascii_digit() {
            digits.push(ch);
            continue;
        }
        if digits.is_empty() {
            return Err(DurationParseError::Invalid {
                input: input.to_string(),
                found: ch.to_string(),
            });
        }
        let mut unit = String::new();
        unit.push(ch);
        while let Some(next) = chars.peek() {
            if next.is_ascii_digit() {
                break;
            }
            unit.push(*next);
            chars.next();
        }
        let multiplier = match unit.trim() {
            "s" => 1u64,
            "m" => 60,
            "h" => 3600,
            "d" => 86_400,
            other => {
                return Err(DurationParseError::UnknownUnit {
                    input: input.to_string(),
                    unit: other.to_string(),
                })
            }
        };
        let value: u64 = digits
            .parse()
            .map_err(|_| DurationParseError::Overflow(input.to_string()))?;
        total = total
            .checked_add(
                value
                    .checked_mul(multiplier)
                    .ok_or_else(|| DurationParseError::Overflow(input.to_string()))?,
            )
            .ok_or_else(|| DurationParseError::Overflow(input.to_string()))?;
        digits.clear();
        matched_any = true;
    }
    if !digits.is_empty() && !matched_any {
        return digits
            .parse()
            .map_err(|_| DurationParseError::Overflow(input.to_string()));
    }
    if !digits.is_empty() {
        return Err(DurationParseError::Invalid {
            input: input.to_string(),
            found: digits,
        });
    }
    Ok(total)
}
