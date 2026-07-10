//! scenes.toml の読み込みと、シーン名からレイヤー一覧への解決。
//!
//! フォーマット:
//! ```toml
//! default = ["status-bar", "task-var"]
//!
//! [scenes.ssbrowse]
//! extends = "default"
//! add = ["ssbrowse"]
//!
//! [scenes.touch-paint]
//! layers = ["status-bar", "task-var", "touch-paint"]
//! ```
//! `layers` が指定されていればそれをそのまま使う(継承しない)。
//! `layers` が無ければ `extends`(既定は `"default"`)のレイヤー一覧 + `add`。

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Deserialize)]
struct RawConfig {
    default: Vec<String>,
    #[serde(default)]
    scenes: HashMap<String, RawScene>,
}

#[derive(Deserialize)]
struct RawScene {
    extends: Option<String>,
    #[serde(default)]
    add: Vec<String>,
    layers: Option<Vec<String>>,
}

pub struct Config {
    /// シーン名(`"default"` 含む) → 解決済みレイヤー一覧。
    resolved: HashMap<String, Vec<String>>,
}

impl Config {
    /// `$FB_SERVER_CONFIG` > `$XDG_CONFIG_HOME/fb-server/scenes.toml` >
    /// `~/.config/fb-server/scenes.toml`。
    pub fn default_path() -> Result<PathBuf> {
        if let Ok(p) = std::env::var("FB_SERVER_CONFIG") {
            if !p.is_empty() {
                return Ok(PathBuf::from(p));
            }
        }
        if let Ok(d) = std::env::var("XDG_CONFIG_HOME") {
            if !d.is_empty() {
                return Ok(PathBuf::from(d).join("fb-server/scenes.toml"));
            }
        }
        let home = std::env::var("HOME").context("HOME が設定されていません")?;
        Ok(PathBuf::from(home).join(".config/fb-server/scenes.toml"))
    }

    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("設定ファイルを読み込めません: {}", path.display()))?;
        Self::parse(&text)
    }

    fn parse(text: &str) -> Result<Self> {
        let raw: RawConfig = toml::from_str(text).context("scenes.toml のパースに失敗")?;
        let mut resolved: HashMap<String, Vec<String>> = HashMap::new();
        resolved.insert("default".to_string(), raw.default.clone());
        for name in raw.scenes.keys() {
            resolve(name, &raw, &mut resolved, &mut Vec::new())?;
        }
        Ok(Config { resolved })
    }

    /// シーン名からレイヤー一覧を得る。未知のシーン名は `"default"` を返す。
    pub fn layers_for(&self, scene: &str) -> &[String] {
        self.resolved
            .get(scene)
            .or_else(|| self.resolved.get("default"))
            .map(|v| v.as_slice())
            .unwrap_or(&[])
    }

    /// `scenes.toml` に定義されたシーン名(`"default"` を除く)一覧。
    /// hello 名がこの中にあれば、そのシーンをアクティブにする。
    pub fn scene_names(&self) -> impl Iterator<Item = &str> {
        self.resolved.keys().filter(|k| k.as_str() != "default").map(|s| s.as_str())
    }
}

/// `name` のレイヤー一覧を再帰的に解決して `resolved` に詰める。
/// `stack` は循環 extends 検出用。
fn resolve(
    name: &str,
    raw: &RawConfig,
    resolved: &mut HashMap<String, Vec<String>>,
    stack: &mut Vec<String>,
) -> Result<Vec<String>> {
    if let Some(v) = resolved.get(name) {
        return Ok(v.clone());
    }
    if stack.contains(&name.to_string()) {
        bail!("scenes.toml: extends の循環参照 ({name})");
    }
    let Some(scene) = raw.scenes.get(name) else {
        bail!("scenes.toml: 未定義のシーン ({name})");
    };
    let layers = if let Some(layers) = &scene.layers {
        layers.clone()
    } else {
        let base_name = scene.extends.as_deref().unwrap_or("default");
        stack.push(name.to_string());
        let mut base = if base_name == "default" {
            raw.default.clone()
        } else {
            resolve(base_name, raw, resolved, stack)?
        };
        stack.pop();
        base.extend(scene.add.iter().cloned());
        base
    };
    resolved.insert(name.to_string(), layers.clone());
    Ok(layers)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
default = ["status-bar", "task-var", "touch-claude", "spotatui-pip"]

[scenes.ssbrowse]
extends = "default"
add = ["ssbrowse"]

[scenes.fbhalf]
extends = "default"
add = ["fbhalf"]

[scenes.dopagaki]
extends = "default"
add = ["dopagaki"]

[scenes.touch-paint]
layers = ["status-bar", "task-var", "touch-paint"]

[scenes.tmux-session]
layers = ["task-var", "tmux-session"]
"#;

    #[test]
    fn default_scene_returns_default_layers() {
        let cfg = Config::parse(SAMPLE).unwrap();
        assert_eq!(
            cfg.layers_for("default"),
            &["status-bar", "task-var", "touch-claude", "spotatui-pip"]
        );
    }

    #[test]
    fn extends_add_appends_to_default() {
        let cfg = Config::parse(SAMPLE).unwrap();
        assert_eq!(
            cfg.layers_for("ssbrowse"),
            &["status-bar", "task-var", "touch-claude", "spotatui-pip", "ssbrowse"]
        );
    }

    #[test]
    fn layers_overrides_without_inheriting() {
        let cfg = Config::parse(SAMPLE).unwrap();
        assert_eq!(cfg.layers_for("touch-paint"), &["status-bar", "task-var", "touch-paint"]);
        assert_eq!(cfg.layers_for("tmux-session"), &["task-var", "tmux-session"]);
    }

    #[test]
    fn unknown_scene_falls_back_to_default() {
        let cfg = Config::parse(SAMPLE).unwrap();
        assert_eq!(cfg.layers_for("no-such-scene"), cfg.layers_for("default"));
    }

    #[test]
    fn cyclic_extends_is_rejected() {
        let text = r#"
default = ["a"]

[scenes.x]
extends = "y"

[scenes.y]
extends = "x"
"#;
        assert!(Config::parse(text).is_err());
    }
}
