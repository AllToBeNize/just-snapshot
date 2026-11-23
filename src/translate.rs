//! Translation through a provider the user configures: any OpenAI-compatible
//! chat endpoint, DeepL, Baidu or Youdao. Plain settings go to
//! `%LOCALAPPDATA%\just-snapshot\translate.txt`; keys go to the Credential Manager
//! and never touch a file. Everything here blocks; call it off the UI thread.

use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    fs, io,
    path::{Path, PathBuf},
    thread::sleep,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use crate::{credential, digest, http};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Provider {
    /// MyMemory's public API: no key, a daily allowance per address.
    Free,
    OpenAi,
    DeepL,
    Baidu,
    Youdao,
}

pub const PROVIDERS: [Provider; 5] = [Provider::Free, Provider::OpenAi, Provider::DeepL, Provider::Baidu, Provider::Youdao];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Field {
    Endpoint,
    Model,
    ApiKey,
    AppId,
    Secret,
    /// Optional for the free service; raises its daily allowance.
    Email,
}

pub const DEFAULT_ENDPOINT: &str = "https://api.openai.com/v1";
pub const DEFAULT_MODEL: &str = "gpt-4o-mini";

impl Provider {
    pub fn name(self) -> &'static str {
        match self {
            Self::Free => "免费翻译",
            Self::OpenAi => "OpenAI 兼容",
            Self::DeepL => "DeepL",
            Self::Baidu => "百度翻译",
            Self::Youdao => "有道翻译",
        }
    }

    fn key(self) -> &'static str {
        match self {
            Self::Free => "free",
            Self::OpenAi => "openai",
            Self::DeepL => "deepl",
            Self::Baidu => "baidu",
            Self::Youdao => "youdao",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        PROVIDERS.into_iter().find(|provider| provider.key() == value)
    }

    pub fn fields(self) -> &'static [Field] {
        match self {
            Self::Free => &[Field::Email],
            Self::OpenAi => &[Field::Endpoint, Field::Model, Field::ApiKey],
            Self::DeepL => &[Field::ApiKey],
            Self::Baidu | Self::Youdao => &[Field::AppId, Field::Secret],
        }
    }

    /// Where to get credentials, shown under the fields.
    pub fn hint(self) -> &'static str {
        match self {
            Self::Free => "不用注册也不用密钥，直接可用（MyMemory 公共接口）。不填邮箱每天约 5000 字，填写邮箱可提高到每天约 5 万字；文字会发到 MyMemory 的服务器翻译，质量一般，正式使用建议配置其他服务。",
            Self::OpenAi => "OpenAI、DeepSeek、通义千问、Kimi、Ollama 等兼容 /chat/completions 的接口都能用；本地模型可以不填 Key。",
            Self::DeepL => "在 DeepL 账户的「API Keys」页复制密钥；免费版密钥以 :fx 结尾，会自动走免费接口。",
            Self::Baidu => "在百度翻译开放平台开通「通用文本翻译」，在开发者信息里复制 APP ID 和密钥。",
            Self::Youdao => "在有道智云 AI 开放平台创建「文本翻译」应用，复制应用 ID 和应用密钥。",
        }
    }

    /// Requests are sent at most this far apart, matching each service's free tier.
    fn spacing(self) -> Duration {
        match self {
            // A public service shared by everyone: go slowly.
            Self::Baidu | Self::Free => Duration::from_millis(1100),
            _ => Duration::from_millis(300),
        }
    }

    /// Longest text sent in one request, in characters.
    fn chunk(self) -> usize {
        match self {
            // MyMemory takes 500 bytes per request; 150 Chinese characters fit.
            Self::Free => 150,
            Self::OpenAi => 6000,
            Self::DeepL => 20_000,
            Self::Baidu | Self::Youdao => 1800,
        }
    }
}

impl Field {
    pub fn label(self, provider: Provider) -> &'static str {
        match (self, provider) {
            (Self::Endpoint, _) => "接口地址",
            (Self::Model, _) => "模型",
            (Self::ApiKey, _) => "API Key",
            (Self::AppId, Provider::Youdao) => "应用 ID",
            (Self::AppId, _) => "APP ID",
            (Self::Secret, Provider::Youdao) => "应用密钥",
            (Self::Secret, _) => "密钥",
            (Self::Email, _) => "邮箱（可选）",
        }
    }

    /// Secrets are kept in the Credential Manager and never shown again.
    pub fn secret(self) -> bool {
        matches!(self, Self::ApiKey | Self::Secret)
    }

    pub fn placeholder(self) -> &'static str {
        match self {
            Self::Endpoint => DEFAULT_ENDPOINT,
            Self::Model => DEFAULT_MODEL,
            _ => "",
        }
    }

    fn key(self) -> &'static str {
        match self {
            Self::Endpoint => "endpoint",
            Self::Model => "model",
            Self::ApiKey => "api_key",
            Self::AppId => "app_id",
            Self::Secret => "secret",
            Self::Email => "email",
        }
    }
}

pub fn secret_name(provider: Provider, field: Field) -> String {
    format!("translate/{}/{}", provider.key(), field.key())
}

pub fn has_secret(provider: Provider, field: Field) -> bool {
    credential::load(&secret_name(provider, field)).is_ok_and(|secret| secret.is_some_and(|s| !s.is_empty()))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Language {
    Auto,
    Chinese,
    English,
    Japanese,
    Korean,
    French,
    German,
    Spanish,
    Russian,
}

pub const LANGUAGES: [Language; 9] = [
    Language::Auto,
    Language::Chinese,
    Language::English,
    Language::Japanese,
    Language::Korean,
    Language::French,
    Language::German,
    Language::Spanish,
    Language::Russian,
];

impl Language {
    /// The name in its own language, as shown in the picker.
    pub fn label(self) -> &'static str {
        match self {
            Self::Auto => "自动",
            Self::Chinese => "中文",
            Self::English => "English",
            Self::Japanese => "日本語",
            Self::Korean => "한국어",
            Self::French => "Français",
            Self::German => "Deutsch",
            Self::Spanish => "Español",
            Self::Russian => "Русский",
        }
    }

    /// The Chinese name, used in the model prompt.
    fn chinese_name(self) -> &'static str {
        match self {
            Self::Auto | Self::Chinese => "简体中文",
            Self::English => "英语",
            Self::Japanese => "日语",
            Self::Korean => "韩语",
            Self::French => "法语",
            Self::German => "德语",
            Self::Spanish => "西班牙语",
            Self::Russian => "俄语",
        }
    }

    /// Auto turns Chinese text into English and anything else into Chinese.
    pub fn resolve(self, text: &str) -> Self {
        if self != Self::Auto {
            return self;
        }
        let (mut han, mut letters, mut other_cjk) = (0usize, 0usize, 0usize);
        for c in text.chars() {
            match c as u32 {
                0x4e00..=0x9fff | 0x3400..=0x4dbf => han += 1,
                0x3040..=0x30ff | 0xac00..=0xd7af | 0x1100..=0x11ff => other_cjk += 1,
                _ if c.is_alphabetic() => letters += 1,
                _ => {}
            }
        }
        // A Han character carries about as much as a word of four letters.
        if other_cjk == 0 && han > 0 && han * 4 >= letters {
            Self::English
        } else {
            Self::Chinese
        }
    }

    fn deepl(self) -> &'static str {
        match self {
            Self::Auto | Self::Chinese => "ZH-HANS",
            Self::English => "EN-US",
            Self::Japanese => "JA",
            Self::Korean => "KO",
            Self::French => "FR",
            Self::German => "DE",
            Self::Spanish => "ES",
            Self::Russian => "RU",
        }
    }

    fn baidu(self) -> &'static str {
        match self {
            Self::Auto | Self::Chinese => "zh",
            Self::English => "en",
            Self::Japanese => "jp",
            Self::Korean => "kor",
            Self::French => "fra",
            Self::German => "de",
            Self::Spanish => "spa",
            Self::Russian => "ru",
        }
    }

    fn mymemory(self) -> &'static str {
        match self {
            Self::Auto | Self::Chinese => "zh-CN",
            Self::English => "en",
            Self::Japanese => "ja",
            Self::Korean => "ko",
            Self::French => "fr",
            Self::German => "de",
            Self::Spanish => "es",
            Self::Russian => "ru",
        }
    }

    fn youdao(self) -> &'static str {
        match self {
            Self::Auto | Self::Chinese => "zh-CHS",
            Self::English => "en",
            Self::Japanese => "ja",
            Self::Korean => "ko",
            Self::French => "fr",
            Self::German => "de",
            Self::Spanish => "es",
            Self::Russian => "ru",
        }
    }
}

/// The non-secret settings.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Config {
    pub provider: Option<Provider>,
    values: BTreeMap<String, String>,
}

const PROVIDER_KEY: &str = "provider";

impl Config {
    pub fn value(&self, provider: Provider, field: Field) -> &str {
        debug_assert!(!field.secret());
        self.values
            .get(&format!("{}.{}", provider.key(), field.key()))
            .map_or("", String::as_str)
    }

    pub fn set(&mut self, provider: Provider, field: Field, value: &str) {
        assert!(!field.secret(), "secrets never go into the settings file");
        let key = format!("{}.{}", provider.key(), field.key());
        let value = value.trim();
        if value.is_empty() {
            self.values.remove(&key);
        } else {
            self.values.insert(key, value.to_string());
        }
    }

    pub fn path() -> Result<PathBuf, String> {
        Ok(crate::hotkey::path()?.with_file_name("translate.txt"))
    }

    pub fn load() -> Result<Self, String> {
        Self::load_from(&Self::path()?)
    }

    fn load_from(path: &Path) -> Result<Self, String> {
        let contents = match fs::read_to_string(path) {
            Ok(contents) => contents,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(error) => return Err(format!("无法读取翻译设置 {}。\n{error}", path.display())),
        };
        let mut config = Self::default();
        for line in contents.trim_start_matches('\u{feff}').lines() {
            let Some((name, value)) = line.split_once('=') else { continue };
            let (name, value) = (name.trim(), value.trim());
            if name == PROVIDER_KEY {
                config.provider = Provider::parse(value);
            } else if !value.is_empty() {
                config.values.insert(name.to_string(), value.to_string());
            }
        }
        Ok(config)
    }

    pub fn save(&self) -> Result<(), String> {
        self.save_to(&Self::path()?)
    }

    fn save_to(&self, path: &Path) -> Result<(), String> {
        let mut contents = format!(
            "{PROVIDER_KEY}={}\r\n",
            self.provider.map_or("", Provider::key)
        );
        for (name, value) in &self.values {
            contents.push_str(&format!("{name}={value}\r\n"));
        }
        let failed = |error: io::Error| format!("无法保存翻译设置 {}。\n{error}", path.display());
        fs::create_dir_all(path.parent().expect("settings directory")).map_err(failed)?;
        let temporary = path.with_extension("tmp");
        fs::write(&temporary, contents).map_err(failed)?;
        fs::rename(&temporary, path).map_err(|error| {
            let _ = fs::remove_file(&temporary);
            failed(error)
        })
    }
}

/// Everything needed for a request, secrets included. Never logged.
pub struct Service {
    pub provider: Provider,
    /// Overrides the built-in address; set for OpenAI-compatible endpoints.
    pub endpoint: String,
    pub model: String,
    pub app_id: String,
    pub secret: String,
}

impl Service {
    /// `Ok(None)` when a required field is empty. Without a chosen provider
    /// the free service is used, so translation works out of the box.
    pub fn from_config(config: &Config) -> Result<Option<Self>, String> {
        let provider = config.provider.unwrap_or(Provider::Free);
        if provider == Provider::Free {
            return Ok(Some(Self {
                provider,
                endpoint: String::new(),
                model: String::new(),
                app_id: config.value(provider, Field::Email).to_string(),
                secret: String::new(),
            }));
        }
        let secret_field = if provider == Provider::Baidu || provider == Provider::Youdao {
            Field::Secret
        } else {
            Field::ApiKey
        };
        let secret = credential::load(&secret_name(provider, secret_field))?.unwrap_or_default();
        let value = |field| config.value(provider, field).to_string();
        let service = match provider {
            Provider::OpenAi => Self {
                provider,
                endpoint: Some(value(Field::Endpoint))
                    .filter(|endpoint| !endpoint.is_empty())
                    .unwrap_or_else(|| DEFAULT_ENDPOINT.into()),
                model: Some(value(Field::Model))
                    .filter(|model| !model.is_empty())
                    .unwrap_or_else(|| DEFAULT_MODEL.into()),
                app_id: String::new(),
                secret,
            },
            _ => Self {
                provider,
                endpoint: String::new(),
                model: String::new(),
                app_id: if provider == Provider::DeepL { String::new() } else { value(Field::AppId) },
                secret,
            },
        };
        let complete = match provider {
            // Local models such as Ollama need no key.
            Provider::OpenAi | Provider::Free => true,
            Provider::DeepL => !service.secret.is_empty(),
            Provider::Baidu | Provider::Youdao => !service.app_id.is_empty() && !service.secret.is_empty(),
        };
        Ok(complete.then_some(service))
    }
}

enum Failure {
    /// Worth another try: network trouble, rate limits, server errors.
    Retry(String),
    Fatal(String),
}

const ATTEMPTS: usize = 3;
const BACKOFF_MS: [u64; ATTEMPTS - 1] = [800, 2000];

/// Translates `text` into `target` (never `Auto`; resolve it first), keeping
/// line breaks. Long text goes out in several requests, one after another.
pub fn translate(service: &Service, text: &str, target: Language) -> Result<String, String> {
    let mut parts = Vec::new();
    for (index, chunk) in chunks(text, service.provider.chunk()).iter().enumerate() {
        if index > 0 {
            sleep(service.provider.spacing());
        }
        parts.push(with_retries(|| request(service, chunk, target)).map_err(|error| {
            format!("翻译失败（{}）\n{error}", service.provider.name())
        })?);
    }
    Ok(parts.join("\n"))
}

fn with_retries(mut attempt: impl FnMut() -> Result<String, Failure>) -> Result<String, String> {
    for round in 0.. {
        match attempt() {
            Ok(text) => return Ok(text),
            Err(Failure::Retry(_)) if round + 1 < ATTEMPTS => {
                sleep(Duration::from_millis(BACKOFF_MS[round]));
            }
            Err(Failure::Retry(error)) => return Err(format!("{error}\n已重试 {} 次。", ATTEMPTS - 1)),
            Err(Failure::Fatal(error)) => return Err(error),
        }
    }
    unreachable!()
}

/// Splits at line breaks so each piece stays under `limit` characters.
fn chunks(text: &str, limit: usize) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut current = String::new();
    for line in text.lines() {
        let mut line = line;
        // A single overlong line is cut by characters.
        while line.chars().count() > limit {
            let cut = line.char_indices().nth(limit).map_or(line.len(), |(index, _)| index);
            if !current.is_empty() {
                chunks.push(std::mem::take(&mut current));
            }
            chunks.push(line[..cut].to_string());
            line = &line[cut..];
        }
        if !current.is_empty() && current.chars().count() + 1 + line.chars().count() > limit {
            chunks.push(std::mem::take(&mut current));
        }
        if !current.is_empty() {
            current.push('\n');
        }
        current.push_str(line);
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}

fn request(service: &Service, text: &str, target: Language) -> Result<String, Failure> {
    match service.provider {
        Provider::Free => mymemory(service, text, target),
        Provider::OpenAi => openai(service, text, target),
        Provider::DeepL => deepl(service, text, target),
        Provider::Baidu => baidu(service, text, target),
        Provider::Youdao => youdao(service, text, target),
    }
}

fn send(url: &str, headers: &[(&str, &str)], body: &str) -> Result<(http::Response, Value), Failure> {
    let response = http::post(url, headers, body.as_bytes()).map_err(Failure::Retry)?;
    let value = serde_json::from_str(&response.body).unwrap_or(Value::Null);
    Ok((response, value))
}

/// Classifies a non-2xx reply, preferring the server's own explanation.
fn http_failure(response: &http::Response, message: Option<&str>, meaning: &str) -> Failure {
    let detail = message
        .map(str::to_string)
        .unwrap_or_else(|| response.body.trim().to_string());
    let text = if detail.is_empty() {
        format!("{meaning}（HTTP {}）", response.status)
    } else {
        format!("{meaning}（HTTP {}）\n服务器返回：{detail}", response.status)
    };
    if response.status == 429 || response.status >= 500 {
        Failure::Retry(text)
    } else {
        Failure::Fatal(text)
    }
}

fn status_meaning(status: u32) -> &'static str {
    match status {
        400 => "请求被拒绝，请检查模型名和接口地址",
        401 | 403 => "认证失败，请检查密钥是否正确",
        404 => "接口地址或模型不存在",
        429 => "请求太频繁或额度不足",
        456 => "本月额度已用完",
        500..=599 => "服务器出错",
        _ => "请求失败",
    }
}

fn openai_url(endpoint: &str) -> String {
    let endpoint = endpoint.trim().trim_end_matches('/');
    if endpoint.ends_with("/chat/completions") {
        endpoint.to_string()
    } else {
        format!("{endpoint}/chat/completions")
    }
}

/// Reasoning models may put their thinking before the answer.
fn strip_thinking(text: &str) -> &str {
    let text = text.trim();
    match (text.starts_with("<think>"), text.find("</think>")) {
        (true, Some(end)) => text[end + "</think>".len()..].trim(),
        _ => text,
    }
}

fn openai(service: &Service, text: &str, target: Language) -> Result<String, Failure> {
    let prompt = format!(
        "你是专业翻译。把用户发来的文字翻译成{}。保留原有的换行和格式，专有名词、代码、路径和网址保持原样。只输出译文，不要解释，不要加引号。",
        target.chinese_name()
    );
    let body = json!({
        "model": service.model,
        "temperature": 0.2,
        "messages": [
            { "role": "system", "content": prompt },
            { "role": "user", "content": text },
        ],
    })
    .to_string();
    let authorization = format!("Bearer {}", service.secret);
    let mut headers = vec![("Content-Type", "application/json")];
    if !service.secret.is_empty() {
        headers.push(("Authorization", authorization.as_str()));
    }
    let (response, value) = send(&openai_url(&service.endpoint), &headers, &body)?;
    if !response.ok() {
        let message = value["error"]["message"].as_str().or_else(|| value["message"].as_str());
        return Err(http_failure(&response, message, status_meaning(response.status)));
    }
    value["choices"][0]["message"]["content"]
        .as_str()
        .map(|content| strip_thinking(content).to_string())
        .ok_or_else(|| Failure::Fatal(format!("接口返回的内容里没有译文。\n服务器返回：{}", response.body.trim())))
}

/// MyMemory: the source language is detected; `app_id` holds the optional email.
fn mymemory(service: &Service, text: &str, target: Language) -> Result<String, Failure> {
    let url = if service.endpoint.is_empty() {
        "https://api.mymemory.translated.net/get"
    } else {
        &service.endpoint
    };
    let pair = format!("autodetect|{}", target.mymemory());
    let mut pairs = vec![("q", text), ("langpair", pair.as_str())];
    let email = service.app_id.trim();
    if !email.is_empty() {
        pairs.push(("de", email));
    }
    let body = http::form(&pairs);
    let (response, value) = send(url, &[("Content-Type", "application/x-www-form-urlencoded")], &body)?;
    // The status comes as a number or a string, and errors often arrive with HTTP 200.
    let status = value["responseStatus"]
        .as_u64()
        .or_else(|| value["responseStatus"].as_str().and_then(|status| status.parse().ok()))
        .unwrap_or(u64::from(response.status));
    let detail = value["responseDetails"].as_str().unwrap_or("").trim();
    if value["quotaFinished"].as_bool() == Some(true) || status == 429 || detail.contains("ALL AVAILABLE FREE") {
        return Err(Failure::Fatal(format!(
            "今天的免费额度用完了。可以在翻译设置里填写邮箱提高额度，或者换用其他翻译服务。\n服务器返回：{}",
            if detail.is_empty() { response.body.trim() } else { detail }
        )));
    }
    if status != 200 || !response.ok() {
        let meaning = if detail.contains("DISTINCT LANGUAGES") {
            "原文已经是目标语言，请换一个目标语言"
        } else if status >= 500 {
            "服务器出错"
        } else {
            "请求失败"
        };
        let text = format!("{meaning}（{status}）\n服务器返回：{}", if detail.is_empty() { response.body.trim() } else { detail });
        return Err(if status >= 500 { Failure::Retry(text) } else { Failure::Fatal(text) });
    }
    value["responseData"]["translatedText"]
        .as_str()
        .map(|text| text.trim().to_string())
        .ok_or_else(|| Failure::Fatal(format!("接口返回的内容里没有译文。\n服务器返回：{}", response.body.trim())))
}

fn deepl(service: &Service, text: &str, target: Language) -> Result<String, Failure> {
    let url = if !service.endpoint.is_empty() {
        service.endpoint.clone()
    } else if service.secret.trim().ends_with(":fx") {
        "https://api-free.deepl.com/v2/translate".into()
    } else {
        "https://api.deepl.com/v2/translate".into()
    };
    let body = json!({
        "text": [text],
        "target_lang": target.deepl(),
        "preserve_formatting": true,
    })
    .to_string();
    let authorization = format!("DeepL-Auth-Key {}", service.secret.trim());
    let (response, value) = send(
        &url,
        &[("Content-Type", "application/json"), ("Authorization", &authorization)],
        &body,
    )?;
    if !response.ok() {
        return Err(http_failure(&response, value["message"].as_str(), status_meaning(response.status)));
    }
    value["translations"][0]["text"]
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| Failure::Fatal(format!("DeepL 返回的内容里没有译文。\n服务器返回：{}", response.body.trim())))
}

fn salt() -> String {
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().subsec_nanos();
    (nanos % 1_000_000_000).to_string()
}

fn baidu_sign(app_id: &str, query: &str, salt: &str, secret: &str) -> String {
    digest::md5(&format!("{app_id}{query}{salt}{secret}"))
}

fn baidu(service: &Service, text: &str, target: Language) -> Result<String, Failure> {
    let url = if service.endpoint.is_empty() {
        "https://fanyi-api.baidu.com/api/trans/vip/translate"
    } else {
        &service.endpoint
    };
    let salt = salt();
    let (app_id, secret) = (service.app_id.trim(), service.secret.trim());
    let sign = baidu_sign(app_id, text, &salt, secret);
    let body = http::form(&[
        ("q", text),
        ("from", "auto"),
        ("to", target.baidu()),
        ("appid", app_id),
        ("salt", &salt),
        ("sign", &sign),
    ]);
    let (response, value) = send(url, &[("Content-Type", "application/x-www-form-urlencoded")], &body)?;
    if !response.ok() {
        return Err(http_failure(&response, None, status_meaning(response.status)));
    }
    let code = match &value["error_code"] {
        Value::String(code) => code.clone(),
        Value::Number(code) => code.to_string(),
        _ => String::new(),
    };
    if !code.is_empty() && code != "52000" {
        {
            let message = value["error_msg"].as_str().unwrap_or("");
            let meaning = match code.as_str() {
                "52001" => "请求超时",
                "52002" => "系统错误",
                "52003" => "未授权用户，请检查 APP ID 是否正确、服务是否已开通",
                "54000" => "必填参数为空",
                "54001" => "签名错误，请检查 APP ID 和密钥是否填对",
                "54003" => "访问频率受限",
                "54004" => "账户余额不足",
                "54005" => "长文本请求过于频繁",
                "58000" => "客户端 IP 不在白名单里",
                "58001" => "不支持这个译文语言",
                "58002" => "服务已关闭，请到控制台开启",
                "90107" => "认证未通过或未生效",
                _ => "百度翻译返回错误",
            };
            let text = format!("{meaning}（错误码 {code}）\n服务器返回：{message}");
            return Err(match code.as_str() {
                "52001" | "52002" | "54003" | "54005" => Failure::Retry(text),
                _ => Failure::Fatal(text),
            });
        }
    }
    let lines: Option<Vec<&str>> = value["trans_result"]
        .as_array()
        .map(|items| items.iter().filter_map(|item| item["dst"].as_str()).collect());
    lines
        .filter(|lines| !lines.is_empty())
        .map(|lines| lines.join("\n"))
        .ok_or_else(|| Failure::Fatal(format!("百度翻译返回的内容里没有译文。\n服务器返回：{}", response.body.trim())))
}

/// Youdao signs a shortened form of long input: first 10, length, last 10.
fn youdao_input(query: &str) -> String {
    let chars: Vec<char> = query.chars().collect();
    if chars.len() <= 20 {
        query.to_string()
    } else {
        let head: String = chars[..10].iter().collect();
        let tail: String = chars[chars.len() - 10..].iter().collect();
        format!("{head}{}{tail}", chars.len())
    }
}

fn youdao(service: &Service, text: &str, target: Language) -> Result<String, Failure> {
    let url = if service.endpoint.is_empty() {
        "https://openapi.youdao.com/api"
    } else {
        &service.endpoint
    };
    let salt = salt();
    let time = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs().to_string();
    let (app_id, secret) = (service.app_id.trim(), service.secret.trim());
    let sign = digest::sha256(&format!("{app_id}{}{salt}{time}{secret}", youdao_input(text)));
    let body = http::form(&[
        ("q", text),
        ("from", "auto"),
        ("to", target.youdao()),
        ("appKey", app_id),
        ("salt", &salt),
        ("sign", &sign),
        ("signType", "v3"),
        ("curtime", &time),
    ]);
    let (response, value) = send(url, &[("Content-Type", "application/x-www-form-urlencoded")], &body)?;
    if !response.ok() {
        return Err(http_failure(&response, None, status_meaning(response.status)));
    }
    let code = value["errorCode"].as_str().unwrap_or("");
    if code != "0" {
        let meaning = match code {
            "101" => "缺少必填参数",
            "102" => "不支持这个语言",
            "108" => "应用 ID 无效，请检查是否填对",
            "110" => "这个应用没有绑定「文本翻译」服务",
            "111" => "开发者账号无效",
            "202" => "签名检验失败，请检查应用 ID 和应用密钥",
            "206" => "时间戳无效，请检查电脑时间",
            "207" => "重放请求",
            "401" => "账户已欠费",
            "411" => "访问频率受限",
            "412" => "长请求过于频繁",
            _ => "有道翻译返回错误",
        };
        let text = format!("{meaning}（错误码 {}）", if code.is_empty() { "无" } else { code });
        let text = if code.is_empty() {
            format!("{text}\n服务器返回：{}", response.body.trim())
        } else {
            text
        };
        return Err(if matches!(code, "411" | "412") { Failure::Retry(text) } else { Failure::Fatal(text) });
    }
    let lines: Option<Vec<&str>> = value["translation"]
        .as_array()
        .map(|items| items.iter().filter_map(Value::as_str).collect());
    lines
        .filter(|lines| !lines.is_empty())
        .map(|lines| lines.join("\n"))
        .ok_or_else(|| Failure::Fatal(format!("有道翻译返回的内容里没有译文。\n服务器返回：{}", response.body.trim())))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::tests::serve;

    fn service(provider: Provider, endpoint: &str) -> Service {
        Service {
            provider,
            endpoint: endpoint.into(),
            model: "test-model".into(),
            app_id: "app".into(),
            secret: "sk-secret".into(),
        }
    }

    fn body(request: &str) -> &str {
        request.split_once("\r\n\r\n").unwrap().1
    }

    fn form_value<'a>(request: &'a str, name: &str) -> &'a str {
        body(request)
            .split('&')
            .find_map(|pair| pair.strip_prefix(&format!("{name}=")))
            .unwrap()
    }

    #[test]
    fn auto_turns_chinese_into_english_and_the_rest_into_chinese() {
        let auto = Language::Auto;
        assert_eq!(auto.resolve("保存到桌面"), Language::English);
        assert_eq!(auto.resolve("识别文字 Snapshot 保存到 C:\\Users\\eric\\Pictures"), Language::English);
        assert_eq!(auto.resolve("Save the file to your desktop"), Language::Chinese);
        assert_eq!(auto.resolve("ファイルを保存"), Language::Chinese);
        assert_eq!(auto.resolve("파일 저장"), Language::Chinese);
        assert_eq!(auto.resolve("Open the 设置 page to change the shortcut"), Language::Chinese);
        assert_eq!(Language::Japanese.resolve("保存"), Language::Japanese);
    }

    #[test]
    fn long_text_splits_at_line_breaks() {
        assert_eq!(chunks("ab\ncd\nef", 5), vec!["ab\ncd", "ef"]);
        assert_eq!(chunks("abcdefg", 3), vec!["abc", "def", "g"]);
        assert_eq!(chunks("一\n二三四五六七", 3), vec!["一", "二三四", "五六七"]);
        assert!(chunks("", 10).is_empty());
    }

    #[test]
    fn signatures_match_the_documented_examples() {
        // From the Baidu Translate API documentation.
        assert_eq!(
            baidu_sign("2015063000000001", "apple", "1435660288", "12345678"),
            "f89f9594663708c1605f3d736d01d2d4"
        );
        assert_eq!(youdao_input("短文本"), "短文本");
        assert_eq!(youdao_input("0123456789abcdefghijKLMNO"), "012345678925fghijKLMNO");
    }

    #[test]
    fn openai_compatible_endpoints_get_a_chat_request() {
        assert_eq!(openai_url("https://api.deepseek.com/v1/"), "https://api.deepseek.com/v1/chat/completions");
        assert_eq!(openai_url("http://x/v1/chat/completions"), "http://x/v1/chat/completions");
        let reply = json!({ "choices": [{ "message": { "content": "<think>想一想</think>\nHello\nWorld" } }] });
        let (address, received) = serve(vec![(200, reply.to_string())]);
        let result = translate(&service(Provider::OpenAi, &format!("{address}/v1")), "你好\n世界", Language::English);
        assert_eq!(result.unwrap(), "Hello\nWorld");
        let request = received.recv().unwrap();
        assert!(request.starts_with("POST /v1/chat/completions"));
        assert!(request.contains("Authorization: Bearer sk-secret"));
        let sent: Value = serde_json::from_str(body(&request)).unwrap();
        assert_eq!(sent["model"], "test-model");
        assert_eq!(sent["messages"][1]["content"], "你好\n世界");
        assert!(sent["messages"][0]["content"].as_str().unwrap().contains("英语"));
    }

    #[test]
    fn server_errors_are_retried_and_auth_errors_are_not() {
        let ok = json!({ "choices": [{ "message": { "content": "好" } }] }).to_string();
        let (address, received) = serve(vec![(503, "busy".into()), (200, ok)]);
        assert_eq!(translate(&service(Provider::OpenAi, &address), "good", Language::Chinese).unwrap(), "好");
        assert_eq!(received.iter().take(2).count(), 2);

        let denied = json!({ "error": { "message": "Incorrect API key provided" } }).to_string();
        let (address, received) = serve(vec![(401, denied)]);
        let error = translate(&service(Provider::OpenAi, &address), "good", Language::Chinese).unwrap_err();
        assert_eq!(received.iter().count(), 1, "a wrong key is not retried");
        assert!(error.starts_with("翻译失败（OpenAI 兼容）\n认证失败"), "{error}");
        assert!(error.contains("Incorrect API key provided"));
        assert!(!error.contains("sk-secret"), "the key never shows in errors");
    }

    #[test]
    fn deepl_sends_the_key_in_its_header() {
        let reply = json!({ "translations": [{ "text": "你好" }] });
        let (address, received) = serve(vec![(200, reply.to_string())]);
        assert_eq!(translate(&service(Provider::DeepL, &address), "Hello", Language::Chinese).unwrap(), "你好");
        let request = received.recv().unwrap();
        assert!(request.contains("Authorization: DeepL-Auth-Key sk-secret"));
        let sent: Value = serde_json::from_str(body(&request)).unwrap();
        assert_eq!(sent["target_lang"], "ZH-HANS");

        let (address, _) = serve(vec![(456, json!({ "message": "Quota exceeded" }).to_string())]);
        let error = translate(&service(Provider::DeepL, &address), "Hello", Language::Chinese).unwrap_err();
        assert!(error.contains("额度已用完") && error.contains("Quota exceeded"), "{error}");
    }

    #[test]
    fn baidu_signs_the_form_and_joins_paragraphs() {
        let reply = json!({ "trans_result": [{ "src": "a", "dst": "甲" }, { "src": "b", "dst": "乙" }] });
        let (address, received) = serve(vec![(200, reply.to_string())]);
        assert_eq!(translate(&service(Provider::Baidu, &address), "a\nb", Language::Chinese).unwrap(), "甲\n乙");
        let request = received.recv().unwrap();
        let salt = form_value(&request, "salt");
        assert_eq!(form_value(&request, "q"), "a%0Ab");
        assert_eq!(form_value(&request, "to"), "zh");
        assert_eq!(form_value(&request, "sign"), baidu_sign("app", "a\nb", salt, "sk-secret"));

        let (address, _) = serve(vec![(200, json!({ "error_code": "54001", "error_msg": "Invalid Sign" }).to_string())]);
        let error = translate(&service(Provider::Baidu, &address), "a", Language::Chinese).unwrap_err();
        assert!(error.contains("签名错误") && error.contains("54001"), "{error}");
    }

    #[test]
    fn youdao_signs_with_sha256() {
        let reply = json!({ "errorCode": "0", "translation": ["Hello"] });
        let (address, received) = serve(vec![(200, reply.to_string())]);
        assert_eq!(translate(&service(Provider::Youdao, &address), "你好", Language::English).unwrap(), "Hello");
        let request = received.recv().unwrap();
        let (salt, time) = (form_value(&request, "salt"), form_value(&request, "curtime"));
        assert_eq!(form_value(&request, "signType"), "v3");
        assert_eq!(
            form_value(&request, "sign"),
            digest::sha256(&format!("app你好{salt}{time}sk-secret"))
        );

        let (address, _) = serve(vec![(200, json!({ "errorCode": "202" }).to_string())]);
        let error = translate(&service(Provider::Youdao, &address), "你好", Language::English).unwrap_err();
        assert!(error.contains("签名检验失败"), "{error}");
    }

    #[test]
    fn the_free_service_needs_no_key_and_explains_its_limit() {
        let config = Config::default();
        let free = Service::from_config(&config).unwrap().expect("works without any settings");
        assert_eq!(free.provider, Provider::Free);
        let reply = json!({ "responseStatus": 200, "responseData": { "translatedText": "Hello, world" }, "quotaFinished": false });
        let (address, received) = serve(vec![(200, reply.to_string())]);
        let mut free = service(Provider::Free, &address);
        free.app_id = "me@example.com".into();
        assert_eq!(translate(&free, "你好，世界", Language::English).unwrap(), "Hello, world");
        let request = received.recv().unwrap();
        assert_eq!(form_value(&request, "langpair"), "autodetect%7Cen");
        assert_eq!(form_value(&request, "de"), "me%40example.com");
        assert!(!request.contains("Authorization"));

        let quota = json!({ "responseStatus": "429", "responseDetails": "MYMEMORY WARNING: YOU USED ALL AVAILABLE FREE TRANSLATIONS FOR TODAY", "quotaFinished": true });
        let (address, received) = serve(vec![(200, quota.to_string())]);
        free.endpoint = address;
        let error = translate(&free, "hi", Language::Chinese).unwrap_err();
        assert!(error.starts_with("翻译失败（免费翻译）\n今天的免费额度用完了"), "{error}");
        assert_eq!(received.iter().count(), 1, "a spent allowance is not retried");

        let same = json!({ "responseStatus": 403, "responseDetails": "PLEASE SELECT TWO DISTINCT LANGUAGES" });
        let (address, _) = serve(vec![(200, same.to_string())]);
        let error = translate(&service(Provider::Free, &address), "你好", Language::Chinese).unwrap_err();
        assert!(error.contains("原文已经是目标语言"), "{error}");
    }

    /// Asks the real MyMemory once. Run with `<test binary> free_service_live --ignored`.
    #[test]
    #[ignore]
    fn free_service_live() {
        let service = Service::from_config(&Config::default()).unwrap().unwrap();
        let text = translate(&service, "Save the screenshot\nCopy to clipboard", Language::Chinese).unwrap();
        println!("{text}");
        assert!(text.chars().any(|c| ('\u{4e00}'..='\u{9fff}').contains(&c)), "{text}");
    }

    #[test]
    fn settings_round_trip_without_secrets() {
        let folder = std::env::temp_dir().join(format!("snapshot-translate-test-{}", std::process::id()));
        let file = folder.join("translate.txt");
        assert_eq!(Config::load_from(&file).unwrap(), Config::default());
        let mut config = Config { provider: Some(Provider::OpenAi), ..Default::default() };
        config.set(Provider::OpenAi, Field::Endpoint, " https://api.deepseek.com/v1 ");
        config.set(Provider::OpenAi, Field::Model, "deepseek-chat");
        config.set(Provider::Baidu, Field::AppId, "2015");
        config.save_to(&file).unwrap();
        let loaded = Config::load_from(&file).unwrap();
        assert_eq!(loaded, config);
        assert_eq!(loaded.value(Provider::OpenAi, Field::Endpoint), "https://api.deepseek.com/v1");
        let text = fs::read_to_string(&file).unwrap();
        assert!(!text.contains("key") && !text.contains("secret"), "{text}");
        fs::remove_file(&file).unwrap();
        fs::remove_dir(&folder).unwrap();
    }
}
