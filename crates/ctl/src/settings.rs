//! The settings editor: the handful of `telemouse.toml` values a person has
//! to get right (mouse CPI, their games, where recordings go, the two
//! hotkeys, the OBS overlay's look), read for the page and written back
//! without disturbing the rest of the file.
//!
//! The file stays the user's: a save changes values in place with
//! `toml_edit`, so comments, ordering and every key this form does not know
//! survive. What is about to be written is parsed back into
//! [`AppConfig`] and validated first — the same check every binary runs at
//! start — and nothing is written when it fails. The write is a temp file in
//! the same directory plus a rename, and it is refused when the file on disk
//! is not the one the page was looking at (the token is a hash of its bytes).
//!
//! Deliberately not editable from the page: bind addresses, `bin_dir`,
//! `log_dir`, Kafka. Those decide who can reach this machine and what the
//! panel launches; they stay a hand edit.
//!
//! `[games]` keys are whatever the user's game is called. There is no list
//! of executable names here or anywhere else, only a shape check.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use telemouse_core::config::{
    AppConfig, ConfigError, OBS_HUD_ITEMS, OBS_HUD_POSITIONS, OBS_LAYOUTS,
};
use telemouse_core::session::GameSens;
use toml_edit::{Array, DocumentMut, InlineTable, Item, Table, Value};

/// The sample config shipped with every release: `telemouse.example.toml`
/// at the workspace root (loopback everywhere, Kafka off). Written as
/// `telemouse.toml` on first start when none exists, and by the first save
/// from the page when there still is none. The path reaches outside this
/// crate, so the crate builds from the workspace only.
pub const SAMPLE_CONFIG: &str = include_str!("../../../telemouse.example.toml");

/// Longest `[games]` key accepted from the page. Windows file names stop at
/// 255; nothing that is a game is near that.
pub const MAX_GAME_KEY_CHARS: usize = 64;
/// Longest `recording.dir` accepted from the page (`MAX_PATH`).
pub const MAX_DIR_CHARS: usize = 260;
/// `yaw_coeff` for a new game when the page sends none: what
/// [`GameSens`] itself defaults to.
pub const DEFAULT_COEFF: f64 = 0.022;

/// What [`seed_config`] did.
pub enum Seed {
    /// A file was already there (or something else is; `load` will say).
    Existing,
    /// The sample was written.
    Written,
    /// The sample could not be written (a directory in the way, a read-only
    /// location, ...); `load` decides what that means.
    Failed(std::io::Error),
}

/// Write the shipped sample to `path` unless something is already there.
/// `create_new` means an existing file is never touched, even one that
/// appears between a check and the write; a half-written file is removed
/// rather than left to fail parsing on the next start.
pub fn seed_config(path: &Path) -> Seed {
    use std::io::Write;
    match std::fs::File::options()
        .write(true)
        .create_new(true)
        .open(path)
    {
        Ok(mut f) => match f
            .write_all(SAMPLE_CONFIG.as_bytes())
            .and_then(|()| f.flush())
        {
            Ok(()) => Seed::Written,
            Err(e) => {
                drop(f);
                let _ = std::fs::remove_file(path);
                Seed::Failed(e)
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Seed::Existing,
        Err(e) => Seed::Failed(e),
    }
}

/// The editable part of the config, as the page shows it. Paths are as the
/// file spells them (`recordings`, not the absolute path it resolves to).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Settings {
    pub mouse_cpi: f64,
    pub marker_hotkey: String,
    pub recording: RecordingSettings,
    pub ctl: CtlSettings,
    pub games: BTreeMap<String, GameSens>,
    pub obs: ObsSettings,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RecordingSettings {
    pub enabled: bool,
    pub dir: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CtlSettings {
    pub hotkey: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ObsSettings {
    pub layout: String,
    pub background: String,
    pub hud: Vec<String>,
    pub hud_position: String,
    pub scale: f64,
}

impl From<&AppConfig> for Settings {
    fn from(c: &AppConfig) -> Self {
        Self {
            mouse_cpi: c.mouse_cpi,
            marker_hotkey: c.marker_hotkey.clone(),
            recording: RecordingSettings {
                enabled: c.recording.enabled,
                dir: c.recording.dir.display().to_string(),
            },
            ctl: CtlSettings {
                hotkey: c.ctl.hotkey.clone(),
            },
            games: c.games.clone(),
            obs: ObsSettings {
                layout: c.viz.obs.layout.clone(),
                background: c.viz.obs.background.clone(),
                hud: c.viz.obs.hud.clone(),
                hud_position: c.viz.obs.hud_position.clone(),
                scale: c.viz.obs.scale,
            },
        }
    }
}

/// The vocabularies the form offers, from the same lists validation uses.
#[derive(Debug, Clone, Serialize)]
pub struct Choices {
    pub obs_layouts: &'static [&'static str],
    pub obs_hud_items: &'static [&'static str],
    pub obs_hud_positions: &'static [&'static str],
}

pub const CHOICES: Choices = Choices {
    obs_layouts: OBS_LAYOUTS,
    obs_hud_items: OBS_HUD_ITEMS,
    obs_hud_positions: OBS_HUD_POSITIONS,
};

/// What the page may not change, for the one line that says so.
pub const NOT_EDITABLE: &[&str] = &[
    "udp.addr",
    "viz.http_addr",
    "ctl.http_addr",
    "ctl.bin_dir",
    "ctl.log_dir",
    "kafka",
    "batch",
];

/// A change request. Every field is optional; a field that is absent is
/// left as the file has it. Anything outside this shape is refused by serde
/// (`deny_unknown_fields`), which is the allow-list.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Patch {
    #[serde(default)]
    pub mouse_cpi: Option<f64>,
    #[serde(default)]
    pub marker_hotkey: Option<String>,
    #[serde(default)]
    pub recording: Option<RecordingPatch>,
    #[serde(default)]
    pub ctl: Option<CtlPatch>,
    /// Exe name → values to add or update, or `null` to remove the game.
    #[serde(default)]
    pub games: Option<BTreeMap<String, Option<GamePatch>>>,
    #[serde(default)]
    pub obs: Option<ObsPatch>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecordingPatch {
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub dir: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CtlPatch {
    #[serde(default)]
    pub hotkey: Option<String>,
}

/// A new game needs `sens`; `yaw_coeff` defaults to [`DEFAULT_COEFF`] and
/// `pitch_coeff` to whatever the yaw ends up as.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GamePatch {
    #[serde(default)]
    pub sens: Option<f64>,
    #[serde(default)]
    pub yaw_coeff: Option<f64>,
    #[serde(default)]
    pub pitch_coeff: Option<f64>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObsPatch {
    #[serde(default)]
    pub layout: Option<String>,
    #[serde(default)]
    pub background: Option<String>,
    #[serde(default)]
    pub hud: Option<Vec<String>>,
    #[serde(default)]
    pub hud_position: Option<String>,
    #[serde(default)]
    pub scale: Option<f64>,
}

/// Why a patch was not applied. `field` is the dotted config name the page
/// puts the message next to (`mouse_cpi`, `games`, `viz.obs.scale`, ...);
/// empty when the problem is the file as a whole.
#[derive(Debug, Clone, PartialEq)]
pub struct Invalid {
    pub field: String,
    pub reason: String,
}

impl Invalid {
    fn new(field: impl Into<String>, reason: impl Into<String>) -> Self {
        Self {
            field: field.into(),
            reason: reason.into(),
        }
    }
}

impl std::fmt::Display for Invalid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.field.is_empty() {
            write!(f, "{}", self.reason)
        } else {
            write!(f, "{}: {}", self.field, self.reason)
        }
    }
}

/// The `[games]` key for what a person typed: trimmed, lowercased (process
/// names are matched in lower case), `.exe` added when it was left off. A
/// shape check only — no path, no characters a file name cannot have, not
/// absurdly long. What the name *is* is nobody's business but the user's.
pub fn game_key(input: &str) -> Result<String, String> {
    let mut key = input.trim().to_lowercase();
    if key.is_empty() {
        return Err("type the game's exe name, e.g. mygame.exe".into());
    }
    if key
        .chars()
        .any(|c| c.is_control() || r#"/\:<>"|?*"#.contains(c))
    {
        return Err(format!(
            "{input:?} is not a file name: use the bare exe name, without a folder"
        ));
    }
    if !key.ends_with(".exe") {
        key.push_str(".exe");
    }
    if key == ".exe" {
        return Err(format!("{input:?} is not an exe name"));
    }
    if key.chars().count() > MAX_GAME_KEY_CHARS {
        return Err(format!(
            "the exe name is longer than {MAX_GAME_KEY_CHARS} characters"
        ));
    }
    Ok(key)
}

fn finite(field: &str, v: f64) -> Result<f64, Invalid> {
    if v.is_finite() {
        Ok(v)
    } else {
        Err(Invalid::new(field, "must be a number"))
    }
}

fn one_line(field: &str, s: &str, max: usize) -> Result<(), Invalid> {
    if s.chars().any(char::is_control) {
        return Err(Invalid::new(field, "must be one line of plain text"));
    }
    if s.chars().count() > max {
        return Err(Invalid::new(
            field,
            format!("is longer than {max} characters"),
        ));
    }
    Ok(())
}

/// Replace `key`'s value, keeping whatever surrounded the old one (the
/// trailing `# comment` lives in the value's decor). A value that already
/// says the same thing is left exactly as the user wrote it.
fn set(table: &mut dyn toml_edit::TableLike, key: &str, new: Value) {
    // Written through the existing item, not `insert`: the comment lines
    // above a key belong to the *key*, and an insert replaces the key too.
    if let Some(item) = table.get_mut(key)
        && let Some(old) = item.as_value()
    {
        if same(old, &new) {
            return;
        }
        let mut new = new;
        *new.decor_mut() = old.decor().clone();
        *item = Item::Value(new);
        return;
    }
    table.insert(key, Item::Value(new));
}

/// Do two values mean the same to the config? `800` and `800.0` do.
fn same(a: &Value, b: &Value) -> bool {
    let num = |v: &Value| v.as_float().or_else(|| v.as_integer().map(|i| i as f64));
    match (a, b) {
        (Value::String(x), Value::String(y)) => x.value() == y.value(),
        (Value::Boolean(x), Value::Boolean(y)) => x.value() == y.value(),
        (Value::Array(x), Value::Array(y)) => {
            x.len() == y.len() && x.iter().zip(y.iter()).all(|(p, q)| same(p, q))
        }
        _ => match (num(a), num(b)) {
            (Some(x), Some(y)) => x == y,
            _ => false,
        },
    }
}

/// The table at `path`, created (as `[a.b]`, parents implicit) when the
/// file does not have it. An `Err` means something that is not a table is
/// in the way, which the config parser would refuse as well.
fn section<'a>(
    doc: &'a mut DocumentMut,
    path: &[&str],
) -> Result<&'a mut dyn toml_edit::TableLike, Invalid> {
    let mut item: &mut Item = doc.as_item_mut();
    for (depth, key) in path.iter().enumerate() {
        let in_the_way = || Invalid::new(path.join("."), "is not a table in telemouse.toml");
        let table = item.as_table_like_mut().ok_or_else(in_the_way)?;
        if !table.contains_key(key) {
            let mut t = Table::new();
            t.set_implicit(depth + 1 < path.len());
            table.insert(key, Item::Table(t));
        }
        item = table.get_mut(key).ok_or_else(in_the_way)?;
    }
    item.as_table_like_mut()
        .ok_or_else(|| Invalid::new(path.join("."), "is not a table in telemouse.toml"))
}

fn apply_games(
    doc: &mut DocumentMut,
    games: &BTreeMap<String, Option<GamePatch>>,
) -> Result<(), Invalid> {
    if games.is_empty() {
        return Ok(());
    }
    // `[games]` itself never needs a header of its own: its children are
    // `[games."x.exe"]` tables. Only made when the file has none.
    if !doc.as_table().contains_key("games") {
        let mut t = Table::new();
        t.set_implicit(true);
        doc.as_table_mut().insert("games", Item::Table(t));
    }
    let inline = !doc.as_table().get("games").is_some_and(Item::is_table);
    let table = section(doc, &["games"])?;
    for (name, change) in games {
        let Some(change) = change else {
            // Remove: by the name as sent (so an entry a hand edit spelled
            // oddly can still be deleted), then by its normal form.
            if table.remove(name).is_none()
                && let Ok(key) = game_key(name)
            {
                table.remove(&key);
            }
            continue;
        };
        let key = game_key(name).map_err(|reason| Invalid::new("games", reason))?;
        let field = |what: &str| format!("games.{key}.{what}");
        let existing = table.get(&key).is_some_and(Item::is_table_like);
        if !existing {
            let sens = change
                .sens
                .ok_or_else(|| Invalid::new(field("sens"), "a new game needs its sensitivity"))?;
            let sens = finite(&field("sens"), sens)?;
            let yaw = finite(
                &field("yaw_coeff"),
                change.yaw_coeff.unwrap_or(DEFAULT_COEFF),
            )?;
            let pitch = finite(&field("pitch_coeff"), change.pitch_coeff.unwrap_or(yaw))?;
            let item = if inline {
                let mut t = InlineTable::new();
                t.insert("sens", sens.into());
                t.insert("yaw_coeff", yaw.into());
                t.insert("pitch_coeff", pitch.into());
                Item::Value(Value::InlineTable(t))
            } else {
                let mut t = Table::new();
                t.insert("sens", toml_edit::value(sens));
                t.insert("yaw_coeff", toml_edit::value(yaw));
                t.insert("pitch_coeff", toml_edit::value(pitch));
                Item::Table(t)
            };
            table.insert(&key, item);
            continue;
        }
        let Some(entry) = table.get_mut(&key).and_then(Item::as_table_like_mut) else {
            continue;
        };
        for (what, v) in [
            ("sens", change.sens),
            ("yaw_coeff", change.yaw_coeff),
            ("pitch_coeff", change.pitch_coeff),
        ] {
            if let Some(v) = v {
                set(entry, what, finite(&field(what), v)?.into());
            }
        }
    }
    Ok(())
}

/// Apply `patch` to the text of a `telemouse.toml` and return the new text.
/// Pure: nothing is read or written, and the result is *not* yet validated
/// as a config — [`checked`] does that.
pub fn apply_patch(text: &str, patch: &Patch) -> Result<String, Invalid> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut doc: DocumentMut = text.parse().map_err(|e: toml_edit::TomlError| {
        Invalid::new(
            "",
            format!("telemouse.toml cannot be parsed, so it cannot be edited here: {e}"),
        )
    })?;

    if let Some(v) = patch.mouse_cpi {
        let v = finite("mouse_cpi", v)?;
        set(doc.as_table_mut(), "mouse_cpi", v.into());
    }
    if let Some(v) = &patch.marker_hotkey {
        one_line("marker_hotkey", v, 64)?;
        set(doc.as_table_mut(), "marker_hotkey", v.trim().into());
    }
    if let Some(r) = &patch.recording {
        if let Some(dir) = &r.dir {
            let dir = dir.trim();
            if dir.is_empty() {
                return Err(Invalid::new("recording.dir", "cannot be empty"));
            }
            one_line("recording.dir", dir, MAX_DIR_CHARS)?;
        }
        if r.enabled.is_some() || r.dir.is_some() {
            let t = section(&mut doc, &["recording"])?;
            if let Some(v) = r.enabled {
                set(t, "enabled", v.into());
            }
            if let Some(dir) = &r.dir {
                set(t, "dir", dir.trim().into());
            }
        }
    }
    if let Some(v) = patch.ctl.as_ref().and_then(|c| c.hotkey.as_ref()) {
        one_line("ctl.hotkey", v, 64)?;
        set(section(&mut doc, &["ctl"])?, "hotkey", v.trim().into());
    }
    if let Some(games) = &patch.games {
        apply_games(&mut doc, games)?;
    }
    if let Some(o) = &patch.obs
        && *o != ObsPatch::default()
    {
        if let Some(v) = o.scale {
            finite("viz.obs.scale", v)?;
        }
        let t = section(&mut doc, &["viz", "obs"])?;
        if let Some(v) = &o.layout {
            set(t, "layout", v.as_str().into());
        }
        if let Some(v) = &o.background {
            set(t, "background", v.trim().into());
        }
        if let Some(v) = &o.hud {
            let mut a = Array::new();
            for item in v {
                a.push(item.as_str());
            }
            set(t, "hud", Value::Array(a));
        }
        if let Some(v) = &o.hud_position {
            set(t, "hud_position", v.as_str().into());
        }
        if let Some(v) = o.scale {
            set(t, "scale", v.into());
        }
    }
    Ok(doc.to_string())
}

/// A config error as one plain sentence and the field it is about, without
/// the file path the core error leads with (the page knows which file).
pub fn plain(e: &ConfigError) -> Invalid {
    match e {
        ConfigError::Invalid { field, reason, .. } => Invalid::new(*field, reason.clone()),
        ConfigError::Parse { source, .. } => Invalid::new("", source.message().to_string()),
        other => Invalid::new("", other.to_string()),
    }
}

/// Parse and validate text exactly as the binaries will when they read it.
pub fn checked(text: &str, path: &Path) -> Result<AppConfig, Invalid> {
    AppConfig::from_toml(text, path).map_err(|e| plain(&e))
}

/// A short, stable fingerprint of the file's bytes: what `GET /api/config`
/// hands out and `POST /api/config` must hand back. FNV-1a plus the length;
/// this guards against an edit made elsewhere, not against an adversary.
pub fn token(bytes: Option<&[u8]>) -> String {
    let Some(bytes) = bytes else {
        return "none".into();
    };
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{h:016x}-{}", bytes.len())
}

/// The file's bytes, or `None` when there is no file.
fn read(path: &Path) -> std::io::Result<Option<Vec<u8>>> {
    match std::fs::read(path) {
        Ok(b) => Ok(Some(b)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

fn utf8(bytes: Vec<u8>) -> Result<String, String> {
    String::from_utf8(bytes).map_err(|_| {
        "telemouse.toml is not UTF-8 text; save it as UTF-8 in your editor".to_string()
    })
}

/// Write `text` next to `path` and rename it over it, so a crash or a full
/// disk leaves the old file, never half of the new one.
pub fn write_atomic(path: &Path, text: &str) -> std::io::Result<()> {
    use std::io::Write;
    let mut tmp: PathBuf = path.to_path_buf();
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "telemouse.toml".into());
    tmp.set_file_name(format!(".{name}.{}.tmp", std::process::id()));
    let written = (|| {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(text.as_bytes())?;
        f.sync_all()
    })()
    .and_then(|()| std::fs::rename(&tmp, path));
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    written
}

/// What `GET /api/config` needs from the disk.
#[derive(Debug, Clone)]
pub struct View {
    pub exists: bool,
    pub token: String,
    /// `None` when the file is there but cannot be used; `error` says why.
    pub settings: Option<Settings>,
    pub error: Option<String>,
}

/// Read the file for the page. A missing file shows the defaults (a save
/// will create it from the shipped sample).
pub fn view(path: &Path) -> View {
    let bytes = match read(path) {
        Ok(b) => b,
        Err(e) => {
            return View {
                exists: true,
                token: token(None),
                settings: None,
                error: Some(format!("telemouse.toml cannot be read: {e}")),
            };
        }
    };
    let token = token(bytes.as_deref());
    let Some(bytes) = bytes else {
        // No file: what a save would start from.
        let settings = checked(SAMPLE_CONFIG, path)
            .ok()
            .map(|c| Settings::from(&c));
        return View {
            exists: false,
            token,
            settings,
            error: None,
        };
    };
    let parsed = utf8(bytes).and_then(|t| checked(&t, path).map_err(|e| e.to_string()));
    match parsed {
        Ok(c) => View {
            exists: true,
            token,
            settings: Some(Settings::from(&c)),
            error: None,
        },
        Err(e) => View {
            exists: true,
            token,
            settings: None,
            error: Some(e),
        },
    }
}

/// A save that went through.
#[derive(Debug, Clone)]
pub struct Saved {
    pub token: String,
    /// The config before, when the old file was usable.
    pub old: Option<AppConfig>,
    pub new: AppConfig,
    /// The file did not exist and was created from the shipped sample.
    pub created: bool,
}

#[derive(Debug)]
pub enum SaveError {
    /// The file changed since the page read it; carries the current token.
    Stale(String),
    Invalid(Invalid),
    Io(String),
}

/// Apply `patch` to the file at `path`: token check, patch, validate, atomic
/// write. Nothing is written unless every step before the write passed.
pub fn save(path: &Path, expected_token: &str, patch: &Patch) -> Result<Saved, SaveError> {
    let io = |e: std::io::Error| SaveError::Io(format!("{}: {e}", path.display()));
    let bytes = read(path).map_err(io)?;
    let current = token(bytes.as_deref());
    if current != expected_token {
        return Err(SaveError::Stale(current));
    }
    let created = bytes.is_none();
    let text = match bytes {
        Some(b) => utf8(b).map_err(|e| SaveError::Invalid(Invalid::new("", e)))?,
        None => SAMPLE_CONFIG.to_string(),
    };
    let old = checked(&text, path).ok();
    let next = apply_patch(&text, patch).map_err(SaveError::Invalid)?;
    let new = checked(&next, path).map_err(SaveError::Invalid)?;
    if created {
        // Same rule as the first start: never write over something that
        // appeared in the meantime.
        match seed_config(path) {
            Seed::Written => {}
            Seed::Existing => {
                return Err(SaveError::Stale(token(read(path).map_err(io)?.as_deref())));
            }
            Seed::Failed(e) => return Err(io(e)),
        }
    }
    write_atomic(path, &next).map_err(io)?;
    Ok(Saved {
        token: token(Some(next.as_bytes())),
        old,
        new,
        created,
    })
}

/// Which editable groups differ between two configs, by the name the page
/// and the log use.
pub fn changed(old: &AppConfig, new: &AppConfig) -> Vec<&'static str> {
    let mut out = Vec::new();
    if old.mouse_cpi != new.mouse_cpi {
        out.push("mouse_cpi");
    }
    if old.marker_hotkey != new.marker_hotkey {
        out.push("marker_hotkey");
    }
    if old.recording != new.recording {
        out.push("recording");
    }
    if old.ctl.hotkey != new.ctl.hotkey {
        out.push("ctl.hotkey");
    }
    if old.games != new.games {
        out.push("games");
    }
    if old.viz.obs != new.viz.obs {
        out.push("viz.obs");
    }
    out
}

/// Who will not see a change until later. `restart_required` is about the
/// panel itself (the tray registered its hotkey at start); `next_start`
/// names running components that read the file when they were launched.
/// Whether recordings are saved by default and where they go is taken up by
/// the panel at once and needs neither.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Effects {
    pub restart_required: Vec<&'static str>,
    pub next_start: Vec<&'static str>,
}

pub fn effects(changed: &[&str], capture_running: bool, viz_running: bool) -> Effects {
    let has = |k: &str| changed.contains(&k);
    let mut e = Effects::default();
    if has("ctl.hotkey") {
        e.restart_required.push("ctl hotkey");
    }
    if capture_running
        && (has("mouse_cpi") || has("marker_hotkey") || has("games") || has("recording"))
    {
        e.next_start.push("capture");
    }
    if viz_running && (has("viz.obs") || has("recording")) {
        e.next_start.push("viz");
    }
    e
}

#[cfg(test)]
mod tests {
    use super::*;

    fn patch(v: serde_json::Value) -> Patch {
        serde_json::from_value(v).expect("a valid patch")
    }

    const FILE: &str = r#"# my settings
# second line

# the mouse
mouse_cpi = 1600.0   # G Pro
marker_hotkey = "f9"
future_top_level_key_the_form_does_not_know = 1

[recording]
enabled = true
dir = "recordings"  # beside this file

[ctl]
# the chord
hotkey = "ctrl+alt+r"

[viz.obs]
layout = "split"            # split | stack | desk | aim
scale = 1.0

# my main game
[games."one.exe"]
sens = 1.0
yaw_coeff = 0.022           # source-like
pitch_coeff = 0.022
"#;

    #[test]
    fn an_empty_patch_changes_nothing() {
        assert_eq!(apply_patch(FILE, &Patch::default()).unwrap(), FILE);
        // Saying what the file already says is not a change either, even
        // where the user wrote an integer.
        let file = FILE.replace("1600.0 ", "1600 ");
        let out = apply_patch(&file, &patch(serde_json::json!({ "mouse_cpi": 1600.0 }))).unwrap();
        assert_eq!(out, file);
    }

    #[test]
    fn values_change_and_every_comment_stays() {
        let out = apply_patch(
            FILE,
            &patch(serde_json::json!({
                "mouse_cpi": 800,
                "marker_hotkey": "f10",
                "recording": { "enabled": false, "dir": "D:/tm" },
                "ctl": { "hotkey": "" },
                "obs": { "layout": "aim", "scale": 1.5, "hud": ["speed", "game"] },
            })),
        )
        .unwrap();
        for kept in [
            "# my settings\n# second line\n",
            "# the mouse\nmouse_cpi = 800.0   # G Pro\n",
            "future_top_level_key_the_form_does_not_know = 1",
            "dir = \"D:/tm\"  # beside this file",
            "# the chord\nhotkey = \"\"",
            "layout = \"aim\"            # split | stack | desk | aim",
            "scale = 1.5",
            "hud = [\"speed\", \"game\"]",
            "# my main game\n[games.\"one.exe\"]",
            "yaw_coeff = 0.022           # source-like",
        ] {
            assert!(out.contains(kept), "lost {kept:?} in:\n{out}");
        }
        // The unknown key makes this text invalid as a config, which is the
        // parser's call, not the patcher's.
        let clean = out.replace("future_top_level_key_the_form_does_not_know = 1\n", "");
        let c = checked(&clean, Path::new("telemouse.toml")).unwrap();
        assert_eq!(c.mouse_cpi, 800.0);
        assert_eq!(c.marker_hotkey, "f10");
        assert!(!c.recording.enabled);
        assert_eq!(c.recording.dir, PathBuf::from("D:/tm"));
        assert_eq!(c.ctl.hotkey, "");
        assert_eq!(c.viz.obs.layout, "aim");
        assert_eq!(c.viz.obs.hud, vec!["speed", "game"]);
    }

    #[test]
    fn games_are_added_updated_and_removed() {
        let out = apply_patch(
            FILE,
            &patch(serde_json::json!({ "games": {
                "  Two Words.EXE ": { "sens": 2.5, "yaw_coeff": 0.0066 },
                "one.exe": { "sens": 1.25 },
            }})),
        )
        .unwrap();
        assert!(out.contains("[games.\"two words.exe\"]"), "{out}");
        assert!(out.contains("sens = 1.25"), "{out}");
        assert!(out.contains("yaw_coeff = 0.022           # source-like"));
        let text = out.replace("future_top_level_key_the_form_does_not_know = 1\n", "");
        let c = checked(&text, Path::new("t.toml")).unwrap();
        assert_eq!(c.games.len(), 2);
        let two = c.games["two words.exe"];
        assert_eq!(
            (two.sens, two.yaw_coeff, two.pitch_coeff),
            (2.5, 0.0066, 0.0066)
        );
        assert_eq!(c.games["one.exe"].sens, 1.25);

        let out = apply_patch(
            &out,
            &patch(serde_json::json!({ "games": { "one.exe": null } })),
        )
        .unwrap();
        assert!(!out.contains("one.exe"), "{out}");
        assert!(out.contains("two words.exe"));
        // Removing what is not there is not an error.
        apply_patch(
            &out,
            &patch(serde_json::json!({ "games": { "gone.exe": null } })),
        )
        .unwrap();
    }

    #[test]
    fn a_file_without_the_sections_gets_them() {
        let out = apply_patch(
            "mouse_cpi = 400.0\n",
            &patch(serde_json::json!({
                "marker_hotkey": "f8",
                "recording": { "enabled": false },
                "ctl": { "hotkey": "ctrl+alt+n" },
                "games": { "g": { "sens": 3 } },
                "obs": { "scale": 2 },
            })),
        )
        .unwrap();
        let c = checked(&out, Path::new("t.toml")).unwrap_or_else(|e| panic!("{e}\n{out}"));
        assert_eq!(c.mouse_cpi, 400.0);
        assert_eq!(c.marker_hotkey, "f8");
        assert!(!c.recording.enabled);
        assert_eq!(c.ctl.hotkey, "ctrl+alt+n");
        assert_eq!(c.games["g.exe"].yaw_coeff, DEFAULT_COEFF);
        assert_eq!(c.viz.obs.scale, 2.0);
        assert!(
            !out.contains("[games]\n"),
            "no empty [games] header:\n{out}"
        );
        assert!(!out.contains("[viz]\n"), "no empty [viz] header:\n{out}");
    }

    #[test]
    fn inline_games_tables_are_edited_in_kind() {
        let out = apply_patch(
            "games = { \"a.exe\" = { sens = 1.0 } }\n",
            &patch(
                serde_json::json!({ "games": { "b.exe": { "sens": 2 }, "a.exe": { "sens": 4 } } }),
            ),
        )
        .unwrap();
        let c = checked(&out, Path::new("t.toml")).unwrap_or_else(|e| panic!("{e}\n{out}"));
        assert_eq!(c.games["a.exe"].sens, 4.0);
        assert_eq!(c.games["b.exe"].sens, 2.0);
    }

    #[test]
    fn the_patch_shape_is_the_allow_list() {
        for refused in [
            serde_json::json!({ "ctl": { "http_addr": "0.0.0.0:7880" } }),
            serde_json::json!({ "ctl": { "bin_dir": "C:/evil" } }),
            serde_json::json!({ "kafka": { "enabled": true } }),
            serde_json::json!({ "udp": { "addr": "10.0.0.1:1" } }),
            serde_json::json!({ "viz": { "http_addr": "0.0.0.0:1" } }),
            serde_json::json!({ "obs": { "buffer_ms": 10 } }),
            serde_json::json!({ "games": { "a.exe": { "sens": 1, "extra": 1 } } }),
        ] {
            assert!(
                serde_json::from_value::<Patch>(refused.clone()).is_err(),
                "{refused} must not be a patch"
            );
        }
    }

    #[test]
    fn game_names_are_shaped_not_listed() {
        assert_eq!(game_key("  MyGame.EXE ").unwrap(), "mygame.exe");
        assert_eq!(game_key("my game").unwrap(), "my game.exe");
        for bad in [
            "",
            "   ",
            "C:\\Games\\x.exe",
            "../x.exe",
            "a/b.exe",
            "x\n.exe",
            "x\u{0}.exe",
            "what?.exe",
            "\"quoted\".exe",
            ".exe",
        ] {
            assert!(game_key(bad).is_err(), "{bad:?} must be refused");
        }
        assert!(game_key(&"x".repeat(MAX_GAME_KEY_CHARS)).is_err());
        assert!(game_key(&"x".repeat(MAX_GAME_KEY_CHARS - 4)).is_ok());
    }

    #[test]
    fn bad_input_names_its_field() {
        let e = apply_patch(
            FILE,
            &patch(serde_json::json!({ "games": { "a/b": { "sens": 1 } } })),
        )
        .unwrap_err();
        assert_eq!(e.field, "games");
        let e = apply_patch(
            FILE,
            &patch(serde_json::json!({ "games": { "new.exe": { "yaw_coeff": 1 } } })),
        )
        .unwrap_err();
        assert_eq!(e.field, "games.new.exe.sens");
        let e = apply_patch(
            FILE,
            &patch(serde_json::json!({ "recording": { "dir": "  " } })),
        )
        .unwrap_err();
        assert_eq!(e.field, "recording.dir");
        let e = apply_patch(
            FILE,
            &patch(serde_json::json!({ "marker_hotkey": "f9\nevil = 1" })),
        )
        .unwrap_err();
        assert_eq!(e.field, "marker_hotkey");
        let e = apply_patch("not toml at all [", &Patch::default()).unwrap_err();
        assert!(e.reason.contains("cannot be parsed"), "{e}");

        // What the patcher lets through, the config rules still judge.
        let out = apply_patch("", &patch(serde_json::json!({ "mouse_cpi": -5 }))).unwrap();
        let e = checked(&out, Path::new("t.toml")).unwrap_err();
        assert_eq!(e.field, "mouse_cpi");
        assert!(!e.to_string().contains("t.toml"), "plain message: {e}");
        let out = apply_patch(
            "",
            &patch(serde_json::json!({ "marker_hotkey": "ctrl+alt+r" })),
        )
        .unwrap();
        assert_eq!(
            checked(&out, Path::new("t.toml")).unwrap_err().field,
            "ctl.hotkey"
        );
    }

    #[test]
    fn a_string_cannot_smuggle_toml() {
        let out = apply_patch(
            "",
            &patch(serde_json::json!({ "recording": { "dir": "x\" \nkafka.enabled = true #" } })),
        );
        assert!(out.is_err(), "control characters are refused");
        let out = apply_patch(
            "",
            &patch(serde_json::json!({ "recording": { "dir": "x\"] [kafka" } })),
        )
        .unwrap();
        let c = checked(&out, Path::new("t.toml")).unwrap();
        assert_eq!(c.recording.dir, PathBuf::from("x\"] [kafka"));
        assert!(!c.kafka.enabled);
    }

    #[test]
    fn save_checks_the_token_validates_and_writes_atomically() {
        let d = crate::manager::tmpdir("settings-save");
        let p = d.join("telemouse.toml");
        std::fs::write(
            &p,
            FILE.replace("future_top_level_key_the_form_does_not_know = 1\n", ""),
        )
        .unwrap();
        let before = std::fs::read(&p).unwrap();
        let t = token(Some(&before));

        let stale = save(
            &p,
            "0000-1",
            &patch(serde_json::json!({ "mouse_cpi": 800 })),
        );
        assert!(matches!(stale, Err(SaveError::Stale(ref now)) if *now == t));
        let bad = save(&p, &t, &patch(serde_json::json!({ "mouse_cpi": 0 })));
        assert!(matches!(bad, Err(SaveError::Invalid(ref i)) if i.field == "mouse_cpi"));
        assert_eq!(std::fs::read(&p).unwrap(), before, "nothing written");

        let ok = save(&p, &t, &patch(serde_json::json!({ "mouse_cpi": 800 }))).unwrap();
        assert!(!ok.created);
        assert_eq!(ok.new.mouse_cpi, 800.0);
        assert_eq!(
            changed(ok.old.as_ref().unwrap(), &ok.new),
            vec!["mouse_cpi"]
        );
        let after = std::fs::read(&p).unwrap();
        assert_eq!(ok.token, token(Some(&after)));
        assert!(
            String::from_utf8(after)
                .unwrap()
                .contains("mouse_cpi = 800.0   # G Pro")
        );
        let leftovers: Vec<_> = std::fs::read_dir(&d)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_missing_file_is_created_from_the_sample() {
        let d = crate::manager::tmpdir("settings-seed");
        let p = d.join("telemouse.toml");
        let v = view(&p);
        assert!(!v.exists);
        assert_eq!(v.token, "none");
        assert_eq!(v.settings.as_ref().unwrap().mouse_cpi, 1600.0);

        let ok = save(&p, "none", &patch(serde_json::json!({ "mouse_cpi": 3200 }))).unwrap();
        assert!(ok.created);
        let text = std::fs::read_to_string(&p).unwrap();
        assert!(text.contains("mouse_cpi = 3200.0"));
        assert!(
            text.starts_with("# telemouse configuration"),
            "the sample's comments came along"
        );
        assert_eq!(text.replace("3200.0", "1600.0"), SAMPLE_CONFIG);

        // An unusable file is reported, not replaced.
        std::fs::write(&p, "mouse_cpi = \"fast\"\n").unwrap();
        let v = view(&p);
        assert!(v.exists && v.settings.is_none());
        assert!(v.error.is_some());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn effects_say_who_has_to_wait() {
        assert_eq!(effects(&["recording"], false, false), Effects::default());
        assert_eq!(
            effects(&["ctl.hotkey", "games"], true, true),
            Effects {
                restart_required: vec!["ctl hotkey"],
                next_start: vec!["capture"],
            }
        );
        assert_eq!(effects(&["viz.obs"], true, true).next_start, vec!["viz"]);
        assert!(effects(&["mouse_cpi"], false, true).next_start.is_empty());
    }
}
