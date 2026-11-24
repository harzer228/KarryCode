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
    connection_limits: BTreeMap<u32, u8>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            desktop: true,
            subagents: 2,
            provider_limits: BTreeMap::new(),
            connection_limits: BTreeMap::new(),
        }
    }
}

impl Settings {
    pub fn delegate_limit(&self, connection: u32) -> u8 {
        let manual = self.manual_limit(connection).unwrap_or(MAX_SUBAGENTS);
        let observed = self
            .provider_limits
            .get(&connection)
            .map_or(MAX_SUBAGENTS, |limit| limit.count.clamp(1, MAX_SUBAGENTS));
        self.subagents.min(manual).min(observed)
    }

    fn manual_limit(&self, connection: u32) -> Option<u8> {
        self.connection_limits
            .get(&connection)
            .map(|count| (*count).clamp(1, MAX_SUBAGENTS))
    }

    pub(super) fn remove_connection(&mut self, connection: u32) {
        self.connection_limits.remove(&connection);
        self.provider_limits.remove(&connection);
    }

    fn policy_label(&self, connection: u32) -> String {
        self.manual_limit(connection)
            .map_or_else(|| tr!("Авто", "Auto").into(), |count| count.to_string())
    }

    fn observed_label(&self, connection: u32) -> String {
        match self.provider_limits.get(&connection) {
            Some(limit) if limit.estimated => trf!("{} (оценка)", "{} (estimate)", limit.count),
            Some(limit) => limit.count.to_string(),
            None => tr!("предел не выявлен", "no cap observed").into(),
        }
    }

    fn edit_connection_limit(&mut self, screen: &mut Screen, connection: u32) -> Result<bool> {
        let policy = self.policy_label(connection);
        let effective = self.delegate_limit(connection);
        let observed = self.observed_label(connection);
        let title = trf!(
            "Параллельные запросы · {policy} · фактически {effective} · {observed}",
            "Parallel requests · {policy} · effective {effective} · {observed}"
        );
        let choices = [
            tr!("Авто", "Auto").into(),
            tr!("Задать лимит", "Set a limit").into(),
            tr!(
                "Сбросить оценку провайдера",
                "Reset the observed provider cap"
            )
            .into(),
        ];
        match screen.choose(&title, &choices)? {
            Some(0) => Ok(self.connection_limits.remove(&connection).is_some()),
            Some(1) => {
                let Some(value) = screen.ask_text(
                    tr!("Параллельные запросы (1–30)", "Parallel requests (1–30)"),
                    false,
                )?
                else {
                    return Ok(false);
                };
                if let Ok(count @ 1..=MAX_SUBAGENTS) = value.trim().parse::<u8>() {
                    return Ok(self.connection_limits.insert(connection, count) != Some(count));
                }
                screen.report(
                    tr!("Неверный лимит", "Invalid limit"),
                    tr!(
                        "Введи целое число от 1 до 30.",
                        "Enter a whole number from 1 to 30."
                    ),
                )?;
                Ok(false)
            }
            Some(2) => Ok(self.provider_limits.remove(&connection).is_some()),
            _ => Ok(false),
        }
    }
}

pub(super) fn menu(
    screen: &mut Screen,
    state: &mut State,
    theme: &mut Theme,
    work: &Path,
    root: &Path,
    desktop_paused: bool,
    mut redraw_language: impl FnMut(&mut Screen, &State) -> Result<()>,
) -> Result<bool> {
    let mut changed = false;
    let mut resume_desktop = false;
    loop {
        let connection = state
            .active_provider(super::DialogMode::Agent)
            .map(|provider| provider.connection.id);
        let parallel = match connection {
            Some(id) => {
                let policy = state.settings.policy_label(id);
                let effective = state.settings.delegate_limit(id);
                trf!(
                    "Параллельные запросы · {policy} · фактически {effective}",
                    "Parallel requests · {policy} · effective {effective}"
                )
            }
            None => tr!(
                "Параллельные запросы · выбери подключение",
                "Parallel requests · choose a connection"
            )
            .into(),
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
            parallel,
        ];
        match screen.choose(tr!("Настройки агента", "Agent settings"), &options)? {
            Some(0) => {
                if let Some(choice) = screen.choose(
                    tr!("Язык интерфейса", "Interface language"),
                    &["Русский".into(), "English".into()],
                )? && crate::language::english() != (choice == 1)
                {
                    crate::language::save(
                        root,
                        [crate::language::Language::Ru, crate::language::Language::En][choice],
                    )?;
                    redraw_language(screen, state)?;
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
                let selected = match connection {
                    Some(id) => Some(id),
                    None if state.connections.is_empty() => {
                        screen.report(
                            tr!("Параллельные запросы", "Parallel requests"),
                            tr!(
                                "Сначала добавь подключение через /provider.",
                                "Add a connection through /provider first."
                            ),
                        )?;
                        None
                    }
                    None => super::models::choose_connection(screen, state)?,
                };
                if let Some(id) = selected {
                    changed |= state.settings.edit_connection_limit(screen, id)?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native::subagents::Limit;

    #[test]
    fn old_settings_use_the_global_limit_and_keep_observations() -> Result<()> {
        let settings: Settings = serde_json::from_str(
            r#"{"subagents":24,"provider_limits":{"8":{"count":3,"estimated":true}}}"#,
        )?;
        assert_eq!(settings.manual_limit(8), None);
        assert_eq!(settings.delegate_limit(8), 3);
        assert_eq!(settings.delegate_limit(9), 24);
        Ok(())
    }

    #[test]
    fn manual_limits_are_persisted_for_each_connection_and_capped_by_observations() -> Result<()> {
        let mut settings = Settings {
            subagents: 24,
            connection_limits: BTreeMap::from([(8, 8), (9, 6)]),
            provider_limits: BTreeMap::from([(
                8,
                Limit {
                    count: 3,
                    estimated: true,
                },
            )]),
            ..Settings::default()
        };
        settings = serde_json::from_slice(&serde_json::to_vec(&settings)?)?;
        assert_eq!(settings.delegate_limit(8), 3);
        assert_eq!(settings.delegate_limit(9), 6);
        settings.provider_limits.remove(&8);
        assert_eq!(settings.delegate_limit(8), 8);
        settings.remove_connection(8);
        assert_eq!(settings.delegate_limit(8), 24);
        assert_eq!(settings.delegate_limit(9), 6);
        settings.subagents = 0;
        assert_eq!(settings.delegate_limit(9), 0);
        Ok(())
    }
}
