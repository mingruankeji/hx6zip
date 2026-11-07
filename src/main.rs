#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod api_client_ui;
mod ui;
#[cfg(windows)]
mod window_icon;

use gpui_kit::*;

const API_TESTER_FLAG: &str = "--api-tester";

fn main() {
    #[cfg(all(windows, not(debug_assertions)))]
    install_panic_handler();

    if std::env::args_os()
        .nth(1)
        .is_some_and(|arg| arg == API_TESTER_FLAG)
    {
        api_client_ui::run();
        return;
    }

    let initial_action = match initial_action() {
        Ok(action) => action,
        Err(error) => {
            rfd::MessageDialog::new()
                .set_title("Hx6Zip 错误")
                .set_description(format!("{error:#}"))
                .set_level(rfd::MessageLevel::Error)
                .show();
            return;
        }
    };
    #[cfg(windows)]
    if std::env::args_os()
        .nth(1)
        .is_some_and(|arg| arg == "--shell-server")
    {
        return;
    }
    let compact = matches!(
        initial_action,
        Some(
            hx6zip::cli::StartupAction::Create { .. }
                | hx6zip::cli::StartupAction::CreateEach { .. }
                | hx6zip::cli::StartupAction::Extract { .. }
                | hx6zip::cli::StartupAction::ConfigureCreate { .. }
                | hx6zip::cli::StartupAction::ConfigureExtract { .. }
                | hx6zip::cli::StartupAction::Split { .. }
                | hx6zip::cli::StartupAction::Merge { .. }
        )
    );
    let setup = matches!(
        initial_action,
        Some(
            hx6zip::cli::StartupAction::ConfigureCreate { .. }
                | hx6zip::cli::StartupAction::ConfigureExtract { .. }
                | hx6zip::cli::StartupAction::Split { .. }
        )
    );
    let title = match &initial_action {
        Some(hx6zip::cli::StartupAction::CreateEach { .. }) => {
            "Hx6Zip - 压缩每个到单独的压缩包中".to_owned()
        }
        Some(hx6zip::cli::StartupAction::Split { .. }) => "Hx6Zip - 分割文件".to_owned(),
        Some(hx6zip::cli::StartupAction::Merge { .. }) => "Hx6Zip - 合并文件".to_owned(),
        Some(hx6zip::cli::StartupAction::Create { format, sources }) => {
            hx6zip::cli::automatic_archive_path(sources, *format)
                .ok()
                .and_then(|path| {
                    path.file_name()
                        .map(|name| format!("Hx6Zip - {}", name.to_string_lossy()))
                })
                .unwrap_or_else(|| "Hx6Zip".to_owned())
        }
        Some(hx6zip::cli::StartupAction::ConfigureCreate { .. }) => {
            "Hx6Zip - 添加到压缩文件".to_owned()
        }
        Some(hx6zip::cli::StartupAction::ConfigureExtract { .. }) => "Hx6Zip - 解压到".to_owned(),
        _ => "Hx6Zip".to_owned(),
    };
    application().with_assets(assets::Assets).run(move |cx| {
        init(cx);
        let dimensions = if setup {
            size(px(680.0), px(520.0))
        } else if compact {
            size(px(640.0), px(410.0))
        } else {
            size(px(1200.0), px(800.0))
        };
        let bounds = Bounds::centered(None, dimensions, cx);
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            window_min_size: Some(if setup {
                size(px(600.0), px(480.0))
            } else if compact {
                size(px(560.0), px(390.0))
            } else {
                size(px(900.0), px(620.0))
            }),
            titlebar: Some(TitlebarOptions {
                title: Some(title.clone().into()),
                ..Default::default()
            }),
            app_id: Some("hx6zip.desktop".to_owned()),
            ..Default::default()
        };
        open_window(options, cx, move |window, cx| {
            #[cfg(windows)]
            window_icon::set_window_icon(window).expect("无法设置 Hx6Zip 窗口图标");
            let view = cx.new(|cx| ui::AppView::new(window, cx));
            let weak_view = view.downgrade();
            window.on_window_should_close(cx, move |_, cx| {
                weak_view
                    .update(cx, |view, cx| view.request_close(cx))
                    .unwrap_or(true)
            });
            if let Some(action) = initial_action {
                view.update(cx, |view, cx| match action {
                    hx6zip::cli::StartupAction::Split { path } => {
                        view.configure_split(path, window, cx)
                    }
                    hx6zip::cli::StartupAction::Merge { sources } => view.start_merge(sources, cx),
                    hx6zip::cli::StartupAction::Extract { path, named_folder } => {
                        view.start_extract_from_path(path, named_folder, cx);
                    }
                    hx6zip::cli::StartupAction::Open(path) => view.open_path(path, cx),
                    hx6zip::cli::StartupAction::Create { format, sources } => {
                        view.start_create_from_sources(sources, format, cx);
                    }
                    hx6zip::cli::StartupAction::CreateEach { sources } => {
                        view.start_create_each(sources, cx);
                    }
                    hx6zip::cli::StartupAction::ConfigureCreate { sources } => {
                        view.configure_create_from_sources(sources, cx);
                    }
                    hx6zip::cli::StartupAction::ConfigureExtract { path } => {
                        view.configure_extract_from_path(path, cx);
                    }
                });
            }
            view
        })
        .expect("无法打开 Hx6Zip 窗口");
        cx.activate(true);
    });
}

fn initial_action() -> anyhow::Result<Option<hx6zip::cli::StartupAction>> {
    #[cfg(windows)]
    match std::env::args_os()
        .nth(1)
        .as_deref()
        .and_then(|arg| arg.to_str())
    {
        Some("--shell-server") => {
            hx6zip::shell::run_server()?;
            return Ok(None);
        }
        Some(
            flag @ ("--shell-create-zip"
            | "--shell-create-each-zip"
            | "--shell-create-7z"
            | "--shell-add-to-archive"
            | "--shell-split-file"
            | "--shell-merge-files"),
        ) => {
            let sources = hx6zip::shell::read_sources(std::io::stdin().lock())?;
            return Ok(Some(match flag {
                "--shell-create-each-zip" => hx6zip::cli::StartupAction::CreateEach { sources },
                "--shell-split-file" => {
                    anyhow::ensure!(sources.len() == 1, "分割需要且只能指定一个文件");
                    hx6zip::cli::StartupAction::Split {
                        path: sources.into_iter().next().unwrap(),
                    }
                }
                "--shell-merge-files" => hx6zip::cli::StartupAction::Merge { sources },
                "--shell-add-to-archive" => hx6zip::cli::StartupAction::ConfigureCreate { sources },
                _ => hx6zip::cli::StartupAction::Create {
                    format: if flag == "--shell-create-zip" {
                        hx6zip::archive::ArchiveFormat::Zip
                    } else {
                        hx6zip::archive::ArchiveFormat::SevenZip
                    },
                    sources,
                },
            }));
        }
        _ => {}
    }
    let args = std::env::args_os().skip(1).collect::<Vec<_>>();
    if args.first().is_some_and(|arg| arg == "--split-file") {
        anyhow::ensure!(args.len() == 2, "分割需要且只能指定一个文件路径");
    }
    if args.first().is_some_and(|arg| arg == "--merge-files") {
        anyhow::ensure!(args.len() >= 3, "合并需要指定至少两个连续分片");
    }
    if args.first().is_some_and(|arg| {
        arg == "--extract-here" || arg == "--extract-folder" || arg == "--extract-to"
    }) {
        anyhow::ensure!(args.len() == 2, "解压需要且只能指定一个压缩包路径");
    }
    if args
        .first()
        .is_some_and(|arg| arg == "--add-to-archive" || arg == "--create-each-zip")
    {
        anyhow::ensure!(args.len() > 1, "请指定要压缩的文件或文件夹");
    }
    Ok(hx6zip::cli::startup_action(args.into_iter()))
}

#[cfg(all(windows, not(debug_assertions)))]
fn install_panic_handler() {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        default_hook(info);
        rfd::MessageDialog::new()
            .set_title("Hx6Zip 错误")
            .set_description(format!(
                "程序遇到意外错误，请保存以下信息并重新启动：\n\n{info}"
            ))
            .set_level(rfd::MessageLevel::Error)
            .show();
    }));
}
