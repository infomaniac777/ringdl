use anyhow::{anyhow, Result};
use url::Url;

#[derive(Debug, Clone)]
pub struct ParsedUrl {
    pub scheme: String,
    pub host: String,
    pub port: u16,
    pub path: String,
}

impl ParsedUrl {
    pub fn parse(input: &str) -> Result<Self> {
        let url = Url::parse(input)?;
        let scheme = url.scheme().to_string();
        if scheme != "http" && scheme != "https" {
            return Err(anyhow!("Only HTTP and HTTPS are supported (got scheme: {})", scheme));
        }
        let host = url.host_str().ok_or_else(|| anyhow!("Missing host in URL"))?.to_string();
        let port = url.port_or_known_default().unwrap_or(if scheme == "https" { 443 } else { 80 });
        let path = if let Some(query) = url.query() {
            format!("{}?{}", url.path(), query)
        } else {
            url.path().to_string()
        };

        Ok(Self { scheme, host, port, path })
    }

    pub fn build_get_request(&self) -> String {
        format!(
            "GET {} HTTP/1.1\r\n\
             Host: {}:{}\r\n\
             User-Agent: ringdl/0.1.0\r\n\
             Accept: */*\r\n\
             Connection: close\r\n\
             \r\n",
            self.path, self.host, self.port
        )
    }
}

#[derive(Debug)]
pub struct HttpResponseHeader {
    pub status_code: u16,
    pub content_length: Option<u64>,
    pub header_len: usize,
}

pub fn parse_http_response_header(buf: &[u8]) -> Result<Option<HttpResponseHeader>> {
    let mut headers = [httparse::EMPTY_HEADER; 64];
    let mut resp = httparse::Response::new(&mut headers);

    match resp.parse(buf)? {
        httparse::Status::Complete(header_len) => {
            let status_code = resp.code.ok_or_else(|| anyhow!("Missing status code in HTTP response"))?;
            if status_code != 200 && status_code != 206 {
                return Err(anyhow!("HTTP request failed with status code: {}", status_code));
            }

            let mut content_length = None;
            for header in resp.headers.iter() {
                if header.name.eq_ignore_ascii_case("Content-Length") {
                    if let Ok(val_str) = std::str::from_utf8(header.value) {
                        if content_length.is_none() {
                            content_length = val_str.trim().parse::<u64>().ok();
                        }
                    }
                }
                if header.name.eq_ignore_ascii_case("Content-Range") {
                    if let Ok(val_str) = std::str::from_utf8(header.value) {
                        // format: bytes 0-0/104857600
                        if let Some(slash_idx) = val_str.find('/') {
                            let total_str = &val_str[slash_idx + 1..];
                            if let Ok(total) = total_str.trim().parse::<u64>() {
                                content_length = Some(total);
                            }
                        }
                    }
                }
            }

            Ok(Some(HttpResponseHeader {
                status_code,
                content_length,
                header_len,
            }))
        }
        httparse::Status::Partial => Ok(None),
    }
}
