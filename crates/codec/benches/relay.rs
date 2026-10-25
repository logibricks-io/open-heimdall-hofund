//! Decode, encode, and a stream relay across two protocols.

#![allow(missing_docs)]

use criterion::{Criterion, criterion_group, criterion_main};
use hofund_codec::{AnthropicMessages, Codec, OpenAiChat};
use hofund_ir::{ContentPart, Message, Request, Response, Role};

fn request() -> Request {
    let mut request = Request::new("demo");
    let mut message = Message::new(Role::User);
    message.content.push(ContentPart::text("hello"));
    request.messages.push(message);
    request
}

fn response() -> Response {
    let mut message = Message::new(Role::Assistant);
    message.content.push(ContentPart::text("hello"));
    Response::new(message)
}

fn decode(criterion: &mut Criterion) {
    let bytes = OpenAiChat.encode_request(&request()).expect("encode");
    criterion.bench_function("decode", |bencher| {
        bencher.iter(|| {
            OpenAiChat
                .decode_request(std::hint::black_box(&bytes))
                .expect("decode")
        })
    });
}

fn encode(criterion: &mut Criterion) {
    let response = response();
    criterion.bench_function("encode", |bencher| {
        bencher.iter(|| {
            OpenAiChat
                .encode_response(std::hint::black_box(&response))
                .expect("encode")
        })
    });
}

fn stream_relay(criterion: &mut Criterion) {
    let sse = concat!(
        "data: {\"id\":\"chatcmpl-1\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"}}]}\n\n",
        "data: [DONE]\n\n",
    );
    let bytes = sse.as_bytes();
    criterion.bench_function("stream_relay", |bencher| {
        bencher.iter(|| {
            let events = OpenAiChat
                .decode_stream(std::hint::black_box(bytes))
                .expect("decode");
            AnthropicMessages.encode_stream(&events).expect("encode")
        })
    });
}

fn frame() -> &'static str {
    concat!(
        "data: {\"id\":\"c\",\"object\":\"chat.completion.chunk\",\"model\":\"upstream\",",
        "\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"}}],\"usage\":",
        "{\"prompt_tokens\":11,\"completion_tokens\":7}}\n\n",
    )
}

fn relay_tap(client: &str, upstream: &str, bytes: &[u8]) -> usize {
    let split = bytes.len() / 2;
    let mut tap = OpenAiChat.raw_tap(client, upstream);
    let mut out = tap.push(std::hint::black_box(&bytes[..split]));
    out.extend(tap.push(std::hint::black_box(&bytes[split..])));
    out.extend(tap.finish());
    std::hint::black_box(out).len()
}

fn passthrough_equal(criterion: &mut Criterion) {
    let bytes = frame().as_bytes();
    criterion.bench_function("passthrough_equal", |bencher| {
        bencher.iter(|| relay_tap("upstream", "upstream", bytes))
    });
}

fn passthrough_differ(criterion: &mut Criterion) {
    let bytes = frame().as_bytes();
    criterion.bench_function("passthrough_differ", |bencher| {
        bencher.iter(|| relay_tap("demo", "upstream", bytes))
    });
}

fn wide_frame() -> Vec<u8> {
    let mut frame = br#"data: {"model":"upstream","pad":""#.to_vec();
    frame.resize(frame.len() + 8 * 1024 * 1024, b'x');
    frame.extend_from_slice(br#""}"#);
    frame.extend_from_slice(b"\n\n");
    frame
}

fn push_chunks(client: &str, upstream: &str, frame: &[u8], chunk: usize) -> usize {
    let mut tap = OpenAiChat.raw_tap(client, upstream);
    let mut forwarded = 0;
    for piece in frame.chunks(chunk) {
        forwarded += tap.push(std::hint::black_box(piece)).len();
    }
    forwarded += tap.finish().len();
    forwarded
}

fn passthrough_8mib(criterion: &mut Criterion) {
    let frame = wide_frame();
    criterion.bench_function("passthrough_8mib_equal", |bencher| {
        bencher.iter(|| push_chunks("upstream", "upstream", &frame, 16 * 1024))
    });
    criterion.bench_function("passthrough_8mib_differ", |bencher| {
        bencher.iter(|| push_chunks("demo", "upstream", &frame, 16 * 1024))
    });
}

fn passthrough_bytes(criterion: &mut Criterion) {
    let bytes = frame().as_bytes();
    criterion.bench_function("passthrough_bytes", |bencher| {
        bencher.iter(|| {
            let mut tap = OpenAiChat.raw_tap("demo", "upstream");
            let mut forwarded = 0;
            for piece in bytes.chunks(1) {
                forwarded += tap.push(std::hint::black_box(piece)).len();
            }
            forwarded += tap.finish().len();
            std::hint::black_box(forwarded)
        })
    });
}

criterion_group!(
    relay,
    decode,
    encode,
    stream_relay,
    passthrough_equal,
    passthrough_differ,
    passthrough_8mib,
    passthrough_bytes
);
criterion_main!(relay);
