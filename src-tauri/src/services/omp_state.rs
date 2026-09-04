//! Read-only OMP provider membership and global default reference.

use crate::error::AppError;
use crate::omp_config::{read_omp_default_role, read_omp_native_providers};
use crate::store::AppState;
use serde::Serialize;

const OMP_APP: &str = "omp";

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct OmpCurrentState {
    pub enabled_provider_ids: Vec<String>,
    pub default_provider_id: Option<String>,
    pub default_model: Option<String>,
}

pub(crate) struct OmpStateService;

impl OmpStateService {
    pub(crate) fn current(state: &AppState) -> Result<OmpCurrentState, AppError> {
        let _guard = futures::executor::block_on(state.proxy_service.lock_switch_for_app(OMP_APP));
        let native = read_omp_native_providers()?;
        let enabled_provider_ids = native.keys().cloned().collect::<Vec<_>>();
        let (default_provider_id, default_model) = match read_omp_default_role() {
            Ok(role) => role.map(|role| split_default_role(&role)).unwrap_or((None, None)),
            Err(error) => {
                log::warn!("Failed to read OMP global default role for advisory UI: {error}");
                (None, None)
            }
        };
        Ok(OmpCurrentState {
            enabled_provider_ids,
            default_provider_id,
            default_model,
        })
    }
}

/// Split an OMP default role (`<provider>/<model>[:thinking]`) at the first
/// `/`: the provider part becomes the default provider id, the remainder
/// (thinking tail included) becomes the default model.
fn split_default_role(role: &str) -> (Option<String>, Option<String>) {
    let Some((provider, model)) = role.split_once('/') else {
        return (None, None);
    };
    if provider.is_empty() {
        return (None, None);
    }
    let model = (!model.is_empty()).then(|| model.to_string());
    (Some(provider.to_string()), model)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::Database;
    use crate::omp_config::test_support::TestAgentDir;
    use serial_test::serial;
    use std::fs;
    use std::sync::Arc;

    #[test]
    #[serial]
    fn state_exposes_every_explicit_provider_node() {
        let _agent = TestAgentDir::new();
        let state = AppState::new(Arc::new(
            Database::memory().expect("create in-memory database"),
        ));
        let models_path = crate::omp_config::get_omp_models_path().expect("models path");
        fs::create_dir_all(models_path.parent().expect("models directory"))
            .expect("create models directory");
        fs::write(
            models_path,
            r#"{
                "providers": {
                    "cc-switch-managed": {
                        "name": "Managed",
                        "baseUrl": "https://api.example.com/v1",
                        "api": "openai-completions",
                        "models": [{ "id": "model-a" }]
                    },
                    "native-oauth": {
                        "oauth": "example",
                        "baseUrl": "https://api.example.com/v1",
                        "api": "openai-completions",
                        "models": [{ "id": "model-b" }]
                    },
                    "anthropic": {},
                    "unsupported": {
                        "futureField": true
                    }
                }
            }"#,
        )
        .expect("write models");
        let config_path = crate::omp_config::get_omp_config_path().expect("config path");
        fs::write(
            config_path,
            "modelRoles:\n  default: cc-switch-managed/model-a:xhigh\n",
        )
        .expect("write config");

        let current = OmpStateService::current(&state).expect("read state");
        assert_eq!(
            current.enabled_provider_ids,
            vec![
                "cc-switch-managed".to_string(),
                "native-oauth".to_string(),
                "anthropic".to_string(),
                "unsupported".to_string(),
            ]
        );
        assert_eq!(
            current.default_provider_id.as_deref(),
            Some("cc-switch-managed")
        );
        assert_eq!(current.default_model.as_deref(), Some("model-a:xhigh"));
    }

    #[test]
    #[serial]
    fn invalid_global_config_does_not_hide_provider_membership() {
        let _agent = TestAgentDir::new();
        let state = AppState::new(Arc::new(
            Database::memory().expect("create in-memory database"),
        ));
        let models_path = crate::omp_config::get_omp_models_path().expect("models path");
        fs::create_dir_all(models_path.parent().expect("models directory"))
            .expect("create models directory");
        fs::write(
            models_path,
            r#"{
                "providers": {
                    "cc-switch-managed": {
                        "name": "Managed",
                        "baseUrl": "https://api.example.com/v1",
                        "api": "openai-completions",
                        "models": [{ "id": "model-a" }]
                    }
                }
            }"#,
        )
        .expect("write models");
        let config_path = crate::omp_config::get_omp_config_path().expect("config path");
        fs::write(config_path, "{not-yaml: [unclosed").expect("write invalid config");

        let current = OmpStateService::current(&state).expect("read membership");
        assert_eq!(
            current.enabled_provider_ids,
            vec!["cc-switch-managed".to_string()]
        );
        assert_eq!(current.default_provider_id, None);
        assert_eq!(current.default_model, None);
    }

    #[test]
    fn role_without_slash_selects_no_default() {
        assert_eq!(split_default_role("bare-model"), (None, None));
        assert_eq!(split_default_role("/model"), (None, None));
        assert_eq!(
            split_default_role("prov/model:xhigh"),
            (
                Some("prov".to_string()),
                Some("model:xhigh".to_string())
            )
        );
    }
}
