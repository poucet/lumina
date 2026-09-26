//! /model — model listing and management.

use lumina_macros::command_group;
use serenity::all::{
    AutocompleteChoice, CommandInteraction, CreateAutocompleteResponse,
    CreateInteractionResponse, CreateInteractionResponseMessage,
};

use super::LuminaContext;

#[command_group(description = "LLM model management")]
#[allow(clippy::module_inception)] // command_group names the slash command after the module
mod model {
    use super::*;

    #[sub_command(description = "List available models")]
    pub async fn list(
        lx: &LuminaContext,
        cmd: &CommandInteraction,
        #[describe("Filter by provider name")] provider: Option<String>,
    ) -> anyhow::Result<()> {
        let models = lx.daemon.model().list_models().await?;

        let filtered: Vec<_> = match &provider {
            Some(p) => {
                let p_lower = p.to_lowercase();
                models.iter().filter(|m| m.id.provider.to_lowercase().contains(&p_lower)).collect()
            }
            None => models.iter().collect(),
        };

        if filtered.is_empty() {
            let msg = match &provider {
                Some(p) => format!("No models found for provider `{p}`"),
                None => "No models available".to_string(),
            };
            return reply_ephemeral(lx, cmd, &msg).await;
        }

        // Group by provider
        let mut by_provider: std::collections::BTreeMap<&str, Vec<String>> =
            std::collections::BTreeMap::new();
        for m in &filtered {
            by_provider
                .entry(&m.id.provider)
                .or_default()
                .push(format!(
                    "`{}` — {}",
                    m.id,
                    m.definition.display_name.as_deref().unwrap_or(&m.id.model)
                ));
        }

        let mut lines = Vec::new();
        for (provider, models) in &by_provider {
            lines.push(format!("**{provider}** ({} models)", models.len()));
            for model in models {
                lines.push(format!("  {model}"));
            }
        }

        let text = lines.join("\n");
        // Leave room for "Page x/y" footer
        let pages = crate::paginator::paginate_text(&text, 1900);
        crate::paginator::send_paginated(lx, cmd, &pages, std::time::Duration::from_secs(120)).await
    }

    #[sub_command(description = "Show the model that will be used here, and where it came from")]
    pub async fn current(
        lx: &LuminaContext,
        cmd: &CommandInteraction,
    ) -> anyhow::Result<()> {
        let channel_id = cmd.channel_id;
        let channel_model = crate::commands::chat::get_topic_tag(lx, channel_id, "model");
        let config_default = lx.config.discord.model_id.as_deref().filter(|s| !s.is_empty());
        let daemon_default = lx.daemon.model().default_model_id().await;

        let (source, model) = match (&channel_model, config_default) {
            (Some(m), _) => ("channel topic tag", m.as_str()),
            (None, Some(m)) => ("lumina.toml config default", m),
            (None, None) if !daemon_default.is_empty() => ("daemon default", daemon_default.as_str()),
            (None, None) => ("daemon default", "(not set)"),
        };

        reply_ephemeral(
            lx,
            cmd,
            &format!(
                "Current model: `{model}` (source: {source})\n\
                 Daemon default: `{daemon}`",
                daemon = if daemon_default.is_empty() { "(not set)" } else { daemon_default.as_str() },
            ),
        ).await
    }

    #[sub_command(description = "List available providers")]
    pub async fn providers(
        lx: &LuminaContext,
        cmd: &CommandInteraction,
    ) -> anyhow::Result<()> {
        let providers = lx.daemon.model().list_providers().await;
        if providers.is_empty() {
            return reply_ephemeral(lx, cmd, "No providers configured").await;
        }

        let lines: Vec<String> = providers
            .iter()
            .map(|p| {
                let env = p.api_key_env.as_deref().unwrap_or("n/a");
                format!("`{}` — env: `{}`", p.name, env)
            })
            .collect();

        reply_ephemeral(lx, cmd, &lines.join("\n")).await
    }

    #[sub_command(description = "Set the daemon's default model (affects all Lumina sessions unless a channel overrides)")]
    pub async fn set(
        lx: &LuminaContext,
        cmd: &CommandInteraction,
        #[describe("Model ID (e.g. gemini/models/gemini-2.5-flash)")] #[autocomplete] id: String,
    ) -> anyhow::Result<()> {
        if id.is_empty() {
            return reply_ephemeral(lx, cmd, "No model id provided.").await;
        }

        // Validate the model exists so we fail early with a clear message
        // instead of deferring the error to the next chat turn.
        let models = lx.daemon.model().list_models().await.unwrap_or_default();
        if !models.iter().any(|m| m.id.to_string() == id) {
            return reply_ephemeral(lx, cmd, &format!("Unknown model: `{id}` — use `/model list` to see what's available.")).await;
        }

        lx.daemon.model().set_default_model(&id).await?;
        reply_ephemeral(lx, cmd, &format!("Default model set to `{id}`.")).await
    }

    /// Autocomplete for `/model set id` — suggests matching model ids.
    pub async fn autocomplete(lx: &LuminaContext, cmd: &CommandInteraction) -> anyhow::Result<()> {
        let partial = cmd.data.autocomplete()
            .map(|o| o.value.to_string())
            .unwrap_or_default();
        let partial_lower = partial.to_lowercase();

        let models = lx.daemon.model().list_models().await.unwrap_or_default();
        let choices: Vec<AutocompleteChoice> = models
            .into_iter()
            .filter(|m| {
                if partial_lower.is_empty() { return true; }
                let full_id = m.id.to_string().to_lowercase();
                let display = m.definition.display_name.as_deref().unwrap_or("").to_lowercase();
                full_id.contains(&partial_lower) || display.contains(&partial_lower)
            })
            .take(25)
            .map(|m| {
                let full_id = m.id.to_string();
                let label = match &m.definition.display_name {
                    Some(name) => format!("{name}  ({full_id})"),
                    None => full_id.clone(),
                };
                let label = if label.len() > 100 { label[..100].to_string() } else { label };
                AutocompleteChoice::new(label, full_id)
            })
            .collect();

        cmd.create_response(&lx.http, CreateInteractionResponse::Autocomplete(
            CreateAutocompleteResponse::new().set_choices(choices),
        )).await?;
        Ok(())
    }

    async fn reply_ephemeral(
        lx: &LuminaContext,
        cmd: &CommandInteraction,
        content: &str,
    ) -> anyhow::Result<()> {
        cmd.create_response(
            &lx.http,
            CreateInteractionResponse::Message(
                CreateInteractionResponseMessage::new()
                    .content(content)
                    .ephemeral(true),
            ),
        )
        .await?;
        Ok(())
    }
}
