use super::{ApiTester, RequestStore, SavedRequest, TreeItem};
use gpui_kit::{
    AnyWindowHandle, App, AppContext, Bounds, Entity, Point, TestAppContext, Window, WindowBounds,
    WindowOptions, init, open_window, px, size,
    test::{TestQueryExt, TestWindowExt},
};
use std::{fs, path::PathBuf};

struct Fixture {
    directory: PathBuf,
    store: RequestStore,
    handle: AnyWindowHandle,
    view: Entity<ApiTester>,
}

impl Fixture {
    fn new(cx: &mut TestAppContext, requests: Vec<SavedRequest>, folders: &[&str]) -> Self {
        Self::with_environment_data(cx, requests, folders, None)
    }

    fn with_environment_data(
        cx: &mut TestAppContext,
        requests: Vec<SavedRequest>,
        folders: &[&str],
        environment_data: Option<&[u8]>,
    ) -> Self {
        let directory = std::env::temp_dir().join(format!("dmgpost-ui-{}", requests[0].id));
        fs::create_dir_all(&directory).expect("create isolated request store");
        let store = RequestStore::at_path(directory.join("requests.json"));
        let folders: Vec<String> = folders.iter().map(|folder| (*folder).to_owned()).collect();
        store.save(&requests, &folders).expect("save fixture");
        if let Some(contents) = environment_data {
            fs::write(store.environments_path(), contents).expect("save environment fixture");
        }
        let (handle, view) = cx.update(|cx| {
            init(cx);
            open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(Bounds {
                        origin: Point::default(),
                        size: size(px(1280.), px(900.)),
                    })),
                    ..Default::default()
                },
                cx,
                |window, cx| {
                    cx.new(|cx| {
                        ApiTester::with_data(
                            Some(store.clone()),
                            requests,
                            folders,
                            None,
                            window,
                            cx,
                        )
                    })
                },
            )
            .expect("open test window")
        });
        Self {
            directory,
            store,
            handle,
            view,
        }
    }

    fn interact(&self, cx: &mut TestAppContext, f: impl FnOnce(&mut Window, &mut App)) {
        cx.update_window(self.handle, |_, window, cx| {
            window.render_frame(cx);
            f(window, cx);
            window.render_frame(cx);
        })
        .expect("dispatch UI interaction");
        cx.run_until_parked();
    }

    fn requests(&self, cx: &mut TestAppContext) -> Vec<SavedRequest> {
        cx.update(|cx| self.view.read(cx).requests.clone())
    }

    fn menu(&self, cx: &mut TestAppContext, target: &str, label: &str) {
        self.interact(cx, |window, cx| window.right_click(target.to_owned(), cx));
        self.interact(cx, |window, cx| click_menu(window, label, cx));
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.directory).expect("remove isolated request store");
    }
}

fn request(name: &str, folder: &str) -> SavedRequest {
    let mut request = SavedRequest::new(name);
    request.folder = folder.to_owned();
    request
}

fn click_menu(window: &mut Window, label: &str, cx: &mut App) {
    let id = window.find_by_label(label).path().last().unwrap().clone();
    window.within("popup-menu").click(id, cx);
}

#[gpui_kit::test]
fn folder_selection_controls_creation_and_expansion(cx: &mut TestAppContext) {
    let fixture = Fixture::new(cx, vec![request("Root", "")], &["Api", "Api/Users"]);
    fixture.interact(cx, |window, cx| {
        window.click("folder-Api/Users", cx);
        window.click("create-request", cx);
    });
    let requests = fixture.requests(cx);
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[1].folder, "Api/Users");
    let created = requests[1].id.clone();
    fixture.interact(cx, |window, cx| {
        window.click("toggle-Api", cx);
        assert!(window.try_find("folder-Api/Users").is_none());
    });
    cx.update(|cx| {
        let view = fixture.view.read(cx);
        assert_eq!(view.active_folder, "Api/Users");
        assert!(!view.expanded_folders.contains("Api"));
    });
    fixture.interact(cx, |window, cx| {
        window.click("active-request-tab", cx);
        window.find("folder-Api/Users");
        window.find(format!("request-row-{created}"));
    });
    assert_eq!(fixture.store.load().unwrap().0[1].folder, "Api/Users");
}

#[gpui_kit::test]
fn request_drag_moves_into_folder_and_back_to_root(cx: &mut TestAppContext) {
    let root = request("Root", "");
    let id = root.id.clone();
    let fixture = Fixture::new(cx, vec![root], &["Api", "Api/Users"]);
    fixture.interact(cx, |window, cx| {
        window.drag_to(format!("request-row-{id}"), "folder-Api/Users", cx);
    });
    assert_eq!(fixture.requests(cx)[0].folder, "Api/Users");
    cx.update(|cx| {
        let view = fixture.view.read(cx);
        assert_eq!(view.active_folder, "Api/Users");
        assert!(view.expanded_folders.contains("Api/Users"));
    });
    fixture.interact(cx, |window, cx| {
        window.drag_to(format!("request-row-{id}"), "tree-root", cx);
    });
    assert_eq!(fixture.requests(cx)[0].folder, "");
    assert_eq!(fixture.store.load().unwrap().0[0].folder, "");
}

#[gpui_kit::test]
fn folder_drag_moves_subtree_and_reorders_siblings(cx: &mut TestAppContext) {
    let fixture = Fixture::new(
        cx,
        vec![
            request("Root", ""),
            request("Session", "Api/Users/Sessions"),
        ],
        &["Api", "Api/Users", "Api/Users/Sessions", "Other"],
    );
    fixture.interact(cx, |window, cx| {
        window.drag_to("folder-Api/Users", "folder-Other", cx);
    });
    assert_eq!(fixture.requests(cx)[1].folder, "Other/Users/Sessions");
    fixture.interact(cx, |window, cx| {
        window.drag_to("folder-Other/Users", "drop-to-root", cx);
    });
    assert_eq!(fixture.requests(cx)[1].folder, "Users/Sessions");
    fixture.interact(cx, |window, cx| {
        window.drag_to(
            "folder-Users",
            format!("drop-before-{:?}", TreeItem::Folder("Api".into())),
            cx,
        );
    });
    let (requests, folders) = fixture.store.load().unwrap();
    assert_eq!(requests[1].folder, "Users/Sessions");
    assert_eq!(folders, ["Users", "Users/Sessions", "Api", "Other"]);
}

#[gpui_kit::test]
fn request_drag_reorders_and_search_filters_nested_requests(cx: &mut TestAppContext) {
    let first = request("First", "");
    let last = request("Last", "");
    let first_id = first.id.clone();
    let last_id = last.id.clone();
    let fixture = Fixture::new(
        cx,
        vec![
            first,
            request("Middle", ""),
            last,
            request("Session", "Api/Users"),
        ],
        &["Api", "Api/Users"],
    );
    fixture.interact(cx, |window, cx| {
        window.drag_to(
            format!("request-row-{first_id}"),
            format!("request-row-{last_id}"),
            cx,
        );
    });
    let (requests, _) = fixture.store.load().unwrap();
    assert_eq!(
        requests
            .iter()
            .map(|request| request.name.as_str())
            .collect::<Vec<_>>(),
        ["Middle", "First", "Last", "Session"]
    );
    fixture.interact(cx, |window, cx| {
        window.click("request-search", cx);
        window.input("Session", cx);
        window.find("folder-Api/Users");
        window.find(format!("request-row-{}", requests[3].id));
        assert!(window.try_find(format!("request-row-{first_id}")).is_none());
    });
}

#[gpui_kit::test]
fn failed_drag_save_rolls_back_tree_and_reports_error(cx: &mut TestAppContext) {
    let root = request("Root", "");
    let id = root.id.clone();
    let fixture = Fixture::new(cx, vec![root], &["Api"]);
    cx.update(|cx| {
        fixture.view.update(cx, |view, _| {
            view.store = Some(RequestStore::at_path(
                fixture.directory.join("missing").join("requests.json"),
            ));
        });
    });
    fixture.interact(cx, |window, cx| {
        window.drag_to(format!("request-row-{id}"), "folder-Api", cx);
    });
    cx.update(|cx| {
        let view = fixture.view.read(cx);
        assert_eq!(view.requests[0].folder, "");
        assert_eq!(view.active_folder, "");
        assert!(view.store_error.is_some());
    });
    assert_eq!(fixture.store.load().unwrap().0[0].folder, "");
}

#[gpui_kit::test]
fn folder_creation_rename_and_delete_update_entire_subtree(cx: &mut TestAppContext) {
    let fixture = Fixture::new(
        cx,
        vec![request("Root", ""), request("Session", "Api/Users")],
        &["Api", "Api/Users"],
    );
    fixture.interact(cx, |window, cx| {
        window.click("folder-Api/Users", cx);
        window.click("create-folder", cx);
        window.click("folder-name", cx);
        window.input("Child", cx);
        window.click("save-folder", cx);
        window.find("folder-Api/Users/Child");
        window.right_click("folder-Api/Users", cx);
    });
    fixture.interact(cx, |window, cx| {
        click_menu(window, "重命名", cx);
    });
    fixture.interact(cx, |window, cx| {
        window.click("folder-name", cx);
        window.press("ctrl-a", cx);
        window.input("Members", cx);
        window.click("save-folder", cx);
        window.find("folder-Api/Members/Child");
    });
    assert_eq!(fixture.requests(cx)[1].folder, "Api/Members");
    cx.update(|cx| assert_eq!(fixture.view.read(cx).active_folder, "Api/Members"));
    fixture.interact(cx, |window, cx| {
        window.right_click("folder-Api/Members", cx);
    });
    fixture.interact(cx, |window, cx| {
        click_menu(window, "删除文件夹", cx);
    });
    fixture.interact(cx, |window, cx| {
        window.click("confirm-delete-folder", cx);
        assert!(window.try_find("folder-Api/Members").is_none());
    });
    let (requests, folders) = fixture.store.load().unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].name, "Root");
    assert_eq!(folders, ["Api"]);
}

#[gpui_kit::test]
fn request_context_menu_delete_removes_request_and_persists(cx: &mut TestAppContext) {
    let victim = request("Victim", "Api");
    let id = victim.id.clone();
    let fixture = Fixture::new(cx, vec![victim, request("Keep", "")], &["Api"]);
    fixture.interact(cx, |window, cx| {
        window.right_click(format!("request-row-{id}"), cx);
    });
    fixture.interact(cx, |window, cx| {
        click_menu(window, "删除", cx);
    });
    fixture.interact(cx, |window, cx| {
        window.click(format!("confirm-delete-{id}"), cx);
    });
    assert_eq!(fixture.requests(cx).len(), 1);
    assert_eq!(fixture.store.load().unwrap().0[0].name, "Keep");
    cx.update(|cx| assert_eq!(fixture.view.read(cx).active_folder, ""));
}

#[gpui_kit::test]
fn request_menu_clone_example_copy_curl_and_info_execute(cx: &mut TestAppContext) {
    let mut source = request("Source", "Api");
    source.url = "https://example.com/users".into();
    source.params = "page=2".into();
    source.body = r#"{"name":"Ada"}"#.into();
    let id = source.id.clone();
    let target = format!("request-row-{id}");
    let fixture = Fixture::new(cx, vec![source.clone()], &["Api"]);
    for (label, expected_name) in [("克隆", "副本 - Source"), ("创建请求示例", "示例 - Source")]
    {
        fixture.menu(cx, &target, label);
        let requests = fixture.requests(cx);
        let copy = requests.last().unwrap();
        assert_eq!(copy.name, expected_name);
        assert_ne!(copy.id, id);
        assert_eq!(copy.folder, source.folder);
        assert_eq!(copy.body, source.body);
        assert_eq!(copy.headers, source.headers);
        assert_eq!(copy.params, source.params);
        assert_eq!(fixture.store.load().unwrap().0.len(), requests.len());
    }
    fixture.menu(cx, &target, "复制请求内容");
    cx.update(|cx| {
        let contents = cx.read_from_clipboard().unwrap().text().unwrap();
        let copied: SavedRequest = serde_json::from_str(&contents).unwrap();
        assert_eq!(copied.id, id);
        assert_eq!(copied.body, source.body);
        assert_eq!(copied.url, source.url);
        assert_eq!(
            fixture.view.read(cx).response_status.as_deref(),
            Some("已复制")
        );
    });
    fixture.menu(cx, &target, "生成 cURL 代码");
    cx.update(|cx| {
        let command = cx.read_from_clipboard().unwrap().text().unwrap();
        assert_eq!(
            command,
            hx6zip::api_client::generate_curl(&source, &Default::default()).unwrap()
        );
        assert_eq!(
            fixture.view.read(cx).response_status.as_deref(),
            Some("已生成代码")
        );
    });
    fixture.menu(cx, &target, "信息");
    cx.update(|cx| {
        let view = fixture.view.read(cx);
        assert_eq!(view.response_status.as_deref(), Some("请求信息"));
        let contents = view.response_content.read(cx).value().to_string();
        assert!(contents.contains(&source.url));
        assert!(contents.contains(&id));
    });
}

#[gpui_kit::test]
fn request_menu_rename_saves_and_delete_can_cancel_or_reset_last_request(cx: &mut TestAppContext) {
    let source = request("Source", "Api");
    let id = source.id.clone();
    let target = format!("request-row-{id}");
    let fixture = Fixture::new(cx, vec![source], &["Api"]);
    fixture.menu(cx, &target, "重命名");
    fixture.interact(cx, |window, cx| {
        window.click(format!("request-name-{id}"), cx);
        window.press("ctrl-a", cx);
        window.input("Renamed", cx);
        window.click(format!("save-rename-{id}"), cx);
    });
    assert_eq!(fixture.store.load().unwrap().0[0].name, "Renamed");
    fixture.menu(cx, &target, "删除");
    fixture.interact(cx, |window, cx| {
        window.click(format!("cancel-delete-{id}"), cx);
        window.find(target.clone());
    });
    assert_eq!(fixture.requests(cx)[0].id, id);
    fixture.menu(cx, &target, "删除");
    fixture.interact(cx, |window, cx| {
        window.click(format!("confirm-delete-{id}"), cx);
    });
    let requests = fixture.requests(cx);
    assert_eq!(requests.len(), 1);
    assert_ne!(requests[0].id, id);
    assert_eq!(requests[0].folder, "");
    assert_eq!(fixture.store.load().unwrap().0[0].id, requests[0].id);
    cx.update(|cx| {
        assert_eq!(
            fixture.view.read(cx).response_status.as_deref(),
            Some("已删除")
        );
    });
}

#[gpui_kit::test]
fn request_move_submenu_moves_to_nested_folder_and_root(cx: &mut TestAppContext) {
    let source = request("Source", "");
    let id = source.id.clone();
    let target = format!("request-row-{id}");
    let fixture = Fixture::new(cx, vec![source], &["Api", "Api/Users"]);
    for (label, expected_folder) in [("Api / Users", "Api/Users"), ("根目录", "")] {
        fixture.menu(cx, &target, "移动到文件夹");
        fixture.interact(cx, |window, cx| {
            let id = window.find_by_label(label).path().last().unwrap().clone();
            window.within("submenu").click(id, cx);
        });
        assert_eq!(fixture.requests(cx)[0].folder, expected_folder);
        assert_eq!(fixture.store.load().unwrap().0[0].folder, expected_folder);
    }
}

#[gpui_kit::test]
fn request_menu_save_failure_keeps_rename_and_delete_pending(cx: &mut TestAppContext) {
    let source = request("Source", "");
    let id = source.id.clone();
    let target = format!("request-row-{id}");
    let fixture = Fixture::new(cx, vec![source], &[]);
    cx.update(|cx| {
        fixture.view.update(cx, |view, _| {
            view.store = Some(RequestStore::at_path(
                fixture.directory.join("missing").join("requests.json"),
            ));
        });
    });
    fixture.menu(cx, &target, "重命名");
    fixture.interact(cx, |window, cx| {
        window.click(format!("request-name-{id}"), cx);
        window.press("ctrl-a", cx);
        window.input("Renamed", cx);
        window.click(format!("save-rename-{id}"), cx);
    });
    cx.update(|cx| {
        let view = fixture.view.read(cx);
        assert_eq!(view.requests[0].name, "Source");
        assert_eq!(view.renaming_request_id.as_deref(), Some(id.as_str()));
        assert!(view.store_error.is_some());
    });
    fixture.interact(cx, |window, cx| {
        window.click(format!("cancel-rename-{id}"), cx)
    });
    fixture.menu(cx, &target, "删除");
    fixture.interact(cx, |window, cx| {
        window.click(format!("confirm-delete-{id}"), cx)
    });
    cx.update(|cx| {
        let view = fixture.view.read(cx);
        assert_eq!(view.requests[0].id, id);
        assert_eq!(view.pending_delete_id.as_deref(), Some(id.as_str()));
        assert!(view.response_error);
    });
    assert_eq!(fixture.store.load().unwrap().0[0].name, "Source");
}

#[gpui_kit::test]
fn folder_menu_creates_child_and_request_and_moves_subtree_to_root(cx: &mut TestAppContext) {
    let fixture = Fixture::new(cx, vec![request("Root", "")], &["Api", "Api/Users"]);
    fixture.menu(cx, "folder-Api/Users", "新建子文件夹");
    fixture.interact(cx, |window, cx| {
        window.click("folder-name", cx);
        window.input("Child", cx);
        window.click("save-folder", cx);
        window.find("folder-Api/Users/Child");
    });
    fixture.menu(cx, "folder-Api/Users/Child", "新建请求");
    assert_eq!(fixture.requests(cx)[1].folder, "Api/Users/Child");
    fixture.menu(cx, "folder-Api/Users", "移动到根目录");
    assert_eq!(fixture.store.load().unwrap().0[1].folder, "Users/Child");
    fixture.interact(cx, |window, _| {
        window.find("folder-Users/Child");
    });
}

#[gpui_kit::test]
fn reveal_file_menu_reports_unavailable_store_instead_of_no_op(cx: &mut TestAppContext) {
    let source = request("Source", "");
    let target = format!("request-row-{}", source.id);
    let fixture = Fixture::new(cx, vec![source], &[]);
    cx.update(|cx| {
        fixture.view.update(cx, |view, _| {
            view.store = None;
            view.store_error = Some("Request store unavailable".into());
        });
    });
    fixture.menu(cx, &target, "在文件资源管理器中显示");
    cx.update(|cx| {
        let view = fixture.view.read(cx);
        assert_eq!(view.response_status.as_deref(), Some("无法打开目录"));
        assert!(view.response_error);
        assert_eq!(
            view.response_content.read(cx).value().to_string(),
            "Request store unavailable"
        );
    });
}

#[gpui_kit::test]
fn malformed_environment_is_reported_and_not_overwritten(cx: &mut TestAppContext) {
    let invalid = b"invalid environment data";
    let fixture = Fixture::with_environment_data(cx, vec![request("Root", "")], &[], Some(invalid));
    cx.update(|cx| {
        let view = fixture.view.read(cx);
        assert_eq!(view.response_status.as_deref(), Some("本地加载失败"));
        assert!(view.response_error);
        assert!(
            view.response_content
                .read(cx)
                .value()
                .contains("读取环境变量失败")
        );
    });
    fixture.interact(cx, |window, cx| {
        fixture.view.update(cx, |view, cx| {
            assert!(!view.persist_environments(window, cx))
        });
    });
    assert_eq!(
        fs::read(fixture.store.environments_path()).unwrap(),
        invalid
    );
}

/// Serves one canned JSON response on a real local socket and returns the raw request.
fn mock_server(body: &'static str) -> (String, std::sync::mpsc::Receiver<String>) {
    use std::io::{BufRead, BufReader, Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut head = String::new();
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            head.push_str(&line);
            if line == "\r\n" || line.is_empty() {
                break;
            }
        }
        let length = head
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().ok())?
            })
            .unwrap_or(0);
        let mut payload = vec![0; length];
        reader.read_exact(&mut payload).unwrap();
        head.push_str(&String::from_utf8_lossy(&payload));
        let mut stream = stream;
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nX-Trace: t1\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .unwrap();
        sender.send(head).unwrap();
    });
    (address, receiver)
}

#[gpui_kit::test]
fn body_type_auth_and_settings_tabs_persist_request_options(cx: &mut TestAppContext) {
    let fixture = Fixture::new(cx, vec![request("Root", "")], &[]);
    fixture.interact(cx, |window, cx| {
        window.click("body-type-4", cx);
        window.click("request-tab-auth", cx);
    });
    fixture.interact(cx, |window, cx| {
        window.click("auth-type-1", cx);
    });
    fixture.interact(cx, |window, cx| {
        window.click("auth-token", cx);
        window.input("secret", cx);
        window.click("request-tab-settings", cx);
    });
    fixture.interact(cx, |window, cx| {
        window.click("setting-timeout", cx);
        window.press("ctrl-a", cx);
        window.input("5000", cx);
        window.click("setting-follow-redirects", cx);
        window.click("setting-verify-tls", cx);
    });
    let saved = fixture.store.load().unwrap().0.remove(0);
    assert_eq!(
        saved.body_type,
        hx6zip::api_client::BodyType::FormUrlEncoded
    );
    assert_eq!(saved.auth.auth_type, hx6zip::api_client::AuthType::Bearer);
    assert_eq!(saved.auth.token, "secret");
    assert_eq!(saved.settings.timeout_ms, 5000);
    assert!(!saved.settings.follow_redirects);
    assert!(!saved.settings.verify_tls);

    fixture.interact(cx, |window, cx| {
        window.click("request-tab-body", cx);
    });
    fixture.interact(cx, |window, cx| {
        window.click("body-type-0", cx);
    });
    assert_eq!(
        fixture.store.load().unwrap().0[0].body_type,
        hx6zip::api_client::BodyType::None
    );
}

#[gpui_kit::test]
fn environment_variables_drive_request_assertions_and_response_tabs(cx: &mut TestAppContext) {
    let (address, received) = mock_server(r#"{"user":{"id":7}}"#);
    let mut source = request("Root", "");
    source.url = "{{base}}/users".into();
    source.params = "q={{name}}\n# skip=1".into();
    source.assertions =
        "res.status eq 200\nres.body.user.id eq 7\nres.headers.x-trace eq t1".into();
    let fixture = Fixture::new(cx, vec![source], &[]);

    fixture.interact(cx, |window, cx| {
        window.click("request-tab-environment", cx);
    });
    fixture.interact(cx, |window, cx| {
        window.click("env-create", cx);
    });
    fixture.interact(cx, |window, cx| {
        window.click("env-name", cx);
        window.input("开发", cx);
        window.click("env-save", cx);
    });
    let variables = format!("base={address}\nname=bob");
    cx.update_window(fixture.handle, |_, window, cx| {
        fixture.view.update(cx, |view, cx| {
            view.env_variables.update(cx, |state, cx| {
                state.set_value(variables.clone(), window, cx);
            });
            view.save_environment_variables(window, cx);
        });
    })
    .unwrap();
    let saved = fixture.store.load_environments().unwrap();
    assert_eq!(saved.environments.len(), 1);
    assert_eq!(saved.environments[0].name, "开发");
    assert_eq!(saved.environments[0].variables, variables);
    assert_eq!(saved.active.as_ref(), Some(&saved.environments[0].id));

    fixture.interact(cx, |window, cx| {
        window.click("request-url", cx);
        window.press("enter", cx);
    });
    for _ in 0..200 {
        if !cx.update(|cx| fixture.view.read(cx).is_loading) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
        cx.run_until_parked();
    }
    let raw = received
        .recv_timeout(std::time::Duration::from_secs(5))
        .unwrap();
    assert!(raw.starts_with("GET /users?q=bob HTTP/1.1"), "{raw}");
    cx.update(|cx| {
        let view = fixture.view.read(cx);
        assert_eq!(view.response_status.as_deref(), Some("200 成功"));
        assert_eq!(view.assertion_results.len(), 3);
        assert!(view.assertion_results.iter().all(|result| result.passed));
        assert!(
            view.response_headers
                .read(cx)
                .value()
                .contains("x-trace: t1")
        );
        assert!(view.response_timeline.read(cx).value().contains("> GET"));
    });
    fixture.interact(cx, |window, cx| {
        window.click("response-tab-assertions", cx);
    });
    fixture.interact(cx, |window, cx| {
        window.find("assertion-result-2");
        window.click("response-tab-headers", cx);
    });
    fixture.interact(cx, |window, _| {
        window.find("response-headers");
    });

    fixture.interact(cx, |window, cx| window.click("env-switcher", cx));
    fixture.interact(cx, |window, cx| click_menu(window, "无环境", cx));
    assert_eq!(fixture.store.load_environments().unwrap().active, None);
}
