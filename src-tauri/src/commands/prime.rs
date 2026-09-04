use crate::provider::UsageScript;
use crate::services::prime_state::{PrimeCurrentState, PrimeStateService};
use crate::services::ProviderService;
use crate::store::AppState;
use tauri::State;

#[tauri::command]
pub(crate) fn get_prime_current_state(
    state: State<'_, AppState>,
) -> Result<PrimeCurrentState, String> {
    PrimeStateService::current(state.inner()).map_err(|error| error.to_string())
}

#[tauri::command]
pub(crate) fn update_prime_provider_usage_script(
    state: State<'_, AppState>,
    id: String,
    #[allow(non_snake_case)] usageScript: UsageScript,
) -> Result<bool, String> {
    ProviderService::update_prime_usage_script(state.inner(), &id, usageScript)
        .map_err(|error| error.to_string())
}
