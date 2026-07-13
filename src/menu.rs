//! Right-click settings menu model.
//!
//! Pure state + label building — no rendering (that lives in `osd`) and no
//! engine logic (that lives in `slideshow`). Kept tiny so it costs nothing on
//! a Pi Zero: the menu is only ever touched while it is open.

use crate::config::{DisplayConfig, PhotoOrder, Transition, WifiConfig};

/// Live album / favourites filter state for menu labels.
pub struct TargetingMenuCtx<'a> {
    pub album_label: &'a str,
    pub favorites_only: bool,
    pub show_favorites: bool,
}

/// A free-text field the menu can edit via the keyboard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditField {
    WifiSsid,
    WifiPassword,
    /// Plugin connection field: `(plugin_config_index, field_index)`.
    Connection {
        plugin_idx: usize,
        field_idx: usize,
    },
}

impl EditField {
    /// Human label shown before the value, e.g. "Wi-Fi network".
    /// Connection fields use the label from [`ConnectionMenuField`].
    pub fn prefix(self) -> &'static str {
        match self {
            EditField::WifiSsid => "Wi-Fi network",
            EditField::WifiPassword => "Wi-Fi password",
            EditField::Connection { .. } => "Setting",
        }
    }
}

/// One editable connection setting advertised by a plugin.
#[derive(Debug, Clone)]
pub struct ConnectionMenuField {
    pub key: String,
    pub label: String,
    pub secret: bool,
    pub value: String,
}

/// Connection UI for one plugin (capability-driven).
#[derive(Debug, Clone)]
pub struct ConnectionMenuCtx {
    pub plugin_idx: usize,
    pub header: String,
    pub fields: Vec<ConnectionMenuField>,
    pub reconnect_label: Option<String>,
}

/// Open/closed state, selection, and in-progress text edit of the settings menu.
#[derive(Debug, Default)]
pub struct Menu {
    pub open: bool,
    pub selected: usize,
    /// When `Some`, keystrokes are captured into `buffer` for this field
    /// instead of navigating the menu.
    pub editing: Option<EditField>,
    /// In-progress text while `editing` is `Some`.
    pub buffer: String,
}

/// What activating a menu row does. The slideshow matches on this.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuAction {
    /// Inert action carried by section-header rows. Headers are never activated
    /// (keyboard nav skips them, clicks on them are ignored), so this is a
    /// placeholder that the slideshow treats as a no-op.
    Noop,
    TogglePause,
    CycleTransition,
    CycleOrder,
    ToggleNoRepeatShown,
    CycleAlbum,
    ToggleFavoritesFilter,
    ToggleFillScreen,
    ToggleLetterboxBlur,
    ToggleOsd,
    ToggleClock,
    CycleSlideDuration,
    /// Switch the active photo source to `config.plugins[idx]`.
    SwitchSource(usize),
    /// Toggle whether Wi-Fi settings are applied to the host OS.
    ToggleWifi,
    /// Begin editing a free-text field (keyboard capture).
    BeginEdit(EditField),
    /// Apply the current Wi-Fi settings to the host OS (Linux/Pi only).
    ApplyWifi,
    /// Apply edited connection fields and reconnect that plugin source.
    ReconnectPlugin(usize),
    SaveConfig,
    Exit,
}

/// Whether a row is an interactive item or a non-selectable section header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowKind {
    /// A normal, selectable/clickable row.
    Item,
    /// A group title. Skipped by keyboard nav and ignored by clicks; drawn in
    /// the accent style by `osd`.
    Header,
}

/// One rendered row: its label, the action it triggers, and whether it is an
/// interactive item or a section header.
pub struct MenuRow {
    pub label: String,
    pub action: MenuAction,
    pub kind: RowKind,
}

impl MenuRow {
    fn new(label: impl Into<String>, action: MenuAction) -> Self {
        Self {
            label: label.into(),
            action,
            kind: RowKind::Item,
        }
    }

    /// A non-selectable section header (e.g. "PLAYBACK").
    fn header(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            action: MenuAction::Noop,
            kind: RowKind::Header,
        }
    }
}

/// Index of the first selectable (non-header) row, or 0 if there are none.
/// Used to place the initial highlight off the leading section header.
pub fn first_selectable(rows: &[MenuRow]) -> usize {
    rows.iter()
        .position(|r| r.kind == RowKind::Item)
        .unwrap_or(0)
}

/// The next selectable row from `from`, stepping one item in the sign direction
/// of `dir`, wrapping around and skipping headers. Returns `from` unchanged when
/// there are no selectable rows or `dir` is 0.
pub fn next_selectable(rows: &[MenuRow], from: usize, dir: i32) -> usize {
    let n = rows.len();
    if n == 0 || dir == 0 {
        return from.min(n.saturating_sub(1));
    }
    // +1 or -1 modulo n (n - 1 ≡ -1 mod n) so we never index out of range.
    let step = if dir > 0 { 1 } else { n - 1 };
    let mut i = from.min(n - 1);
    for _ in 0..n {
        i = (i + step) % n;
        if rows[i].kind == RowKind::Item {
            return i;
        }
    }
    from.min(n - 1)
}

/// Snap `idx` onto a selectable row: itself if it is an item, otherwise the next
/// item forward. Keeps the highlight off a header after the row set changes
/// (e.g. when the menu first opens or a conditional group appears/disappears).
pub fn snap_to_selectable(rows: &[MenuRow], idx: usize) -> usize {
    let n = rows.len();
    if n == 0 {
        return 0;
    }
    let i = idx.min(n - 1);
    if rows[i].kind == RowKind::Item {
        i
    } else {
        next_selectable(rows, i, 1)
    }
}

/// Everything `build_rows` needs to render the menu. Bundled into a struct so
/// the growing parameter list stays readable at the call sites.
pub struct RowsCtx<'a> {
    pub display: &'a DisplayConfig,
    pub paused: bool,
    /// `(name, is_active)` for each configured source, in config order.
    pub sources: &'a [(String, bool)],
    pub wifi: &'a WifiConfig,
    /// Capability-driven connection settings for plugins that advertise them.
    pub connections: &'a [ConnectionMenuCtx],
    /// Album / favourites targeting when an active plugin supports it.
    pub targeting: Option<&'a TargetingMenuCtx<'a>>,
    /// The field currently being edited (its row shows the live buffer).
    pub editing: Option<EditField>,
    /// Live keystroke buffer for the editing field.
    pub buffer: &'a str,
}

/// Build the menu rows from the live config and UI state. Rebuilt on every
/// change so labels always reflect current values. Source-agnostic for the
/// photo sources; Wi-Fi rows are always shown and connection rows appear when
/// a plugin advertises connection capabilities.
pub fn build_rows(ctx: &RowsCtx) -> Vec<MenuRow> {
    let d = ctx.display;
    let on = |b: bool| if b { "on" } else { "off" };
    let mut rows = Vec::with_capacity(24 + ctx.sources.len());

    // ── Playback ───────────────────────────────────────────────────────────
    rows.push(MenuRow::header("PLAYBACK"));
    rows.push(MenuRow::new(
        if ctx.paused { "Resume" } else { "Pause" },
        MenuAction::TogglePause,
    ));
    rows.push(MenuRow::new(
        format!("Slide time: {}s", d.slide_duration_secs),
        MenuAction::CycleSlideDuration,
    ));
    rows.push(MenuRow::new(
        format!("Transition: {}", transition_name(&d.transition)),
        MenuAction::CycleTransition,
    ));
    rows.push(MenuRow::new(
        format!("Order: {}", order_name(&d.order)),
        MenuAction::CycleOrder,
    ));
    rows.push(MenuRow::new(
        format!("No repeat shown: {}", on(d.no_repeat_shown)),
        MenuAction::ToggleNoRepeatShown,
    ));

    if let Some(t) = ctx.targeting {
        rows.push(MenuRow::header("TARGETING"));
        rows.push(MenuRow::new(
            format!("Album: {}", t.album_label),
            MenuAction::CycleAlbum,
        ));
        if t.show_favorites {
            rows.push(MenuRow::new(
                format!("Favourites only: {}", on(t.favorites_only)),
                MenuAction::ToggleFavoritesFilter,
            ));
        }
    }

    // ── Display ──────────────────────────────────────────────────────────────
    rows.push(MenuRow::header("DISPLAY"));
    rows.push(MenuRow::new(
        format!("Fit: {}", if d.fill_screen { "fill" } else { "letterbox" }),
        MenuAction::ToggleFillScreen,
    ));
    rows.push(MenuRow::new(
        format!("Letterbox blur: {}", on(d.letterbox_blur)),
        MenuAction::ToggleLetterboxBlur,
    ));
    rows.push(MenuRow::new(
        format!("Info overlay: {}", on(d.show_osd)),
        MenuAction::ToggleOsd,
    ));
    rows.push(MenuRow::new(
        format!("Clock: {}", on(d.show_clock)),
        MenuAction::ToggleClock,
    ));

    // ── Sources (only when any are configured) ───────────────────────────────
    if !ctx.sources.is_empty() {
        rows.push(MenuRow::header("SOURCES"));
        for (i, (name, active)) in ctx.sources.iter().enumerate() {
            let mark = if *active { " (active)" } else { "" };
            rows.push(MenuRow::new(
                format!("{name}{mark}"),
                MenuAction::SwitchSource(i),
            ));
        }
    }

    // ── Network (Wi-Fi) ──────────────────────────────────────────────────────
    rows.push(MenuRow::header("NETWORK"));
    rows.push(MenuRow::new(
        format!("Wi-Fi: {}", on(ctx.wifi.enabled)),
        MenuAction::ToggleWifi,
    ));
    rows.push(edit_row(
        ctx,
        EditField::WifiSsid,
        &display_value(&ctx.wifi.ssid),
    ));
    rows.push(edit_row(
        ctx,
        EditField::WifiPassword,
        secret_value(&ctx.wifi.password),
    ));
    rows.push(MenuRow::new("Apply Wi-Fi now", MenuAction::ApplyWifi));

    // ── Connection settings (capability-driven per plugin) ───────────────────
    for conn in ctx.connections {
        rows.push(MenuRow::header(conn.header.clone()));
        for (field_idx, field) in conn.fields.iter().enumerate() {
            let edit = EditField::Connection {
                plugin_idx: conn.plugin_idx,
                field_idx,
            };
            let shown = if field.secret {
                secret_from_set(!field.value.is_empty()).to_string()
            } else {
                display_value(&field.value)
            };
            let label = if ctx.editing == Some(edit) {
                format!("{}: {}_", field.label, ctx.buffer)
            } else {
                format!("{}: {shown}", field.label)
            };
            rows.push(MenuRow::new(label, MenuAction::BeginEdit(edit)));
        }
        if let Some(label) = &conn.reconnect_label {
            rows.push(MenuRow::new(
                label.clone(),
                MenuAction::ReconnectPlugin(conn.plugin_idx),
            ));
        }
    }

    // ── System ───────────────────────────────────────────────────────────────
    rows.push(MenuRow::header("SYSTEM"));
    rows.push(MenuRow::new("Save settings", MenuAction::SaveConfig));
    rows.push(MenuRow::new("Exit", MenuAction::Exit));
    rows
}

/// Build an editable field row: shows the live buffer with a cursor when this is
/// the field being edited, otherwise the supplied display value.
fn edit_row(ctx: &RowsCtx, field: EditField, current: &str) -> MenuRow {
    let label = if ctx.editing == Some(field) {
        // Show what is being typed (even secrets — the user is entering it) with
        // a trailing cursor. font8x8 has no block glyph, so use '_'.
        format!("{}: {}_", field.prefix(), ctx.buffer)
    } else {
        format!("{}: {current}", field.prefix())
    };
    MenuRow::new(label, MenuAction::BeginEdit(field))
}

/// Non-secret value for display, or a placeholder when empty.
fn display_value(s: &str) -> String {
    if s.is_empty() {
        "(unset)".to_string()
    } else {
        s.to_string()
    }
}

/// Masked representation of a secret: fixed-width dots when set (length not
/// leaked), placeholder when empty.
fn secret_value(s: &str) -> &'static str {
    secret_from_set(!s.is_empty())
}

/// Masked label when only a presence flag is available (e.g. PhotoPrism password).
fn secret_from_set(has_secret: bool) -> &'static str {
    if has_secret {
        "****"
    } else {
        "(unset)"
    }
}

fn transition_name(t: &Transition) -> &'static str {
    match t {
        Transition::Cut => "cut",
        Transition::Fade => "fade",
        Transition::SlideLeft => "slide left",
        Transition::SlideRight => "slide right",
    }
}

fn order_name(o: &PhotoOrder) -> &'static str {
    match o {
        PhotoOrder::Shuffle => "shuffle",
        PhotoOrder::Chronological => "chronological",
        PhotoOrder::NewestFirst => "newest first",
        PhotoOrder::DateCluster => "date clusters",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx<'a>(
        d: &'a DisplayConfig,
        paused: bool,
        sources: &'a [(String, bool)],
        wifi: &'a WifiConfig,
    ) -> RowsCtx<'a> {
        RowsCtx {
            display: d,
            paused,
            sources,
            wifi,
            connections: &[],
            targeting: None,
            editing: None,
            buffer: "",
        }
    }

    #[test]
    fn rows_include_sources_and_exit() {
        let d = DisplayConfig::default();
        let w = WifiConfig::default();
        let sources = vec![
            ("directory".to_string(), true),
            ("photoprism".to_string(), false),
        ];
        let rows = build_rows(&ctx(&d, false, &sources, &w));
        assert!(rows.iter().any(|r| matches!(r.action, MenuAction::Exit)));
        assert!(rows
            .iter()
            .any(|r| matches!(r.action, MenuAction::SwitchSource(0))));
        assert!(rows
            .iter()
            .any(|r| matches!(r.action, MenuAction::SwitchSource(1))));
        // The active source is marked.
        assert!(rows
            .iter()
            .any(|r| r.label.contains("directory") && r.label.contains("active")));
    }

    #[test]
    fn pause_label_reflects_state() {
        let d = DisplayConfig::default();
        let w = WifiConfig::default();
        let rows = build_rows(&ctx(&d, true, &[], &w));
        let pause = rows
            .iter()
            .find(|r| matches!(r.action, MenuAction::TogglePause))
            .unwrap();
        assert_eq!(pause.label, "Resume");
    }

    #[test]
    fn rows_start_with_a_header_and_groups_are_present() {
        let d = DisplayConfig::default();
        let w = WifiConfig::default();
        let rows = build_rows(&ctx(&d, false, &[], &w));
        // The first row is a non-selectable section header, never an item.
        assert_eq!(rows[0].kind, RowKind::Header);
        let headers: Vec<&str> = rows
            .iter()
            .filter(|r| r.kind == RowKind::Header)
            .map(|r| r.label.as_str())
            .collect();
        assert!(headers.contains(&"PLAYBACK"));
        assert!(headers.contains(&"DISPLAY"));
        assert!(headers.contains(&"NETWORK"));
        assert!(headers.contains(&"SYSTEM"));
        // No SOURCES / PHOTOPRISM header when neither is configured.
        assert!(!headers.contains(&"SOURCES"));
        assert!(!headers.contains(&"PHOTOPRISM"));
    }

    #[test]
    fn nav_skips_headers_and_wraps() {
        let d = DisplayConfig::default();
        let w = WifiConfig::default();
        let rows = build_rows(&ctx(&d, false, &[], &w));

        // Initial highlight lands on the first item (row after PLAYBACK header).
        let first = first_selectable(&rows);
        assert_eq!(rows[first].kind, RowKind::Item);
        assert_eq!(first, 1, "header at 0, first item at 1");

        // Stepping forward never stops on a header.
        let mut idx = first;
        for _ in 0..rows.len() * 2 {
            idx = next_selectable(&rows, idx, 1);
            assert_eq!(rows[idx].kind, RowKind::Item);
        }
        // Stepping backward from the first item wraps to the last item.
        let last_item = rows.iter().rposition(|r| r.kind == RowKind::Item).unwrap();
        assert_eq!(next_selectable(&rows, first, -1), last_item);

        // Snapping a header index moves forward to an item.
        assert_eq!(snap_to_selectable(&rows, 0), first);
    }

    #[test]
    fn wifi_rows_present_and_password_masked() {
        let d = DisplayConfig::default();
        let w = WifiConfig {
            enabled: true,
            ssid: "home".into(),
            password: "secret".into(),
            password_file: None,
            country: String::new(),
        };
        let rows = build_rows(&ctx(&d, false, &[], &w));
        assert!(rows
            .iter()
            .any(|r| matches!(r.action, MenuAction::ToggleWifi)));
        assert!(rows.iter().any(|r| matches!(
            r.action,
            MenuAction::BeginEdit(EditField::WifiSsid)
        ) && r.label.contains("home")));
        // Password is masked — the secret never appears in a label.
        let pw = rows
            .iter()
            .find(|r| matches!(r.action, MenuAction::BeginEdit(EditField::WifiPassword)))
            .unwrap();
        assert!(!pw.label.contains("secret"));
        assert!(pw.label.contains("****"));
        assert!(rows
            .iter()
            .any(|r| matches!(r.action, MenuAction::ApplyWifi)));
    }

    #[test]
    fn connection_rows_only_when_configured() {
        let d = DisplayConfig::default();
        let w = WifiConfig::default();
        let none = build_rows(&ctx(&d, false, &[], &w));
        assert!(!none
            .iter()
            .any(|r| matches!(r.action, MenuAction::ReconnectPlugin(_))));

        let connections = [ConnectionMenuCtx {
            plugin_idx: 0,
            header: "PHOTOPRISM".into(),
            fields: vec![
                ConnectionMenuField {
                    key: "url".into(),
                    label: "PhotoPrism URL".into(),
                    secret: false,
                    value: "http://pp.local:2342".into(),
                },
                ConnectionMenuField {
                    key: "password".into(),
                    label: "PhotoPrism password".into(),
                    secret: true,
                    value: "secret".into(),
                },
            ],
            reconnect_label: Some("Connect PhotoPrism".into()),
        }];
        let with = build_rows(&RowsCtx {
            connections: &connections,
            ..ctx(&d, false, &[], &w)
        });
        assert!(with
            .iter()
            .any(|r| matches!(r.action, MenuAction::ReconnectPlugin(0))));
        assert!(with.iter().any(|r| r.label.contains("pp.local")));
        let pw = with
            .iter()
            .find(|r| r.label.starts_with("PhotoPrism password:"))
            .unwrap();
        assert!(pw.label.contains("****"));
    }

    #[test]
    fn editing_field_shows_buffer_with_cursor() {
        let d = DisplayConfig::default();
        let w = WifiConfig::default();
        let rows = build_rows(&RowsCtx {
            editing: Some(EditField::WifiSsid),
            buffer: "myne",
            ..ctx(&d, false, &[], &w)
        });
        let ssid = rows
            .iter()
            .find(|r| matches!(r.action, MenuAction::BeginEdit(EditField::WifiSsid)))
            .unwrap();
        assert!(ssid.label.ends_with("myne_"));
    }
}
