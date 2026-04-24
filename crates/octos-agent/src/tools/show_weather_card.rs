//! Show-weather-card tool: a ONE-STEP weather card producer.
//!
//! # Why this exists
//!
//! The generic `send_app_card` tool requires the LLM to chain two tool calls
//! (first `get_weather` to fetch real data, then `send_app_card` to wrap it
//! in an `org.octos.app` envelope). In practice, Kimi k2.5 / k2.6 completes
//! the first step, sees the weather text, and goes straight to `EndTurn` with
//! a plain text reply instead of calling `send_app_card`. Even explicit
//! system-prompt rules like "you MUST call send_app_card after get_weather"
//! do not reliably change this behavior.
//!
//! This tool collapses the two steps into one: the LLM only needs to emit
//! a single `show_weather_card(city)` call. The tool internally:
//!
//! 1. Geocodes the city via open-meteo
//! 2. Fetches current weather via open-meteo
//! 3. Maps the returned weather code to the 6-way condition enum
//!    (`sunny`/`cloudy`/`rainy`/`snowy`/`stormy`/`foggy`) that matches
//!    Robrix's weather type registry in
//!    `robrix2:src/home/app_registry/weather.rs`
//! 4. Builds the full `initial_state` with real values (no hallucination)
//! 5. Emits a single `OutboundMessage` whose metadata contains
//!    `org.octos.app` — the Matrix channel then copies that verbatim into
//!    the outgoing Matrix event `content` so Robrix can render it as a
//!    native GPU weather card
//!
//! # No fabrication by construction
//!
//! Because the weather values come from `open-meteo.com` inside this tool,
//! not from the LLM, there is no path for the LLM to make up fake weather
//! data. The only thing the LLM provides is the city name.
//!
//! # Fallback
//!
//! If geocoding fails or the open-meteo API is unreachable, the tool
//! returns a non-success `ToolResult` with a human-readable error message.
//! The LLM can then reply with plain text explaining the failure — no
//! app envelope is emitted, so Robrix does not render a stale / blank
//! card.

use std::time::Duration;

use async_trait::async_trait;
use chrono::Utc;
use eyre::{Result, WrapErr};
use octos_core::OutboundMessage;
use reqwest::Client;
use serde::Deserialize;
use serde_json::{Value as JsonValue, json};
use tokio::sync::mpsc;

use super::{Tool, ToolResult};

const APP_REPLY_TAGS: &[&str] = &["gateway", "app_reply"];

/// Per-session tool: the channel/chat_id context is pre-bound so every
/// `execute` call knows where to deliver the outbound card.
pub struct ShowWeatherCardTool {
    out_tx: mpsc::Sender<OutboundMessage>,
    default_channel: std::sync::Mutex<String>,
    default_chat_id: std::sync::Mutex<String>,
    /// HTTP client used for open-meteo. Created once per tool instance
    /// so the TLS stack and connection pool are shared across calls.
    http: Client,
}

impl ShowWeatherCardTool {
    pub fn new(out_tx: mpsc::Sender<OutboundMessage>) -> Self {
        Self::build(out_tx, "", "")
    }

    pub fn with_context(
        out_tx: mpsc::Sender<OutboundMessage>,
        channel: impl Into<String>,
        chat_id: impl Into<String>,
    ) -> Self {
        Self::build(out_tx, channel, chat_id)
    }

    fn build(
        out_tx: mpsc::Sender<OutboundMessage>,
        channel: impl Into<String>,
        chat_id: impl Into<String>,
    ) -> Self {
        let http = Client::builder()
            .timeout(Duration::from_secs(12))
            .connect_timeout(Duration::from_secs(5))
            .user_agent("octos/show_weather_card")
            .build()
            .expect("reqwest client builder should never fail on a minimal config");
        Self {
            out_tx,
            default_channel: std::sync::Mutex::new(channel.into()),
            default_chat_id: std::sync::Mutex::new(chat_id.into()),
            http,
        }
    }

    /// Update the default channel/chat_id context (called per inbound
    /// message). Prefer `with_context` for per-session instances.
    pub fn set_context(&self, channel: &str, chat_id: &str) {
        *self
            .default_channel
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = channel.to_string();
        *self
            .default_chat_id
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = chat_id.to_string();
    }
}

#[derive(Deserialize)]
struct Input {
    /// City name. English names work best with open-meteo. Non-English
    /// names (e.g. "北京") are also accepted — the geocoding step falls
    /// back to language-specific search.
    city: String,
    /// Optional one-line fallback text shown on clients that don't
    /// render `org.octos.app`. Defaults to something like
    /// "Beijing 22°C sunny" if not provided.
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    language: Option<String>,
    #[serde(default)]
    channel: Option<String>,
    #[serde(default)]
    chat_id: Option<String>,
}

// Open-meteo geocoding response
#[derive(Deserialize)]
struct GeoResult {
    results: Option<Vec<GeoLocation>>,
}

#[derive(Deserialize, Clone)]
struct GeoLocation {
    latitude: f64,
    longitude: f64,
    name: String,
    #[serde(default)]
    timezone: Option<String>,
    #[serde(default)]
    population: Option<u64>,
}

// Open-meteo current weather response
#[derive(Deserialize)]
struct WeatherResponse {
    current: Option<CurrentWeather>,
    #[serde(default)]
    hourly: Option<HourlyWeather>,
    #[serde(default)]
    daily: Option<DailyWeather>,
}

#[derive(Deserialize)]
struct CurrentWeather {
    temperature_2m: Option<f64>,
    relative_humidity_2m: Option<f64>,
    apparent_temperature: Option<f64>,
    weather_code: Option<u32>,
    wind_speed_10m: Option<f64>,
}

#[derive(Deserialize, Default)]
struct HourlyWeather {
    #[serde(default)]
    time: Vec<String>,
    #[serde(default)]
    temperature_2m: Vec<f64>,
    #[serde(default)]
    weather_code: Vec<u32>,
    #[serde(default)]
    precipitation_probability: Vec<f64>,
}

#[derive(Deserialize, Default)]
struct DailyWeather {
    #[serde(default)]
    time: Vec<String>,
    #[serde(default)]
    temperature_2m_max: Vec<f64>,
    #[serde(default)]
    temperature_2m_min: Vec<f64>,
    #[serde(default)]
    precipitation_probability_max: Vec<f64>,
    #[serde(default)]
    uv_index_max: Vec<f64>,
}

/// Map a WMO weather code (same encoding open-meteo returns) to one of
/// the six visual condition enum values used by Robrix's weather type
/// registry. Any unknown code falls back to `sunny` rather than failing —
/// the card still displays usefully even if the condition chip is
/// approximate.
fn weather_code_to_condition(code: u32) -> &'static str {
    match code {
        0 | 1 => "sunny",             // Clear sky / Mainly clear
        2 | 3 => "cloudy",            // Partly cloudy / Overcast
        45 | 48 => "foggy",           // Fog / Rime fog
        51..=57 => "rainy",           // Drizzle / Freezing drizzle
        61..=67 => "rainy",           // Rain / Freezing rain
        80..=82 => "rainy",           // Rain showers
        71..=77 | 85 | 86 => "snowy", // Snow / Snow showers / Snow grains
        95 | 96 | 99 => "stormy",     // Thunderstorm
        _ => "sunny",
    }
}

/// Matrix event content uses canonical JSON, which only supports integer
/// numbers. Fractional JSON numbers inside `org.octos.app` cause Palpo to
/// panic while canonicalizing the event (`IntConvert`). Keep decimals in the
/// fallback text, but round structured numeric fields before embedding them
/// into Matrix event content.
fn matrix_safe_number(value: f64) -> JsonValue {
    JsonValue::from(value.round() as i64)
}

fn first_number(values: &[f64]) -> Option<JsonValue> {
    values.first().copied().map(matrix_safe_number)
}

fn parse_local_hour(raw: &str) -> Option<i32> {
    raw.get(11..13).and_then(|h| h.parse::<i32>().ok())
}

fn pick_period_payload(
    hourly: &HourlyWeather,
    target_date: &str,
    slot: &str,
    target_hour: i32,
) -> Option<JsonValue> {
    let mut best_idx: Option<usize> = None;
    let mut best_distance = i32::MAX;

    for (idx, raw_time) in hourly.time.iter().enumerate() {
        if !raw_time.starts_with(target_date) {
            continue;
        }
        let Some(hour) = parse_local_hour(raw_time) else {
            continue;
        };
        let distance = (hour - target_hour).abs();
        if distance < best_distance {
            best_distance = distance;
            best_idx = Some(idx);
        }
    }

    let idx = best_idx?;
    let temp_c = *hourly.temperature_2m.get(idx)?;
    let condition = hourly
        .weather_code
        .get(idx)
        .copied()
        .map(weather_code_to_condition)
        .unwrap_or("sunny");
    let precip = hourly
        .precipitation_probability
        .get(idx)
        .copied()
        .map(|value| value.round() as i64);

    let mut payload = serde_json::Map::new();
    payload.insert("slot".to_string(), JsonValue::String(slot.to_string()));
    payload.insert("temp_c".to_string(), matrix_safe_number(temp_c));
    payload.insert(
        "condition".to_string(),
        JsonValue::String(condition.to_string()),
    );
    if let Some(prob) = precip {
        payload.insert("precipitation_probability".to_string(), json!(prob));
    }
    Some(JsonValue::Object(payload))
}

fn build_periods_payload(hourly: &HourlyWeather, daily: &DailyWeather) -> Vec<JsonValue> {
    let Some(target_date) = daily.time.first() else {
        return Vec::new();
    };

    [("morning", 8), ("noon", 13), ("night", 20)]
        .into_iter()
        .filter_map(|(slot, hour)| pick_period_payload(hourly, target_date, slot, hour))
        .collect()
}

fn normalize_language_code(raw: &str) -> Option<&'static str> {
    match raw.trim() {
        "en" | "en-US" | "English" => Some("en"),
        "zh" | "zh-CN" | "zh_CN" | "ChineseSimplified" => Some("zh-CN"),
        _ => None,
    }
}

fn build_initial_state(
    location: &GeoLocation,
    weather: &WeatherResponse,
    language: Option<&str>,
) -> Result<serde_json::Map<String, JsonValue>> {
    let current = weather
        .current
        .as_ref()
        .ok_or_else(|| eyre::eyre!("open-meteo returned no current-weather block"))?;
    let temp_c = current
        .temperature_2m
        .ok_or_else(|| eyre::eyre!("open-meteo returned no temperature"))?;

    let condition = current
        .weather_code
        .map(weather_code_to_condition)
        .unwrap_or("sunny");

    let mut initial_state = serde_json::Map::new();
    initial_state.insert(
        "location".to_string(),
        JsonValue::String(location.name.clone()),
    );
    initial_state.insert("temp_c".to_string(), matrix_safe_number(temp_c));
    initial_state.insert(
        "condition".to_string(),
        JsonValue::String(condition.to_string()),
    );
    if let Some(h) = current.relative_humidity_2m {
        initial_state.insert("humidity".to_string(), json!(h.round() as i64));
    }
    if let Some(w) = current.wind_speed_10m {
        initial_state.insert("wind_kph".to_string(), matrix_safe_number(w));
    }
    if let Some(f) = current.apparent_temperature {
        initial_state.insert("feels_like_c".to_string(), matrix_safe_number(f));
    }
    initial_state.insert(
        "updated_at".to_string(),
        JsonValue::String(Utc::now().to_rfc3339()),
    );
    if let Some(language) = language.and_then(normalize_language_code) {
        initial_state.insert("language".to_string(), json!(language));
    }

    if let Some(daily) = weather.daily.as_ref() {
        if let Some(value) = first_number(&daily.temperature_2m_max) {
            initial_state.insert("high_c".to_string(), value);
        }
        if let Some(value) = first_number(&daily.temperature_2m_min) {
            initial_state.insert("low_c".to_string(), value);
        }
        if let Some(value) = first_number(&daily.precipitation_probability_max) {
            initial_state.insert("precipitation_probability_max".to_string(), value);
        }
        if let Some(value) = first_number(&daily.uv_index_max) {
            initial_state.insert("uv_index_max".to_string(), value);
        }
        if let Some(hourly) = weather.hourly.as_ref() {
            let periods = build_periods_payload(hourly, daily);
            if !periods.is_empty() {
                initial_state.insert("periods".to_string(), JsonValue::Array(periods));
            }
        }
    }

    Ok(initial_state)
}

/// Percent-encode a string for use in a URL query component. Minimal
/// implementation (good enough for city names with CJK / Latin / spaces);
/// avoids a new cargo dependency on `urlencoding`.
fn percent_encode(input: &str) -> String {
    let mut out = String::with_capacity(input.len() * 3);
    for byte in input.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char);
            }
            _ => {
                out.push('%');
                out.push_str(&format!("{byte:02X}"));
            }
        }
    }
    out
}

#[async_trait]
impl Tool for ShowWeatherCardTool {
    fn name(&self) -> &str {
        "show_weather_card"
    }

    fn description(&self) -> &str {
        "Show a native GPU-rendered weather card for a city. This is a \
         ONE-STEP tool — you do NOT need to call `get_weather` first. This \
         tool internally fetches real weather from open-meteo, maps it to \
         the Robrix weather card schema, and emits a message with a \
         structured `org.octos.app` envelope. Use this tool for ANY weather \
         query from the user. Capable clients (Robrix) render this as a \
         full-width GPU card showing city, temperature, conditions, \
         humidity, wind, and feels-like. Other clients fall back to the \
         `body` text. You provide the city name — and, when the user asks \
         for a specific reply language, also provide `language` (`zh-CN` \
         or `en`) — the tool handles \
         geocoding, real-data fetching, condition classification, and \
         envelope construction. NEVER call `get_weather` followed by \
         `send_app_card` for weather queries — use this tool instead. \
         After this tool succeeds, the card IS the reply; do NOT \
         additionally output a markdown text summary."
    }

    fn tags(&self) -> &[&str] {
        APP_REPLY_TAGS
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "city": {
                    "type": "string",
                    "description": "City name. Both English (\"Beijing\", \
                        \"New York\") and non-English (\"北京\") are accepted. \
                        English works most reliably with the open-meteo \
                        geocoder."
                },
                "body": {
                    "type": "string",
                    "description": "Optional one-line fallback text shown by \
                        clients that don't render the app envelope. If \
                        omitted, the tool auto-generates one like \
                        \"Beijing 22°C sunny\". Use the user's language \
                        if you set this explicitly."
                },
                "language": {
                    "type": "string",
                    "description": "Optional reply language for the card copy. \
                        Use `zh-CN` when the user asks for Chinese, and `en` \
                        for English. This overrides the capable client's \
                        global UI language for this card only."
                },
                "channel": {
                    "type": "string",
                    "description": "Target channel. Defaults to current."
                },
                "chat_id": {
                    "type": "string",
                    "description": "Target chat/user ID. Defaults to current."
                }
            },
            "required": ["city"]
        })
    }

    async fn execute(&self, args: &serde_json::Value) -> Result<ToolResult> {
        let input: Input = serde_json::from_value(args.clone())
            .wrap_err("invalid show_weather_card tool input")?;

        let channel = input.channel.unwrap_or_else(|| {
            self.default_channel
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
        });
        let chat_id = input.chat_id.unwrap_or_else(|| {
            self.default_chat_id
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
        });

        if channel.is_empty() || chat_id.is_empty() {
            return Ok(ToolResult {
                output: "Error: No target channel/chat specified.".into(),
                success: false,
                ..Default::default()
            });
        }

        if input.city.trim().is_empty() {
            return Ok(ToolResult {
                output: "Error: `city` must not be empty.".into(),
                success: false,
                ..Default::default()
            });
        }

        // Step 1: geocode the city.
        let location = match geocode(&self.http, &input.city).await {
            Ok(loc) => loc,
            Err(e) => {
                return Ok(ToolResult {
                    output: format!(
                        "Error: Could not find city '{}': {}. Try a more specific or English city name.",
                        input.city, e
                    ),
                    success: false,
                    ..Default::default()
                });
            }
        };

        // Step 2: fetch current + hourly + daily weather.
        let weather = match fetch_weather_bundle(&self.http, &location).await {
            Ok(w) => w,
            Err(e) => {
                return Ok(ToolResult {
                    output: format!("Error: Weather fetch for {} failed: {e}", location.name),
                    success: false,
                    ..Default::default()
                });
            }
        };

        // Step 3: build the `initial_state` object with real values.
        let initial_state =
            match build_initial_state(&location, &weather, input.language.as_deref()) {
                Ok(state) => state,
                Err(e) => {
                    return Ok(ToolResult {
                        output: format!(
                            "Error: Weather payload build for {} failed: {e}",
                            location.name
                        ),
                        success: false,
                        ..Default::default()
                    });
                }
            };
        let temp_c = weather
            .current
            .as_ref()
            .and_then(|current| current.temperature_2m)
            .unwrap_or_default();
        let condition = initial_state
            .get("condition")
            .and_then(JsonValue::as_str)
            .unwrap_or("sunny");

        // Step 4: build the fallback body text (user-visible on
        // non-Robrix clients).
        let body = input.body.unwrap_or_else(|| {
            format!(
                "{} {:.1}°C {}",
                location.name,
                temp_c,
                condition_fallback_text(condition)
            )
        });

        // Step 5: wrap in the `org.octos.app` metadata envelope. The
        // Matrix channel `send_matrix_message` path reads this key and
        // copies it into the outgoing event content.
        let metadata = json!({
            "org.octos.app": {
                "type": "weather",
                "version": 2,
                "initial_state": JsonValue::Object(initial_state),
            }
        });

        let msg = OutboundMessage {
            channel: channel.clone(),
            chat_id: chat_id.clone(),
            content: body,
            reply_to: None,
            media: vec![],
            metadata,
        };

        self.out_tx
            .send(msg)
            .await
            .map_err(|e| eyre::eyre!("failed to send weather card message: {e}"))?;

        Ok(ToolResult {
            output: String::new(),
            success: true,
            ..Default::default()
        })
    }
}

/// Pretty fallback string for the six conditions. Keep the vocabulary
/// simple so it renders well inside a one-line preview.
fn condition_fallback_text(condition: &str) -> &'static str {
    match condition {
        "sunny" => "sunny",
        "cloudy" => "cloudy",
        "rainy" => "rain",
        "snowy" => "snow",
        "stormy" => "thunderstorm",
        "foggy" => "fog",
        _ => "",
    }
}

/// Geocode a city name via open-meteo. Tries plain search first; if the
/// city contains non-ASCII characters, retries with language-specific
/// hints so we can resolve Chinese / Japanese / etc. names.
async fn geocode(client: &Client, city: &str) -> Result<GeoLocation> {
    let encoded = percent_encode(city);
    let has_non_ascii = city.bytes().any(|b| b > 127);

    let langs: &[&str] = if has_non_ascii {
        &["", "zh", "ja", "ko", "ru", "ar", "hi"]
    } else {
        &[""]
    };

    let mut last_err: Option<String> = None;
    for lang in langs {
        let url = if lang.is_empty() {
            format!("https://geocoding-api.open-meteo.com/v1/search?name={encoded}&count=5")
        } else {
            format!(
                "https://geocoding-api.open-meteo.com/v1/search?name={encoded}&count=5&language={lang}"
            )
        };

        let resp = match client.get(&url).send().await {
            Ok(r) => r,
            Err(e) => {
                last_err = Some(e.to_string());
                continue;
            }
        };
        let geo: GeoResult = match resp.json().await {
            Ok(g) => g,
            Err(e) => {
                last_err = Some(e.to_string());
                continue;
            }
        };
        if let Some(mut results) = geo.results {
            if !results.is_empty() {
                // Prefer the most populated result (avoids tiny hamlets).
                results.sort_by_key(|result| std::cmp::Reverse(result.population.unwrap_or(0)));
                return Ok(results.into_iter().next().unwrap());
            }
        }
    }

    Err(eyre::eyre!(
        "no results from open-meteo geocoding{}",
        last_err.map(|e| format!(": {e}")).unwrap_or_default()
    ))
}

/// Fetch current weather plus the day guidance inputs for the given
/// location via open-meteo.
async fn fetch_weather_bundle(client: &Client, location: &GeoLocation) -> Result<WeatherResponse> {
    let url = format!(
        "https://api.open-meteo.com/v1/forecast?latitude={}&longitude={}\
         &current=temperature_2m,relative_humidity_2m,apparent_temperature,weather_code,wind_speed_10m\
         &hourly=temperature_2m,weather_code,precipitation_probability\
         &daily=temperature_2m_max,temperature_2m_min,precipitation_probability_max,uv_index_max\
         &forecast_days=1\
         &timezone={}",
        location.latitude,
        location.longitude,
        location.timezone.as_deref().unwrap_or("auto"),
    );
    let resp = client
        .get(&url)
        .send()
        .await
        .wrap_err("weather request failed")?;
    let wr: WeatherResponse = resp
        .json()
        .await
        .wrap_err("weather response parse failed")?;
    if wr.current.is_none() {
        return Err(eyre::eyre!("open-meteo returned no current-weather block"));
    }
    Ok(wr)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn weather_code_table_covers_all_six_conditions() {
        assert_eq!(weather_code_to_condition(0), "sunny");
        assert_eq!(weather_code_to_condition(1), "sunny");
        assert_eq!(weather_code_to_condition(2), "cloudy");
        assert_eq!(weather_code_to_condition(3), "cloudy");
        assert_eq!(weather_code_to_condition(45), "foggy");
        assert_eq!(weather_code_to_condition(48), "foggy");
        assert_eq!(weather_code_to_condition(51), "rainy");
        assert_eq!(weather_code_to_condition(63), "rainy");
        assert_eq!(weather_code_to_condition(82), "rainy");
        assert_eq!(weather_code_to_condition(71), "snowy");
        assert_eq!(weather_code_to_condition(86), "snowy");
        assert_eq!(weather_code_to_condition(95), "stormy");
        assert_eq!(weather_code_to_condition(99), "stormy");
        // Unknown code falls back to sunny rather than failing.
        assert_eq!(weather_code_to_condition(42424), "sunny");
    }

    #[test]
    fn matrix_safe_number_rounds_fractional_values() {
        assert_eq!(matrix_safe_number(23.4), json!(23));
        assert_eq!(matrix_safe_number(23.5), json!(24));
        assert_eq!(matrix_safe_number(-1.6), json!(-2));
    }

    #[test]
    fn percent_encode_handles_ascii_and_non_ascii() {
        assert_eq!(percent_encode("Beijing"), "Beijing");
        assert_eq!(percent_encode("New York"), "New%20York");
        // CJK "北京" is 6 bytes in UTF-8.
        let encoded = percent_encode("北京");
        assert_eq!(encoded, "%E5%8C%97%E4%BA%AC");
    }

    #[test]
    fn condition_fallback_text_returns_expected_labels() {
        assert_eq!(condition_fallback_text("sunny"), "sunny");
        assert_eq!(condition_fallback_text("rainy"), "rain");
        assert_eq!(condition_fallback_text("stormy"), "thunderstorm");
        assert_eq!(condition_fallback_text("unknown_enum"), "");
    }

    #[test]
    fn build_initial_state_includes_v2_guidance_fields() {
        let location = GeoLocation {
            latitude: 39.9,
            longitude: 116.4,
            name: "Beijing".to_string(),
            timezone: Some("Asia/Shanghai".to_string()),
            population: Some(1),
        };
        let weather = WeatherResponse {
            current: Some(CurrentWeather {
                temperature_2m: Some(16.0),
                relative_humidity_2m: Some(81.0),
                apparent_temperature: Some(17.0),
                weather_code: Some(2),
                wind_speed_10m: Some(3.0),
            }),
            hourly: Some(HourlyWeather {
                time: vec![
                    "2026-04-16T08:00".to_string(),
                    "2026-04-16T13:00".to_string(),
                    "2026-04-16T20:00".to_string(),
                ],
                temperature_2m: vec![13.0, 24.0, 14.0],
                weather_code: vec![2, 0, 2],
                precipitation_probability: vec![10.0, 0.0, 5.0],
            }),
            daily: Some(DailyWeather {
                time: vec!["2026-04-16".to_string()],
                temperature_2m_max: vec![24.0],
                temperature_2m_min: vec![12.0],
                precipitation_probability_max: vec![10.0],
                uv_index_max: vec![6.0],
            }),
        };

        let initial_state =
            build_initial_state(&location, &weather, Some("zh-CN")).expect("state should build");
        assert_eq!(initial_state.get("high_c"), Some(&json!(24)));
        assert_eq!(initial_state.get("low_c"), Some(&json!(12)));
        assert_eq!(initial_state.get("language"), Some(&json!("zh-CN")));
        assert_eq!(
            initial_state.get("precipitation_probability_max"),
            Some(&json!(10))
        );
        assert_eq!(initial_state.get("uv_index_max"), Some(&json!(6)));

        let periods = initial_state
            .get("periods")
            .and_then(JsonValue::as_array)
            .expect("periods array");
        assert_eq!(periods.len(), 3);
        assert_eq!(periods[0]["slot"], json!("morning"));
        assert_eq!(periods[1]["slot"], json!("noon"));
        assert_eq!(periods[2]["slot"], json!("night"));
    }

    #[test]
    fn build_periods_payload_picks_target_hours_for_each_slot() {
        let hourly = HourlyWeather {
            time: vec![
                "2026-04-16T06:00".to_string(),
                "2026-04-16T08:00".to_string(),
                "2026-04-16T12:00".to_string(),
                "2026-04-16T13:00".to_string(),
                "2026-04-16T20:00".to_string(),
            ],
            temperature_2m: vec![10.0, 12.0, 22.0, 24.0, 15.0],
            weather_code: vec![2, 2, 0, 0, 45],
            precipitation_probability: vec![5.0, 10.0, 0.0, 0.0, 15.0],
        };
        let daily = DailyWeather {
            time: vec!["2026-04-16".to_string()],
            ..Default::default()
        };

        let periods = build_periods_payload(&hourly, &daily);
        assert_eq!(periods.len(), 3);
        assert_eq!(periods[0]["slot"], json!("morning"));
        assert_eq!(periods[0]["temp_c"], json!(12));
        assert_eq!(periods[1]["slot"], json!("noon"));
        assert_eq!(periods[1]["temp_c"], json!(24));
        assert_eq!(periods[2]["slot"], json!("night"));
        assert_eq!(periods[2]["temp_c"], json!(15));
    }

    #[test]
    fn normalize_language_code_maps_supported_aliases() {
        assert_eq!(normalize_language_code("zh"), Some("zh-CN"));
        assert_eq!(normalize_language_code("zh-CN"), Some("zh-CN"));
        assert_eq!(normalize_language_code("en-US"), Some("en"));
        assert_eq!(normalize_language_code("English"), Some("en"));
        assert_eq!(normalize_language_code("fr"), None);
    }

    #[tokio::test]
    async fn execute_rejects_empty_city() {
        let (tx, _rx) = mpsc::channel(1);
        let tool = ShowWeatherCardTool::with_context(tx, "matrix", "!room:example.org");
        let result = tool
            .execute(&serde_json::json!({ "city": "" }))
            .await
            .expect("tool run ok");
        assert!(!result.success);
        assert!(result.output.contains("city"));
    }

    #[tokio::test]
    async fn execute_rejects_missing_context() {
        // No channel/chat_id bound and none in the args.
        let (tx, _rx) = mpsc::channel(1);
        let tool = ShowWeatherCardTool::new(tx);
        let result = tool
            .execute(&serde_json::json!({ "city": "Beijing" }))
            .await
            .expect("tool run ok");
        assert!(!result.success);
        assert!(result.output.contains("channel"));
    }
}
