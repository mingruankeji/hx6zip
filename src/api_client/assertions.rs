use serde_json::Value;

use super::vars::is_disabled_line;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AssertionResult {
    pub expression: String,
    pub passed: bool,
    pub message: String,
}

pub struct AssertionContext<'a> {
    pub status: u16,
    pub response_time_ms: u128,
    pub headers: &'a [(String, String)],
    pub body: &'a str,
}

const OPERATORS: [&str; 14] = [
    "eq",
    "neq",
    "gt",
    "gte",
    "lt",
    "lte",
    "contains",
    "notContains",
    "startsWith",
    "endsWith",
    "exists",
    "notExists",
    "isEmpty",
    "isNotEmpty",
];

/// Evaluates lines such as `res.status eq 200` or `res.body.data[0].id exists`.
pub fn evaluate(raw: &str, context: &AssertionContext<'_>) -> Vec<AssertionResult> {
    let json = serde_json::from_str::<Value>(context.body).ok();
    raw.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !is_disabled_line(line))
        .map(|line| {
            let (passed, message) = match evaluate_line(line, context, json.as_ref()) {
                Ok(result) => result,
                Err(error) => (false, error),
            };
            AssertionResult {
                expression: line.to_string(),
                passed,
                message,
            }
        })
        .collect()
}

fn evaluate_line(
    line: &str,
    context: &AssertionContext<'_>,
    json: Option<&Value>,
) -> Result<(bool, String), String> {
    let mut parts = line.splitn(3, char::is_whitespace);
    let target = parts.next().unwrap_or_default();
    let operator = parts
        .next()
        .map(str::trim)
        .filter(|op| !op.is_empty())
        .ok_or_else(|| "缺少运算符，格式：目标 运算符 期望值".to_string())?;
    if !OPERATORS.contains(&operator) {
        return Err(format!("不支持的运算符：{operator}"));
    }
    let expected_raw = parts.next().map(str::trim).unwrap_or_default();
    let actual = resolve_target(target, context, json)?;

    let passed = match operator {
        "exists" => actual.is_some(),
        "notExists" => actual.is_none(),
        "isEmpty" => actual.as_ref().is_none_or(is_empty),
        "isNotEmpty" => actual.as_ref().is_some_and(|value| !is_empty(value)),
        _ => {
            if expected_raw.is_empty() {
                return Err(format!("运算符 {operator} 需要期望值"));
            }
            let expected = parse_expected(expected_raw);
            let Some(actual) = actual.as_ref() else {
                return Ok((false, format!("{target} 不存在")));
            };
            match operator {
                "eq" => loose_eq(actual, &expected),
                "neq" => !loose_eq(actual, &expected),
                "gt" | "gte" | "lt" | "lte" => {
                    let (Some(left), Some(right)) = (as_number(actual), as_number(&expected))
                    else {
                        return Ok((false, format!("{target} 或期望值不是数字")));
                    };
                    match operator {
                        "gt" => left > right,
                        "gte" => left >= right,
                        "lt" => left < right,
                        _ => left <= right,
                    }
                }
                "contains" => contains(actual, &expected),
                "notContains" => !contains(actual, &expected),
                "startsWith" => as_text(actual).starts_with(&as_text(&expected)),
                _ => as_text(actual).ends_with(&as_text(&expected)),
            }
        }
    };

    let actual_text = actual
        .as_ref()
        .map(display)
        .unwrap_or_else(|| "（不存在）".into());
    let message = if passed {
        format!("通过，实际值：{actual_text}")
    } else {
        format!("失败，实际值：{actual_text}")
    };
    Ok((passed, message))
}

fn resolve_target(
    target: &str,
    context: &AssertionContext<'_>,
    json: Option<&Value>,
) -> Result<Option<Value>, String> {
    match target {
        "res.status" => return Ok(Some(Value::from(context.status))),
        "res.responseTime" => return Ok(Some(Value::from(context.response_time_ms as u64))),
        "res.body" => {
            return Ok(Some(
                json.cloned()
                    .unwrap_or_else(|| Value::String(context.body.to_string())),
            ));
        }
        _ => {}
    }
    if let Some(name) = target.strip_prefix("res.headers.") {
        return Ok(context
            .headers
            .iter()
            .find(|(header, _)| header.eq_ignore_ascii_case(name))
            .map(|(_, value)| Value::String(value.clone())));
    }
    if let Some(path) = target.strip_prefix("res.body") {
        let Some(json) = json else {
            return Ok(None);
        };
        return Ok(lookup_path(json, path)?.cloned());
    }
    Err(format!(
        "不支持的目标：{target}（可用 res.status、res.responseTime、res.headers.名称、res.body.路径）"
    ))
}

fn lookup_path<'a>(root: &'a Value, path: &str) -> Result<Option<&'a Value>, String> {
    let mut current = root;
    let mut rest = path;
    while !rest.is_empty() {
        if let Some(after_dot) = rest.strip_prefix('.') {
            let end = after_dot.find(['.', '[']).unwrap_or(after_dot.len());
            let key = &after_dot[..end];
            if key.is_empty() {
                return Err("路径中存在空字段名".into());
            }
            match current.get(key) {
                Some(value) => current = value,
                None => return Ok(None),
            }
            rest = &after_dot[end..];
        } else if let Some(after_bracket) = rest.strip_prefix('[') {
            let end = after_bracket
                .find(']')
                .ok_or_else(|| "路径中缺少 ]".to_string())?;
            let index = after_bracket[..end]
                .trim()
                .parse::<usize>()
                .map_err(|_| "数组下标必须是非负整数".to_string())?;
            match current.get(index) {
                Some(value) => current = value,
                None => return Ok(None),
            }
            rest = &after_bracket[end + 1..];
        } else {
            return Err(format!("无法解析路径：{path}"));
        }
    }
    Ok(Some(current))
}

fn parse_expected(raw: &str) -> Value {
    serde_json::from_str(raw).unwrap_or_else(|_| {
        let unquoted = raw
            .strip_prefix('\'')
            .and_then(|value| value.strip_suffix('\''))
            .unwrap_or(raw);
        Value::String(unquoted.to_string())
    })
}

fn is_empty(value: &Value) -> bool {
    match value {
        Value::Null => true,
        Value::String(text) => text.is_empty(),
        Value::Array(items) => items.is_empty(),
        Value::Object(map) => map.is_empty(),
        _ => false,
    }
}

fn as_number(value: &Value) -> Option<f64> {
    match value {
        Value::Number(number) => number.as_f64(),
        Value::String(text) => text.trim().parse().ok(),
        _ => None,
    }
}

fn as_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

fn display(value: &Value) -> String {
    let text = as_text(value);
    if text.chars().count() > 80 {
        format!("{}…", text.chars().take(80).collect::<String>())
    } else {
        text
    }
}

fn loose_eq(actual: &Value, expected: &Value) -> bool {
    if actual == expected {
        return true;
    }
    match (as_number(actual), as_number(expected)) {
        (Some(left), Some(right)) => left == right,
        _ => as_text(actual) == as_text(expected),
    }
}

fn contains(actual: &Value, expected: &Value) -> bool {
    match actual {
        Value::Array(items) => items.iter().any(|item| loose_eq(item, expected)),
        Value::Object(map) => map.contains_key(&as_text(expected)),
        _ => as_text(actual).contains(&as_text(expected)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context<'a>(headers: &'a [(String, String)], body: &'a str) -> AssertionContext<'a> {
        AssertionContext {
            status: 200,
            response_time_ms: 120,
            headers,
            body,
        }
    }

    #[test]
    fn evaluates_status_time_headers_and_body_paths() {
        let headers = vec![("Content-Type".to_string(), "application/json".to_string())];
        let body = r#"{"data":{"items":[{"id":7,"name":"张三"}]},"ok":true,"tags":["a"]}"#;
        let rules = "res.status eq 200\n\
            res.responseTime lt 1000\n\
            res.headers.content-type contains json\n\
            res.body.data.items[0].id eq 7\n\
            res.body.data.items[0].name eq 张三\n\
            res.body.ok eq true\n\
            res.body.tags contains \"a\"\n\
            res.body.missing notExists\n\
            # res.status eq 500\n\
            res.body.data.items isNotEmpty";

        let results = evaluate(rules, &context(&headers, body));

        assert_eq!(results.len(), 9);
        assert!(results.iter().all(|r| r.passed), "{results:#?}");
    }

    #[test]
    fn reports_failures_and_syntax_errors() {
        let results = evaluate(
            "res.status eq 404\nres.status\nres.status like 1\nres.foo eq 1",
            &context(&[], "plain"),
        );

        assert_eq!(results.len(), 4);
        assert!(results.iter().all(|r| !r.passed));
        assert!(results[0].message.contains("200"));
        assert!(results[1].message.contains("缺少运算符"));
        assert!(results[2].message.contains("不支持的运算符"));
        assert!(results[3].message.contains("不支持的目标"));
    }
}
