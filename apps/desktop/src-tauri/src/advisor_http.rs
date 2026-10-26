//! Desktop AI transport. Requests leave the native process, avoiding WebView CORS.
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

#[derive(Clone, Copy, Default, PartialEq, Eq, Hash, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectionMode {
    #[default]
    Default,
    Direct,
}

#[derive(serde::Serialize)]
pub struct AdvisorHttpResponse {
    status: u16,
    body: String,
}

#[derive(Default)]
pub struct AdvisorHttpState {
    clients: Mutex<HashMap<ConnectionMode, reqwest::Client>>,
}

#[tauri::command]
pub async fn advisor_http_request(
    state: tauri::State<'_, AdvisorHttpState>,
    url: String,
    headers: HashMap<String, String>,
    body: String,
    connection_mode: Option<ConnectionMode>,
) -> Result<AdvisorHttpResponse, String> {
    state.request(url, headers, body, connection_mode).await
}

impl AdvisorHttpState {
    fn client(&self, mode: ConnectionMode) -> Result<reqwest::Client, String> {
        let mut clients = self
            .clients
            .lock()
            .map_err(|_| "AI 连接状态暂不可用".to_string())?;
        if let Some(client) = clients.get(&mode) {
            return Ok(client.clone());
        }
        let mut builder = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .timeout(Duration::from_secs(120))
            .pool_idle_timeout(Duration::from_secs(90))
            .pool_max_idle_per_host(2)
            // Never forward an API key to a redirect destination.
            .redirect(reqwest::redirect::Policy::none());
        if mode == ConnectionMode::Direct {
            builder = builder.no_proxy();
        }
        let client = builder
            .build()
            .map_err(|_| "无法创建 AI 网络连接".to_string())?;
        clients.insert(mode, client.clone());
        Ok(client)
    }

    pub async fn request(
        &self,
        url: String,
        headers: HashMap<String, String>,
        body: String,
        connection_mode: Option<ConnectionMode>,
    ) -> Result<AdvisorHttpResponse, String> {
        let url = reqwest::Url::parse(&url).map_err(|_| "AI 接口地址格式不正确".to_string())?;
        if !matches!(url.scheme(), "http" | "https")
            || !url.username().is_empty()
            || url.password().is_some()
        {
            return Err("AI 接口地址须为 http 或 https，且不能包含用户名和密码".into());
        }
        let client = self.client(connection_mode.unwrap_or_default())?;
        let mut request = client.post(url.clone()).body(body);
        let mut secrets = Vec::new();
        for (name, value) in headers {
            match name.to_ascii_lowercase().as_str() {
                "content-type" | "authorization" | "x-api-key" | "anthropic-version" => {
                    if name.eq_ignore_ascii_case("authorization") {
                        secrets.push(value.strip_prefix("Bearer ").unwrap_or(&value).to_owned());
                    } else if name.eq_ignore_ascii_case("x-api-key") {
                        secrets.push(value.clone());
                    }
                    request = request.header(name, value);
                }
                _ => return Err("AI 请求包含不支持的标头".into()),
            }
        }
        for (name, value) in url.query_pairs() {
            if name == "key" {
                secrets.push(value.into_owned());
            }
        }
        let mut response = request.send().await.map_err(network_error)?;
        let status = response.status().as_u16();
        if response.status().is_redirection() {
            return Err("AI 接口返回了跳转，请填写服务商提供的最终 API 地址".into());
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(network_error)? {
            if bytes.len() + chunk.len() > 8 * 1024 * 1024 {
                return Err("AI 返回内容过大，请缩小本次问题后重试".into());
            }
            bytes.extend_from_slice(&chunk);
        }
        let mut body = String::from_utf8_lossy(&bytes).into_owned();
        if !(200..300).contains(&status) {
            for secret in secrets {
                if !secret.is_empty() {
                    body = body.replace(&secret, "[已隐藏密钥]");
                }
            }
        }
        Ok(AdvisorHttpResponse { status, body })
    }
}

fn network_error(error: reqwest::Error) -> String {
    network_error_message(error.is_timeout(), error.is_connect(), &error)
}

fn network_error_message(
    timeout: bool,
    connect: bool,
    error: &(dyn std::error::Error + 'static),
) -> String {
    // Inspect the chain only for categorization; never display raw URLs or keys.
    let mut detail = String::new();
    let mut cause = Some(error);
    while let Some(error) = cause {
        detail.push_str(&error.to_string().to_lowercase());
        detail.push(' ');
        cause = error.source();
    }
    if timeout {
        "AI 请求超时，请稍后重试或检查服务商状态".into()
    } else if detail.contains("certificate") || detail.contains("unknownissuer") {
        "AI 服务的 HTTPS 证书验证失败，请检查服务商证书或代理设置".into()
    } else if detail.contains("tls") || detail.contains("handshake") {
        "AI 服务的安全连接被中断（TLS 握手未完成）。可在 AI 设置中尝试「直接连接」，或检查代理线路"
            .into()
    } else if detail.contains("dns") || detail.contains("resolve") {
        "无法解析 AI 服务地址，请检查 Base URL 和网络 DNS 设置".into()
    } else if connect {
        "无法连接 AI 服务，请检查接口地址和网络；也可在 AI 设置中尝试「直接连接」".into()
    } else {
        // reqwest's display text can contain the URL, including a Gemini API key.
        "AI 网络请求中断，请检查网络或稍后重试".into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    fn server(status: &str, body: &str) -> (String, std::thread::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!(
            "http://{}/v1/chat/completions",
            listener.local_addr().unwrap()
        );
        let response = format!(
            "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let task = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut bytes = [0; 8192];
            let n = stream.read(&mut bytes).unwrap();
            stream.write_all(response.as_bytes()).unwrap();
            String::from_utf8_lossy(&bytes[..n]).into_owned()
        });
        (url, task)
    }

    #[tokio::test]
    async fn native_post_succeeds_without_cors_and_sends_auth() {
        let (url, task) = server("200 OK", r#"{"choices":[{"message":{"content":"OK"}}]}"#);
        let response = AdvisorHttpState::default()
            .request(
                url,
                HashMap::from([
                    ("Authorization".into(), "Bearer test-secret".into()),
                    ("Content-Type".into(), "application/json".into()),
                ]),
                "{}".into(),
                None,
            )
            .await
            .unwrap();
        assert_eq!(response.status, 200);
        assert!(response.body.contains("OK"));
        let request = task.join().unwrap().to_lowercase();
        assert!(request.starts_with("post /v1/chat/completions"));
        assert!(request.contains("authorization: bearer test-secret"));
        assert!(!request.contains("\r\norigin:"));
    }

    #[tokio::test]
    async fn http_errors_preserve_status_and_redact_key() {
        let (url, task) = server("401 Unauthorized", "invalid test-secret");
        let response = AdvisorHttpState::default()
            .request(
                url,
                HashMap::from([("Authorization".into(), "Bearer test-secret".into())]),
                "{}".into(),
                None,
            )
            .await
            .unwrap();
        assert_eq!(response.status, 401);
        assert!(!response.body.contains("test-secret"));
        task.join().unwrap();
    }

    #[tokio::test]
    async fn rejects_invalid_urls_and_redirects() {
        assert!(AdvisorHttpState::default()
            .request("file:///secret".into(), HashMap::new(), "{}".into(), None)
            .await
            .is_err());
        let (url, task) = server("302 Found", "");
        assert!(AdvisorHttpState::default()
            .request(url, HashMap::new(), "{}".into(), None)
            .await
            .err()
            .unwrap()
            .contains("跳转"));
        task.join().unwrap();
    }

    #[test]
    fn precise_connection_errors_do_not_expose_urls_or_keys() {
        for (detail, expected) in [
            (
                "tls handshake eof https://example.invalid?key=secret",
                "TLS 握手",
            ),
            ("invalid peer certificate: UnknownIssuer", "证书验证失败"),
            ("dns error: failed to resolve", "无法解析"),
        ] {
            let error = std::io::Error::other(detail);
            let message = network_error_message(false, true, &error);
            assert!(message.contains(expected));
            assert!(!message.contains("secret"));
            assert!(!message.contains("example.invalid"));
        }
    }

    #[tokio::test]
    async fn direct_request_succeeds() {
        let (url, task) = server("200 OK", "{}");
        let response = AdvisorHttpState::default()
            .request(
                url,
                HashMap::new(),
                "{}".into(),
                Some(ConnectionMode::Direct),
            )
            .await
            .unwrap();
        assert_eq!(response.status, 200);
        task.join().unwrap();
    }

    #[tokio::test]
    async fn sequential_requests_reuse_one_tcp_connection_with_fresh_credentials() {
        use std::io::{BufRead, BufReader};
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!(
            "http://{}/v1/chat/completions",
            listener.local_addr().unwrap()
        );
        let task = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut stream = BufReader::new(stream);
            let mut requests = Vec::new();
            for _ in 0..2 {
                let mut headers = String::new();
                let mut length = 0;
                loop {
                    let mut line = String::new();
                    assert!(
                        stream.read_line(&mut line).unwrap() > 0,
                        "second request must use the same socket"
                    );
                    if line == "\r\n" {
                        break;
                    }
                    if let Some(value) = line.to_lowercase().strip_prefix("content-length:") {
                        length = value.trim().parse().unwrap();
                    }
                    headers.push_str(&line);
                }
                let mut body = vec![0; length];
                stream.read_exact(&mut body).unwrap();
                requests.push((headers.to_lowercase(), String::from_utf8(body).unwrap()));
                stream
                    .get_mut()
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}")
                    .unwrap();
                stream.get_mut().flush().unwrap();
            }
            requests
        });
        let state = AdvisorHttpState::default();
        for (key, body) in [
            ("first-key", "{\"model\":\"first\"}"),
            ("second-key", "{\"model\":\"second\"}"),
        ] {
            let response = tokio::time::timeout(
                Duration::from_secs(8),
                state.request(
                    url.clone(),
                    HashMap::from([("Authorization".into(), format!("Bearer {key}"))]),
                    body.into(),
                    Some(ConnectionMode::Direct),
                ),
            )
            .await
            .unwrap()
            .unwrap();
            assert_eq!(response.status, 200);
        }
        let requests = task.join().unwrap();
        assert!(requests[0].0.contains("authorization: bearer first-key"));
        assert!(requests[1].0.contains("authorization: bearer second-key"));
        assert!(!requests[1].0.contains("first-key"));
        assert_eq!(requests[1].1, "{\"model\":\"second\"}");
    }

    #[tokio::test]
    async fn pooled_client_reconnects_when_server_closes_connection() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!(
            "http://{}/v1/chat/completions",
            listener.local_addr().unwrap()
        );
        let task = std::thread::spawn(move || {
            for _ in 0..2 {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut bytes = [0; 8192];
                assert!(stream.read(&mut bytes).unwrap() > 0);
                stream
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}",
                    )
                    .unwrap();
            }
        });
        let state = AdvisorHttpState::default();
        for _ in 0..2 {
            let response = tokio::time::timeout(
                Duration::from_secs(8),
                state.request(
                    url.clone(),
                    HashMap::new(),
                    "{}".into(),
                    Some(ConnectionMode::Direct),
                ),
            )
            .await
            .unwrap()
            .unwrap();
            assert_eq!(response.status, 200);
        }
        task.join().unwrap();
    }
}
