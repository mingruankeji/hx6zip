use std::collections::{BTreeSet, HashMap};
use std::hash::{BuildHasher, Hasher};
use std::time::{SystemTime, UNIX_EPOCH};

pub type Variables = HashMap<String, String>;

pub fn is_disabled_line(line: &str) -> bool {
    let line = line.trim_start();
    line.starts_with('#') || line.starts_with("//")
}

/// Parses `name=value` lines. Blank and disabled (`#` / `//`) lines are skipped.
pub fn parse_variables(raw: &str) -> Result<Variables, String> {
    let mut variables = Variables::new();
    for (index, line) in raw.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() || is_disabled_line(trimmed) {
            continue;
        }
        let (name, value) = trimmed
            .split_once('=')
            .ok_or_else(|| format!("第 {} 行环境变量格式错误，应为 名称=值。", index + 1))?;
        let name = name.trim();
        if name.is_empty() {
            return Err(format!("第 {} 行环境变量名称不能为空。", index + 1));
        }
        variables.insert(name.to_string(), value.trim().to_string());
    }
    Ok(variables)
}

/// Replaces `{{name}}` placeholders. Unknown names are kept as-is and collected in `unresolved`.
pub fn interpolate(text: &str, variables: &Variables, unresolved: &mut BTreeSet<String>) -> String {
    let mut output = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("{{") {
        output.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find("}}") else {
            output.push_str(&rest[start..]);
            return output;
        };
        let name = after[..end].trim();
        match resolve(name, variables) {
            Some(value) => output.push_str(&value),
            None => {
                if !name.is_empty() {
                    unresolved.insert(name.to_string());
                }
                output.push_str(&rest[start..start + 2 + end + 2]);
            }
        }
        rest = &after[end + 2..];
    }
    output.push_str(rest);
    output
}

fn resolve(name: &str, variables: &Variables) -> Option<String> {
    if let Some(value) = variables.get(name) {
        return Some(value.clone());
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    match name {
        "$timestamp" => Some(now.as_secs().to_string()),
        "$isoTimestamp" => Some(iso_timestamp(now.as_secs(), now.subsec_millis())),
        "$randomInt" => Some((random_u64() % 1000).to_string()),
        "$guid" | "$randomUUID" => Some(random_uuid()),
        _ => None,
    }
}

fn random_u64() -> u64 {
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    hasher.write_u128(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos(),
    );
    hasher.finish()
}

fn random_uuid() -> String {
    let high = random_u64();
    let low = random_u64();
    let mut bytes = [0u8; 16];
    bytes[..8].copy_from_slice(&high.to_be_bytes());
    bytes[8..].copy_from_slice(&low.to_be_bytes());
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex = bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

fn iso_timestamp(secs: u64, millis: u32) -> String {
    let days = (secs / 86_400) as i64;
    let seconds_of_day = secs % 86_400;
    // Civil-from-days algorithm (Howard Hinnant).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{millis:03}Z",
        seconds_of_day / 3600,
        seconds_of_day / 60 % 60,
        seconds_of_day % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_variables_and_skips_disabled_lines() {
        let vars = parse_variables("host = localhost\n# token=x\n// a=b\n\nport=8080").unwrap();

        assert_eq!(vars.len(), 2);
        assert_eq!(vars["host"], "localhost");
        assert_eq!(vars["port"], "8080");
        assert!(parse_variables("broken").unwrap_err().contains("第 1 行"));
    }

    #[test]
    fn interpolates_known_and_reports_unknown_variables() {
        let vars = parse_variables("host=api.test\nid=7").unwrap();
        let mut unresolved = BTreeSet::new();

        let text = interpolate(
            "http://{{host}}/u/{{ id }}?t={{missing}}",
            &vars,
            &mut unresolved,
        );

        assert_eq!(text, "http://api.test/u/7?t={{missing}}");
        assert_eq!(unresolved.into_iter().collect::<Vec<_>>(), vec!["missing"]);
    }

    #[test]
    fn resolves_dynamic_variables() {
        let mut unresolved = BTreeSet::new();
        let vars = Variables::new();

        let guid = interpolate("{{$guid}}", &vars, &mut unresolved);
        let ts = interpolate("{{$timestamp}}", &vars, &mut unresolved);

        assert_eq!(guid.len(), 36);
        assert!(ts.parse::<u64>().is_ok());
        assert!(unresolved.is_empty());
        assert_eq!(iso_timestamp(0, 5), "1970-01-01T00:00:00.005Z");
        assert_eq!(iso_timestamp(1_709_210_096, 0), "2024-02-29T12:34:56.000Z");
    }
}
