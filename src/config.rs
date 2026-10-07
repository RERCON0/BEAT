use serde::{Deserialize, Serialize};
use std::path::PathBuf;

const MAX_CONFIG_BYTES: u64 = 1024 * 1024;

/// The config is written after every settings change, but a hand-edited or
/// damaged file must never come back as silent defaults: read problems keep
/// the old file and block saving.
fn read_state(path: &std::path::Path, cap: u64) -> Result<Option<String>, String> {
    use std::io::Read;
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(crate::i18n::trf!("не удалось открыть {path:?}: {err}", err = err, path = path)),
    };
    if file.metadata().map_err(|e| e.to_string())?.len() > cap {
        return Err(crate::i18n::trf!("файл {path:?} превышает лимит {} МБ", cap / 1024 / 1024, path = path));
    }
    let mut bytes = Vec::new();
    file.take(cap + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| crate::i18n::trf!("не удалось прочитать {path:?}: {e}", e = e, path = path))?;
    if bytes.len() as u64 > cap {
        return Err(crate::i18n::trf!("файл {path:?} превышает лимит {} МБ", cap / 1024 / 1024, path = path));
    }
    String::from_utf8(bytes).map(Some).map_err(|_| crate::i18n::trf!("файл {path:?} не в UTF-8", path = path))
}

pub(crate) fn atomic_write(path: &std::path::Path, bytes: &[u8]) -> Result<(), String> {
    use std::io::Write;
    let nonce =
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_err(|e| e.to_string())?.as_nanos();
    let temp = path.with_extension(format!("{}.{}.tmp", std::process::id(), nonce));
    let mut created = false;
    let result = (|| -> std::io::Result<()> {
        let mut file = std::fs::OpenOptions::new().write(true).create_new(true).open(&temp)?;
        created = true;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&temp, path)
    })();
    if created && result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result.map_err(|e| crate::i18n::trf!("не удалось записать {path:?}: {e}", e = e, path = path))
}

/// A corrupt file is set aside so the user can recover instead of having it
/// silently replaced by defaults on the next save.
fn backup_corrupt(path: &std::path::Path) -> Option<String> {
    let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    let backup = path.with_extension(format!("corrupt-{stamp}.bak"));
    if backup.exists() {
        return None;
    }
    std::fs::rename(path, &backup).ok()?;
    backup.file_name().map(|name| name.to_string_lossy().into_owned())
}

#[cfg(windows)]
fn protect_secret(secret: &str) -> Result<String, String> {
    if secret.is_empty() {
        return Ok(String::new());
    }
    let encrypted = dpapi(secret.as_bytes(), true)?;
    let mut out = String::from("dpapi:v1:");
    for byte in encrypted {
        out.push_str(&format!("{byte:02x}"));
    }
    Ok(out)
}

/// Server password on disk.
///
/// Windows has DPAPI, which is scoped to the logged-in user. There is no
/// equivalent built into this crate for other systems, and the previous
/// fallback wrote the Navidrome password to `config.json` in cleartext — so
/// instead of storing it, saving reports that it was left out. The user types
/// it again after a restart; every other setting still persists.
#[cfg(not(windows))]
fn protect_secret(_secret: &str) -> Result<String, String> {
    Err(crate::i18n::tr("на этой платформе пароль нельзя сохранить безопасно").into())
}

#[cfg(windows)]
fn unprotect_secret(stored: &str) -> Result<String, String> {
    let hex = stored.strip_prefix("dpapi:v1:").ok_or(crate::i18n::tr("неизвестный формат пароля"))?;
    if hex.len() % 2 != 0 || !hex.is_ascii() {
        return Err(crate::i18n::tr("повреждённый пароль").into());
    }
    let bytes = hex
        .as_bytes()
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| {
            let pair = std::str::from_utf8(pair).map_err(|_| crate::i18n::tr("повреждённый пароль").to_string())?;
            u8::from_str_radix(pair, 16).map_err(|_| crate::i18n::tr("повреждённый пароль").to_string())
        })
        .collect::<Result<Vec<_>, _>>()?;
    String::from_utf8(dpapi(&bytes, false)?).map_err(|_| crate::i18n::tr("повреждённый пароль").into())
}

#[cfg(not(windows))]
fn unprotect_secret(_stored: &str) -> Result<String, String> {
    Err(crate::i18n::tr("пароль сохранён для Windows и не может быть прочитан на этой системе").into())
}

#[cfg(windows)]
fn dpapi(input: &[u8], encrypt: bool) -> Result<Vec<u8>, String> {
    use std::ffi::c_void;
    #[repr(C)]
    struct DataBlob {
        size: u32,
        data: *mut u8,
    }
    #[link(name = "Crypt32")]
    unsafe extern "system" {
        fn CryptProtectData(
            input: *const DataBlob,
            description: *const u16,
            entropy: *const DataBlob,
            reserved: *mut c_void,
            prompt: *const c_void,
            flags: u32,
            output: *mut DataBlob,
        ) -> i32;
        fn CryptUnprotectData(
            input: *const DataBlob,
            description: *mut *mut u16,
            entropy: *const DataBlob,
            reserved: *mut c_void,
            prompt: *const c_void,
            flags: u32,
            output: *mut DataBlob,
        ) -> i32;
    }
    #[link(name = "Kernel32")]
    unsafe extern "system" {
        fn LocalFree(memory: *mut c_void) -> *mut c_void;
    }
    let size = u32::try_from(input.len()).map_err(|_| crate::i18n::tr("пароль слишком длинный").to_string())?;
    let source = DataBlob { size, data: input.as_ptr() as *mut u8 };
    let mut output = DataBlob { size: 0, data: std::ptr::null_mut() };
    // DPAPI is scoped to the current Windows user. UI prompts are disabled.
    let ok = unsafe {
        if encrypt {
            CryptProtectData(
                &source,
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null_mut(),
                std::ptr::null(),
                1,
                &mut output,
            )
        } else {
            CryptUnprotectData(
                &source,
                std::ptr::null_mut(),
                std::ptr::null(),
                std::ptr::null_mut(),
                std::ptr::null(),
                1,
                &mut output,
            )
        }
    };
    if ok == 0 {
        return Err(crate::i18n::trf!("защита пароля Windows: {}", std::io::Error::last_os_error()));
    }
    if output.data.is_null() && output.size != 0 {
        return Err(crate::i18n::tr("повреждённый ответ Windows DPAPI").into());
    }
    let bytes = if output.size == 0 {
        Vec::new()
    } else {
        unsafe { std::slice::from_raw_parts(output.data, output.size as usize).to_vec() }
    };
    if !output.data.is_null() {
        unsafe {
            LocalFree(output.data.cast());
        }
    }
    Ok(bytes)
}

/// What the server should send for playback/download. `raw` keeps the original
/// file (mp3/flac/ogg/...), `mp3` asks Navidrome to transcode on the fly.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "lowercase")]
pub enum StreamFormat {
    Raw,
    Mp3,
}

impl StreamFormat {
    pub fn label(self) -> &'static str {
        match self {
            Self::Raw => crate::i18n::tr("оригинал"),
            Self::Mp3 => crate::i18n::tr("mp3 (транскод)"),
        }
    }
}

fn default_format() -> StreamFormat {
    StreamFormat::Raw
}
fn default_bit_rate() -> u32 {
    320
}
fn default_volume() -> f32 {
    0.8
}
fn default_true() -> bool {
    true
}

#[derive(Serialize, Deserialize, Clone)]
pub struct Config {
    /// Base URL of the Navidrome server, for example `https://music.example.com`.
    #[serde(default)]
    pub server_url: String,
    #[serde(default)]
    pub user: String,
    /// Plain password in memory; DPAPI-protected on disk.
    #[serde(default)]
    pub password: String,
    /// Where cached audio lives; hand-dropped local files are picked up from
    /// here too. Empty = the system Music folder + `BEAT`.
    #[serde(default)]
    pub cache_dir: String,
    /// Additional read-only local music roots. Cache deletion never uses them.
    #[serde(default)]
    pub library_dirs: Vec<String>,
    #[serde(default)]
    pub language: crate::i18n::Language,
    #[serde(default = "default_format")]
    pub stream_format: StreamFormat,
    /// Cap for mp3 transcoding, kbps (0 = server default).
    #[serde(default = "default_bit_rate")]
    pub bit_rate: u32,
    /// Parallel downloads (1..=3).
    #[serde(default = "default_parallel")]
    pub parallel_downloads: usize,
    /// Detect and cache songs added after the first complete catalog scan.
    #[serde(default)]
    pub auto_cache_new: bool,
    #[serde(default = "default_true")]
    pub dark_mode: bool,
    #[serde(default = "default_volume")]
    pub volume: f32,
    /// Kept in memory so a failed DPAPI decrypt cannot erase a saved password.
    #[serde(skip)]
    pub unreadable_password: Option<String>,
    #[serde(skip)]
    pub warning: Option<String>,
    #[serde(skip)]
    pub save_blocked: bool,
}

fn default_parallel() -> usize {
    3
}

/// Keeps the first `max` characters; `String::truncate` takes a byte index
/// and panics inside a multi-byte character.
fn truncate_chars(text: &mut String, max: usize) {
    if let Some((end, _)) = text.char_indices().nth(max) {
        text.truncate(end);
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            server_url: String::new(),
            user: String::new(),
            password: String::new(),
            cache_dir: String::new(),
            library_dirs: Vec::new(),
            language: crate::i18n::Language::En,
            stream_format: StreamFormat::Raw,
            bit_rate: 320,
            parallel_downloads: 3,
            auto_cache_new: false,
            dark_mode: true,
            volume: 0.8,
            unreadable_password: None,
            warning: None,
            save_blocked: false,
        }
    }
}

impl Config {
    pub fn sanitize(&mut self) {
        self.parallel_downloads = self.parallel_downloads.clamp(1, 3);
        if !self.volume.is_finite() {
            self.volume = 0.8;
        }
        self.volume = self.volume.clamp(0.0, 1.0);
        self.bit_rate = self.bit_rate.min(320);
        truncate_chars(&mut self.server_url, 500);
        truncate_chars(&mut self.cache_dir, 500);
        self.library_dirs.truncate(16);
        for dir in &mut self.library_dirs {
            *dir = dir.trim().to_owned();
            truncate_chars(dir, 500);
        }
        self.library_dirs.retain(|dir| !dir.is_empty() && std::path::Path::new(dir).is_absolute());
        let mut seen = std::collections::HashSet::new();
        self.library_dirs.retain(|dir| seen.insert(crate::local::path_key(std::path::Path::new(dir))));
    }

    pub fn cache_root(&self) -> PathBuf {
        if self.cache_dir.trim().is_empty() {
            music_dir().map(|music| music.join("BEAT")).unwrap_or_else(|| config_path().with_file_name("cache"))
        } else {
            PathBuf::from(self.cache_dir.trim())
        }
    }

    pub fn local_roots(&self) -> Vec<PathBuf> {
        self.library_dirs.iter().map(PathBuf::from).collect()
    }

    pub fn load() -> Self {
        let path = config_path();
        let mut cfg = match read_state(&path, MAX_CONFIG_BYTES) {
            Ok(Some(raw)) => match serde_json::from_str(&raw) {
                Ok(cfg) => cfg,
                Err(_) => {
                    let backup = backup_corrupt(&path);
                    Config {
                        warning: Some(match &backup {
                            Some(name) => crate::i18n::trf!("config.json повреждён — загружены значения по умолчанию; старый файл сохранён как {name}", name = name),
                            None => crate::i18n::tr("config.json повреждён; резервная копия не создана, сохранение заблокировано").into(),
                        }),
                        save_blocked: backup.is_none(),
                        ..Config::default()
                    }
                }
            },
            Ok(None) => Config::default(),
            Err(err) => Config {
                warning: Some(crate::i18n::trf!("{err}; сохранение заблокировано", err = err)),
                save_blocked: true,
                ..Config::default()
            },
        };
        cfg.sanitize();
        #[cfg(windows)]
        let had_plaintext = !cfg.password.is_empty() && !cfg.password.starts_with("dpapi:v1:");
        if cfg.password.starts_with("dpapi:v1:") {
            match unprotect_secret(&cfg.password) {
                Ok(secret) => cfg.password = secret,
                Err(_) => {
                    cfg.unreadable_password = Some(cfg.password.clone());
                    cfg.password.clear();
                    cfg.warning = Some(
                        crate::i18n::tr("пароль сервера не удалось расшифровать; введите его заново в настройках")
                            .into(),
                    );
                }
            }
        }
        #[cfg(windows)]
        if had_plaintext {
            if let Err(err) = cfg.save() {
                cfg.warning = Some(crate::i18n::trf!("не удалось защитить сохранённый пароль: {err}", err = err));
            }
        }
        cfg
    }

    pub fn save(&mut self) -> Result<(), String> {
        self.save_to(&config_path())
    }

    fn save_to(&mut self, path: &std::path::Path) -> Result<(), String> {
        self.save_to_with(path, protect_secret)
    }

    fn save_to_with(
        &mut self,
        path: &std::path::Path,
        protect: fn(&str) -> Result<String, String>,
    ) -> Result<(), String> {
        if self.save_blocked {
            return Err(crate::i18n::tr(
                "config.json не был прочитан; сохранение заблокировано во избежание потери данных",
            )
            .into());
        }
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)
                .map_err(|e| crate::i18n::trf!("не удалось создать {dir:?}: {e}", dir = dir, e = e))?;
        }
        self.warning = None;
        let mut disk = self.clone();
        disk.password = if self.password.is_empty() {
            self.unreadable_password.clone().unwrap_or_default()
        } else {
            match protect(&self.password) {
                Ok(stored) => stored,
                Err(err) => {
                    self.warning = Some(crate::i18n::trf!("пароль не сохранён: {err}", err = err));
                    // On the supported platform a temporary DPAPI failure must
                    // not replace the previous settings with an empty password.
                    #[cfg(windows)]
                    return Err(crate::i18n::trf!(
                        "не удалось защитить пароль; настройки не изменены: {err}",
                        err = err
                    ));
                    #[cfg(not(windows))]
                    String::new()
                }
            }
        };
        let raw = serde_json::to_vec_pretty(&disk).map_err(|e| e.to_string())?;
        if raw.len() as u64 > MAX_CONFIG_BYTES {
            return Err(crate::i18n::tr("config.json слишком большой").into());
        }
        atomic_write(path, &raw)
    }

    /// The user edited (or cleared) the password field: a stored ciphertext
    /// that could not be decrypted must not come back on save.
    pub fn forget_unreadable_password(&mut self) {
        self.unreadable_password = None;
    }
}

pub fn config_path() -> PathBuf {
    let base = std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("beat").join("config.json")
}

/// The real "Музыка" shell folder (`SHGetKnownFolderPath`), which can be
/// relocated (for example into OneDrive); $HOME/Music is the fallback.
#[cfg(windows)]
pub fn music_dir() -> Option<PathBuf> {
    use std::ffi::c_void;
    #[repr(C)]
    struct Guid {
        data1: u32,
        data2: u16,
        data3: u16,
        data4: [u8; 8],
    }
    const FOLDERID_MUSIC: Guid = Guid {
        data1: 0x4BD8_D571,
        data2: 0x6D19,
        data3: 0x48D3,
        data4: [0xBE, 0x97, 0x42, 0x22, 0x20, 0x08, 0x0E, 0x43],
    };
    #[link(name = "Shell32")]
    unsafe extern "system" {
        fn SHGetKnownFolderPath(rfid: *const Guid, flags: u32, token: *mut c_void, path: *mut *mut u16) -> i32;
    }
    #[link(name = "Ole32")]
    unsafe extern "system" {
        fn CoTaskMemFree(memory: *mut c_void);
    }
    unsafe {
        let mut ptr: *mut u16 = std::ptr::null_mut();
        if SHGetKnownFolderPath(&FOLDERID_MUSIC, 0, std::ptr::null_mut(), &mut ptr) != 0 || ptr.is_null() {
            return None;
        }
        let mut len = 0usize;
        while *ptr.add(len) != 0 {
            len += 1;
        }
        let path = String::from_utf16_lossy(std::slice::from_raw_parts(ptr, len));
        CoTaskMemFree(ptr.cast());
        Some(PathBuf::from(path))
    }
}

#[cfg(not(windows))]
pub fn music_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join("Music"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn music_roots_are_bounded_absolute_and_deduplicated_and_language_persists() {
        let mut config = Config {
            library_dirs: vec!["relative".into(), " ".into()],
            language: crate::i18n::Language::Ru,
            ..Default::default()
        };
        let first = std::env::temp_dir().join("beat-library-test").to_string_lossy().into_owned();
        config.library_dirs.extend([first.clone(), first.clone()]);
        config.sanitize();
        assert_eq!(config.library_dirs, vec![first]);
        assert_eq!(config.local_roots().len(), 1);
        for i in 0..40 {
            config.library_dirs.push(std::env::temp_dir().join(format!("music-{i}")).to_string_lossy().into_owned());
        }
        config.sanitize();
        assert_eq!(config.local_roots().len(), 16);
        let back: Config = serde_json::from_str(&serde_json::to_string(&config).unwrap()).unwrap();
        assert_eq!(back.language, crate::i18n::Language::Ru);
        assert_eq!(back.library_dirs, config.library_dirs);
    }

    #[test]
    fn config_roundtrips_with_defaults_for_missing_fields() {
        let cfg = Config { server_url: "https://music.example".into(), user: "me".into(), ..Config::default() };
        let raw = serde_json::to_string(&cfg).unwrap();
        let back: Config = serde_json::from_str(&raw).unwrap();
        assert_eq!(back.server_url, "https://music.example");
        assert_eq!(back.user, "me");
        assert_eq!(back.stream_format, StreamFormat::Raw);
        assert_eq!(back.parallel_downloads, 3);
        let legacy: Config = serde_json::from_str("{}").unwrap();
        assert_eq!(legacy.bit_rate, 320);
        assert!(legacy.dark_mode);
        assert!(!legacy.auto_cache_new);
        let enabled: Config = serde_json::from_str(r#"{"auto_cache_new":true}"#).unwrap();
        assert!(enabled.auto_cache_new);
    }

    #[test]
    fn sanitize_clamps_volume_and_parallelism() {
        let mut cfg = Config { volume: 5.0, parallel_downloads: 99, bit_rate: 999, ..Config::default() };
        cfg.sanitize();
        assert_eq!(cfg.volume, 1.0);
        assert_eq!(cfg.parallel_downloads, 3);
        assert_eq!(cfg.bit_rate, 320);
        let mut cfg = Config { volume: f32::NAN, ..Config::default() };
        cfg.sanitize();
        assert_eq!(cfg.volume, 0.8);
    }

    #[test]
    fn sanitize_truncates_long_multibyte_values_without_panicking() {
        // Byte 500 falls inside a two-byte letter: a byte-index truncate panics,
        // and a config that panics on load keeps the app from ever starting.
        let mut cfg = Config {
            cache_dir: format!("a{}", "я".repeat(600)),
            server_url: format!("https://{}", "я".repeat(600)),
            ..Config::default()
        };
        cfg.sanitize();
        assert_eq!(cfg.cache_dir.chars().count(), 500);
        assert_eq!(cfg.server_url.chars().count(), 500);
        assert!(cfg.cache_dir.starts_with("aя"));
    }

    #[test]
    fn cache_root_defaults_under_the_music_folder() {
        let cfg = Config::default();
        let root = cfg.cache_root();
        assert!(root.is_absolute() || root.starts_with("."));
        assert_eq!(root.file_name().unwrap().to_string_lossy(), "BEAT");
        let cfg = Config { cache_dir: r"D:\Music\Beat".into(), ..Config::default() };
        assert_eq!(cfg.cache_root(), PathBuf::from(r"D:\Music\Beat"));
    }

    #[test]
    fn oversized_config_write_is_refused() {
        let path = std::env::temp_dir().join(format!(
            "beat-oversize-{}-{}.json",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        let mut cfg = Config { server_url: format!("https://{}", "x".repeat(2 * 1024 * 1024)), ..Config::default() };
        cfg.save_blocked = true;
        assert!(cfg.save_to(&path).is_err());
        cfg.save_blocked = false;
        assert!(cfg.save_to(&path).is_err());
        assert!(!path.exists());
        cfg.server_url = "https://music.example".into();
        cfg.save_to(&path).unwrap();
        assert!(path.exists());
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn the_password_never_lands_in_the_file_in_cleartext() {
        // Whatever the platform, config.json must never contain the Navidrome
        // password verbatim: DPAPI on Windows, not stored at all elsewhere.
        let path = std::env::temp_dir().join(format!(
            "beat-secret-{}-{}.json",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        let mut cfg = Config {
            server_url: "https://music.example".into(),
            user: "me".into(),
            password: "hunter2".into(),
            ..Config::default()
        };
        cfg.save_to(&path).unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(!raw.contains("hunter2"), "the password was stored unprotected: {raw}");
        // The rest of the settings survive.
        let back: Config = serde_json::from_str(&raw).unwrap();
        assert_eq!(back.server_url, "https://music.example");
        assert_eq!(back.user, "me");
        std::fs::remove_file(path).unwrap();
    }

    #[cfg(not(windows))]
    #[test]
    fn without_dpapi_the_password_is_dropped_with_a_warning() {
        let path = std::env::temp_dir().join(format!(
            "beat-nosecret-{}-{}.json",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        let mut cfg = Config { password: "hunter2".into(), volume: 0.5, ..Config::default() };
        cfg.save_to(&path).unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(!raw.contains("hunter2"), "{raw}");
        assert!(cfg.warning.as_deref().is_some_and(|w| w.contains("пароль не сохранён")), "{:?}", cfg.warning);
        let back: Config = serde_json::from_str(&raw).unwrap();
        assert!(back.password.is_empty());
        assert_eq!(back.volume, 0.5);
        std::fs::remove_file(path).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn protection_failure_keeps_the_previous_settings() {
        let path = std::env::temp_dir().join(format!("beat-protect-failure-{}.json", std::process::id()));
        let original = b"previous encrypted settings";
        std::fs::write(&path, original).unwrap();
        let mut cfg = Config { password: "new secret".into(), ..Config::default() };
        let result = cfg.save_to_with(&path, |_| Err("DPAPI unavailable".into()));
        assert!(result.is_err());
        assert_eq!(std::fs::read(&path).unwrap(), original);
        assert!(cfg.warning.is_some());
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn atomic_write_replaces_existing_file() {
        let path = std::env::temp_dir().join(format!("beat-atomic-{}.json", std::process::id()));
        atomic_write(&path, b"one").unwrap();
        atomic_write(&path, b"two").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"two");
        std::fs::remove_file(path).unwrap();
    }
}
