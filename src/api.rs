use crate::config::Config;
use anyhow::{bail, Context, Result};
use serde::Deserialize;
use serde_json::json;
use std::{io::Read, time::Duration};

#[derive(Deserialize)]
struct Response {
    choices: Vec<Choice>,
}
#[derive(Deserialize)]
struct Choice {
    message: Message,
    finish_reason: Option<String>,
}
#[derive(Deserialize)]
struct Message {
    content: Option<String>,
}

pub fn rewrite(config: &Config, key: &str, input: &str) -> Result<String> {
    complete(config, key, input, &config.prompt)
}
pub fn translate(config: &Config, key: &str, input: &str) -> Result<String> {
    complete(config, key, input, &config.translation_prompt)
}
fn complete(config: &Config, key: &str, input: &str, prompt: &str) -> Result<String> {
    config.validate()?;
    if key.trim().is_empty() {
        bail!("请先在设置中填写 API Key");
    }
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(config.timeout_secs))
        .connect_timeout(Duration::from_secs(10))
        .redirect(reqwest::redirect::Policy::none())
        .build()?;
    let response = client.post(config.endpoint()?).bearer_auth(key.trim())
        .json(&json!({ "model": config.model.trim(), "stream": false,
            "messages": [{"role": "system", "content": prompt}, {"role": "user", "content": input}] }))
        .send().map_err(|e| if e.is_timeout() { anyhow::anyhow!("请求超时，请重试") } else { anyhow::anyhow!("无法连接 API，请检查地址和网络") })?;
    if !response.status().is_success() {
        // Never display server bodies: they can echo credentials or user text.
        bail!(
            "API 返回 HTTP {}。请检查 Key、模型和额度",
            response.status().as_u16()
        );
    }
    let mut bytes = Vec::new();
    response.take(1024 * 1024 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > 1024 * 1024 {
        bail!("API 响应过大");
    }
    parse(&bytes)
}
fn parse(bytes: &[u8]) -> Result<String> {
    let response: Response =
        serde_json::from_slice(bytes).context("API 未返回标准 Chat Completions JSON")?;
    let choice = response.choices.first().context("API 未返回结果")?;
    if matches!(
        choice.finish_reason.as_deref(),
        Some("length" | "content_filter")
    ) {
        bail!("模型输出不完整，已保留原文");
    }
    let text = choice.message.content.as_deref().unwrap_or("").trim();
    if text.is_empty() {
        bail!("模型返回空内容，已保留原文");
    }
    Ok(text.into())
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::{io::Write, net::TcpListener, thread};

    #[test]
    fn local_http_contract_and_error_redaction() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            for (index, status) in [200, 200, 401].into_iter().enumerate() {
                let (mut socket, _) = listener.accept().unwrap();
                socket
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut request = Vec::new();
                let mut buffer = [0u8; 4096];
                loop {
                    let n = socket.read(&mut buffer).unwrap();
                    assert!(n > 0);
                    request.extend_from_slice(&buffer[..n]);
                    if let Some(header_end) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                        let header = String::from_utf8_lossy(&request[..header_end]).to_lowercase();
                        let length: usize = header
                            .lines()
                            .find_map(|line| line.strip_prefix("content-length: "))
                            .unwrap()
                            .parse()
                            .unwrap();
                        if request.len() >= header_end + 4 + length {
                            assert!(header.starts_with("post /v1/chat/completions http/1.1"));
                            assert!(header.contains("authorization: bearer local-test-only"));
                            let body: serde_json::Value =
                                serde_json::from_slice(&request[header_end + 4..]).unwrap();
                            assert_eq!(body["messages"][0]["role"], "system");
                            assert_eq!(
                                body["messages"][0]["content"],
                                if index == 1 {
                                    crate::config::TRANSLATION_PROMPT
                                } else {
                                    crate::config::PROMPT
                                }
                            );
                            assert_eq!(
                                body["messages"][1]["content"],
                                if index == 1 {
                                    "Please check the deployment."
                                } else {
                                    "测试原文"
                                }
                            );
                            assert_eq!(body["model"], "fixture-model");
                            assert_eq!(body["stream"], false);
                            break;
                        }
                    }
                }
                let body = if status == 200 {
                    json!({"choices":[{"message":{"content":if index==1 {"请检查部署情况。"} else {"Natural English."}},"finish_reason":"stop"}]}).to_string()
                } else {
                    "secret-from-server-must-not-appear".into()
                };
                write!(socket, "HTTP/1.1 {status} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
            }
        });
        let c = Config {
            base_url: format!("http://{address}/v1"),
            model: "fixture-model".into(),
            ..Default::default()
        };
        assert_eq!(
            rewrite(&c, "local-test-only", "测试原文").unwrap(),
            "Natural English."
        );
        assert_eq!(
            translate(&c, "local-test-only", "Please check the deployment.").unwrap(),
            "请检查部署情况。"
        );
        let error = rewrite(&c, "local-test-only", "测试原文")
            .unwrap_err()
            .to_string();
        assert!(error.contains("401"));
        assert!(!error.contains("secret-from-server"));
        server.join().unwrap();
    }
    #[test]
    fn response_contract() {
        assert_eq!(
            parse(br#"{"choices":[{"message":{"content":" Hello. "},"finish_reason":"stop"}]}"#)
                .unwrap(),
            "Hello."
        );
        for body in [
            r#"{"choices":[]}"#,
            r#"{"choices":[{"message":{"content":null}}]}"#,
            r#"{"choices":[{"message":{"content":"cut"},"finish_reason":"length"}]}"#,
        ] {
            assert!(parse(body.as_bytes()).is_err());
        }
    }
}
