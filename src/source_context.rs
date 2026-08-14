use crate::clients::{ClientId, PathRoot};
use crate::scanner::ScannerSettings;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::path::{Component, Path, PathBuf};

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
        self.var_os(key)
            .map(|value| value.to_string_lossy().into_owned())
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
        let mut source_env_paths = resolve_source_environment_paths(
            &cwd,
            &home_dir,
            use_env_roots,
            &inputs,
            platform_config_dir.as_deref(),
        )?;
        let extra_scan_paths = resolve_extra_scan_paths(&cwd, use_env_roots, &inputs)?;
        let extra_dirs_input = inputs.input(ENV_TOKSCALE_EXTRA_DIRS);
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
        } else if input.state == InputState::Empty {
            ResolvedPathInput::fallback(
                &input,
                fallback_source_env_path(key, home, platform_config_dir)?,
            )
        } else if let Some(raw) = input.value.as_deref() {
            if source_env_uses_nonblank_semantics(key) && raw.to_string_lossy().trim().is_empty() {
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

fn source_env_uses_nonblank_semantics(key: &str) -> bool {
    key != ENV_XDG_CONFIG_HOME
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
        return platform_config_dir
            .map(|root| root.join("tokscale"))
            .ok_or(SourceContextUnavailable);
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
            .map(|path| fully_qualified(cwd, &path))
            .collect::<Result<_, _>>()?;
    }
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
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    };
    Ok(lexically_normalize(&joined))
}

#[cfg(windows)]
fn fully_qualified(base: &Path, path: &Path) -> Result<PathBuf, SourceContextUnavailable> {
    use std::path::Prefix;

    if path.is_absolute() {
        return Ok(lexically_normalize(path));
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
                if drive != base_drive {
                    return Err(SourceContextUnavailable);
                }
                let tail = path.components().skip(1).collect::<PathBuf>();
                Ok(lexically_normalize(&base.join(tail)))
            }
            _ => Err(SourceContextUnavailable),
        },
        Some(Component::RootDir) => {
            let Some(Component::Prefix(prefix)) = base.components().next() else {
                return Err(SourceContextUnavailable);
            };
            let mut result = PathBuf::from(prefix.as_os_str());
            result.push(path);
            Ok(lexically_normalize(&result))
        }
        _ => Ok(lexically_normalize(&base.join(path))),
    }
}

fn lexically_normalize(path: &Path) -> PathBuf {
    let mut result = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                let last_is_normal = result
                    .components()
                    .next_back()
                    .is_some_and(|last| matches!(last, Component::Normal(_)));
                if last_is_normal {
                    result.pop();
                }
            }
            other => result.push(other.as_os_str()),
        }
    }
    result
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
        let values = values.into_iter().collect::<BTreeMap<_, _>>();
        SourceResolutionInputs::capture_with(|key| values.get(key).cloned(), fixture_platform(root))
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
    fn lexical_normalization_preserves_nonexistent_paths() {
        assert_eq!(
            lexically_normalize(Path::new("/tmp/a/../b/./c")),
            PathBuf::from("/tmp/b/c")
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
        assert_eq!(
            context.home_dir(),
            lexically_normalize(&captured_cwd.join("home"))
        );
        assert_eq!(
            context.scanner_settings().opencode_db_paths,
            vec![lexically_normalize(&captured_cwd.join("db/opencode.db"))]
        );
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
            &vec![lexically_normalize(
                &captured_cwd.join("codex-a/sessions/a.jsonl")
            )]
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
    fn descriptor_has_fixed_sha256_and_unix_native_byte_vectors() {
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
        #[cfg(unix)]
        assert_eq!(
            context.identity_string(),
            "sc1:ec742e1aed93e5a289aec6f187315ea2df114057523eea30e3c6952c0a605d80"
        );
        #[cfg(windows)]
        assert_eq!(
            context.identity_string(),
            "sc1:feec4781dd7181640e65b2c558e217bbc053a987eb365f87eb86f6ac86b6b8ad"
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
    fn windows_paths_use_capture_drive_and_reject_ambiguous_drives() {
        let base = Path::new(r"C:\capture\work");
        assert_eq!(
            fully_qualified(base, Path::new(r"C:profile\sessions")).unwrap(),
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
    fn pricing_and_cache_configuration_do_not_change_source_identity() {
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
