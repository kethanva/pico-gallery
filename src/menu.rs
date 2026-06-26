//! Right-click settings menu model.
//!
//! Pure state + label building — no rendering (that lives in `osd`) and no
//! engine logic (that lives in `slideshow`). Kept tiny so it costs nothing on
//! a Pi Zero: the menu is only ever touched while it is open.

use crate::config::{DisplayConfig, PhotoOrder, Transition};

/// Open/closed state and current selection of the settings menu.
#[derive(Debug, Default)]
pub struct Menu {
    pub open: bool,
    pub selected: usize,
}

/// What activating a menu row does. The slideshow matches on this.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuAction {
    TogglePause,
    CycleTransition,
    CycleOrder,
    ToggleFillScreen,
    ToggleLetterboxBlur,
    ToggleOsd,
    ToggleClock,
    CycleSlideDuration,
    /// Switch the active photo source to `config.plugins[idx]`.
    SwitchSource(usize),
    SaveConfig,
    Exit,
}

/// One rendered row: its label and the action it triggers.
pub struct MenuRow {
    pub label: String,
    pub action: MenuAction,
}

impl MenuRow {
    fn new(label: impl Into<String>, action: MenuAction) -> Self {
        Self {
            label: label.into(),
            action,
        }
    }
}

/// Build the menu rows from the live display config, pause state, and the list
/// of configured sources (`(name, is_active)`). Rebuilt on every change so the
/// labels always reflect current values. Source-agnostic: it lists whatever
/// `[[plugins]]` entries exist, so the menu works no matter which plugins are
/// compiled in.
pub fn build_rows(d: &DisplayConfig, paused: bool, sources: &[(String, bool)]) -> Vec<MenuRow> {
    let on = |b: bool| if b { "on" } else { "off" };
    let mut rows = Vec::with_capacity(9 + sources.len());

    rows.push(MenuRow::new(
        if paused { "Resume" } else { "Pause" },
        MenuAction::TogglePause,
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
    rows.push(MenuRow::new(
        format!("Slide time: {}s", d.slide_duration_secs),
        MenuAction::CycleSlideDuration,
    ));

    for (i, (name, active)) in sources.iter().enumerate() {
        let mark = if *active { " (active)" } else { "" };
        rows.push(MenuRow::new(
            format!("Source: {name}{mark}"),
            MenuAction::SwitchSource(i),
        ));
    }

    rows.push(MenuRow::new("Save settings", MenuAction::SaveConfig));
    rows.push(MenuRow::new("Exit", MenuAction::Exit));
    rows
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

    #[test]
    fn rows_include_sources_and_exit() {
        let d = DisplayConfig::default();
        let sources = vec![
            ("directory".to_string(), true),
            ("photoprism".to_string(), false),
        ];
        let rows = build_rows(&d, false, &sources);
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
        let rows = build_rows(&d, true, &[]);
        assert_eq!(rows[0].label, "Resume");
    }
}
