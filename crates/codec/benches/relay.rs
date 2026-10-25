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

fn passthrough(criterion: &mut Criterion) {
    let frame = concat!(
        "data: {\"id\":\"c\",\"object\":\"chat.completion.chunk\",\"model\":\"upstream\",",
        "\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"}}],\"usage\":",
        "{\"prompt_tokens\":11,\"completion_tokens\":7}}\n\n",
    );
    let bytes = frame.as_bytes();
    let split = bytes.len() / 2;
    criterion.bench_function("passthrough", |bencher| {
        bencher.iter(|| {
            let mut tap = OpenAiChat.raw_tap("demo");
            let mut out = tap.push(std::hint::black_box(&bytes[..split]));
            out.extend(tap.push(std::hint::black_box(&bytes[split..])));
            out.extend(tap.finish());
            std::hint::black_box(out)
        })
    });
}

criterion_group!(relay, decode, encode, stream_relay, passthrough);
criterion_main!(relay);
