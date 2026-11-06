use std::time::Instant;
use std::{collections::HashSet, process::Command, time::SystemTime};

use gpui_kit::base::Disableable;
use gpui_kit::component::IndexPath;
use gpui_kit::component::Theme;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::input::{Input, InputEvent, InputState, Textarea, TextareaState};
use gpui_kit::component::menu::{ContextMenuExt, DropdownMenu, PopupMenuItem};
use gpui_kit::component::scroll::{ScrollableElement, ScrollbarMode};
use gpui_kit::component::select::{SearchableVec, Select, SelectEvent, SelectState};
use gpui_kit::{
    AnyElement, App, AppContext, Bounds, ClickEvent, ClipboardItem, Context, Entity, FontWeight,
    IntoElement, Render, Rgba, SharedString, Subscription, TestSupportExt, TitlebarOptions, Window,
    WindowBounds, WindowOptions, application, assets, div, init, open_window, prelude::*, px, rgb,
    size,
};
use mzip::api_client::tree::{self, TreeItem};
use mzip::api_client::vars::{self, Variables};
use mzip::api_client::{
    ApiKeyLocation, AssertionResult, AuthType, BodyType, Environment, EnvironmentCollection,
    RequestSettings, RequestStore, SavedRequest, StoreLockError, execute_request, format_size,
    generate_curl,
};

#[derive(Clone)]
struct TreeDrag {
    item: TreeItem,
    label: String,
}

impl Render for TreeDrag {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .px_3()
            .py_2()
            .rounded_md()
            .bg(rgb(0x292a2e))
            .border_1()
            .border_color(rgb(0xe67e45))
            .text_size(px(12.))
            .text_color(rgb(0xe4e4e7))
            .child(self.label.clone())
    }
}

struct ApiTester {
    method: Entity<SelectState<SearchableVec<&'static str>>>,
    search: Entity<InputState>,
    url: Entity<InputState>,
    params: Entity<TextareaState>,
    headers: Entity<TextareaState>,
    body: Entity<TextareaState>,
    body_type: BodyType,
    auth_type: AuthType,
    api_key_location: ApiKeyLocation,
    auth_token: Entity<InputState>,
    auth_username: Entity<InputState>,
    auth_password: Entity<InputState>,
    auth_key: Entity<InputState>,
    auth_value: Entity<InputState>,
    assertions: Entity<TextareaState>,
    timeout: Entity<InputState>,
    follow_redirects: bool,
    verify_tls: bool,
    environments: EnvironmentCollection,
    environment_error: Option<String>,
    env_variables: Entity<TextareaState>,
    env_name_input: Option<Entity<InputState>>,
    env_edit: Option<EnvEdit>,
    pending_delete_env: bool,
    response_content: Entity<TextareaState>,
    response_headers: Entity<TextareaState>,
    response_timeline: Entity<TextareaState>,
    response_tab: ResponseTab,
    response_size: Option<String>,
    response_header_count: usize,
    assertion_results: Vec<AssertionResult>,
    request_generation: u64,
    active_tab: RequestTab,
    is_loading: bool,
    response_status: Option<String>,
    response_time: Option<String>,
    response_error: bool,
    store: Option<RequestStore>,
    store_error: Option<String>,
    requests: Vec<SavedRequest>,
    folders: Vec<String>,
    expanded_folders: HashSet<String>,
    active_request_id: String,
    active_folder: String,
    creating_folder: bool,
    folder_parent: String,
    folder_edit_source: Option<String>,
    folder_input: Option<Entity<InputState>>,
    renaming_request_id: Option<String>,
    rename_input: Option<Entity<InputState>>,
    pending_delete_id: Option<String>,
    pending_delete_folder: Option<String>,
    _subscriptions: Vec<Subscription>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RequestTab {
    Body,
    Headers,
    Auth,
    Assertions,
    Settings,
    Environment,
    Params,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ResponseTab {
    Body,
    Headers,
    Timeline,
    Assertions,
}

#[derive(Clone, PartialEq, Eq)]
enum EnvEdit {
    Create,
    Rename(String),
}

impl ApiTester {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let (store, requests, folders, store_error) = match RequestStore::open() {
            Ok((store, requests, folders)) => (Some(store), requests, folders, None),
            Err(error) => (
                None,
                vec![SavedRequest::new("未保存请求")],
                Vec::new(),
                Some(error),
            ),
        };
        Self::with_data(store, requests, folders, store_error, window, cx)
    }

    fn with_data(
        store: Option<RequestStore>,
        requests: Vec<SavedRequest>,
        folders: Vec<String>,
        store_error: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let active_request_id = requests[0].id.clone();
        let initial_request = requests[0].clone();
        let expanded_folders = folders.iter().cloned().collect();
        let (environments, environment_error) = match store.as_ref() {
            Some(store) => match store.load_environments() {
                Ok(environments) => (environments, None),
                Err(error) => (EnvironmentCollection::default(), Some(error)),
            },
            None => (EnvironmentCollection::default(), None),
        };
        let initial_response = store_error
            .clone()
            .or_else(|| environment_error.clone())
            .unwrap_or_else(|| "发送请求后，响应内容会显示在这里。".into());
        let load_failed = store_error.is_some() || environment_error.is_some();
        let mut this = Self {
            method: cx.new(|cx| {
                SelectState::new(
                    SearchableVec::new(vec![
                        "GET", "POST", "PUT", "PATCH", "DELETE", "HEAD", "OPTIONS",
                    ]),
                    Some(IndexPath::default()),
                    window,
                    cx,
                )
            }),
            search: cx.new(|cx| InputState::new(window, cx).placeholder("搜索请求…")),
            url: cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder("请输入接口地址，例如：https://example.com/api")
            }),
            params: cx.new(|cx| {
                TextareaState::new(window, cx)
                    .rows(6)
                    .placeholder("每行一个参数，格式：名称=值\n例如：\npage=1\nkeyword=示例")
            }),
            headers: cx.new(|cx| {
                TextareaState::new(window, cx)
                    .rows(6)
                    .default_value("Content-Type: application/json")
                    .placeholder(
                        "请求头格式：名称: 值\n例如：Content-Type: application/json\nAuthorization: Bearer 令牌",
                    )
            }),
            body: cx.new(|cx| {
                TextareaState::new(window, cx)
                    .rows(6)
                    .placeholder("{\n  \"名称\": \"示例\"\n}")
            }),
            body_type: BodyType::Json,
            auth_type: AuthType::None,
            api_key_location: ApiKeyLocation::Header,
            auth_token: cx.new(|cx| {
                InputState::new(window, cx)
                    .masked(true)
                    .placeholder("令牌，例如：{{token}}")
            }),
            auth_username: cx.new(|cx| InputState::new(window, cx).placeholder("用户名")),
            auth_password: cx.new(|cx| {
                InputState::new(window, cx)
                    .masked(true)
                    .placeholder("密码")
            }),
            auth_key: cx.new(|cx| {
                InputState::new(window, cx).placeholder("键名，例如：X-API-Key")
            }),
            auth_value: cx.new(|cx| {
                InputState::new(window, cx)
                    .masked(true)
                    .placeholder("键值")
            }),
            assertions: cx.new(|cx| {
                TextareaState::new(window, cx).rows(6).placeholder(
                    "每行一条断言，格式：目标 运算符 期望值\n例如：\nres.status eq 200\nres.body.data.id exists\nres.responseTime lt 1000",
                )
            }),
            timeout: cx.new(|cx| InputState::new(window, cx).placeholder("30000")),
            follow_redirects: true,
            verify_tls: true,
            environments,
            environment_error,
            env_variables: cx.new(|cx| {
                TextareaState::new(window, cx).rows(6).placeholder(
                    "每行一个变量，格式：名称=值\n例如：\nbaseUrl=https://api.example.com\ntoken=abc123",
                )
            }),
            env_name_input: None,
            env_edit: None,
            pending_delete_env: false,
            response_content: cx.new(|cx| {
                TextareaState::new(window, cx)
                    .rows(8)
                    .context_menu(false)
                    .default_value(initial_response)
            }),
            response_headers: cx.new(|cx| TextareaState::new(window, cx).rows(8).context_menu(false)),
            response_timeline: cx
                .new(|cx| TextareaState::new(window, cx).rows(8).context_menu(false)),
            response_tab: ResponseTab::Body,
            response_size: None,
            response_header_count: 0,
            assertion_results: Vec::new(),
            request_generation: 0,
            active_tab: RequestTab::Body,
            is_loading: false,
            response_status: load_failed.then(|| "本地加载失败".into()),
            response_time: None,
            response_error: load_failed,
            store,
            store_error,
            requests,
            folders,
            expanded_folders,
            active_request_id,
            active_folder: initial_request.folder.clone(),
            creating_folder: false,
            folder_parent: String::new(),
            folder_edit_source: None,
            folder_input: None,
            renaming_request_id: None,
            rename_input: None,
            pending_delete_id: None,
            pending_delete_folder: None,
            _subscriptions: Vec::new(),
        };
        this.apply_request(&initial_request, window, cx);
        this.load_active_environment_variables(window, cx);

        this._subscriptions.push(cx.subscribe_in(
            &this.search,
            window,
            |_this, _, event: &InputEvent, _, cx| {
                if matches!(event, InputEvent::Change) {
                    cx.notify();
                }
            },
        ));
        this._subscriptions.push(cx.subscribe_in(
            &this.url,
            window,
            |this, _, event: &InputEvent, window, cx| match event {
                InputEvent::Change => this.persist_active(window, cx),
                InputEvent::PressEnter { .. } => this.send_request(window, cx),
                _ => {}
            },
        ));
        for state in [
            this.auth_token.clone(),
            this.auth_username.clone(),
            this.auth_password.clone(),
            this.auth_key.clone(),
            this.auth_value.clone(),
            this.timeout.clone(),
        ] {
            this._subscriptions.push(cx.subscribe_in(
                &state,
                window,
                |this, _, event: &InputEvent, window, cx| {
                    if matches!(event, InputEvent::Change) {
                        this.persist_active(window, cx);
                    }
                },
            ));
        }
        this._subscriptions.push(cx.subscribe_in(
            &this.env_variables,
            window,
            |this, _, event: &InputEvent, window, cx| {
                if matches!(event, InputEvent::Change) {
                    this.save_environment_variables(window, cx);
                }
            },
        ));
        this._subscriptions.push(cx.subscribe_in(
            &this.params,
            window,
            |this, _, event: &InputEvent, window, cx| {
                if matches!(event, InputEvent::Change) {
                    this.persist_active(window, cx);
                }
            },
        ));
        for state in [
            this.headers.clone(),
            this.body.clone(),
            this.assertions.clone(),
        ] {
            this._subscriptions.push(cx.subscribe_in(
                &state,
                window,
                |this, _, event: &InputEvent, window, cx| {
                    if matches!(event, InputEvent::Change) {
                        this.persist_active(window, cx);
                    }
                },
            ));
        }
        this._subscriptions.push(cx.subscribe_in(
            &this.method,
            window,
            |this, _, event: &SelectEvent<SearchableVec<&'static str>>, window, cx| {
                if matches!(event, SelectEvent::Confirm(Some(_))) {
                    this.persist_active(window, cx);
                }
            },
        ));
        this
    }

    fn send_request(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.is_loading {
            return;
        }
        self.persist_active(window, cx);
        let Some(request) = self.snapshot_active(cx) else {
            return;
        };
        let variables = match self.active_variables() {
            Ok(variables) => variables,
            Err(error) => {
                self.show_notice("环境变量错误", error, true, window, cx);
                cx.notify();
                return;
            }
        };

        self.request_generation += 1;
        let generation = self.request_generation;
        self.is_loading = true;
        self.response_status = None;
        self.response_time = None;
        self.response_size = None;
        self.response_header_count = 0;
        self.assertion_results.clear();
        self.set_response_content("正在发送请求…", window, cx);
        self.set_response_details(String::new(), String::new(), window, cx);
        self.response_error = false;
        cx.notify();

        cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    let started = Instant::now();
                    let result = execute_request(&request, &variables);
                    (result, started.elapsed())
                })
                .await;

            let _ = this.update_in(cx, |view, window, cx| {
                if view.request_generation != generation {
                    return;
                }
                view.is_loading = false;
                match result {
                    (Ok(response), _) => {
                        let headers = response
                            .headers
                            .iter()
                            .map(|(name, value)| format!("{name}: {value}"))
                            .collect::<Vec<_>>()
                            .join("\n");
                        let mut body = response.body;
                        if !response.unresolved.is_empty() {
                            body = format!(
                                "提示：以下变量未定义：{}\n\n{body}",
                                response.unresolved.join("、")
                            );
                        }
                        view.response_status = Some(response.status);
                        view.response_time = Some(format!("{} 毫秒", response.elapsed_ms));
                        view.response_size = Some(format_size(response.size));
                        view.response_header_count = response.headers.len();
                        view.response_error = !response.success;
                        view.set_response_content(body, window, cx);
                        view.set_response_details(headers, response.timeline, window, cx);
                        if response.assertions.iter().any(|result| !result.passed) {
                            view.response_tab = ResponseTab::Assertions;
                        }
                        view.assertion_results = response.assertions;
                    }
                    (Err(error), elapsed) => {
                        view.response_status = Some("请求失败".into());
                        view.response_time = Some(format!("{} 毫秒", elapsed.as_millis()));
                        view.response_tab = ResponseTab::Body;
                        view.set_response_content(error, window, cx);
                        view.response_error = true;
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn cancel_request(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.is_loading {
            return;
        }
        self.request_generation += 1;
        self.is_loading = false;
        self.show_notice(
            "已取消",
            "请求已取消，结果将被忽略。".into(),
            true,
            window,
            cx,
        );
        cx.notify();
    }

    fn set_body_type(&mut self, body_type: BodyType, window: &mut Window, cx: &mut Context<Self>) {
        self.body_type = body_type;
        self.persist_active(window, cx);
    }

    fn format_json_body(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.body.read(cx).value().to_string();
        match serde_json::from_str::<serde_json::Value>(&text) {
            Ok(json) => {
                let pretty = serde_json::to_string_pretty(&json).unwrap_or(text);
                self.body.update(cx, |state, cx| {
                    state.set_value(pretty, window, cx);
                });
                self.persist_active(window, cx);
            }
            Err(error) => {
                self.show_notice(
                    "JSON 无效",
                    format!("无法格式化请求体：{error}"),
                    true,
                    window,
                    cx,
                );
                cx.notify();
            }
        }
    }

    fn set_auth_type(&mut self, auth_type: AuthType, window: &mut Window, cx: &mut Context<Self>) {
        self.auth_type = auth_type;
        self.persist_active(window, cx);
    }

    fn set_api_key_location(
        &mut self,
        location: ApiKeyLocation,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.api_key_location = location;
        self.persist_active(window, cx);
    }

    fn active_environment(&self) -> Option<&Environment> {
        let id = self.environments.active.as_ref()?;
        self.environments
            .environments
            .iter()
            .find(|environment| &environment.id == id)
    }

    fn active_variables(&self) -> Result<Variables, String> {
        match self.active_environment() {
            Some(environment) => vars::parse_variables(&environment.variables)
                .map_err(|error| format!("环境“{}”：{error}", environment.name)),
            None => Ok(Variables::new()),
        }
    }

    fn set_response_details(
        &mut self,
        headers: String,
        timeline: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.response_headers.update(cx, |state, cx| {
            state.set_value(headers, window, cx);
        });
        self.response_timeline.update(cx, |state, cx| {
            state.set_value(timeline, window, cx);
        });
    }

    fn persist_environments(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if let Some(error) = &self.environment_error {
            self.show_notice("环境保存失败", error.clone(), true, window, cx);
            return false;
        }
        let Some(store) = &self.store else {
            return true;
        };
        match store.save_environments(&self.environments) {
            Ok(()) => true,
            Err(error) => {
                self.show_notice("环境保存失败", error, true, window, cx);
                false
            }
        }
    }

    fn load_active_environment_variables(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let variables = self
            .active_environment()
            .map(|environment| environment.variables.clone())
            .unwrap_or_default();
        self.env_variables.update(cx, |state, cx| {
            state.set_value(variables, window, cx);
        });
    }

    fn save_environment_variables(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let value = self.env_variables.read(cx).value().to_string();
        let Some(id) = self.environments.active.clone() else {
            return;
        };
        if let Some(environment) = self
            .environments
            .environments
            .iter_mut()
            .find(|environment| environment.id == id)
        {
            environment.variables = value;
        }
        self.persist_environments(window, cx);
        cx.notify();
    }

    fn select_environment(
        &mut self,
        id: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.environments.active = id;
        self.env_edit = None;
        self.env_name_input = None;
        self.pending_delete_env = false;
        self.load_active_environment_variables(window, cx);
        self.persist_environments(window, cx);
        cx.notify();
    }

    fn begin_environment_edit(
        &mut self,
        edit: EnvEdit,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let initial = match &edit {
            EnvEdit::Create => String::new(),
            EnvEdit::Rename(id) => self
                .environments
                .environments
                .iter()
                .find(|environment| &environment.id == id)
                .map(|environment| environment.name.clone())
                .unwrap_or_default(),
        };
        let input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("环境名称，例如：开发")
                .default_value(initial)
        });
        input.update(cx, |state, cx| state.focus(window, cx));
        self.pending_delete_env = false;
        self.env_name_input = Some(input);
        self.env_edit = Some(edit);
        self.active_tab = RequestTab::Environment;
        cx.notify();
    }

    fn save_environment_name(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(edit), Some(input)) = (self.env_edit.clone(), self.env_name_input.clone()) else {
            return;
        };
        let name = input.read(cx).value().trim().to_string();
        if name.is_empty() {
            self.show_notice("名称无效", "环境名称不能为空。".into(), true, window, cx);
            cx.notify();
            return;
        }
        let duplicate = self.environments.environments.iter().any(|environment| {
            environment.name == name && EnvEdit::Rename(environment.id.clone()) != edit
        });
        if duplicate {
            self.show_notice(
                "名称重复",
                format!("环境“{name}”已存在。"),
                true,
                window,
                cx,
            );
            cx.notify();
            return;
        }
        match edit {
            EnvEdit::Create => {
                let environment = Environment::new(name);
                self.environments.active = Some(environment.id.clone());
                self.environments.environments.push(environment);
                self.load_active_environment_variables(window, cx);
            }
            EnvEdit::Rename(id) => {
                if let Some(environment) = self
                    .environments
                    .environments
                    .iter_mut()
                    .find(|environment| environment.id == id)
                {
                    environment.name = name;
                }
            }
        }
        self.env_edit = None;
        self.env_name_input = None;
        self.persist_environments(window, cx);
        cx.notify();
    }

    fn delete_active_environment(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(id) = self.environments.active.clone() else {
            return;
        };
        self.environments
            .environments
            .retain(|environment| environment.id != id);
        self.pending_delete_env = false;
        self.select_environment(None, window, cx);
    }
    fn active_request(&self) -> Option<&SavedRequest> {
        self.requests
            .iter()
            .find(|request| request.id == self.active_request_id)
    }

    fn apply_request(
        &mut self,
        request: &SavedRequest,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let method = match request.method.as_str() {
            "GET" => "GET",
            "POST" => "POST",
            "PUT" => "PUT",
            "PATCH" => "PATCH",
            "DELETE" => "DELETE",
            "HEAD" => "HEAD",
            "OPTIONS" => "OPTIONS",
            _ => "GET",
        };
        self.method.update(cx, |state, cx| {
            state.set_selected_value(&method, window, cx);
        });
        self.url.update(cx, |state, cx| {
            state.set_value(request.url.clone(), window, cx);
        });
        self.params.update(cx, |state, cx| {
            state.set_value(request.params.clone(), window, cx);
        });
        self.headers.update(cx, |state, cx| {
            state.set_value(request.headers.clone(), window, cx);
        });
        self.body.update(cx, |state, cx| {
            state.set_value(request.body.clone(), window, cx);
        });
        self.body_type = request.body_type;
        self.auth_type = request.auth.auth_type;
        self.api_key_location = request.auth.location;
        for (state, value) in [
            (&self.auth_token, &request.auth.token),
            (&self.auth_username, &request.auth.username),
            (&self.auth_password, &request.auth.password),
            (&self.auth_key, &request.auth.key),
            (&self.auth_value, &request.auth.value),
        ] {
            state.update(cx, |state, cx| {
                state.set_value(value.clone(), window, cx);
            });
        }
        self.assertions.update(cx, |state, cx| {
            state.set_value(request.assertions.clone(), window, cx);
        });
        self.timeout.update(cx, |state, cx| {
            state.set_value(request.settings.timeout_ms.to_string(), window, cx);
        });
        self.follow_redirects = request.settings.follow_redirects;
        self.verify_tls = request.settings.verify_tls;
    }

    fn snapshot_active(&self, cx: &App) -> Option<SavedRequest> {
        let mut request = self.active_request()?.clone();
        request.method = self
            .method
            .read(cx)
            .selected_value()
            .copied()
            .unwrap_or("GET")
            .to_string();
        request.url = self.url.read(cx).value().to_string();
        request.params = self.params.read(cx).value().to_string();
        request.headers = self.headers.read(cx).value().to_string();
        request.body = self.body.read(cx).value().to_string();
        request.body_type = self.body_type;
        request.auth.auth_type = self.auth_type;
        request.auth.location = self.api_key_location;
        request.auth.token = self.auth_token.read(cx).value().to_string();
        request.auth.username = self.auth_username.read(cx).value().to_string();
        request.auth.password = self.auth_password.read(cx).value().to_string();
        request.auth.key = self.auth_key.read(cx).value().to_string();
        request.auth.value = self.auth_value.read(cx).value().to_string();
        request.assertions = self.assertions.read(cx).value().to_string();
        let timeout = self.timeout.read(cx).value().trim().to_string();
        request.settings = RequestSettings {
            timeout_ms: if timeout.is_empty() {
                RequestSettings::default().timeout_ms
            } else {
                timeout.parse().unwrap_or(request.settings.timeout_ms)
            },
            follow_redirects: self.follow_redirects,
            verify_tls: self.verify_tls,
        };
        request.updated_at = timestamp_now();
        Some(request)
    }

    fn capture_active(&mut self, cx: &App) {
        let Some(request) = self.snapshot_active(cx) else {
            return;
        };
        if let Some(existing) = self
            .requests
            .iter_mut()
            .find(|existing| existing.id == request.id)
        {
            *existing = request;
        }
    }

    fn persist_active(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.capture_active(cx);
        self.persist_requests(window, cx);
        cx.notify();
    }

    fn persist_requests(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some(store) = &self.store else {
            if let Some(error) = &self.store_error {
                self.show_notice("本地保存失败", error.clone(), true, window, cx);
            }
            return false;
        };
        match store.save(&self.requests, &self.folders) {
            Ok(()) => {
                self.store_error = None;
                true
            }
            Err(error) => {
                self.store_error = Some(error.clone());
                self.show_notice("本地保存失败", error, true, window, cx);
                false
            }
        }
    }

    fn set_response_content(
        &mut self,
        content: impl Into<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.response_content.update(cx, |state, cx| {
            state.set_value(content.into(), window, cx);
        });
    }

    fn show_notice(
        &mut self,
        status: &str,
        body: String,
        is_error: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.response_status = Some(status.into());
        self.response_time = None;
        self.response_size = None;
        self.response_tab = ResponseTab::Body;
        self.set_response_content(body, window, cx);
        self.response_error = is_error;
    }

    fn select_request(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let changing_request = self.active_request_id != id;
        if changing_request {
            self.persist_active(window, cx);
        }
        let Some(request) = self
            .requests
            .iter()
            .find(|request| request.id == id)
            .cloned()
        else {
            self.show_notice("请求不存在", "所选请求已不存在。".into(), true, window, cx);
            cx.notify();
            return;
        };
        self.active_folder = request.folder.clone();
        self.reveal_folder(&request.folder);
        if !changing_request {
            cx.notify();
            return;
        }
        self.active_request_id = id.to_owned();
        self.pending_delete_id = None;
        self.renaming_request_id = None;
        self.rename_input = None;
        self.apply_request(&request, window, cx);
        cx.notify();
    }

    fn create_request(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.capture_active(cx);
        let previous_requests = self.requests.clone();
        let previous_folders = self.folders.clone();
        let mut request = SavedRequest::new("未命名请求");
        request.folder = self.active_folder.clone();
        self.requests.push(request.clone());
        if !self.commit_tree_change(previous_requests, previous_folders, window, cx) {
            return;
        }
        self.active_request_id = request.id.clone();
        self.reveal_folder(&request.folder);
        self.search
            .update(cx, |state, cx| state.set_value("", window, cx));
        self.apply_request(&request, window, cx);
        cx.notify();
    }

    fn duplicate_request(
        &mut self,
        id: &str,
        name_prefix: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.capture_active(cx);
        let Some(source) = self
            .requests
            .iter()
            .find(|request| request.id == id)
            .cloned()
        else {
            return;
        };
        let mut copy = source.clone();
        copy.id = SavedRequest::new("").id;
        copy.name = format!("{name_prefix}{}", source.name);
        copy.created_at = timestamp_now();
        copy.updated_at = copy.created_at;
        let previous_requests = self.requests.clone();
        let previous_folders = self.folders.clone();
        self.requests.push(copy.clone());
        if !self.commit_tree_change(previous_requests, previous_folders, window, cx) {
            return;
        }
        self.active_request_id = copy.id.clone();
        self.active_folder = copy.folder.clone();
        self.reveal_folder(&copy.folder);
        self.search
            .update(cx, |state, cx| state.set_value("", window, cx));
        self.apply_request(&copy, window, cx);
        cx.notify();
    }

    fn begin_rename(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(request) = self.requests.iter().find(|request| request.id == id) else {
            return;
        };
        let initial_name = request.name.clone();
        let input = cx.new(|cx| InputState::new(window, cx));
        input.update(cx, |state, cx| state.set_value(initial_name, window, cx));
        self.renaming_request_id = Some(id.to_owned());
        self.rename_input = Some(input);
        cx.notify();
    }

    fn finish_rename(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(input) = &self.rename_input else {
            return;
        };
        let name = input.read(cx).value().trim().to_owned();
        if name.is_empty() {
            self.show_notice(
                "名称不能为空",
                "请输入请求名称后再保存。".into(),
                true,
                window,
                cx,
            );
            cx.notify();
            return;
        }
        self.capture_active(cx);
        let previous_requests = self.requests.clone();
        let previous_folders = self.folders.clone();
        let Some(request) = self.requests.iter_mut().find(|request| request.id == id) else {
            self.show_notice("请求不存在", "所选请求已不存在。".into(), true, window, cx);
            cx.notify();
            return;
        };
        request.name = name;
        request.updated_at = timestamp_now();
        if !self.commit_tree_change(previous_requests, previous_folders, window, cx) {
            return;
        }
        self.renaming_request_id = None;
        self.rename_input = None;
        cx.notify();
    }

    fn request_delete(&mut self, id: &str, cx: &mut Context<Self>) {
        self.pending_delete_id = Some(id.to_owned());
        cx.notify();
    }

    fn confirm_delete(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        if !self.requests.iter().any(|request| request.id == id) {
            self.show_notice("请求不存在", "所选请求已不存在。".into(), true, window, cx);
            cx.notify();
            return;
        }
        self.capture_active(cx);
        let previous_requests = self.requests.clone();
        let previous_folders = self.folders.clone();
        let deleting_last_request = self.requests.len() == 1;
        if deleting_last_request {
            self.requests = vec![SavedRequest::new("未命名请求")];
        } else {
            self.requests.retain(|request| request.id != id);
        }
        if !self.commit_tree_change(previous_requests, previous_folders, window, cx) {
            return;
        }
        self.pending_delete_id = None;
        self.renaming_request_id = None;
        self.rename_input = None;
        if self.active_request_id == id {
            let request = self.requests[0].clone();
            self.active_request_id = request.id.clone();
            self.active_folder = request.folder.clone();
            self.reveal_folder(&request.folder);
            self.apply_request(&request, window, cx);
        }
        if deleting_last_request {
            self.show_notice(
                "已删除",
                "最后一个请求已删除，已创建新的空白请求。".into(),
                false,
                window,
                cx,
            );
        }
        cx.notify();
    }

    fn copy_request(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.capture_active(cx);
        let Some(request) = self.requests.iter().find(|request| request.id == id) else {
            return;
        };
        match serde_json::to_string_pretty(request) {
            Ok(contents) => {
                cx.write_to_clipboard(ClipboardItem::new_string(contents));
                self.show_notice(
                    "已复制",
                    "请求定义已复制到剪贴板。".into(),
                    false,
                    window,
                    cx,
                );
            }
            Err(error) => self.show_notice(
                "复制失败",
                format!("序列化请求失败：{error}"),
                true,
                window,
                cx,
            ),
        }
        cx.notify();
    }

    fn copy_curl(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.capture_active(cx);
        let Some(request) = self.requests.iter().find(|request| request.id == id) else {
            return;
        };
        let result = self
            .active_variables()
            .and_then(|variables| generate_curl(request, &variables));
        match result {
            Ok(command) => {
                cx.write_to_clipboard(ClipboardItem::new_string(command));
                self.show_notice(
                    "已生成代码",
                    "cURL 代码已复制到剪贴板。".into(),
                    false,
                    window,
                    cx,
                );
            }
            Err(error) => self.show_notice("生成失败", error, true, window, cx),
        }
        cx.notify();
    }

    fn show_request_info(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.capture_active(cx);
        let Some(request) = self.requests.iter().find(|request| request.id == id) else {
            return;
        };
        let body = format!(
            "名称：{}\n方法：{}\n地址：{}\n创建时间：{}\n更新时间：{}\n请求 ID：{}",
            request.name,
            request.method,
            if request.url.is_empty() {
                "（未设置）"
            } else {
                &request.url
            },
            format_timestamp(request.created_at),
            format_timestamp(request.updated_at),
            request.id,
        );
        self.show_notice("请求信息", body, false, window, cx);
        cx.notify();
    }

    fn reveal_request_file(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(store) = &self.store else {
            self.show_notice(
                "无法打开目录",
                self.store_error
                    .clone()
                    .unwrap_or_else(|| "请求数据目录不可用。".into()),
                true,
                window,
                cx,
            );
            cx.notify();
            return;
        };
        let path = store.path().to_path_buf();
        #[cfg(target_os = "windows")]
        let result = Command::new("explorer.exe")
            .arg(format!("/select,{}", path.display()))
            .spawn();
        #[cfg(target_os = "macos")]
        let result = Command::new("open").arg("-R").arg(&path).spawn();
        #[cfg(all(not(target_os = "windows"), not(target_os = "macos")))]
        let result = Command::new("xdg-open")
            .arg(path.parent().unwrap_or(path.as_path()))
            .spawn();
        match result {
            Ok(_) => self.show_notice(
                "已打开目录",
                format!("请求数据文件：{}", path.display()),
                false,
                window,
                cx,
            ),
            Err(error) => self.show_notice(
                "打开目录失败",
                format!("无法启动文件管理器：{error}"),
                true,
                window,
                cx,
            ),
        }
        cx.notify();
    }

    fn begin_create_folder(&mut self, parent: &str, window: &mut Window, cx: &mut Context<Self>) {
        if !self.select_folder(parent, window, cx) {
            return;
        }
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("文件夹名称"));
        self.folder_input = Some(input);
        self.folder_parent = parent.to_owned();
        self.folder_edit_source = None;
        self.creating_folder = true;
        cx.notify();
    }

    fn begin_rename_folder(&mut self, folder: &str, window: &mut Window, cx: &mut Context<Self>) {
        if !self.select_folder(folder, window, cx) {
            return;
        }
        self.folder_input = Some(cx.new(|cx| {
            InputState::new(window, cx).default_value(tree::folder_name(folder).to_owned())
        }));
        self.folder_parent = tree::folder_parent(folder).to_owned();
        self.folder_edit_source = Some(folder.to_owned());
        self.creating_folder = true;
        cx.notify();
    }

    fn finish_create_folder(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(input) = &self.folder_input else {
            return;
        };
        let name = input.read(cx).value().trim().to_owned();
        if let Err(error) = tree::validate_folder_name(&name) {
            self.show_notice("文件夹名称无效", error, true, window, cx);
            cx.notify();
            return;
        }
        let path = if self.folder_parent.is_empty() {
            name
        } else {
            format!("{}/{}", self.folder_parent, name)
        };
        let source = self.folder_edit_source.clone();
        if source.is_none()
            && self
                .folders
                .iter()
                .any(|folder| folder.eq_ignore_ascii_case(&path))
        {
            self.show_notice(
                "文件夹已存在",
                format!("“{path}”已经存在。"),
                true,
                window,
                cx,
            );
            cx.notify();
            return;
        }
        self.capture_active(cx);
        let previous_requests = self.requests.clone();
        let previous_folders = self.folders.clone();
        if let Some(source) = &source {
            if let Err(error) =
                tree::relocate_folder(&mut self.requests, &mut self.folders, source, &path)
            {
                self.show_notice("重命名失败", error, true, window, cx);
                cx.notify();
                return;
            }
        } else {
            self.folders.push(path.clone());
        }
        if !self.commit_tree_change(previous_requests, previous_folders, window, cx) {
            return;
        }
        if let Some(source) = &source {
            self.remap_tree_navigation(source, &path);
        }
        self.active_folder = path.clone();
        self.reveal_folder(&path);
        self.folder_input = None;
        self.creating_folder = false;
        self.folder_parent.clear();
        self.folder_edit_source = None;
        cx.notify();
    }

    fn move_request(
        &mut self,
        id: &str,
        folder: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.drop_tree_item(&TreeItem::Request(id.to_owned()), folder, None, window, cx);
    }

    fn toggle_folder(&mut self, folder: &str, cx: &mut Context<Self>) {
        if !self.expanded_folders.remove(folder) {
            self.expanded_folders.insert(folder.to_owned());
        }
        cx.notify();
    }

    fn reveal_folder(&mut self, folder: &str) {
        self.expanded_folders.extend(tree::folder_ancestors(folder));
    }

    fn select_folder(&mut self, folder: &str, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if !folder.is_empty() && !self.folders.iter().any(|existing| existing == folder) {
            self.show_notice(
                "文件夹不存在",
                "所选文件夹不存在，请重新选择。".into(),
                true,
                window,
                cx,
            );
            cx.notify();
            return false;
        }
        self.active_folder = folder.to_owned();
        self.reveal_folder(folder);
        cx.notify();
        true
    }

    fn commit_tree_change(
        &mut self,
        previous_requests: Vec<SavedRequest>,
        previous_folders: Vec<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.persist_requests(window, cx) {
            self.requests = previous_requests;
            self.folders = previous_folders;
            cx.notify();
            return false;
        }
        true
    }

    fn remap_tree_navigation(&mut self, source: &str, destination: &str) {
        self.active_folder = tree::remap_folder(&self.active_folder, source, destination);
        self.expanded_folders = self
            .expanded_folders
            .iter()
            .map(|folder| tree::remap_folder(folder, source, destination))
            .collect();
        self.folder_parent = tree::remap_folder(&self.folder_parent, source, destination);
        self.folder_edit_source = self
            .folder_edit_source
            .as_ref()
            .map(|folder| tree::remap_folder(folder, source, destination));
        self.pending_delete_folder = self
            .pending_delete_folder
            .as_ref()
            .map(|folder| tree::remap_folder(folder, source, destination));
    }

    fn drop_tree_item(
        &mut self,
        item: &TreeItem,
        parent: &str,
        before: Option<&TreeItem>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.capture_active(cx);
        let previous_requests = self.requests.clone();
        let previous_folders = self.folders.clone();
        let moved =
            match tree::move_item(&mut self.requests, &mut self.folders, item, parent, before) {
                Ok(moved) => moved,
                Err(error) => {
                    self.show_notice("无法移动", error, true, window, cx);
                    cx.notify();
                    return;
                }
            };
        if !self.commit_tree_change(previous_requests, previous_folders, window, cx) {
            return;
        }
        match moved {
            TreeItem::Request(id) => {
                if let Some(request) = self
                    .requests
                    .iter()
                    .find(|request| request.id == id)
                    .cloned()
                {
                    self.active_request_id = id;
                    self.active_folder = request.folder.clone();
                    self.reveal_folder(&request.folder);
                    self.apply_request(&request, window, cx);
                }
            }
            TreeItem::Folder(destination) => {
                if let TreeItem::Folder(source) = item {
                    self.remap_tree_navigation(source, &destination);
                }
                self.active_folder = destination.clone();
                self.reveal_folder(&destination);
            }
        }
        cx.notify();
    }

    fn confirm_delete_folder(&mut self, folder: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.capture_active(cx);
        let previous_requests = self.requests.clone();
        let previous_folders = self.folders.clone();
        if let Err(error) = tree::delete_folder(&mut self.requests, &mut self.folders, folder) {
            self.show_notice("删除失败", error, true, window, cx);
            cx.notify();
            return;
        }
        if self.requests.is_empty() {
            self.requests.push(SavedRequest::new("未命名请求"));
        }
        if !self.commit_tree_change(previous_requests, previous_folders, window, cx) {
            return;
        }
        if tree::in_folder(&self.active_folder, folder) {
            self.active_folder = tree::folder_parent(folder).to_owned();
        }
        self.expanded_folders
            .retain(|path| !tree::in_folder(path, folder));
        if !self
            .requests
            .iter()
            .any(|request| request.id == self.active_request_id)
        {
            let request = self.requests[0].clone();
            self.active_request_id = request.id.clone();
            self.active_folder = request.folder.clone();
            self.reveal_folder(&request.folder);
            self.apply_request(&request, window, cx);
        }
        if tree::in_folder(&self.folder_parent, folder)
            || self
                .folder_edit_source
                .as_ref()
                .is_some_and(|path| tree::in_folder(path, folder))
        {
            self.creating_folder = false;
            self.folder_input = None;
            self.folder_edit_source = None;
        }
        self.pending_delete_folder = None;
        self.renaming_request_id = None;
        self.rename_input = None;
        self.pending_delete_id = None;
        cx.notify();
    }

    fn render_request_row(&self, request: &SavedRequest, cx: &mut Context<Self>) -> AnyElement {
        let id = request.id.clone();
        let request_id = id.clone();
        let weak_view = cx.entity().downgrade();
        let is_selected = id == self.active_request_id;
        let method_color = method_color(&request.method);

        let row = if self.renaming_request_id.as_deref() == Some(id.as_str()) {
            if let Some(input) = &self.rename_input {
                div()
                    .id(format!("request-row-{id}"))
                    .test_support()
                    .flex()
                    .items_center()
                    .gap_1()
                    .px_2()
                    .py_1()
                    .child(
                        div().flex_1().child(
                            Input::new(input)
                                .id(format!("request-name-{id}"))
                                .text_color(rgb(0x111827))
                                .bg(rgb(0xffffff)),
                        ),
                    )
                    .child(
                        Button::new(format!("save-rename-{id}"))
                            .primary()
                            .label("保存")
                            .on_click({
                                let weak_view = weak_view.clone();
                                let id = id.clone();
                                move |_, window, cx| {
                                    let _ = weak_view.update(cx, |this, cx| {
                                        this.finish_rename(&id, window, cx);
                                    });
                                }
                            }),
                    )
                    .child(
                        Button::new(format!("cancel-rename-{id}"))
                            .label("取消")
                            .on_click({
                                let weak_view = weak_view.clone();
                                move |_, _, cx| {
                                    let _ = weak_view.update(cx, |this, cx| {
                                        this.renaming_request_id = None;
                                        this.rename_input = None;
                                        cx.notify();
                                    });
                                }
                            }),
                    )
                    .into_any_element()
            } else {
                div().into_any_element()
            }
        } else if self.pending_delete_id.as_deref() == Some(id.as_str()) {
            div()
                .id(format!("request-row-{id}"))
                .flex()
                .items_center()
                .gap_1()
                .px_2()
                .py_2()
                .child(
                    div()
                        .flex_1()
                        .text_size(px(11.))
                        .text_color(rgb(0xfca5a5))
                        .child("确认删除此请求？"),
                )
                .child(
                    Button::new(format!("confirm-delete-{id}"))
                        .label("删除")
                        .on_click({
                            let weak_view = weak_view.clone();
                            let id = id.clone();
                            move |_, window, cx| {
                                let _ = weak_view.update(cx, |this, cx| {
                                    this.confirm_delete(&id, window, cx);
                                });
                            }
                        }),
                )
                .child(
                    Button::new(format!("cancel-delete-{id}"))
                        .label("取消")
                        .on_click({
                            let weak_view = weak_view.clone();
                            move |_, _, cx| {
                                let _ = weak_view.update(cx, |this, cx| {
                                    this.pending_delete_id = None;
                                    cx.notify();
                                });
                            }
                        }),
                )
                .into_any_element()
        } else {
            div()
                .id(format!("request-row-{id}"))
                .test_support()
                .flex()
                .items_center()
                .gap_2()
                .rounded_md()
                .when(is_selected, |this| this.bg(rgb(0x292a2e)))
                .px_3()
                .py_2()
                .text_size(px(12.))
                .cursor_pointer()
                .on_drag(
                    TreeDrag {
                        item: TreeItem::Request(id.clone()),
                        label: format!("{} {}", request.method, request.name),
                    },
                    |drag, _, _, cx| {
                        cx.stop_propagation();
                        cx.new(|_| drag.clone())
                    },
                )
                .drag_over::<TreeDrag>(|style, _, _, _| {
                    style.border_t_2().border_color(rgb(0xe67e45))
                })
                .on_drop({
                    let weak_view = weak_view.clone();
                    let target = TreeItem::Request(id.clone());
                    let parent = request.folder.clone();
                    move |drag: &TreeDrag, window, cx| {
                        cx.stop_propagation();
                        let _ = weak_view.update(cx, |this, cx| {
                            this.drop_tree_item(&drag.item, &parent, Some(&target), window, cx);
                        });
                    }
                })
                .on_click({
                    let weak_view = weak_view.clone();
                    let id = request_id.clone();
                    move |_, window, cx| {
                        let _ = weak_view.update(cx, |this, cx| {
                            this.select_request(&id, window, cx);
                        });
                    }
                })
                .child(
                    div()
                        .text_size(px(10.))
                        .font_weight(FontWeight::BOLD)
                        .text_color(method_color)
                        .child(request.method.clone()),
                )
                .child(
                    div()
                        .flex_1()
                        .overflow_hidden()
                        .text_color(rgb(0xd4d4d8))
                        .child(request.name.clone()),
                )
                .context_menu({
                    let id = id.clone();
                    let weak_view = weak_view.clone();
                    let folders = self.folders.clone();
                    move |menu, window, cx| {
                        menu.submenu("移动到文件夹", window, cx, {
                            let weak_view = weak_view.clone();
                            let id = id.clone();
                            let folders = folders.clone();
                            move |menu, _, _| {
                                let menu = menu.item(PopupMenuItem::new("根目录").on_click({
                                    let weak_view = weak_view.clone();
                                    let id = id.clone();
                                    move |_, window, cx| {
                                        let _ = weak_view.update(cx, |this, cx| {
                                            this.move_request(&id, "", window, cx);
                                        });
                                    }
                                }));
                                folders.iter().fold(menu, |menu, folder| {
                                    menu.item(
                                        PopupMenuItem::new(folder.replace('/', " / ")).on_click({
                                            let weak_view = weak_view.clone();
                                            let id = id.clone();
                                            let folder = folder.clone();
                                            move |_, window, cx| {
                                                let _ = weak_view.update(cx, |this, cx| {
                                                    this.move_request(&id, &folder, window, cx);
                                                });
                                            }
                                        }),
                                    )
                                })
                            }
                        })
                        .item(PopupMenuItem::new("克隆").on_click({
                            let weak_view = weak_view.clone();
                            let id = id.clone();
                            move |_, window, cx| {
                                let _ = weak_view.update(cx, |this, cx| {
                                    this.duplicate_request(&id, "副本 - ", window, cx);
                                });
                            }
                        }))
                        .item(PopupMenuItem::new("复制请求内容").on_click({
                            let weak_view = weak_view.clone();
                            let id = id.clone();
                            move |_, window, cx| {
                                let _ = weak_view.update(cx, |this, cx| {
                                    this.copy_request(&id, window, cx);
                                });
                            }
                        }))
                        .item(PopupMenuItem::new("重命名").on_click({
                            let weak_view = weak_view.clone();
                            let id = id.clone();
                            move |_, window, cx| {
                                let _ = weak_view.update(cx, |this, cx| {
                                    this.begin_rename(&id, window, cx);
                                });
                            }
                        }))
                        .item(PopupMenuItem::new("生成 cURL 代码").on_click({
                            let weak_view = weak_view.clone();
                            let id = id.clone();
                            move |_, window, cx| {
                                let _ = weak_view.update(cx, |this, cx| {
                                    this.copy_curl(&id, window, cx);
                                });
                            }
                        }))
                        .item(PopupMenuItem::new("创建请求示例").on_click({
                            let weak_view = weak_view.clone();
                            let id = id.clone();
                            move |_, window, cx| {
                                let _ = weak_view.update(cx, |this, cx| {
                                    this.duplicate_request(&id, "示例 - ", window, cx);
                                });
                            }
                        }))
                        .item(PopupMenuItem::new("在文件资源管理器中显示").on_click({
                            let weak_view = weak_view.clone();
                            move |_, window, cx| {
                                let _ = weak_view.update(cx, |this, cx| {
                                    this.reveal_request_file(window, cx);
                                });
                            }
                        }))
                        .item(PopupMenuItem::new("信息").on_click({
                            let weak_view = weak_view.clone();
                            let id = id.clone();
                            move |_, window, cx| {
                                let _ = weak_view.update(cx, |this, cx| {
                                    this.show_request_info(&id, window, cx);
                                });
                            }
                        }))
                        .separator()
                        .item(PopupMenuItem::new("删除").on_click({
                            let weak_view = weak_view.clone();
                            let id = id.clone();
                            move |_, _, cx| {
                                let _ = weak_view.update(cx, |this, cx| {
                                    this.request_delete(&id, cx);
                                });
                            }
                        }))
                    }
                })
                .into_any_element()
        };

        row
    }

    fn render_folder_creator(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(input) = &self.folder_input else {
            return div().into_any_element();
        };
        div()
            .px_2()
            .py_1()
            .flex()
            .flex_col()
            .gap_1()
            .child(
                div()
                    .text_size(px(11.))
                    .text_color(rgb(0xe4e4e7))
                    .child(format!(
                        "{}：{}",
                        if self.folder_edit_source.is_some() {
                            "重命名文件夹"
                        } else {
                            "新建文件夹"
                        },
                        if self.folder_parent.is_empty() {
                            "根目录"
                        } else {
                            &self.folder_parent
                        },
                    )),
            )
            .child(
                div()
                    .flex()
                    .gap_1()
                    .child(
                        div().flex_1().child(
                            Input::new(input)
                                .id("folder-name")
                                .text_color(rgb(0x111827))
                                .bg(rgb(0xffffff)),
                        ),
                    )
                    .child(
                        Button::new("save-folder")
                            .primary()
                            .label(if self.folder_edit_source.is_some() {
                                "保存"
                            } else {
                                "添加"
                            })
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.finish_create_folder(window, cx);
                            })),
                    )
                    .child(
                        Button::new("cancel-folder")
                            .label("取消")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.creating_folder = false;
                                this.folder_input = None;
                                this.folder_parent.clear();
                                this.folder_edit_source = None;
                                cx.notify();
                            })),
                    ),
            )
            .into_any_element()
    }

    fn render_drop_marker(
        &self,
        parent: &str,
        before: TreeItem,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let weak_view = cx.entity().downgrade();
        let parent = parent.to_owned();
        div()
            .id(format!("drop-before-{before:?}"))
            .test_support()
            .h(px(5.))
            .w_full()
            .drag_over::<TreeDrag>(|style, _, _, _| style.bg(rgb(0xe67e45)))
            .on_drop(move |drag: &TreeDrag, window, cx| {
                cx.stop_propagation();
                let _ = weak_view.update(cx, |this, cx| {
                    this.drop_tree_item(&drag.item, &parent, Some(&before), window, cx);
                });
            })
            .into_any_element()
    }

    fn render_delete_folder_confirmation(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(folder) = self.pending_delete_folder.as_ref() else {
            return div().into_any_element();
        };
        let request_count = self
            .requests
            .iter()
            .filter(|request| tree::in_folder(&request.folder, folder))
            .count();
        let folder = folder.clone();
        div()
            .mx_2()
            .my_2()
            .p_2()
            .border_1()
            .border_color(rgb(0xf87171))
            .flex()
            .flex_col()
            .gap_2()
            .text_size(px(11.))
            .text_color(rgb(0xfca5a5))
            .child(format!(
                "删除“{folder}”及其全部子文件夹、{request_count} 个请求？"
            ))
            .child(
                div()
                    .flex()
                    .gap_2()
                    .child(
                        Button::new("confirm-delete-folder")
                            .label("确认删除")
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.confirm_delete_folder(&folder, window, cx);
                            })),
                    )
                    .child(Button::new("cancel-delete-folder").label("取消").on_click(
                        cx.listener(|this, _, _, cx| {
                            this.pending_delete_folder = None;
                            cx.notify();
                        }),
                    )),
            )
            .into_any_element()
    }

    fn render_folder_tree(
        &self,
        parent: &str,
        query: &str,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let prefix = if parent.is_empty() {
            String::new()
        } else {
            format!("{parent}/")
        };
        let child_folders: Vec<_> = self
            .folders
            .iter()
            .filter_map(|folder| {
                let remainder = folder.strip_prefix(&prefix)?;
                (!remainder.is_empty() && !remainder.contains('/')).then(|| folder.clone())
            })
            .collect();
        let mut elements = Vec::new();

        for folder in child_folders {
            let folder_matches = folder.to_lowercase().contains(query);
            let has_visible_request = self.requests.iter().any(|request| {
                (request.folder == folder || request.folder.starts_with(&format!("{folder}/")))
                    && (folder_matches || request_matches_query(request, query))
            });
            if !query.is_empty() && !has_visible_request && !folder_matches {
                continue;
            }

            let weak_view = cx.entity().downgrade();
            let expanded = self.expanded_folders.contains(&folder) || !query.is_empty();
            let depth = folder.matches('/').count() as f32 + 1.;
            elements.push(self.render_drop_marker(parent, TreeItem::Folder(folder.clone()), cx));
            let folder_row = div()
                .id(format!("folder-{folder}"))
                .test_support()
                .flex()
                .items_center()
                .gap_2()
                .pl(px(depth * 10.))
                .pr_2()
                .py_1()
                .rounded_md()
                .cursor_pointer()
                .text_size(px(12.))
                .font_weight(FontWeight::MEDIUM)
                .text_color(rgb(0xd4d4d8))
                .when(self.active_folder == folder, |this| this.bg(rgb(0x292a2e)))
                .on_drag(
                    TreeDrag {
                        item: TreeItem::Folder(folder.clone()),
                        label: format!("文件夹 {}", folder.replace('/', " / ")),
                    },
                    |drag, _, _, cx| {
                        cx.stop_propagation();
                        cx.new(|_| drag.clone())
                    },
                )
                .drag_over::<TreeDrag>(|style, _, _, _| {
                    style
                        .bg(rgb(0x453327))
                        .border_1()
                        .border_color(rgb(0xe67e45))
                })
                .on_drop({
                    let folder = folder.clone();
                    let weak_view = weak_view.clone();
                    move |drag: &TreeDrag, window, cx| {
                        cx.stop_propagation();
                        let _ = weak_view.update(cx, |this, cx| {
                            this.drop_tree_item(&drag.item, &folder, None, window, cx);
                        });
                    }
                })
                .on_click({
                    let folder = folder.clone();
                    let weak_view = weak_view.clone();
                    move |_, window, cx| {
                        let _ = weak_view.update(cx, |this, cx| {
                            this.select_folder(&folder, window, cx);
                        });
                    }
                })
                .child(
                    div()
                        .id(format!("toggle-{folder}"))
                        .test_support()
                        .px_1()
                        .on_click({
                            let folder = folder.clone();
                            let weak_view = weak_view.clone();
                            move |_, _, cx| {
                                cx.stop_propagation();
                                let _ = weak_view.update(cx, |this, cx| {
                                    this.toggle_folder(&folder, cx);
                                });
                            }
                        })
                        .child(if expanded { "▾" } else { "▸" }),
                )
                .child(folder_leaf_name(&folder).to_owned())
                .child(
                    div()
                        .ml_auto()
                        .text_size(px(10.))
                        .text_color(rgb(0x85868b))
                        .child(
                            self.requests
                                .iter()
                                .filter(|request| tree::in_folder(&request.folder, &folder))
                                .count()
                                .to_string(),
                        ),
                )
                .context_menu({
                    let folder = folder.clone();
                    let weak_view = weak_view.clone();
                    move |menu, _, _| {
                        menu.item(PopupMenuItem::new("新建请求").on_click({
                            let folder = folder.clone();
                            let weak_view = weak_view.clone();
                            move |_, window, cx| {
                                let _ = weak_view.update(cx, |this, cx| {
                                    if this.select_folder(&folder, window, cx) {
                                        this.create_request(window, cx);
                                    }
                                });
                            }
                        }))
                        .item(PopupMenuItem::new("新建子文件夹").on_click({
                            let folder = folder.clone();
                            let weak_view = weak_view.clone();
                            move |_, window, cx| {
                                let _ = weak_view.update(cx, |this, cx| {
                                    this.begin_create_folder(&folder, window, cx);
                                });
                            }
                        }))
                        .item(PopupMenuItem::new("重命名").on_click({
                            let folder = folder.clone();
                            let weak_view = weak_view.clone();
                            move |_, window, cx| {
                                let _ = weak_view.update(cx, |this, cx| {
                                    this.begin_rename_folder(&folder, window, cx);
                                });
                            }
                        }))
                        .item(PopupMenuItem::new("移动到根目录").on_click({
                            let folder = folder.clone();
                            let weak_view = weak_view.clone();
                            move |_, window, cx| {
                                let _ = weak_view.update(cx, |this, cx| {
                                    this.drop_tree_item(
                                        &TreeItem::Folder(folder.clone()),
                                        "",
                                        None,
                                        window,
                                        cx,
                                    );
                                });
                            }
                        }))
                        .separator()
                        .item(PopupMenuItem::new("删除文件夹").on_click({
                            let folder = folder.clone();
                            let weak_view = weak_view.clone();
                            move |_, _, cx| {
                                let _ = weak_view.update(cx, |this, cx| {
                                    this.pending_delete_folder = Some(folder.clone());
                                    cx.notify();
                                });
                            }
                        }))
                    }
                });
            elements.push(folder_row.into_any_element());

            if !expanded {
                continue;
            }
            let direct_requests: Vec<_> = self
                .requests
                .iter()
                .filter(|request| request.folder == folder)
                .filter(|request| query.is_empty() || request_matches_query(request, query))
                .collect();
            elements.extend(direct_requests.into_iter().map(|request| {
                div()
                    .pl(px(depth * 12.))
                    .child(self.render_request_row(request, cx))
                    .into_any_element()
            }));
            elements.extend(self.render_folder_tree(&folder, query, cx));
        }
        elements
    }

    fn render_request_list(&self, cx: &mut Context<Self>) -> AnyElement {
        let query = self.search.read(cx).value().to_string().to_lowercase();
        let root_requests: Vec<_> = self
            .requests
            .iter()
            .filter(|request| request.folder.is_empty())
            .filter(|request| query.is_empty() || request_matches_query(request, &query))
            .collect();
        let mut list = div().flex().flex_col().gap_1().px_2();
        let weak_view = cx.entity().downgrade();
        let root_has_matches = !root_requests.is_empty();
        list = list.child(
            div()
                .id("tree-root")
                .test_support()
                .px_2()
                .pt_2()
                .pb_1()
                .rounded_md()
                .cursor_pointer()
                .when(self.active_folder.is_empty(), |this| this.bg(rgb(0x292a2e)))
                .text_size(px(11.))
                .text_color(rgb(0x85868b))
                .drag_over::<TreeDrag>(|style, _, _, _| {
                    style
                        .bg(rgb(0x453327))
                        .border_1()
                        .border_color(rgb(0xe67e45))
                })
                .on_drop({
                    let weak_view = weak_view.clone();
                    move |drag: &TreeDrag, window, cx| {
                        cx.stop_propagation();
                        let _ = weak_view.update(cx, |this, cx| {
                            this.drop_tree_item(&drag.item, "", None, window, cx);
                        });
                    }
                })
                .on_click({
                    let weak_view = weak_view.clone();
                    move |_, window, cx| {
                        let _ = weak_view.update(cx, |this, cx| {
                            this.select_folder("", window, cx);
                        });
                    }
                })
                .child("根目录"),
        );
        list = list.children(
            root_requests
                .iter()
                .map(|request| self.render_request_row(request, cx)),
        );
        let folder_elements = self.render_folder_tree("", &query, cx);
        let has_matches = root_has_matches || !folder_elements.is_empty();
        list = list.children(folder_elements);

        if !has_matches {
            list = list.child(
                div()
                    .px_3()
                    .py_3()
                    .text_size(px(11.))
                    .text_color(rgb(0x85868b))
                    .child("没有匹配的请求"),
            );
        }
        list = list.child(
            div()
                .id("drop-to-root")
                .test_support()
                .min_h(px(42.))
                .p_2()
                .mt_2()
                .rounded_md()
                .text_size(px(10.))
                .text_color(rgb(0x77787e))
                .drag_over::<TreeDrag>(|style, _, _, _| {
                    style
                        .bg(rgb(0x453327))
                        .border_1()
                        .border_color(rgb(0xe67e45))
                })
                .on_drop(move |drag: &TreeDrag, window, cx| {
                    cx.stop_propagation();
                    let _ = weak_view.update(cx, |this, cx| {
                        this.drop_tree_item(&drag.item, "", None, window, cx);
                    });
                })
                .child("拖到此处移至根目录"),
        );
        list.into_any_element()
    }

    fn render_copyable(
        &self,
        id: &'static str,
        state: &Entity<TextareaState>,
        dim: bool,
    ) -> impl IntoElement {
        let source = state.downgrade();
        div()
            .id(id)
            .test_support()
            .size_full()
            .context_menu(move |menu, _, _| {
                menu.item(PopupMenuItem::new("复制").on_click({
                    let source = source.clone();
                    move |_, _, cx| {
                        if let Some(state) = source.upgrade() {
                            let text = state.read(cx).value().to_string();
                            cx.write_to_clipboard(ClipboardItem::new_string(text));
                        }
                    }
                }))
            })
            .child(
                Textarea::new(state)
                    .size_full()
                    .text_color(if dim { rgb(0x8b8b91) } else { rgb(0xd4d4d8) })
                    .bg(rgb(0x17181b))
                    .context_menu(|menu, _, _| menu),
            )
    }

    fn render_assertion_results(&self) -> AnyElement {
        if self.assertion_results.is_empty() {
            return hint(
                "在请求的“断言”页添加断言（例如 res.status eq 200），发送后在此查看结果。",
            )
            .into_any_element();
        }
        div()
            .id("assertion-results")
            .size_full()
            .overflow_y_scrollbar()
            .flex()
            .flex_col()
            .gap_2()
            .children(
                self.assertion_results
                    .iter()
                    .enumerate()
                    .map(|(index, result)| {
                        div()
                            .id(SharedString::from(format!("assertion-result-{index}")))
                            .test_support()
                            .p_2()
                            .rounded_md()
                            .border_1()
                            .border_color(if result.passed {
                                rgb(0x245c3a)
                            } else {
                                rgb(0x7f2d2d)
                            })
                            .bg(if result.passed {
                                rgb(0x16271d)
                            } else {
                                rgb(0x2b1717)
                            })
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(
                                div()
                                    .flex()
                                    .gap_2()
                                    .text_size(px(12.))
                                    .child(
                                        div()
                                            .font_weight(FontWeight::BOLD)
                                            .text_color(if result.passed {
                                                rgb(0x4ade80)
                                            } else {
                                                rgb(0xf87171)
                                            })
                                            .child(if result.passed { "通过" } else { "失败" }),
                                    )
                                    .child(
                                        div()
                                            .text_color(rgb(0xe4e4e7))
                                            .child(result.expression.clone()),
                                    ),
                            )
                            .child(
                                div()
                                    .text_size(px(11.))
                                    .text_color(rgb(0xa1a1aa))
                                    .child(result.message.clone()),
                            )
                    }),
            )
            .into_any_element()
    }

    fn render_response_panel(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let passed = self.assertion_results.iter().filter(|r| r.passed).count();
        let total = self.assertion_results.len();
        let assertion_label = if total == 0 {
            "断言".to_string()
        } else {
            format!("断言 ({passed}/{total})")
        };
        let header_label = if self.response_header_count == 0 {
            "响应头".to_string()
        } else {
            format!("响应头 ({})", self.response_header_count)
        };
        let content = match self.response_tab {
            ResponseTab::Body => self
                .render_copyable(
                    "response-body",
                    &self.response_content,
                    self.response_status.is_none(),
                )
                .into_any_element(),
            ResponseTab::Headers => self
                .render_copyable("response-headers", &self.response_headers, false)
                .into_any_element(),
            ResponseTab::Timeline => self
                .render_copyable("response-timeline", &self.response_timeline, false)
                .into_any_element(),
            ResponseTab::Assertions => self.render_assertion_results(),
        };
        div()
            .flex_1()
            .min_h(px(0.))
            .flex()
            .flex_col()
            .child(
                div()
                    .h(px(36.))
                    .px_5()
                    .flex()
                    .items_end()
                    .gap_5()
                    .border_b_1()
                    .border_color(rgb(0x303136))
                    .child(tab_button(
                        "response-tab-body",
                        "响应体",
                        self.response_tab == ResponseTab::Body,
                        cx.listener(|this, _, _, cx| {
                            this.response_tab = ResponseTab::Body;
                            cx.notify();
                        }),
                    ))
                    .child(tab_button(
                        "response-tab-headers",
                        header_label,
                        self.response_tab == ResponseTab::Headers,
                        cx.listener(|this, _, _, cx| {
                            this.response_tab = ResponseTab::Headers;
                            cx.notify();
                        }),
                    ))
                    .child(tab_button(
                        "response-tab-timeline",
                        "时间线",
                        self.response_tab == ResponseTab::Timeline,
                        cx.listener(|this, _, _, cx| {
                            this.response_tab = ResponseTab::Timeline;
                            cx.notify();
                        }),
                    ))
                    .child(tab_button(
                        "response-tab-assertions",
                        assertion_label,
                        self.response_tab == ResponseTab::Assertions,
                        cx.listener(|this, _, _, cx| {
                            this.response_tab = ResponseTab::Assertions;
                            cx.notify();
                        }),
                    )),
            )
            .child(div().flex_1().min_h(px(0.)).px_4().py_3().child(content))
    }

    fn render_body_tab(&self, cx: &mut Context<Self>) -> AnyElement {
        let mut types = div().flex().flex_wrap().items_center().gap_2();
        for (index, body_type) in BodyType::ALL.into_iter().enumerate() {
            types = types.child(chip(
                SharedString::from(format!("body-type-{index}")),
                body_type.label(),
                self.body_type == body_type,
                cx.listener(move |this, _, window, cx| this.set_body_type(body_type, window, cx)),
            ));
        }
        if self.body_type == BodyType::Json {
            types =
                types.child(Button::new("format-json").label("格式化").on_click(
                    cx.listener(|this, _, window, cx| this.format_json_body(window, cx)),
                ));
        }
        let content = match self.body_type {
            BodyType::None => hint("此请求不发送请求体。").into_any_element(),
            body_type => div()
                .flex_1()
                .min_h(px(0.))
                .flex()
                .flex_col()
                .gap_2()
                .when(body_type.is_form(), |this| {
                    this.child(hint(if body_type == BodyType::Multipart {
                        "每行一个字段：名称=值；上传文件写成 名称=@文件路径；以 # 或 // 开头的行会被忽略。"
                    } else {
                        "每行一个字段：名称=值；以 # 或 // 开头的行会被忽略。"
                    }))
                })
                .child(div().flex_1().min_h(px(0.)).child(white_textarea(&self.body)))
                .into_any_element(),
        };
        div()
            .size_full()
            .flex()
            .flex_col()
            .gap_3()
            .child(types)
            .child(content)
            .into_any_element()
    }

    fn render_auth_tab(&self, cx: &mut Context<Self>) -> AnyElement {
        let mut types = div().flex().flex_wrap().gap_2();
        for (index, auth_type) in AuthType::ALL.into_iter().enumerate() {
            types = types.child(chip(
                SharedString::from(format!("auth-type-{index}")),
                auth_type.label(),
                self.auth_type == auth_type,
                cx.listener(move |this, _, window, cx| this.set_auth_type(auth_type, window, cx)),
            ));
        }
        let fields = match self.auth_type {
            AuthType::None => div().child(hint("此请求不使用认证。")),
            AuthType::Bearer => div()
                .flex()
                .flex_col()
                .gap_2()
                .child(field_label("令牌"))
                .child(white_input(&self.auth_token, "auth-token").mask_toggle())
                .child(hint("将自动添加请求头 Authorization: Bearer <令牌>。")),
            AuthType::Basic => div()
                .flex()
                .flex_col()
                .gap_2()
                .child(field_label("用户名"))
                .child(white_input(&self.auth_username, "auth-username"))
                .child(field_label("密码"))
                .child(white_input(&self.auth_password, "auth-password").mask_toggle()),
            AuthType::ApiKey => div()
                .flex()
                .flex_col()
                .gap_2()
                .child(field_label("键名"))
                .child(white_input(&self.auth_key, "auth-key"))
                .child(field_label("键值"))
                .child(white_input(&self.auth_value, "auth-value").mask_toggle())
                .child(field_label("添加到"))
                .child(div().flex().gap_2().children(
                    [ApiKeyLocation::Header, ApiKeyLocation::Query].map(|location| {
                        chip(
                            SharedString::from(match location {
                                ApiKeyLocation::Header => "api-key-header",
                                ApiKeyLocation::Query => "api-key-query",
                            }),
                            location.label(),
                            self.api_key_location == location,
                            cx.listener(move |this, _, window, cx| {
                                this.set_api_key_location(location, window, cx)
                            }),
                        )
                    }),
                )),
        };
        div()
            .id("auth-tab")
            .size_full()
            .overflow_y_scrollbar()
            .flex()
            .flex_col()
            .gap_3()
            .child(types)
            .child(fields)
            .child(hint(
                "认证字段支持 {{变量}}。若请求头中已手动设置 Authorization，则以请求头为准。",
            ))
            .into_any_element()
    }

    fn render_settings_tab(&self, cx: &mut Context<Self>) -> AnyElement {
        div()
            .id("settings-tab")
            .size_full()
            .overflow_y_scrollbar()
            .flex()
            .flex_col()
            .gap_3()
            .child(field_label("超时时间（毫秒）"))
            .child(
                div()
                    .w(px(220.))
                    .child(white_input(&self.timeout, "setting-timeout")),
            )
            .child(hint("填 0 表示不限制超时。"))
            .child(toggle(
                "setting-follow-redirects",
                "自动跟随重定向",
                self.follow_redirects,
                cx.listener(|this, _, window, cx| {
                    this.follow_redirects = !this.follow_redirects;
                    this.persist_active(window, cx);
                }),
            ))
            .child(toggle(
                "setting-verify-tls",
                "验证 SSL/TLS 证书",
                self.verify_tls,
                cx.listener(|this, _, window, cx| {
                    this.verify_tls = !this.verify_tls;
                    this.persist_active(window, cx);
                }),
            ))
            .into_any_element()
    }

    fn render_environment_tab(&self, cx: &mut Context<Self>) -> AnyElement {
        let active = self.environments.active.clone();
        let mut list = div().flex().flex_wrap().items_center().gap_2().child(chip(
            "env-chip-none",
            "无环境",
            active.is_none(),
            cx.listener(|this, _, window, cx| this.select_environment(None, window, cx)),
        ));
        for environment in &self.environments.environments {
            let id = environment.id.clone();
            list = list.child(chip(
                SharedString::from(format!("env-chip-{}", environment.id)),
                environment.name.clone(),
                active.as_ref() == Some(&environment.id),
                cx.listener(move |this, _, window, cx| {
                    this.select_environment(Some(id.clone()), window, cx)
                }),
            ));
        }
        let has_active = self.active_environment().is_some();
        let actions = div()
            .flex()
            .gap_2()
            .child(
                Button::new("env-create")
                    .label("新建环境")
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.begin_environment_edit(EnvEdit::Create, window, cx)
                    })),
            )
            .child(
                Button::new("env-rename")
                    .label("重命名")
                    .disabled(!has_active)
                    .on_click(cx.listener(|this, _, window, cx| {
                        if let Some(id) = this.environments.active.clone() {
                            this.begin_environment_edit(EnvEdit::Rename(id), window, cx);
                        }
                    })),
            )
            .child(
                Button::new("env-delete")
                    .label("删除")
                    .disabled(!has_active)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.pending_delete_env = true;
                        this.env_edit = None;
                        this.env_name_input = None;
                        cx.notify();
                    })),
            );

        let mut panel = div()
            .size_full()
            .flex()
            .flex_col()
            .gap_3()
            .child(list)
            .child(actions);
        if let Some(input) = &self.env_name_input {
            panel = panel.child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(div().w(px(220.)).child(white_input(input, "env-name")))
                    .child(Button::new("env-save").primary().label("保存").on_click(
                        cx.listener(|this, _, window, cx| this.save_environment_name(window, cx)),
                    ))
                    .child(
                        Button::new("env-cancel")
                            .label("取消")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.env_edit = None;
                                this.env_name_input = None;
                                cx.notify();
                            })),
                    ),
            );
        }
        if self.pending_delete_env
            && let Some(environment) = self.active_environment()
        {
            panel =
                panel.child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .text_size(px(12.))
                        .text_color(rgb(0xf87171))
                        .child(format!("确认删除环境“{}”？", environment.name))
                        .child(
                            Button::new("env-confirm-delete")
                                .danger()
                                .label("删除")
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.delete_active_environment(window, cx)
                                })),
                        )
                        .child(Button::new("env-cancel-delete").label("取消").on_click(
                            cx.listener(|this, _, _, cx| {
                                this.pending_delete_env = false;
                                cx.notify();
                            }),
                        )),
                );
        }
        panel = panel.child(hint(
            "在地址、参数、请求头、请求体、认证、断言中用 {{变量名}} 引用变量。内置变量：{{$timestamp}}、{{$isoTimestamp}}、{{$randomInt}}、{{$guid}}。",
        ));
        if has_active {
            panel = panel
                .child(hint(
                    "每行一个变量：名称=值；以 # 或 // 开头的行会被忽略。修改后自动保存。",
                ))
                .child(
                    div()
                        .flex_1()
                        .min_h(px(0.))
                        .child(white_textarea(&self.env_variables)),
                );
        } else {
            panel = panel.child(hint("当前未选择环境。新建或选择一个环境后即可编辑变量。"));
        }
        panel.into_any_element()
    }

    fn render_request_tab(&self, cx: &mut Context<Self>) -> AnyElement {
        match self.active_tab {
            RequestTab::Body => self.render_body_tab(cx),
            RequestTab::Headers => with_hint(
                "每行一个请求头：名称: 值；以 # 或 // 开头的行不会发送。",
                white_textarea(&self.headers),
            ),
            RequestTab::Auth => self.render_auth_tab(cx),
            RequestTab::Assertions => with_hint(
                "格式：目标 运算符 期望值。目标：res.status、res.responseTime、res.headers.名称、res.body、res.body.路径（如 res.body.data[0].id）。运算符：eq neq gt gte lt lte contains notContains startsWith endsWith exists notExists isEmpty isNotEmpty。",
                white_textarea(&self.assertions),
            ),
            RequestTab::Settings => self.render_settings_tab(cx),
            RequestTab::Environment => self.render_environment_tab(cx),
            RequestTab::Params => with_hint(
                "每行一个参数：名称=值，会自动编码并追加到地址；以 # 或 // 开头的行不会发送。",
                white_textarea(&self.params),
            ),
        }
    }

    fn render_environment_switcher(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let weak_view = cx.entity().downgrade();
        let label = match self.active_environment() {
            Some(environment) => format!("环境：{}", environment.name),
            None => "无环境".to_string(),
        };
        let active = self.environments.active.clone();
        let environments = self
            .environments
            .environments
            .iter()
            .map(|environment| (environment.id.clone(), environment.name.clone()))
            .collect::<Vec<_>>();
        Button::new("env-switcher")
            .label(label)
            .dropdown_menu(move |mut menu, _, _| {
                let entries = std::iter::once((None, "无环境".to_string())).chain(
                    environments
                        .iter()
                        .map(|(id, name)| (Some(id.clone()), name.clone())),
                );
                for (id, name) in entries {
                    let weak_view = weak_view.clone();
                    let checked = id == active;
                    menu = menu.item(PopupMenuItem::new(name).checked(checked).on_click(
                        move |_, window, cx| {
                            let id = id.clone();
                            let _ = weak_view.update(cx, |this, cx| {
                                this.select_environment(id, window, cx);
                            });
                        },
                    ));
                }
                let weak_view = weak_view.clone();
                menu.separator()
                    .item(PopupMenuItem::new("管理环境").on_click(move |_, _, cx| {
                        let _ = weak_view.update(cx, |this, cx| {
                            this.active_tab = RequestTab::Environment;
                            cx.notify();
                        });
                    }))
            })
    }
}
impl Render for ApiTester {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let status_color = if self.response_error {
            rgb(0xf87171)
        } else {
            rgb(0x4ade80)
        };
        let selected_method = self
            .method
            .read(cx)
            .selected_value()
            .copied()
            .unwrap_or("GET");
        let method_color = method_color(selected_method);
        let active_name = self
            .active_request()
            .map(|request| {
                if request.folder.is_empty() {
                    request.name.clone()
                } else {
                    format!("{} / {}", request.folder.replace('/', " / "), request.name)
                }
            })
            .unwrap_or_else(|| "未命名请求".into());

        div()
            .size_full()
            .flex()
            .pt(px(8.))
            .bg(rgb(0x1b1d21))
            .text_color(rgb(0xe4e4e7))
            .child(
                div()
                    .w(px(224.))
                    .h_full()
                    .flex()
                    .flex_col()
                    .bg(rgb(0x151619))
                    .border_r_1()
                    .border_color(rgb(0x303136))
                    .child(
                        div()
                            .h(px(56.))
                            .px_4()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(
                                div()
                                    .size(px(25.))
                                    .rounded(px(7.))
                                    .bg(rgb(0xe67e45))
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .text_color(rgb(0xffffff))
                                    .font_weight(FontWeight::BOLD)
                                    .child("M"),
                            )
                            .child(
                                div()
                                    .text_size(px(14.))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child("MZip API"),
                            )
                            .child(
                                div()
                                    .id("edition-badge")
                                    .test_support()
                                    .text_size(px(11.))
                                    .text_color(rgb(0xf5b48a))
                                    .child("大明哥专用版本 😊"),
                            ),
                    )
                    .child(div().h(px(1.)).bg(rgb(0x303136)))
                    .child(
                        div()
                            .px_4()
                            .pt_5()
                            .pb_2()
                            .flex()
                            .items_center()
                            .justify_between()
                            .text_size(px(11.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(rgb(0x85868b))
                            .child("本地请求")
                            .child(
                                div()
                                    .flex()
                                    .gap_1()
                                    .child(Button::new("create-folder").label("文件夹+").on_click(
                                        cx.listener(|this, _, window, cx| {
                                            let parent = this.active_folder.clone();
                                            this.begin_create_folder(&parent, window, cx);
                                        }),
                                    ))
                                    .child(Button::new("create-request").label("请求+").on_click(
                                        cx.listener(|this, _, window, cx| {
                                            this.create_request(window, cx);
                                        }),
                                    )),
                            ),
                    )
                    .child(
                        div().mx_2().my_2().child(
                            Input::new(&self.search)
                                .id("request-search")
                                .text_color(rgb(0x111827))
                                .bg(rgb(0xffffff)),
                        ),
                    )
                    .when(self.creating_folder, |this| {
                        this.child(self.render_folder_creator(cx))
                    })
                    .when(self.pending_delete_folder.is_some(), |this| {
                        this.child(self.render_delete_folder_confirmation(cx))
                    })
                    .child(
                        div()
                            .flex_1()
                            .min_h(px(0.))
                            .overflow_y_scrollbar()
                            .scrollbar_width(px(8.))
                            .child(self.render_request_list(cx)),
                    )
                    .child(
                        div()
                            .mt_auto()
                            .px_4()
                            .py_3()
                            .border_t_1()
                            .border_color(rgb(0x303136))
                            .text_size(px(11.))
                            .text_color(rgb(0x77787e))
                            .child(format!(
                                "当前位置：{}",
                                if self.active_folder.is_empty() {
                                    "根目录".to_owned()
                                } else {
                                    self.active_folder.replace('/', " / ")
                                }
                            )),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .h_full()
                    .flex()
                    .flex_col()
                    .bg(rgb(0x1b1d21))
                    .child(
                        div()
                            .h(px(42.))
                            .px_4()
                            .flex()
                            .items_center()
                            .gap_2()
                            .border_b_1()
                            .border_color(rgb(0x303136))
                            .bg(rgb(0x202125))
                            .child(
                                div()
                                    .id("active-request-tab")
                                    .test_support()
                                    .h_full()
                                    .min_w_0()
                                    .flex()
                                    .items_center()
                                    .gap_2()
                                    .px_3()
                                    .border_b_2()
                                    .border_color(rgb(0xe67e45))
                                    .text_size(px(12.))
                                    .cursor_pointer()
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        let id = this.active_request_id.clone();
                                        this.select_request(&id, window, cx);
                                    }))
                                    .child(
                                        div()
                                            .text_size(px(10.))
                                            .font_weight(FontWeight::BOLD)
                                            .text_color(method_color)
                                            .child(selected_method),
                                    )
                                    .child(div().truncate().child(active_name)),
                            )
                            .child(div().flex_1())
                            .child(self.render_environment_switcher(cx)),
                    )
                    .child(
                        div()
                            .px_5()
                            .py_4()
                            .border_b_1()
                            .border_color(rgb(0x303136))
                            .child(
                                div()
                                    .flex()
                                    .gap_2()
                                    .items_center()
                                    .rounded_lg()
                                    .border_1()
                                    .border_color(rgb(0x414247))
                                    .bg(rgb(0x25262a))
                                    .p_1()
                                    .child(div().w(px(132.)).child(Select::new(&self.method)))
                                    .child(
                                        div().flex_1().child(
                                            Input::new(&self.url)
                                                .id("request-url")
                                                .text_color(rgb(0x111827))
                                                .bg(rgb(0xffffff)),
                                        ),
                                    )
                                    .when(self.is_loading, |this| {
                                        this.child(
                                            Button::new("cancel-request").label("取消").on_click(
                                                cx.listener(|this, _, window, cx| {
                                                    this.cancel_request(window, cx);
                                                }),
                                            ),
                                        )
                                    })
                                    .child(
                                        Button::new("send-request")
                                            .primary()
                                            .px_5()
                                            .label(if self.is_loading {
                                                "发送中…"
                                            } else {
                                                "发送"
                                            })
                                            .disabled(self.is_loading)
                                            .on_click(cx.listener(|this, _, window, cx| {
                                                this.send_request(window, cx);
                                            })),
                                    ),
                            ),
                    )
                    .child(
                        div()
                            .flex_1()
                            .flex()
                            .min_w(px(0.))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w(px(0.))
                                    .flex()
                                    .flex_col()
                                    .child(
                                        div()
                                            .h(px(42.))
                                            .px_5()
                                            .flex()
                                            .items_center()
                                            .gap_5()
                                            .border_b_1()
                                            .border_color(rgb(0x303136))
                                            .child(tab_button(
                                                "request-tab-body",
                                                "请求体",
                                                self.active_tab == RequestTab::Body,
                                                cx.listener(|this, _, _, cx| {
                                                    this.active_tab = RequestTab::Body;
                                                    cx.notify();
                                                }),
                                            ))
                                            .child(tab_button(
                                                "request-tab-headers",
                                                "请求头",
                                                self.active_tab == RequestTab::Headers,
                                                cx.listener(|this, _, _, cx| {
                                                    this.active_tab = RequestTab::Headers;
                                                    cx.notify();
                                                }),
                                            ))
                                            .child(tab_button(
                                                "request-tab-auth",
                                                "认证",
                                                self.active_tab == RequestTab::Auth,
                                                cx.listener(|this, _, _, cx| {
                                                    this.active_tab = RequestTab::Auth;
                                                    cx.notify();
                                                }),
                                            ))
                                            .child(tab_button(
                                                "request-tab-assertions",
                                                "断言",
                                                self.active_tab == RequestTab::Assertions,
                                                cx.listener(|this, _, _, cx| {
                                                    this.active_tab = RequestTab::Assertions;
                                                    cx.notify();
                                                }),
                                            ))
                                            .child(tab_button(
                                                "request-tab-settings",
                                                "设置",
                                                self.active_tab == RequestTab::Settings,
                                                cx.listener(|this, _, _, cx| {
                                                    this.active_tab = RequestTab::Settings;
                                                    cx.notify();
                                                }),
                                            ))
                                            .child(tab_button(
                                                "request-tab-environment",
                                                "环境",
                                                self.active_tab == RequestTab::Environment,
                                                cx.listener(|this, _, _, cx| {
                                                    this.active_tab = RequestTab::Environment;
                                                    cx.notify();
                                                }),
                                            ))
                                            .child(tab_button(
                                                "request-tab-params",
                                                "参数",
                                                self.active_tab == RequestTab::Params,
                                                cx.listener(|this, _, _, cx| {
                                                    this.active_tab = RequestTab::Params;
                                                    cx.notify();
                                                }),
                                            )),
                                    )
                                    .child(
                                        div()
                                            .flex_1()
                                            .min_h(px(0.))
                                            .px_4()
                                            .py_3()
                                            .child(self.render_request_tab(cx)),
                                    ),
                            )
                            .child(div().w(px(1.)).h_full().bg(rgb(0x3a3b40)))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w(px(0.))
                                    .flex()
                                    .flex_col()
                                    .child(
                                        div()
                                            .h(px(42.))
                                            .px_5()
                                            .flex()
                                            .items_center()
                                            .justify_between()
                                            .border_b_1()
                                            .border_color(rgb(0x303136))
                                            .child(
                                                div()
                                                    .flex()
                                                    .items_center()
                                                    .gap_4()
                                                    .child(
                                                        div()
                                                            .text_size(px(12.))
                                                            .font_weight(FontWeight::SEMIBOLD)
                                                            .child("响应"),
                                                    )
                                                    .when_some(
                                                        self.response_status.as_ref(),
                                                        |this, status| {
                                                            this.child(
                                                                div()
                                                                    .text_size(px(11.))
                                                                    .font_weight(FontWeight::MEDIUM)
                                                                    .text_color(status_color)
                                                                    .child(status.clone()),
                                                            )
                                                        },
                                                    ),
                                            )
                                            .when_some(
                                                self.response_time.as_ref(),
                                                |this, elapsed| {
                                                    let mut meta = elapsed.clone();
                                                    if let Some(size) = &self.response_size {
                                                        meta.push_str(" · ");
                                                        meta.push_str(size);
                                                    }
                                                    this.child(
                                                        div()
                                                            .id("response-meta")
                                                            .test_support()
                                                            .text_size(px(11.))
                                                            .text_color(rgb(0x99999f))
                                                            .child(meta),
                                                    )
                                                },
                                            ),
                                    )
                                    .child(self.render_response_panel(cx)),
                            ),
                    ),
            )
    }
}

fn request_matches_query(request: &SavedRequest, query: &str) -> bool {
    query.is_empty()
        || request.name.to_lowercase().contains(query)
        || request.method.to_lowercase().contains(query)
        || request.url.to_lowercase().contains(query)
        || request.folder.to_lowercase().contains(query)
}

fn folder_leaf_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

fn method_color(method: &str) -> Rgba {
    match method {
        "POST" => rgb(0xf0a050),
        "PUT" => rgb(0x60a5fa),
        "PATCH" => rgb(0xc084fc),
        "DELETE" => rgb(0xf87171),
        "HEAD" => rgb(0xa1a1aa),
        _ => rgb(0x4ade80),
    }
}

fn timestamp_now() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn format_timestamp(timestamp: u64) -> String {
    format!("{timestamp}（Unix 毫秒）")
}

fn tab_button(
    id: &'static str,
    label: impl Into<SharedString>,
    active: bool,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    div()
        .id(id)
        .test_support()
        .cursor_pointer()
        .pb_2()
        .text_size(px(13.))
        .font_weight(if active {
            FontWeight::SEMIBOLD
        } else {
            FontWeight::NORMAL
        })
        .text_color(if active { rgb(0x58a6ff) } else { rgb(0x8b949e) })
        .border_b_2()
        .border_color(if active { rgb(0x1f6feb) } else { rgb(0x161b22) })
        .on_click(on_click)
        .child(label.into())
}

fn chip(
    id: impl Into<SharedString>,
    label: impl Into<SharedString>,
    active: bool,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    div()
        .id(id.into())
        .test_support()
        .cursor_pointer()
        .px_3()
        .py_1()
        .rounded_md()
        .border_1()
        .text_size(px(12.))
        .border_color(if active { rgb(0xe67e45) } else { rgb(0x414247) })
        .bg(if active { rgb(0x453327) } else { rgb(0x25262a) })
        .text_color(if active { rgb(0xf5b48a) } else { rgb(0xc4c4c8) })
        .hover(|style| style.border_color(rgb(0xe67e45)))
        .on_click(on_click)
        .child(label.into())
}

fn toggle(
    id: &'static str,
    label: &'static str,
    checked: bool,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    div()
        .id(id)
        .test_support()
        .cursor_pointer()
        .flex()
        .items_center()
        .gap_2()
        .text_size(px(13.))
        .text_color(rgb(0xe4e4e7))
        .on_click(on_click)
        .child(
            div()
                .size(px(16.))
                .rounded(px(4.))
                .border_1()
                .border_color(if checked {
                    rgb(0xe67e45)
                } else {
                    rgb(0x6b6c72)
                })
                .bg(if checked {
                    rgb(0xe67e45)
                } else {
                    rgb(0x25262a)
                })
                .flex()
                .items_center()
                .justify_center()
                .when(checked, |this| {
                    this.child(div().size(px(6.)).rounded_full().bg(rgb(0xffffff)))
                }),
        )
        .child(label)
}

fn hint(text: impl Into<SharedString>) -> gpui_kit::Div {
    div()
        .text_size(px(11.))
        .text_color(rgb(0x85868b))
        .child(text.into())
}

fn field_label(text: &'static str) -> impl IntoElement {
    div()
        .text_size(px(12.))
        .font_weight(FontWeight::MEDIUM)
        .text_color(rgb(0xc4c4c8))
        .child(text)
}

fn white_textarea(state: &Entity<TextareaState>) -> impl IntoElement {
    Textarea::new(state)
        .size_full()
        .text_color(rgb(0x111827))
        .bg(rgb(0xffffff))
}

fn white_input(state: &Entity<InputState>, id: &'static str) -> Input {
    Input::new(state)
        .id(id)
        .text_color(rgb(0x111827))
        .bg(rgb(0xffffff))
}

fn with_hint(text: &'static str, content: impl IntoElement) -> AnyElement {
    div()
        .size_full()
        .flex()
        .flex_col()
        .gap_2()
        .child(hint(text))
        .child(div().flex_1().min_h(px(0.)).child(content))
        .into_any_element()
}

#[cfg(all(test, feature = "ui-tests"))]
mod ui_tests;

const WINDOW_TITLE: &str = "MZip - API 测试工具";

pub fn run() {
    let _store_lock = match RequestStore::acquire_lock() {
        Ok(lock) => lock,
        Err(StoreLockError::AlreadyOpen) => {
            if !focus_existing_window() {
                rfd::MessageDialog::new()
                    .set_title(WINDOW_TITLE)
                    .set_description("API 测试工具已打开，请从任务栏切换到已有窗口。")
                    .set_level(rfd::MessageLevel::Info)
                    .show();
            }
            return;
        }
        Err(StoreLockError::Unavailable(error)) => {
            rfd::MessageDialog::new()
                .set_title(WINDOW_TITLE)
                .set_description(error)
                .set_level(rfd::MessageLevel::Error)
                .show();
            return;
        }
    };

    application().with_assets(assets::Assets).run(|cx| {
        init(cx);
        Theme::update(cx, |theme| {
            theme.colors.foreground = rgb(0x111827).into();
            theme.colors.muted_foreground = rgb(0x4b5563).into();
            theme.colors.background = rgb(0xffffff).into();
            theme.colors.input = rgb(0xcbd5e1).into();
            theme.colors.caret = rgb(0x111827).into();
        });
        Theme::set_scrollbar_mode(ScrollbarMode::Always, cx);
        let window_options = WindowOptions {
            titlebar: Some(TitlebarOptions {
                title: Some(WINDOW_TITLE.into()),
                appears_transparent: false,
                ..Default::default()
            }),
            window_min_size: Some(size(px(960.), px(640.))),
            window_bounds: Some(WindowBounds::Maximized(Bounds::centered(
                None,
                size(px(1280.), px(900.)),
                cx,
            ))),
            ..Default::default()
        };
        open_window(window_options, cx, |window, cx| {
            #[cfg(windows)]
            crate::window_icon::set_window_icon(window).expect("无法设置 API 测试工具窗口图标");
            cx.new(|cx| ApiTester::new(window, cx))
        })
        .expect("无法打开 API 测试工具窗口");
    });
}

#[cfg(windows)]
fn focus_existing_window() -> bool {
    use windows::{
        Win32::UI::WindowsAndMessaging::{
            FindWindowW, IsIconic, SW_RESTORE, SetForegroundWindow, ShowWindow,
        },
        core::HSTRING,
    };

    let Ok(window) = (unsafe { FindWindowW(None, &HSTRING::from(WINDOW_TITLE)) }) else {
        return false;
    };
    unsafe {
        if IsIconic(window).as_bool() {
            let _ = ShowWindow(window, SW_RESTORE);
        }
        SetForegroundWindow(window).as_bool()
    }
}

#[cfg(not(windows))]
fn focus_existing_window() -> bool {
    false
}
