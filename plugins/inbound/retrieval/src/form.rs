//! The multipart form of a speech-to-text call, read only for its `model`.
//!
//! The form goes to each provider as the client sent it, boundary and all.
//! Only the content of each `model` field that is not a file is replaced, in
//! place, by the provider's own model id.

use std::ops::Range;

use hofund_plugin_api::{Body, RawBody};

/// A multipart form and where its `model` fields sit in it.
#[derive(Clone, Debug)]
pub(crate) struct AudioForm {
    raw: Body,
    content_type: String,
    /// The trimmed text of the last `model` field, empty when there is none.
    model: String,
    /// Where each `model` field's content sits in `raw`, in order.
    fields: Vec<Range<usize>>,
}

impl AudioForm {
    /// Reads `body` sent with `content_type`.
    ///
    /// # Errors
    ///
    /// Returns a message for the client when `content_type` is not
    /// `multipart/form-data` with a boundary, or the body does not parse as
    /// such a form.
    pub(crate) fn read(content_type: &str, raw: &Body) -> Result<Self, &'static str> {
        let body = raw.bytes();
        let boundary = boundary(content_type).ok_or(
            "send the audio as multipart/form-data, as /v1/audio/transcriptions takes it: a file and a model",
        )?;
        let mut model = String::new();
        let mut fields = Vec::new();
        for part in parts(body, boundary.as_bytes()).ok_or("the multipart form does not parse")? {
            let (name, file) = disposition(&body[part.headers.clone()]);
            if name.as_deref() == Some("model") && !file {
                model = String::from_utf8_lossy(&body[part.content.clone()])
                    .trim()
                    .to_owned();
                fields.push(part.content);
            }
        }
        Ok(Self {
            raw: raw.clone(),
            content_type: content_type.to_owned(),
            model,
            fields,
        })
    }

    /// The model the form names, empty when it names none.
    pub(crate) fn model(&self) -> &str {
        &self.model
    }

    /// The form with every `model` field set to `model`, under the content
    /// type it was sent with: the form as sent when that is the name it
    /// already has.
    pub(crate) fn for_model(&self, model: &str) -> RawBody {
        let raw = self.raw.bytes();
        if model == self.model {
            return RawBody::new(self.content_type.clone(), raw.clone());
        }
        let mut out = Vec::with_capacity(raw.len() + model.len());
        let mut at = 0;
        for field in &self.fields {
            out.extend_from_slice(&raw[at..field.start]);
            out.extend_from_slice(model.as_bytes());
            at = field.end;
        }
        out.extend_from_slice(&raw[at..]);
        RawBody::new(self.content_type.clone(), out)
    }
}

/// The boundary of a `multipart/form-data` content type.
fn boundary(content_type: &str) -> Option<String> {
    let mut params = content_type.split(';');
    let kind = params.next()?.trim();
    if !kind.eq_ignore_ascii_case("multipart/form-data") {
        return None;
    }
    params
        .filter_map(param)
        .find(|(key, _)| key.eq_ignore_ascii_case("boundary"))
        .map(|(_, value)| value)
        .filter(|value| (1..=70).contains(&value.len()))
}

/// `key=value` or `key="value"`, trimmed.
fn param(text: &str) -> Option<(String, String)> {
    let (key, value) = text.split_once('=')?;
    let value = value.trim();
    let value = value
        .strip_prefix('"')
        .and_then(|rest| rest.strip_suffix('"'))
        .unwrap_or(value);
    Some((key.trim().to_owned(), value.to_owned()))
}

/// One part of a form: where its headers and its content sit in the body.
#[derive(Debug, PartialEq, Eq)]
struct Part {
    headers: Range<usize>,
    content: Range<usize>,
}

/// The parts of `body` under `boundary`, or `None` when it is not a whole
/// form: no opening delimiter, a part without a header end, or no closing
/// delimiter.
fn parts(body: &[u8], boundary: &[u8]) -> Option<Vec<Part>> {
    let delimiter = [b"--".as_slice(), boundary].concat();
    let between = [b"\r\n--".as_slice(), boundary].concat();
    // The first delimiter starts the body or a line.
    let mut at = if body.starts_with(&delimiter) {
        delimiter.len()
    } else {
        find(body, &between, 0)? + between.len()
    };
    let mut out = Vec::new();
    loop {
        if body[at..].starts_with(b"--") {
            return Some(out);
        }
        while matches!(body.get(at), Some(b' ' | b'\t')) {
            at += 1;
        }
        if !body[at..].starts_with(b"\r\n") {
            return None;
        }
        at += 2;
        let (headers, start) = if body[at..].starts_with(b"\r\n") {
            (at..at, at + 2)
        } else {
            let end = find(body, b"\r\n\r\n", at)?;
            (at..end, end + 4)
        };
        let end = find(body, &between, start)?;
        out.push(Part {
            headers,
            content: start..end,
        });
        at = end + between.len();
    }
}

/// The field name of a part's headers, and whether it is a file. A part
/// whose disposition is not `form-data` has no name.
fn disposition(headers: &[u8]) -> (Option<String>, bool) {
    let text = String::from_utf8_lossy(headers);
    let Some(value) = text.split("\r\n").find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.trim()
            .eq_ignore_ascii_case("content-disposition")
            .then_some(value)
    }) else {
        return (None, false);
    };
    let mut params = value.split(';');
    // Only a form-data part has a field name, as the reference reads it.
    if !params
        .next()
        .is_some_and(|kind| kind.trim().eq_ignore_ascii_case("form-data"))
    {
        return (None, false);
    }
    let mut name = None;
    let mut file = false;
    for (key, value) in params.filter_map(param) {
        let key = key.to_ascii_lowercase();
        if key == "name" {
            name = Some(value);
        } else if key == "filename" || key == "filename*" {
            file = true;
        }
    }
    (name, file)
}

fn find(haystack: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    haystack
        .get(from..)?
        .windows(needle.len())
        .position(|window| window == needle)
        .map(|index| from + index)
}

#[cfg(test)]
mod tests {
    use hofund_plugin_api::Body;

    use super::{AudioForm, Part, boundary, disposition, parts};

    const TYPE: &str = "multipart/form-data; boundary=XyZ";

    fn form(fields: &[(&str, &str)]) -> Body {
        let mut out = String::new();
        for (headers, content) in fields {
            out.push_str(&format!("--XyZ\r\n{headers}\r\n\r\n{content}\r\n"));
        }
        out.push_str("--XyZ--\r\n");
        Body::from_bytes(out.into_bytes())
    }

    /// What `form` sends for `model`, with its content type.
    fn sent(form: &AudioForm, model: &str) -> (String, String) {
        let body = form.for_model(model);
        (
            body.content_type().to_owned(),
            String::from_utf8(body.bytes().to_vec()).expect("utf8"),
        )
    }

    fn text(body: &Body) -> String {
        String::from_utf8(body.bytes().to_vec()).expect("utf8")
    }

    const FILE: (&str, &str) = (
        "Content-Disposition: form-data; name=\"file\"; filename=\"a.wav\"\r\nContent-Type: audio/wav",
        "RIFF\0model\r\n--Xy",
    );
    const MODEL: (&str, &str) = (
        "Content-Disposition: form-data; name=\"model\"",
        " speech \r\n",
    );

    #[test]
    fn the_boundary_is_read_from_a_form_data_type_only() {
        assert_eq!(boundary(TYPE).as_deref(), Some("XyZ"));
        assert_eq!(
            boundary("Multipart/Form-Data ; charset=utf-8; Boundary=\"a b\"").as_deref(),
            Some("a b")
        );
        for refused in [
            "application/json",
            "multipart/mixed; boundary=x",
            "multipart/form-data",
            "multipart/form-data; boundary=",
            "multipart/form-data; boundary=\"\"",
            &format!("multipart/form-data; boundary={}", "b".repeat(71)),
        ] {
            assert_eq!(boundary(refused), None, "{refused}");
        }
        assert!(boundary(&format!("multipart/form-data; boundary={}", "b".repeat(70))).is_some());
    }

    #[test]
    fn parts_sit_between_delimiters_with_or_without_a_preamble() {
        let body = b"--b\r\nA: 1\r\n\r\nxy\r\n--b \t\r\n\r\nz\r\n--b--";
        assert_eq!(
            parts(body, b"b"),
            Some(vec![
                Part {
                    headers: 5..9,
                    content: 13..15
                },
                Part {
                    headers: 24..24,
                    content: 26..27
                },
            ])
        );
        let preamble = b"ignored\r\n--b\r\nA: 1\r\n\r\nq\r\n--b--\r\nepilogue";
        let found = parts(preamble, b"b").expect("parts");
        assert_eq!(found.len(), 1);
        assert_eq!(&preamble[found[0].content.clone()], b"q");
        assert_eq!(parts(b"--b--", b"b"), Some(Vec::new()));
        // A delimiter not at a line start is preamble.
        assert_eq!(
            parts(b"x--b\r\nA: 1\r\n\r\nq\r\n--b--", b"b"),
            Some(Vec::new())
        );
        for broken in [
            b"no delimiter".as_slice(),
            b"--b\r\nA: 1\r\n\r\nq",
            b"--b\r\nA: 1\r\nq\r\n--b--",
            b"--bX\r\n\r\nq\r\n--b--",
            b"--b",
            b"--b-\r\nA: 1\r\n\r\nq\r\n--b--",
        ] {
            assert_eq!(
                parts(broken, b"b"),
                None,
                "{}",
                String::from_utf8_lossy(broken)
            );
        }
    }

    #[test]
    fn a_disposition_gives_the_name_and_whether_it_is_a_file() {
        let read = |headers: &str| disposition(headers.as_bytes());
        assert_eq!(read(MODEL.0), (Some("model".to_owned()), false));
        assert_eq!(read(FILE.0), (Some("file".to_owned()), true));
        assert_eq!(
            read("X: 1\r\ncontent-disposition: form-data; NAME=model; filename*=utf-8''a"),
            (Some("model".to_owned()), true)
        );
        assert_eq!(read("Content-Type: text/plain"), (None, false));
        assert_eq!(read("Content-Disposition: form-data"), (None, false));
        assert_eq!(
            read("Content-Disposition: attachment; name=model"),
            (None, false)
        );
        assert_eq!(read("Content-Disposition: name=model"), (None, false));
        assert_eq!(
            read("Content-Disposition:  Form-Data ; name=model"),
            (Some("model".to_owned()), false)
        );
    }

    #[test]
    fn the_model_is_read_and_replaced_in_place() {
        let body = form(&[
            FILE,
            MODEL,
            ("Content-Disposition: form-data; name=\"prompt\"", "hi"),
        ]);
        let form_read = AudioForm::read(TYPE, &body).expect("form");
        assert_eq!(form_read.model(), "speech");
        assert_eq!(sent(&form_read, "speech"), (TYPE.to_owned(), text(&body)));
        let expected = text(&body).replace(
            "name=\"model\"\r\n\r\n speech \r\n",
            "name=\"model\"\r\n\r\nvendor-1",
        );
        assert_eq!(sent(&form_read, "vendor-1"), (TYPE.to_owned(), expected));
    }

    #[test]
    fn every_model_field_is_replaced_and_the_last_one_names_the_model() {
        let other = ("Content-Disposition: form-data; name=\"model\"", "second");
        let file_model = (
            "Content-Disposition: form-data; name=\"model\"; filename=\"m.txt\"",
            "not-a-name",
        );
        let body = form(&[MODEL, file_model, other]);
        let read = AudioForm::read(TYPE, &body).expect("form");
        assert_eq!(read.model(), "second");
        let (_, sent) = sent(&read, "v");
        assert_eq!(sent.matches("\r\n\r\nv\r\n").count(), 2, "{sent}");
        assert!(sent.contains("not-a-name"), "{sent}");
        assert!(
            !sent.contains("speech") && !sent.contains("second"),
            "{sent}"
        );
    }

    #[test]
    fn a_form_without_a_model_names_none_and_a_bad_form_is_refused() {
        let body = form(&[FILE]);
        let read = AudioForm::read(TYPE, &body).expect("form");
        assert_eq!(read.model(), "");
        assert_eq!(sent(&read, "v").1, text(&body));
        assert_eq!(
            AudioForm::read("application/json", &body).expect_err("type"),
            "send the audio as multipart/form-data, as /v1/audio/transcriptions takes it: a file and a model"
        );
        assert_eq!(
            AudioForm::read("multipart/form-data; boundary=other", &body).expect_err("parse"),
            "the multipart form does not parse"
        );
    }
}
