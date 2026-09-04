//! Read-only Prime provider membership and global default reference.

use crate::error::AppError;
use crate::prime_config::{read_prime_native_defaults, read_prime_native_providers};
use crate::store::AppState;
use serde::Serialize;

const PRIME_APP: &str = "prime";

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PrimeCurrentState {
    pub enabled_provider_ids: Vec<String>,
    pub default_provider_id: Option<String>,
}

pub(crate) struct PrimeStateService;

impl PrimeStateService {
    pub(crate) fn current(state: &AppState) -> Result<PrimeCurrentState, AppError> {
        let _guard =
            futures::executor::block_on(state.proxy_service.lock_switch_for_app(PRIME_APP));
        let native = read_prime_native_providers()?;
        let enabled_provider_ids = native.keys().cloned().collect::<Vec<_>>();
        let default_provider_id = match read_prime_native_defaults() {
            Ok(defaults) => defaults.default_provider,
            Err(error) => {
                log::warn!("Failed to read Prime global default provider for advisory UI: {error}");
                None
            }
        };
        Ok(PrimeCurrentState {
            enabled_provider_ids,
            default_provider_id,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::Database;
    use crate::prime_config::test_support::TestAgentDir;
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
        let models_path = crate::prime_config::get_prime_models_path().expect("models path");
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
        let settings_path = crate::prime_config::get_prime_settings_path().expect("settings path");
        fs::write(
            settings_path,
            r#"{"defaultProvider":"cc-switch-managed","defaultModel":"model-a"}"#,
        )
        .expect("write settings");

        let current = PrimeStateService::current(&state).expect("read state");
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
    }

    #[test]
    #[serial]
    fn invalid_global_settings_do_not_hide_provider_membership() {
        let _agent = TestAgentDir::new();
        let state = AppState::new(Arc::new(
            Database::memory().expect("create in-memory database"),
        ));
        let models_path = crate::prime_config::get_prime_models_path().expect("models path");
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
        let settings_path = crate::prime_config::get_prime_settings_path().expect("settings path");
        fs::write(settings_path, "[]").expect("write invalid settings");

        let current = PrimeStateService::current(&state).expect("read membership");
        assert_eq!(
            current.enabled_provider_ids,
            vec!["cc-switch-managed".to_string()]
        );
        assert_eq!(current.default_provider_id, None);
    }
}
