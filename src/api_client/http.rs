use std::collections::BTreeSet;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use reqwest::blocking::{Client, multipart};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};

use super::assertions::{self, AssertionContext, AssertionResult};
use super::vars::{Variables, interpolate, is_disabled_line};
use super::{ApiKeyLocation, AuthType, BodyType, SavedRequest};

const MAX_TIMELINE_BODY_CHARS: usize = 4000;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FormValue {
    Text(String),
    File(PathBuf),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PreparedBody {
    None,
    Text(String),
    Multipart(Vec<(String, FormValue)>),
}

#[derive(Clone, Debug)]
pub struct PreparedRequest {
    pub method: reqwest::Method,
    pub url: url::Url,
    pub headers: Vec<(String, String)>,
    pub body: PreparedBody,
    pub unresolved: BTreeSet<String>,
}

#[derive(Clone, Debug)]
pub struct HttpResponse {
    pub status: String,
    pub status_code: u16,
    pub success: bool,
    pub body: String,
    pub size: usize,
    pub elapsed_ms: u128,
    pub headers: Vec<(String, String)>,
    pub timeline: String,
    pub final_url: String,
    pub assertions: Vec<AssertionResult>,
    pub unresolved: Vec<String>,
}

/// Applies variables, auth, body mode and query parameters, without sending anything.
pub fn prepare_request(
    request: &SavedRequest,
    variables: &Variables,
) -> Result<PreparedRequest, String> {
    let mut unresolved = BTreeSet::new();
    let mut interp = |text: &str| interpolate(text, variables, &mut unresolved);

    let method = reqwest::Method::from_bytes(request.method.trim().as_bytes())
        .map_err(|error| format!("无效的 HTTP 方法：{error}"))?;
    let raw_url = interp(request.url.trim());
    let raw_url = raw_url.trim();
    if raw_url.is_empty() {
        return Err("请输入请求 URL。".into());
    }
    let raw_url = if raw_url.contains("://") {
        raw_url.to_string()
    } else {
        format!("http://{raw_url}")
    };

    let mut query = parse_pairs(&interp(&request.params), '=', "查询参数")?;
    let mut headers = parse_pairs(&interp(&request.headers), ':', "请求头")?;
    if query.iter().any(|(key, _)| key.is_empty()) {
        return Err("查询参数名称不能为空。".into());
    }

    let auth = &request.auth;
    match auth.auth_type {
        AuthType::None => {}
        AuthType::Bearer => {
            let token = interp(auth.token.trim());
            if !token.is_empty() && !has_header(&headers, "authorization") {
                headers.push(("Authorization".into(), format!("Bearer {token}")));
            }
        }
        AuthType::Basic => {
            if !has_header(&headers, "authorization") {
                let credentials = format!("{}:{}", interp(&auth.username), interp(&auth.password));
                headers.push((
                    "Authorization".into(),
                    format!("Basic {}", base64_encode(credentials.as_bytes())),
                ));
            }
        }
        AuthType::ApiKey => {
            let key = interp(auth.key.trim());
            if !key.is_empty() {
                let value = interp(&auth.value);
                match auth.location {
                    ApiKeyLocation::Header => headers.push((key, value)),
                    ApiKeyLocation::Query => query.push((key, value)),
                }
            }
        }
    }

    let mut url = url::Url::parse(&raw_url).map_err(|error| format!("请求地址无效：{error}"))?;
    if !query.is_empty() {
        url.query_pairs_mut().extend_pairs(
            query
                .iter()
                .map(|(key, value)| (key.as_str(), value.as_str())),
        );
    }

    let body = match request.body_type {
        BodyType::None => PreparedBody::None,
        BodyType::Json | BodyType::Xml | BodyType::Text => {
            let text = interp(&request.body);
            if text.trim().is_empty() {
                PreparedBody::None
            } else {
                PreparedBody::Text(text)
            }
        }
        BodyType::FormUrlEncoded => {
            let fields = parse_pairs(&interp(&request.body), '=', "表单字段")?;
            if fields.is_empty() {
                PreparedBody::None
            } else {
                let mut serializer = url::form_urlencoded::Serializer::new(String::new());
                for (name, value) in &fields {
                    serializer.append_pair(name, value);
                }
                PreparedBody::Text(serializer.finish())
            }
        }
        BodyType::Multipart => {
            let fields = parse_pairs(&interp(&request.body), '=', "表单字段")?;
            let mut parts = Vec::new();
            for (name, value) in fields {
                let part = match value.strip_prefix('@') {
                    Some(path) => {
                        let path = PathBuf::from(path.trim());
                        if !path.is_file() {
                            return Err(format!(
                                "表单字段 {name} 的文件不存在：{}",
                                path.display()
                            ));
                        }
                        FormValue::File(path)
                    }
                    None => FormValue::Text(value),
                };
                parts.push((name, part));
            }
            if parts.is_empty() {
                PreparedBody::None
            } else {
                PreparedBody::Multipart(parts)
            }
        }
    };

    match &body {
        PreparedBody::Multipart(_) => headers.retain(|(name, _)| !is_content_type(name)),
        PreparedBody::Text(_) => {
            if let Some(content_type) = request.body_type.content_type() {
                let existing = headers.iter_mut().find(|(name, _)| is_content_type(name));
                match existing {
                    // The default JSON header is replaced when another body mode is chosen.
                    Some((_, value))
                        if value.trim().eq_ignore_ascii_case("application/json")
                            && request.body_type != BodyType::Json =>
                    {
                        *value = content_type.to_string();
                    }
                    Some(_) => {}
                    None => headers.push(("Content-Type".into(), content_type.to_string())),
                }
            }
        }
        PreparedBody::None => {}
    }

    Ok(PreparedRequest {
        method,
        url,
        headers,
        body,
        unresolved,
    })
}

pub fn execute_request(
    request: &SavedRequest,
    variables: &Variables,
) -> Result<HttpResponse, String> {
    let prepared = prepare_request(request, variables)?;
    let settings = &request.settings;

    let mut builder = Client::builder()
        .danger_accept_invalid_certs(!settings.verify_tls)
        .redirect(if settings.follow_redirects {
            reqwest::redirect::Policy::limited(10)
        } else {
            reqwest::redirect::Policy::none()
        });
    builder = if settings.timeout_ms > 0 {
        builder.timeout(Duration::from_millis(settings.timeout_ms))
    } else {
        builder.timeout(None)
    };
    let client = builder
        .build()
        .map_err(|error| format!("创建 HTTP 客户端失败：{error}"))?;

    let mut header_map = HeaderMap::new();
    for (index, (name, value)) in prepared.headers.iter().enumerate() {
        let name = HeaderName::from_bytes(name.as_bytes())
            .map_err(|error| format!("第 {} 个请求头名称无效：{error}", index + 1))?;
        let value = HeaderValue::from_str(value)
            .map_err(|error| format!("请求头 {name} 的值无效：{error}"))?;
        header_map.append(name, value);
    }

    let mut builder = client
        .request(prepared.method.clone(), prepared.url.clone())
        .headers(header_map);
    let mut timeline_body = String::new();
    match &prepared.body {
        PreparedBody::None => {}
        PreparedBody::Text(text) => {
            timeline_body = truncate(text);
            builder = builder.body(text.clone());
        }
        PreparedBody::Multipart(fields) => {
            let mut form = multipart::Form::new();
            for (name, value) in fields {
                form = match value {
                    FormValue::Text(text) => {
                        timeline_body.push_str(&format!("{name}={text}\n"));
                        form.text(name.clone(), text.clone())
                    }
                    FormValue::File(path) => {
                        timeline_body.push_str(&format!("{name}=@{}\n", path.display()));
                        form.file(name.clone(), path)
                            .map_err(|error| format!("读取文件 {} 失败：{error}", path.display()))?
                    }
                };
            }
            builder = builder.multipart(form);
        }
    }
    let built = builder
        .build()
        .map_err(|error| format!("构建请求失败：{error}"))?;

    let mut timeline = format!("> {} {}\n", built.method(), built.url());
    for (name, value) in built.headers() {
        timeline.push_str(&format!(
            "> {}: {}\n",
            name,
            value.to_str().unwrap_or("<二进制>")
        ));
    }
    if !timeline_body.is_empty() {
        timeline.push_str(">\n");
        for line in timeline_body.lines() {
            timeline.push_str(&format!("> {line}\n"));
        }
    }

    let started = Instant::now();
    let response = client.execute(built).map_err(|error| {
        if error.is_timeout() {
            format!("请求超时（超过 {} 毫秒）：{error}", settings.timeout_ms)
        } else if error.is_connect() {
            format!("连接失败：{error}")
        } else {
            format!("请求发送失败：{error}")
        }
    })?;

    let status_code = response.status().as_u16();
    let success = response.status().is_success();
    let reason = localized_status(status_code)
        .or(response.status().canonical_reason())
        .unwrap_or("未知状态");
    let status = format!("{status_code} {reason}");
    let final_url = response.url().to_string();
    let headers = response
        .headers()
        .iter()
        .map(|(name, value)| {
            (
                name.to_string(),
                String::from_utf8_lossy(value.as_bytes()).into_owned(),
            )
        })
        .collect::<Vec<_>>();
    let bytes = response
        .bytes()
        .map_err(|error| format!("读取响应失败：{error}"))?;
    let elapsed_ms = started.elapsed().as_millis();
    let text = String::from_utf8_lossy(&bytes).into_owned();
    let body = match serde_json::from_str::<serde_json::Value>(&text) {
        Ok(json) => serde_json::to_string_pretty(&json).unwrap_or_else(|_| text.clone()),
        Err(_) => text.clone(),
    };

    timeline.push_str(&format!("\n< {status}\n"));
    for (name, value) in &headers {
        timeline.push_str(&format!("< {name}: {value}\n"));
    }
    timeline.push_str(&format!(
        "\n耗时：{elapsed_ms} 毫秒\n大小：{}\n最终地址：{final_url}\n",
        format_size(bytes.len())
    ));
    if !prepared.unresolved.is_empty() {
        timeline.push_str(&format!(
            "未解析的变量：{}\n",
            prepared
                .unresolved
                .iter()
                .cloned()
                .collect::<Vec<_>>()
                .join("、")
        ));
    }

    let mut unresolved = prepared.unresolved.clone();
    let assertion_rules = interpolate(&request.assertions, variables, &mut unresolved);
    let assertions = assertions::evaluate(
        &assertion_rules,
        &AssertionContext {
            status: status_code,
            response_time_ms: elapsed_ms,
            headers: &headers,
            body: &text,
        },
    );

    Ok(HttpResponse {
        status,
        status_code,
        success,
        body,
        size: bytes.len(),
        elapsed_ms,
        headers,
        timeline,
        final_url,
        assertions,
        unresolved: unresolved.into_iter().collect(),
    })
}

pub fn generate_curl(request: &SavedRequest, variables: &Variables) -> Result<String, String> {
    let prepared = prepare_request(request, variables)?;
    let mut command = format!(
        "curl --request {} {}",
        shell_quote(prepared.method.as_str()),
        shell_quote(prepared.url.as_str())
    );
    let mut push = |argument: String| command.push_str(&format!(" \\\n  {argument}"));
    for (name, value) in &prepared.headers {
        push(format!(
            "--header {}",
            shell_quote(&format!("{name}: {value}"))
        ));
    }
    match &prepared.body {
        PreparedBody::None => {}
        PreparedBody::Text(text) => push(format!("--data-raw {}", shell_quote(text))),
        PreparedBody::Multipart(fields) => {
            for (name, value) in fields {
                let field = match value {
                    FormValue::Text(text) => format!("{name}={text}"),
                    FormValue::File(path) => format!("{name}=@{}", path.display()),
                };
                push(format!("--form {}", shell_quote(&field)));
            }
        }
    }
    let settings = &request.settings;
    if settings.timeout_ms > 0 {
        push(format!(
            "--max-time {}",
            settings.timeout_ms as f64 / 1000.0
        ));
    }
    if settings.follow_redirects {
        push("--location".into());
    }
    if !settings.verify_tls {
        push("--insecure".into());
    }
    Ok(command)
}

pub fn format_size(bytes: usize) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.2} MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

fn parse_pairs(raw: &str, separator: char, kind: &str) -> Result<Vec<(String, String)>, String> {
    let mut pairs = Vec::new();
    for (index, line) in raw.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || is_disabled_line(line) {
            continue;
        }
        let (name, value) = line.split_once(separator).ok_or_else(|| {
            format!(
                "第 {} 行{kind}格式错误，应为 名称{}值。",
                index + 1,
                if separator == ':' { ": " } else { "=" }
            )
        })?;
        pairs.push((name.trim().to_string(), value.trim().to_string()));
    }
    Ok(pairs)
}

fn has_header(headers: &[(String, String)], name: &str) -> bool {
    headers
        .iter()
        .any(|(header, _)| header.eq_ignore_ascii_case(name))
}

fn is_content_type(name: &str) -> bool {
    name.eq_ignore_ascii_case("content-type")
}

fn truncate(text: &str) -> String {
    if text.chars().count() > MAX_TIMELINE_BODY_CHARS {
        format!(
            "{}…（已截断）",
            text.chars()
                .take(MAX_TIMELINE_BODY_CHARS)
                .collect::<String>()
        )
    } else {
        text.to_string()
    }
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn base64_encode(input: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut output = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        output.push(TABLE[(n >> 18) as usize & 63] as char);
        output.push(TABLE[(n >> 12) as usize & 63] as char);
        output.push(if chunk.len() > 1 {
            TABLE[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        output.push(if chunk.len() > 2 {
            TABLE[n as usize & 63] as char
        } else {
            '='
        });
    }
    output
}

fn localized_status(code: u16) -> Option<&'static str> {
    Some(match code {
        200 => "成功",
        201 => "已创建",
        202 => "已接受",
        204 => "无内容",
        301 => "永久重定向",
        302 => "临时重定向",
        304 => "未修改",
        307 => "临时重定向",
        308 => "永久重定向",
        400 => "请求错误",
        401 => "未授权",
        403 => "禁止访问",
        404 => "未找到",
        405 => "方法不允许",
        408 => "请求超时",
        409 => "冲突",
        415 => "不支持的媒体类型",
        422 => "无法处理",
        429 => "请求过于频繁",
        500 => "服务器内部错误",
        501 => "尚未实现",
        502 => "网关错误",
        503 => "服务不可用",
        504 => "网关超时",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::super::vars::parse_variables;
    use super::*;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    use std::sync::mpsc;

    fn request(method: &str, url: &str) -> SavedRequest {
        let mut request = SavedRequest::new("测试");
        request.method = method.into();
        request.url = url.into();
        request
    }

    /// Serves one canned response and returns the raw request text received.
    fn mock_server(response: &'static str) -> (String, mpsc::Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        let (sender, receiver) = mpsc::channel();
        std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut head = String::new();
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                head.push_str(&line);
                if line == "\r\n" || line.is_empty() {
                    break;
                }
            }
            let length = head
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().ok())?
                })
                .unwrap_or(0);
            let mut body = vec![0; length];
            reader.read_exact(&mut body).unwrap();
            head.push_str(&String::from_utf8_lossy(&body));
            let mut stream = stream;
            stream.write_all(response.as_bytes()).unwrap();
            sender.send(head).unwrap();
        });
        (address, receiver)
    }

    #[test]
    fn sends_interpolated_json_with_bearer_auth_and_evaluates_assertions() {
        let (address, received) = mock_server(
            "HTTP/1.1 201 Created\r\nContent-Type: application/json\r\nX-Id: 9\r\nContent-Length: 18\r\nConnection: close\r\n\r\n{\"id\":9,\"ok\":true}",
        );
        let variables = parse_variables(&format!("base={address}\ntoken=abc\nname=张三")).unwrap();
        let mut req = request("POST", "{{base}}/users");
        req.params = "page=1\n# skipped=1".into();
        req.body = r#"{"name":"{{name}}"}"#.into();
        req.auth.auth_type = AuthType::Bearer;
        req.auth.token = "{{token}}".into();
        req.assertions = "res.status eq 201\nres.body.id eq 9\nres.headers.x-id eq 9".into();

        let response = execute_request(&req, &variables).unwrap();
        let raw = received.recv().unwrap();

        assert!(raw.starts_with("POST /users?page=1 HTTP/1.1"), "{raw}");
        assert!(raw.to_lowercase().contains("authorization: bearer abc"));
        assert!(raw.contains(r#"{"name":"张三"}"#));
        assert!(!raw.contains("skipped"));
        assert_eq!(response.status, "201 已创建");
        assert!(response.success);
        assert!(response.body.contains("\"ok\": true"));
        assert!(
            response
                .headers
                .iter()
                .any(|(name, value)| name == "x-id" && value == "9")
        );
        assert!(response.timeline.contains("> POST"));
        assert!(response.timeline.contains("< 201 已创建"));
        assert_eq!(response.assertions.len(), 3);
        assert!(
            response.assertions.iter().all(|a| a.passed),
            "{:?}",
            response.assertions
        );
    }

    #[test]
    fn does_not_follow_redirects_when_disabled() {
        let (address, _received) = mock_server(
            "HTTP/1.1 302 Found\r\nLocation: /elsewhere\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        );
        let mut req = request("GET", &address);
        req.settings.follow_redirects = false;

        let response = execute_request(&req, &Variables::new()).unwrap();

        assert_eq!(response.status_code, 302);
        assert!(!response.success);
    }

    #[test]
    fn prepares_form_basic_auth_and_api_key_query() {
        let mut req = request("POST", "example.com/login");
        req.body_type = BodyType::FormUrlEncoded;
        req.body = "user=张 三\n// skip=1\npass=a&b".into();
        req.auth.auth_type = AuthType::Basic;
        req.auth.username = "Aladdin".into();
        req.auth.password = "open sesame".into();

        let prepared = prepare_request(&req, &Variables::new()).unwrap();

        assert_eq!(prepared.url.as_str(), "http://example.com/login");
        assert_eq!(
            prepared.body,
            PreparedBody::Text("user=%E5%BC%A0+%E4%B8%89&pass=a%26b".into())
        );
        assert!(prepared.headers.contains(&(
            "Content-Type".into(),
            "application/x-www-form-urlencoded".into()
        )));
        assert!(prepared.headers.contains(&(
            "Authorization".into(),
            "Basic QWxhZGRpbjpvcGVuIHNlc2FtZQ==".into()
        )));

        req.auth.auth_type = AuthType::ApiKey;
        req.auth.key = "api_key".into();
        req.auth.value = "k1".into();
        req.auth.location = ApiKeyLocation::Query;
        let prepared = prepare_request(&req, &Variables::new()).unwrap();
        assert_eq!(prepared.url.query(), Some("api_key=k1"));
    }

    #[test]
    fn multipart_requires_existing_files_and_drops_content_type() {
        let mut req = request("POST", "https://example.com/upload");
        req.body_type = BodyType::Multipart;
        req.body = "title=报告\nfile=@C:\\definitely-missing-dmgpost.txt".into();
        assert!(
            prepare_request(&req, &Variables::new())
                .unwrap_err()
                .contains("文件不存在")
        );

        req.body = "title=报告".into();
        let prepared = prepare_request(&req, &Variables::new()).unwrap();
        assert!(
            prepared
                .headers
                .iter()
                .all(|(name, _)| !is_content_type(name))
        );
        assert_eq!(
            prepared.body,
            PreparedBody::Multipart(vec![("title".into(), FormValue::Text("报告".into()))])
        );
    }

    #[test]
    fn none_body_type_sends_no_body() {
        let mut req = request("POST", "https://example.com");
        req.body_type = BodyType::None;
        req.body = "ignored".into();

        assert_eq!(
            prepare_request(&req, &Variables::new()).unwrap().body,
            PreparedBody::None
        );
    }

    #[test]
    fn generates_curl_with_auth_settings_and_quoted_body() {
        let mut req = request("POST", "https://example.com/items");
        req.params = "search=hello world".into();
        req.body = r#"{"name":"O'Reilly"}"#.into();
        req.auth.auth_type = AuthType::Bearer;
        req.auth.token = "t".into();
        req.settings.verify_tls = false;
        req.settings.timeout_ms = 1500;

        let command = generate_curl(&req, &Variables::new()).unwrap();

        assert!(command.contains("'https://example.com/items?search=hello+world'"));
        assert!(command.contains("--header 'Content-Type: application/json'"));
        assert!(command.contains("--header 'Authorization: Bearer t'"));
        assert!(command.contains("O'\\''Reilly"));
        assert!(command.contains("--max-time 1.5"));
        assert!(command.contains("--location"));
        assert!(command.contains("--insecure"));
    }

    #[test]
    fn rejects_empty_url_and_malformed_lines() {
        let req = request("GET", " ");
        assert_eq!(
            execute_request(&req, &Variables::new()).unwrap_err(),
            "请输入请求 URL。"
        );

        let mut req = request("GET", "https://example.com");
        req.headers = "missing separator".into();
        assert!(
            prepare_request(&req, &Variables::new())
                .unwrap_err()
                .contains("第 1 行请求头")
        );
        req.headers.clear();
        req.params = "page".into();
        assert!(
            prepare_request(&req, &Variables::new())
                .unwrap_err()
                .contains("第 1 行查询参数")
        );
    }

    #[test]
    fn helpers_format_sizes_statuses_and_base64() {
        assert_eq!(format_size(512), "512 B");
        assert_eq!(format_size(1536), "1.5 KB");
        assert_eq!(localized_status(404), Some("未找到"));
        assert_eq!(localized_status(299), None);
        assert_eq!(base64_encode(b"ab"), "YWI=");
        assert_eq!(base64_encode(b"a"), "YQ==");
    }
}
