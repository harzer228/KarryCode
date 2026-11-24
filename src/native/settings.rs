use super::{
    State,
    subagents::MAX_SUBAGENTS,
    ui::{Screen, theme::Theme},
};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::Path};

#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub(super) struct Settings {
    pub desktop: bool,
    subagents: u8,
    pub provider_limits: BTreeMap<u32, super::subagents::Limit>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            desktop: true,
            subagents: 2,
            provider_limits: BTreeMap::new(),
        }
    }
}

impl Settings {
    pub fn delegate_limit(&self, connection: u32) -> u8 {
        self.subagents.min(MAX_SUBAGENTS).min(
            self.provider_limits
                .get(&connection)
                .map_or(MAX_SUBAGENTS, |limit| limit.count.clamp(1, MAX_SUBAGENTS)),
        )
    }
}

pub(super) fn menu(
    screen: &mut Screen,
    state: &mut State,
    theme: &mut Theme,
    work: &Path,
    root: &Path,
    desktop_paused: bool,
) -> Result<bool> {
    let mut changed = false;
    let mut resume_desktop = false;
    loop {
        let connection = state
            .active_provider(super::DialogMode::Agent)
            .map(|provider| provider.connection.id);
        let limit = connection.and_then(|id| state.settings.provider_limits.get(&id));
        let saved_limit = match limit {
            Some(limit) if limit.estimated => trf!("{} (оценка)", "{} (estimate)", limit.count),
            Some(limit) => limit.count.to_string(),
            None => tr!("неизвестен", "unknown").into(),
        };
        let desktop = if !cfg!(windows) {
            tr!("только Windows", "Windows only")
        } else if !state.settings.desktop {
            tr!("выкл.", "off")
        } else if desktop_paused && !resume_desktop {
            tr!("остановлено · возобновить", "paused · resume")
        } else {
            tr!("вкл.", "on")
        };
        let options = [
            trf!("Язык · {}", "Language · {}", tr!("Русский", "English")),
            tr!("Тема агента", "Agent theme").into(),
            trf!("Рабочий стол · {desktop}", "Desktop control · {desktop}"),
            trf!(
                "Саб-агенты · {} одновременно",
                "Subagents · {} concurrent",
                state.settings.subagents.min(MAX_SUBAGENTS)
            ),
            trf!(
                "Предел подключения · {saved_limit} · сбросить",
                "Connection limit · {saved_limit} · reset"
            ),
        ];
        match screen.choose(tr!("Настройки агента", "Agent settings"), &options)? {
            Some(0) => {
                if let Some(choice) = screen.choose(
                    tr!("Язык интерфейса", "Interface language"),
                    &["Русский".into(), "English".into()],
                )? {
                    crate::language::save(
                        root,
                        [crate::language::Language::Ru, crate::language::Language::En][choice],
                    )?;
                }
            }
            Some(1) => {
                if let Some(next) = screen.choose_theme(*theme)? {
                    next.save(work)?;
                    *theme = next;
                    screen.set_theme(next);
                    changed = true;
                }
            }
            Some(2) if cfg!(windows) => {
                if state.settings.desktop && desktop_paused && !resume_desktop {
                    resume_desktop = true;
                } else {
                    state.settings.desktop = !state.settings.desktop;
                    resume_desktop = state.settings.desktop;
                    changed = true;
                }
            }
            Some(3) => {
                if let Some(value) = screen.ask_text(
                    tr!(
                        "Саб-агенты одновременно (0–30)",
                        "Concurrent subagents (0–30)"
                    ),
                    false,
                )? {
                    match value.trim().parse::<u8>() {
                        Ok(count) if count <= MAX_SUBAGENTS => {
                            state.settings.subagents = count;
                            changed = true;
                        }
                        _ => screen.report(
                            tr!("Неверный лимит", "Invalid limit"),
                            tr!(
                                "Введи целое число от 0 до 30.",
                                "Enter a whole number from 0 to 30."
                            ),
                        )?,
                    }
                }
            }
            Some(4) => {
                if let Some(connection) = connection {
                    changed |= state.settings.provider_limits.remove(&connection).is_some();
                }
            }
            Some(_) => {}
            None => {
                if changed {
                    super::save(work, state)?;
                }
                return Ok(resume_desktop);
            }
        }
    }
}
