use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Color {
    pub r: f32,
    pub g: f32,
    pub b: f32,
    pub a: f32,
}

impl Color {
    pub const fn rgba(r: u8, g: u8, b: u8, a: u8) -> Self {
        Self {
            r: r as f32 / 255.0,
            g: g as f32 / 255.0,
            b: b as f32 / 255.0,
            a: a as f32 / 255.0,
        }
    }

    /// Parses `#RRGGBB` or `#RRGGBBAA`.
    pub fn parse(s: &str) -> Option<Self> {
        let hex = s.trim().strip_prefix('#')?;
        if !(hex.len() == 6 || hex.len() == 8) || !hex.is_ascii() {
            return None;
        }
        let byte = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).ok();
        let a = if hex.len() == 8 { byte(6)? } else { 255 };
        Some(Self::rgba(byte(0)?, byte(2)?, byte(4)?, a))
    }

    pub fn alpha(self, a: f32) -> Self {
        Self { a: self.a * a, ..self }
    }
}

impl Color {
    /// `#RRGGBB`, or `#RRGGBBAA` when not opaque.
    pub fn to_hex(self) -> String {
        let b = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
        let (r, g, bl, a) = (b(self.r), b(self.g), b(self.b), b(self.a));
        if a == 255 { format!("#{r:02x}{g:02x}{bl:02x}") } else { format!("#{r:02x}{g:02x}{bl:02x}{a:02x}") }
    }
}

impl Serialize for Color {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_hex())
    }
}

impl<'de> Deserialize<'de> for Color {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Color::parse(&s).ok_or_else(|| {
            serde::de::Error::custom(format!("invalid color {s:?}, expected \"#RRGGBB\" or \"#RRGGBBAA\""))
        })
    }
}

#[derive(Deserialize, Serialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum Position {
    #[default]
    Bottom,
    Top,
    Left,
    Right,
}

#[derive(Deserialize, Serialize, Clone, Debug, PartialEq)]
#[serde(untagged)]
pub enum MonitorSel {
    Index(usize),
    Name(String),
}

impl Default for MonitorSel {
    fn default() -> Self {
        MonitorSel::Name("primary".into())
    }
}

#[derive(Deserialize, Serialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum Theme {
    #[default]
    Auto,
    Dark,
    Light,
}

#[derive(Deserialize, Serialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum IndicatorStyle {
    #[default]
    Dot,
    Line,
    None,
}

#[derive(Deserialize, Serialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct Dock {
    pub position: Position,
    pub monitor: MonitorSel,
    pub autohide: bool,
    pub autohide_delay_ms: u32,
    pub reserve_space: bool,
    pub always_on_top: bool,
    pub show_running: bool,
    pub show_all_desktops: bool,
    pub show_labels: bool,
    pub hide_on_fullscreen: bool,
    pub click_minimizes: bool,
    pub autostart: bool,
    pub gpu: bool,
}

impl Default for Dock {
    fn default() -> Self {
        Self {
            position: Position::Bottom,
            monitor: MonitorSel::default(),
            autohide: false,
            autohide_delay_ms: 400,
            reserve_space: false,
            always_on_top: true,
            show_running: true,
            show_all_desktops: false,
            show_labels: true,
            hide_on_fullscreen: true,
            click_minimizes: true,
            autostart: false,
            gpu: false,
        }
    }
}

#[derive(Deserialize, Serialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct Appearance {
    pub theme: Theme,
    pub icon_size: f32,
    pub spacing: f32,
    /// Older single padding value; used for any direction not set below.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub padding: Option<f32>,
    /// Left and right of the icons.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub padding_x: Option<f32>,
    /// Above and below the icons.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub padding_y: Option<f32>,
    pub edge_margin: f32,
    pub corner_radius: f32,
    pub blur: bool,
    pub indicator: IndicatorStyle,
    pub background: Option<Color>,
    pub border: Option<Color>,
    pub indicator_color: Option<Color>,
    pub active_indicator_color: Option<Color>,
    pub attention_color: Option<Color>,
    pub label_background: Option<Color>,
    pub label_text: Option<Color>,
    pub label_font: String,
    pub label_size: f32,
}

impl Appearance {
    pub fn padding_x(&self) -> f32 {
        self.padding_x.or(self.padding).unwrap_or(8.0)
    }
    pub fn padding_y(&self) -> f32 {
        self.padding_y.or(self.padding).unwrap_or(8.0)
    }
}

impl Default for Appearance {
    fn default() -> Self {
        Self {
            theme: Theme::Auto,
            icon_size: 48.0,
            spacing: 6.0,
            padding: None,
            padding_x: None,
            padding_y: None,
            edge_margin: 8.0,
            corner_radius: 18.0,
            blur: true,
            indicator: IndicatorStyle::Dot,
            background: None,
            border: None,
            indicator_color: None,
            active_indicator_color: None,
            attention_color: None,
            label_background: None,
            label_text: None,
            label_font: "Segoe UI Variable Text".into(),
            label_size: 13.0,
        }
    }
}

#[derive(Deserialize, Serialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct Magnification {
    pub enabled: bool,
    pub scale: f32,
    pub range: f32,
}

impl Default for Magnification {
    fn default() -> Self {
        Self { enabled: true, scale: 1.6, range: 3.0 }
    }
}

#[derive(Deserialize, Serialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct Animation {
    pub enabled: bool,
    pub speed: f32,
    pub bounce_on_launch: bool,
}

impl Default for Animation {
    fn default() -> Self {
        Self { enabled: true, speed: 1.0, bounce_on_launch: true }
    }
}

#[derive(Deserialize, Serialize, Clone, Debug, PartialEq, Default)]
pub struct Pinned {
    pub path: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub args: Option<String>,
    #[serde(default)]
    pub working_dir: Option<String>,
}

#[derive(Deserialize, Serialize, Clone, Debug, PartialEq, Default)]
#[serde(default)]
pub struct Config {
    pub dock: Dock,
    pub appearance: Appearance,
    pub magnification: Magnification,
    pub animation: Animation,
    pub pinned: Vec<Pinned>,
}

/// Resolved colors for the current theme.
#[derive(Clone, Copy, Debug)]
pub struct Palette {
    pub background: Color,
    pub border: Color,
    pub separator: Color,
    pub indicator: Color,
    pub active: Color,
    pub attention: Color,
    pub label_bg: Color,
    pub label_text: Color,
    pub placeholder: Color,
}

impl Config {
    fn sanitize(&mut self) {
        let a = &mut self.appearance;
        a.icon_size = a.icon_size.clamp(16.0, 256.0);
        a.spacing = a.spacing.clamp(0.0, 64.0);
        for p in [&mut a.padding, &mut a.padding_x, &mut a.padding_y] {
            *p = p.map(|v| v.clamp(0.0, 64.0));
        }
        a.edge_margin = a.edge_margin.clamp(0.0, 200.0);
        a.corner_radius = a.corner_radius.max(0.0);
        a.label_size = a.label_size.clamp(6.0, 48.0);
        let m = &mut self.magnification;
        m.scale = m.scale.clamp(1.0, 4.0);
        m.range = m.range.clamp(1.0, 10.0);
        self.animation.speed = self.animation.speed.clamp(0.1, 10.0);
        self.dock.autohide_delay_ms = self.dock.autohide_delay_ms.min(10_000);
        self.pinned.retain(|p| !p.path.trim().is_empty());
    }

    pub fn palette(&self, dark: bool, blur: bool) -> Palette {
        let a = &self.appearance;
        let def = if dark {
            Palette {
                background: Color::rgba(0x1c, 0x1c, 0x1e, if blur { 0x8c } else { 0xeb }),
                border: Color::rgba(0xff, 0xff, 0xff, 0x24),
                separator: Color::rgba(0xff, 0xff, 0xff, 0x38),
                indicator: Color::rgba(0xff, 0xff, 0xff, 0xa8),
                active: Color::rgba(0xff, 0xff, 0xff, 0xff),
                attention: Color::rgba(0xff, 0xb0, 0x2e, 0xff),
                label_bg: Color::rgba(0x2a, 0x2a, 0x2d, 0xf2),
                label_text: Color::rgba(0xf5, 0xf5, 0xf7, 0xff),
                placeholder: Color::rgba(0x5a, 0x5a, 0x60, 0xff),
            }
        } else {
            Palette {
                background: Color::rgba(0xf6, 0xf6, 0xf8, if blur { 0x99 } else { 0xf0 }),
                border: Color::rgba(0x00, 0x00, 0x00, 0x1c),
                separator: Color::rgba(0x00, 0x00, 0x00, 0x2a),
                indicator: Color::rgba(0x1d, 0x1d, 0x1f, 0x90),
                active: Color::rgba(0x1d, 0x1d, 0x1f, 0xe6),
                attention: Color::rgba(0xe8, 0x8a, 0x00, 0xff),
                label_bg: Color::rgba(0xfb, 0xfb, 0xfd, 0xf5),
                label_text: Color::rgba(0x1d, 0x1d, 0x1f, 0xff),
                placeholder: Color::rgba(0xb8, 0xb8, 0xc0, 0xff),
            }
        };
        Palette {
            background: a.background.unwrap_or(def.background),
            border: a.border.unwrap_or(def.border),
            separator: a.border.map(|c| c.alpha(1.6)).unwrap_or(def.separator),
            indicator: a.indicator_color.unwrap_or(def.indicator),
            active: a.active_indicator_color.unwrap_or(def.active),
            attention: a.attention_color.unwrap_or(def.attention),
            label_bg: a.label_background.unwrap_or(def.label_bg),
            label_text: a.label_text.unwrap_or(def.label_text),
            placeholder: def.placeholder,
        }
    }
}

pub fn config_path() -> PathBuf {
    crate::util::data_dir().join("config.toml")
}

pub fn parse(text: &str) -> Result<Config, String> {
    let mut cfg: Config = toml::from_str(text).map_err(|e| e.to_string())?;
    cfg.sanitize();
    Ok(cfg)
}

/// Loads the config, writing the default one first if none exists.
/// Returns the parsed config (or the error) plus the raw text.
pub fn load(path: &Path) -> (Result<Config, String>, String) {
    if !path.exists() {
        let _ = std::fs::create_dir_all(path.parent().unwrap_or(Path::new(".")));
        let _ = std::fs::write(path, default_text());
    }
    match std::fs::read_to_string(path) {
        Ok(text) => (parse(&text), text),
        Err(e) => (Err(format!("cannot read {}: {e}", path.display())), String::new()),
    }
}

/// Rewrites only the `[[pinned]]` entries, keeping the user's comments and formatting elsewhere.
pub fn save_pinned(path: &Path, pinned: &[Pinned]) -> Result<String, String> {
    let mut doc = read_document(path)?;
    write_pinned(&mut doc, pinned);
    write_document(path, &doc)
}

/// Writes every setting, editing values in place so comments, layout and unknown keys survive.
pub fn save(path: &Path, cfg: &Config) -> Result<String, String> {
    let mut doc = read_document(path)?;
    let fresh = toml::to_string(cfg)
        .map_err(|e| e.to_string())?
        .parse::<toml_edit::DocumentMut>()
        .map_err(|e| e.to_string())?;
    for section in ["dock", "appearance", "magnification", "animation"] {
        let Some(values) = fresh.get(section).and_then(|i| i.as_table()) else { continue };
        if !doc.contains_table(section) {
            doc[section] = toml_edit::table();
        }
        let Some(table) = doc[section].as_table_mut() else { continue };
        for (key, item) in values.iter() {
            if let Some(v) = item.as_value() {
                set_value(table, key, tidy(v.clone()));
            }
        }
        // Optional settings that are now unset (e.g. a colour back to the theme default).
        for key in OPTIONAL_KEYS {
            if !values.contains_key(key) {
                table.remove(key);
            }
        }
    }
    write_pinned(&mut doc, &cfg.pinned);
    write_document(path, &doc)
}

const OPTIONAL_KEYS: [&str; 10] = [
    "padding",
    "padding_x",
    "padding_y",
    "background",
    "border",
    "indicator_color",
    "active_indicator_color",
    "attention_color",
    "label_background",
    "label_text",
];

fn read_document(path: &Path) -> Result<toml_edit::DocumentMut, String> {
    let text = std::fs::read_to_string(path).unwrap_or_else(|_| default_text());
    text.parse::<toml_edit::DocumentMut>().map_err(|e| e.to_string())
}

fn write_document(path: &Path, doc: &toml_edit::DocumentMut) -> Result<String, String> {
    let out = doc.to_string();
    std::fs::write(path, &out).map_err(|e| e.to_string())?;
    Ok(out)
}

/// Replaces a value but keeps its surrounding whitespace and trailing comment.
fn set_value(table: &mut toml_edit::Table, key: &str, mut value: toml_edit::Value) {
    match table.get_mut(key).and_then(|i| i.as_value_mut()) {
        Some(old) => {
            if old.to_string().trim() == value.to_string().trim() {
                return;
            }
            *value.decor_mut() = old.decor().clone();
            *old = value;
        }
        None => {
            table[key] = toml_edit::Item::Value(value);
        }
    }
}

/// Settings are f32; write `48` rather than `48.0` and `1.6` rather than `1.600000023841858`.
fn tidy(v: toml_edit::Value) -> toml_edit::Value {
    match v.as_float() {
        Some(f) if f.fract() == 0.0 && f.abs() < 1e9 => (f as i64).into(),
        Some(f) => ((f * 1000.0).round() / 1000.0).into(),
        None => v,
    }
}

fn write_pinned(doc: &mut toml_edit::DocumentMut, pinned: &[Pinned]) {
    // The comment block above the first [[pinned]] belongs to that table; carry it over.
    let comment = doc
        .get("pinned")
        .and_then(|i| i.as_array_of_tables())
        .and_then(|a| a.get(0))
        .and_then(|t| t.decor().prefix())
        .and_then(|p| p.as_str())
        .filter(|p| p.contains('#'))
        .map_or_else(|| PINNED_COMMENT.to_string(), str::to_string);
    let mut arr = toml_edit::ArrayOfTables::new();
    for p in pinned {
        let mut t = toml_edit::Table::new();
        t["path"] = toml_edit::value(p.path.clone());
        if let Some(n) = &p.name {
            t["name"] = toml_edit::value(n.clone());
        }
        if let Some(a) = &p.args {
            t["args"] = toml_edit::value(a.clone());
        }
        if let Some(w) = &p.working_dir {
            t["working_dir"] = toml_edit::value(w.clone());
        }
        arr.push(t);
    }
    if let Some(first) = arr.get_mut(0) {
        first.decor_mut().set_prefix(comment);
    }
    doc["pinned"] = toml_edit::Item::ArrayOfTables(arr);
}

fn default_pins() -> Vec<(String, &'static str)> {
    let windir = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into());
    let local = std::env::var("LOCALAPPDATA").unwrap_or_default();
    let mut pins = vec![(format!(r"{windir}\explorer.exe"), "File Explorer")];
    for edge in [
        r"C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe",
        r"C:\Program Files\Microsoft\Edge\Application\msedge.exe",
    ] {
        if Path::new(edge).exists() {
            pins.push((edge.into(), "Microsoft Edge"));
            break;
        }
    }
    let packaged = |family: &str| Path::new(&format!(r"{local}\Microsoft\WindowsApps\{family}")).exists();
    if packaged("Microsoft.WindowsTerminal_8wekyb3d8bbwe") {
        pins.push((r"shell:AppsFolder\Microsoft.WindowsTerminal_8wekyb3d8bbwe!App".into(), "Terminal"));
    }
    if packaged("Microsoft.WindowsNotepad_8wekyb3d8bbwe") {
        pins.push((r"shell:AppsFolder\Microsoft.WindowsNotepad_8wekyb3d8bbwe!App".into(), "Notepad"));
    } else {
        pins.push((format!(r"{windir}\System32\notepad.exe"), "Notepad"));
    }
    pins.push((
        r"shell:AppsFolder\windows.immersivecontrolpanel_cw5n1h2txyewy!microsoft.windows.immersivecontrolpanel".into(),
        "Settings",
    ));
    pins
}

pub fn default_text() -> String {
    let mut s = String::from(DEFAULT_HEADER);
    s.push_str(PINNED_COMMENT.trim_end_matches('\n'));
    for (path, name) in default_pins() {
        s.push_str(&format!("\n[[pinned]]\npath = '{path}'\nname = \"{name}\"\n"));
    }
    s
}

const DEFAULT_HEADER: &str = r##"# Windock configuration
# Changes apply as soon as you save this file.

[dock]
position = "bottom"          # bottom | top | left | right
monitor = "primary"          # "primary" or a monitor number (0, 1, ...)
autohide = false             # slide the dock away until the mouse touches the screen edge
autohide_delay_ms = 400
reserve_space = false        # stop maximized windows from covering the dock
always_on_top = true
show_running = true          # also show running apps that aren't pinned
show_all_desktops = false    # include apps on other virtual desktops
show_labels = true           # show the app name while hovering
hide_on_fullscreen = true    # get out of the way of games and videos
click_minimizes = true       # clicking the focused app minimizes it
autostart = false            # start Windock when you sign in
gpu = false                  # render on the GPU (uses ~40 MB more memory; software is plenty for a dock)

[appearance]
theme = "auto"               # auto | dark | light
icon_size = 48
spacing = 6
padding_x = 8                # left and right of the icons
padding_y = 8                # above and below the icons
edge_margin = 8              # gap between the dock and the screen edge
corner_radius = 18
blur = true                  # frosted glass (needs Windows transparency effects on)
indicator = "dot"            # dot | line | none
label_font = "Segoe UI Variable Text"
label_size = 13
# Colors are "#RRGGBB" or "#RRGGBBAA". Leave them out to follow the theme.
# background = "#1c1c1e8c"
# border = "#ffffff24"
# indicator_color = "#ffffffa8"
# active_indicator_color = "#ffffff"
# attention_color = "#ffb02e"
# label_background = "#2a2a2df2"
# label_text = "#f5f5f7"

[magnification]
enabled = true
scale = 1.6                  # hovered icon size relative to normal (1.0 = off)
range = 3.0                  # how many neighbouring icons grow along with it

[animation]
enabled = true
speed = 1.0
bounce_on_launch = true
"##;

const PINNED_COMMENT: &str = r"
# Pinned apps, in order. `path` can be an .exe, a shortcut (.lnk), a folder,
# or a Store app written as 'shell:AppsFolder\<AppUserModelID>'.
# Optional: name, args, working_dir.
# Drop files onto the dock to pin them; drag an icon off the dock to remove it.
# Right-click the dock > Import taskbar pins to add what's pinned to the taskbar.
";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_parses() {
        let cfg = parse(&default_text()).unwrap();
        assert!(!cfg.pinned.is_empty());
    }

    #[test]
    fn saving_pins_keeps_comments_and_settings() {
        let path = std::env::temp_dir().join(format!("windock-test-{}.toml", std::process::id()));
        std::fs::write(&path, default_text().replace("icon_size = 48", "icon_size = 40  # mine")).unwrap();
        let pins = vec![Pinned { path: r"C:\a.exe".into(), ..Default::default() }, Pinned { path: r"C:\b.lnk".into(), name: Some("B".into()), ..Default::default() }];
        let text = save_pinned(&path, &pins).unwrap();
        let _ = std::fs::remove_file(&path);
        assert!(text.contains("icon_size = 40  # mine"));
        assert!(text.contains("# Pinned apps, in order."));
        assert_eq!(parse(&text).unwrap().pinned, pins);
    }

    #[test]
    fn saving_everything_keeps_comments_and_tidies_numbers() {
        let path = std::env::temp_dir().join(format!("windock-test3-{}.toml", std::process::id()));
        let text = default_text().replace("# background = \"#1c1c1e8c\"", "background = \"#112233\"");
        std::fs::write(&path, &text).unwrap();
        let mut cfg = parse(&text).unwrap();
        cfg.appearance.icon_size = 40.0;
        cfg.magnification.scale = 1.7;
        cfg.dock.autohide = true;
        cfg.appearance.background = None;
        cfg.appearance.border = Some(Color::rgba(255, 255, 255, 0x20));
        let out = save(&path, &cfg).unwrap();
        let _ = std::fs::remove_file(&path);
        assert!(out.contains("autohide = true             # slide the dock away"), "{out}");
        assert!(out.contains("icon_size = 40\n"));
        assert!(out.contains("scale = 1.7 "));
        assert!(!out.contains("background = \"#112233\""));
        assert!(out.contains("border = \"#ffffff20\""));
        assert!(out.contains("# Pinned apps, in order."));
        assert_eq!(parse(&out).unwrap(), cfg);
    }

    #[test]
    fn single_padding_still_works_and_split_padding_overrides_it() {
        let old = parse("[appearance]\npadding = 16\n").unwrap();
        assert_eq!((old.appearance.padding_x(), old.appearance.padding_y()), (16.0, 16.0));
        let split = parse("[appearance]\npadding = 16\npadding_y = 4\n").unwrap();
        assert_eq!((split.appearance.padding_x(), split.appearance.padding_y()), (16.0, 4.0));
    }

    #[test]
    fn saving_split_padding_replaces_the_old_key() {
        let path = std::env::temp_dir().join(format!("windock-test4-{}.toml", std::process::id()));
        std::fs::write(&path, "[appearance]\nicon_size = 32\npadding = 16\n").unwrap();
        let mut cfg = parse(&std::fs::read_to_string(&path).unwrap()).unwrap();
        cfg.appearance.padding = None;
        cfg.appearance.padding_x = Some(12.0);
        cfg.appearance.padding_y = Some(6.0);
        let out = save(&path, &cfg).unwrap();
        let _ = std::fs::remove_file(&path);
        assert!(!out.contains("padding ="), "{out}");
        assert!(out.contains("padding_x = 12") && out.contains("padding_y = 6"), "{out}");
    }

    #[test]
    fn saving_pins_restores_a_lost_comment() {
        let path = std::env::temp_dir().join(format!("windock-test2-{}.toml", std::process::id()));
        std::fs::write(&path, "[dock]\nautohide = true\n\n[[pinned]]\npath = 'C:\\x.exe'\n").unwrap();
        let text = save_pinned(&path, &[Pinned { path: r"C:\x.exe".into(), ..Default::default() }]).unwrap();
        let _ = std::fs::remove_file(&path);
        assert!(text.contains("# Pinned apps, in order."));
        assert!(text.contains("autohide = true"));
    }
}
