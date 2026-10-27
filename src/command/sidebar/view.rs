//! Rule-based per-window views with a shared on/off toggle.

use std::collections::{HashMap, HashSet};
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Result, bail};

use crate::cmd::Cmd;
use crate::config::SidebarView;
use crate::multiplexer::{AgentPane, LivePaneInfo};
use serde::{Deserialize, Serialize};

pub const OPTION: &str = "@workmux_sidebar_views_enabled";
pub const SCOPE_OPTION: &str = "@workmux_sidebar_scope";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowView {
    pub name: String,
    pub pane_ids: HashSet<String>,
}

pub fn current_window() -> Result<String> {
    let pane = std::env::var("TMUX_PANE").ok();
    let mut args = vec!["display-message", "-p"];
    if let Some(pane) = pane.as_deref() {
        args.extend(["-t", pane]);
    }
    args.push("#{window_id}");
    Ok(Cmd::new("tmux")
        .args(&args)
        .run_and_capture_stdout()?
        .trim()
        .to_string())
}

pub fn read_scope(window: &str) -> Option<WindowView> {
    let value = Cmd::new("tmux")
        .args(&["show-option", "-wqv", "-t", window, SCOPE_OPTION])
        .run_and_capture_stdout()
        .ok()?;
    serde_json::from_str(&value).ok()
}

/// Configured views are enabled unless the server has explicitly disabled them.
pub fn is_enabled(value: Option<&str>) -> bool {
    value.map(str::trim) != Some("off")
}

pub fn set_enabled(mode: Option<&str>) -> Result<()> {
    let enabled = match mode {
        Some("on") => true,
        Some("off") => false,
        Some(mode) => bail!("invalid sidebar view mode {mode:?}; expected on or off"),
        None => {
            let current = Cmd::new("tmux")
                .args(&["show-option", "-gqv", OPTION])
                .run_and_capture_stdout()?;
            !is_enabled(Some(&current))
        }
    };
    Cmd::new("tmux")
        .args(&[
            "set-option",
            "-g",
            OPTION,
            if enabled { "on" } else { "off" },
        ])
        .run()?;
    super::signal_daemon();
    Ok(())
}

fn automatic_name<'a>(views: &'a [SidebarView], session: &str, project: &Path) -> &'a str {
    views
        .iter()
        .find(|view| !view.other && matches(view, session, project))
        .or_else(|| views.iter().find(|view| view.other))
        .map_or("All", |view| view.name.as_str())
}

/// Normalize nonexistent paths lexically too, without confusing sibling names.
fn normalize(path: &Path) -> PathBuf {
    if let Ok(path) = path.canonicalize() {
        return path;
    }
    let mut result = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                result.pop();
            }
            _ => result.push(component),
        }
    }
    result
}

fn matches(view: &SidebarView, session: &str, project: &Path) -> bool {
    view.sessions.iter().any(|name| name == session)
        || view
            .project_dirs
            .iter()
            .any(|dir| project.starts_with(normalize(&crate::util::expand_tilde(dir))))
}

fn includes(views: &[SidebarView], view: &SidebarView, session: &str, project: &Path) -> bool {
    if view.other {
        !views
            .iter()
            .filter(|v| !v.other)
            .any(|v| matches(v, session, project))
    } else {
        matches(view, session, project)
    }
}

/// Cache Git discovery across ticks; refresh so moved repositories recover.
#[derive(Default)]
pub struct ProjectCache {
    projects: HashMap<PathBuf, (Instant, PathBuf)>,
    /// Keep the last active agent when focus moves into its sidebar.
    hosts: HashMap<String, String>,
}

impl ProjectCache {
    fn root(&mut self, path: &Path) -> PathBuf {
        let entry = self
            .projects
            .entry(path.to_path_buf())
            .or_insert_with(|| (Instant::now(), project_root(path)));
        if entry.0.elapsed() > Duration::from_secs(30) {
            *entry = (Instant::now(), project_root(path));
        }
        entry.1.clone()
    }

    pub fn window_views(
        &mut self,
        views: &[SidebarView],
        agents: &[AgentPane],
        panes: &HashMap<String, LivePaneInfo>,
        active: &HashSet<String>,
        enabled: bool,
    ) -> HashMap<String, WindowView> {
        if views.is_empty() {
            self.projects.clear();
            self.hosts.clear();
            return HashMap::new();
        }
        self.projects.retain(|path, _| {
            agents.iter().any(|a| &a.path == path) || panes.values().any(|p| &p.working_dir == path)
        });
        let needs_paths = views.iter().any(|v| !v.project_dirs.is_empty());
        let projects: HashMap<_, _> = agents
            .iter()
            .map(|agent| {
                let root = if needs_paths {
                    self.root(&agent.path)
                } else {
                    agent.path.clone()
                };
                (agent.pane_id.as_str(), root)
            })
            .collect();
        let memberships: HashMap<_, _> = views
            .iter()
            .map(|view| {
                let ids: HashSet<String> = agents
                    .iter()
                    .filter(|a| includes(views, view, &a.session, &projects[a.pane_id.as_str()]))
                    .map(|a| a.pane_id.clone())
                    .collect();
                (view.name.as_str(), ids)
            })
            .collect();
        let mut windows: HashMap<&str, Vec<(&String, &LivePaneInfo)>> = HashMap::new();
        for (id, pane) in panes {
            if let Some(window) = pane.window_id.as_deref() {
                windows.entry(window).or_default().push((id, pane));
            }
        }
        self.hosts
            .retain(|window, _| windows.contains_key(window.as_str()));
        windows
            .into_iter()
            .map(|(window, mut window_panes)| {
                window_panes.sort_by_key(|(id, _)| {
                    id.trim_start_matches('%')
                        .parse::<u64>()
                        .unwrap_or(u64::MAX)
                });
                let previous = self.hosts.get(window);
                let host = agents
                    .iter()
                    .filter(|a| a.window_id == window)
                    .min_by_key(|a| {
                        (
                            !active.contains(&a.pane_id),
                            previous != Some(&a.pane_id),
                            &a.pane_id,
                        )
                    });
                if let Some(agent) = host {
                    self.hosts.insert(window.to_string(), agent.pane_id.clone());
                }
                let name = if !enabled {
                    "All"
                } else if let Some(agent) = host {
                    automatic_name(views, &agent.session, &projects[agent.pane_id.as_str()])
                } else {
                    let (_, pane) = window_panes[0];
                    let root = if needs_paths {
                        self.root(&pane.working_dir)
                    } else {
                        pane.working_dir.clone()
                    };
                    automatic_name(views, pane.session.as_deref().unwrap_or_default(), &root)
                };
                let pane_ids = memberships
                    .get(name)
                    .cloned()
                    .unwrap_or_else(|| agents.iter().map(|a| a.pane_id.clone()).collect());
                (
                    window.to_string(),
                    WindowView {
                        name: name.to_string(),
                        pane_ids,
                    },
                )
            })
            .collect()
    }
}

fn project_root(path: &Path) -> PathBuf {
    normalize(
        &crate::git::get_main_worktree_root_in(Some(path)).unwrap_or_else(|_| path.to_path_buf()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn views() -> Vec<SidebarView> {
        serde_yaml::from_str("- name: Named\n  sessions: [foo, bar, xyz]\n- name: Code\n  project_dirs: [/home/me/code]\n- name: Work\n  project_dirs: [/home/me/work]\n- name: Other\n  other: true\n").unwrap()
    }

    #[test]
    fn matching_and_complement() {
        let views = views();
        let code = Path::new("/home/me/code/app");
        assert!(includes(&views, &views[0], "foo", code));
        assert!(includes(&views, &views[1], "foo", code));
        assert!(!includes(&views, &views[3], "foo", Path::new("/elsewhere")));
        assert!(!includes(&views, &views[3], "misc", code));
        assert!(includes(
            &views,
            &views[3],
            "misc",
            Path::new("/home/me/code-other")
        ));
        assert!(matches(&views[1], "misc", Path::new("/home/me/code")));
        assert!(!matches(&views[0], "Foo", code));
        assert!(!includes(
            &views,
            &views[3],
            "misc",
            Path::new("/home/me/work/app")
        ));
    }

    #[test]
    fn views_default_to_on_and_can_be_disabled() {
        assert!(is_enabled(None));
        assert!(is_enabled(Some("")));
        assert!(is_enabled(Some("on")));
        assert!(!is_enabled(Some("off")));
        assert!(!is_enabled(Some("off\n")));
        assert!(set_enabled(Some("Named")).is_err());
    }

    #[test]
    fn home_expansion_and_directory_boundaries() {
        let view = SidebarView {
            name: "Home".into(),
            project_dirs: vec!["~/code/../work".into()],
            ..Default::default()
        };
        let home = crate::util::expand_tilde("~");
        assert!(matches(&view, "", &normalize(&home.join("work/app"))));
        assert!(!matches(
            &view,
            "",
            &normalize(&home.join("work-other/app"))
        ));
    }

    #[test]
    fn linked_worktree_uses_main_project_location() {
        let temp = tempfile::tempdir().unwrap();
        let main = temp.path().join("code/project");
        let linked = temp.path().join("elsewhere/feature");
        std::fs::create_dir_all(&main).unwrap();
        for args in [
            vec!["init", "-q"],
            vec![
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.com",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-qm",
                "initial",
                "--allow-empty",
            ],
            vec![
                "worktree",
                "add",
                "-qb",
                "feature",
                linked.to_str().unwrap(),
            ],
        ] {
            Cmd::new("git").args(&args).workdir(&main).run().unwrap();
        }
        assert_eq!(project_root(&linked), main.canonicalize().unwrap());
        let view = SidebarView {
            name: "Code".into(),
            project_dirs: vec![temp.path().join("code").to_string_lossy().into_owned()],
            ..Default::default()
        };
        assert!(matches(&view, "", &project_root(&linked)));
        assert_eq!(
            project_root(&temp.path().join("non-repo")),
            normalize(&temp.path().join("non-repo"))
        );
    }

    fn agent(id: &str, window: &str, session: &str, path: &str) -> AgentPane {
        AgentPane {
            pane_id: id.into(),
            window_id: window.into(),
            session: session.into(),
            path: path.into(),
            window_name: window.into(),
            window_index: None,
            pane_title: None,
            status: None,
            status_ts: None,
            activity_ts: None,
            updated_ts: None,
            window_cmd: None,
            agent_command: None,
            agent_kind: None,
            prompt: None,
        }
    }

    fn live(agent: &AgentPane) -> LivePaneInfo {
        LivePaneInfo {
            window_id: Some(agent.window_id.clone()),
            session: Some(agent.session.clone()),
            working_dir: agent.path.clone(),
            pid: None,
            current_command: None,
            title: None,
            window: None,
            session_id: None,
            window_index: None,
        }
    }

    #[test]
    fn automatic_selection_prefers_regular_rules_then_other_then_all() {
        let mut views = views();
        let other = views.pop().unwrap();
        views.insert(0, other);
        assert_eq!(
            automatic_name(&views, "foo", Path::new("/home/me/code/app")),
            "Named"
        );
        assert_eq!(
            automatic_name(&views, "misc", Path::new("/home/me/work/app")),
            "Work"
        );
        assert_eq!(
            automatic_name(&views, "misc", Path::new("/elsewhere")),
            "Other"
        );
        assert_eq!(
            automatic_name(&views[1..], "misc", Path::new("/elsewhere")),
            "All"
        );
        assert_eq!(automatic_name(&[], "foo", Path::new("/elsewhere")), "All");
    }

    #[test]
    fn window_views_are_independent_and_focus_in_sidebar_keeps_the_last_agent() {
        let views: Vec<SidebarView> = serde_yaml::from_str(
            "- {name: Other, other: true}\n- {name: Named, sessions: [foo]}\n",
        )
        .unwrap();
        let agents = vec![
            agent("%1", "@1", "foo", "/one"),
            agent("%2", "@2", "bar", "/two"),
            agent("%3", "@2", "foo", "/three"),
        ];
        let panes = agents
            .iter()
            .map(|a| (a.pane_id.clone(), live(a)))
            .collect();
        let mut cache = ProjectCache::default();
        let active = HashSet::from(["%1".into(), "%2".into()]);
        let resolved = cache.window_views(&views, &agents, &panes, &active, true);
        assert_eq!(resolved["@1"].name, "Named");
        assert_eq!(
            resolved["@1"].pane_ids,
            HashSet::from(["%1".into(), "%3".into()])
        );
        assert_eq!(resolved["@2"].name, "Other");
        assert_eq!(resolved["@2"].pane_ids, HashSet::from(["%2".into()]));

        let active = HashSet::from(["%3".into()]);
        assert_eq!(
            cache.window_views(&views, &agents, &panes, &active, true)["@2"].name,
            "Named"
        );
        let inactive = HashSet::new();
        assert_eq!(
            cache.window_views(&views, &agents, &panes, &inactive, true)["@2"].name,
            "Named"
        );

        let resolved = cache.window_views(&views, &agents, &panes, &inactive, false);
        assert_eq!(resolved["@1"].name, "All");
        assert_eq!(resolved["@2"].name, "All");
        assert_eq!(resolved["@1"].pane_ids.len(), 3);
        assert_eq!(resolved["@1"].pane_ids, resolved["@2"].pane_ids);
        let restored = cache.window_views(
            &views,
            &agents,
            &panes,
            &HashSet::from(["%1".into(), "%2".into()]),
            true,
        );
        assert_eq!(restored["@1"].name, "Named");
        assert_eq!(restored["@2"].name, "Other");
        for enabled in [true, false] {
            assert!(
                cache
                    .window_views(&[], &agents, &panes, &inactive, enabled)
                    .is_empty()
            );
        }
    }

    #[test]
    fn unregistered_window_uses_its_oldest_panes_directory() {
        let first = agent("%2", "@1", "misc", "/home/me/work/project");
        let sidebar = agent("%10", "@1", "misc", "/home/me/code/project");
        let panes = HashMap::from([("%2".into(), live(&first)), ("%10".into(), live(&sidebar))]);
        let resolved = ProjectCache::default().window_views(
            &views(),
            &[],
            &panes,
            &HashSet::from(["%10".into()]),
            true,
        );
        assert_eq!(resolved["@1"].name, "Work");
        assert!(resolved["@1"].pane_ids.is_empty());
    }

    #[test]
    fn validate_config() {
        crate::config::validate_sidebar_views(&views()).unwrap();
        for yaml in [
            "- {name: All, sessions: [foo]}",
            "- {name: empty}",
            "- {name: Other, other: true, sessions: [foo]}",
            "- {name: A, other: true}\n- {name: B, other: true}",
            "- {name: A, sessions: [foo]}\n- {name: A, sessions: [bar]}",
            "- {name: A, project_dirs: [relative]}",
        ] {
            let views: Vec<SidebarView> = serde_yaml::from_str(yaml).unwrap();
            assert!(
                crate::config::validate_sidebar_views(&views).is_err(),
                "{yaml}"
            );
        }
    }
}
