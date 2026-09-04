//! Thin adapter for OMP's native files.
//!
//! OMP owns account login in its sqlite vault and the default model in
//! `<agent-dir>/config.yml` (`modelRoles.default`). CC Switch only manages
//! explicit provider entries in `models.json` plus that default role.

use crate::config::{atomic_write_private, get_home_dir};
use crate::error::AppError;
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex, MutexGuard};

const MAX_OMP_FILE_BYTES: u64 = 1024 * 1024;
const MISSING_MODELS_REVISION: &str = "missing";
static MODELS_FILE_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));
#[cfg(test)]
static TEST_AGENT_DIR: LazyLock<Mutex<Option<PathBuf>>> = LazyLock::new(|| Mutex::new(None));

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub(crate) struct OmpNativeDefaults {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_provider: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_dir: Option<String>,
}

pub(crate) fn get_omp_agent_dir() -> Result<PathBuf, AppError> {
    #[cfg(test)]
    if let Some(path) = TEST_AGENT_DIR
        .lock()
        .expect("lock OMP test directory")
        .clone()
    {
        return resolve_omp_agent_dir(
            Some(path),
            None,
            None,
            get_home_dir().join(".omp").join("agent"),
        );
    }

    let profile = std::env::var("OMP_PROFILE").ok().filter(|value| {
        let trimmed = value.trim();
        !trimmed.is_empty() && *trimmed != "default"
    });
    resolve_omp_agent_dir(
        crate::settings::get_omp_override_dir(),
        profile,
        std::env::var_os("PI_CODING_AGENT_DIR"),
        get_home_dir().join(".omp").join("agent"),
    )
}

fn resolve_omp_agent_dir(
    settings_override: Option<PathBuf>,
    omp_profile: Option<String>,
    pi_env_override: Option<std::ffi::OsString>,
    default_path: PathBuf,
) -> Result<PathBuf, AppError> {
    if let Some(path) = settings_override {
        return require_absolute(path, "OMP settings override");
    }
    if let Some(name) = omp_profile
        .map(|name| name.trim().to_string())
        .filter(|name| !name.is_empty() && name != "default")
    {
        let dot_omp = default_path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| default_path.clone());
        return Ok(dot_omp.join("profiles").join(name).join("agent"));
    }
    let (path, source) = match pi_env_override {
        Some(value) if !value.is_empty() => (
            crate::settings::resolve_override_path(value.to_string_lossy().as_ref()),
            "PI_CODING_AGENT_DIR",
        ),
        _ => (default_path, "OMP default"),
    };
    require_absolute(path, source)
}

fn require_absolute(path: PathBuf, source: &str) -> Result<PathBuf, AppError> {
    if !path.is_absolute() {
        return Err(AppError::InvalidInput(format!(
            "{source} must resolve to an absolute directory: {}",
            path.display()
        )));
    }
    Ok(path)
}

pub(crate) fn get_omp_models_path() -> Result<PathBuf, AppError> {
    Ok(get_omp_agent_dir()?.join("models.json"))
}

/// OMP keeps no `settings.json`; this mirror of the Pi helper is kept for
/// structural parity only. OMP's default model lives in `config.yml`.
#[allow(dead_code)]
pub(crate) fn get_omp_settings_path() -> Result<PathBuf, AppError> {
    Ok(get_omp_agent_dir()?.join("settings.json"))
}

/// Mirror of the Pi helper kept for structural parity only (see above).
#[allow(dead_code)]
pub(crate) fn read_omp_native_defaults() -> Result<OmpNativeDefaults, AppError> {
    let path = get_omp_settings_path()?;
    if !path.exists() {
        return Ok(OmpNativeDefaults::default());
    }
    let value = read_json5_value(&path, "OMP settings")?;
    let object = value.as_object().ok_or_else(|| {
        AppError::Config(format!(
            "OMP settings root must be an object: {}",
            path.display()
        ))
    })?;
    Ok(OmpNativeDefaults {
        default_provider: optional_string(object, "defaultProvider", &path)?,
        default_model: optional_string(object, "defaultModel", &path)?,
        session_dir: optional_string(object, "sessionDir", &path)?,
    })
}

pub(crate) fn get_omp_config_path() -> Result<PathBuf, AppError> {
    Ok(get_omp_agent_dir()?.join("config.yml"))
}

/// Read OMP's global default role (`modelRoles.default` in `config.yml`).
///
/// A missing file means "no default selected". An unreadable or invalid file
/// is an error; the user's file is never renamed or quarantined here.
pub(crate) fn read_omp_default_role() -> Result<Option<String>, AppError> {
    let path = get_omp_config_path()?;
    if !path.exists() {
        return Ok(None);
    }
    let mapping = read_config_mapping(&path)?;
    let Some(roles) = yaml_mapping_lookup(&mapping, "modelRoles") else {
        return Ok(None);
    };
    if roles.is_null() {
        return Ok(None);
    }
    let roles = roles.as_mapping().ok_or_else(|| {
        AppError::Config(format!(
            "OMP config 'modelRoles' must be a mapping: {}",
            path.display()
        ))
    })?;
    match yaml_mapping_lookup(roles, "default") {
        None | Some(serde_yaml::Value::Null) => Ok(None),
        Some(serde_yaml::Value::String(value)) => Ok(Some(value.clone())),
        Some(_) => Err(AppError::Config(format!(
            "OMP config 'modelRoles.default' must be a string: {}",
            path.display()
        ))),
    }
}

/// Point OMP's global default role at `<provider_id>/<model_id>`.
///
/// Any `:thinking`-style tail on the previous default is carried over, all
/// other `config.yml` keys are preserved, and the file is written back with
/// 0600 permissions via the shared atomic writer.
pub(crate) fn write_omp_default_role(
    provider_id: &str,
    model_id: &str,
) -> Result<(), AppError> {
    let path = get_omp_config_path()?;
    let previous = read_omp_default_role()?;
    let suffix = previous
        .as_deref()
        .and_then(|role| role.find(':').map(|idx| role[idx..].to_string()))
        .unwrap_or_default();
    let mut mapping = if path.exists() {
        read_config_mapping(&path)?
    } else {
        serde_yaml::Mapping::new()
    };
    if yaml_mapping_lookup(&mapping, "modelRoles").is_none() {
        mapping.insert(
            serde_yaml::Value::String("modelRoles".to_string()),
            serde_yaml::Value::Mapping(serde_yaml::Mapping::new()),
        );
    }
    let roles = yaml_mapping_lookup_mut(&mut mapping, "modelRoles").ok_or_else(|| {
        AppError::Config(format!(
            "OMP config 'modelRoles' must be a mapping: {}",
            path.display()
        ))
    })?;
    let roles = roles.as_mapping_mut().ok_or_else(|| {
        AppError::Config(format!(
            "OMP config 'modelRoles' must be a mapping: {}",
            path.display()
        ))
    })?;
    roles.insert(
        serde_yaml::Value::String("default".to_string()),
        serde_yaml::Value::String(format!("{provider_id}/{model_id}{suffix}")),
    );
    let text = serde_yaml::to_string(&serde_yaml::Value::Mapping(mapping)).map_err(|source| {
        AppError::Config(format!(
            "failed to serialize OMP config ({}): {source}",
            path.display()
        ))
    })?;
    ensure_private_models_parent(&path)?;
    atomic_write_private(&path, text.as_bytes())
}

fn read_config_mapping(path: &Path) -> Result<serde_yaml::Mapping, AppError> {
    let bytes = read_file_limited(path, "OMP config")?;
    let source = String::from_utf8(bytes).map_err(|error| {
        AppError::Config(format!(
            "OMP config file must be UTF-8 ({}): {error}",
            path.display()
        ))
    })?;
    if source.trim().is_empty() {
        return Ok(serde_yaml::Mapping::new());
    }
    let document: serde_yaml::Value =
        serde_yaml::from_str(&source).map_err(|error| {
            AppError::Config(format!(
                "OMP config file is not valid YAML ({}): {error}",
                path.display()
            ))
        })?;
    if document.is_null() {
        return Ok(serde_yaml::Mapping::new());
    }
    document.as_mapping().cloned().ok_or_else(|| {
        AppError::Config(format!(
            "OMP config root must be a mapping: {}",
            path.display()
        ))
    })
}

fn yaml_mapping_lookup<'a>(
    mapping: &'a serde_yaml::Mapping,
    key: &str,
) -> Option<&'a serde_yaml::Value> {
    mapping
        .iter()
        .find(|(k, _)| k.as_str() == Some(key))
        .map(|(_, v)| v)
}

fn yaml_mapping_lookup_mut<'a>(
    mapping: &'a mut serde_yaml::Mapping,
    key: &str,
) -> Option<&'a mut serde_yaml::Value> {
    mapping
        .iter_mut()
        .find(|(k, _)| k.as_str() == Some(key))
        .map(|(_, v)| v)
}

pub(crate) fn read_omp_native_providers() -> Result<IndexMap<String, Value>, AppError> {
    let _guard = lock_models_file()?;
    read_omp_native_providers_locked(&get_omp_models_path()?)
}

pub(crate) fn read_omp_native_provider(provider_key: &str) -> Result<Option<Value>, AppError> {
    let _guard = lock_models_file()?;
    let path = get_omp_models_path()?;
    let document = read_models_document(&path)?;
    Ok(providers(&document, &path)?.get(provider_key).cloned())
}

pub(crate) fn omp_provider_exists(provider_key: &str) -> Result<bool, AppError> {
    let _guard = lock_models_file()?;
    let path = get_omp_models_path()?;
    let document = read_models_document(&path)?;
    Ok(providers(&document, &path)?.contains_key(provider_key))
}

pub(crate) fn insert_omp_provider(provider_key: &str, config: &Value) -> Result<bool, AppError> {
    validate_provider_node(provider_key, config)?;
    let _guard = lock_models_file()?;
    let path = get_omp_models_path()?;
    let (mut document, expected_revision) = read_models_document_with_revision(&path)?;
    let providers = providers_mut(&mut document, &path)?;

    match providers.get(provider_key) {
        Some(current) if current == config => return Ok(false),
        Some(_) => {
            return Err(AppError::InvalidInput(format!(
                "OMP provider key '{provider_key}' already exists in models.json"
            )))
        }
        None => {}
    }

    providers.insert(provider_key.to_string(), config.clone());
    write_models_document(&path, &document, &expected_revision)?;
    Ok(true)
}

pub(crate) fn replace_omp_provider(
    provider_key: &str,
    expected: &Value,
    replacement: &Value,
) -> Result<(), AppError> {
    validate_provider_node(provider_key, replacement)?;
    let _guard = lock_models_file()?;
    let path = get_omp_models_path()?;
    let (mut document, expected_revision) = read_models_document_with_revision(&path)?;
    let providers = providers_mut(&mut document, &path)?;
    let current = providers.get(provider_key).ok_or_else(|| {
        AppError::Conflict(format!(
            "OMP provider '{provider_key}' is no longer present in models.json"
        ))
    })?;
    if current != expected {
        return Err(AppError::Conflict(format!(
            "OMP provider '{provider_key}' changed outside CC Switch"
        )));
    }
    if current == replacement {
        return Ok(());
    }
    providers.insert(provider_key.to_string(), replacement.clone());
    write_models_document(&path, &document, &expected_revision)
}

pub(crate) fn replace_omp_provider_if_present(
    provider_key: &str,
    replacement: &Value,
) -> Result<Option<Value>, AppError> {
    validate_provider_node(provider_key, replacement)?;
    let _guard = lock_models_file()?;
    let path = get_omp_models_path()?;
    let (mut document, expected_revision) = read_models_document_with_revision(&path)?;
    let providers = providers_mut(&mut document, &path)?;
    let Some(current) = providers.get(provider_key).cloned() else {
        return Ok(None);
    };
    if current == *replacement {
        return Ok(Some(current));
    }
    providers.insert(provider_key.to_string(), replacement.clone());
    write_models_document(&path, &document, &expected_revision)?;
    Ok(Some(current))
}

pub(crate) fn remove_omp_provider(provider_key: &str) -> Result<Option<Value>, AppError> {
    remove_omp_provider_inner(provider_key, None)
}

pub(crate) fn remove_omp_provider_if_matches(
    provider_key: &str,
    expected: &Value,
) -> Result<bool, AppError> {
    remove_omp_provider_inner(provider_key, Some(expected)).map(|removed| removed.is_some())
}

fn remove_omp_provider_inner(
    provider_key: &str,
    expected: Option<&Value>,
) -> Result<Option<Value>, AppError> {
    let _guard = lock_models_file()?;
    let path = get_omp_models_path()?;
    let (mut document, expected_revision) = read_models_document_with_revision(&path)?;
    let providers = providers_mut(&mut document, &path)?;
    let Some(current) = providers.get(provider_key).cloned() else {
        return Ok(None);
    };
    if expected.is_some_and(|expected| current != *expected) {
        return Err(AppError::Conflict(format!(
            "OMP provider '{provider_key}' changed outside CC Switch"
        )));
    }
    providers.remove(provider_key);
    write_models_document(&path, &document, &expected_revision)?;
    Ok(Some(current))
}

pub(crate) fn restore_omp_provider_if_missing(
    provider_key: &str,
    config: &Value,
) -> Result<(), AppError> {
    let _guard = lock_models_file()?;
    let path = get_omp_models_path()?;
    let (mut document, expected_revision) = read_models_document_with_revision(&path)?;
    let providers = providers_mut(&mut document, &path)?;
    match providers.get(provider_key) {
        Some(current) if current == config => Ok(()),
        Some(_) => Err(AppError::Conflict(format!(
            "cannot restore OMP provider '{provider_key}' because another value now owns the key"
        ))),
        None => {
            providers.insert(provider_key.to_string(), config.clone());
            write_models_document(&path, &document, &expected_revision)
        }
    }
}

/// Validate the shape CC Switch can persist as one
/// `models.json.providers.<provider_key>` node.
///
/// Provider ownership is intentionally source-based: every explicit object in
/// `models.json.providers` is manageable, including keys also built into OMP.
/// OMP's `/login` credentials live in `auth.json` and are never read here.
pub(crate) fn validate_provider_node(provider_key: &str, config: &Value) -> Result<(), AppError> {
    if provider_key.trim().is_empty() {
        return Err(AppError::InvalidInput(
            "OMP provider key cannot be empty".to_string(),
        ));
    }
    config.as_object().ok_or_else(|| {
        AppError::InvalidInput("OMP provider configuration must be an object".to_string())
    })?;
    Ok(())
}

pub(crate) fn provider_base_url(config: &Value) -> Result<String, AppError> {
    let provider = config.as_object().ok_or_else(|| {
        AppError::InvalidInput("OMP provider configuration must be an object".to_string())
    })?;
    nonempty_string(provider.get("baseUrl"))
        .or_else(|| {
            provider
                .get("models")
                .and_then(Value::as_array)
                .and_then(|models| {
                    models
                        .iter()
                        .find_map(|model| nonempty_string(model.get("baseUrl")))
                })
        })
        .map(str::to_string)
        .ok_or_else(|| AppError::InvalidInput("OMP provider has no request URL".to_string()))
}

fn lock_models_file() -> Result<MutexGuard<'static, ()>, AppError> {
    MODELS_FILE_LOCK
        .lock()
        .map_err(|error| AppError::Config(format!("OMP models file lock is poisoned: {error}")))
}

fn read_omp_native_providers_locked(path: &Path) -> Result<IndexMap<String, Value>, AppError> {
    let document = read_models_document(path)?;
    let providers = providers(&document, path)?;
    Ok(providers
        .iter()
        .map(|(provider_key, config)| (provider_key.clone(), config.clone()))
        .collect())
}

fn read_models_document(path: &Path) -> Result<Value, AppError> {
    read_models_document_with_revision(path).map(|(document, _)| document)
}

fn read_models_document_with_revision(path: &Path) -> Result<(Value, String), AppError> {
    if !path.exists() {
        return Ok((
            Value::Object(Map::new()),
            MISSING_MODELS_REVISION.to_string(),
        ));
    }
    let bytes = read_file_limited(path, "OMP models")?;
    let revision = revision(&bytes);
    let document = parse_json5_value(path, "OMP models", bytes)?;
    Ok((document, revision))
}

fn read_json5_value(path: &Path, label: &str) -> Result<Value, AppError> {
    parse_json5_value(path, label, read_file_limited(path, label)?)
}

fn read_file_limited(path: &Path, label: &str) -> Result<Vec<u8>, AppError> {
    let file = fs::File::open(path).map_err(|error| AppError::io(path, error))?;
    let metadata = file.metadata().map_err(|error| AppError::io(path, error))?;
    if metadata.len() > MAX_OMP_FILE_BYTES {
        return Err(AppError::InvalidInput(format!(
            "{label} file exceeds the 1 MiB limit: {}",
            path.display()
        )));
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(MAX_OMP_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| AppError::io(path, error))?;
    if bytes.len() as u64 > MAX_OMP_FILE_BYTES {
        return Err(AppError::InvalidInput(format!(
            "{label} file exceeds the 1 MiB limit: {}",
            path.display()
        )));
    }
    Ok(bytes)
}

fn parse_json5_value(path: &Path, label: &str, bytes: Vec<u8>) -> Result<Value, AppError> {
    let source = String::from_utf8(bytes).map_err(|error| {
        AppError::Config(format!(
            "{label} file must be UTF-8 ({}): {error}",
            path.display()
        ))
    })?;
    json5::from_str(&source).map_err(|error| {
        AppError::Config(format!(
            "{label} file is not valid JSON/JSONC ({}): {error}",
            path.display()
        ))
    })
}

fn providers<'a>(document: &'a Value, path: &Path) -> Result<&'a Map<String, Value>, AppError> {
    let root = document.as_object().ok_or_else(|| {
        AppError::Config(format!(
            "OMP models root must be an object: {}",
            path.display()
        ))
    })?;
    match root.get("providers") {
        None => Ok(empty_json_object()),
        Some(Value::Object(providers)) => Ok(providers),
        Some(_) => Err(AppError::Config(format!(
            "OMP models 'providers' must be an object: {}",
            path.display()
        ))),
    }
}

fn providers_mut<'a>(
    document: &'a mut Value,
    path: &Path,
) -> Result<&'a mut Map<String, Value>, AppError> {
    let root = document.as_object_mut().ok_or_else(|| {
        AppError::Config(format!(
            "OMP models root must be an object: {}",
            path.display()
        ))
    })?;
    let value = root
        .entry("providers".to_string())
        .or_insert_with(|| Value::Object(Map::new()));
    value.as_object_mut().ok_or_else(|| {
        AppError::Config(format!(
            "OMP models 'providers' must be an object: {}",
            path.display()
        ))
    })
}

fn empty_json_object() -> &'static Map<String, Value> {
    static EMPTY: LazyLock<Map<String, Value>> = LazyLock::new(Map::new);
    &EMPTY
}

fn write_models_document(
    path: &Path,
    document: &Value,
    expected_revision: &str,
) -> Result<(), AppError> {
    let mut bytes =
        serde_json::to_vec_pretty(document).map_err(|source| AppError::JsonSerialize { source })?;
    bytes.push(b'\n');
    ensure_private_models_parent(path)?;
    ensure_models_revision(path, expected_revision)?;
    atomic_write_private(path, &bytes)
}

fn ensure_models_revision(path: &Path, expected_revision: &str) -> Result<(), AppError> {
    let actual_revision = match fs::File::open(path) {
        Ok(_) => revision(&read_file_limited(path, "OMP models")?),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            MISSING_MODELS_REVISION.to_string()
        }
        Err(error) => return Err(AppError::io(path, error)),
    };
    if actual_revision == expected_revision {
        Ok(())
    } else {
        Err(AppError::Conflict(format!(
            "OMP models.json changed outside CC Switch: {}",
            path.display()
        )))
    }
}

fn revision(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn ensure_private_models_parent(path: &Path) -> Result<(), AppError> {
    let parent = path.parent().ok_or_else(|| {
        AppError::Config(format!(
            "OMP models path has no parent directory: {}",
            path.display()
        ))
    })?;
    let created = !parent.exists();
    fs::create_dir_all(parent).map_err(|source| AppError::io(parent, source))?;

    #[cfg(not(unix))]
    let _ = created;

    #[cfg(unix)]
    if created {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(parent, fs::Permissions::from_mode(0o700))
            .map_err(|source| AppError::io(parent, source))?;
    }

    Ok(())
}

fn optional_string(
    object: &Map<String, Value>,
    key: &str,
    path: &Path,
) -> Result<Option<String>, AppError> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value.clone())),
        Some(_) => Err(AppError::Config(format!(
            "OMP settings '{key}' must be a string: {}",
            path.display()
        ))),
    }
}

fn nonempty_string(value: Option<&Value>) -> Option<&str> {
    value
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
}

#[cfg(test)]
pub(crate) mod test_support {
    use std::path::{Path, PathBuf};

    pub(crate) struct TestAgentDir {
        _dir: Option<tempfile::TempDir>,
        previous: Option<PathBuf>,
    }

    impl TestAgentDir {
        pub(crate) fn new() -> Self {
            let dir = tempfile::tempdir().expect("create OMP test directory");
            let agent_dir = dir.path().join("agent");
            Self::set(agent_dir, Some(dir))
        }

        pub(crate) fn at(agent_dir: &Path) -> Self {
            Self::set(agent_dir.to_path_buf(), None)
        }

        fn set(agent_dir: PathBuf, dir: Option<tempfile::TempDir>) -> Self {
            let previous = super::TEST_AGENT_DIR
                .lock()
                .expect("lock OMP test directory")
                .replace(agent_dir);
            Self {
                _dir: dir,
                previous,
            }
        }
    }

    impl Drop for TestAgentDir {
        fn drop(&mut self) {
            *super::TEST_AGENT_DIR
                .lock()
                .expect("lock OMP test directory") = self.previous.take();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use serial_test::serial;

    fn provider() -> Value {
        json!({
            "name": "Example",
            "baseUrl": "https://api.example.com/v1",
            "api": "openai-completions",
            "apiKey": "secret",
            "models": [{"id": "example-model"}]
        })
    }

    #[test]
    fn provider_node_accepts_unknown_native_fields() {
        let mut value = provider();
        value["sdkOption"] = json!({"timeout": 30});
        value["models"][0]["compat"] = json!({"supportsDeveloperRole": true});
        validate_provider_node("cc-switch-example", &value).expect("valid provider");
    }

    #[test]
    fn provider_node_ownership_depends_on_models_json_membership() {
        let mut oauth = provider();
        oauth["oauth"] = json!("anthropic");
        validate_provider_node("cc-switch-example", &oauth)
            .expect("an explicit models.json node stays manageable");
        validate_provider_node("anthropic", &json!({}))
            .expect("a built-in provider key may be explicitly configured");
        assert!(validate_provider_node("", &json!({})).is_err());
        assert!(validate_provider_node("anthropic", &json!("invalid")).is_err());
    }

    #[test]
    fn relative_agent_directory_is_rejected() {
        let error = resolve_omp_agent_dir(
            Some(PathBuf::from("relative/omp-agent")),
            None,
            None,
            PathBuf::from("default"),
        )
        .expect_err("relative OMP directory must be rejected");
        assert!(error.to_string().contains("absolute directory"));
    }

    #[test]
    fn settings_directory_precedes_profile_and_environment() {
        let temp = tempfile::tempdir().expect("tempdir");
        let settings_dir = temp.path().join("settings-agent");
        let env_dir = temp.path().join("env-agent");

        assert_eq!(
            resolve_omp_agent_dir(
                Some(settings_dir.clone()),
                Some("some-profile".to_string()),
                Some(env_dir.clone().into_os_string()),
                temp.path().join("default-agent"),
            )
            .expect("resolve OMP directory"),
            settings_dir
        );
    }

    #[test]
    fn profile_relocates_below_dot_omp_before_pi_fallback() {
        let temp = tempfile::tempdir().expect("tempdir");
        let default_agent = temp.path().join(".omp").join("agent");
        let env_dir = temp.path().join("env-agent");

        assert_eq!(
            resolve_omp_agent_dir(
                None,
                Some("cctest".to_string()),
                Some(env_dir.into_os_string()),
                default_agent.clone(),
            )
            .expect("resolve OMP directory"),
            temp.path()
                .join(".omp")
                .join("profiles")
                .join("cctest")
                .join("agent")
        );
        assert_eq!(
            resolve_omp_agent_dir(
                None,
                Some("  default  ".to_string()),
                Some(env_dir.clone().into_os_string()),
                default_agent.clone(),
            )
            .expect("the default profile must not relocate"),
            env_dir
        );
        assert_eq!(
            resolve_omp_agent_dir(None, None, None, default_agent.clone())
                .expect("resolve OMP directory"),
            default_agent
        );
    }

    #[test]
    #[serial]
    fn duplicate_provider_key_is_validation_not_a_write_conflict() {
        let _agent = test_support::TestAgentDir::new();
        insert_omp_provider("duplicate", &provider()).expect("insert provider");
        let mut replacement = provider();
        replacement["name"] = json!("Other");

        let error = insert_omp_provider("duplicate", &replacement)
            .expect_err("duplicate provider key must be rejected");
        assert!(matches!(error, AppError::InvalidInput(_)));
    }

    #[cfg(unix)]
    #[test]
    #[serial]
    fn newly_created_models_file_and_agent_directory_are_private() {
        use std::os::unix::fs::PermissionsExt;

        let _agent = test_support::TestAgentDir::new();
        insert_omp_provider("cc-switch-private", &provider()).expect("write private models file");

        let path = get_omp_models_path().expect("models path");
        let file_mode = fs::metadata(&path)
            .expect("models metadata")
            .permissions()
            .mode()
            & 0o777;
        let directory_mode = fs::metadata(path.parent().expect("agent directory"))
            .expect("agent directory metadata")
            .permissions()
            .mode()
            & 0o777;

        assert_eq!(file_mode, 0o600);
        assert_eq!(directory_mode, 0o700);
    }

    #[test]
    #[serial]
    fn stale_models_revision_does_not_overwrite_an_external_edit() {
        let _agent = test_support::TestAgentDir::new();
        let path = get_omp_models_path().expect("models path");
        ensure_private_models_parent(&path).expect("create agent directory");
        fs::write(&path, r#"{"providers":{"external":{"models":[]}}}"#)
            .expect("write initial models");
        let (_, stale_revision) =
            read_models_document_with_revision(&path).expect("read models revision");

        let external = r#"{"providers":{"external":{"models":[]},"omp-added":{"models":[]}}}"#;
        fs::write(&path, external).expect("edit models externally");

        let replacement = json!({"providers": {"cc-switch": provider()}});
        let error = write_models_document(&path, &replacement, &stale_revision)
            .expect_err("stale write must fail");
        assert!(matches!(error, AppError::Conflict(_)));
        assert_eq!(
            fs::read_to_string(path).expect("read external models"),
            external
        );
    }
    #[test]
    #[serial]
    fn missing_config_means_no_default_role() {
        let _agent = test_support::TestAgentDir::new();
        assert_eq!(read_omp_default_role().expect("read default role"), None);
    }

    #[test]
    #[serial]
    fn default_role_round_trip_preserves_other_keys_and_thinking_tail() {
        let _agent = test_support::TestAgentDir::new();
        let path = get_omp_config_path().expect("config path");
        fs::create_dir_all(path.parent().expect("agent directory")).expect("create agent dir");
        fs::write(
            &path,
            "theme: dark\nmodelRoles:\n  default: old-prov/old-model:xhigh\n",
        )
        .expect("write config");

        write_omp_default_role("new-prov", "new-model").expect("write default role");
        assert_eq!(
            read_omp_default_role().expect("read default role"),
            Some("new-prov/new-model:xhigh".to_string())
        );
        let text = fs::read_to_string(&path).expect("read config");
        assert!(text.contains("theme: dark"), "other keys survive: {text}");

        write_omp_default_role("plain-prov", "plain-model").expect("rewrite default role");
        assert_eq!(
            read_omp_default_role().expect("read default role"),
            Some("plain-prov/plain-model:xhigh".to_string())
        );
    }

    #[test]
    #[serial]
    fn default_role_without_tail_writes_bare_role() {
        let _agent = test_support::TestAgentDir::new();
        write_omp_default_role("prov", "model").expect("write default role");
        assert_eq!(
            read_omp_default_role().expect("read default role"),
            Some("prov/model".to_string())
        );
    }

    #[test]
    #[serial]
    fn invalid_config_yaml_is_an_error_not_a_quarantine() {
        let _agent = test_support::TestAgentDir::new();
        let path = get_omp_config_path().expect("config path");
        fs::create_dir_all(path.parent().expect("agent directory")).expect("create agent dir");
        fs::write(&path, "{not-yaml: [unclosed").expect("write invalid config");

        let error = read_omp_default_role().expect_err("invalid YAML must fail");
        assert!(matches!(error, AppError::Config(_)));
        assert!(path.exists(), "the user file must be left in place");
    }
}
