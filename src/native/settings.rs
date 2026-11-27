use super::{
    State,
    subagents::MAX_SUBAGENTS,
    ui::{Screen, theme::Theme},
};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};

#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub(super) struct Settings {
    pub desktop: bool,
    subagents: u8,
    pub provider_limits: BTreeMap<u32, super::subagents::Limit>,
    connection_limits: BTreeMap<u32, u8>,
    pub(super) model_aliases: BTreeMap<String, u32>,
    pub(super) needs_model_key: BTreeSet<u32>,
    pub(super) approved_model_projects: BTreeMap<String, super::model_config::ProjectApproval>,
    pub(super) extensions: Vec<super::extensions::Grant>,
    pub(super) appearance: Option<String>,
    pub(super) custom_locale: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            desktop: true,
            subagents: 2,
            provider_limits: BTreeMap::new(),
            connection_limits: BTreeMap::new(),
            model_aliases: BTreeMap::new(),
            needs_model_key: BTreeSet::new(),
            approved_model_projects: BTreeMap::new(),
            extensions: Vec::new(),
            appearance: None,
            custom_locale: true,
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
        self.needs_model_key.remove(&connection);
        self.model_aliases.retain(|_, id| *id != connection);
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

pub(super) struct MenuContext<'a> {
    pub work: &'a Path,
    pub root: &'a Path,
    pub packs: Option<&'a [(String, super::appearance::Appearance)]>,
    pub connection: Option<u32>,
}

pub(super) fn appearance_menu(
    screen: &mut Screen,
    settings: &mut Settings,
    theme: &mut Theme,
    work: &Path,
    packs: Option<&[(String, super::appearance::Appearance)]>,
) -> Result<bool> {
    let choice = match packs {
        Some(packs) => super::appearance::choose(screen, packs, settings.appearance.as_deref())?,
        None => Some(None),
    };
    match choice {
        None => Ok(false),
        Some(Some(id)) => {
            let pack = packs
                .unwrap()
                .iter()
                .find(|(name, _)| name == &id)
                .map(|(_, pack)| pack.clone());
            screen.set_appearance(pack, true)?;
            settings.appearance = Some(id);
            settings.custom_locale = true;
            Ok(true)
        }
        Some(None) => {
            let Some(selected) = screen.choose_theme(*theme)? else {
                return Ok(false);
            };
            selected.save(work)?;
            *theme = selected;
            settings.appearance = None;
            screen.set_appearance(None, false)?;
            screen.set_theme(selected);
            Ok(true)
        }
    }
}

pub(super) fn menu(
    screen: &mut Screen,
    state: &mut State,
    theme: &mut Theme,
    context: MenuContext<'_>,
    desktop_paused: bool,
    mut redraw_language: impl FnMut(&mut Screen, &State) -> Result<()>,
) -> Result<bool> {
    let mut changed = false;
    let mut resume_desktop = false;
    loop {
        let connection = context.connection;
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
            tr!(
                "Оформление и расширения языка",
                "Appearance and language packs"
            )
            .into(),
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
                let mut choices = vec!["Русский".into(), "English".into()];
                let selected = context
                    .packs
                    .and_then(|packs| {
                        packs
                            .iter()
                            .find(|(id, _)| Some(id) == state.settings.appearance.as_ref())
                    })
                    .map(|(_, pack)| pack);
                if selected.is_some_and(super::appearance::Appearance::has_locale) {
                    choices.push(tr!("Язык пакета", "Pack language").into());
                }
                if let Some(choice) =
                    screen.choose(tr!("Язык интерфейса", "Interface language"), &choices)?
                {
                    if choice < 2 {
                        crate::language::save(
                            context.root,
                            [crate::language::Language::Ru, crate::language::Language::En][choice],
                        )?;
                    }
                    state.settings.custom_locale = choice == 2;
                    screen.set_appearance(selected.cloned(), state.settings.custom_locale)?;
                    changed = true;
                    redraw_language(screen, state)?;
                }
            }
            Some(1) => {
                if appearance_menu(
                    screen,
                    &mut state.settings,
                    theme,
                    context.work,
                    context.packs,
                )? {
                    changed = true;
                    redraw_language(screen, state)?;
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
                    super::save(context.work, state)?;
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
