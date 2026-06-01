use codex_protocol::models::ContentItem;
use codex_protocol::models::FunctionCallOutputContentItem;
use codex_protocol::models::ImageDetail;
use codex_protocol::models::ResponseItem;
use codex_utils_image::ImageProcessingError;
use codex_utils_image::PromptImageMode;
use codex_utils_image::load_data_url_for_prompt;

#[derive(Debug, thiserror::Error)]
pub(crate) enum ResponsesCodexStrictImagePreparationError {
    #[error(
        "Responses Codex strict mode image detail only supports `original`, `high`, or `auto`; got `low`"
    )]
    UnsupportedLowDetail,
    #[error(
        "Responses Codex strict mode only supports data URL images; got image_url={image_url_preview:?}"
    )]
    NonDataUrl { image_url_preview: String },
    #[error("Responses Codex strict mode failed to prepare image: {0}")]
    ImageProcessing(#[from] ImageProcessingError),
}

pub(crate) fn prepare_response_items_for_responses_codex_strict_mode(
    items: &mut [ResponseItem],
) -> Result<(), ResponsesCodexStrictImagePreparationError> {
    prepare_response_items(items, PreparationMode::AllImages)
}

pub(crate) fn prepare_response_items_for_responses_codex_strict_mode_request_fallback(
    items: &mut [ResponseItem],
) -> Result<(), ResponsesCodexStrictImagePreparationError> {
    prepare_response_items(items, PreparationMode::UnpreparedImagesOnly)
}

#[derive(Clone, Copy)]
enum PreparationMode {
    AllImages,
    UnpreparedImagesOnly,
}

fn prepare_response_items(
    items: &mut [ResponseItem],
    mode: PreparationMode,
) -> Result<(), ResponsesCodexStrictImagePreparationError> {
    for item in items {
        prepare_response_item(item, mode)?;
    }
    Ok(())
}

fn prepare_response_item(
    item: &mut ResponseItem,
    mode: PreparationMode,
) -> Result<(), ResponsesCodexStrictImagePreparationError> {
    match item {
        ResponseItem::Message { content, .. } => prepare_content_items(content, mode),
        ResponseItem::FunctionCallOutput { output, .. }
        | ResponseItem::CustomToolCallOutput { output, .. } => {
            if let Some(content_items) = output.content_items_mut() {
                prepare_function_call_output_content_items(content_items, mode)?;
            }
            Ok(())
        }
        ResponseItem::Reasoning { .. }
        | ResponseItem::LocalShellCall { .. }
        | ResponseItem::FunctionCall { .. }
        | ResponseItem::ToolSearchCall { .. }
        | ResponseItem::CustomToolCall { .. }
        | ResponseItem::ToolSearchOutput { .. }
        | ResponseItem::WebSearchCall { .. }
        | ResponseItem::ImageGenerationCall { .. }
        | ResponseItem::Compaction { .. }
        | ResponseItem::CompactionTrigger
        | ResponseItem::ContextCompaction { .. }
        | ResponseItem::Other => Ok(()),
    }
}

fn prepare_content_items(
    items: &mut [ContentItem],
    mode: PreparationMode,
) -> Result<(), ResponsesCodexStrictImagePreparationError> {
    for item in items {
        if let ContentItem::InputImage { image_url, detail } = item {
            prepare_image_url_if_needed(image_url, detail, mode)?;
        }
    }
    Ok(())
}

fn prepare_function_call_output_content_items(
    items: &mut [FunctionCallOutputContentItem],
    mode: PreparationMode,
) -> Result<(), ResponsesCodexStrictImagePreparationError> {
    for item in items {
        if let FunctionCallOutputContentItem::InputImage { image_url, detail } = item {
            prepare_image_url_if_needed(image_url, detail, mode)?;
        }
    }
    Ok(())
}

fn prepare_image_url_if_needed(
    image_url: &mut String,
    detail: &mut Option<ImageDetail>,
    mode: PreparationMode,
) -> Result<(), ResponsesCodexStrictImagePreparationError> {
    if matches!(mode, PreparationMode::UnpreparedImagesOnly)
        && detail.is_none()
        && image_url.starts_with("data:")
    {
        return Ok(());
    }
    prepare_image_url_for_responses_codex_strict_mode(image_url, detail)
}

fn prepare_image_url_for_responses_codex_strict_mode(
    image_url: &mut String,
    detail: &mut Option<ImageDetail>,
) -> Result<(), ResponsesCodexStrictImagePreparationError> {
    if !image_url.starts_with("data:") {
        return Err(ResponsesCodexStrictImagePreparationError::NonDataUrl {
            image_url_preview: image_url.chars().take(128).collect(),
        });
    }

    let mode = prompt_image_mode_for_responses_codex_strict_detail(*detail)?;
    let image = load_data_url_for_prompt(image_url, mode)?;
    *image_url = image.into_data_url();
    *detail = None;
    Ok(())
}

fn prompt_image_mode_for_responses_codex_strict_detail(
    detail: Option<ImageDetail>,
) -> Result<PromptImageMode, ResponsesCodexStrictImagePreparationError> {
    match detail {
        None | Some(ImageDetail::Auto | ImageDetail::Original) => Ok(PromptImageMode::Original),
        Some(ImageDetail::High) => Ok(PromptImageMode::ResizeToFit),
        Some(ImageDetail::Low) => {
            Err(ResponsesCodexStrictImagePreparationError::UnsupportedLowDetail)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;
    use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
    use codex_protocol::models::ContentItem;
    use codex_protocol::models::FunctionCallOutputPayload;

    const TINY_PNG_BYTES: &[u8] = &[
        137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 1, 0, 0, 0, 1, 8, 6,
        0, 0, 0, 31, 21, 196, 137, 0, 0, 0, 11, 73, 68, 65, 84, 120, 156, 99, 96, 0, 2, 0, 0, 5, 0,
        1, 122, 94, 171, 63, 0, 0, 0, 0, 73, 69, 78, 68, 174, 66, 96, 130,
    ];

    fn tiny_png_data_url() -> String {
        format!(
            "data:image/png;base64,{}",
            BASE64_STANDARD.encode(TINY_PNG_BYTES)
        )
    }

    #[test]
    fn strict_preparation_strips_detail_from_message_images() {
        let mut items = vec![ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputImage {
                image_url: tiny_png_data_url(),
                detail: Some(ImageDetail::High),
            }],
            phase: None,
        }];

        prepare_response_items_for_responses_codex_strict_mode(&mut items)
            .expect("strict image preparation should succeed");

        let ResponseItem::Message { content, .. } = &items[0] else {
            panic!("expected message item");
        };
        let [ContentItem::InputImage { image_url, detail }] = content.as_slice() else {
            panic!("expected one input image");
        };
        assert!(image_url.starts_with("data:image/png;base64,"));
        assert_eq!(*detail, None);
    }

    #[test]
    fn strict_preparation_rejects_http_images() {
        let mut items = vec![ResponseItem::FunctionCallOutput {
            call_id: "call-1".to_string(),
            output: FunctionCallOutputPayload::from_content_items(vec![
                FunctionCallOutputContentItem::InputImage {
                    image_url: "https://example.com/image.png".to_string(),
                    detail: Some(ImageDetail::Original),
                },
            ]),
        }];

        let err = prepare_response_items_for_responses_codex_strict_mode(&mut items)
            .expect_err("HTTP image URL should fail");

        assert!(matches!(
            err,
            ResponsesCodexStrictImagePreparationError::NonDataUrl { .. }
        ));
    }

    #[test]
    fn strict_preparation_request_fallback_skips_prepared_images() {
        let original_image_url = tiny_png_data_url();
        let mut items = vec![ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputImage {
                image_url: original_image_url.clone(),
                detail: None,
            }],
            phase: None,
        }];

        prepare_response_items_for_responses_codex_strict_mode_request_fallback(&mut items)
            .expect("request fallback should succeed for already-prepared image");

        let ResponseItem::Message { content, .. } = &items[0] else {
            panic!("expected message item");
        };
        let [ContentItem::InputImage { image_url, detail }] = content.as_slice() else {
            panic!("expected one input image");
        };
        assert_eq!(image_url, &original_image_url);
        assert_eq!(*detail, None);
    }

    #[test]
    fn strict_preparation_request_fallback_rejects_http_images_without_detail() {
        let mut items = vec![ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputImage {
                image_url: "https://example.com/image.png".to_string(),
                detail: None,
            }],
            phase: None,
        }];

        let err =
            prepare_response_items_for_responses_codex_strict_mode_request_fallback(&mut items)
                .expect_err("unprepared HTTP image URL should fail even without detail metadata");

        assert!(matches!(
            err,
            ResponsesCodexStrictImagePreparationError::NonDataUrl { .. }
        ));
    }

    #[test]
    fn strict_preparation_rejects_low_detail() {
        let mut items = vec![ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputImage {
                image_url: tiny_png_data_url(),
                detail: Some(ImageDetail::Low),
            }],
            phase: None,
        }];

        let err = prepare_response_items_for_responses_codex_strict_mode(&mut items)
            .expect_err("low detail should fail");

        assert!(matches!(
            err,
            ResponsesCodexStrictImagePreparationError::UnsupportedLowDetail
        ));
    }

    #[test]
    fn strict_preparation_rejects_unsupported_url_schemes() {
        let mut items = vec![ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputImage {
                image_url: "file:///tmp/image.png".to_string(),
                detail: None,
            }],
            phase: None,
        }];

        let err = prepare_response_items_for_responses_codex_strict_mode(&mut items)
            .expect_err("unsupported scheme should fail");

        assert!(matches!(
            err,
            ResponsesCodexStrictImagePreparationError::NonDataUrl { .. }
        ));
    }
}
