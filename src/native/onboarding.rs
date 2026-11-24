use super::{
    State,
    models::{self, Connection, ModelSettings, Protocol},
    ui::Screen,
};
use anyhow::Result;

pub(super) struct PendingSetup {
    connection: Connection,
    model: ModelSettings,
}

impl PendingSetup {
    pub(super) fn apply(self, state: &mut State) {
        let id = self.connection.id;
        state.connections.push(self.connection);
        models::add_models(state, id, vec![self.model]);
    }
}

pub(super) fn run(screen: &mut Screen, state: &State) -> Result<Option<PendingSetup>> {
    let kinds = [
        "OpenAI".into(),
        "Anthropic".into(),
        tr!("Свой провайдер", "Custom provider").into(),
        tr!(
            "Локальный сервер · Ollama или совместимый",
            "Local server · Ollama or compatible"
        )
        .into(),
        tr!("Пропустить настройку", "Skip setup").into(),
    ];
    let (name, url, protocol) = match screen.choose(
        tr!(
            "1/3 · Первый запуск · Провайдер",
            "1/3 · First launch · Provider"
        ),
        &kinds,
    )? {
        Some(0) => (
            "OpenAI",
            "https://api.openai.com/v1",
            Protocol::ChatCompletions,
        ),
        Some(1) => (
            "Anthropic",
            "https://api.anthropic.com/v1",
            Protocol::AnthropicMessages,
        ),
        Some(2) => (
            tr!("Свой провайдер", "Custom provider"),
            "",
            Protocol::ChatCompletions,
        ),
        Some(3) => (
            tr!("Локальный сервер", "Local server"),
            "http://127.0.0.1:11434/v1",
            Protocol::ChatCompletions,
        ),
        _ => return Ok(None),
    };
    let preset = Connection {
        id: state.next_connection_id()?,
        name: name.into(),
        url: url.into(),
        protocol,
        key: String::new(),
    };
    let Some(connection) = screen.setup_connection(&preset)? else {
        return Ok(None);
    };
    let Some(model) = models::setup_model(screen, &connection)? else {
        return Ok(None);
    };
    let summary = trf!(
        "{} · {}\nМодель: {}\nAPI-ключ: {}",
        "{} · {}\nModel: {}\nAPI key: {}",
        connection.name,
        connection.url,
        model.model,
        if connection.key.is_empty() {
            tr!("без ключа", "none")
        } else {
            "••••"
        }
    );
    if screen.confirm(tr!("Сохранить настройку?", "Save your setup?"), &summary)?
    {
        Ok(Some(PendingSetup { connection, model }))
    } else {
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native::DialogMode;

    #[test]
    fn confirmed_local_setup_can_be_saved_and_used_as_the_default_model() -> Result<()> {
        let mut state = State::default();
        PendingSetup {
            connection: Connection {
                id: state.next_connection_id()?,
                name: "Local".into(),
                url: "http://127.0.0.1:11434/v1".into(),
                protocol: Protocol::ChatCompletions,
                key: String::new(),
            },
            model: ModelSettings {
                model: "local-fixture".into(),
                context: 8192,
                input: 8192,
                output: 1024,
                ..ModelSettings::default()
            },
        }
        .apply(&mut state);
        let restored = models::load_state(serde_json::to_vec(&state)?.as_slice())?;
        let active = restored.active_provider(DialogMode::Agent).unwrap();
        assert_eq!(active.model, "local-fixture");
        assert_eq!(active.connection.url, "http://127.0.0.1:11434/v1");
        assert!(active.connection.key.is_empty());
        Ok(())
    }
}
