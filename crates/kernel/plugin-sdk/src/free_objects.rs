use std::collections::VecDeque;
use std::pin::Pin;

use bytes::Bytes;
use futures_util::{Stream, StreamExt};
use hofund_codec::FreeObjectRaw;
use hofund_ir::{FreeObjectEvents, FreeObjectPaths, StreamEvent};
use hofund_plugin_api::{BodyStream, PluginError};

type EventStream = Pin<Box<dyn Stream<Item = Result<StreamEvent, PluginError>> + Send>>;

pub(crate) fn events(stream: EventStream, paths: FreeObjectPaths) -> EventStream {
    Box::pin(futures_util::stream::unfold(
        EventState {
            stream,
            filter: FreeObjectEvents::new(paths),
            pending: VecDeque::new(),
            finished: false,
        },
        |mut state| async move {
            loop {
                if let Some(item) = state.pending.pop_front() {
                    return Some((item, state));
                }
                if state.finished {
                    return None;
                }
                match state.stream.next().await {
                    Some(Ok(event)) => {
                        state
                            .pending
                            .extend(state.filter.push(event).into_iter().map(Ok));
                    }
                    Some(Err(error)) => {
                        state.finished = true;
                        state
                            .pending
                            .extend(state.filter.finish().into_iter().map(Ok));
                        state.pending.push_back(Err(error));
                    }
                    None => {
                        state.finished = true;
                        state
                            .pending
                            .extend(state.filter.finish().into_iter().map(Ok));
                    }
                }
            }
        },
    ))
}

pub(crate) fn raw(
    body: BodyStream,
    protocol: &str,
    streaming: bool,
    paths: FreeObjectPaths,
) -> BodyStream {
    let Some(filter) = FreeObjectRaw::new(protocol, streaming, paths) else {
        return body;
    };
    BodyStream::from_stream(futures_util::stream::unfold(
        RawState {
            body,
            filter,
            pending: VecDeque::new(),
            finished: false,
        },
        |mut state| async move {
            loop {
                if let Some(item) = state.pending.pop_front() {
                    return Some((item, state));
                }
                if state.finished {
                    return None;
                }
                match state.body.next_chunk().await {
                    Some(Ok(chunk)) => {
                        let out = state.filter.push(&chunk);
                        if !out.is_empty() {
                            state.pending.push_back(Ok(Bytes::from(out)));
                        }
                    }
                    Some(Err(error)) => {
                        state.finished = true;
                        let out = state.filter.finish();
                        if !out.is_empty() {
                            state.pending.push_back(Ok(Bytes::from(out)));
                        }
                        state.pending.push_back(Err(error));
                    }
                    None => {
                        state.finished = true;
                        let out = state.filter.finish();
                        if !out.is_empty() {
                            state.pending.push_back(Ok(Bytes::from(out)));
                        }
                    }
                }
            }
        },
    ))
}

struct EventState {
    stream: EventStream,
    filter: FreeObjectEvents,
    pending: VecDeque<Result<StreamEvent, PluginError>>,
    finished: bool,
}

struct RawState {
    body: BodyStream,
    filter: FreeObjectRaw,
    pending: VecDeque<Result<Bytes, PluginError>>,
    finished: bool,
}
