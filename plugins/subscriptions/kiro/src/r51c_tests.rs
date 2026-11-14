use hofund_plugin_api::{
    ContentPart, Extensions, ImagePart, ImageSource, Message, Request, Role, SecretString,
    ToolArguments, ToolCallPart, ToolResultPart,
};
use hofund_plugin_sdk::json::{self, Value, json};

use crate::creds::Credential;
use crate::kiro_body::{FittedCache, IMAGE_LIMIT};

fn credential() -> Credential {
    Credential {
        access: SecretString::from("a".to_owned()),
        refresh: SecretString::from("r".to_owned()),
        expires_at: None,
        region: "us-east-1".to_owned(),
        profile_arn: "arn:aws:codewhisperer:us-east-1:1:profile/P".to_owned(),
        label: "Kiro".to_owned(),
        method: "social".to_owned(),
        provider: String::new(),
        client_id: String::new(),
        client_secret: SecretString::from(String::new()),
        client_id_hash: String::new(),
        token_url: String::new(),
        ide_token_path: String::new(),
        db_key: String::new(),
        cli_db_path: String::new(),
    }
}

fn encoded(request: &Request) -> Value {
    let bytes = crate::kiro_body::encode(request, &credential()).expect("encode");
    json::from_slice(&bytes).expect("request JSON")
}

fn message(role: Role, content: Vec<ContentPart>) -> Message {
    Message {
        role,
        content,
        extensions: Extensions::new(),
    }
}

fn image(data: &str) -> ContentPart {
    ContentPart::Image(ImagePart {
        source: ImageSource::Base64 {
            media_type: "image/png".to_owned(),
            data: data.to_owned(),
        },
        detail: None,
        extensions: Extensions::new(),
    })
}

fn tool_result(data: &str) -> ContentPart {
    ContentPart::ToolResult(ToolResultPart {
        id: "call_view1".to_owned(),
        is_error: false,
        content: vec![image(data)],
        extensions: Extensions::new(),
    })
}

fn view_call() -> ContentPart {
    ContentPart::ToolCall(ToolCallPart {
        id: "call_view1".to_owned(),
        name: "view_image".to_owned(),
        arguments: ToolArguments::Json {
            value: json!({"path": "/tmp/sheet.png"}),
        },
        extensions: Extensions::new(),
    })
}

fn image_request(data: &str, follow: bool) -> Request {
    let mut messages = vec![
        message(Role::User, vec![ContentPart::text("inspect the image")]),
        message(Role::Assistant, vec![view_call()]),
        message(Role::User, vec![tool_result(data)]),
    ];
    if follow {
        messages.push(message(
            Role::Assistant,
            vec![ContentPart::text("It is a contact sheet.")],
        ));
        messages.push(message(
            Role::User,
            vec![ContentPart::text("make it smaller")],
        ));
    }
    Request::new("claude-opus-5.5").with_messages(messages)
}

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = u32::MAX;
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = if crc & 1 == 1 {
                0xedb8_8320 ^ (crc >> 1)
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

fn chunk(kind: &[u8; 4], data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() + 12);
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    let crc = crc32(&out[4..]);
    out.extend_from_slice(&crc.to_be_bytes());
    out
}

fn adler32(bytes: &[u8]) -> u32 {
    let mut a = 1_u32;
    let mut b = 0_u32;
    for byte in bytes {
        a = (a + u32::from(*byte)) % 65_521;
        b = (b + a) % 65_521;
    }
    (b << 16) | a
}

fn stored_zlib(bytes: &[u8]) -> Vec<u8> {
    let mut out = vec![0x78, 0x01];
    let chunks = if bytes.is_empty() { &[][..] } else { bytes };
    if chunks.is_empty() {
        out.extend_from_slice(&[1, 0, 0, 0xff, 0xff]);
    } else {
        let count = chunks.len().div_ceil(65_535);
        for (index, part) in chunks.chunks(65_535).enumerate() {
            out.push(u8::from(index + 1 == count));
            let len = part.len() as u16;
            out.extend_from_slice(&len.to_le_bytes());
            out.extend_from_slice(&(!len).to_le_bytes());
            out.extend_from_slice(part);
        }
    }
    out.extend_from_slice(&adler32(bytes).to_be_bytes());
    out
}

fn png(width: u32, height: u32, noise: bool, size: usize) -> Vec<u8> {
    let stride = width as usize * 3 + 1;
    let mut rows = if u64::from(width) * u64::from(height) > 50_000_000 {
        vec![0; 1000]
    } else {
        vec![0x80_u8; stride * height as usize]
    };
    let mut random = 0x9e37_79b9_u32;
    if rows.len() > 1000 {
        for (index, byte) in rows.iter_mut().enumerate() {
            if index % stride == 0 {
                *byte = 0;
            } else if noise {
                random ^= random << 13;
                random ^= random >> 17;
                random ^= random << 5;
                *byte = random as u8;
            }
        }
    }
    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    ihdr.extend_from_slice(&[8, 2, 0, 0, 0]);
    let signature = [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
    let head = chunk(b"IHDR", &ihdr);
    let data = chunk(b"IDAT", &stored_zlib(&rows));
    let end = chunk(b"IEND", &[]);
    let base_len = signature.len() + head.len() + data.len() + end.len();
    let padding = size
        .checked_sub(base_len + 12)
        .unwrap_or_else(|| panic!("fixture size {size} does not fit PNG of {base_len}"));
    let mut out = Vec::with_capacity(size);
    out.extend_from_slice(&signature);
    out.extend_from_slice(&head);
    out.extend_from_slice(&chunk(b"pdNg", &vec![0x2a; padding]));
    out.extend_from_slice(&data);
    out.extend_from_slice(&end);
    assert_eq!(out.len(), size);
    out
}

fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let value = u32::from(chunk[0]) << 16
            | u32::from(chunk.get(1).copied().unwrap_or(0)) << 8
            | u32::from(chunk.get(2).copied().unwrap_or(0));
        out.push(char::from(ALPHABET[((value >> 18) & 63) as usize]));
        out.push(char::from(ALPHABET[((value >> 12) & 63) as usize]));
        out.push(if chunk.len() > 1 {
            char::from(ALPHABET[((value >> 6) & 63) as usize])
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            char::from(ALPHABET[(value & 63) as usize])
        } else {
            '='
        });
    }
    out
}

fn decode_base64(text: &str) -> Vec<u8> {
    fn digit(byte: u8) -> u32 {
        match byte {
            b'A'..=b'Z' => u32::from(byte - b'A'),
            b'a'..=b'z' => u32::from(byte - b'a') + 26,
            b'0'..=b'9' => u32::from(byte - b'0') + 52,
            b'+' => 62,
            b'/' => 63,
            _ => 0,
        }
    }
    let mut out = Vec::with_capacity(text.len() / 4 * 3);
    for chunk in text.as_bytes().chunks(4) {
        let value =
            digit(chunk[0]) << 18 | digit(chunk[1]) << 12 | digit(chunk[2]) << 6 | digit(chunk[3]);
        out.push((value >> 16) as u8);
        if chunk[2] != b'=' {
            out.push((value >> 8) as u8);
        }
        if chunk[3] != b'=' {
            out.push(value as u8);
        }
    }
    out
}

fn dimensions(base64: &str) -> (u32, u32) {
    let bytes = decode_base64(base64);
    assert_eq!(
        &bytes[..8],
        &[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]
    );
    (
        u32::from_be_bytes(bytes[16..20].try_into().expect("width")),
        u32::from_be_bytes(bytes[20..24].try_into().expect("height")),
    )
}

#[test]
fn r51c_image_limit_is_exact_and_one_quad_over_is_fitted() {
    let at = base64(&png(64, 48, false, 3_932_160));
    let padded = base64(&png(64, 48, false, 3_932_159));
    let over = base64(&png(64, 48, false, 3_932_161));
    assert_eq!(
        [at.len(), padded.len(), over.len()],
        [IMAGE_LIMIT, IMAGE_LIMIT, IMAGE_LIMIT + 4]
    );
    for data in [&at, &padded] {
        let body = encoded(&image_request(data, false));
        assert_eq!(
            body["conversationState"]["currentMessage"]["userInputMessage"]["images"][0]["source"]
                ["bytes"],
            Value::String(data.clone())
        );
    }
    let before = image_request(&over, false);
    let original = before.messages.clone();
    let body = encoded(&before);
    let fitted = &body["conversationState"]["currentMessage"]["userInputMessage"]["images"][0];
    let bytes = fitted["source"]["bytes"].as_str().expect("fitted bytes");
    assert!(bytes.len() < IMAGE_LIMIT);
    assert_eq!(fitted["format"], "png");
    assert_eq!(dimensions(bytes), (64, 48));
    assert_eq!(before.messages, original);
}

#[test]
fn r51c_noise_falls_back_to_webp_and_reuses_the_cached_fit() {
    let data = base64(&png(1389, 1000, true, 4_171_428));
    assert_eq!(data.len(), 5_561_904);
    let request = image_request(&data, false);
    let mut cache = FittedCache::default();
    let mut encode = || {
        let bytes = crate::kiro_body::encode_with_cache(&request, &credential(), &mut cache)
            .expect("encode");
        json::from_slice::<Value>(&bytes).expect("JSON")["conversationState"]["currentMessage"]
            ["userInputMessage"]["images"][0]
            .clone()
    };
    let first = encode();
    let second = encode();
    assert_eq!(first["format"], "webp");
    assert_eq!(first, second);
    let bytes = decode_base64(first["source"]["bytes"].as_str().expect("bytes"));
    assert!(first["source"]["bytes"].as_str().expect("bytes").len() <= IMAGE_LIMIT);
    assert_eq!(&bytes[..4], b"RIFF");
    assert_eq!(&bytes[8..12], b"WEBP");
}

#[test]
fn r51c_small_images_stay_in_order_beside_a_fitted_image() {
    let small = base64(&png(8, 8, false, 1000));
    let large = base64(&png(64, 48, false, 3_932_161));
    let request = Request::new("claude-opus-5.5").with_messages(vec![message(
        Role::User,
        vec![image(&small), image(&large), image(&small)],
    )]);
    let body = encoded(&request);
    let images = body["conversationState"]["currentMessage"]["userInputMessage"]["images"]
        .as_array()
        .expect("images");
    assert_eq!(images.len(), 3);
    assert_eq!(images[0]["source"]["bytes"], small);
    assert_eq!(images[2]["source"]["bytes"], small);
    assert!(images[1]["source"]["bytes"].as_str().expect("large").len() <= IMAGE_LIMIT);
}

#[test]
fn r51c_unreadable_and_pixel_bomb_images_become_exact_notes() {
    let junk = base64(&vec![0x5a; 4_000_000]);
    let huge = base64(&png(20_000, 20_000, false, 4_000_000));
    for (data, why) in [
        (junk, "it couldn't be read as an image"),
        (huge, "it has too many pixels to resize"),
    ] {
        let body = encoded(&image_request(&data, true));
        let state = &body["conversationState"];
        let user = state["history"]
            .as_array()
            .expect("history")
            .iter()
            .filter_map(|entry| entry.get("userInputMessage"))
            .find(|user| {
                user["content"]
                    .as_str()
                    .is_some_and(|content| content.contains("was left out"))
            })
            .expect("image note");
        assert!(user.get("images").is_none());
        let content = user["content"].as_str().expect("note");
        assert!(content.contains(&format!(
            "[An image (png, 5.1 MB as Base64) was left out of this message: Kiro takes at most 5.0 MB per image, and {why}.]"
        )));
        assert!(content.len() < 300);
        assert!(!content.contains("/tmp"));
        assert_eq!(
            user["userInputMessageContext"]["toolResults"][0]["toolUseId"],
            "call_view1"
        );
        assert_eq!(
            state["currentMessage"]["userInputMessage"]["content"],
            "make it smaller"
        );
    }
}

#[test]
fn r51c_inline_image_is_fitted_and_later_history_keeps_it() {
    let data = base64(&png(64, 48, false, 3_932_161));
    let inline = Request::new("claude-opus-5.5").with_messages(vec![message(
        Role::User,
        vec![image(&data), ContentPart::text("what is this?")],
    )]);
    let body = encoded(&inline);
    assert_eq!(
        body["conversationState"]["currentMessage"]["userInputMessage"]["content"],
        "what is this?"
    );
    assert!(
        body["conversationState"]["currentMessage"]["userInputMessage"]["images"][0]["source"]
            ["bytes"]
            .as_str()
            .expect("bytes")
            .len()
            <= IMAGE_LIMIT
    );

    let body = encoded(&image_request(&data, true));
    let state = &body["conversationState"];
    let historical = state["history"]
        .as_array()
        .expect("history")
        .iter()
        .filter_map(|entry| entry.get("userInputMessage"))
        .find(|user| user.get("images").is_some())
        .expect("historical image");
    assert!(
        historical["images"][0]["source"]["bytes"]
            .as_str()
            .expect("bytes")
            .len()
            <= IMAGE_LIMIT
    );
    assert!(
        state["currentMessage"]["userInputMessage"]
            .get("images")
            .is_none()
    );
}

#[test]
fn r51c_oversized_gif_is_fitted_to_png_without_losing_message_text() {
    let mut gif = b"GIF89a\x01\x00\x01\x00\x80\x00\x00\x00\x00\x00\xff\xff\xff\x2c\x00\x00\x00\x00\x01\x00\x01\x00\x00\x02\x02\x44\x01\x00\x3b".to_vec();
    gif.resize(4_000_035, 0);
    let data = base64(&gif);
    assert!(data.len() > IMAGE_LIMIT);
    let request = Request::new("claude-opus-5.5").with_messages(vec![message(
        Role::User,
        vec![
            ContentPart::text("before the image"),
            ContentPart::Image(ImagePart {
                source: ImageSource::Base64 {
                    media_type: "image/gif".to_owned(),
                    data,
                },
                detail: None,
                extensions: Extensions::new(),
            }),
            ContentPart::text("after the image"),
        ],
    )]);

    let body = encoded(&request);
    let user = &body["conversationState"]["currentMessage"]["userInputMessage"];
    assert_eq!(user["content"], "before the image\n\nafter the image");
    let fitted = &user["images"][0];
    assert_eq!(fitted["format"], "png");
    let bytes = fitted["source"]["bytes"].as_str().expect("fitted bytes");
    assert!(bytes.len() <= IMAGE_LIMIT);
    assert_eq!(dimensions(bytes), (1, 1));
}
