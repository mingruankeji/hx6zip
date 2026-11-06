use std::fs::{self, File, OpenOptions, TryLockError};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use directories::{BaseDirs, ProjectDirs};
use serde::{Deserialize, Serialize};

pub mod assertions;
pub mod http;
pub mod tree;
pub mod vars;

pub use assertions::AssertionResult;
pub use http::{HttpResponse, execute_request, format_size, generate_curl};

static REQUEST_ID_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BodyType {
    None,
    #[default]
    Json,
    Xml,
    Text,
    FormUrlEncoded,
    Multipart,
}

impl BodyType {
    pub const ALL: [BodyType; 6] = [
        BodyType::None,
        BodyType::Json,
        BodyType::Xml,
        BodyType::Text,
        BodyType::FormUrlEncoded,
        BodyType::Multipart,
    ];

    pub fn label(self) -> &'static str {
        match self {
            BodyType::None => "无请求体",
            BodyType::Json => "JSON",
            BodyType::Xml => "XML",
            BodyType::Text => "纯文本",
            BodyType::FormUrlEncoded => "表单 URL 编码",
            BodyType::Multipart => "多部分表单",
        }
    }

    pub fn content_type(self) -> Option<&'static str> {
        match self {
            BodyType::Json => Some("application/json"),
            BodyType::Xml => Some("application/xml"),
            BodyType::Text => Some("text/plain"),
            BodyType::FormUrlEncoded => Some("application/x-www-form-urlencoded"),
            BodyType::None | BodyType::Multipart => None,
        }
    }

    pub fn is_form(self) -> bool {
        matches!(self, BodyType::FormUrlEncoded | BodyType::Multipart)
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AuthType {
    #[default]
    None,
    Bearer,
    Basic,
    ApiKey,
}

impl AuthType {
    pub const ALL: [AuthType; 4] = [
        AuthType::None,
        AuthType::Bearer,
        AuthType::Basic,
        AuthType::ApiKey,
    ];

    pub fn label(self) -> &'static str {
        match self {
            AuthType::None => "无认证",
            AuthType::Bearer => "Bearer 令牌",
            AuthType::Basic => "Basic 认证",
            AuthType::ApiKey => "API Key",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ApiKeyLocation {
    #[default]
    Header,
    Query,
}

impl ApiKeyLocation {
    pub fn label(self) -> &'static str {
        match self {
            ApiKeyLocation::Header => "请求头",
            ApiKeyLocation::Query => "查询参数",
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default)]
pub struct Auth {
    pub auth_type: AuthType,
    pub token: String,
    pub username: String,
    pub password: String,
    pub key: String,
    pub value: String,
    pub location: ApiKeyLocation,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default)]
pub struct RequestSettings {
    pub timeout_ms: u64,
    pub follow_redirects: bool,
    pub verify_tls: bool,
}

impl Default for RequestSettings {
    fn default() -> Self {
        Self {
            timeout_ms: 30_000,
            follow_redirects: true,
            verify_tls: true,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct SavedRequest {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub folder: String,
    pub method: String,
    pub url: String,
    pub params: String,
    pub headers: String,
    pub body: String,
    #[serde(default)]
    pub body_type: BodyType,
    #[serde(default)]
    pub auth: Auth,
    #[serde(default)]
    pub settings: RequestSettings,
    #[serde(default)]
    pub assertions: String,
    pub created_at: u64,
    pub updated_at: u64,
}

impl SavedRequest {
    pub fn new(name: impl Into<String>) -> Self {
        let now = current_timestamp();
        Self {
            id: new_request_id(),
            name: name.into(),
            folder: String::new(),
            method: "GET".into(),
            url: String::new(),
            params: String::new(),
            headers: "Content-Type: application/json".into(),
            body: String::new(),
            body_type: BodyType::Json,
            auth: Auth::default(),
            settings: RequestSettings::default(),
            assertions: String::new(),
            created_at: now,
            updated_at: now,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Environment {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub variables: String,
}

impl Environment {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            id: new_request_id(),
            name: name.into(),
            variables: String::new(),
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct EnvironmentCollection {
    #[serde(default)]
    pub environments: Vec<Environment>,
    #[serde(default)]
    pub active: Option<String>,
}

#[derive(Clone, Debug)]
pub struct RequestStore {
    path: PathBuf,
}

#[derive(Debug)]
pub enum StoreLockError {
    AlreadyOpen,
    Unavailable(String),
}

pub struct RequestStoreLock {
    _file: File,
}

impl RequestStoreLock {
    fn acquire_in(directory: &Path) -> Result<Self, StoreLockError> {
        create_data_directory(directory).map_err(StoreLockError::Unavailable)?;
        let file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(directory.join("api-client.lock"))
            .map_err(|error| {
                StoreLockError::Unavailable(format!("无法打开 API 数据锁：{error}"))
            })?;
        match file.try_lock() {
            Ok(()) => Ok(Self { _file: file }),
            Err(TryLockError::WouldBlock) => Err(StoreLockError::AlreadyOpen),
            Err(TryLockError::Error(error)) => Err(StoreLockError::Unavailable(format!(
                "无法锁定 API 数据：{error}"
            ))),
        }
    }
}

#[derive(Deserialize, Serialize)]
struct RequestCollection {
    version: u32,
    requests: Vec<SavedRequest>,
    #[serde(default)]
    folders: Vec<String>,
}

impl RequestStore {
    pub fn acquire_lock() -> Result<RequestStoreLock, StoreLockError> {
        let directory = data_directory().map_err(StoreLockError::Unavailable)?;
        RequestStoreLock::acquire_in(&directory)
    }

    pub fn open() -> Result<(Self, Vec<SavedRequest>, Vec<String>), String> {
        let directory = data_directory()?;
        let legacy_directories = [
            legacy_data_directory("dmgpost", "dmgpost")?,
            legacy_data_directory("hbruno", "HBruno")?,
        ];
        Self::open_in(&directory, &legacy_directories)
    }

    fn open_in(
        directory: &Path,
        legacy_directories: &[PathBuf],
    ) -> Result<(Self, Vec<SavedRequest>, Vec<String>), String> {
        create_data_directory(directory)?;
        migrate_legacy_data(directory, legacy_directories)?;
        let store = Self {
            path: directory.join("requests.json"),
        };
        let (requests, folders) = store.load()?;
        Ok((store, requests, folders))
    }

    pub fn load(&self) -> Result<(Vec<SavedRequest>, Vec<String>), String> {
        let store = self;
        let (mut requests, mut folders) = match fs::read(&store.path) {
            Ok(contents) => {
                let collection: RequestCollection = serde_json::from_slice(&contents)
                    .map_err(|error| format!("读取已保存请求失败，数据文件格式无效：{error}"))?;
                if collection.version != 1 {
                    return Err(format!("请求数据版本 {} 不受支持。", collection.version));
                }
                (collection.requests, collection.folders)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let requests = vec![SavedRequest::new("未命名请求")];
                let folders = Vec::new();
                store.save(&requests, &folders)?;
                (requests, folders)
            }
            Err(error) => return Err(format!("读取已保存请求失败：{error}")),
        };
        let folders_before_reconciliation = folders.clone();
        let created_default_request = requests.is_empty();
        if created_default_request {
            requests.push(SavedRequest::new("未命名请求"));
        }
        for request_folder in folders.clone().into_iter().chain(
            requests
                .iter()
                .map(|request| request.folder.clone())
                .filter(|folder| !folder.is_empty()),
        ) {
            let mut path = String::new();
            for segment in request_folder.split('/') {
                if !path.is_empty() {
                    path.push('/');
                }
                path.push_str(segment);
                if !folders.contains(&path) {
                    folders.push(path.clone());
                }
            }
        }
        let mut seen = std::collections::HashSet::new();
        folders.retain(|folder| seen.insert(folder.clone()));
        if created_default_request || folders != folders_before_reconciliation {
            store.save(&requests, &folders)?;
        }

        Ok((requests, folders))
    }

    pub fn save(&self, requests: &[SavedRequest], folders: &[String]) -> Result<(), String> {
        let collection = RequestCollection {
            version: 1,
            requests: requests.to_vec(),
            folders: folders.to_vec(),
        };
        let contents = serde_json::to_vec_pretty(&collection)
            .map_err(|error| format!("序列化请求失败：{error}"))?;
        fs::write(&self.path, contents).map_err(|error| format!("保存请求失败：{error}"))
    }

    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    pub fn at_path(path: PathBuf) -> Self {
        Self { path }
    }

    pub fn environments_path(&self) -> PathBuf {
        self.path.with_file_name("environments.json")
    }

    pub fn load_environments(&self) -> Result<EnvironmentCollection, String> {
        match fs::read(self.environments_path()) {
            Ok(contents) => serde_json::from_slice(&contents)
                .map_err(|error| format!("读取环境变量失败，数据文件格式无效：{error}")),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                Ok(EnvironmentCollection::default())
            }
            Err(error) => Err(format!("读取环境变量失败：{error}")),
        }
    }

    pub fn save_environments(&self, collection: &EnvironmentCollection) -> Result<(), String> {
        let contents = serde_json::to_vec_pretty(collection)
            .map_err(|error| format!("序列化环境变量失败：{error}"))?;
        fs::write(self.environments_path(), contents)
            .map_err(|error| format!("保存环境变量失败：{error}"))
    }
}

fn data_directory() -> Result<PathBuf, String> {
    let base = BaseDirs::new().ok_or_else(|| "无法确定应用数据目录。".to_owned())?;
    Ok(base.data_local_dir().join("MZip").join("api-client"))
}

fn legacy_data_directory(organization: &str, application: &str) -> Result<PathBuf, String> {
    ProjectDirs::from("com", organization, application)
        .map(|dirs| dirs.data_local_dir().to_path_buf())
        .ok_or_else(|| "无法确定旧版 API 工具的数据目录。".to_owned())
}

fn create_data_directory(directory: &Path) -> Result<(), String> {
    fs::create_dir_all(directory).map_err(|error| format!("创建请求数据目录失败：{error}"))
}

fn migrate_legacy_data(directory: &Path, legacy_directories: &[PathBuf]) -> Result<(), String> {
    let request_path = directory.join("requests.json");
    if request_path
        .try_exists()
        .map_err(|error| format!("检查请求数据文件失败：{error}"))?
    {
        return Ok(());
    }

    let mut source_directory = None;
    for name in ["requests.json", "environments.json"] {
        for legacy_directory in legacy_directories {
            let source = legacy_directory.join(name);
            if source
                .try_exists()
                .map_err(|error| format!("检查旧版 API 数据 {} 失败：{error}", source.display()))?
            {
                source_directory = Some((legacy_directory, name));
                break;
            }
        }
        if source_directory.is_some() {
            break;
        }
    }
    let Some((source_directory, required_name)) = source_directory else {
        return Ok(());
    };

    // Publish requests last so an interrupted migration can retry the environment copy.
    for name in ["environments.json", "requests.json"] {
        let target = directory.join(name);
        if target
            .try_exists()
            .map_err(|error| format!("检查 API 数据文件失败：{error}"))?
        {
            continue;
        }
        let source_path = source_directory.join(name);
        let mut source = match File::open(&source_path) {
            Ok(source) => source,
            Err(error) if error.kind() == io::ErrorKind::NotFound && name != required_name => {
                continue;
            }
            Err(error) => {
                return Err(format!(
                    "读取旧版 API 数据 {} 失败：{error}",
                    source_path.display()
                ));
            }
        };
        let mut temporary = tempfile::NamedTempFile::new_in(directory)
            .map_err(|error| format!("创建 API 数据临时文件失败：{error}"))?;
        io::copy(&mut source, &mut temporary).map_err(|error| {
            format!("复制旧版 API 数据 {} 失败：{error}", source_path.display())
        })?;
        temporary
            .as_file()
            .sync_all()
            .map_err(|error| format!("写入 API 数据失败：{error}"))?;
        match temporary.persist_noclobber(&target) {
            Ok(_) => {}
            Err(error) if error.error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => {
                return Err(format!("保存迁移后的 API 数据失败：{}", error.error));
            }
        }
    }
    Ok(())
}

pub(crate) fn current_timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

pub(crate) fn new_request_id() -> String {
    let timestamp = current_timestamp();
    let counter = REQUEST_ID_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{timestamp:x}-{counter:x}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn create_legacy_store(directory: &Path, name: &str) -> (SavedRequest, EnvironmentCollection) {
        fs::create_dir_all(directory).unwrap();
        let store = RequestStore::at_path(directory.join("requests.json"));
        let mut request = SavedRequest::new(name);
        request.folder = "接口/用户".into();
        store
            .save(&[request.clone()], &["接口".into(), "接口/用户".into()])
            .unwrap();
        let mut environment = Environment::new("开发");
        environment.variables = "host=example.test".into();
        let environments = EnvironmentCollection {
            active: Some(environment.id.clone()),
            environments: vec![environment],
        };
        store.save_environments(&environments).unwrap();
        (request, environments)
    }

    #[cfg(windows)]
    #[test]
    fn windows_data_paths_match_current_and_legacy_locations() {
        let base = BaseDirs::new().unwrap().data_local_dir().to_path_buf();

        assert_eq!(
            data_directory().unwrap(),
            base.join("MZip").join("api-client")
        );
        assert_eq!(
            legacy_data_directory("dmgpost", "dmgpost").unwrap(),
            base.join("dmgpost").join("dmgpost").join("data")
        );
        assert_eq!(
            legacy_data_directory("hbruno", "HBruno").unwrap(),
            base.join("hbruno").join("HBruno").join("data")
        );
    }

    #[test]
    fn migrates_dmgpost_requests_and_environments_once_without_deleting_source() {
        let temp = tempfile::tempdir().unwrap();
        let current = temp.path().join("mzip");
        let old = temp.path().join("dmgpost");
        let older = temp.path().join("hbruno");
        let (request, environments) = create_legacy_store(&old, "旧请求");
        create_legacy_store(&older, "更旧请求");

        let (store, requests, folders) =
            RequestStore::open_in(&current, &[old.clone(), older]).unwrap();

        assert_eq!(requests, vec![request.clone()]);
        assert_eq!(folders, vec!["接口", "接口/用户"]);
        assert_eq!(store.load_environments().unwrap(), environments);
        assert_eq!(
            fs::read(old.join("requests.json")).unwrap(),
            fs::read(current.join("requests.json")).unwrap()
        );
        assert!(old.join("environments.json").is_file());

        let updated = SavedRequest::new("MZip 新请求");
        store.save(&[updated.clone()], &[]).unwrap();
        fs::remove_file(store.environments_path()).unwrap();
        let (reopened, requests, _) = RequestStore::open_in(&current, &[old]).unwrap();
        assert_eq!(requests, vec![updated]);
        assert_eq!(
            reopened.load_environments().unwrap(),
            EnvironmentCollection::default()
        );
    }

    #[test]
    fn migrates_hbruno_when_dmgpost_data_is_missing() {
        let temp = tempfile::tempdir().unwrap();
        let current = temp.path().join("mzip");
        let old = temp.path().join("dmgpost");
        let older = temp.path().join("hbruno");
        let (request, environments) = create_legacy_store(&older, "HBruno 请求");

        let (store, requests, _) = RequestStore::open_in(&current, &[old, older]).unwrap();

        assert_eq!(requests, vec![request]);
        assert_eq!(store.load_environments().unwrap(), environments);
    }

    #[test]
    fn migration_does_not_mix_requests_with_an_older_environment() {
        let temp = tempfile::tempdir().unwrap();
        let current = temp.path().join("mzip");
        let old = temp.path().join("dmgpost");
        let older = temp.path().join("hbruno");
        let (request, _) = create_legacy_store(&old, "旧请求");
        fs::remove_file(old.join("environments.json")).unwrap();
        create_legacy_store(&older, "更旧请求");

        let (store, requests, _) = RequestStore::open_in(&current, &[old, older]).unwrap();

        assert_eq!(requests, vec![request]);
        assert_eq!(
            store.load_environments().unwrap(),
            EnvironmentCollection::default()
        );
        assert!(!store.environments_path().exists());
    }

    #[test]
    fn migration_keeps_legacy_environment_even_without_saved_requests() {
        let temp = tempfile::tempdir().unwrap();
        let current = temp.path().join("mzip");
        let old = temp.path().join("dmgpost");
        let (_, environments) = create_legacy_store(&old, "旧请求");
        fs::remove_file(old.join("requests.json")).unwrap();

        let (store, requests, _) = RequestStore::open_in(&current, &[old]).unwrap();

        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].name, "未命名请求");
        assert_eq!(store.load_environments().unwrap(), environments);
    }

    #[test]
    fn migration_preserves_existing_environment_and_retries_after_error() {
        let temp = tempfile::tempdir().unwrap();
        let current = temp.path().join("mzip");
        let old = temp.path().join("dmgpost");
        fs::create_dir_all(&current).unwrap();
        fs::create_dir_all(&old).unwrap();
        let (request, _) = create_legacy_store(&old, "可恢复的请求");
        let request_file = old.join("requests.json");
        let request_contents = fs::read(&request_file).unwrap();
        fs::remove_file(&request_file).unwrap();
        fs::create_dir(&request_file).unwrap();
        let local_environment = br#"{"environments":[],"active":null}"#;
        fs::write(current.join("environments.json"), local_environment).unwrap();

        let error = RequestStore::open_in(&current, &[old.clone()]).unwrap_err();
        assert!(error.contains("旧版 API 数据"), "{error}");
        assert!(!current.join("requests.json").exists());
        assert_eq!(
            fs::read(current.join("environments.json")).unwrap(),
            local_environment
        );

        fs::remove_dir(&request_file).unwrap();
        fs::write(&request_file, request_contents).unwrap();
        let (store, requests, _) = RequestStore::open_in(&current, &[old]).unwrap();
        assert_eq!(requests, vec![request]);
        assert_eq!(
            store.load_environments().unwrap(),
            EnvironmentCollection::default()
        );
    }

    #[test]
    fn malformed_legacy_requests_are_reported_instead_of_replaced() {
        let temp = tempfile::tempdir().unwrap();
        let current = temp.path().join("mzip");
        let old = temp.path().join("dmgpost");
        fs::create_dir(&old).unwrap();
        fs::write(old.join("requests.json"), b"invalid json").unwrap();

        let error = RequestStore::open_in(&current, &[old.clone()]).unwrap_err();

        assert!(error.contains("数据文件格式无效"), "{error}");
        assert_eq!(
            fs::read(current.join("requests.json")).unwrap(),
            b"invalid json"
        );
        assert_eq!(
            fs::read(old.join("requests.json")).unwrap(),
            b"invalid json"
        );
    }

    #[test]
    fn request_store_lock_excludes_other_instances_until_released() {
        let temp = tempfile::tempdir().unwrap();
        let directory = temp.path().join("mzip");

        let first = RequestStoreLock::acquire_in(&directory).unwrap();
        assert!(matches!(
            RequestStoreLock::acquire_in(&directory),
            Err(StoreLockError::AlreadyOpen)
        ));
        drop(first);
        assert!(RequestStoreLock::acquire_in(&directory).is_ok());
    }

    #[test]
    fn request_store_lock_excludes_other_processes() {
        const DIRECTORY: &str = "MZIP_TEST_API_LOCK_DIRECTORY";
        const EXPECT_LOCKED: &str = "MZIP_TEST_API_LOCK_EXPECT_LOCKED";
        if let Some(directory) = std::env::var_os(DIRECTORY) {
            let result = RequestStoreLock::acquire_in(Path::new(&directory));
            if std::env::var(EXPECT_LOCKED).unwrap() == "1" {
                assert!(matches!(result, Err(StoreLockError::AlreadyOpen)));
            } else {
                assert!(result.is_ok());
            }
            return;
        }

        let temp = tempfile::tempdir().unwrap();
        let directory = temp.path().join("mzip");
        let first = RequestStoreLock::acquire_in(&directory).unwrap();
        let run_child = |expect_locked: bool| {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .arg("--exact")
                .arg("api_client::tests::request_store_lock_excludes_other_processes")
                .env(DIRECTORY, &directory)
                .env(EXPECT_LOCKED, if expect_locked { "1" } else { "0" })
                .output()
                .unwrap();
            let stdout = String::from_utf8_lossy(&output.stdout);
            assert!(
                output.status.success() && stdout.contains("1 passed;"),
                "lock probe failed: {}{}",
                stdout,
                String::from_utf8_lossy(&output.stderr)
            );
        };
        run_child(true);
        drop(first);
        run_child(false);
    }

    #[test]
    fn saved_request_defaults_to_json_headers() {
        let request = SavedRequest::new("测试请求");

        assert_eq!(request.name, "测试请求");
        assert_eq!(request.method, "GET");
        assert_eq!(request.headers, "Content-Type: application/json");
        assert!(!request.id.is_empty());
    }

    #[test]
    fn request_store_round_trips_requests() {
        let directory =
            std::env::temp_dir().join(format!("dmgpost-store-test-{}", new_request_id()));
        fs::create_dir_all(&directory).unwrap();
        let store = RequestStore::at_path(directory.join("requests.json"));
        let mut request = SavedRequest::new("持久化测试");
        request.folder = "用户接口".into();
        let requests = vec![request];
        let folders = vec!["用户接口".to_string()];

        store.save(&requests, &folders).unwrap();
        let contents = fs::read(&store.path).unwrap();
        let collection: RequestCollection = serde_json::from_slice(&contents).unwrap();

        assert_eq!(collection.version, 1);
        assert_eq!(collection.folders, folders);
        assert_eq!(collection.requests, requests);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn saved_request_deserializes_without_folder_for_legacy_data() {
        let request: SavedRequest = serde_json::from_str(
            r#"{
                "id":"legacy",
                "name":"旧请求",
                "method":"GET",
                "url":"",
                "params":"",
                "headers":"Content-Type: application/json",
                "body":"",
                "created_at":1,
                "updated_at":1
            }"#,
        )
        .unwrap();

        assert!(request.folder.is_empty());
        assert_eq!(request.body_type, BodyType::Json);
        assert_eq!(request.auth.auth_type, AuthType::None);
        assert_eq!(request.settings, RequestSettings::default());
        assert!(request.assertions.is_empty());
    }

    #[test]
    fn environments_round_trip_next_to_requests() {
        let directory = std::env::temp_dir().join(format!("dmgpost-env-test-{}", new_request_id()));
        fs::create_dir_all(&directory).unwrap();
        let store = RequestStore::at_path(directory.join("requests.json"));
        assert_eq!(
            store.load_environments().unwrap(),
            EnvironmentCollection::default()
        );

        let mut environment = Environment::new("开发");
        environment.variables = "host=localhost".into();
        let collection = EnvironmentCollection {
            active: Some(environment.id.clone()),
            environments: vec![environment],
        };
        store.save_environments(&collection).unwrap();

        assert_eq!(store.load_environments().unwrap(), collection);
        fs::remove_dir_all(directory).unwrap();
    }
}
