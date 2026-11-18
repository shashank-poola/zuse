//! Preview the bytes staged in the composer without depending on their original path.
use gpui::{
    AnyElement, Context, Focusable, IntoElement, KeyDownEvent, ParentElement, SharedString, Styled,
    Window, div, prelude::*, px,
};
use std::sync::Arc;

use super::Composer;
use crate::{attachments, icons, theme::Theme};

#[derive(Clone)]
pub(super) struct FilePreview {
    name: String,
    text: String,
}

pub(super) struct PdfPreview {
    name: String,
    bytes: Arc<Vec<u8>>,
    index: usize,
    pages: usize,
}

enum PreviewResult {
    Pdf { png: Vec<u8>, pages: usize },
    Text(String),
}

fn preview_contents(name: &str, bytes: &Arc<Vec<u8>>, index: usize) -> PreviewResult {
    if name.to_ascii_lowercase().ends_with(".pdf") {
        let result = attachments::render_pdf_page(bytes.clone(), index, 1600.0);
        return match result {
            Some((png, pages)) => PreviewResult::Pdf { png, pages },
            None => PreviewResult::Text("This PDF could not be previewed.".into()),
        };
    }
    const MAX_TEXT: usize = 256 * 1024;
    match std::str::from_utf8(bytes) {
        Ok(text) if !text.contains('\0') => {
            let end = text.floor_char_boundary(text.len().min(MAX_TEXT));
            let mut text = text[..end].to_owned();
            if end < bytes.len() {
                text.push_str("\n\nPreview truncated.");
            }
            PreviewResult::Text(text)
        }
        _ => PreviewResult::Text("A preview is not available for this file type.".into()),
    }
}

impl Composer {
    pub(super) fn clear_attachment_preview(&mut self) {
        self.preview = None;
        self.file_preview = None;
        self.pdf_preview = None;
        self.file_preview_generation = self.file_preview_generation.wrapping_add(1);
        self.preview_focus_pending = false;
    }

    pub(super) fn open_file_preview(&mut self, id: &str, cx: &mut Context<Self>) {
        let Some(att) = self
            .staged()
            .iter()
            .find(|att| att.id == id && att.image().is_none())
        else {
            return;
        };
        let attachments::AttachmentContent::File(bytes) = &att.content else {
            return;
        };
        let (name, bytes, id, key) = (
            att.name.clone(),
            bytes.clone(),
            id.to_owned(),
            self.current_key.clone(),
        );
        self.file_preview_generation = self.file_preview_generation.wrapping_add(1);
        let generation = self.file_preview_generation;
        self.preview = None;
        self.pdf_preview = None;
        self.file_preview = Some(FilePreview {
            name: name.clone(),
            text: "Loading preview…".into(),
        });
        self.preview_focus_pending = true;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let render_name = name.clone();
            let render_bytes = bytes.clone();
            let result = cx
                .background_executor()
                .spawn(async move { preview_contents(&render_name, &render_bytes, 0) })
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.file_preview_generation != generation
                    || this.current_key != key
                    || !this.staged().iter().any(|att| att.id == id)
                    || this.file_preview.is_none()
                {
                    return;
                }
                match result {
                    PreviewResult::Text(text) => {
                        if let Some(preview) = this.file_preview.as_mut() {
                            preview.text = text;
                        }
                    }
                    PreviewResult::Pdf { png, pages } => {
                        let name = this.file_preview.take().unwrap().name;
                        this.pdf_preview = Some(PdfPreview {
                            name: name.clone(),
                            bytes,
                            index: 0,
                            pages,
                        });
                        let image = gpui::Image::from_bytes(gpui::ImageFormat::Png, png);
                        this.preview = Some(attachments::PreviewImage::new(name, Arc::new(image)));
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub(super) fn step_pdf_preview(&mut self, step: isize, cx: &mut Context<Self>) {
        let Some(pdf) = self.pdf_preview.as_mut() else {
            return;
        };
        let index = pdf.index as isize + step;
        if index < 0 || index >= pdf.pages as isize {
            return;
        }
        let (name, bytes, index) = (pdf.name.clone(), pdf.bytes.clone(), index as usize);
        self.file_preview_generation = self.file_preview_generation.wrapping_add(1);
        let generation = self.file_preview_generation;
        cx.spawn(async move |this, cx| {
            let render_name = name.clone();
            let result = cx
                .background_executor()
                .spawn(async move { preview_contents(&render_name, &bytes, index) })
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.file_preview_generation != generation
                    || this.pdf_preview.is_none()
                    || this.preview.is_none()
                {
                    return;
                }
                if let PreviewResult::Pdf { png, .. } = result {
                    this.pdf_preview.as_mut().unwrap().index = index;
                    let image = gpui::Image::from_bytes(gpui::ImageFormat::Png, png);
                    this.preview = Some(attachments::PreviewImage::new(name, Arc::new(image)));
                } else {
                    crate::toast::show(
                        cx,
                        crate::toast::ToastKind::Error,
                        "Couldn't load this PDF page.",
                    );
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    pub(super) fn pdf_gallery(&self, cx: &mut Context<Self>) -> Option<attachments::LightboxNav> {
        let pdf = self.pdf_preview.as_ref()?;
        let weak = cx.weak_entity();
        Some(attachments::LightboxNav {
            index: pdf.index,
            count: pdf.pages,
            thumbs: Vec::new(),
            swipe: self.pdf_swipe.clone(),
            on_step: std::rc::Rc::new(move |step, _, cx| {
                let _ = weak.update(cx, |this, cx| this.step_pdf_preview(step, cx));
            }),
        })
    }

    fn close_file_preview(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.file_preview_generation = self.file_preview_generation.wrapping_add(1);
        self.file_preview = None;
        window.focus(&self.input.focus_handle(cx), cx);
        cx.notify();
    }

    pub(super) fn render_file_preview(
        &self,
        preview: FilePreview,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let viewport = window.viewport_size();
        let theme = Theme::of(cx);
        gpui::deferred(
            gpui::anchored()
                .position(gpui::point(px(0.0), px(0.0)))
                .child(
                    div()
                        .id("file-preview-overlay")
                        .occlude()
                        .track_focus(&self.preview_focus)
                        .w(viewport.width)
                        .h(viewport.height)
                        .bg(crate::popover::scrim_alpha(0.7))
                        .flex()
                        .items_center()
                        .justify_center()
                        .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                            if event.keystroke.key == "escape" {
                                cx.stop_propagation();
                                this.close_file_preview(window, cx);
                            }
                        }))
                        .on_click(
                            cx.listener(|this, _, window, cx| this.close_file_preview(window, cx)),
                        )
                        .child(
                            div()
                                .id("file-preview-dialog")
                                .w(px((f32::from(viewport.width) * 0.8).min(720.0)))
                                .max_h(px(f32::from(viewport.height) * 0.8))
                                .bg(theme.bg)
                                .border_1()
                                .border_color(theme.border)
                                .rounded(px(12.0))
                                .overflow_hidden()
                                .flex()
                                .flex_col()
                                .on_click(|_, _, cx| cx.stop_propagation())
                                .child(
                                    div()
                                        .flex()
                                        .items_center()
                                        .justify_between()
                                        .gap(px(12.0))
                                        .p(px(16.0))
                                        .child(
                                            div()
                                                .min_w_0()
                                                .truncate()
                                                .text_color(theme.text)
                                                .child(preview.name),
                                        )
                                        .child(
                                            div()
                                                .id("file-preview-close")
                                                .cursor_pointer()
                                                .role(gpui::Role::Button)
                                                .aria_label("Close preview")
                                                .on_click(cx.listener(|this, _, window, cx| {
                                                    cx.stop_propagation();
                                                    this.close_file_preview(window, cx);
                                                }))
                                                .child(
                                                    icons::icon(icons::CLOSE)
                                                        .size(px(18.0))
                                                        .text_color(theme.text_muted),
                                                ),
                                        ),
                                )
                                .child(
                                    div()
                                        .id("file-preview-content")
                                        .overflow_y_scroll()
                                        .p(px(16.0))
                                        .text_color(theme.text)
                                        .text_size(px(13.0))
                                        .child(SharedString::from(preview.text)),
                                ),
                        ),
                ),
        )
        .priority(3)
        .into_any_element()
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn two_page_pdf() -> Vec<u8> {
        let objects = [
            "<< /Type /Catalog /Pages 2 0 R >>",
            "<< /Type /Pages /Kids [3 0 R 4 0 R] /Count 2 >>",
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] /Resources << >> >>",
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 150] /Resources << >> >>",
        ];
        let mut pdf = b"%PDF-1.4\n".to_vec();
        let mut offsets = Vec::new();
        for (index, object) in objects.iter().enumerate() {
            offsets.push(pdf.len());
            pdf.extend_from_slice(format!("{} 0 obj\n{}\nendobj\n", index + 1, object).as_bytes());
        }
        let xref = pdf.len();
        pdf.extend_from_slice(
            format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes(),
        );
        for offset in offsets {
            pdf.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
        }
        pdf.extend_from_slice(
            format!(
                "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
                objects.len() + 1
            )
            .as_bytes(),
        );
        pdf
    }

    #[gpui::test]
    fn staged_pdf_thumbnail_loads_and_is_removed_with_attachment(cx: &mut gpui::TestAppContext) {
        let (_dir, handle) = super::super::tests::composer_focus_window(cx);
        let id = handle
            .update(cx, |composer, _, cx| {
                let key = composer.current_key.clone();
                composer.adopt_staged_attachments(
                    key,
                    vec![attachments::StagedAttachment::from_file(
                        "test.pdf",
                        two_page_pdf(),
                    )],
                    cx,
                );
                let id = composer.staged()[0].id.clone();
                let theme = Theme::of(cx).clone();
                composer.render_image_strip(&theme, cx);
                id
            })
            .unwrap();
        cx.run_until_parked();
        handle
            .read_with(cx, |composer, _| {
                assert!(
                    composer
                        .document_thumbnails
                        .get(&id)
                        .is_some_and(Option::is_some)
                );
            })
            .unwrap();
        handle
            .update(cx, |composer, _, cx| {
                composer.remove_attachment(&id, cx);
                let theme = Theme::of(cx).clone();
                composer.render_image_strip(&theme, cx);
                assert!(!composer.document_thumbnails.contains_key(&id));
            })
            .unwrap();
    }

    #[gpui::test]
    fn clearing_preview_cancels_pending_file_load(cx: &mut gpui::TestAppContext) {
        let (_dir, handle) = super::super::tests::composer_focus_window(cx);
        handle
            .update(cx, |composer, _, cx| {
                let key = composer.current_key.clone();
                composer.adopt_staged_attachments(
                    key,
                    vec![attachments::StagedAttachment::from_file(
                        "notes.txt",
                        b"hello".to_vec(),
                    )],
                    cx,
                );
                let id = composer.staged()[0].id.clone();
                composer.open_file_preview(&id, cx);
                assert!(composer.file_preview.is_some());
                composer.clear_attachment_preview();
            })
            .unwrap();
        cx.run_until_parked();
        handle
            .read_with(cx, |composer, _| {
                assert!(composer.file_preview.is_none());
                assert!(composer.pdf_preview.is_none());
                assert!(composer.preview.is_none());
            })
            .unwrap();
    }

    #[gpui::test]
    fn pdf_preview_opens_and_navigates_pages(cx: &mut gpui::TestAppContext) {
        let (_dir, handle) = super::super::tests::composer_focus_window(cx);
        handle
            .update(cx, |composer, _, cx| {
                let key = composer.current_key.clone();
                composer.adopt_staged_attachments(
                    key,
                    vec![attachments::StagedAttachment::from_file(
                        "test.pdf",
                        two_page_pdf(),
                    )],
                    cx,
                );
                let id = composer.staged()[0].id.clone();
                composer.open_file_preview(&id, cx);
            })
            .unwrap();
        cx.run_until_parked();
        handle
            .read_with(cx, |composer, _| {
                assert!(composer.preview.is_some());
                assert_eq!(composer.pdf_preview.as_ref().unwrap().pages, 2);
            })
            .unwrap();
        handle
            .update(cx, |composer, _, cx| composer.step_pdf_preview(1, cx))
            .unwrap();
        cx.run_until_parked();
        handle
            .read_with(cx, |composer, _| {
                assert!(composer.preview.is_some());
                assert_eq!(composer.pdf_preview.as_ref().unwrap().index, 1);
            })
            .unwrap();
        handle
            .update(cx, |composer, _, cx| {
                composer.pdf_preview.as_mut().unwrap().bytes = Arc::new(Vec::new());
                composer.step_pdf_preview(-1, cx);
            })
            .unwrap();
        cx.run_until_parked();
        handle
            .read_with(cx, |composer, _| {
                assert_eq!(composer.pdf_preview.as_ref().unwrap().index, 1);
                assert!(composer.preview.is_some());
            })
            .unwrap();
    }

    #[test]
    fn previews_each_pdf_page_and_handles_invalid_documents() {
        let pdf = Arc::new(two_page_pdf());
        for page in 0..2 {
            match preview_contents("test.pdf", &pdf, page) {
                PreviewResult::Pdf { png, pages } => {
                    assert_eq!(pages, 2);
                    assert!(png.starts_with(b"\x89PNG\r\n\x1a\n"));
                }
                _ => panic!("expected a rendered PDF page"),
            }
        }
        assert!(matches!(
            preview_contents("bad.pdf", &Arc::new(b"not a pdf".to_vec()), 0),
            PreviewResult::Text(_)
        ));
    }

    #[test]
    fn pdf_thumbnail_renders_the_complete_page_within_its_pixel_budget() {
        let pdf = Arc::new(two_page_pdf());
        let (png, pages) = attachments::render_pdf_page(pdf.clone(), 0, 256.0).unwrap();
        let (width, height) = crate::appshots::png_dimensions(&png).unwrap();
        assert_eq!(pages, 2);
        assert!(width > 0 && height > 0 && width.max(height) <= 256);
        assert!(attachments::render_pdf_page(pdf, 2, 256.0).is_none());
    }

    #[test]
    fn previews_text_and_handles_binary_files() {
        assert!(
            matches!(preview_contents("notes.txt", &Arc::new("hello ✓".as_bytes().to_vec()), 0), PreviewResult::Text(text) if text == "hello ✓")
        );
        assert!(
            matches!(preview_contents("file.bin", &Arc::new(b"\0\xff".to_vec()), 0), PreviewResult::Text(text) if text.contains("not available"))
        );
    }
}
