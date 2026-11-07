use std::{cell::Cell, path::PathBuf, sync::mpsc, time::Duration};

use anyhow::{Context as _, Result};
use gpui_kit::{
    component::{
        WindowExt as _,
        input::{Input, InputState},
    },
    *,
};
use hx6zip::archive::{Cancelled, PasswordPrompt};

use super::AppView;

pub(super) struct PasswordRequest {
    path: PathBuf,
    retry: bool,
    response: mpsc::Sender<Option<String>>,
}

impl AppView {
    pub(super) fn password_prompt(cx: &mut Context<Self>) -> PasswordPrompt {
        let (sender, receiver) = async_channel::bounded::<PasswordRequest>(1);
        cx.spawn(async move |view, cx| {
            while let Ok(request) = receiver.recv().await {
                if view
                    .update(cx, |this, cx| {
                        this.password_request = Some(request);
                        cx.notify();
                    })
                    .is_err()
                {
                    return;
                }
            }
        })
        .detach();
        PasswordPrompt::new(move |path, retry, control| -> Result<String> {
            let (response, receiver) = mpsc::channel();
            sender
                .send_blocking(PasswordRequest {
                    path: path.to_path_buf(),
                    retry,
                    response,
                })
                .context("无法显示压缩包密码对话框")?;
            loop {
                control.check_cancelled()?;
                match receiver.recv_timeout(Duration::from_millis(100)) {
                    Ok(Some(password)) => return Ok(password),
                    Ok(None) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                        return Err(Cancelled.into());
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                }
            }
        })
    }

    pub(super) fn show_password_prompt(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(request) = self.password_request.take() else {
            return;
        };
        let input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("请输入压缩包密码")
                .masked(true)
        });
        let initial_focus = Cell::new(true);
        let description = format!(
            "{}\n{}",
            request.path.display(),
            if request.retry {
                "密码不正确，请重新输入。若密码确认无误，压缩包可能已损坏。"
            } else {
                "此压缩包已加密，请输入密码后继续。"
            },
        );
        window.open_alert_dialog(cx, move |dialog, window, cx| {
            if initial_focus.replace(false) {
                input.focus_handle(cx).focus(window, cx);
            }
            let response = request.response.clone();
            let close_response = request.response.clone();
            let confirm_input = input.clone();
            let close_input = input.clone();
            dialog
                .title("输入压缩包密码")
                .description(description.clone())
                .centered()
                .show_cancel(true)
                .ok_text("确定")
                .cancel_text("取消")
                .child(Input::new(&input).mask_toggle())
                .on_ok(move |_, _, cx| {
                    let password = confirm_input.read(cx).value().to_string();
                    if password.is_empty() {
                        return false;
                    }
                    let _ = response.send(Some(password));
                    true
                })
                .on_close(move |_, window, cx| {
                    close_input.update(cx, |input, cx| input.set_value("", window, cx));
                    let _ = close_response.send(None);
                })
        });
    }
}

#[cfg(test)]
mod tests {
    use std::{path::PathBuf, sync::mpsc};

    use gpui_kit::{
        TestAppContext,
        component::{
            WindowExt as _,
            dialog::{Cancel, Confirm},
        },
    };

    use super::{AppView, PasswordRequest};

    #[gpui_kit::test]
    fn quick_extract_worker_requests_password_and_resumes_after_confirmation(
        cx: &mut TestAppContext,
    ) {
        use gpui_kit::{base::Root, *};
        use hx6zip::archive::{ArchiveFormat, CreateOptions, JobControl, create_archive};
        use std::{
            fs,
            time::{Duration, Instant},
        };

        cx.executor().allow_parking();
        cx.update(gpui_kit::init);
        let (root, cx) = cx.add_window_view(|window, cx| {
            let view = cx.new(|cx| AppView::new(window, cx));
            Root::new(view, window, cx)
        });
        let view = root.read_with(cx, |root, _| {
            root.view().clone().downcast::<AppView>().unwrap()
        });
        for extension in ["zip", "7z", "rar"] {
            let temp = tempfile::tempdir().unwrap();
            let archive = temp.path().join(format!("sample.{extension}"));
            let (password, relative, contents) = if extension == "rar" {
                fs::copy(
                    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                        .join("tests")
                        .join("fixtures")
                        .join("libarchive")
                        .join("test_read_format_rar5_encrypted_filenames.rar"),
                    &archive,
                )
                .unwrap();
                ("password", "c.txt", b"This is from c.txt".as_slice())
            } else {
                let source = temp.path().join("hello.txt");
                fs::write(&source, b"hello").unwrap();
                create_archive(
                    &CreateOptions {
                        sources: vec![source],
                        destination: archive.clone(),
                        format: if extension == "zip" {
                            ArchiveFormat::Zip
                        } else {
                            ArchiveFormat::SevenZip
                        },
                        password: Some("secret".to_owned()),
                        overwrite: false,
                    },
                    &JobControl::default(),
                )
                .unwrap();
                ("secret", "hello.txt", b"hello".as_slice())
            };
            view.update(cx, |app, cx| app.start_extract_from_path(archive, true, cx));
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                cx.run_until_parked();
                let active = cx.update(|window, cx| {
                    window.draw(cx).clear(cx);
                    window.has_active_dialog(cx)
                });
                if active {
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "{extension}: password dialog did not appear"
                );
                std::thread::sleep(Duration::from_millis(1));
            }
            assert!(!temp.path().join("sample").join(relative).exists());
            cx.simulate_input(password);
            cx.dispatch_action(Confirm { secondary: false });
            loop {
                cx.run_until_parked();
                if view.read_with(cx, |app, _| app.running.is_none()) {
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "{extension}: extraction did not resume"
                );
                std::thread::sleep(Duration::from_millis(1));
            }
            view.read_with(cx, |app, _| assert!(!app.status_is_error, "{}", app.status));
            assert_eq!(
                fs::read(temp.path().join("sample").join(relative)).unwrap(),
                contents
            );
        }
    }

    #[gpui_kit::test]
    fn password_dialog_accepts_input_and_cancels_without_returning_a_password(
        cx: &mut TestAppContext,
    ) {
        use gpui_kit::{base::Root, *};

        cx.update(gpui_kit::init);
        let (root, cx) = cx.add_window_view(|window, cx| {
            let view = cx.new(|cx| AppView::new(window, cx));
            Root::new(view, window, cx)
        });
        let view = root.read_with(cx, |root, _| {
            root.view().clone().downcast::<AppView>().unwrap()
        });
        for (confirm, shell_progress) in [(true, false), (false, true)] {
            let (response, receiver) = mpsc::channel();
            view.update(cx, |app, cx| {
                app.shell_progress = shell_progress;
                app.password_request = Some(PasswordRequest {
                    path: PathBuf::from(r"C:\encrypted.zip"),
                    retry: false,
                    response,
                });
                cx.notify();
            });
            cx.update(|window, cx| window.draw(cx).clear(cx));
            cx.run_until_parked();
            cx.update(|window, cx| {
                window.draw(cx).clear(cx);
                assert!(window.has_active_dialog(cx));
            });
            cx.dispatch_action(Confirm { secondary: false });
            cx.run_until_parked();
            assert!(
                receiver.try_recv().is_err(),
                "empty passwords must not be accepted"
            );
            if confirm {
                cx.simulate_input("test-secret");
                cx.dispatch_action(Confirm { secondary: false });
            } else {
                cx.dispatch_action(Cancel);
            }
            cx.run_until_parked();
            assert_eq!(
                receiver.try_recv().unwrap(),
                confirm.then(|| "test-secret".to_owned())
            );
            cx.update(|window, cx| {
                window.draw(cx).clear(cx);
                assert!(!window.has_active_dialog(cx));
            });
        }
    }
}
