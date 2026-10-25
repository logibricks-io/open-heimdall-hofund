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

criterion_group!(relay, decode, encode, stream_relay);
criterion_main!(relay);
