use crate::clients::{ClientId, PathRoot};
use crate::scanner::ScannerSettings;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
#[cfg(windows)]
use std::path::Component;
use std::path::{Path, PathBuf};

const DOMAIN: &[u8] = b"tokenbar-source-context";
const RESOLVER_CONTRACT_VERSION: u32 = 1;

const ENV_HOME: &str = "HOME";
const ENV_XDG_DATA_HOME: &str = "XDG_DATA_HOME";
const ENV_TOKSCALE_CONFIG_DIR: &str = "TOKSCALE_CONFIG_DIR";
const ENV_XDG_CONFIG_HOME: &str = "XDG_CONFIG_HOME";
const ENV_TOKSCALE_HEADLESS_DIR: &str = "TOKSCALE_HEADLESS_DIR";
const ENV_COPILOT_EXPORTER: &str = "COPILOT_OTEL_FILE_EXPORTER_PATH";
const ENV_LOCALAPPDATA: &str = "LOCALAPPDATA";
const ENV_APPDATA: &str = "APPDATA";
const ENV_KIMI_CODE_HOME: &str = "KIMI_CODE_HOME";
const ENV_TOKSCALE_EXTRA_DIRS: &str = "TOKSCALE_EXTRA_DIRS";
const ENV_GOOSE_PATH_ROOT: &str = "GOOSE_PATH_ROOT";
const ENV_CODEBUFF_DATA_DIR: &str = "CODEBUFF_DATA_DIR";
const ENV_XDG_RUNTIME_DIR: &str = "XDG_RUNTIME_DIR";
const ENV_TOKSCALE_PRICING_CACHE_ONLY: &str = "TOKSCALE_PRICING_CACHE_ONLY";

const PLATFORM_SOURCE_ENV_KEYS: &[&str] = &[
    ENV_HOME,
    ENV_XDG_DATA_HOME,
    ENV_TOKSCALE_CONFIG_DIR,
    ENV_XDG_CONFIG_HOME,
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SourceContextUnavailable;

impl std::fmt::Display for SourceContextUnavailable {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("sourceContextUnavailable")
    }
}

impl std::error::Error for SourceContextUnavailable {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InputState {
    Unset,
    Empty,
    Value,
}

impl InputState {
    const fn tag(self) -> u8 {
        match self {
            Self::Unset => 0,
            Self::Empty => 1,
            Self::Value => 2,
        }
    }
}

#[derive(Debug, Clone)]
struct CapturedInput {
    state: InputState,
    value: Option<OsString>,
}

impl CapturedInput {
    fn unset() -> Self {
        Self {
            state: InputState::Unset,
            value: None,
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct SourceResolutionInputs {
    values: BTreeMap<&'static str, CapturedInput>,
    config_dir: Option<PathBuf>,
    data_local_dir: Option<PathBuf>,
    platform_home: Option<PathBuf>,
    #[cfg_attr(not(unix), allow(dead_code))]
    temp_dir: PathBuf,
}

#[derive(Debug, Clone)]
struct PlatformInputs {
    config_dir: Option<PathBuf>,
    data_local_dir: Option<PathBuf>,
    home_dir: Option<PathBuf>,
    temp_dir: PathBuf,
}

impl PlatformInputs {
    fn capture() -> Self {
        Self {
            config_dir: dirs::config_dir(),
            data_local_dir: dirs::data_local_dir(),
            home_dir: dirs::home_dir(),
            temp_dir: std::env::temp_dir(),
        }
    }
}

impl SourceResolutionInputs {
    fn capture() -> Self {
        Self::capture_with(|key| std::env::var_os(key), PlatformInputs::capture())
    }

    fn capture_with(
        mut read_env: impl FnMut(&str) -> Option<OsString>,
        platform: PlatformInputs,
    ) -> Self {
        let mut keys = resolver_environment_keys();
        keys.push(ENV_XDG_RUNTIME_DIR);
        keys.push(ENV_TOKSCALE_PRICING_CACHE_ONLY);
        keys.sort_unstable();
        keys.dedup();
        let values = keys
            .into_iter()
            .map(|key| {
                let value = read_env(key);
                let state = match value.as_ref() {
                    None => InputState::Unset,
                    Some(value) if value.is_empty() => InputState::Empty,
                    Some(_) => InputState::Value,
                };
                (key, CapturedInput { state, value })
            })
            .collect();
        Self {
            values,
            config_dir: platform.config_dir,
            data_local_dir: platform.data_local_dir,
            platform_home: platform.home_dir,
            temp_dir: platform.temp_dir,
        }
    }

    fn input(&self, key: &str) -> CapturedInput {
        self.values
            .get(key)
            .cloned()
            .unwrap_or_else(CapturedInput::unset)
    }

    pub(crate) fn var_os(&self, key: &str) -> Option<&OsStr> {
        self.values.get(key)?.value.as_deref()
    }

    pub(crate) fn var_nonempty(&self, key: &str) -> Option<&OsStr> {
        self.var_os(key).filter(|value| !value.is_empty())
    }

    pub(crate) fn var_string(&self, key: &str) -> Option<String> {
        self.var_os(key)?.to_str().map(ToOwned::to_owned)
    }
}

#[derive(Debug, Clone)]
pub struct ResolvedLocalSourceContext {
    home_dir: PathBuf,
    use_env_roots: bool,
    scanner_settings: ScannerSettings,
    pricing_cache_only: bool,
    source_cache_dir: Option<PathBuf>,
    platform_config_dir: Option<PathBuf>,
    platform_data_local_dir: Option<PathBuf>,
    source_env_paths: BTreeMap<&'static str, ResolvedPathInput>,
    codex_archive_root: PathBuf,
    extra_scan_paths: Vec<(ClientId, PathBuf)>,
    identity: [u8; 32],
}

#[derive(Debug, Clone)]
struct ResolvedPathInput {
    state: InputState,
    resolution: PathResolution,
    path: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PathResolution {
    Ignored,
    Fallback,
    Explicit,
}

impl PathResolution {
    const fn tag(self) -> u8 {
        match self {
            Self::Ignored => 0,
            Self::Fallback => 1,
            Self::Explicit => 2,
        }
    }
}

impl ResolvedPathInput {
    fn ignored() -> Self {
        Self {
            state: InputState::Unset,
            resolution: PathResolution::Ignored,
            path: None,
        }
    }

    fn fallback(input: &CapturedInput, path: PathBuf) -> Self {
        Self {
            state: input.state,
            resolution: PathResolution::Fallback,
            path: Some(path),
        }
    }

    fn unavailable(path: PathBuf) -> Self {
        Self {
            state: InputState::Unset,
            resolution: PathResolution::Fallback,
            path: Some(path),
        }
    }

    fn explicit(input: &CapturedInput, path: PathBuf) -> Self {
        Self {
            state: input.state,
            resolution: PathResolution::Explicit,
            path: Some(path),
        }
    }
}

impl ResolvedLocalSourceContext {
    pub fn capture(
        home_dir: Option<PathBuf>,
        use_env_roots: bool,
        scanner_settings: ScannerSettings,
    ) -> Result<Self, SourceContextUnavailable> {
        let cwd = std::env::current_dir().map_err(|_| SourceContextUnavailable)?;
        Self::capture_resolved(
            cwd,
            home_dir,
            use_env_roots,
            scanner_settings,
            SourceResolutionInputs::capture(),
        )
    }

    fn capture_resolved(
        cwd: PathBuf,
        home_dir: Option<PathBuf>,
        use_env_roots: bool,
        scanner_settings: ScannerSettings,
        inputs: SourceResolutionInputs,
    ) -> Result<Self, SourceContextUnavailable> {
        let home = home_dir
            .filter(|path| !path.as_os_str().is_empty())
            .or_else(|| inputs.var_nonempty(ENV_HOME).map(PathBuf::from))
            .or_else(|| inputs.platform_home.clone())
            .ok_or(SourceContextUnavailable)?;
        let home_dir = fully_qualified(&cwd, &home)?;
        let scanner_settings = resolve_scanner_settings(&cwd, scanner_settings)?;
        let pricing_cache_only = inputs
            .var_string(ENV_TOKSCALE_PRICING_CACHE_ONLY)
            .is_some_and(|value| matches!(value.as_str(), "1" | "true" | "TRUE" | "yes" | "YES"));
        let source_cache_dir = resolve_source_cache_dir(&cwd, &home_dir, &inputs)?;
        let platform_config_dir = platform_config_root(&inputs, &cwd)?;
        let platform_data_local_dir = platform_data_local_root(&inputs, &cwd)?;
        let codex_archive_root =
            resolve_codex_archive_root(&cwd, &home_dir, use_env_roots, &inputs)?;
        let mut source_env_paths = resolve_source_environment_paths(
            &cwd,
            &home_dir,
            use_env_roots,
            &inputs,
            platform_config_dir.as_deref(),
        )?;
        let extra_scan_paths = resolve_extra_scan_paths(&cwd, use_env_roots, &inputs)?;
        let mut extra_dirs_input = inputs.input(ENV_TOKSCALE_EXTRA_DIRS);
        if extra_dirs_input
            .value
            .as_deref()
            .is_some_and(|value| value.to_str().is_none())
        {
            extra_dirs_input = CapturedInput::unset();
        }
        source_env_paths.insert(
            ENV_TOKSCALE_EXTRA_DIRS,
            if !use_env_roots {
                ResolvedPathInput::ignored()
            } else if extra_scan_paths.is_empty() {
                ResolvedPathInput::fallback(&extra_dirs_input, home_dir.clone())
            } else {
                ResolvedPathInput::explicit(&extra_dirs_input, home_dir.clone())
            },
        );

        let mut context = Self {
            home_dir,
            use_env_roots,
            scanner_settings,
            pricing_cache_only,
            source_cache_dir,
            platform_config_dir,
            platform_data_local_dir,
            source_env_paths,
            codex_archive_root,
            extra_scan_paths,
            identity: [0; 32],
        };
        context.identity = context.compute_identity()?;
        Ok(context)
    }

    pub fn home_dir(&self) -> &Path {
        &self.home_dir
    }

    pub fn use_env_roots(&self) -> bool {
        self.use_env_roots
    }

    pub fn scanner_settings(&self) -> &ScannerSettings {
        &self.scanner_settings
    }

    pub fn identity_bytes(&self) -> [u8; 32] {
        self.identity
    }

    pub fn pricing_cache_only(&self) -> bool {
        self.pricing_cache_only
    }

    pub fn identity_string(&self) -> String {
        let mut result = String::with_capacity(68);
        result.push_str("sc1:");
        for byte in self.identity {
            use std::fmt::Write as _;
            let _ = write!(&mut result, "{byte:02x}");
        }
        result
    }

    pub(crate) fn source_cache_dir(&self) -> Option<&Path> {
        self.source_cache_dir.as_deref()
    }

    #[cfg(target_os = "windows")]
    pub(crate) fn data_local_dir(&self) -> Option<&Path> {
        self.platform_data_local_dir.as_deref()
    }

    pub(crate) fn source_env_path(&self, key: &str) -> Option<&Path> {
        self.source_env_paths.get(key)?.path.as_deref()
    }

    pub(crate) fn source_env_is_explicit(&self, key: &str) -> bool {
        self.source_env_paths
            .get(key)
            .is_some_and(|input| input.resolution == PathResolution::Explicit)
    }

    pub(crate) fn codex_archive_root(&self) -> &Path {
        &self.codex_archive_root
    }

    pub(crate) fn extra_scan_paths(&self) -> &[(ClientId, PathBuf)] {
        &self.extra_scan_paths
    }

    pub(crate) fn resolve_client_root(
        &self,
        root: PathRoot,
    ) -> Result<PathBuf, SourceContextUnavailable> {
        let fallback = match root {
            PathRoot::Home => self.home_dir.clone(),
            PathRoot::XdgData => self
                .source_env_path(ENV_XDG_DATA_HOME)
                .map(Path::to_path_buf)
                .unwrap_or_else(|| self.home_dir.join(".local/share")),
            PathRoot::Config => {
                if self.use_env_roots {
                    if let Some(custom) = self.source_env_path(ENV_TOKSCALE_CONFIG_DIR) {
                        custom.to_path_buf()
                    } else if cfg!(target_os = "linux") {
                        self.source_env_path(ENV_XDG_CONFIG_HOME)
                            .map(|root| root.join("tokscale"))
                            .unwrap_or_else(|| self.home_dir.join(".config/tokscale"))
                    } else if cfg!(target_os = "windows") {
                        self.platform_config_dir
                            .as_ref()
                            .map(|root| root.join("tokscale"))
                            .unwrap_or_else(|| self.home_dir.join(".config/tokscale"))
                    } else {
                        self.home_dir.join(".config/tokscale")
                    }
                } else if cfg!(target_os = "windows") {
                    self.home_dir.join("AppData/Roaming/tokscale")
                } else {
                    self.home_dir.join(".config/tokscale")
                }
            }
            PathRoot::EnvVar {
                var,
                fallback_relative,
            } => self
                .source_env_path(var)
                .map(Path::to_path_buf)
                .unwrap_or_else(|| self.home_dir.join(fallback_relative)),
        };
        Ok(fallback)
    }

    pub(crate) fn resolve_client_path(
        &self,
        client: ClientId,
    ) -> Result<PathBuf, SourceContextUnavailable> {
        let def = client.data();
        Ok(self.resolve_client_root(def.root)?.join(def.relative_path))
    }

    fn compute_identity(&self) -> Result<[u8; 32], SourceContextUnavailable> {
        let mut descriptor = Descriptor::default();
        descriptor.field(1);
        descriptor.bytes(DOMAIN)?;
        descriptor.field(2);
        descriptor.u32(RESOLVER_CONTRACT_VERSION);
        descriptor.field(3);
        descriptor.u8(target_os_tag());
        descriptor.field(4);
        descriptor.bool(self.use_env_roots);
        descriptor.field(5);
        descriptor.path(&self.home_dir)?;

        descriptor.field(6);
        descriptor.count(self.source_env_paths.len())?;
        for (key, input) in &self.source_env_paths {
            descriptor.text(key)?;
            descriptor.u8(input.state.tag());
            descriptor.u8(input.resolution.tag());
            descriptor.optional_path(input.path.as_deref())?;
        }

        descriptor.field(7);
        descriptor.optional_path(self.platform_config_dir.as_deref())?;
        descriptor.field(8);
        descriptor.optional_path(self.platform_data_local_dir.as_deref())?;

        descriptor.field(9);
        descriptor.count(ClientId::COUNT)?;
        for client in ClientId::iter() {
            let definition = client.data();
            descriptor.u32(client as u32);
            descriptor.text(definition.id)?;
            descriptor.u8(path_root_tag(definition.root));
            descriptor.text(definition.relative_path)?;
            descriptor.text(definition.pattern)?;
            descriptor.path(&self.resolve_client_path(client)?)?;
        }

        descriptor.field(10);
        descriptor.count(self.scanner_settings.opencode_db_paths.len())?;
        for path in &self.scanner_settings.opencode_db_paths {
            descriptor.path(path)?;
        }
        descriptor.field(11);
        descriptor.count(self.scanner_settings.extra_scan_paths.len())?;
        for (client, paths) in &self.scanner_settings.extra_scan_paths {
            descriptor.text(client)?;
            descriptor.count(paths.len())?;
            for path in paths {
                descriptor.path(path)?;
            }
        }

        descriptor.field(12);
        descriptor.count(self.extra_scan_paths.len())?;
        for (client, path) in &self.extra_scan_paths {
            descriptor.u32(*client as u32);
            descriptor.path(path)?;
        }

        descriptor.field(13);
        descriptor.path(&self.codex_archive_root)?;

        descriptor.field(14);
        descriptor.optional_path(self.source_cache_dir.as_deref())?;

        Ok(Sha256::digest(descriptor.0).into())
    }
}

fn resolver_environment_keys() -> Vec<&'static str> {
    let mut keys = PLATFORM_SOURCE_ENV_KEYS.to_vec();
    keys.extend_from_slice(crate::scanner::DIRECT_SOURCE_ENV_KEYS);
    for client in ClientId::iter() {
        if let PathRoot::EnvVar { var, .. } = client.data().root {
            keys.push(var);
        }
    }
    keys
}

fn path_root_tag(root: PathRoot) -> u8 {
    match root {
        PathRoot::Home => 1,
        PathRoot::XdgData => 2,
        PathRoot::Config => 3,
        PathRoot::EnvVar { .. } => 4,
    }
}

fn resolve_codex_archive_root(
    cwd: &Path,
    home: &Path,
    use_env_roots: bool,
    inputs: &SourceResolutionInputs,
) -> Result<PathBuf, SourceContextUnavailable> {
    let path = if use_env_roots {
        inputs
            .var_string("CODEX_HOME")
            .map(|root| PathBuf::from(format!("{root}/archived_sessions")))
            .unwrap_or_else(|| home.join(".codex/archived_sessions"))
    } else {
        home.join(".codex/archived_sessions")
    };
    fully_qualified(cwd, &path)
}

fn resolve_source_environment_paths(
    cwd: &Path,
    home: &Path,
    use_env_roots: bool,
    inputs: &SourceResolutionInputs,
    platform_config_dir: Option<&Path>,
) -> Result<BTreeMap<&'static str, ResolvedPathInput>, SourceContextUnavailable> {
    let mut resolved = BTreeMap::new();
    let mut keys = resolver_environment_keys();
    keys.sort_unstable();
    keys.dedup();

    for key in keys {
        let input = inputs.input(key);
        let value = if key == ENV_HOME {
            ResolvedPathInput::fallback(&input, home.to_path_buf())
        } else if !use_env_roots {
            ResolvedPathInput::ignored()
        } else if key == ENV_TOKSCALE_HEADLESS_DIR {
            match input.value.as_deref() {
                None => ResolvedPathInput::fallback(
                    &input,
                    fallback_source_env_path(key, home, platform_config_dir)?,
                ),
                Some(raw) => match raw.to_str() {
                    Some(value) => {
                        ResolvedPathInput::explicit(&input, fully_qualified(cwd, Path::new(value))?)
                    }
                    None => ResolvedPathInput::unavailable(fallback_source_env_path(
                        key,
                        home,
                        platform_config_dir,
                    )?),
                },
            }
        } else if source_env_trims_surrounding_whitespace(key) {
            match input.value.as_deref() {
                None => ResolvedPathInput::fallback(
                    &input,
                    fallback_source_env_path(key, home, platform_config_dir)?,
                ),
                Some(raw) => match raw.to_str() {
                    Some(value) if value.trim().is_empty() => ResolvedPathInput::fallback(
                        &input,
                        fallback_source_env_path(key, home, platform_config_dir)?,
                    ),
                    Some(value) => ResolvedPathInput::explicit(
                        &input,
                        fully_qualified(cwd, Path::new(value.trim()))?,
                    ),
                    None => ResolvedPathInput::unavailable(fallback_source_env_path(
                        key,
                        home,
                        platform_config_dir,
                    )?),
                },
            }
        } else if cfg!(target_os = "linux")
            && key == ENV_XDG_CONFIG_HOME
            && input.state == InputState::Empty
        {
            ResolvedPathInput::explicit(&input, PathBuf::from(std::path::MAIN_SEPARATOR_STR))
        } else if input.state == InputState::Empty {
            ResolvedPathInput::fallback(
                &input,
                fallback_source_env_path(key, home, platform_config_dir)?,
            )
        } else if let Some(raw) = input.value.as_deref() {
            if source_env_requires_unicode(key) && raw.to_str().is_none() {
                ResolvedPathInput::unavailable(fallback_source_env_path(
                    key,
                    home,
                    platform_config_dir,
                )?)
            } else if source_env_uses_nonblank_semantics(key)
                && raw.to_str().is_some_and(|value| value.trim().is_empty())
            {
                ResolvedPathInput::fallback(
                    &input,
                    fallback_source_env_path(key, home, platform_config_dir)?,
                )
            } else if key == ENV_TOKSCALE_EXTRA_DIRS {
                ResolvedPathInput::fallback(&input, home.to_path_buf())
            } else {
                ResolvedPathInput::explicit(&input, fully_qualified(cwd, Path::new(raw))?)
            }
        } else {
            ResolvedPathInput::fallback(
                &input,
                fallback_source_env_path(key, home, platform_config_dir)?,
            )
        };
        resolved.insert(key, value);
    }

    Ok(resolved)
}

fn source_env_requires_unicode(key: &str) -> bool {
    key == ENV_XDG_DATA_HOME
        || (cfg!(target_os = "linux") && key == ENV_XDG_CONFIG_HOME)
        || ClientId::iter()
            .any(|client| matches!(client.data().root, PathRoot::EnvVar { var, .. } if var == key))
}

fn source_env_uses_nonblank_semantics(key: &str) -> bool {
    key != ENV_XDG_DATA_HOME
        && key != ENV_TOKSCALE_CONFIG_DIR
        && key != ENV_XDG_CONFIG_HOME
        && key != ENV_APPDATA
        && key != ENV_LOCALAPPDATA
}

fn source_env_trims_surrounding_whitespace(key: &str) -> bool {
    key == ENV_COPILOT_EXPORTER || key == ENV_GOOSE_PATH_ROOT || key == ENV_CODEBUFF_DATA_DIR
}

fn fallback_source_env_path(
    key: &str,
    home: &Path,
    platform_config_dir: Option<&Path>,
) -> Result<PathBuf, SourceContextUnavailable> {
    if key == ENV_XDG_DATA_HOME {
        return Ok(home.join(".local/share"));
    }
    if key == ENV_TOKSCALE_CONFIG_DIR {
        if cfg!(target_os = "macos") {
            return Ok(home.join(".config/tokscale"));
        }
        return Ok(platform_config_dir
            .map(|root| root.join("tokscale"))
            .unwrap_or_else(|| home.join(".config/tokscale")));
    }
    if key == ENV_XDG_CONFIG_HOME {
        return Ok(home.join(".config"));
    }
    if key == ENV_TOKSCALE_HEADLESS_DIR {
        return Ok(home.join(".config/tokscale/headless"));
    }
    if key == ENV_COPILOT_EXPORTER {
        return Ok(home.join(".copilot/otel"));
    }
    if key == ENV_LOCALAPPDATA {
        return Ok(home.join("AppData/Local"));
    }
    if key == ENV_APPDATA {
        return Ok(home.join("AppData/Roaming"));
    }
    if key == ENV_KIMI_CODE_HOME {
        return Ok(home.join(".kimi-code"));
    }
    if key == ENV_TOKSCALE_EXTRA_DIRS {
        return Ok(home.to_path_buf());
    }
    if key == ENV_GOOSE_PATH_ROOT {
        return Ok(home.join(".local/share/goose"));
    }
    for client in ClientId::iter() {
        if let PathRoot::EnvVar {
            var,
            fallback_relative,
        } = client.data().root
        {
            if var == key {
                return Ok(home.join(fallback_relative));
            }
        }
    }
    Ok(home.to_path_buf())
}

fn platform_config_root(
    inputs: &SourceResolutionInputs,
    cwd: &Path,
) -> Result<Option<PathBuf>, SourceContextUnavailable> {
    inputs
        .config_dir
        .as_deref()
        .map(|path| fully_qualified(cwd, path))
        .transpose()
}

fn platform_data_local_root(
    inputs: &SourceResolutionInputs,
    cwd: &Path,
) -> Result<Option<PathBuf>, SourceContextUnavailable> {
    inputs
        .data_local_dir
        .as_deref()
        .map(|path| fully_qualified(cwd, path))
        .transpose()
}

fn resolve_extra_scan_paths(
    cwd: &Path,
    use_env_roots: bool,
    inputs: &SourceResolutionInputs,
) -> Result<Vec<(ClientId, PathBuf)>, SourceContextUnavailable> {
    if !use_env_roots {
        return Ok(Vec::new());
    }
    let Some(value) = inputs.var_string(ENV_TOKSCALE_EXTRA_DIRS) else {
        return Ok(Vec::new());
    };
    let enabled = ClientId::iter().collect();
    crate::scanner::parse_extra_dirs(&value, &enabled)
        .into_iter()
        .map(|(client, path)| fully_qualified(cwd, Path::new(&path)).map(|path| (client, path)))
        .collect()
}

fn resolve_scanner_settings(
    cwd: &Path,
    mut settings: ScannerSettings,
) -> Result<ScannerSettings, SourceContextUnavailable> {
    settings.opencode_db_paths = settings
        .opencode_db_paths
        .into_iter()
        .map(|path| fully_qualified(cwd, &path))
        .collect::<Result<_, _>>()?;
    for paths in settings.extra_scan_paths.values_mut() {
        *paths = std::mem::take(paths)
            .into_iter()
            .filter(|path| !path.as_os_str().is_empty())
            .map(|path| fully_qualified(cwd, &path))
            .collect::<Result<_, _>>()?;
    }
    settings
        .extra_scan_paths
        .retain(|_, paths| !paths.is_empty());
    Ok(settings)
}

fn resolve_source_cache_dir(
    cwd: &Path,
    home: &Path,
    inputs: &SourceResolutionInputs,
) -> Result<Option<PathBuf>, SourceContextUnavailable> {
    let config = if let Some(custom) = inputs.var_nonempty(ENV_TOKSCALE_CONFIG_DIR) {
        Some(PathBuf::from(custom))
    } else if cfg!(target_os = "macos") {
        Some(home.join(".config/tokscale"))
    } else {
        inputs.config_dir.clone().map(|path| path.join("tokscale"))
    };
    if let Some(config) = config {
        return fully_qualified(cwd, &config.join("cache")).map(Some);
    }
    if let Some(runtime) = inputs.var_os(ENV_XDG_RUNTIME_DIR) {
        return fully_qualified(cwd, &PathBuf::from(runtime).join("tokscale")).map(Some);
    }
    #[cfg(unix)]
    {
        let uid = unsafe { libc::geteuid() };
        fully_qualified(cwd, &inputs.temp_dir.join(format!("tokscale-uid-{uid}"))).map(Some)
    }
    #[cfg(not(unix))]
    {
        // Windows normally resolves a configured cache root above. Fail closed
        // instead of reading USERNAME/USER after the immutable capture phase.
        Ok(None)
    }
}

#[cfg(not(windows))]
fn fully_qualified(base: &Path, path: &Path) -> Result<PathBuf, SourceContextUnavailable> {
    Ok(if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    })
}

#[cfg(windows)]
fn fully_qualified(base: &Path, path: &Path) -> Result<PathBuf, SourceContextUnavailable> {
    use std::path::Prefix;

    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }
    match path.components().next() {
        Some(Component::Prefix(prefix)) => match prefix.kind() {
            Prefix::Disk(drive) | Prefix::VerbatimDisk(drive) => {
                let Some(Component::Prefix(base_prefix)) = base.components().next() else {
                    return Err(SourceContextUnavailable);
                };
                let base_drive = match base_prefix.kind() {
                    Prefix::Disk(base_drive) | Prefix::VerbatimDisk(base_drive) => base_drive,
                    _ => return Err(SourceContextUnavailable),
                };
                if !drive.eq_ignore_ascii_case(&base_drive) {
                    return Err(SourceContextUnavailable);
                }
                let tail = path.components().skip(1).collect::<PathBuf>();
                Ok(base.join(tail))
            }
            _ => Err(SourceContextUnavailable),
        },
        Some(Component::RootDir) => {
            let Some(Component::Prefix(prefix)) = base.components().next() else {
                return Err(SourceContextUnavailable);
            };
            let mut result = PathBuf::from(prefix.as_os_str());
            result.push(path);
            Ok(result)
        }
        _ => Ok(base.join(path)),
    }
}

fn target_os_tag() -> u8 {
    if cfg!(target_os = "windows") {
        1
    } else if cfg!(target_os = "macos") {
        2
    } else if cfg!(target_os = "linux") {
        3
    } else {
        255
    }
}

#[derive(Default)]
struct Descriptor(Vec<u8>);

impl Descriptor {
    fn field(&mut self, value: u8) {
        self.u8(0xf0);
        self.u8(value);
    }

    fn u8(&mut self, value: u8) {
        self.0.push(value);
    }

    fn bool(&mut self, value: bool) {
        self.u8(u8::from(value));
    }

    fn u32(&mut self, value: u32) {
        self.0.extend_from_slice(&value.to_be_bytes());
    }

    fn count(&mut self, value: usize) -> Result<(), SourceContextUnavailable> {
        self.u32(u32::try_from(value).map_err(|_| SourceContextUnavailable)?);
        Ok(())
    }

    fn bytes(&mut self, value: &[u8]) -> Result<(), SourceContextUnavailable> {
        self.count(value.len())?;
        self.0.extend_from_slice(value);
        Ok(())
    }

    fn text(&mut self, value: &str) -> Result<(), SourceContextUnavailable> {
        self.bytes(value.as_bytes())
    }

    fn optional_path(&mut self, value: Option<&Path>) -> Result<(), SourceContextUnavailable> {
        self.bool(value.is_some());
        if let Some(value) = value {
            self.path(value)?;
        }
        Ok(())
    }

    fn path(&mut self, value: &Path) -> Result<(), SourceContextUnavailable> {
        self.u8(1);
        self.os(value.as_os_str())
    }

    #[cfg(unix)]
    fn os(&mut self, value: &OsStr) -> Result<(), SourceContextUnavailable> {
        use std::os::unix::ffi::OsStrExt;
        self.u8(1);
        self.bytes(value.as_bytes())
    }

    #[cfg(windows)]
    fn os(&mut self, value: &OsStr) -> Result<(), SourceContextUnavailable> {
        use std::os::windows::ffi::OsStrExt;
        self.u8(2);
        let units = value.encode_wide().collect::<Vec<_>>();
        self.count(units.len())?;
        for unit in units {
            self.0.extend_from_slice(&unit.to_be_bytes());
        }
        Ok(())
    }

    #[cfg(not(any(unix, windows)))]
    fn os(&mut self, value: &OsStr) -> Result<(), SourceContextUnavailable> {
        self.u8(3);
        self.bytes(value.to_string_lossy().as_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    use std::fs;
    use tempfile::TempDir;

    fn fixture_platform(root: &Path) -> PlatformInputs {
        PlatformInputs {
            config_dir: Some(root.join("platform-config")),
            data_local_dir: Some(root.join("platform-data")),
            home_dir: Some(root.join("platform-home")),
            temp_dir: root.join("tmp"),
        }
    }

    fn fixture_inputs(
        root: &Path,
        values: impl IntoIterator<Item = (&'static str, OsString)>,
    ) -> SourceResolutionInputs {
        fixture_inputs_with_platform(values, fixture_platform(root))
    }

    fn fixture_inputs_with_platform(
        values: impl IntoIterator<Item = (&'static str, OsString)>,
        platform: PlatformInputs,
    ) -> SourceResolutionInputs {
        let values = values.into_iter().collect::<BTreeMap<_, _>>();
        SourceResolutionInputs::capture_with(|key| values.get(key).cloned(), platform)
    }

    fn fixture_root() -> PathBuf {
        if cfg!(windows) {
            PathBuf::from(r"C:\fixture")
        } else {
            PathBuf::from("/fixture")
        }
    }

    fn fixture_path(relative: &str) -> PathBuf {
        fixture_root().join(relative)
    }

    struct EnvGuard(Vec<(&'static str, Option<OsString>)>);

    impl EnvGuard {
        fn capture(keys: &[&'static str]) -> Self {
            Self(
                keys.iter()
                    .map(|key| (*key, std::env::var_os(key)))
                    .collect(),
            )
        }

        fn set(&self, key: &'static str, value: impl AsRef<OsStr>) {
            unsafe { std::env::set_var(key, value) };
        }

        #[cfg(windows)]
        fn remove(&self, key: &'static str) {
            unsafe { std::env::remove_var(key) };
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            unsafe {
                for (key, value) in self.0.drain(..) {
                    match value {
                        Some(value) => std::env::set_var(key, value),
                        None => std::env::remove_var(key),
                    }
                }
            }
        }
    }

    struct CwdGuard(PathBuf);

    impl CwdGuard {
        fn change(path: &Path) -> Self {
            let old = std::env::current_dir().unwrap();
            std::env::set_current_dir(path).unwrap();
            Self(old)
        }
    }

    impl Drop for CwdGuard {
        fn drop(&mut self) {
            std::env::set_current_dir(&self.0).unwrap();
        }
    }

    #[test]
    fn path_qualification_preserves_parent_components() {
        let base = fixture_path("base");
        let relative = Path::new("a/../b/./c");
        assert_eq!(
            fully_qualified(&base, relative).unwrap(),
            base.join(relative)
        );
    }

    #[cfg(unix)]
    #[test]
    fn source_paths_preserve_symlink_parent_semantics() {
        use std::os::unix::fs::symlink;

        let root = TempDir::new().unwrap();
        let cwd = root.path().join("cwd");
        let target = root.path().join("target");
        let target_inner = target.join("inner");
        let target_codex = target.join("codex");
        let lexical_codex = cwd.join("codex");
        fs::create_dir_all(&cwd).unwrap();
        fs::create_dir_all(&target_inner).unwrap();
        fs::create_dir_all(&target_codex).unwrap();
        fs::create_dir_all(&lexical_codex).unwrap();
        symlink(&target_inner, cwd.join("link")).unwrap();

        let configured = cwd.join("link/../codex");
        let context = ResolvedLocalSourceContext::capture_resolved(
            cwd,
            Some(root.path().join("home")),
            true,
            ScannerSettings::default(),
            fixture_inputs(
                root.path(),
                [("CODEX_HOME", configured.clone().into_os_string())],
            ),
        )
        .unwrap();
        let resolved = context.source_env_path("CODEX_HOME").unwrap();

        assert_eq!(resolved, configured);
        assert_eq!(
            fs::canonicalize(resolved).unwrap(),
            fs::canonicalize(target_codex).unwrap()
        );
        assert_ne!(
            fs::canonicalize(resolved).unwrap(),
            fs::canonicalize(lexical_codex).unwrap()
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_generic_config_root_uses_captured_home() {
        let root = fixture_root();
        let home = fixture_path("home");
        let context = ResolvedLocalSourceContext::capture_resolved(
            fixture_path("cwd"),
            Some(home.clone()),
            true,
            ScannerSettings::default(),
            fixture_inputs(&root, []),
        )
        .unwrap();

        assert_eq!(
            context.resolve_client_root(PathRoot::Config).unwrap(),
            home.join(".config/tokscale")
        );
        assert_ne!(
            context.resolve_client_root(PathRoot::Config).unwrap(),
            root.join("platform-config/tokscale")
        );
        assert_eq!(
            context.source_cache_dir(),
            Some(home.join(".config/tokscale/cache").as_path())
        );
    }

    #[test]
    fn missing_platform_config_dir_falls_back_to_captured_home() {
        let root = fixture_root();
        let home = fixture_path("home");
        let mut platform = fixture_platform(&root);
        platform.config_dir = None;
        let context = ResolvedLocalSourceContext::capture_resolved(
            fixture_path("cwd"),
            Some(home.clone()),
            true,
            ScannerSettings::default(),
            fixture_inputs_with_platform([], platform),
        )
        .unwrap();

        assert_eq!(
            context.resolve_client_root(PathRoot::Config).unwrap(),
            home.join(".config/tokscale")
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn captured_runtime_fallback_cache_root_changes_identity_without_platform_config() {
        let root = fixture_root();
        let mut platform = fixture_platform(&root);
        platform.config_dir = None;
        let runtime_a = fixture_path("runtime-a");
        let runtime_b = fixture_path("runtime-b");
        let capture = |runtime: &Path| {
            ResolvedLocalSourceContext::capture_resolved(
                fixture_path("cwd"),
                Some(fixture_path("home")),
                false,
                ScannerSettings::default(),
                fixture_inputs_with_platform(
                    [(ENV_XDG_RUNTIME_DIR, runtime.as_os_str().to_os_string())],
                    platform.clone(),
                ),
            )
            .unwrap()
        };

        let first = capture(&runtime_a);
        let second = capture(&runtime_b);

        assert_eq!(
            first.source_cache_dir(),
            Some(runtime_a.join("tokscale").as_path())
        );
        assert_eq!(
            second.source_cache_dir(),
            Some(runtime_b.join("tokscale").as_path())
        );
        assert_ne!(first.identity_bytes(), second.identity_bytes());
    }

    #[test]
    fn explicit_tokscale_config_dir_wins_over_platform_and_home_fallbacks() {
        let root = fixture_root();
        let explicit = fixture_path("explicit-config");
        let context = ResolvedLocalSourceContext::capture_resolved(
            fixture_path("cwd"),
            Some(fixture_path("home")),
            true,
            ScannerSettings::default(),
            fixture_inputs(
                &root,
                [(ENV_TOKSCALE_CONFIG_DIR, explicit.clone().into_os_string())],
            ),
        )
        .unwrap();

        assert_eq!(
            context.resolve_client_root(PathRoot::Config).unwrap(),
            explicit
        );
    }

    #[test]
    fn captured_cache_root_changes_identity_when_env_roots_are_disabled() {
        let root = fixture_root();
        let cwd = fixture_path("cwd");
        let home = fixture_path("home");
        let cache_a = fixture_path("cache-config-a");
        let cache_b = fixture_path("cache-config-b");
        let capture = |cache_config: &Path| {
            ResolvedLocalSourceContext::capture_resolved(
                cwd.clone(),
                Some(home.clone()),
                false,
                ScannerSettings::default(),
                fixture_inputs(
                    &root,
                    [(
                        ENV_TOKSCALE_CONFIG_DIR,
                        cache_config.as_os_str().to_os_string(),
                    )],
                ),
            )
            .unwrap()
        };

        let first = capture(&cache_a);
        let second = capture(&cache_b);

        assert_eq!(
            first.source_cache_dir(),
            Some(cache_a.join("cache").as_path())
        );
        assert_eq!(
            second.source_cache_dir(),
            Some(cache_b.join("cache").as_path())
        );
        assert_eq!(first.source_env_path(ENV_TOKSCALE_CONFIG_DIR), None);
        assert_eq!(second.source_env_path(ENV_TOKSCALE_CONFIG_DIR), None);
        for client in ClientId::iter() {
            assert_eq!(
                first.resolve_client_path(client).unwrap(),
                second.resolve_client_path(client).unwrap(),
                "{}",
                client.as_str()
            );
        }
        assert_eq!(
            first.scanner_settings().opencode_db_paths,
            second.scanner_settings().opencode_db_paths
        );
        assert_eq!(
            first.scanner_settings().extra_scan_paths,
            second.scanner_settings().extra_scan_paths
        );
        assert_eq!(first.extra_scan_paths(), second.extra_scan_paths());
        assert_eq!(first.codex_archive_root(), second.codex_archive_root());
        assert_ne!(first.identity_bytes(), second.identity_bytes());
    }

    #[test]
    fn tokscale_config_dir_preserves_whitespace_only_explicit_values() {
        let root = fixture_root();
        let cwd = fixture_path("cwd");
        let home = fixture_path("home");
        let whitespace = "\u{00a0}";
        let explicit = fully_qualified(&cwd, Path::new(whitespace)).unwrap();
        let context = ResolvedLocalSourceContext::capture_resolved(
            cwd,
            Some(home),
            true,
            ScannerSettings::default(),
            fixture_inputs(
                &root,
                [(ENV_TOKSCALE_CONFIG_DIR, OsString::from(whitespace))],
            ),
        )
        .unwrap();

        assert!(context.source_env_is_explicit(ENV_TOKSCALE_CONFIG_DIR));
        assert_eq!(
            context.resolve_client_root(PathRoot::Config).unwrap(),
            explicit
        );
        assert_eq!(
            context.source_cache_dir(),
            Some(explicit.join("cache").as_path())
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn xdg_config_home_preserves_explicit_empty_root() {
        let root = fixture_root();
        let context = ResolvedLocalSourceContext::capture_resolved(
            fixture_path("cwd"),
            Some(fixture_path("home")),
            true,
            ScannerSettings::default(),
            fixture_inputs(&root, [(ENV_XDG_CONFIG_HOME, OsString::new())]),
        )
        .unwrap();

        assert!(context.source_env_is_explicit(ENV_XDG_CONFIG_HOME));
        assert_eq!(
            context.resolve_client_root(PathRoot::Config).unwrap(),
            PathBuf::from("/tokscale")
        );
    }

    #[test]
    fn xdg_data_home_preserves_whitespace_only_explicit_values() {
        let root = fixture_root();
        let cwd = fixture_path("cwd");
        let home = fixture_path("home");
        let unset = ResolvedLocalSourceContext::capture_resolved(
            cwd.clone(),
            Some(home.clone()),
            true,
            ScannerSettings::default(),
            fixture_inputs(&root, []),
        )
        .unwrap();
        let empty = ResolvedLocalSourceContext::capture_resolved(
            cwd.clone(),
            Some(home.clone()),
            true,
            ScannerSettings::default(),
            fixture_inputs(&root, [(ENV_XDG_DATA_HOME, OsString::new())]),
        )
        .unwrap();
        let whitespace = ResolvedLocalSourceContext::capture_resolved(
            cwd.clone(),
            Some(home.clone()),
            true,
            ScannerSettings::default(),
            fixture_inputs(&root, [(ENV_XDG_DATA_HOME, OsString::from(" \t "))]),
        )
        .unwrap();

        let fallback = home.join(".local/share");
        assert_eq!(
            unset.resolve_client_root(PathRoot::XdgData).unwrap(),
            fallback
        );
        assert_eq!(
            empty.resolve_client_root(PathRoot::XdgData).unwrap(),
            fallback
        );
        assert!(!unset.source_env_is_explicit(ENV_XDG_DATA_HOME));
        assert!(!empty.source_env_is_explicit(ENV_XDG_DATA_HOME));
        assert!(whitespace.source_env_is_explicit(ENV_XDG_DATA_HOME));
        assert_eq!(
            whitespace.resolve_client_root(PathRoot::XdgData).unwrap(),
            fully_qualified(&cwd, Path::new(" \t ")).unwrap()
        );
    }

    #[test]
    fn codex_archive_root_preserves_dedicated_override_semantics() {
        let root = fixture_root();
        let cwd = fixture_path("cwd");
        let home = fixture_path("home");
        let capture = |value: Option<&str>| {
            ResolvedLocalSourceContext::capture_resolved(
                cwd.clone(),
                Some(home.clone()),
                true,
                ScannerSettings::default(),
                fixture_inputs(
                    &root,
                    value.map(|value| ("CODEX_HOME", OsString::from(value))),
                ),
            )
            .unwrap()
        };

        let unset = capture(None);
        let empty = capture(Some(""));
        let whitespace = capture(Some("\u{00a0}"));
        let other_whitespace = capture(Some(" "));
        let explicit = capture(Some("custom-codex"));
        let main_fallback = home.join(".codex");

        assert_eq!(
            unset.codex_archive_root(),
            main_fallback.join("archived_sessions")
        );
        assert_eq!(
            empty.codex_archive_root(),
            fully_qualified(&cwd, Path::new("/archived_sessions")).unwrap()
        );
        assert_eq!(
            whitespace.codex_archive_root(),
            fully_qualified(&cwd, Path::new("\u{00a0}/archived_sessions")).unwrap()
        );
        assert_eq!(
            explicit.codex_archive_root(),
            cwd.join("custom-codex/archived_sessions")
        );
        assert_eq!(
            empty
                .resolve_client_root(ClientId::Codex.data().root)
                .unwrap(),
            main_fallback
        );
        assert_eq!(
            whitespace
                .resolve_client_root(ClientId::Codex.data().root)
                .unwrap(),
            home.join(".codex")
        );
        assert_ne!(
            whitespace.identity_bytes(),
            other_whitespace.identity_bytes()
        );
    }

    #[test]
    fn direct_scanner_overrides_trim_like_legacy_resolvers() {
        let root = fixture_root();
        let cwd = fixture_path("cwd");
        let home = fixture_path("home");

        for (key, raw) in [
            (ENV_COPILOT_EXPORTER, "  copilot.jsonl  "),
            (ENV_GOOSE_PATH_ROOT, "  goose-root  "),
            (ENV_CODEBUFF_DATA_DIR, "  codebuff-root  "),
        ] {
            let context = ResolvedLocalSourceContext::capture_resolved(
                cwd.clone(),
                Some(home.clone()),
                true,
                ScannerSettings::default(),
                fixture_inputs(&root, [(key, OsString::from(raw))]),
            )
            .unwrap();
            assert!(context.source_env_is_explicit(key));
            assert_eq!(
                context.source_env_path(key),
                Some(
                    fully_qualified(&cwd, Path::new(raw.trim()))
                        .unwrap()
                        .as_path()
                ),
                "{key}"
            );
        }

        for (key, fallback) in [
            (ENV_COPILOT_EXPORTER, home.join(".copilot/otel")),
            (ENV_GOOSE_PATH_ROOT, home.join(".local/share/goose")),
            (ENV_CODEBUFF_DATA_DIR, home.join(".config/manicode")),
        ] {
            let context = ResolvedLocalSourceContext::capture_resolved(
                cwd.clone(),
                Some(home.clone()),
                true,
                ScannerSettings::default(),
                fixture_inputs(&root, [(key, OsString::from(" \t "))]),
            )
            .unwrap();
            assert!(!context.source_env_is_explicit(key));
            assert_eq!(
                context.source_env_path(key),
                Some(fallback.as_path()),
                "{key}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn unicode_string_overrides_reject_non_utf8_native_values() {
        use std::os::unix::ffi::OsStringExt;

        let root = TempDir::new().unwrap();
        let cwd = root.path().join("cwd");
        let home = root.path().join("home");
        fs::create_dir_all(&cwd).unwrap();

        let headless_raw = OsString::from_vec(b"  headless-\xff  ".to_vec());
        let copilot_raw = OsString::from_vec(b"  copilot-\xff  ".to_vec());
        let goose_raw = OsString::from_vec(b"  goose-\xff  ".to_vec());
        let codebuff_raw = OsString::from_vec(b"  codebuff-\xff  ".to_vec());
        let extra_dirs_raw = OsString::from_vec(b"codex=extra-\xff".to_vec());

        let fallback_headless = home.join(".config/tokscale/headless/codex/fallback.jsonl");
        let fallback_copilot = home.join(".copilot/otel/fallback.jsonl");
        let fallback_goose = home.join(".local/share/goose/sessions/sessions.db");
        let fallback_codebuff = home.join(".config/manicode/projects/p/chats/c/chat-messages.json");
        for path in [
            &fallback_headless,
            &fallback_copilot,
            &fallback_goose,
            &fallback_codebuff,
        ] {
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, b"{}\n").unwrap();
        }

        let copilot_decoy = cwd.join("copilot-\u{fffd}");
        let goose_decoy = cwd.join("goose-\u{fffd}/data/sessions/sessions.db");
        let codebuff_decoy = cwd.join("codebuff-\u{fffd}/projects/p/chats/c/chat-messages.json");
        let extra_dirs_decoy = cwd.join("extra-\u{fffd}/sessions/decoy.jsonl");
        for path in [
            &copilot_decoy,
            &goose_decoy,
            &codebuff_decoy,
            &extra_dirs_decoy,
        ] {
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, b"{}\n").unwrap();
        }

        let context = ResolvedLocalSourceContext::capture_resolved(
            cwd.clone(),
            Some(home.clone()),
            true,
            ScannerSettings::default(),
            fixture_inputs(
                root.path(),
                [
                    (ENV_TOKSCALE_HEADLESS_DIR, headless_raw),
                    (ENV_COPILOT_EXPORTER, copilot_raw),
                    (ENV_GOOSE_PATH_ROOT, goose_raw),
                    (ENV_CODEBUFF_DATA_DIR, codebuff_raw),
                    (ENV_TOKSCALE_EXTRA_DIRS, extra_dirs_raw),
                ],
            ),
        )
        .unwrap();
        let unset = ResolvedLocalSourceContext::capture_resolved(
            cwd,
            Some(home.clone()),
            true,
            ScannerSettings::default(),
            fixture_inputs(root.path(), []),
        )
        .unwrap();

        for (key, fallback) in [
            (
                ENV_TOKSCALE_HEADLESS_DIR,
                home.join(".config/tokscale/headless"),
            ),
            (ENV_COPILOT_EXPORTER, home.join(".copilot/otel")),
            (ENV_GOOSE_PATH_ROOT, home.join(".local/share/goose")),
            (ENV_CODEBUFF_DATA_DIR, home.join(".config/manicode")),
        ] {
            assert!(!context.source_env_is_explicit(key), "{key}");
            assert_eq!(
                context.source_env_path(key),
                Some(fallback.as_path()),
                "{key}"
            );
            assert!(
                !context
                    .source_env_path(key)
                    .unwrap()
                    .to_string_lossy()
                    .contains('\u{fffd}'),
                "{key}"
            );
        }

        assert_eq!(
            crate::scanner::headless_roots_with_source_context(&context).unwrap(),
            vec![
                home.join(".config/tokscale/headless"),
                home.join("Library/Application Support/tokscale/headless"),
            ]
        );
        let scan = crate::scanner::scan_all_clients_with_source_context(
            &context,
            &[
                "codex".to_string(),
                "copilot".to_string(),
                "goose".to_string(),
                "codebuff".to_string(),
            ],
        )
        .unwrap();
        assert_eq!(scan.get(ClientId::Codex), &[fallback_headless]);
        assert_eq!(scan.get(ClientId::Copilot), &[fallback_copilot]);
        assert_eq!(scan.goose_db, Some(fallback_goose));
        assert_eq!(scan.get(ClientId::Codebuff), &[fallback_codebuff]);
        assert!(context.extra_scan_paths().is_empty());
        assert!(!context.source_env_is_explicit(ENV_TOKSCALE_EXTRA_DIRS));

        // Legacy std::env::var treats every invalid native value as unavailable,
        // so the rejected payload intentionally does not contribute to identity.
        assert_eq!(context.identity_bytes(), unset.identity_bytes());
    }

    #[cfg(unix)]
    #[test]
    fn unicode_only_env_roots_reject_non_utf8_native_values() {
        use std::os::unix::ffi::OsStringExt;

        let root = TempDir::new().unwrap();
        let cwd = root.path().join("cwd");
        let home = root.path().join("home");
        let invalid = OsString::from_vec(b"invalid-\xff".to_vec());
        let unset = ResolvedLocalSourceContext::capture_resolved(
            cwd.clone(),
            Some(home.clone()),
            true,
            ScannerSettings::default(),
            fixture_inputs(root.path(), []),
        )
        .unwrap();

        let mut keys = resolver_environment_keys()
            .into_iter()
            .filter(|key| source_env_requires_unicode(key))
            .collect::<Vec<_>>();
        keys.sort_unstable();
        keys.dedup();
        for key in keys {
            let invalid_context = ResolvedLocalSourceContext::capture_resolved(
                cwd.clone(),
                Some(home.clone()),
                true,
                ScannerSettings::default(),
                fixture_inputs(root.path(), [(key, invalid.clone())]),
            )
            .unwrap();

            assert!(!invalid_context.source_env_is_explicit(key), "{key}");
            assert_eq!(
                invalid_context.source_env_path(key),
                unset.source_env_path(key),
                "{key}"
            );
            assert_eq!(
                invalid_context.identity_bytes(),
                unset.identity_bytes(),
                "{key}"
            );
            assert!(
                !invalid_context
                    .source_env_path(key)
                    .unwrap()
                    .to_string_lossy()
                    .contains('\u{fffd}'),
                "{key}"
            );
        }
    }

    #[cfg(windows)]
    #[test]
    fn unicode_string_overrides_reject_unpaired_utf16_values() {
        use std::os::windows::ffi::OsStringExt;

        let root = fixture_root();
        let cwd = fixture_path("cwd");
        let home = fixture_path("home");
        let invalid = OsString::from_wide(&[0x0020, 0xd800, 0x0020]);
        let mut extra_dirs_units = "codex=extra-".encode_utf16().collect::<Vec<_>>();
        extra_dirs_units.push(0xd800);
        let invalid_extra_dirs = OsString::from_wide(&extra_dirs_units);
        let context = ResolvedLocalSourceContext::capture_resolved(
            cwd.clone(),
            Some(home.clone()),
            true,
            ScannerSettings::default(),
            fixture_inputs(
                &root,
                [
                    (ENV_TOKSCALE_HEADLESS_DIR, invalid.clone()),
                    (ENV_COPILOT_EXPORTER, invalid.clone()),
                    (ENV_GOOSE_PATH_ROOT, invalid.clone()),
                    (ENV_CODEBUFF_DATA_DIR, invalid.clone()),
                    (ENV_TOKSCALE_EXTRA_DIRS, invalid_extra_dirs),
                ],
            ),
        )
        .unwrap();
        let unset = ResolvedLocalSourceContext::capture_resolved(
            cwd,
            Some(home.clone()),
            true,
            ScannerSettings::default(),
            fixture_inputs(&root, []),
        )
        .unwrap();

        for (key, fallback) in [
            (
                ENV_TOKSCALE_HEADLESS_DIR,
                home.join(".config/tokscale/headless"),
            ),
            (ENV_COPILOT_EXPORTER, home.join(".copilot/otel")),
            (ENV_GOOSE_PATH_ROOT, home.join(".local/share/goose")),
            (ENV_CODEBUFF_DATA_DIR, home.join(".config/manicode")),
        ] {
            assert!(!context.source_env_is_explicit(key), "{key}");
            assert_eq!(
                context.source_env_path(key),
                Some(fallback.as_path()),
                "{key}"
            );
        }
        assert!(context.extra_scan_paths().is_empty());
        assert!(!context.source_env_is_explicit(ENV_TOKSCALE_EXTRA_DIRS));
        assert_eq!(context.identity_bytes(), unset.identity_bytes());
    }

    #[cfg(windows)]
    #[test]
    fn unicode_only_env_roots_reject_unpaired_utf16_values() {
        use std::os::windows::ffi::OsStringExt;

        let root = fixture_root();
        let cwd = fixture_path("cwd");
        let home = fixture_path("home");
        let invalid = OsString::from_wide(&[
            0x0069, 0x006e, 0x0076, 0x0061, 0x006c, 0x0069, 0x0064, 0xd800,
        ]);
        let unset = ResolvedLocalSourceContext::capture_resolved(
            cwd.clone(),
            Some(home.clone()),
            true,
            ScannerSettings::default(),
            fixture_inputs(&root, []),
        )
        .unwrap();

        let mut keys = resolver_environment_keys()
            .into_iter()
            .filter(|key| source_env_requires_unicode(key))
            .collect::<Vec<_>>();
        keys.sort_unstable();
        keys.dedup();
        for key in keys {
            let invalid_context = ResolvedLocalSourceContext::capture_resolved(
                cwd.clone(),
                Some(home.clone()),
                true,
                ScannerSettings::default(),
                fixture_inputs(&root, [(key, invalid.clone())]),
            )
            .unwrap();

            assert!(!invalid_context.source_env_is_explicit(key), "{key}");
            assert_eq!(
                invalid_context.source_env_path(key),
                unset.source_env_path(key),
                "{key}"
            );
            assert_eq!(
                invalid_context.identity_bytes(),
                unset.identity_bytes(),
                "{key}"
            );
            assert!(
                !invalid_context
                    .source_env_path(key)
                    .unwrap()
                    .to_string_lossy()
                    .contains('\u{fffd}'),
                "{key}"
            );
        }
    }

    #[test]
    fn ordinary_env_paths_preserve_surrounding_whitespace() {
        let root = fixture_root();
        let cwd = fixture_path("cwd");
        let raw = "  codex-root  ";
        let context = ResolvedLocalSourceContext::capture_resolved(
            cwd.clone(),
            Some(fixture_path("home")),
            true,
            ScannerSettings::default(),
            fixture_inputs(&root, [("CODEX_HOME", OsString::from(raw))]),
        )
        .unwrap();

        assert!(context.source_env_is_explicit("CODEX_HOME"));
        assert_eq!(
            context.source_env_path("CODEX_HOME"),
            Some(fully_qualified(&cwd, Path::new(raw)).unwrap().as_path())
        );
    }

    #[test]
    #[serial]
    fn relative_configuration_is_bound_to_capture_cwd() {
        let first = TempDir::new().unwrap();
        let second = TempDir::new().unwrap();
        let _guard = CwdGuard::change(first.path());
        let captured_cwd = std::env::current_dir().unwrap();
        let mut settings = ScannerSettings::default();
        settings
            .opencode_db_paths
            .push(PathBuf::from("db/opencode.db"));
        let context =
            ResolvedLocalSourceContext::capture(Some(PathBuf::from("home")), false, settings)
                .unwrap();
        std::env::set_current_dir(second.path()).unwrap();
        assert_eq!(context.home_dir(), captured_cwd.join("home"));
        assert_eq!(
            context.scanner_settings().opencode_db_paths,
            vec![captured_cwd.join("db/opencode.db")]
        );
    }

    #[test]
    fn scanner_setting_empty_paths_are_identity_neutral() {
        let root = fixture_root();
        let cwd = fixture_path("cwd");
        let home = fixture_path("home");
        let capture = |settings| {
            ResolvedLocalSourceContext::capture_resolved(
                cwd.clone(),
                Some(home.clone()),
                false,
                settings,
                fixture_inputs(&root, []),
            )
            .unwrap()
        };

        let default = capture(ScannerSettings::default());
        let empty_only = capture(ScannerSettings {
            extra_scan_paths: BTreeMap::from([
                ("codex".to_string(), vec![PathBuf::new()]),
                ("gemini".to_string(), Vec::new()),
            ]),
            ..Default::default()
        });

        let codex_root = PathBuf::from("extra/codex");
        let codex_sibling = PathBuf::from("extra/codex-sibling");
        let whitespace = PathBuf::from(" \t ");
        let gemini_root = PathBuf::from("extra/gemini");
        let normalized = capture(ScannerSettings {
            extra_scan_paths: BTreeMap::from([
                (
                    "codex".to_string(),
                    vec![
                        codex_root.clone(),
                        codex_sibling.clone(),
                        whitespace.clone(),
                    ],
                ),
                ("gemini".to_string(), vec![gemini_root.clone()]),
            ]),
            ..Default::default()
        });
        let mixed = capture(ScannerSettings {
            extra_scan_paths: BTreeMap::from([
                (
                    "codex".to_string(),
                    vec![
                        PathBuf::new(),
                        codex_root.clone(),
                        PathBuf::new(),
                        codex_sibling.clone(),
                        whitespace.clone(),
                    ],
                ),
                (
                    "gemini".to_string(),
                    vec![PathBuf::new(), gemini_root.clone(), PathBuf::new()],
                ),
                ("claude".to_string(), vec![PathBuf::new()]),
            ]),
            ..Default::default()
        });

        assert!(empty_only.scanner_settings().extra_scan_paths.is_empty());
        assert_eq!(
            default.scanner_settings().extra_scan_paths,
            empty_only.scanner_settings().extra_scan_paths
        );
        assert_eq!(default.identity_bytes(), empty_only.identity_bytes());
        assert_eq!(
            normalized.scanner_settings().extra_scan_paths,
            mixed.scanner_settings().extra_scan_paths
        );
        assert_eq!(normalized.identity_bytes(), mixed.identity_bytes());
        assert_eq!(
            normalized.scanner_settings().extra_scan_paths["codex"],
            vec![
                cwd.join(codex_root),
                cwd.join(codex_sibling),
                cwd.join(whitespace),
            ]
        );
        assert_eq!(
            normalized.scanner_settings().extra_scan_paths["gemini"],
            vec![cwd.join(gemini_root)]
        );
        assert!(!mixed
            .scanner_settings()
            .extra_scan_paths
            .contains_key("claude"));
    }

    #[test]
    fn context_scanner_does_not_expand_empty_extra_path_to_capture_cwd() {
        let root = TempDir::new().unwrap();
        let cwd = root.path().join("capture-cwd");
        let home = root.path().join("home");
        let intended_relative = PathBuf::from("configured-codex");
        let intended_root = cwd.join(&intended_relative);
        let unrelated = cwd.join("unrelated.jsonl");
        let intended = intended_root.join("intended.jsonl");
        fs::create_dir_all(&intended_root).unwrap();
        fs::write(&unrelated, b"{}\n").unwrap();
        fs::write(&intended, b"{}\n").unwrap();

        let capture = |paths: Vec<PathBuf>| {
            ResolvedLocalSourceContext::capture_resolved(
                cwd.clone(),
                Some(home.clone()),
                false,
                ScannerSettings {
                    extra_scan_paths: BTreeMap::from([("codex".to_string(), paths)]),
                    ..Default::default()
                },
                fixture_inputs(root.path(), []),
            )
            .unwrap()
        };
        let mixed = capture(vec![PathBuf::new(), intended_relative.clone()]);
        let normalized = capture(vec![intended_relative]);
        let clients = ["codex".to_string()];

        let mixed_scan =
            crate::scanner::scan_all_clients_with_source_context(&mixed, &clients).unwrap();
        let normalized_scan =
            crate::scanner::scan_all_clients_with_source_context(&normalized, &clients).unwrap();
        let mixed_files = mixed_scan.get(ClientId::Codex);

        assert_eq!(mixed_files, normalized_scan.get(ClientId::Codex));
        assert_eq!(mixed_files, &vec![intended]);
        assert!(!mixed_files.contains(&unrelated));
    }

    #[test]
    #[serial]
    fn identity_is_configuration_only() {
        let root = TempDir::new().unwrap();
        let home = root.path().join("home");
        fs::create_dir_all(home.join(".codex/sessions")).unwrap();
        let _guard = CwdGuard::change(root.path());
        let context = ResolvedLocalSourceContext::capture(
            Some(home.clone()),
            false,
            ScannerSettings::default(),
        )
        .unwrap();
        let before = context.identity_string();
        fs::write(home.join(".codex/sessions/session.jsonl"), b"content").unwrap();
        assert_eq!(context.identity_string(), before);
        assert_eq!(before.len(), 68);
        assert!(before[4..]
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()));
    }

    #[test]
    fn cache_content_and_mtime_are_excluded_from_source_identity() {
        let root = TempDir::new().unwrap();
        let cache_config = root.path().join("cache-config");
        let cache_root = cache_config.join("cache");
        let marker = cache_root.join("state.bin");
        fs::create_dir_all(&cache_root).unwrap();
        fs::write(&marker, b"first").unwrap();
        let set_mtime =
            |seconds| {
                fs::File::options()
                    .write(true)
                    .open(&marker)
                    .unwrap()
                    .set_times(fs::FileTimes::new().set_modified(
                        std::time::UNIX_EPOCH + std::time::Duration::from_secs(seconds),
                    ))
                    .unwrap();
            };
        let capture = || {
            ResolvedLocalSourceContext::capture_resolved(
                root.path().join("cwd"),
                Some(root.path().join("home")),
                false,
                ScannerSettings::default(),
                fixture_inputs(
                    root.path(),
                    [(
                        ENV_TOKSCALE_CONFIG_DIR,
                        cache_config.as_os_str().to_os_string(),
                    )],
                ),
            )
            .unwrap()
        };

        set_mtime(1_700_000_000);
        let first_metadata = fs::metadata(&marker).unwrap();
        let first = capture();
        fs::write(&marker, b"different and longer cache content").unwrap();
        set_mtime(1_800_000_000);
        let second_metadata = fs::metadata(&marker).unwrap();
        let second = capture();

        assert_ne!(first_metadata.len(), second_metadata.len());
        assert_ne!(
            first_metadata.modified().unwrap(),
            second_metadata.modified().unwrap()
        );
        assert_eq!(first.source_cache_dir(), Some(cache_root.as_path()));
        assert_eq!(second.source_cache_dir(), Some(cache_root.as_path()));
        assert_eq!(first.identity_bytes(), second.identity_bytes());
    }

    #[test]
    #[serial]
    fn captured_scanner_ignores_later_env_and_cwd_mutation() {
        let root = TempDir::new().unwrap();
        let cwd_a = root.path().join("cwd-a");
        let cwd_b = root.path().join("cwd-b");
        fs::create_dir_all(&cwd_a).unwrap();
        fs::create_dir_all(&cwd_b).unwrap();
        let env = EnvGuard::capture(&["CODEX_HOME"]);
        let _cwd = CwdGuard::change(&cwd_a);
        let captured_cwd = std::env::current_dir().unwrap();
        env.set("CODEX_HOME", "codex-a");
        fs::create_dir_all(cwd_a.join("codex-a/sessions")).unwrap();
        fs::write(cwd_a.join("codex-a/sessions/a.jsonl"), b"{}\n").unwrap();
        fs::create_dir_all(cwd_b.join("codex-b/sessions")).unwrap();
        fs::write(cwd_b.join("codex-b/sessions/b.jsonl"), b"{}\n").unwrap();

        let context = ResolvedLocalSourceContext::capture(
            Some(root.path().join("home")),
            true,
            ScannerSettings::default(),
        )
        .unwrap();
        let identity = context.identity_string();
        env.set("CODEX_HOME", cwd_b.join("codex-b"));
        std::env::set_current_dir(&cwd_b).unwrap();

        let scan =
            crate::scanner::scan_all_clients_with_source_context(&context, &["codex".to_string()])
                .unwrap();
        assert_eq!(
            scan.get(ClientId::Codex),
            &vec![captured_cwd.join("codex-a/sessions/a.jsonl")]
        );
        assert_eq!(context.identity_string(), identity);
    }

    #[test]
    fn resolver_environment_inventory_covers_every_client_env_root() {
        let keys = resolver_environment_keys();
        for key in crate::scanner::DIRECT_SOURCE_ENV_KEYS {
            assert!(keys.contains(key), "missing scanner input {key}");
        }
        for client in ClientId::iter() {
            if let PathRoot::EnvVar { var, .. } = client.data().root {
                assert!(keys.contains(&var), "missing {var}");
            }
        }
        assert!(!keys.contains(&ENV_TOKSCALE_PRICING_CACHE_ONLY));
        assert!(!keys.contains(&ENV_XDG_RUNTIME_DIR));
    }

    #[test]
    fn unicode_only_env_classification_tracks_every_client_env_root() {
        for client in ClientId::iter() {
            if let PathRoot::EnvVar { var, .. } = client.data().root {
                assert!(source_env_requires_unicode(var), "missing {var}");
            }
        }
        assert!(source_env_requires_unicode(ENV_XDG_DATA_HOME));
        assert_eq!(
            source_env_requires_unicode(ENV_XDG_CONFIG_HOME),
            cfg!(target_os = "linux")
        );
        assert!(!source_env_requires_unicode(ENV_APPDATA));
        assert!(!source_env_requires_unicode(ENV_LOCALAPPDATA));
    }

    #[test]
    fn descriptor_has_fixed_sha256_and_native_path_vectors() {
        let root = fixture_root();
        let inputs = fixture_inputs(
            &root,
            [
                (ENV_HOME, fixture_path("home").into_os_string()),
                ("CODEX_HOME", OsString::from("relative-codex")),
            ],
        );
        let mut settings = ScannerSettings::default();
        settings
            .opencode_db_paths
            .push(PathBuf::from("db/opencode.db"));
        let context = ResolvedLocalSourceContext::capture_resolved(
            fixture_path("cwd"),
            None,
            true,
            settings,
            inputs,
        )
        .unwrap();
        let identity = context.identity_string();
        assert_eq!(identity.len(), 68);
        assert!(identity.strip_prefix("sc1:").is_some_and(|hex| hex
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))));
        #[cfg(unix)]
        assert_eq!(
            identity,
            "sc1:f980f9db19f3217fcc6059d8e5f1253b50307020c97e8584f2da357dce7d3424"
        );

        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStringExt;
            let mut descriptor = Descriptor::default();
            descriptor
                .path(Path::new(&OsString::from_vec(vec![b'/', b'x', 0xff])))
                .unwrap();
            assert_eq!(descriptor.0, vec![1, 1, 0, 0, 0, 3, b'/', b'x', 0xff]);
        }
    }

    #[cfg(windows)]
    #[test]
    fn windows_appdata_and_localappdata_preserve_native_state_matrix() {
        use std::os::windows::ffi::OsStringExt;

        let root = fixture_root();
        let cwd = fixture_path("cwd");
        let home = fixture_path("home");

        for (key, fallback, absolute_name) in [
            (ENV_APPDATA, home.join("AppData/Roaming"), "appdata-root"),
            (
                ENV_LOCALAPPDATA,
                home.join("AppData/Local"),
                "localappdata-root",
            ),
        ] {
            let capture = |value: Option<OsString>, use_env_roots| {
                ResolvedLocalSourceContext::capture_resolved(
                    cwd.clone(),
                    Some(home.clone()),
                    use_env_roots,
                    ScannerSettings::default(),
                    fixture_inputs(&root, value.map(|value| (key, value))),
                )
                .unwrap()
            };
            let whitespace_raw = OsString::from(" \t ");
            let relative_raw = OsString::from("relative-root");
            let absolute = fixture_path(absolute_name);
            let invalid_raw = OsString::from_wide(&[
                0x006e, 0x0061, 0x0074, 0x0069, 0x0076, 0x0065, 0x002d, 0xd800,
            ]);

            let unset = capture(None, true);
            let empty = capture(Some(OsString::new()), true);
            let whitespace = capture(Some(whitespace_raw.clone()), true);
            let relative = capture(Some(relative_raw.clone()), true);
            let absolute_context = capture(Some(absolute.clone().into_os_string()), true);
            let invalid = capture(Some(invalid_raw.clone()), true);
            let disabled_unset = capture(None, false);
            let disabled_invalid = capture(Some(invalid_raw.clone()), false);

            assert_eq!(unset.source_env_path(key), Some(fallback.as_path()));
            assert_eq!(empty.source_env_path(key), Some(fallback.as_path()));
            assert!(!unset.source_env_is_explicit(key));
            assert!(!empty.source_env_is_explicit(key));
            assert_ne!(unset.identity_bytes(), empty.identity_bytes());

            for context in [&whitespace, &relative, &absolute_context, &invalid] {
                assert!(context.source_env_is_explicit(key));
            }
            assert_eq!(
                whitespace.source_env_path(key),
                Some(
                    fully_qualified(&cwd, Path::new(&whitespace_raw))
                        .unwrap()
                        .as_path()
                )
            );
            assert_eq!(
                relative.source_env_path(key),
                Some(
                    fully_qualified(&cwd, Path::new(&relative_raw))
                        .unwrap()
                        .as_path()
                )
            );
            assert_eq!(
                absolute_context.source_env_path(key),
                Some(absolute.as_path())
            );
            assert_eq!(
                invalid.source_env_path(key),
                Some(
                    fully_qualified(&cwd, Path::new(&invalid_raw))
                        .unwrap()
                        .as_path()
                )
            );
            assert_eq!(
                std::collections::BTreeSet::from([
                    whitespace.identity_bytes(),
                    relative.identity_bytes(),
                    absolute_context.identity_bytes(),
                    invalid.identity_bytes(),
                ])
                .len(),
                4
            );

            assert_eq!(disabled_unset.source_env_path(key), None);
            assert_eq!(disabled_invalid.source_env_path(key), None);
            assert!(!disabled_invalid.source_env_is_explicit(key));
            assert_eq!(
                disabled_unset.identity_bytes(),
                disabled_invalid.identity_bytes()
            );
        }
    }

    #[cfg(windows)]
    #[test]
    #[serial]
    fn windows_cline_appdata_context_matches_legacy_and_preserves_wtf16() {
        use std::os::windows::ffi::OsStringExt;

        let root = TempDir::new().unwrap();
        let cwd = root.path().join("cwd");
        let home = root.path().join("home");
        let appdata = root.path().join("appdata");
        let relative_tasks = Path::new(
            "Code/User/globalStorage/saoudrizwan.claude-dev/tasks/task-valid/ui_messages.json",
        );
        let valid_file = appdata.join(relative_tasks);
        fs::create_dir_all(&cwd).unwrap();
        fs::create_dir_all(valid_file.parent().unwrap()).unwrap();
        fs::write(&valid_file, b"[]").unwrap();

        let env = EnvGuard::capture(&[ENV_APPDATA, ENV_TOKSCALE_EXTRA_DIRS]);
        env.set(ENV_APPDATA, &appdata);
        env.remove(ENV_TOKSCALE_EXTRA_DIRS);
        let legacy = crate::scanner::scan_all_clients_with_env_strategy(
            home.to_str().unwrap(),
            &["cline".to_string()],
            true,
        );
        let valid_context = ResolvedLocalSourceContext::capture_resolved(
            cwd.clone(),
            Some(home.clone()),
            true,
            ScannerSettings::default(),
            fixture_inputs(
                root.path(),
                [(ENV_APPDATA, appdata.clone().into_os_string())],
            ),
        )
        .unwrap();
        let resolved = crate::scanner::scan_all_clients_with_source_context(
            &valid_context,
            &["cline".to_string()],
        )
        .unwrap();
        assert_eq!(resolved.get(ClientId::Cline), legacy.get(ClientId::Cline));
        assert_eq!(resolved.get(ClientId::Cline), &[valid_file]);

        let mut native_name = "native-appdata-".encode_utf16().collect::<Vec<_>>();
        native_name.push(0xd800);
        let native_root = root.path().join(OsString::from_wide(&native_name));
        let decoy_root = root.path().join("native-appdata-\u{fffd}");
        let native_file = native_root.join(relative_tasks);
        let decoy_file = decoy_root.join(relative_tasks);
        fs::create_dir_all(native_file.parent().unwrap()).unwrap();
        fs::create_dir_all(decoy_file.parent().unwrap()).unwrap();
        fs::write(&native_file, b"[]").unwrap();
        fs::write(&decoy_file, b"[]").unwrap();
        assert!(native_root.to_str().is_none());
        assert!(decoy_root.to_str().is_some());

        env.set(ENV_APPDATA, &decoy_root);
        let invalid_context = ResolvedLocalSourceContext::capture_resolved(
            cwd,
            Some(home),
            true,
            ScannerSettings::default(),
            fixture_inputs(
                root.path(),
                [(ENV_APPDATA, native_root.clone().into_os_string())],
            ),
        )
        .unwrap();
        assert_eq!(
            invalid_context.source_env_path(ENV_APPDATA),
            Some(native_root.as_path())
        );
        let invalid_scan = crate::scanner::scan_all_clients_with_source_context(
            &invalid_context,
            &["cline".to_string()],
        )
        .unwrap();
        assert_eq!(invalid_scan.get(ClientId::Cline), &[native_file]);
        assert!(!invalid_scan.get(ClientId::Cline).contains(&decoy_file));
    }

    #[cfg(windows)]
    #[test]
    fn windows_invalid_hermes_home_keeps_localappdata_discovery() {
        use std::os::windows::ffi::OsStringExt;

        let root = TempDir::new().unwrap();
        let cwd = root.path().join("cwd");
        let home = root.path().join("home");
        let localappdata = root.path().join("localappdata");
        let local_db = localappdata.join("hermes/state.db");
        fs::create_dir_all(&cwd).unwrap();
        fs::create_dir_all(local_db.parent().unwrap()).unwrap();
        fs::write(&local_db, b"").unwrap();

        let mut invalid_name = "invalid-hermes-".encode_utf16().collect::<Vec<_>>();
        invalid_name.push(0xd800);
        let invalid_home = root.path().join(OsString::from_wide(&invalid_name));
        let context = ResolvedLocalSourceContext::capture_resolved(
            cwd,
            Some(home.clone()),
            true,
            ScannerSettings::default(),
            fixture_inputs(
                root.path(),
                [
                    ("HERMES_HOME", invalid_home.into_os_string()),
                    (ENV_LOCALAPPDATA, localappdata.clone().into_os_string()),
                ],
            ),
        )
        .unwrap();

        assert!(!context.source_env_is_explicit("HERMES_HOME"));
        assert_eq!(
            context.source_env_path("HERMES_HOME"),
            Some(home.join(".hermes").as_path())
        );
        assert!(context.source_env_is_explicit(ENV_LOCALAPPDATA));
        let scan =
            crate::scanner::scan_all_clients_with_source_context(&context, &["hermes".to_string()])
                .unwrap();
        assert_eq!(scan.hermes_db, Some(local_db));
    }

    #[cfg(windows)]
    #[test]
    fn windows_paths_use_capture_drive_and_reject_ambiguous_drives() {
        let base = Path::new(r"C:\capture\work");
        assert_eq!(
            fully_qualified(base, Path::new(r"C:profile\sessions")).unwrap(),
            PathBuf::from(r"C:\capture\work\profile\sessions")
        );
        assert_eq!(
            fully_qualified(base, Path::new(r"c:profile\sessions")).unwrap(),
            PathBuf::from(r"C:\capture\work\profile\sessions")
        );
        assert_eq!(
            fully_qualified(base, Path::new(r"\profile\sessions")).unwrap(),
            PathBuf::from(r"C:\profile\sessions")
        );
        assert_eq!(
            fully_qualified(base, Path::new(r"\\server\share\profile\sessions")).unwrap(),
            PathBuf::from(r"\\server\share\profile\sessions")
        );
        assert_eq!(
            fully_qualified(base, Path::new(r"D:profile\sessions")),
            Err(SourceContextUnavailable)
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_descriptor_preserves_wtf16_code_units() {
        use std::os::windows::ffi::OsStringExt;

        let value = OsString::from_wide(&[0x0043, 0x003a, 0x005c, 0xd800, 0x0061]);
        let mut descriptor = Descriptor::default();
        descriptor.os(&value).unwrap();
        assert_eq!(
            descriptor.0,
            vec![2, 0, 0, 0, 5, 0x00, 0x43, 0x00, 0x3a, 0x00, 0x5c, 0xd8, 0x00, 0x00, 0x61,]
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_none_and_some_cache_roots_have_distinct_identity() {
        let root = fixture_root();
        let mut platform = fixture_platform(&root);
        platform.config_dir = None;
        let without_cache = ResolvedLocalSourceContext::capture_resolved(
            fixture_path("cwd"),
            Some(fixture_path("home")),
            false,
            ScannerSettings::default(),
            fixture_inputs_with_platform([], platform.clone()),
        )
        .unwrap();
        let explicit = fixture_path("explicit-cache-config");
        let with_cache = ResolvedLocalSourceContext::capture_resolved(
            fixture_path("cwd"),
            Some(fixture_path("home")),
            false,
            ScannerSettings::default(),
            fixture_inputs_with_platform(
                [(ENV_TOKSCALE_CONFIG_DIR, explicit.as_os_str().to_os_string())],
                platform,
            ),
        )
        .unwrap();

        assert_eq!(without_cache.source_cache_dir(), None);
        assert_eq!(
            with_cache.source_cache_dir(),
            Some(explicit.join("cache").as_path())
        );
        assert_ne!(without_cache.identity_bytes(), with_cache.identity_bytes());
    }

    #[cfg(windows)]
    #[test]
    fn windows_cache_root_identity_preserves_unpaired_utf16() {
        use std::os::windows::ffi::OsStringExt;

        let root = fixture_root();
        let native_root = PathBuf::from(OsString::from_wide(&[0x0043, 0x003a, 0x005c, 0xd800]));
        let replacement_decoy =
            PathBuf::from(OsString::from_wide(&[0x0043, 0x003a, 0x005c, 0xfffd]));
        let capture = |cache_config: &Path| {
            ResolvedLocalSourceContext::capture_resolved(
                fixture_path("cwd"),
                Some(fixture_path("home")),
                false,
                ScannerSettings::default(),
                fixture_inputs(
                    &root,
                    [(
                        ENV_TOKSCALE_CONFIG_DIR,
                        cache_config.as_os_str().to_os_string(),
                    )],
                ),
            )
            .unwrap()
        };

        let native = capture(&native_root);
        let decoy = capture(&replacement_decoy);

        assert_eq!(
            native.source_cache_dir(),
            Some(native_root.join("cache").as_path())
        );
        assert_eq!(
            decoy.source_cache_dir(),
            Some(replacement_decoy.join("cache").as_path())
        );
        assert_ne!(native.identity_bytes(), decoy.identity_bytes());
    }

    #[test]
    fn scanner_configuration_and_extra_dirs_change_source_identity() {
        let root = fixture_root();
        let inputs = fixture_inputs(&root, [(ENV_HOME, fixture_path("home").into_os_string())]);
        let base = ResolvedLocalSourceContext::capture_resolved(
            fixture_path("cwd"),
            None,
            true,
            ScannerSettings::default(),
            inputs.clone(),
        )
        .unwrap();

        let mut scanner_settings = ScannerSettings::default();
        scanner_settings
            .opencode_db_paths
            .push(PathBuf::from("db/opencode.db"));
        scanner_settings
            .extra_scan_paths
            .insert("codex".to_string(), vec![PathBuf::from("extra/codex")]);
        let configured = ResolvedLocalSourceContext::capture_resolved(
            fixture_path("cwd"),
            None,
            true,
            scanner_settings,
            inputs,
        )
        .unwrap();
        assert_ne!(base.identity_bytes(), configured.identity_bytes());

        let extra_dirs = ResolvedLocalSourceContext::capture_resolved(
            fixture_path("cwd"),
            None,
            true,
            ScannerSettings::default(),
            fixture_inputs(
                &root,
                [
                    (ENV_HOME, fixture_path("home").into_os_string()),
                    (
                        ENV_TOKSCALE_EXTRA_DIRS,
                        OsString::from(format!("codex={}", fixture_path("external").display())),
                    ),
                ],
            ),
        )
        .unwrap();
        assert_ne!(base.identity_bytes(), extra_dirs.identity_bytes());
    }

    #[test]
    fn content_and_year_are_excluded_from_source_identity() {
        let root = TempDir::new().unwrap();
        let home = root.path().join("home");
        let session = home.join(".codex/sessions/session.jsonl");
        fs::create_dir_all(session.parent().unwrap()).unwrap();
        let context =
            ResolvedLocalSourceContext::capture(Some(home), false, ScannerSettings::default())
                .unwrap();
        let identity = context.identity_string();

        fs::write(&session, b"first").unwrap();
        let first_metadata = fs::metadata(&session).unwrap();
        assert_eq!(context.identity_string(), identity);
        fs::write(&session, b"longer second content").unwrap();
        assert_ne!(fs::metadata(&session).unwrap().len(), first_metadata.len());
        assert_eq!(context.identity_string(), identity);
        fs::remove_file(&session).unwrap();
        assert_eq!(context.identity_string(), identity);

        let first_year = crate::LocalParseOptions {
            year: Some("2025".to_string()),
            ..Default::default()
        };
        let second_year = crate::LocalParseOptions {
            year: Some("2026".to_string()),
            ..Default::default()
        };
        assert_eq!(context.identity_string(), identity);
        assert_ne!(first_year.year, second_year.year);
    }

    #[test]
    fn pricing_cache_only_and_unused_runtime_fallback_do_not_change_source_identity() {
        let root = fixture_root();
        let base = fixture_inputs(&root, [(ENV_HOME, fixture_path("home").into_os_string())]);
        let changed = fixture_inputs(
            &root,
            [
                (ENV_HOME, fixture_path("home").into_os_string()),
                (ENV_TOKSCALE_PRICING_CACHE_ONLY, OsString::from("1")),
                (
                    ENV_XDG_RUNTIME_DIR,
                    fixture_path("cache-canary").into_os_string(),
                ),
            ],
        );
        let first = ResolvedLocalSourceContext::capture_resolved(
            fixture_path("cwd"),
            None,
            true,
            ScannerSettings::default(),
            base,
        )
        .unwrap();
        let second = ResolvedLocalSourceContext::capture_resolved(
            fixture_path("cwd"),
            None,
            true,
            ScannerSettings::default(),
            changed,
        )
        .unwrap();
        assert_eq!(first.source_cache_dir(), second.source_cache_dir());
        assert_eq!(first.identity_bytes(), second.identity_bytes());
        assert!(!first.pricing_cache_only());
        assert!(second.pricing_cache_only());
    }

    #[test]
    fn ignored_environment_does_not_change_identity_when_env_roots_are_disabled() {
        let root = fixture_root();
        let contexts = [
            fixture_inputs(&root, []),
            fixture_inputs(&root, [("CODEX_HOME", OsString::new())]),
            fixture_inputs(&root, [("CODEX_HOME", OsString::from("profile-a"))]),
            fixture_inputs(&root, [("CODEX_HOME", OsString::from("profile-b"))]),
        ]
        .into_iter()
        .map(|inputs| {
            ResolvedLocalSourceContext::capture_resolved(
                fixture_path("cwd"),
                Some(fixture_path("home")),
                false,
                ScannerSettings::default(),
                inputs,
            )
            .unwrap()
        })
        .collect::<Vec<_>>();
        assert!(contexts
            .windows(2)
            .all(|pair| pair[0].identity_bytes() == pair[1].identity_bytes()));
    }

    #[tokio::test]
    #[serial]
    async fn retained_only_report_is_bound_to_captured_cache_root_identity() {
        let root = TempDir::new().unwrap();
        let cwd = root.path().join("cwd");
        let source_home = root.path().join("source-home");
        let cache_config_a = root.path().join("cache-config-a");
        let cache_config_b = root.path().join("cache-config-b");
        let pricing_sandbox = root.path().join("pricing-sandbox");
        fs::create_dir_all(&cwd).unwrap();
        fs::create_dir_all(&pricing_sandbox).unwrap();

        let claude_dir = source_home.join(".claude/projects/myproject");
        fs::create_dir_all(&claude_dir).unwrap();
        let transcript = claude_dir.join("conversation.jsonl");
        let turn_one = r#"{"type":"assistant","timestamp":"2024-12-01T10:00:00.000Z","requestId":"req_001","message":{"id":"msg_001","model":"claude-3-5-sonnet","usage":{"input_tokens":100,"output_tokens":50}}}"#;
        let turn_two = r#"{"type":"assistant","timestamp":"2024-12-01T10:05:00.000Z","requestId":"req_002","message":{"id":"msg_002","model":"claude-3-5-sonnet","usage":{"input_tokens":200,"output_tokens":60}}}"#;
        fs::write(&transcript, format!("{turn_one}\n{turn_two}\n")).unwrap();

        let env = EnvGuard::capture(&[
            ENV_HOME,
            ENV_TOKSCALE_CONFIG_DIR,
            ENV_XDG_CONFIG_HOME,
            "XDG_CACHE_HOME",
        ]);
        env.set(ENV_HOME, &pricing_sandbox);
        env.set(ENV_TOKSCALE_CONFIG_DIR, &pricing_sandbox);
        env.set(ENV_XDG_CONFIG_HOME, pricing_sandbox.join("xdg-config"));
        env.set("XDG_CACHE_HOME", pricing_sandbox.join("xdg-cache"));

        let capture = |cache_config: &Path| {
            ResolvedLocalSourceContext::capture_resolved(
                cwd.clone(),
                Some(source_home.clone()),
                false,
                ScannerSettings::default(),
                fixture_inputs(
                    root.path(),
                    [
                        (
                            ENV_TOKSCALE_CONFIG_DIR,
                            cache_config.as_os_str().to_os_string(),
                        ),
                        (ENV_TOKSCALE_PRICING_CACHE_ONLY, OsString::from("1")),
                    ],
                ),
            )
            .unwrap()
        };
        let context_a = capture(&cache_config_a);
        let context_b = capture(&cache_config_b);
        let report_options = || crate::ReportOptions {
            clients: Some(vec!["claude".to_string()]),
            ..Default::default()
        };

        assert_eq!(
            context_a.resolve_client_path(ClientId::Claude).unwrap(),
            context_b.resolve_client_path(ClientId::Claude).unwrap()
        );
        assert_eq!(
            context_a.source_cache_dir(),
            Some(cache_config_a.join("cache").as_path())
        );
        assert_eq!(
            context_b.source_cache_dir(),
            Some(cache_config_b.join("cache").as_path())
        );
        assert_ne!(context_a.identity_bytes(), context_b.identity_bytes());

        let seeded_a = crate::get_model_report_with_source_context(&context_a, report_options())
            .await
            .unwrap();
        assert_eq!(seeded_a.total_messages, 2);
        assert_eq!(seeded_a.total_input, 300);
        assert_eq!(seeded_a.total_output, 110);

        fs::write(&transcript, format!("{turn_two}\n")).unwrap();

        let retained_a = crate::get_model_report_with_source_context(&context_a, report_options())
            .await
            .unwrap();
        let cold_b = crate::get_model_report_with_source_context(&context_b, report_options())
            .await
            .unwrap();

        assert_eq!(retained_a.total_messages, 2);
        assert_eq!(retained_a.total_input, 300);
        assert_eq!(retained_a.total_output, 110);
        assert_eq!(cold_b.total_messages, 1);
        assert_eq!(cold_b.total_input, 200);
        assert_eq!(cold_b.total_output, 60);
        assert_ne!(retained_a.total_output, cold_b.total_output);
    }

    #[test]
    fn unset_empty_fallback_and_explicit_states_are_unambiguous() {
        let root = fixture_root();
        let unset = ResolvedLocalSourceContext::capture_resolved(
            fixture_path("cwd"),
            Some(fixture_path("home")),
            true,
            ScannerSettings::default(),
            fixture_inputs(&root, []),
        )
        .unwrap();
        let empty = ResolvedLocalSourceContext::capture_resolved(
            fixture_path("cwd"),
            Some(fixture_path("home")),
            true,
            ScannerSettings::default(),
            fixture_inputs(&root, [("CODEX_HOME", OsString::new())]),
        )
        .unwrap();
        let explicit_fallback = ResolvedLocalSourceContext::capture_resolved(
            fixture_path("cwd"),
            Some(fixture_path("home")),
            true,
            ScannerSettings::default(),
            fixture_inputs(
                &root,
                [("CODEX_HOME", fixture_path("home/.codex").into_os_string())],
            ),
        )
        .unwrap();
        assert_ne!(unset.identity_bytes(), empty.identity_bytes());
        assert_ne!(empty.identity_bytes(), explicit_fallback.identity_bytes());
        assert_ne!(unset.identity_bytes(), explicit_fallback.identity_bytes());
        assert_eq!(
            unset
                .resolve_client_root(ClientId::Codex.data().root)
                .unwrap(),
            explicit_fallback
                .resolve_client_root(ClientId::Codex.data().root)
                .unwrap()
        );
    }
}
