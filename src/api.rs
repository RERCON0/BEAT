//! Navidrome (Subsonic API) client. Auth is token-based: `t = md5(password +
//! salt)` with a random per-run salt, so the password itself never travels in
//! a URL. Everything is read with a size cap and errors are redacted.
//!
//! The token *does* travel in the query string, because that is what the
//! Subsonic protocol prescribes — which means any proxy, `netsh trace` or WAF
//! between BEAT and the server sees a full account credential. It cannot be
//! moved out of the URL, so the code does what it can instead: redirects are
//! followed only inside the configured origin (`redirect_policy`), and the
//! token, the salt and the password are scrubbed from every message the user
//! sees (`Client::redact`).

use crate::config::{Config, StreamFormat};
use md5::{Digest, Md5};
use serde::{Deserialize, Serialize};
use std::time::Duration;

const API_VERSION: &str = "1.16.1";
const CLIENT_NAME: &str = "BEAT";
/// Longest track length (seconds) taken from the server: anything above is a
/// bogus value that would break the seek bar and `Duration` arithmetic.
pub const MAX_DURATION_SECS: f64 = 1.0e7;
const MAX_JSON_BYTES: usize = 4 * 1024 * 1024;
const MAX_COVER_BYTES: usize = 8 * 1024 * 1024;

pub struct Server {
    pub base: String,
    pub user: String,
    pub password: String,
}

impl Server {
    pub fn from_config(cfg: &Config) -> Self {
        Self {
            base: cfg.server_url.trim().to_owned(),
            user: cfg.user.trim().to_owned(),
            password: cfg.password.clone(),
        }
    }

    pub fn ready(&self) -> bool {
        !self.base.is_empty() && !self.user.is_empty() && !self.password.is_empty()
    }

    /// Stable account identity, even when a saved password is unavailable.
    pub fn catalog_key(&self) -> Option<String> {
        let base = checked_base_url(&self.base).ok()?;
        let user = self.user.trim();
        (!user.is_empty()).then(|| md5_hex(&format!("{base}\0{user}")))
    }
}

#[derive(Clone)]
pub struct Client {
    http: reqwest::blocking::Client,
    stream_http: reqwest::blocking::Client,
    base: String,
    user: String,
    password: String,
    token: String,
    salt: String,
    format: StreamFormat,
    bit_rate: u32,
}

impl Client {
    pub fn new(server: &Server, format: StreamFormat, bit_rate: u32) -> Result<Self, String> {
        let base = checked_base_url(&server.base)?;
        let salt = random_salt();
        let token = md5_hex(&format!("{}{}", server.password, salt));
        let http = reqwest::blocking::Client::builder()
            .connect_timeout(Duration::from_secs(20))
            .timeout(Duration::from_secs(45))
            .redirect(redirect_policy(&base))
            .user_agent(concat!("beat/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| crate::i18n::trf!("HTTP-клиент: {e}", e = e))?;
        // Audio reads can pause between chunks while the server transcodes;
        // the timeout applies per read operation, not to the whole download.
        let stream_http = reqwest::blocking::Client::builder()
            .connect_timeout(Duration::from_secs(20))
            .timeout(Duration::from_secs(120))
            .redirect(redirect_policy(&base))
            .user_agent(concat!("beat/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| crate::i18n::trf!("HTTP-клиент: {e}", e = e))?;
        Ok(Self {
            http,
            stream_http,
            base,
            user: server.user.trim().to_owned(),
            password: server.password.clone(),
            token,
            salt,
            format,
            bit_rate,
        })
    }

    fn url(&self, view: &str, params: &[(&str, &str)]) -> Result<reqwest::Url, String> {
        let mut url = reqwest::Url::parse(&format!("{}/rest/{view}", self.base))
            .map_err(|_| crate::i18n::tr("некорректный адрес запроса").to_string())?;
        {
            let mut query = url.query_pairs_mut();
            query.append_pair("u", &self.user);
            query.append_pair("t", &self.token);
            query.append_pair("s", &self.salt);
            query.append_pair("v", API_VERSION);
            query.append_pair("c", CLIENT_NAME);
            query.append_pair("f", "json");
            for (key, value) in params {
                query.append_pair(key, value);
            }
        }
        Ok(url)
    }

    fn redact(&self, message: String) -> String {
        let mut out = message;
        if !self.password.is_empty() {
            out = out.replace(&self.password, crate::i18n::tr("[пароль скрыт]"));
        }
        if !self.token.is_empty() {
            out = out.replace(&self.token, crate::i18n::tr("[токен скрыт]"));
        }
        if !self.salt.is_empty() {
            out = out.replace(&self.salt, crate::i18n::tr("[salt скрыт]"));
        }
        out
    }

    fn get_json(&self, view: &str, params: &[(&str, &str)]) -> Result<serde_json::Value, String> {
        let url = self.url(view, params)?;
        let mut resp = self.http.get(url).send().map_err(|e| self.redact(describe_network_error(&e)))?;
        let status = resp.status();
        let body = read_capped(&mut resp, MAX_JSON_BYTES).map_err(|e| self.redact(e))?;
        let text =
            String::from_utf8(body).map_err(|_| crate::i18n::tr("сервер прислал некорректный UTF-8").to_string())?;
        if !status.is_success() {
            return Err(format!("HTTP {} — {}", status.as_u16(), snippet(&self.redact(text))));
        }
        parse_response(&text).map_err(|e| snippet(&self.redact(e)))
    }

    pub fn ping(&self) -> Result<(), String> {
        self.get_json("ping.view", &[])?;
        Ok(())
    }

    pub fn artists(&self) -> Result<Vec<Artist>, String> {
        let v = self.get_json("getArtists.view", &[])?;
        Ok(parse_artists(&v))
    }

    pub fn artist(&self, id: &str) -> Result<(Artist, Vec<Album>), String> {
        let v = self.get_json("getArtist.view", &[("id", id)])?;
        Ok(parse_artist(&v))
    }

    pub fn album(&self, id: &str) -> Result<(Album, Vec<Song>), String> {
        let v = self.get_json("getAlbum.view", &[("id", id)])?;
        Ok(parse_album(&v))
    }

    pub fn album_list(&self, kind: &str, size: u32, offset: u32) -> Result<Vec<Album>, String> {
        self.album_page(kind, size, offset, false)
    }

    pub fn catalog_albums(&self, size: u32, offset: u32) -> Result<Vec<Album>, String> {
        self.album_page("alphabeticalByName", size, offset, true)
    }

    fn album_page(&self, kind: &str, size: u32, offset: u32, strict: bool) -> Result<Vec<Album>, String> {
        let v = self.get_json(
            "getAlbumList2.view",
            &[("type", kind), ("size", &size.to_string()), ("offset", &offset.to_string())],
        )?;
        let Some(list) = v.get("albumList2").and_then(|list| list.as_object()) else {
            return Err(crate::i18n::tr("сервер не прислал список альбомов").into());
        };
        if list.get("album").is_some_and(|albums| !albums.is_array()) {
            return Err(crate::i18n::tr("сервер прислал некорректный список альбомов").into());
        }
        // A malformed entry is dropped rather than failing the page: this list
        // only fills the album grid, where one broken row from a non-Navidrome
        // server must not hide every other album. The full-library walk uses
        // `catalog_album`, which stays strict on purpose (see its doc comment).
        let albums = parse_album_list(&v);
        if strict
            && list.get("album").and_then(|items| items.as_array()).is_some_and(|items| items.len() != albums.len())
        {
            return Err(crate::i18n::tr("сервер прислал неполную страницу альбомов; полный обход прерван").into());
        }
        Ok(albums)
    }

    /// Strict variant for a full-library scan. The regular album view can
    /// display a partial reply, but a catalog baseline must not silently
    /// record it as complete and later treat old songs as newly added.
    pub fn catalog_album(&self, id: &str) -> Result<Vec<Song>, String> {
        let v = self.get_json("getAlbum.view", &[("id", id)])?;
        let Some(album) = v.get("album").and_then(|album| album.as_object()) else {
            return Err(crate::i18n::tr("сервер не прислал альбом для проверки библиотеки").into());
        };
        if album.get("id").and_then(|value| value.as_str()) != Some(id) {
            return Err(crate::i18n::tr("сервер прислал другой альбом для проверки библиотеки").into());
        }
        if album.get("song").is_some_and(|songs| !songs.is_array()) {
            return Err(crate::i18n::tr("сервер прислал некорректный список песен").into());
        }
        let songs = parse_album(&v).1;
        let declared_count = album
            .get("songCount")
            .map(|count| count.as_u64().or_else(|| count.as_str().and_then(|text| text.trim().parse::<u64>().ok())));
        if album.get("song").and_then(|songs| songs.as_array()).is_some_and(|raw| raw.len() != songs.len())
            || declared_count.is_some_and(|count| count != Some(songs.len() as u64))
        {
            return Err(crate::i18n::tr("сервер прислал неполный список песен альбома").into());
        }
        Ok(songs)
    }

    /// Separate on-disk catalog checkpoints for each server and user, without
    /// putting the URL or username in the file name.
    pub fn catalog_key(&self) -> String {
        md5_hex(&format!("{}\0{}", self.base, self.user))
    }

    pub fn search(&self, query: &str) -> Result<SearchResult, String> {
        let v = self.get_json(
            "search3.view",
            &[("query", query), ("artistCount", "8"), ("albumCount", "8"), ("songCount", "30")],
        )?;
        Ok(parse_search(&v))
    }

    pub fn cover_bytes(&self, cover_id: &str, size: u32) -> Result<Vec<u8>, String> {
        let url = self.url("getCoverArt.view", &[("id", cover_id), ("size", &size.to_string())])?;
        let mut resp = self.http.get(url).send().map_err(|e| self.redact(describe_network_error(&e)))?;
        let status = resp.status();
        if !status.is_success() {
            let body = read_capped(&mut resp, MAX_COVER_BYTES).unwrap_or_default();
            let text = String::from_utf8_lossy(&body).into_owned();
            return Err(crate::i18n::trf!("обложка: HTTP {} — {}", status.as_u16(), snippet(&self.redact(text))));
        }
        read_capped(&mut resp, MAX_COVER_BYTES).map_err(|e| self.redact(e))
    }

    /// Opens the audio stream for a song; the caller reads it (and reports
    /// progress) while writing the cache file.
    pub fn open_stream(&self, song: &Song) -> Result<AudioStream, String> {
        let (format, suffix, max_bit_rate) = match self.format {
            StreamFormat::Raw => ("raw", safe_suffix(&song.suffix), None),
            StreamFormat::Mp3 => ("mp3", "mp3".to_owned(), Some(self.bit_rate.to_string())),
        };
        let mut params = vec![("id", song.id.as_str()), ("estimateContentLength", "true"), ("format", format)];
        if let Some(rate) = max_bit_rate.as_deref() {
            params.push(("maxBitRate", rate));
        }
        let url = self.url("stream.view", &params)?;
        let mut resp = self.stream_http.get(url).send().map_err(|e| self.redact(describe_network_error(&e)))?;
        let status = resp.status();
        if !status.is_success() {
            let body = read_capped(&mut resp, 256 * 1024).unwrap_or_default();
            let text = String::from_utf8_lossy(&body).into_owned();
            return Err(crate::i18n::trf!("поток: HTTP {} — {}", status.as_u16(), snippet(&self.redact(text))));
        }
        let total = content_length(&resp);
        // The server's content type is more reliable than the song suffix
        // some scanners leave empty or wrong; it only names the cache file,
        // decoding still probes the bytes.
        let suffix = match self.format {
            StreamFormat::Raw => resp
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok())
                .and_then(suffix_from_content_type)
                .unwrap_or(suffix),
            StreamFormat::Mp3 => suffix,
        };
        Ok(AudioStream { reader: Box::new(resp), total, suffix })
    }
}

fn suffix_from_content_type(value: &str) -> Option<String> {
    let codec = value.split(';').next().unwrap_or_default().trim().to_ascii_lowercase();
    let suffix = match codec.as_str() {
        "audio/mpeg" | "audio/mp3" => "mp3",
        "audio/flac" | "audio/x-flac" => "flac",
        "audio/ogg" | "application/ogg" => "ogg",
        "audio/opus" => "opus",
        "audio/webm" | "video/webm" => "webm",
        "audio/mp4" | "audio/m4a" | "audio/x-m4a" => "m4a",
        "audio/wav" | "audio/x-wav" | "audio/wave" => "wav",
        _ => return None,
    };
    Some(suffix.to_owned())
}

pub struct AudioStream {
    pub reader: Box<dyn std::io::Read + Send>,
    pub total: Option<u64>,
    pub suffix: String,
}

fn content_length(resp: &reqwest::blocking::Response) -> Option<u64> {
    resp.content_length().filter(|len| *len > 0)
}

const MAX_REDIRECTS: usize = 5;

/// Every request carries the account name, token and salt in its query, and a
/// redirect target usually repeats that query. So a redirect is followed only
/// inside the configured origin (same scheme, host and port): that keeps the
/// HTTPS-only rule from being bypassed and the token from reaching another
/// host. The refusal never names the target, whose query holds the token.
fn redirect_policy(base: &str) -> reqwest::redirect::Policy {
    let base = reqwest::Url::parse(base).ok();
    reqwest::redirect::Policy::custom(move |attempt| {
        if attempt.previous().len() >= MAX_REDIRECTS {
            return attempt.error(crate::i18n::tr("слишком много перенаправлений"));
        }
        match &base {
            Some(base) if same_origin(base, attempt.url()) => attempt.follow(),
            _ => attempt.error(crate::i18n::tr(
                "сервер перенаправляет на другой адрес; укажите в настройках итоговый адрес сервера",
            )),
        }
    })
}

fn same_origin(a: &reqwest::Url, b: &reqwest::Url) -> bool {
    a.scheme() == b.scheme() && a.host_str() == b.host_str() && a.port_or_known_default() == b.port_or_known_default()
}

/// Same rule as the rest of the family: HTTPS only, HTTP just for localhost.
fn checked_base_url(raw: &str) -> Result<String, String> {
    let url = reqwest::Url::parse(raw.trim()).map_err(|_| crate::i18n::tr("некорректный адрес сервера").to_string())?;
    let host = url.host_str().ok_or(crate::i18n::tr("адрес сервера без хоста"))?;
    // IPv6 hosts come back in brackets: `[::1]`.
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    let local = host == "localhost" || bare.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback());
    if (url.scheme() != "https" && !(url.scheme() == "http" && local))
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(crate::i18n::tr("адрес сервера должен быть HTTPS (HTTP допустим только для localhost) и без логина, параметров или фрагмента").into());
    }
    Ok(url.as_str().trim_end_matches('/').to_owned())
}

/// 128 random bits as hex. `RandomState` is keyed from the OS random
/// generator, so no extra crate is needed; the salt must not be guessable
/// from the process id or the clock.
fn random_salt() -> String {
    use std::hash::{BuildHasher, Hasher};
    let mut salt = String::with_capacity(32);
    for _ in 0..2 {
        let hasher = std::collections::hash_map::RandomState::new().build_hasher();
        salt.push_str(&format!("{:016x}", hasher.finish()));
    }
    salt
}

pub(crate) fn md5_hex(input: &str) -> String {
    let mut hasher = Md5::new();
    hasher.update(input.as_bytes());
    let digest = hasher.finalize();
    let mut out = String::with_capacity(32);
    for byte in digest {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// Extensions are used for cache file names and a symphonia hint: keep them
/// short, alphanumeric, lowercase.
fn safe_suffix(suffix: &str) -> String {
    let cleaned: String =
        suffix.chars().filter(|c| c.is_ascii_alphanumeric()).take(5).collect::<String>().to_ascii_lowercase();
    match cleaned.as_str() {
        "mp3" | "flac" | "ogg" | "oga" | "opus" | "webm" | "mka" | "wav" | "m4a" | "aac" | "mp4" | "wv" => cleaned,
        _ => "mp3".to_owned(),
    }
}

fn read_capped(resp: &mut reqwest::blocking::Response, cap: usize) -> Result<Vec<u8>, String> {
    use std::io::Read;
    let mut bytes = Vec::new();
    resp.by_ref()
        .take(cap as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| crate::i18n::trf!("не удалось прочитать ответ сервера: {e}", e = e))?;
    if bytes.len() > cap {
        return Err(crate::i18n::tr("ответ сервера слишком большой").into());
    }
    Ok(bytes)
}

fn describe_network_error(e: &reqwest::Error) -> String {
    if e.is_timeout() {
        return crate::i18n::tr("сеть: сервер не отвечает; проверьте адрес и соединение").into();
    }
    let mut root = e.to_string();
    let mut source: Option<&(dyn std::error::Error + 'static)> = std::error::Error::source(e);
    while let Some(err) = source {
        root = err.to_string();
        source = err.source();
    }
    crate::i18n::trf!("сеть: {root}", root = root)
}

fn snippet(body: &str) -> String {
    let flat: String = body.chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
    let flat = flat.trim();
    if flat.chars().count() > 300 {
        let cut: String = flat.chars().take(300).collect();
        format!("{cut}…")
    } else {
        flat.to_owned()
    }
}

// ---------------------------------------------------------------------------
// Response parsing. Every field is optional: Navidrome omits what it does not
// know, and some servers send numbers as strings.
// ---------------------------------------------------------------------------

pub const MAX_TEXT_CHARS: usize = 1024;

pub fn bounded_text(text: &str) -> String {
    text.chars().take(MAX_TEXT_CHARS).collect()
}

pub(crate) fn de_text<'de, D: serde::Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    String::deserialize(d).map(|text| bounded_text(&text))
}

fn de_suffix<'de, D: serde::Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    String::deserialize(d).map(|suffix| safe_suffix(&suffix))
}

/// Local rows and cover files use reserved namespaces. A server cannot name
/// a track in those namespaces and make its row refer to a local file.
pub fn remote_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 1024
        && !id.contains('\0')
        && !["local:", "local-album:", "local-artist:", "file:"].iter().any(|prefix| id.starts_with(prefix))
}

fn remote_album(mut album: Album) -> Album {
    if !remote_id(&album.cover_id) {
        album.cover_id.clear();
    }
    album
}

fn remote_song(mut song: Song) -> Song {
    if !remote_id(&song.cover_id) {
        song.cover_id.clear();
    }
    song
}

fn de_u64<'de, D: serde::Deserializer<'de>>(d: D) -> Result<u64, D::Error> {
    d.deserialize_any(U64Visitor)
}

/// Server numbers arrive as numbers or as strings, and a field the server did
/// not fill in arrives as `null` or a bool. A tolerant visitor keeps one odd
/// field from discarding the whole artist/album/song it belongs to.
struct U64Visitor;

impl<'de> serde::de::Visitor<'de> for U64Visitor {
    type Value = u64;

    fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("a number or a numeric string")
    }
    fn visit_u64<E>(self, value: u64) -> Result<u64, E> {
        Ok(value)
    }
    fn visit_i64<E>(self, value: i64) -> Result<u64, E> {
        Ok(value.max(0) as u64)
    }
    fn visit_f64<E>(self, value: f64) -> Result<u64, E> {
        Ok(if value.is_finite() { value.max(0.0) as u64 } else { 0 })
    }
    fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<u64, E> {
        Ok(value.trim().parse().unwrap_or(0))
    }
    fn visit_unit<E>(self) -> Result<u64, E> {
        Ok(0)
    }
    fn visit_bool<E>(self, _: bool) -> Result<u64, E> {
        Ok(0)
    }
}

fn de_u32<'de, D: serde::Deserializer<'de>>(d: D) -> Result<u32, D::Error> {
    de_u64(d).map(|v| v.min(u32::MAX as u64) as u32)
}

fn de_f64<'de, D: serde::Deserializer<'de>>(d: D) -> Result<f64, D::Error> {
    let seconds = d.deserialize_any(F64Visitor)?;
    Ok(if seconds.is_finite() { seconds.clamp(0.0, MAX_DURATION_SECS) } else { 0.0 })
}

struct F64Visitor;

impl<'de> serde::de::Visitor<'de> for F64Visitor {
    type Value = f64;

    fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("a number or a numeric string")
    }
    fn visit_f64<E>(self, value: f64) -> Result<f64, E> {
        Ok(value)
    }
    fn visit_u64<E>(self, value: u64) -> Result<f64, E> {
        Ok(value as f64)
    }
    fn visit_i64<E>(self, value: i64) -> Result<f64, E> {
        Ok(value as f64)
    }
    fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<f64, E> {
        Ok(value.trim().parse().unwrap_or(0.0))
    }
    fn visit_unit<E>(self) -> Result<f64, E> {
        Ok(0.0)
    }
    fn visit_bool<E>(self, _: bool) -> Result<f64, E> {
        Ok(0.0)
    }
}

/// Whole seconds, bounded like `de_f64`.
fn de_secs<'de, D: serde::Deserializer<'de>>(d: D) -> Result<u64, D::Error> {
    de_f64(d).map(|seconds| seconds as u64)
}

#[derive(Deserialize, Clone, Default, Debug)]
pub struct Artist {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    #[serde(deserialize_with = "de_text")]
    pub name: String,
    #[serde(default, rename = "albumCount", deserialize_with = "de_u32")]
    pub album_count: u32,
}

#[derive(Deserialize, Clone, Default, Debug)]
pub struct Album {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    #[serde(deserialize_with = "de_text")]
    pub name: String,
    #[serde(default)]
    #[serde(deserialize_with = "de_text")]
    pub artist: String,
    #[serde(default, rename = "coverArt")]
    pub cover_id: String,
    #[serde(default, deserialize_with = "de_u32")]
    pub year: u32,
    #[serde(default, deserialize_with = "de_secs")]
    pub duration: u64,
}

#[derive(Serialize, Deserialize, Clone, Default, Debug)]
pub struct Song {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    #[serde(deserialize_with = "de_text")]
    pub title: String,
    #[serde(default)]
    #[serde(deserialize_with = "de_text")]
    pub artist: String,
    #[serde(default)]
    #[serde(deserialize_with = "de_text")]
    pub album: String,
    /// Cover id of the song, for the unified library list. `rename` on both
    /// ser and de keeps the on-disk cache in the Subsonic spelling.
    #[serde(default, rename = "coverArt")]
    pub cover_id: String,
    #[serde(default, deserialize_with = "de_u32")]
    pub track: u32,
    #[serde(default, deserialize_with = "de_f64")]
    pub duration: f64,
    #[serde(default)]
    #[serde(deserialize_with = "de_suffix")]
    pub suffix: String,
}

#[derive(Deserialize, Clone, Default, Debug)]
pub struct SearchResult {
    #[serde(default)]
    pub artists: Vec<Artist>,
    #[serde(default)]
    pub albums: Vec<Album>,
    #[serde(default)]
    pub songs: Vec<Song>,
}

/// `subsonic-response` envelope: `status` + an optional `error` object.
fn parse_response(body: &str) -> Result<serde_json::Value, String> {
    let mut value: serde_json::Value =
        serde_json::from_str(body).map_err(|_| crate::i18n::tr("не удалось разобрать ответ сервера").to_string())?;
    // Taken out of the envelope, not cloned: the payload can be megabytes.
    let response = value
        .get_mut("subsonic-response")
        .map(serde_json::Value::take)
        .ok_or(crate::i18n::tr("ответ не похож на Subsonic API (нет subsonic-response)"))?;
    if response.get("status").and_then(|s| s.as_str()) != Some("ok") {
        let message = response
            .get("error")
            .and_then(|e| e.get("message"))
            .and_then(|m| m.as_str())
            .unwrap_or(crate::i18n::tr("сервер отклонил запрос"));
        let code = response.get("error").and_then(|e| e.get("code")).and_then(|c| c.as_u64());
        return Err(match code {
            Some(code) => crate::i18n::trf!("сервер: {message} (код {code})", code = code, message = message),
            None => crate::i18n::trf!("сервер: {message}", message = message),
        });
    }
    Ok(response)
}

fn parse_artists(response: &serde_json::Value) -> Vec<Artist> {
    let mut artists = Vec::new();
    if let Some(indexes) = response.pointer("/artists/index").and_then(|i| i.as_array()) {
        for index in indexes {
            if let Some(list) = index.get("artist").and_then(|a| a.as_array()) {
                for artist in list {
                    match serde_json::from_value::<Artist>(artist.clone()) {
                        Ok(artist) if remote_id(&artist.id) => artists.push(artist),
                        _ => {}
                    }
                }
            }
        }
    }
    artists.sort_by_cached_key(|artist| artist.name.to_lowercase());
    let mut ids = std::collections::HashSet::new();
    artists.retain(|artist| ids.insert(artist.id.clone()));
    artists
}

fn parse_artist(response: &serde_json::Value) -> (Artist, Vec<Album>) {
    let Some(node) = response.get("artist") else { return (Artist::default(), Vec::new()) };
    let artist = serde_json::from_value::<Artist>(node.clone()).unwrap_or_default();
    let mut albums: Vec<Album> = node
        .get("album")
        .and_then(|a| a.as_array())
        .map(|list| {
            list.iter()
                .filter_map(|a| serde_json::from_value::<Album>(a.clone()).ok())
                .filter(|a| remote_id(&a.id))
                .map(remote_album)
                .collect()
        })
        .unwrap_or_default();
    albums.sort_by_cached_key(|album| (album.year, album.name.to_lowercase()));
    (artist, albums)
}

fn parse_album(response: &serde_json::Value) -> (Album, Vec<Song>) {
    let Some(node) = response.get("album") else { return (Album::default(), Vec::new()) };
    let album = remote_album(serde_json::from_value::<Album>(node.clone()).unwrap_or_default());
    let mut songs: Vec<Song> = node
        .get("song")
        .and_then(|s| s.as_array())
        .map(|list| {
            list.iter()
                .filter_map(|s| serde_json::from_value::<Song>(s.clone()).ok())
                .filter(|s| remote_id(&s.id))
                .map(remote_song)
                .collect()
        })
        .unwrap_or_default();
    songs.sort_by_key(|s| s.track);
    (album, songs)
}

fn parse_album_list(response: &serde_json::Value) -> Vec<Album> {
    response
        .pointer("/albumList2/album")
        .and_then(|a| a.as_array())
        .map(|list| {
            list.iter()
                .filter_map(|a| serde_json::from_value::<Album>(a.clone()).ok())
                .filter(|a| remote_id(&a.id))
                .map(remote_album)
                .collect()
        })
        .unwrap_or_default()
}

fn parse_search(response: &serde_json::Value) -> SearchResult {
    let Some(node) = response.get("searchResult3") else { return SearchResult::default() };
    let list = |key: &str| node.get(key).and_then(|a| a.as_array());
    SearchResult {
        artists: list("artist")
            .into_iter()
            .flatten()
            .take(8)
            .filter_map(|a| serde_json::from_value::<Artist>(a.clone()).ok())
            .filter(|a| remote_id(&a.id))
            .collect(),
        albums: list("album")
            .into_iter()
            .flatten()
            .take(8)
            .filter_map(|a| serde_json::from_value::<Album>(a.clone()).ok())
            .filter(|a| remote_id(&a.id))
            .map(remote_album)
            .collect(),
        songs: list("song")
            .into_iter()
            .flatten()
            .take(30)
            .filter_map(|s| serde_json::from_value::<Song>(s.clone()).ok())
            .filter(|s| remote_id(&s.id))
            .map(remote_song)
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secrets_are_redacted_before_a_server_error_is_shortened() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let server = Server {
            base: format!("http://{}", listener.local_addr().unwrap()),
            user: "test".into(),
            password: "very-long-private-password".into(),
        };
        let client = Client::new(&server, StreamFormat::Raw, 320).unwrap();
        let responses = std::sync::Mutex::new(std::collections::VecDeque::from([
            (500, format!("{}{}", "x".repeat(290), server.password)),
            (500, format!("{}{}", "x".repeat(290), client.token)),
            (200, serde_json::json!({"subsonic-response":{"status":"failed","error":{"message":format!("{}{}", "x".repeat(290), client.salt)}}}).to_string()),
            (500, format!("{}{}", "x".repeat(290), server.password)),
            (500, format!("{}{}", "x".repeat(290), client.token)),
        ]));
        let worker = serve(listener, 5, move |_| {
            let (status, body) = responses.lock().unwrap().pop_front().unwrap();
            format!("HTTP/1.1 {status} Test\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len())
        });
        let mut errors = (0..3).map(|_| client.ping().unwrap_err()).collect::<Vec<_>>();
        errors.push(client.cover_bytes("1", 96).unwrap_err());
        errors.push(client.open_stream(&Song::default()).err().unwrap());
        worker.join().unwrap();
        for error in errors {
            assert!(error.chars().count() < 400);
            for secret in [&server.password, &client.token, &client.salt] {
                assert!(!error.contains(&secret[..10]), "a partial credential reached the UI");
            }
        }
        assert_eq!(server.catalog_key().unwrap(), client.catalog_key());
    }

    #[test]
    fn a_full_catalog_rejects_a_page_that_the_album_view_can_partially_display() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let client = Client::new(
            &Server {
                base: format!("http://{}", listener.local_addr().unwrap()),
                user: "u".into(),
                password: "p".into(),
            },
            StreamFormat::Raw,
            320,
        )
        .unwrap();
        let worker = serve(listener, 2, |_| {
            let body = r#"{"subsonic-response":{"status":"ok","albumList2":{"album":[{"id":"1"},{"id":"local-album:spoof"}]}}}"#;
            format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len())
        });
        assert_eq!(client.album_list("newest", 100, 0).unwrap().len(), 1);
        assert!(client.catalog_albums(100, 0).is_err());
        worker.join().unwrap();
    }

    #[test]
    fn remote_metadata_cannot_impersonate_local_files_or_grow_unbounded_labels() {
        let value = serde_json::json!({"album":{"id":"1","song":[{"id":"local:victim.mp3"},{"id":"2","title":"я".repeat(100_000),"coverArt":"file:private.mp3"}]}});
        let (_, songs) = parse_album(&value);
        assert_eq!(songs.len(), 1);
        assert_eq!(songs[0].title.chars().count(), MAX_TEXT_CHARS);
        assert!(songs[0].cover_id.is_empty());
        assert!(!remote_id(&"a".repeat(1025)));
    }

    #[test]
    fn base_url_must_be_secure_and_clean() {
        assert!(checked_base_url("http://music.example.com").is_err());
        assert!(checked_base_url("https://user:pass@music.example.com").is_err());
        assert!(checked_base_url("https://music.example.com/?x=1").is_err());
        assert_eq!(checked_base_url("https://music.example.com/").unwrap(), "https://music.example.com");
        assert_eq!(checked_base_url("http://127.0.0.1:4533").unwrap(), "http://127.0.0.1:4533");
        assert_eq!(checked_base_url("http://localhost:4533/").unwrap(), "http://localhost:4533");
    }

    #[test]
    fn token_is_md5_of_password_plus_salt() {
        // Well-known vectors pin the implementation.
        assert_eq!(md5_hex(""), "d41d8cd98f00b204e9800998ecf8427e");
        assert_eq!(md5_hex("abc"), "900150983cd24fb0d6963f7d28e17f72");
    }

    #[test]
    fn safe_suffix_keeps_known_audio_extensions_only() {
        assert_eq!(safe_suffix("FLAC"), "flac");
        assert_eq!(safe_suffix("mp3"), "mp3");
        assert_eq!(safe_suffix("../../exe"), "mp3");
        assert_eq!(safe_suffix(""), "mp3");
        assert_eq!(safe_suffix("wav!"), "wav");
    }

    #[test]
    fn content_type_names_the_cache_extension() {
        assert_eq!(suffix_from_content_type("audio/flac").as_deref(), Some("flac"));
        assert_eq!(suffix_from_content_type("audio/ogg; codecs=opus").as_deref(), Some("ogg"));
        assert_eq!(suffix_from_content_type("audio/mpeg").as_deref(), Some("mp3"));
        assert_eq!(suffix_from_content_type("text/html"), None);
    }

    #[test]
    fn ipv6_loopback_is_local_over_http_but_other_addresses_are_not() {
        assert_eq!(checked_base_url("http://[::1]:4533/").unwrap(), "http://[::1]:4533");
        assert!(checked_base_url("http://[2001:db8::1]:4533").is_err());
        assert!(checked_base_url("http://192.168.1.5:4533").is_err());
        assert!(checked_base_url("https://[2001:db8::1]:4533").is_ok());
    }

    #[test]
    fn every_client_gets_its_own_random_salt() {
        let server = Server { base: "https://music.example".into(), user: "u".into(), password: "hunter2".into() };
        let first = Client::new(&server, StreamFormat::Raw, 320).unwrap();
        let second = Client::new(&server, StreamFormat::Raw, 320).unwrap();
        for client in [&first, &second] {
            assert_eq!(client.salt.len(), 32, "{}", client.salt);
            assert!(client.salt.chars().all(|c| c.is_ascii_hexdigit()));
            assert_eq!(client.token, md5_hex(&format!("hunter2{}", client.salt)));
        }
        assert_ne!(first.salt, second.salt);
        // Not built from the process id or the clock, which anyone can guess.
        let pid = format!("{:x}", std::process::id());
        assert!(!first.salt.starts_with(&pid), "{}", first.salt);
    }

    #[test]
    fn a_bogus_track_duration_from_the_server_is_clamped() {
        let body = r#"{"subsonic-response":{"status":"ok","album":{"id":"9","name":"R","duration":1e300,"song":[
            {"id":"a","title":"Huge","duration":1e300},
            {"id":"b","title":"Negative","duration":-5},
            {"id":"c","title":"Text","duration":"1e400"}
        ]}}}"#;
        let response = parse_response(body).unwrap();
        let (album, songs) = parse_album(&response);
        assert!(album.duration as f64 <= MAX_DURATION_SECS, "{}", album.duration);
        assert_eq!(songs[0].duration, MAX_DURATION_SECS);
        assert_eq!(songs[1].duration, 0.0);
        assert!(songs[2].duration.is_finite() && songs[2].duration <= MAX_DURATION_SECS);
    }

    #[test]
    fn artists_parse_and_sort() {
        let body = r#"{"subsonic-response":{"status":"ok","artists":{"index":[
            {"name":"b","artist":[{"id":"2","name":"Beta","albumCount":1}]},
            {"name":"a","artist":[{"id":"1","name":"alpha","albumCount":"3"}]}
        ]}}}"#;
        let response = parse_response(body).unwrap();
        let artists = parse_artists(&response);
        assert_eq!(artists.len(), 2);
        assert_eq!(artists[0].name, "alpha");
        assert_eq!(artists[0].album_count, 3);
        assert_eq!(artists[1].id, "2");
    }

    #[test]
    fn artists_with_the_same_id_are_not_listed_twice() {
        let value = serde_json::json!({"artists": {"index": [{"artist": [
            {"id": "same", "name": "Alpha"}, {"id": "other", "name": "Bravo"},
            {"id": "same", "name": "Zulu"}
        ]}]}});
        let artists = parse_artists(&value);
        assert_eq!(artists.iter().map(|a| a.id.as_str()).collect::<Vec<_>>(), ["same", "other"]);
    }

    #[test]
    fn album_and_search_parse_typical_payloads() {
        let body = r#"{"subsonic-response":{"status":"ok","album":{"id":"9","name":"Record","artist":"Band","artistId":"7","coverArt":"al-9","year":2024,"songCount":2,"duration":341,"song":[
            {"id":"b","title":"Second","artist":"Band","album":"Record","albumId":"9","track":2,"duration":180.5,"suffix":"flac","size":44100000,"bitRate":1411},
            {"id":"a","title":"First","track":1,"duration":161,"suffix":"mp3"}
        ]}}}"#;
        let response = parse_response(body).unwrap();
        let (album, songs) = parse_album(&response);
        assert_eq!(album.name, "Record");
        assert_eq!(album.cover_id, "al-9");
        assert_eq!(album.year, 2024);
        assert_eq!(songs.len(), 2);
        assert_eq!(songs[0].id, "a");
        assert_eq!(songs[1].duration, 180.5);
        assert_eq!(songs[1].suffix, "flac");

        let body = r#"{"subsonic-response":{"status":"ok","searchResult3":{"artist":[{"id":"1","name":"A"}],"album":[{"id":"2","name":"B"}],"song":[{"id":"3","title":"C"}]}}}"#;
        let response = parse_response(body).unwrap();
        let search = parse_search(&response);
        assert_eq!(search.artists.len(), 1);
        assert_eq!(search.albums.len(), 1);
        assert_eq!(search.songs[0].title, "C");
    }

    #[test]
    fn subsonic_error_status_is_reported() {
        let body =
            r#"{"subsonic-response":{"status":"failed","error":{"code":40,"message":"Wrong username or password"}}}"#;
        let err = parse_response(body).unwrap_err();
        assert!(err.contains("Wrong username or password"));
        assert!(err.contains("40"));
        assert!(parse_response("<html>bad gateway</html>").is_err());
        assert!(parse_response(r#"{"other":1}"#).is_err());
    }

    #[test]
    fn catalog_album_refuses_incomplete_or_mismatched_payloads() {
        // (body, must_fail): a full-library baseline may only be recorded from
        // a reply that provably lists every song of the album.
        let scenarios = [
            (r#"{"subsonic-response":{"status":"ok","album":{"id":"9","songCount":2,"song":[{"id":"a"}]}}}"#, true),
            (r#"{"subsonic-response":{"status":"ok","album":{"id":"9","song":[{"id":""}]}}}"#, true),
            (r#"{"subsonic-response":{"status":"ok","album":{"id":"8","song":[{"id":"a"}]}}}"#, true),
            (r#"{"subsonic-response":{"status":"ok","album":{"id":"9","songCount":"","song":[{"id":"a"}]}}}"#, true),
            (
                r#"{"subsonic-response":{"status":"ok","album":{"id":"9","songCount":"2","song":[{"id":"a"},{"id":"b"}]}}}"#,
                false,
            ),
        ];
        for (body, must_fail) in scenarios {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let base = format!("http://{}/", listener.local_addr().unwrap());
            let reply = format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
            let server = serve(listener, 1, move |_| reply.clone());
            let client =
                Client::new(&Server { base, user: "u".into(), password: "p".into() }, StreamFormat::Raw, 320).unwrap();
            let result = client.catalog_album("9");
            server.join().unwrap();
            assert_eq!(result.is_err(), must_fail, "{body}");
        }
    }

    #[test]
    fn one_odd_field_does_not_discard_the_whole_object() {
        // A server that sends `null` or a bool where a number belongs used to
        // make the entire artist/album fail to parse and vanish from the list.
        let body = r#"{"subsonic-response":{"status":"ok","artists":{"index":[
            {"name":"a","artist":[{"id":"1","name":"Alpha","albumCount":null}]},
            {"name":"b","artist":[{"id":"2","name":"Beta","albumCount":true}]},
            {"name":"c","artist":[{"id":"3","name":"Gamma","albumCount":"7"}]}
        ]}}}"#;
        let response = parse_response(body).unwrap();
        let artists = parse_artists(&response);
        assert_eq!(artists.len(), 3, "artists were dropped: {artists:?}");
        assert_eq!(artists.iter().map(|a| a.album_count).collect::<Vec<_>>(), [0, 0, 7]);
    }

    #[test]
    fn durations_survive_a_null_or_bool_field() {
        let body = r#"{"subsonic-response":{"status":"ok","album":{"id":"9","song":[
            {"id":"a","title":"Null","duration":null},
            {"id":"b","title":"Bool","duration":true},
            {"id":"c","title":"Neg","duration":-5},
            {"id":"d","title":"Text","duration":"1e400"}
        ]}}}"#;
        let response = parse_response(body).unwrap();
        let (_, songs) = parse_album(&response);
        assert_eq!(songs.len(), 4, "songs were dropped: {songs:?}");
        for song in &songs {
            assert!(song.duration.is_finite() && song.duration >= 0.0, "{song:?}");
        }
        assert_eq!(songs[3].duration, 0.0);
    }

    #[test]
    fn token_and_password_are_redacted_from_errors() {
        let server = Server { base: "https://music.example".into(), user: "u".into(), password: "hunter2".into() };
        let client = Client::new(&server, StreamFormat::Raw, 320).unwrap();
        let message = client.redact("failed for hunter2 and token".to_owned());
        assert!(!message.contains("hunter2"));
        let token = client.token.clone();
        assert!(!client.redact(format!("t={token}")).contains(&token));
    }

    /// Answers each accepted connection with `respond(request line)` and returns
    /// the request lines it saw once `count` requests were served.
    fn serve(
        listener: std::net::TcpListener,
        count: usize,
        respond: impl Fn(&str) -> String + Send + 'static,
    ) -> std::thread::JoinHandle<Vec<String>> {
        use std::io::{Read, Write};
        std::thread::spawn(move || {
            let mut seen = Vec::new();
            for _ in 0..count {
                let (mut stream, _) = listener.accept().unwrap();
                stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
                let mut request = Vec::new();
                let mut buf = [0u8; 4096];
                loop {
                    let n = stream.read(&mut buf).unwrap();
                    if n == 0 {
                        break;
                    }
                    request.extend_from_slice(&buf[..n]);
                    if request.windows(4).any(|w| w == b"\r\n\r\n") {
                        break;
                    }
                }
                let line = String::from_utf8_lossy(&request).lines().next().unwrap_or_default().to_owned();
                let reply = respond(&line);
                seen.push(line);
                let _ = stream.write_all(reply.as_bytes());
            }
            seen
        })
    }

    fn ok_json() -> String {
        let body = r#"{"subsonic-response":{"status":"ok","version":"1.16.1"}}"#;
        format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len())
    }

    #[test]
    fn a_redirect_to_another_origin_never_carries_the_token_along() {
        let other = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let other_addr = other.local_addr().unwrap();
        other.set_nonblocking(true).unwrap();
        let hit = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let watcher = {
            let hit = hit.clone();
            std::thread::spawn(move || {
                let deadline = std::time::Instant::now() + Duration::from_secs(3);
                while std::time::Instant::now() < deadline {
                    if let Ok((mut stream, _)) = other.accept() {
                        use std::io::{Read, Write};
                        hit.store(true, std::sync::atomic::Ordering::SeqCst);
                        let mut buf = [0u8; 4096];
                        let _ = stream.read(&mut buf);
                        let _ = stream.write_all(ok_json().as_bytes());
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
            })
        };
        let first = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}/", first.local_addr().unwrap());
        let server = serve(first, 1, move |line| {
            let query =
                line.split_once('?').map(|(_, rest)| rest.split(' ').next().unwrap_or_default()).unwrap_or_default();
            format!("HTTP/1.1 302 Found\r\nLocation: http://{other_addr}/rest/ping.view?{query}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
        });
        let client =
            Client::new(&Server { base, user: "u".into(), password: "hunter2".into() }, StreamFormat::Raw, 320)
                .unwrap();
        let result = client.ping();
        server.join().unwrap();
        watcher.join().unwrap();
        assert!(!hit.load(std::sync::atomic::Ordering::SeqCst), "the redirect was followed to another origin");
        let err = result.expect_err("a cross-origin redirect must fail the request");
        assert!(!err.contains("hunter2") && !err.contains("t="), "the error leaks credentials: {err}");
    }

    #[test]
    fn a_redirect_inside_the_same_origin_is_followed() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}/", listener.local_addr().unwrap());
        let server = serve(listener, 2, |line| {
            if line.contains("again=1") {
                ok_json()
            } else {
                "HTTP/1.1 301 Moved Permanently\r\nLocation: /rest/ping.view?again=1\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_owned()
            }
        });
        let client =
            Client::new(&Server { base, user: "u".into(), password: "p".into() }, StreamFormat::Raw, 320).unwrap();
        let result = client.ping();
        let seen = server.join().unwrap();
        assert!(result.is_ok(), "{result:?}");
        assert!(seen[1].contains("again=1"), "{seen:?}");
    }

    #[test]
    fn requests_carry_token_auth_and_stream_returns_bytes() {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}/", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let mut seen = Vec::new();
            for index in 0..2 {
                let (mut stream, _) = listener.accept().unwrap();
                stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
                let mut request = Vec::new();
                let mut buf = [0u8; 4096];
                loop {
                    let n = stream.read(&mut buf).unwrap();
                    assert!(n > 0);
                    request.extend_from_slice(&buf[..n]);
                    if request.windows(4).any(|w| w == b"\r\n\r\n") {
                        break;
                    }
                }
                let headers = String::from_utf8_lossy(&request).into_owned();
                let line = headers.lines().next().unwrap_or_default().to_owned();
                seen.push(line.clone());
                if index == 0 {
                    let body = r#"{"subsonic-response":{"status":"ok","version":"1.16.1"}}"#;
                    write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
                } else {
                    let body = b"data";
                    write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: audio/mpeg\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).unwrap();
                    stream.write_all(body).unwrap();
                }
            }
            seen
        });
        let server_conf = Server { base, user: "listener".into(), password: "hunter2".into() };
        let client = Client::new(&server_conf, StreamFormat::Raw, 320).unwrap();
        client.ping().unwrap();
        let song = Song { id: "42".into(), suffix: "mp3".into(), ..Song::default() };
        let mut stream = client.open_stream(&song).unwrap();
        let mut bytes = Vec::new();
        stream.reader.read_to_end(&mut bytes).unwrap();
        let seen = server.join().unwrap();
        assert_eq!(bytes, b"data");
        assert_eq!(stream.total, Some(4));
        assert_eq!(stream.suffix, "mp3");
        assert!(seen[0].starts_with("GET /rest/ping.view?"), "{}", seen[0]);
        assert!(seen[1].starts_with("GET /rest/stream.view?"), "{}", seen[1]);
        for line in &seen {
            assert!(line.contains("u=listener"));
            assert!(line.contains("t=") && line.contains("s="));
            assert!(!line.contains("hunter2"), "the password must never travel: {line}");
        }
        assert!(seen[1].contains("format=raw"));
    }
}
