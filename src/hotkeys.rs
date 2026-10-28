use crate::config::Config;
use anyhow::{ensure, Context, Result};
use global_hotkey::{hotkey::HotKey, GlobalHotKeyManager};
use std::str::FromStr;

#[derive(Clone, Copy)]
pub struct Hotkeys {
    pub rewrite: HotKey,
    pub translate: HotKey,
}
fn parse(text: &str, label: &str) -> Result<HotKey> {
    let key =
        HotKey::from_str(text).with_context(|| format!("{label}格式不正确，示例：Alt+KeyD"))?;
    ensure!(
        !key.mods.is_empty(),
        "{label}至少需要一个修饰键，例如 Alt 或 Control"
    );
    Ok(key)
}
impl Hotkeys {
    pub fn from_config(config: &Config) -> Result<Self> {
        let keys = Self {
            rewrite: parse(&config.hotkey, "英文润色快捷键")?,
            translate: parse(&config.translation_hotkey, "中文翻译快捷键")?,
        };
        ensure!(
            keys.rewrite != keys.translate,
            "英文润色和中文翻译不能使用相同快捷键"
        );
        Ok(keys)
    }
    pub fn keys(self) -> [HotKey; 2] {
        [self.rewrite, self.translate]
    }
    pub fn additions(self, old: Option<Self>) -> Vec<HotKey> {
        self.keys()
            .into_iter()
            .filter(|key| old.is_none_or(|old| !old.keys().contains(key)))
            .collect()
    }
    pub fn removals(self, new: Self) -> Vec<HotKey> {
        self.keys()
            .into_iter()
            .filter(|key| !new.keys().contains(key))
            .collect()
    }
    /// Register only newly needed combinations. Existing combinations are kept,
    /// including when the user swaps the two actions' shortcuts.
    pub fn prepare(self, manager: &GlobalHotKeyManager, old: Option<Self>) -> Result<Vec<HotKey>> {
        let mut added = Vec::new();
        for key in self.additions(old) {
            if let Err(error) = manager.register(key) {
                for registered in &added {
                    let _ = manager.unregister(*registered);
                }
                return Err(error).context("快捷键已被占用或无法注册，请更换组合");
            }
            added.push(key);
        }
        Ok(added)
    }
    pub fn unregister(self, manager: &GlobalHotKeyManager) -> Result<()> {
        let mut removed = Vec::new();
        for key in self.keys() {
            if let Err(error) = manager.unregister(key) {
                for previous in removed {
                    let _ = manager.register(previous);
                }
                return Err(error.into());
            }
            removed.push(key);
        }
        Ok(())
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn conflicts_and_swaps() {
        let mut config = Config::default();
        let old = Hotkeys::from_config(&config).unwrap();
        assert_eq!(old.additions(None).len(), 2);
        config.translation_hotkey = config.hotkey.clone();
        assert!(Hotkeys::from_config(&config).is_err());
        config.hotkey = "Alt+KeyD".into();
        config.translation_hotkey = "Alt+KeyF".into();
        let swapped = Hotkeys::from_config(&config).unwrap();
        assert!(swapped.additions(Some(old)).is_empty());
        assert!(old.removals(swapped).is_empty());
        config.translation_hotkey = "KeyD".into();
        assert!(Hotkeys::from_config(&config).is_err());
    }
}
