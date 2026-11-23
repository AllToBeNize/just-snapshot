//! A minimal blocking HTTPS POST over WinHTTP: system TLS and the system
//! proxy, nothing bundled. Call it off the UI thread.

use std::{ffi::c_void, ptr::null};
use windows_sys::{
    core::w,
    Win32::{Foundation::GetLastError, Networking::WinHttp::*},
};

use crate::wide;

const RESOLVE_MS: i32 = 10_000;
const CONNECT_MS: i32 = 10_000;
const SEND_MS: i32 = 30_000;
/// Language models can take a while before the first byte.
const RECEIVE_MS: i32 = 90_000;

pub struct Response {
    pub status: u32,
    pub body: String,
}

impl Response {
    pub fn ok(&self) -> bool {
        (200..300).contains(&self.status)
    }
}

struct Handle(*mut c_void);

impl Drop for Handle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { WinHttpCloseHandle(self.0) };
        }
    }
}

fn check(handle: *mut c_void, step: &str) -> Result<Handle, String> {
    if handle.is_null() {
        Err(failure(step))
    } else {
        Ok(Handle(handle))
    }
}

fn failure(step: &str) -> String {
    let code = unsafe { GetLastError() };
    let reason = match code {
        12002 => "等待服务器响应超时",
        12005 | 12006 => "接口地址格式不对",
        12007 => "找不到服务器，请检查接口地址和网络",
        12029 => "连不上服务器，请检查网络或代理",
        12030 => "连接被服务器中断",
        12038 | 12044 | 12045 | 12175 => "无法建立安全连接（证书或 TLS 出错）",
        12152 => "服务器返回了无法识别的响应",
        _ => "网络请求失败",
    };
    format!("{reason}（{step}，错误码 {code}）")
}

/// POSTs `body` to `url` with `headers` (`Name: value`). Any HTTP status is
/// returned; only transport failures are errors.
pub fn post(url: &str, headers: &[(&str, &str)], body: &[u8]) -> Result<Response, String> {
    unsafe {
        let url_wide = wide(url);
        let mut parts = URL_COMPONENTS {
            dwStructSize: size_of::<URL_COMPONENTS>() as u32,
            dwSchemeLength: u32::MAX,
            dwHostNameLength: u32::MAX,
            dwUrlPathLength: u32::MAX,
            dwExtraInfoLength: u32::MAX,
            ..Default::default()
        };
        if WinHttpCrackUrl(url_wide.as_ptr(), 0, 0, &mut parts) == 0 {
            return Err(format!("接口地址格式不对：{url}"));
        }
        let host = String::from_utf16_lossy(std::slice::from_raw_parts(
            parts.lpszHostName,
            parts.dwHostNameLength as usize,
        ));
        // The path pointer runs on through the query string.
        let path = String::from_utf16_lossy(std::slice::from_raw_parts(
            parts.lpszUrlPath,
            (parts.dwUrlPathLength + parts.dwExtraInfoLength) as usize,
        ));
        let secure = parts.nScheme == WINHTTP_INTERNET_SCHEME_HTTPS;

        let mut session = WinHttpOpen(
            w!("just-snapshot"),
            WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY,
            null(),
            null(),
            0,
        );
        if session.is_null() {
            // Windows before 8.1 has no automatic proxy mode.
            session = WinHttpOpen(w!("just-snapshot"), WINHTTP_ACCESS_TYPE_DEFAULT_PROXY, null(), null(), 0);
        }
        let session = check(session, "打开会话")?;
        WinHttpSetTimeouts(session.0, RESOLVE_MS, CONNECT_MS, SEND_MS, RECEIVE_MS);
        let host_wide = wide(&host);
        let connection = check(
            WinHttpConnect(session.0, host_wide.as_ptr(), parts.nPort, 0),
            "连接服务器",
        )?;
        let path_wide = wide(if path.is_empty() { "/" } else { &path });
        let request = check(
            WinHttpOpenRequest(
                connection.0,
                w!("POST"),
                path_wide.as_ptr(),
                null(),
                null(),
                null(),
                if secure { WINHTTP_FLAG_SECURE } else { 0 },
            ),
            "创建请求",
        )?;
        let header_text: String = headers
            .iter()
            .map(|(name, value)| format!("{name}: {value}\r\n"))
            .collect();
        let header_wide = wide(&header_text);
        if WinHttpSendRequest(
            request.0,
            header_wide.as_ptr(),
            u32::MAX,
            body.as_ptr().cast(),
            body.len() as u32,
            body.len() as u32,
            0,
        ) == 0
        {
            return Err(failure("发送请求"));
        }
        if WinHttpReceiveResponse(request.0, std::ptr::null_mut()) == 0 {
            return Err(failure("接收响应"));
        }
        let mut status = 0u32;
        let mut size = size_of::<u32>() as u32;
        if WinHttpQueryHeaders(
            request.0,
            WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
            null(),
            (&mut status as *mut u32).cast(),
            &mut size,
            std::ptr::null_mut(),
        ) == 0
        {
            return Err(failure("读取状态码"));
        }
        let mut bytes = Vec::new();
        let mut chunk = vec![0u8; 16 * 1024];
        loop {
            let mut read = 0u32;
            if WinHttpReadData(request.0, chunk.as_mut_ptr().cast(), chunk.len() as u32, &mut read) == 0 {
                return Err(failure("读取响应"));
            }
            if read == 0 {
                break;
            }
            bytes.extend_from_slice(&chunk[..read as usize]);
        }
        Ok(Response {
            status,
            body: String::from_utf8_lossy(&bytes).into_owned(),
        })
    }
}

/// `application/x-www-form-urlencoded` value encoding.
pub fn form(pairs: &[(&str, &str)]) -> String {
    pairs
        .iter()
        .map(|(key, value)| format!("{}={}", encode(key), encode(value)))
        .collect::<Vec<_>>()
        .join("&")
}

fn encode(value: &str) -> String {
    value
        .bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (byte as char).to_string()
            }
            _ => format!("%{byte:02X}"),
        })
        .collect()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::{
        io::{Read, Write},
        net::TcpListener,
        sync::mpsc,
    };

    /// A one-shot local server: answers each request with the next canned
    /// `(status, body)` and hands back what it received.
    pub(crate) fn serve(responses: Vec<(u32, String)>) -> (String, mpsc::Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = format!("http://{}", listener.local_addr().unwrap());
        let (sender, receiver) = mpsc::channel();
        std::thread::spawn(move || {
            for (status, body) in responses {
                let Ok((mut stream, _)) = listener.accept() else { return };
                let mut received = Vec::new();
                let mut buffer = [0u8; 8192];
                // Read the headers, then exactly Content-Length body bytes.
                loop {
                    let read = stream.read(&mut buffer).unwrap_or(0);
                    if read == 0 {
                        break;
                    }
                    received.extend_from_slice(&buffer[..read]);
                    let text = String::from_utf8_lossy(&received).into_owned();
                    if let Some(end) = text.find("\r\n\r\n") {
                        let length = text
                            .lines()
                            .find_map(|line| {
                                let (name, value) = line.split_once(':')?;
                                name.eq_ignore_ascii_case("content-length")
                                    .then(|| value.trim().parse::<usize>().ok())?
                            })
                            .unwrap_or(0);
                        if received.len() >= end + 4 + length {
                            break;
                        }
                    }
                }
                let _ = sender.send(String::from_utf8_lossy(&received).into_owned());
                let reply = format!(
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(reply.as_bytes());
            }
        });
        (address, receiver)
    }

    #[test]
    fn posts_and_reads_the_reply() {
        let (address, received) = serve(vec![(201, "{\"好\":1}".into())]);
        let response = post(
            &format!("{address}/v1/x?a=1"),
            &[("Content-Type", "application/json"), ("X-Test", "yes")],
            "{\"q\":\"你好\"}".as_bytes(),
        )
        .unwrap();
        assert_eq!(response.status, 201);
        assert_eq!(response.body, "{\"好\":1}");
        let request = received.recv().unwrap();
        assert!(request.starts_with("POST /v1/x?a=1 HTTP/1.1"), "{request}");
        assert!(request.contains("X-Test: yes"));
        assert!(request.ends_with("{\"q\":\"你好\"}"));
    }

    #[test]
    fn transport_failures_explain_themselves() {
        // Nothing listens on port 9 of localhost.
        let error = post("http://127.0.0.1:9/", &[], b"").err().unwrap();
        assert!(error.contains("连不上服务器"), "{error}");
        assert!(post("not a url", &[], b"").err().unwrap().contains("格式不对"));
    }

    #[test]
    fn form_encoding_follows_rfc3986() {
        assert_eq!(form(&[("q", "a b&c"), ("x", "你")]), "q=a%20b%26c&x=%E4%BD%A0");
    }
}
