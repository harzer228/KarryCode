---
name: configure-models
description: Choose KarryCode providers and models through TOML while keeping authentication in the profile.
---

Use `/config` to find the profile and project model files. For project defaults write `.karrycode/models.toml` with `version = 1`, `provider` and `model`. Add optional limits and `reasoning` supported by that profile model. Preview the exact file and tell the user to approve it in `/config`.

Profile settings use a `providers` table, `models` entries and an optional `default` provider/model pair. Keep tokens within the model context and request data only from public metadata or a user-approved test of the chosen API. API keys belong in `/provider`. Report which limits or capabilities remain unverified.
