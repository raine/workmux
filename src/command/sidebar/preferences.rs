//! Sidebar preferences shared by every sidebar on a server.
//!
//! tmux keeps them in global options, Zellij and Rex in the state file. Each
//! host decodes them into `Preferences` once, and every reader resolves them
//! the same way: the shared value, else what settings.json persisted, else
//! the config.

use std::collections::HashSet;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::config::{Config, SidebarGroupBy, SidebarPosition};
use crate::state::GlobalSettings;

use super::app::{SidebarFilterMode, SidebarLayoutMode};

/// Grouping the user chose at runtime, which overrides the config's.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum GroupingOverride {
    /// No runtime choice: settings.json or the config decides.
    #[default]
    Unset,
    /// One ungrouped list.
    Flat,
    By(SidebarGroupBy),
}

impl GroupingOverride {
    /// Decode a stored value. An unknown value is no choice.
    pub(super) fn parse(raw: &str) -> Self {
        match super::parse_sidebar_group_by(raw) {
            Ok(None | Some(SidebarGroupBy::None)) => Self::Flat,
            Ok(Some(group_by)) => Self::By(group_by),
            Err(_) => Self::Unset,
        }
    }

    pub(super) fn from_group_by(group_by: Option<SidebarGroupBy>) -> Self {
        match group_by {
            None | Some(SidebarGroupBy::None) => Self::Flat,
            Some(group_by) => Self::By(group_by),
        }
    }

    /// The stored form, or `None` for no choice.
    pub(super) fn value(self) -> Option<&'static str> {
        match self {
            Self::Unset => None,
            Self::Flat => Some(super::group_by_option_value(None)),
            Self::By(group_by) => Some(super::group_by_option_value(Some(group_by))),
        }
    }

    /// The grouping in effect.
    pub(super) fn resolve(self, configured: Option<SidebarGroupBy>) -> Option<SidebarGroupBy> {
        match self {
            Self::Flat => None,
            Self::By(group_by) => Some(group_by),
            Self::Unset => saved_setting(|settings| settings.sidebar_group_by)
                .map_or(configured, |mode| {
                    super::parse_sidebar_group_by(&mode).unwrap_or(configured)
                }),
        }
    }
}

impl Serialize for GroupingOverride {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.value().serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for GroupingOverride {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = Option::<String>::deserialize(deserializer)?;
        Ok(value.as_deref().map_or(Self::Unset, Self::parse))
    }
}

/// Preferences every sidebar on a server shares. A field that is unset
/// falls back to settings.json or the config.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub(super) struct Preferences {
    #[serde(deserialize_with = "lenient")]
    pub position: Option<SidebarPosition>,
    #[serde(deserialize_with = "lenient")]
    pub layout: Option<SidebarLayoutMode>,
    #[serde(deserialize_with = "lenient")]
    pub filter: Option<SidebarFilterMode>,
    /// Whether configured views apply. Unset means they do.
    #[serde(deserialize_with = "lenient")]
    pub views_enabled: Option<bool>,
    #[serde(deserialize_with = "lenient")]
    pub sleeping_panes: HashSet<String>,
    #[serde(rename = "group_by", deserialize_with = "lenient")]
    pub grouping: GroupingOverride,
    /// Group labels the user expanded.
    #[serde(deserialize_with = "lenient")]
    pub expanded_groups: Vec<String>,
}

/// Deserialize a field, reading a value of the wrong shape as its default,
/// so one bad field does not discard the others.
fn lenient<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: serde::de::DeserializeOwned + Default,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(serde_json::from_value(value).unwrap_or_default())
}

impl Preferences {
    pub(super) fn layout_mode(&self, config: &Config) -> SidebarLayoutMode {
        self.layout
            .or_else(|| {
                saved_setting(|settings| settings.sidebar_layout)
                    .and_then(|layout| parse_layout(&layout))
            })
            .or_else(|| config.sidebar.layout.as_deref().and_then(parse_layout))
            .unwrap_or_default()
    }

    pub(super) fn filter_mode(&self) -> SidebarFilterMode {
        filter_mode(self.filter)
    }

    pub(super) fn views_enabled(&self) -> bool {
        self.views_enabled.unwrap_or(true)
    }
}

/// The filter in effect, given the shared value.
pub(super) fn filter_mode(shared: Option<SidebarFilterMode>) -> SidebarFilterMode {
    shared
        .or_else(|| {
            saved_setting(|settings| settings.sidebar_filter)
                .map(|mode| SidebarFilterMode::from_str(&mode))
        })
        .unwrap_or_default()
}

pub(super) fn parse_position(raw: &str) -> Option<SidebarPosition> {
    match raw.trim() {
        "top" => Some(SidebarPosition::Top),
        "left" => Some(SidebarPosition::Left),
        _ => None,
    }
}

pub(super) fn parse_layout(raw: &str) -> Option<SidebarLayoutMode> {
    match raw.trim() {
        "tiles" => Some(SidebarLayoutMode::Tiles),
        "compact" => Some(SidebarLayoutMode::Compact),
        _ => None,
    }
}

/// A sidebar preference settings.json persisted.
fn saved_setting(field: impl FnOnce(GlobalSettings) -> Option<String>) -> Option<String> {
    field(crate::state::StateStore::new().ok()?.load_settings().ok()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grouping_round_trips_through_its_stored_form() {
        for grouping in [
            GroupingOverride::Unset,
            GroupingOverride::Flat,
            GroupingOverride::By(SidebarGroupBy::Project),
            GroupingOverride::By(SidebarGroupBy::Session),
        ] {
            let parsed = grouping
                .value()
                .map_or(GroupingOverride::Unset, GroupingOverride::parse);
            assert_eq!(parsed, grouping);
        }
        assert_eq!(GroupingOverride::parse("off"), GroupingOverride::Flat);
        assert_eq!(GroupingOverride::parse("bogus"), GroupingOverride::Unset);
    }

    #[test]
    fn explicit_grouping_ignores_the_config() {
        let configured = Some(SidebarGroupBy::Project);
        assert_eq!(GroupingOverride::Flat.resolve(configured), None);
        assert_eq!(
            GroupingOverride::By(SidebarGroupBy::Session).resolve(configured),
            Some(SidebarGroupBy::Session)
        );
    }

    #[test]
    fn file_form_keeps_its_shape() {
        let prefs = Preferences {
            position: Some(SidebarPosition::Top),
            layout: Some(SidebarLayoutMode::Compact),
            filter: Some(SidebarFilterMode::Session),
            views_enabled: Some(false),
            sleeping_panes: HashSet::from(["%1".to_string()]),
            grouping: GroupingOverride::Flat,
            expanded_groups: vec!["api".to_string()],
        };
        let json = serde_json::to_value(&prefs).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "position": "top",
                "layout": "compact",
                "filter": "session",
                "views_enabled": false,
                "sleeping_panes": ["%1"],
                "group_by": "none",
                "expanded_groups": ["api"],
            })
        );
        assert_eq!(serde_json::from_value::<Preferences>(json).unwrap(), prefs);
    }

    #[test]
    fn a_bad_field_keeps_the_others() {
        let prefs: Preferences = serde_json::from_value(serde_json::json!({
            "position": "sideways",
            "layout": 7,
            "filter": "session",
            "group_by": "project",
        }))
        .unwrap();
        assert_eq!(prefs.position, None);
        assert_eq!(prefs.layout, None);
        assert_eq!(prefs.filter, Some(SidebarFilterMode::Session));
        assert_eq!(
            prefs.grouping,
            GroupingOverride::By(SidebarGroupBy::Project)
        );
    }
}
