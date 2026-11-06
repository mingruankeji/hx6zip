use std::time::Instant;

use gpui_kit::{
    component::{
        ActiveTheme, Disableable, Icon, IconName, Sizable,
        button::{Button, ButtonCustomVariant, ButtonVariants},
        checkbox::Checkbox,
        input::Input,
        select::Select,
        tooltip::Tooltip,
    },
    prelude::FluentBuilder,
    *,
};

use super::{
    AppView, JobKind, Page, ShellSetup, format_size,
    progress::{countdown_seconds, format_duration},
};
use mzip::archive::{ArchiveFormat, IntegrityMode};

#[derive(Clone, Copy)]
struct Palette {
    background: Rgba,
    surface: Rgba,
    surface_alt: Rgba,
    border: Rgba,
    text: Rgba,
    muted: Rgba,
    accent: Rgba,
    accent_soft: Rgba,
    danger: Rgba,
}

impl Palette {
    fn new(dark: bool) -> Self {
        if dark {
            Self {
                background: rgb(0x101827),
                surface: rgb(0x1b2637),
                surface_alt: rgb(0x222f42),
                border: rgb(0x354257),
                text: rgb(0xe8eef8),
                muted: rgb(0x9caec6),
                accent: rgb(0x78a9ff),
                accent_soft: rgb(0x243d62),
                danger: rgb(0xff9a9a),
            }
        } else {
            Self {
                background: rgb(0xf4f7fb),
                surface: rgb(0xffffff),
                surface_alt: rgb(0xf8fafd),
                border: rgb(0xe0e7f0),
                text: rgb(0x18263b),
                muted: rgb(0x6a7c93),
                accent: rgb(0x2868d7),
                accent_soft: rgb(0xeaf2ff),
                danger: rgb(0xbd3333),
            }
        }
    }
}

impl Render for AppView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.refresh_filter(cx);
        let palette = Palette::new(self.dark);
        if self.shell_setup == Some(ShellSetup::Split) {
            return self.render_split_setup(palette, cx).into_any_element();
        }
        if self.shell_setup.is_some() {
            return self.render_shell_setup(palette, cx).into_any_element();
        }
        if self.shell_progress {
            return self.render_shell_progress(palette, cx).into_any_element();
        }
        let content = match self.page {
            Page::Explorer => self.render_explorer(window, cx),
            Page::Browse => self.render_browse(palette, cx),
            Page::Create => self.render_create(palette, cx),
            Page::Settings => self.render_settings(palette, cx),
        };

        div()
            .id("mzip-root")
            .size_full()
            .flex()
            .flex_col()
            .font_family(cx.theme().font_family.clone())
            .text_color(palette.text)
            .bg(palette.background)
            .on_drop(cx.listener(|this, paths: &ExternalPaths, _, cx| {
                this.handle_drop(paths.paths(), cx);
            }))
            .child(self.render_main_menu(cx))
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .child(if self.page == Page::Explorer {
                        self.render_explorer_sidebar(cx).into_any_element()
                    } else {
                        self.render_sidebar(cx).into_any_element()
                    })
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .h_full()
                            .flex()
                            .flex_col()
                            .when(self.page != Page::Explorer, |element| {
                                element.child(self.render_header(palette, cx))
                            })
                            .child(
                                div()
                                    .flex_1()
                                    .min_h_0()
                                    .px(px(16.0))
                                    .pb(px(16.0))
                                    .child(content),
                            )
                            .child(self.render_status(palette, cx)),
                    ),
            )
            .into_any_element()
    }
}

impl AppView {
    fn render_split_setup(&self, palette: Palette, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .flex()
            .flex_col()
            .font_family(cx.theme().font_family.clone())
            .text_color(palette.text)
            .bg(palette.background)
            .child(
                div()
                    .flex_1()
                    .p(px(20.0))
                    .flex()
                    .flex_col()
                    .gap(px(18.0))
                    .child(
                        div()
                            .text_size(px(22.0))
                            .font_weight(FontWeight::BOLD)
                            .child("分割文件"),
                    )
                    .child(
                        div()
                            .text_size(px(12.0))
                            .text_color(palette.muted)
                            .truncate()
                            .child(
                                self.sources
                                    .first()
                                    .map_or_else(String::new, |path| path.display().to_string()),
                            ),
                    )
                    .child(div().child("分割文件到"))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(10.0))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .child(Input::new(&self.split_destination)),
                            )
                            .child(
                                Button::new("split-browse")
                                    .outline()
                                    .label("选择…")
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.choose_split_destination(window, cx)
                                    })),
                            ),
                    )
                    .child(div().child("分卷大小 MB"))
                    .child(div().w(px(240.0)).child(Select::new(&self.split_size)))
                    .child(div().text_size(px(11.0)).text_color(palette.muted).child(
                        "1 MB = 1024² 字节；生成 文件名.001、.002…，不压缩、不覆盖已有文件。",
                    )),
            )
            .child(self.render_status(palette, cx))
            .child(
                div()
                    .p(px(16.0))
                    .flex()
                    .justify_end()
                    .gap(px(10.0))
                    .child(
                        Button::new("split-cancel")
                            .outline()
                            .label("取消")
                            .on_click(|_, _, cx| cx.quit()),
                    )
                    .child(
                        Button::new("split-start")
                            .primary()
                            .label("开始分割")
                            .disabled(self.sources.is_empty())
                            .on_click(cx.listener(|this, _, _, cx| this.start_split(cx))),
                    ),
            )
    }

    fn render_shell_setup(&self, palette: Palette, cx: &mut Context<Self>) -> impl IntoElement {
        let is_create = self.shell_setup == Some(ShellSetup::Create);
        let destination = self
            .destination
            .as_ref()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "尚未选择位置".to_owned());
        let full_destination = destination.clone();
        let source = if is_create {
            format!("已选择 {} 个文件或文件夹", self.sources.len())
        } else {
            self.last_open_path.as_ref().map_or_else(
                || "尚未选择压缩包".to_owned(),
                |path| format!("压缩包：{}", path.display()),
            )
        };
        div()
            .size_full()
            .flex()
            .flex_col()
            .font_family(cx.theme().font_family.clone())
            .text_color(palette.text)
            .bg(palette.background)
            .child(
                div()
                    .flex_1()
                    .p(px(20.0))
                    .flex()
                    .flex_col()
                    .gap(px(14.0))
                    .child(
                        div()
                            .text_size(px(22.0))
                            .font_weight(FontWeight::BOLD)
                            .child(if is_create {
                                "添加到压缩文件"
                            } else {
                                "解压到"
                            }),
                    )
                    .child(
                        div()
                            .text_size(px(12.0))
                            .text_color(palette.muted)
                            .truncate()
                            .child(source),
                    )
                    .when(is_create, |element| {
                        element.child(
                            div()
                                .flex()
                                .items_center()
                                .gap(px(10.0))
                                .child(div().w(px(76.0)).child("压缩格式"))
                                .child(
                                    format_button(
                                        "shell-format-zip",
                                        "ZIP",
                                        self.format == ArchiveFormat::Zip,
                                    )
                                    .on_click(cx.listener(
                                        |this, _, _, cx| this.set_format(ArchiveFormat::Zip, cx),
                                    )),
                                )
                                .child(
                                    format_button(
                                        "shell-format-7z",
                                        "7z",
                                        self.format == ArchiveFormat::SevenZip,
                                    )
                                    .on_click(cx.listener(
                                        |this, _, _, cx| {
                                            this.set_format(ArchiveFormat::SevenZip, cx)
                                        },
                                    )),
                                ),
                        )
                    })
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(10.0))
                            .child(div().w(px(76.0)).child(if is_create {
                                "保存位置"
                            } else {
                                "目标文件夹"
                            }))
                            .child(
                                div()
                                    .id("shell-setup-destination")
                                    .flex_1()
                                    .min_w_0()
                                    .px(px(10.0))
                                    .py(px(9.0))
                                    .rounded(px(6.0))
                                    .bg(palette.surface)
                                    .text_size(px(12.0))
                                    .text_ellipsis_middle()
                                    .tooltip(move |window, cx| {
                                        full_text_tooltip(full_destination.clone(), window, cx)
                                    })
                                    .child(destination),
                            )
                            .child(
                                Button::new("shell-choose-destination")
                                    .outline()
                                    .label("浏览")
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        if is_create {
                                            this.choose_destination(cx);
                                        } else {
                                            this.choose_extract_destination(cx);
                                        }
                                    })),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(10.0))
                            .child(div().w(px(76.0)).child("密码"))
                            .child(div().flex_1().child(if is_create {
                                Input::new(&self.create_password).mask_toggle()
                            } else {
                                Input::new(&self.archive_password).mask_toggle()
                            })),
                    )
                    .when(is_create, |element| {
                        element.child(
                            div()
                                .flex()
                                .items_center()
                                .gap(px(10.0))
                                .child(div().w(px(76.0)).child("确认密码"))
                                .child(
                                    div()
                                        .flex_1()
                                        .child(Input::new(&self.confirm_password).mask_toggle()),
                                ),
                        )
                    })
                    .child(div().text_size(px(11.0)).text_color(palette.muted).child(
                        if is_create {
                            "密码留空为不加密；ZIP 的文件名仍可见。"
                        } else {
                            "遇到同名文件时会询问是否全部替换。"
                        },
                    )),
            )
            .child(self.render_status(palette, cx))
            .child(
                div()
                    .p(px(16.0))
                    .flex()
                    .justify_end()
                    .gap(px(10.0))
                    .child(
                        Button::new("cancel-shell-setup")
                            .outline()
                            .label("取消")
                            .on_click(|_, _, cx| cx.quit()),
                    )
                    .child(
                        Button::new("start-shell-setup")
                            .primary()
                            .label(if is_create {
                                "开始压缩"
                            } else {
                                "开始解压"
                            })
                            .disabled(self.destination.is_none())
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if is_create {
                                    this.create(cx);
                                } else {
                                    this.extract_to_destination(cx);
                                }
                            })),
                    ),
            )
    }

    fn render_shell_progress(&self, palette: Palette, cx: &mut Context<Self>) -> impl IntoElement {
        let destination = if self.job_kind == JobKind::CreateEach {
            "保存至：每个源文件或文件夹所在目录".to_owned()
        } else {
            self.destination.as_ref().map_or_else(
                || "无法确定保存位置".to_owned(),
                |path| {
                    format!(
                        "{}：{}",
                        if self.shell_extract {
                            "解压至"
                        } else {
                            "保存至"
                        },
                        path.display()
                    )
                },
            )
        };
        let full_destination = destination.clone();
        div()
            .size_full()
            .flex()
            .flex_col()
            .font_family(cx.theme().font_family.clone())
            .text_color(palette.text)
            .bg(palette.background)
            .child(
                div()
                    .px(px(20.0))
                    .pt(px(16.0))
                    .pb(px(4.0))
                    .flex()
                    .flex_col()
                    .gap(px(6.0))
                    .child(
                        div()
                            .text_size(px(20.0))
                            .font_weight(FontWeight::BOLD)
                            .child(
                                if matches!(self.job_kind, JobKind::Split | JobKind::Merge) {
                                    if self.job_kind == JobKind::Split {
                                        "MZip · 分割文件".to_owned()
                                    } else {
                                        "MZip · 合并文件".to_owned()
                                    }
                                } else if self.job_kind == JobKind::CreateEach {
                                    "MZip · 逐个压缩为 ZIP".to_owned()
                                } else if self.shell_extract {
                                    "MZip · 解压".to_owned()
                                } else {
                                    format!(
                                        "MZip · {} 压缩",
                                        self.format.extension().to_ascii_uppercase()
                                    )
                                },
                            ),
                    )
                    .child(if self.job_kind == JobKind::Split {
                        self.sources
                            .first()
                            .map_or_else(String::new, |path| format!("分割：{}", path.display()))
                    } else if self.job_kind == JobKind::Merge {
                        format!("将 {} 个分片按编号合并为原文件", self.sources.len())
                    } else if self.job_kind == JobKind::CreateEach {
                        format!("将 {} 个所选项目分别压缩为独立 ZIP", self.sources.len())
                    } else if self.shell_extract {
                        self.last_open_path
                            .as_ref()
                            .map_or_else(String::new, |path| {
                                format!(
                                    "解压：{}",
                                    path.file_name().unwrap_or_default().to_string_lossy()
                                )
                            })
                    } else {
                        format!("将 {} 个所选项目合并为一个压缩包", self.sources.len())
                    })
                    .child(
                        div()
                            .id("shell-destination")
                            .text_size(px(12.0))
                            .text_color(palette.muted)
                            .line_clamp(2)
                            .text_ellipsis_middle()
                            .tooltip(move |window, cx| {
                                full_text_tooltip(full_destination.clone(), window, cx)
                            })
                            .child(destination),
                    )
                    .child(div().text_size(px(11.0)).text_color(palette.muted).child(
                        if self.shell_extract {
                            "自动开始 · 同名文件先确认 · 加密包请在主窗口输入密码"
                        } else if self.shell_custom {
                            "不覆盖已有文件"
                        } else {
                            "自动开始 · 重名加序号 · 不覆盖已有文件"
                        },
                    )),
            )
            .child(self.render_status(palette, cx))
            .child(div().flex_1().min_h(px(4.0)))
            .child(
                div()
                    .px(px(16.0))
                    .pb(px(14.0))
                    .pt(px(6.0))
                    .flex()
                    .justify_end()
                    .gap(px(10.0))
                    .when(self.shell_extract, |element| {
                        element.child(
                            Button::new("open-extract-main")
                                .outline()
                                .label("打开主窗口")
                                .disabled(self.running.is_some())
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.shell_progress = false;
                                    this.shell_extract = false;
                                    this.job_timing = None;
                                    this.close_deadline = None;
                                    window.resize(size(px(1200.0), px(800.0)));
                                    if let Some(path) = this.last_open_path.clone() {
                                        this.open_path(path, cx);
                                    }
                                })),
                        )
                    })
                    .child(
                        Button::new("background-job")
                            .outline()
                            .label("后台")
                            .disabled(self.running.is_none())
                            .on_click(|_, window, _| window.minimize_window()),
                    )
                    .when_some(self.close_deadline, |element, deadline| {
                        element.child(
                            div()
                                .self_center()
                                .text_size(px(12.0))
                                .text_color(palette.muted)
                                .child(format!(
                                    "{} 秒后自动关闭",
                                    countdown_seconds(deadline, Instant::now())
                                )),
                        )
                    })
                    .child(
                        Button::new("close-progress")
                            .label("关闭")
                            .disabled(self.running.is_some())
                            .on_click(|_, _, cx| cx.quit()),
                    ),
            )
    }

    fn render_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let nav = |id: &'static str, page: Page, icon: IconName, label: &'static str| {
            let active = self.page == page;
            Button::new(id)
                .custom(
                    ButtonCustomVariant::new(cx)
                        .color(if active { rgb(0x263e62) } else { rgb(0x142238) }.into())
                        .foreground(if active { rgb(0xffffff) } else { rgb(0xb9c9de) }.into())
                        .hover(rgb(0x293d59).into())
                        .active(rgb(0x263e62).into()),
                )
                .accessibility_label(label)
                .w_full()
                .h(px(42.0))
                .px(px(13.0))
                .rounded(px(8.0))
                .child(
                    div()
                        .w_full()
                        .flex()
                        .items_center()
                        .gap(px(8.0))
                        .child(Icon::new(icon).small())
                        .child(label),
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.page = page;
                    cx.notify();
                }))
        };

        div()
            .w(px(208.0))
            .h_full()
            .flex()
            .flex_col()
            .px(px(17.0))
            .py(px(26.0))
            .bg(rgb(0x142238))
            .text_color(rgb(0xffffff))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(10.0))
                    .child(
                        div()
                            .w(px(37.0))
                            .h(px(37.0))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(10.0))
                            .bg(rgb(0x3e7ae4))
                            .text_size(px(18.0))
                            .font_weight(FontWeight::BOLD)
                            .child("M"),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .child(
                                div()
                                    .text_size(px(20.0))
                                    .font_weight(FontWeight::BOLD)
                                    .child("MZip"),
                            )
                            .child(
                                div()
                                    .text_size(px(11.0))
                                    .text_color(rgb(0x9cb0cd))
                                    .child("轻松管理压缩文件"),
                            ),
                    ),
            )
            .child(div().h(px(42.0)))
            .child(
                div()
                    .px(px(12.0))
                    .pb(px(10.0))
                    .text_size(px(11.0))
                    .text_color(rgb(0x839bbd))
                    .child("工作区"),
            )
            .child(nav(
                "nav-explorer",
                Page::Explorer,
                IconName::FolderClosed,
                "资源管理器",
            ))
            .child(div().h(px(5.0)))
            .child(nav(
                "nav-browse",
                Page::Browse,
                IconName::FolderOpen,
                "浏览与解压",
            ))
            .child(div().h(px(5.0)))
            .child(nav(
                "nav-create",
                Page::Create,
                IconName::Plus,
                "创建压缩包",
            ))
            .child(div().h(px(5.0)))
            .child(nav(
                "nav-settings",
                Page::Settings,
                IconName::Settings,
                "设置",
            ))
            .child(div().flex_1())
            .child(
                div()
                    .rounded(px(9.0))
                    .p(px(12.0))
                    .bg(rgb(0x1c304c))
                    .text_size(px(12.0))
                    .text_color(rgb(0xc0cfe2))
                    .child(
                        div()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("简单 · 安全 · 原生"),
                    )
                    .child(
                        div()
                            .pt(px(5.0))
                            .text_color(rgb(0x98abc4))
                            .child("支持 ZIP、7z、RAR、CAB 和 TAR 等"),
                    ),
            )
    }

    fn render_header(&self, palette: Palette, cx: &mut Context<Self>) -> impl IntoElement {
        let (title, subtitle) = match self.page {
            Page::Explorer => ("资源管理器", "浏览文件，右键执行压缩和解压"),
            Page::Browse => ("浏览与解压", "查看归档内容，安全提取所需文件"),
            Page::Create => ("创建压缩包", "将文件和文件夹整理为 ZIP 或 7z"),
            Page::Settings => ("设置", "选择压缩和解压的完整性模式"),
        };
        div()
            .h(px(92.0))
            .px(px(28.0))
            .flex()
            .items_center()
            .justify_between()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(4.0))
                    .child(
                        div()
                            .text_size(px(24.0))
                            .font_weight(FontWeight::BOLD)
                            .child(title),
                    )
                    .child(
                        div()
                            .text_size(px(12.0))
                            .text_color(palette.muted)
                            .child(subtitle),
                    ),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(4.0))
                    .when(cfg!(windows), |row| {
                        #[cfg(windows)]
                        let row = row.child(
                            Button::new("default-apps")
                                .ghost()
                                .label("设置默认打开方式")
                                .on_click(cx.listener(|this, _, _, cx| this.open_default_apps(cx))),
                        );
                        row
                    })
                    .child(
                        Button::new("theme-toggle")
                            .ghost()
                            .icon(if self.dark {
                                IconName::Sun
                            } else {
                                IconName::Moon
                            })
                            .label(if self.dark { "浅色" } else { "深色" })
                            .on_click(
                                cx.listener(|this, _, window, cx| this.change_theme(window, cx)),
                            ),
                    ),
            )
    }

    fn render_browse(&self, palette: Palette, cx: &mut Context<Self>) -> AnyElement {
        let busy = self.running.is_some();
        let has_archive = self.archive.is_some();
        let archive_name = self
            .archive
            .as_ref()
            .and_then(|archive| archive.path.file_name())
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_else(|| "尚未打开压缩包".to_owned());
        let archive_details = self
            .archive
            .as_ref()
            .map(|archive| {
                format!(
                    "{} 个条目  ·  原始大小 {}  ·  压缩包 {}",
                    archive.entries.len(),
                    format_size(archive.total_size),
                    format_size(archive.archive_size)
                )
            })
            .unwrap_or_else(|| "支持拖入文件，或使用右侧按钮选择".to_owned());
        let path = self
            .archive
            .as_ref()
            .map(|archive| archive.path.display().to_string())
            .unwrap_or_default();

        div()
            .size_full()
            .flex()
            .flex_col()
            .gap(px(14.0))
            .child(
                div()
                    .rounded(px(12.0))
                    .border_1()
                    .border_color(palette.border)
                    .bg(palette.surface)
                    .p(px(16.0))
                    .flex()
                    .items_center()
                    .gap(px(14.0))
                    .child(
                        div()
                            .w(px(44.0))
                            .h(px(44.0))
                            .rounded(px(10.0))
                            .bg(palette.accent_soft)
                            .text_color(palette.accent)
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(Icon::new(IconName::FolderOpen)),
                    )
                    .child(
                        div()
                            .min_w_0()
                            .flex_1()
                            .flex()
                            .flex_col()
                            .gap(px(3.0))
                            .child(
                                div()
                                    .text_size(px(15.0))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .truncate()
                                    .child(archive_name),
                            )
                            .child(
                                div()
                                    .text_size(px(12.0))
                                    .text_color(palette.muted)
                                    .child(archive_details),
                            )
                            .child(
                                div()
                                    .text_size(px(11.0))
                                    .text_color(palette.muted)
                                    .truncate()
                                    .child(path),
                            ),
                    )
                    .child(
                        Button::new("open-archive")
                            .primary()
                            .icon(IconName::FolderOpen)
                            .label("打开压缩包")
                            .disabled(busy)
                            .on_click(cx.listener(|this, _, _, cx| this.choose_archive(cx))),
                    ),
            )
            .child(
                div()
                    .rounded(px(12.0))
                    .border_1()
                    .border_color(palette.border)
                    .bg(palette.surface)
                    .p(px(14.0))
                    .flex()
                    .flex_col()
                    .gap(px(12.0))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(8.0))
                            .child(
                                Button::new("extract-all")
                                    .primary()
                                    .label("解压全部")
                                    .disabled(busy || !has_archive)
                                    .on_click(
                                        cx.listener(|this, _, _, cx| this.extract(false, cx)),
                                    ),
                            )
                            .child(
                                Button::new("extract-selected")
                                    .outline()
                                    .label("解压选中")
                                    .disabled(busy || !has_archive || self.selected.is_empty())
                                    .on_click(cx.listener(|this, _, _, cx| this.extract(true, cx))),
                            )
                            .child(div().flex_1())
                            .child(
                                div().w(px(240.0)).child(
                                    Input::new(&self.search)
                                        .prefix(IconName::Search)
                                        .cleanable(true),
                                ),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(14.0))
                            .child(
                                div().w(px(260.0)).child(
                                    Input::new(&self.archive_password)
                                        .mask_toggle()
                                        .disabled(busy),
                                ),
                            )
                            .child(
                                Button::new("retry-open")
                                    .ghost()
                                    .small()
                                    .label("使用密码重新打开")
                                    .disabled(busy || self.last_open_path.is_none())
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        if let Some(path) = this.last_open_path.clone() {
                                            this.open_path(path, cx);
                                        }
                                    })),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(14.0))
                            .child(
                                Checkbox::new("smart-extract")
                                    .label("智能文件夹")
                                    .checked(self.smart_extract)
                                    .disabled(busy)
                                    .on_click(cx.listener(|this, checked, _, cx| {
                                        this.smart_extract = *checked;
                                        cx.notify();
                                    })),
                            )
                            .child(
                                Checkbox::new("open-after-extract")
                                    .label("完成后打开")
                                    .checked(self.open_after_extract)
                                    .disabled(busy)
                                    .on_click(cx.listener(|this, checked, _, cx| {
                                        this.open_after_extract = *checked;
                                        cx.notify();
                                    })),
                            ),
                    ),
            )
            .child(self.render_entries(palette, cx))
            .into_any_element()
    }

    fn render_entries(&self, palette: Palette, cx: &mut Context<Self>) -> impl IntoElement {
        let busy = self.running.is_some();
        let visible = self.visible_entries.len();
        let list = if visible == 0 {
            let hint = if self.archive.is_none() {
                "打开一个压缩包后，文件列表会显示在这里"
            } else {
                "没有匹配的文件"
            };
            div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .text_size(px(13.0))
                .text_color(palette.muted)
                .child(hint)
                .into_any_element()
        } else {
            uniform_list(
                "archive-entry-list",
                visible,
                cx.processor(move |this, range: std::ops::Range<usize>, _, cx| {
                    let Some(archive) = &this.archive else {
                        return Vec::new();
                    };
                    range
                        .filter_map(|row| {
                            let index = *this.visible_entries.get(row)?;
                            let entry = archive.entries.get(index)?;
                            let selected = this.selected.contains(&index);
                            let kind = if entry.is_directory {
                                "文件夹"
                            } else {
                                "文件"
                            };
                            let compressed = entry
                                .compressed_size
                                .map(format_size)
                                .unwrap_or_else(|| "—".to_owned());
                            let name = entry.name.clone();
                            Some(
                                div()
                                    .w_full()
                                    .h(px(39.0))
                                    .px(px(12.0))
                                    .flex()
                                    .items_center()
                                    .border_b_1()
                                    .border_color(palette.border)
                                    .bg(if selected {
                                        palette.accent_soft
                                    } else {
                                        palette.surface
                                    })
                                    .text_size(px(12.0))
                                    .child(
                                        div().w(px(38.0)).child(
                                            Checkbox::new(SharedString::from(format!(
                                                "entry-{index}"
                                            )))
                                            .accessibility_label(format!("选择 {name}"))
                                            .checked(selected)
                                            .disabled(busy)
                                            .on_click(cx.listener(move |this, checked, _, cx| {
                                                if *checked {
                                                    this.selected.insert(index);
                                                } else {
                                                    this.selected.remove(&index);
                                                }
                                                cx.notify();
                                            })),
                                        ),
                                    )
                                    .child(
                                        div()
                                            .flex_1()
                                            .min_w_0()
                                            .flex()
                                            .items_center()
                                            .gap(px(8.0))
                                            .child(
                                                Icon::new(if entry.is_directory {
                                                    IconName::FolderClosed
                                                } else {
                                                    IconName::File
                                                })
                                                .small(),
                                            )
                                            .child(div().flex_1().min_w_0().truncate().child(name)),
                                    )
                                    .child(div().w(px(70.0)).text_color(palette.muted).child(kind))
                                    .child(
                                        div()
                                            .w(px(90.0))
                                            .text_color(palette.muted)
                                            .child(format_size(entry.size)),
                                    )
                                    .child(
                                        div()
                                            .w(px(90.0))
                                            .text_color(palette.muted)
                                            .child(compressed),
                                    ),
                            )
                        })
                        .collect()
                }),
            )
            .w_full()
            .h_full()
            .into_any_element()
        };

        div()
            .flex_1()
            .min_h_0()
            .rounded(px(12.0))
            .border_1()
            .border_color(palette.border)
            .bg(palette.surface)
            .overflow_hidden()
            .flex()
            .flex_col()
            .child(
                div()
                    .h(px(47.0))
                    .px(px(14.0))
                    .flex()
                    .items_center()
                    .gap(px(7.0))
                    .child(
                        div()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_size(px(13.0))
                            .child("文件列表"),
                    )
                    .child(
                        div()
                            .text_color(palette.muted)
                            .text_size(px(12.0))
                            .child(format!("{visible} 项 · 已选 {}", self.selected.len())),
                    )
                    .child(div().flex_1())
                    .child(
                        Button::new("select-visible")
                            .ghost()
                            .small()
                            .label("全选结果")
                            .disabled(busy || visible == 0)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.selected.extend(this.visible_entries.iter().copied());
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("clear-selection")
                            .ghost()
                            .small()
                            .label("清除选择")
                            .disabled(busy || self.selected.is_empty())
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.selected.clear();
                                cx.notify();
                            })),
                    ),
            )
            .child(
                div()
                    .h(px(34.0))
                    .px(px(12.0))
                    .flex()
                    .items_center()
                    .bg(palette.surface_alt)
                    .text_size(px(11.0))
                    .text_color(palette.muted)
                    .child(div().w(px(38.0)))
                    .child(div().flex_1().child("名称"))
                    .child(div().w(px(70.0)).child("类型"))
                    .child(div().w(px(90.0)).child("大小"))
                    .child(div().w(px(90.0)).child("压缩后")),
            )
            .child(div().flex_1().min_h_0().child(list))
    }

    fn render_settings(&self, palette: Palette, cx: &mut Context<Self>) -> AnyElement {
        let busy = self.running.is_some();
        let mode_card =
            |mode: IntegrityMode, summary: &'static str, points: &[&'static str]| {
                let active = self.integrity_mode == mode;
                let title = if mode == IntegrityMode::Safe {
                    "安全模式"
                } else {
                    "高速模式（默认）"
                };
                let id = if mode == IntegrityMode::Safe {
                    "integrity-safe"
                } else {
                    "integrity-normal"
                };
                div()
                    .flex_1()
                    .min_w_0()
                    .rounded(px(12.0))
                    .border_2()
                    .border_color(if active {
                        palette.accent
                    } else {
                        palette.border
                    })
                    .bg(if active {
                        palette.accent_soft
                    } else {
                        palette.surface
                    })
                    .p(px(16.0))
                    .flex()
                    .flex_col()
                    .gap(px(8.0))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .gap(px(8.0))
                            .child(
                                div()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_size(px(15.0))
                                    .child(title),
                            )
                            .child(
                                format_button(
                                    id,
                                    if active {
                                        "使用中"
                                    } else {
                                        "使用此模式"
                                    },
                                    active,
                                )
                                .small()
                                .disabled(busy)
                                .on_click(cx.listener(
                                    move |this, _, _, cx| this.set_integrity_mode(mode, cx),
                                )),
                            ),
                    )
                    .child(
                        div()
                            .text_size(px(12.0))
                            .text_color(palette.muted)
                            .child(summary),
                    )
                    .children(points.iter().map(|point| {
                        div()
                            .flex()
                            .gap(px(6.0))
                            .text_size(px(12.0))
                            .child("•")
                            .child(div().flex_1().min_w_0().child(*point))
                    }))
            };
        let comparison = [
            ("压缩后回读压缩包", "自动，全部解码", "不进行"),
            ("与源文件逐字节比较", "进行", "不进行"),
            ("压缩期间源文件变化", "检测到则不生成压缩包", "无法发现"),
            (
                "解压中途失败或取消",
                "整个压缩包回滚，恢复被覆盖的文件",
                "已解压的文件保留",
            ),
            (
                "被覆盖的同名文件",
                "提交成功前保留备份，失败时恢复",
                "直接替换",
            ),
            ("速度", "较慢（压缩约多一遍读取和解码）", "较快"),
        ];
        let row = |cells: [&'static str; 3], header: bool| {
            div()
                .flex()
                .border_b_1()
                .border_color(palette.border)
                .text_size(px(12.0))
                .when(header, |row| {
                    row.font_weight(FontWeight::SEMIBOLD)
                        .bg(palette.surface_alt)
                })
                .children(cells.into_iter().enumerate().map(|(index, cell)| {
                    div()
                        .when(index == 0, |cell| cell.w(px(150.0)))
                        .when(index > 0, |cell| cell.flex_1())
                        .min_w_0()
                        .px(px(10.0))
                        .py(px(7.0))
                        .child(cell)
                }))
        };

        div()
            .id("settings-scroll")
            .size_full()
            .overflow_y_scroll()
            .child(
                div()
                    .w_full()
                    .flex()
                    .flex_col()
                    .gap(px(14.0))
                    .pb(px(10.0))
                    .child(
                        div()
                            .rounded(px(12.0))
                            .border_1()
                            .border_color(palette.border)
                            .bg(palette.surface)
                            .p(px(18.0))
                            .flex()
                            .flex_col()
                            .gap(px(12.0))
                            .child(section_title("1", "完整性模式", palette))
                            .child(
                                div()
                                    .text_size(px(12.0))
                                    .text_color(palette.muted)
                                    .child(
                                        "该设置对此后的所有压缩和解压任务生效，包括资源管理器右键菜单发起的任务。",
                                    ),
                            )
                            .child(
                                div()
                                    .text_size(px(12.0))
                                    .text_color(palette.muted)
                                    .child(
                                        "拖到外部程序时，目标由接收程序写入，失败或取消不能保证回滚；重要文件请使用“解压所选到”。",
                                    ),
                            )
                            .child(
                                div()
                                    .flex()
                                    .gap(px(12.0))
                                    .child(mode_card(
                                        IntegrityMode::Safe,
                                        "适合重要备份：允许失败，但不会留下不完整的结果。",
                                        &[
                                            "压缩完成后完整回读压缩包，并与源文件逐字节比较；全部一致才生成压缩包。",
                                            "压缩期间源文件被修改、增加或删除时，放弃本次压缩。",
                                            "解压先写入临时文件夹，全部校验通过后再提交到目标位置。",
                                            "任一文件失败或取消时，回滚当前压缩包的全部输出，并恢复被覆盖的原文件。",
                                        ],
                                    ))
                                    .child(mode_card(
                                        IntegrityMode::Normal,
                                        "与 7-Zip 等常见压缩软件的行为一致，速度更快。",
                                        &[
                                            "压缩完成后不回读、不与源文件比较；失败时仍不会留下半成品压缩包。",
                                            "解压直接写入目标文件夹，仍会校验格式自带的 CRC 等校验值。",
                                            "中途失败或取消时，已解压的文件会保留，结果可能不完整。",
                                            "适合日常临时使用；如需确认，可在完成后手动测试或比较。",
                                        ],
                                    )),
                            ),
                    )
                    .child(
                        div()
                            .rounded(px(12.0))
                            .border_1()
                            .border_color(palette.border)
                            .bg(palette.surface)
                            .p(px(18.0))
                            .flex()
                            .flex_col()
                            .gap(px(12.0))
                            .child(section_title("2", "两种模式的区别", palette))
                            .child(
                                div()
                                    .rounded(px(8.0))
                                    .border_1()
                                    .border_color(palette.border)
                                    .child(row(["项目", "安全模式", "高速模式"], true))
                                    .children(comparison.into_iter().map(|(item, safe, normal)| {
                                        row([item, safe, normal], false)
                                    })),
                            )
                            .child(
                                div()
                                    .text_size(px(11.0))
                                    .text_color(palette.muted)
                                    .child(
                                        "两种模式都无法防止断电、系统崩溃或强制结束进程造成的问题；重要数据请保留多份备份。",
                                    ),
                            ),
                    ),
            )
            .into_any_element()
    }

    fn render_create(&self, palette: Palette, cx: &mut Context<Self>) -> AnyElement {
        let busy = self.running.is_some();
        let source_rows = self
            .sources
            .iter()
            .enumerate()
            .map(|(index, path)| {
                let name = path.display().to_string();
                div()
                    .h(px(36.0))
                    .px(px(10.0))
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .border_b_1()
                    .border_color(palette.border)
                    .child(
                        Icon::new(if path.is_dir() {
                            IconName::FolderClosed
                        } else {
                            IconName::File
                        })
                        .small(),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(px(12.0))
                            .child(name),
                    )
                    .child(
                        Button::new(SharedString::from(format!("remove-source-{index}")))
                            .ghost()
                            .small()
                            .icon(IconName::Close)
                            .disabled(busy)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if index < this.sources.len() {
                                    this.sources.remove(index);
                                    cx.notify();
                                }
                            })),
                    )
            })
            .collect::<Vec<_>>();
        let destination = self
            .destination
            .as_ref()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "尚未选择保存位置".to_owned());

        div()
            .id("create-scroll")
            .size_full()
            .overflow_y_scroll()
            .child(
                div()
                    .w_full()
                    .flex()
                    .flex_col()
                    .gap(px(14.0))
                    .pb(px(10.0))
                    .child(
                        div()
                            .rounded(px(12.0))
                            .border_1()
                            .border_color(palette.border)
                            .bg(palette.surface)
                            .p(px(18.0))
                            .flex()
                            .flex_col()
                            .gap(px(12.0))
                            .child(section_title("1", "选择文件", palette))
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap(px(8.0))
                                    .child(
                                        Button::new("add-files")
                                            .outline()
                                            .icon(IconName::Plus)
                                            .label("添加文件")
                                            .disabled(busy)
                                            .on_click(
                                                cx.listener(|this, _, _, cx| this.choose_files(cx)),
                                            ),
                                    )
                                    .child(
                                        Button::new("add-directory")
                                            .outline()
                                            .icon(IconName::FolderOpen)
                                            .label("添加文件夹")
                                            .disabled(busy)
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.choose_directory(cx)
                                            })),
                                    )
                                    .child(div().flex_1())
                                    .child(
                                        Button::new("clear-sources")
                                            .ghost()
                                            .label("清空")
                                            .disabled(busy || self.sources.is_empty())
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.sources.clear();
                                                cx.notify();
                                            })),
                                    ),
                            )
                            .child(
                                div()
                                    .id("source-list")
                                    .max_h(px(150.0))
                                    .overflow_y_scroll()
                                    .rounded(px(8.0))
                                    .bg(palette.surface_alt)
                                    .when(self.sources.is_empty(), |element| {
                                        element.child(
                                            div()
                                                .p(px(12.0))
                                                .text_size(px(12.0))
                                                .text_color(palette.muted)
                                                .child("添加文件或文件夹，也可以直接拖到窗口中。"),
                                        )
                                    })
                                    .children(source_rows),
                            ),
                    )
                    .child(
                        div()
                            .rounded(px(12.0))
                            .border_1()
                            .border_color(palette.border)
                            .bg(palette.surface)
                            .p(px(18.0))
                            .flex()
                            .flex_col()
                            .gap(px(12.0))
                            .child(section_title("2", "设置压缩包", palette))
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap(px(10.0))
                                    .child(
                                        div()
                                            .w(px(76.0))
                                            .text_size(px(12.0))
                                            .text_color(palette.muted)
                                            .child("压缩格式"),
                                    )
                                    .child(
                                        format_button(
                                            "format-zip",
                                            "ZIP",
                                            self.format == ArchiveFormat::Zip,
                                        )
                                        .disabled(busy)
                                        .on_click(
                                            cx.listener(|this, _, _, cx| {
                                                this.set_format(ArchiveFormat::Zip, cx)
                                            }),
                                        ),
                                    )
                                    .child(
                                        format_button(
                                            "format-7z",
                                            "7z",
                                            self.format == ArchiveFormat::SevenZip,
                                        )
                                        .disabled(busy)
                                        .on_click(
                                            cx.listener(|this, _, _, cx| {
                                                this.set_format(ArchiveFormat::SevenZip, cx)
                                            }),
                                        ),
                                    ),
                            )
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap(px(10.0))
                                    .child(
                                        div()
                                            .w(px(76.0))
                                            .text_size(px(12.0))
                                            .text_color(palette.muted)
                                            .child("保存到"),
                                    )
                                    .child(
                                        div()
                                            .flex_1()
                                            .min_w_0()
                                            .px(px(10.0))
                                            .py(px(9.0))
                                            .rounded(px(6.0))
                                            .bg(palette.surface_alt)
                                            .text_size(px(12.0))
                                            .truncate()
                                            .child(destination),
                                    )
                                    .child(
                                        Button::new("choose-destination")
                                            .outline()
                                            .label("选择位置")
                                            .disabled(busy)
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.choose_destination(cx)
                                            })),
                                    ),
                            )
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap(px(10.0))
                                    .child(
                                        div()
                                            .w(px(76.0))
                                            .text_size(px(12.0))
                                            .text_color(palette.muted)
                                            .child("加密密码"),
                                    )
                                    .child(
                                        div().flex_1().child(
                                            Input::new(&self.create_password)
                                                .mask_toggle()
                                                .disabled(busy),
                                        ),
                                    )
                                    .child(
                                        div().flex_1().child(
                                            Input::new(&self.confirm_password)
                                                .mask_toggle()
                                                .disabled(busy),
                                        ),
                                    ),
                            )
                            .child(
                                div()
                                    .pl(px(86.0))
                                    .text_size(px(11.0))
                                    .text_color(palette.muted)
                                    .child("密码留空为不加密；ZIP 的文件名仍可见。"),
                            )
                            .child(
                                div().pl(px(86.0)).child(
                                    Checkbox::new("overwrite-create")
                                        .label("允许覆盖已有压缩包")
                                        .checked(self.overwrite_create)
                                        .disabled(busy)
                                        .on_click(cx.listener(|this, checked, _, cx| {
                                            this.overwrite_create = *checked;
                                            cx.notify();
                                        })),
                                ),
                            ),
                    )
                    .child(
                        div().flex().justify_end().child(
                            Button::new("start-create")
                                .primary()
                                .icon(IconName::Plus)
                                .label("开始压缩")
                                .disabled(busy)
                                .on_click(cx.listener(|this, _, _, cx| this.create(cx))),
                        ),
                    ),
            )
            .into_any_element()
    }

    fn job_progress_title(&self) -> &'static str {
        match self.job_kind {
            JobKind::Extract | JobKind::Drag => "解压进度",
            JobKind::Split => "分割进度",
            JobKind::Merge => "合并进度",
            JobKind::Update => "更新进度",
            _ => "压缩进度",
        }
    }

    fn render_job_progress(&self, palette: Palette) -> Option<Div> {
        let mut section = div().flex().flex_col().gap(px(7.0));
        let mut empty = true;
        if self.job_kind == JobKind::CreateEach {
            if let Some(progress) = &self.progress {
                empty = false;
                section = section.child(
                    div()
                        .text_size(px(11.0))
                        .text_color(palette.muted)
                        .truncate()
                        .child(format!("{}：{}", progress.operation, progress.current_file)),
                );
            }
        } else if let Some(progress) = &self.progress {
            empty = false;
            let completed = self
                .job_timing
                .as_ref()
                .is_some_and(|timing| timing.completed());
            // Verification only starts after compression or extraction has finished.
            let done = completed || self.verify_progress.is_some();
            let fraction = if done {
                1.0
            } else if progress.total_bytes == 0 {
                0.0
            } else {
                (progress.completed_bytes as f64 / progress.total_bytes as f64).clamp(0.0, 1.0)
                    as f32
            };
            let operation = if done {
                match self.job_kind {
                    JobKind::Extract | JobKind::Drag => "解压完成",
                    JobKind::Split => "分割完成",
                    JobKind::Merge => "合并完成",
                    _ => "压缩完成",
                }
            } else if progress.operation == "正在压缩"
                && progress.total_bytes > 0
                && progress.completed_bytes == progress.total_bytes
            {
                "数据已读入，正在完成压缩"
            } else {
                progress.operation
            };
            let current = if done {
                operation.to_owned()
            } else {
                format!("{}：{}", operation, progress.current_file)
            };
            let percentage = if self.job_timing.is_some() && progress.total_bytes == 0 && !done {
                "--%".to_owned()
            } else {
                format!("{:.0}%", fraction * 100.0)
            };
            let amount = format!(
                "{} / {}",
                format_size(progress.completed_bytes),
                format_size(progress.total_bytes)
            );
            section = section
                .child(
                    div()
                        .flex()
                        .gap(px(10.0))
                        .items_center()
                        .text_size(px(11.0))
                        .text_color(palette.muted)
                        .child(div().flex_1().min_w_0().truncate().child(current))
                        .child(if self.job_kind == JobKind::Create {
                            format!("{percentage} · 已读入 {amount}")
                        } else {
                            format!("{percentage} · {amount}")
                        }),
                )
                .child(progress_bar(fraction, palette));
        } else if self.job_timing.is_some() && self.running.is_some() {
            empty = false;
            section = section.child(progress_bar(0.0, palette));
        }
        if let Some(timing) = &self.job_timing {
            empty = false;
            let cancelled = self
                .running
                .as_ref()
                .is_some_and(|token| token.is_cancelled());
            let metrics = timing.metrics(self.progress.as_ref(), Instant::now(), cancelled);
            let speed = metrics
                .speed_mib_per_second
                .map_or_else(|| "--".to_owned(), |value| format!("{value:.2} MB/s"));
            let remaining = metrics
                .remaining
                .map_or_else(|| "--".to_owned(), format_duration);
            let original_size = self.progress.as_ref().map_or_else(
                || "--".to_owned(),
                |progress| format_size(progress.total_bytes),
            );
            let archive_size = self
                .progress
                .as_ref()
                .and_then(|progress| progress.archive_size)
                .map_or_else(|| "--".to_owned(), format_size);
            let ratio = metrics
                .ratio_percent
                .map_or_else(|| "--".to_owned(), |value| format!("{value:.1}%"));
            let complete = timing.completed();
            section = section.child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(5.0))
                    .text_size(px(11.0))
                    .text_color(palette.muted)
                    .child(
                        div()
                            .flex()
                            .gap(px(8.0))
                            .child(progress_metric("平均速度", speed))
                            .child(progress_metric(
                                "已用时间",
                                format_duration(metrics.elapsed),
                            ))
                            .child(progress_metric("预计剩余", remaining)),
                    )
                    .when(self.job_kind == JobKind::Create, |element| {
                        element.child(
                            div()
                                .flex()
                                .gap(px(8.0))
                                .child(progress_metric("压缩前", original_size))
                                .child(progress_metric(
                                    if complete {
                                        "压缩后"
                                    } else {
                                        "压缩后（当前）"
                                    },
                                    archive_size,
                                ))
                                .child(progress_metric(
                                    if complete {
                                        "压缩率"
                                    } else {
                                        "压缩率（约）"
                                    },
                                    ratio,
                                )),
                        )
                    }),
            );
        }
        (!empty).then_some(section)
    }

    fn render_verify_progress(&self, palette: Palette) -> Option<Div> {
        let timing = self.job_timing.as_ref()?;
        if !matches!(
            self.job_kind,
            JobKind::Create | JobKind::Extract | JobKind::Drag
        ) {
            return None;
        }
        let completed = timing.completed();
        let extract = matches!(self.job_kind, JobKind::Extract | JobKind::Drag);
        let line = |text: String, detail: Option<String>| {
            div()
                .flex()
                .gap(px(10.0))
                .items_center()
                .text_size(px(11.0))
                .text_color(palette.muted)
                .child(div().flex_1().min_w_0().truncate().child(text))
                .when_some(detail, |element, detail| element.child(detail))
        };
        let section = div().flex().flex_col().gap(px(7.0));
        if self.job_integrity != IntegrityMode::Safe {
            let text = match (extract, completed) {
                (true, true) => "已通过格式自带的 CRC 等校验（高速模式，不做额外校验）",
                (true, false) => "高速模式：解压时同步校验格式自带的 CRC 等校验值",
                (false, _) => "高速模式：压缩后不回读校验，可在设置中切换为安全模式",
            };
            return Some(section.child(line(text.to_owned(), None)));
        }
        let (text, fraction, detail) = match &self.verify_progress {
            Some(progress) => {
                let fraction = if completed {
                    1.0
                } else if progress.total_bytes == 0 {
                    0.0
                } else {
                    (progress.completed_bytes as f64 / progress.total_bytes as f64).clamp(0.0, 1.0)
                        as f32
                };
                let text = match (completed, extract) {
                    (true, true) => "校验通过，解压结果已提交".to_owned(),
                    (true, false) => "校验通过：压缩包与源文件逐字节一致".to_owned(),
                    (false, _) => format!("{}：{}", progress.operation, progress.current_file),
                };
                let detail = format!(
                    "{:.0}% · {} / {}",
                    fraction * 100.0,
                    format_size(progress.completed_bytes),
                    format_size(progress.total_bytes)
                );
                (text, fraction, Some(detail))
            }
            None if completed => ("校验通过".to_owned(), 1.0, None),
            None if self.running.is_some() => (
                if extract {
                    "解压时同步校验 CRC，完成后校验全部解压结果"
                } else {
                    "等待压缩完成后回读压缩包并与源文件比较"
                }
                .to_owned(),
                0.0,
                None,
            ),
            None => ("校验未完成".to_owned(), 0.0, None),
        };
        Some(
            section
                .child(line(text, detail))
                .child(progress_bar(fraction, palette)),
        )
    }

    fn render_status(&self, palette: Palette, cx: &mut Context<Self>) -> impl IntoElement {
        let shell = self.shell_progress;
        let footer = div().flex().flex_col();
        let mut footer = if shell {
            footer.px(px(20.0)).gap(px(8.0))
        } else {
            footer
                .px(px(28.0))
                .py(px(12.0))
                .border_t_1()
                .border_color(palette.border)
                .bg(palette.surface)
                .gap(px(7.0))
        };
        let sections = [
            (self.job_progress_title(), self.render_job_progress(palette)),
            ("校验进度", self.render_verify_progress(palette)),
        ];
        for (title, section) in sections {
            if let Some(section) = section {
                footer = footer.child(if shell {
                    progress_card(title, section, palette)
                } else {
                    section
                });
            }
        }
        let full_status = self.status.clone();
        footer.child(
            div()
                .flex()
                .items_center()
                .gap(px(10.0))
                .child(
                    div()
                        .id("job-status")
                        .flex_1()
                        .min_w_0()
                        .text_size(px(12.0))
                        .text_color(if self.status_is_error {
                            palette.danger
                        } else {
                            palette.muted
                        })
                        .when(self.shell_progress, |element| {
                            element.line_clamp(2).text_ellipsis_middle().tooltip(
                                move |window, cx| {
                                    full_text_tooltip(full_status.clone(), window, cx)
                                },
                            )
                        })
                        .child(self.status.clone()),
                )
                .when(self.running.is_some(), |element| {
                    let cancelled = self
                        .running
                        .as_ref()
                        .is_some_and(|token| token.is_cancelled());
                    let paused = self.running.as_ref().is_some_and(|token| token.is_paused());
                    element
                        .child(
                            Button::new("pause-job")
                                .outline()
                                .small()
                                .label(if paused { "继续" } else { "暂停" })
                                .disabled(cancelled)
                                .on_click(cx.listener(|this, _, _, cx| this.toggle_pause(cx))),
                        )
                        .child(
                            Button::new("cancel-job")
                                .outline()
                                .small()
                                .label("取消任务")
                                .disabled(cancelled)
                                .on_click(cx.listener(|this, _, _, cx| this.cancel(cx))),
                        )
                }),
        )
    }
}

fn full_text_tooltip(text: String, window: &mut Window, cx: &mut App) -> AnyView {
    Tooltip::element(move |_, _| {
        div()
            .w(px(480.0))
            .min_w_0()
            .whitespace_normal()
            .child(text.clone())
    })
    .build(window, cx)
}

fn progress_bar(fraction: f32, palette: Palette) -> Div {
    div()
        .h(px(4.0))
        .w_full()
        .rounded(px(2.0))
        .bg(palette.border)
        .child(
            div()
                .h_full()
                .w(relative(fraction))
                .rounded(px(2.0))
                .bg(palette.accent),
        )
}

fn progress_card(title: &'static str, body: Div, palette: Palette) -> Div {
    div()
        .rounded(px(10.0))
        .border_1()
        .border_color(palette.border)
        .bg(palette.surface)
        .px(px(14.0))
        .py(px(10.0))
        .flex()
        .flex_col()
        .gap(px(6.0))
        .child(
            div()
                .text_size(px(12.0))
                .font_weight(FontWeight::SEMIBOLD)
                .child(title),
        )
        .child(body)
}

fn progress_metric(label: &str, value: String) -> impl IntoElement {
    div()
        .flex_1()
        .min_w_0()
        .truncate()
        .child(format!("{label}：{value}"))
}

fn section_title(number: &'static str, label: &'static str, palette: Palette) -> impl IntoElement {
    div()
        .flex()
        .items_center()
        .gap(px(9.0))
        .child(
            div()
                .w(px(24.0))
                .h(px(24.0))
                .rounded(px(6.0))
                .bg(palette.accent_soft)
                .text_color(palette.accent)
                .flex()
                .items_center()
                .justify_center()
                .text_size(px(12.0))
                .font_weight(FontWeight::BOLD)
                .child(number),
        )
        .child(
            div()
                .font_weight(FontWeight::SEMIBOLD)
                .text_size(px(14.0))
                .child(label),
        )
}

fn format_button(id: &'static str, label: &'static str, active: bool) -> Button {
    let button = Button::new(id).label(label);
    if active {
        button.primary()
    } else {
        button.outline()
    }
}
